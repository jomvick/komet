//! `komet serve` — turn this machine into a Komet environment in one command: a
//! sync server, an engine that executes the sessions started there, and a
//! pairing link to paste in the desktop app.
//!
//! The persisted [`ServeConfig`] is the single source of truth for the token,
//! namespace and public URL, so a service restart needs no environment variable.
//! It is a secret: the file is 0600 and the link is only printed on the run that
//! creates it (or when asked explicitly).

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use komet_proto::{EnvError, PAIR_VERSION, PairPayload, decode_pair_link, encode_pair_link};
use komet_sync_server::ServerIdentity;

pub const SERVE_FILE: &str = "serve.json";
pub const DEFAULT_PORT: u16 = 8787;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeConfig {
    pub token: String,
    pub org_id: String,
    pub user_id: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    pub label: String,
}

impl ServeConfig {
    /// Load `{data_dir}/serve.json`, creating it on first run. Flags win over the
    /// file and are persisted back; the second value is `true` when this call
    /// produced a pairing link that does not exist yet — first run, or the URL,
    /// label or token changed. That is the run allowed to print the link.
    ///
    /// `lan` fills in a missing public URL with this machine's LAN address —
    /// explicitly asked for, and only ever when no URL is configured yet: a
    /// `--public-url` (or one already stored) is the operator's choice and wins.
    pub fn load_or_create(
        data_dir: &Path,
        public_url: Option<String>,
        port: Option<u16>,
        label: Option<String>,
        lan: bool,
    ) -> anyhow::Result<(Self, bool)> {
        let path = data_dir.join(SERVE_FILE);
        let stored: Option<Self> = match std::fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("{}: not a komet serve config", path.display()))?,
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        let pick = |field: fn(&Self) -> String| stored.as_ref().map(field);
        let effective_port = port
            .or(stored.as_ref().map(|c| c.port))
            .unwrap_or(DEFAULT_PORT);
        let configured = public_url
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .or_else(|| stored.as_ref().and_then(|c| c.public_url.clone()));
        let public_url = match configured {
            Some(url) => Some(url),
            None if lan => Some(lan_url(effective_port)?),
            None => None,
        };
        let config = Self {
            token: pick(|c| c.token.clone()).unwrap_or_else(fresh_token),
            org_id: pick(|c| c.org_id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            user_id: pick(|c| c.user_id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            port: effective_port,
            public_url,
            label: label
                .or_else(|| stored.as_ref().map(|c| c.label.clone()))
                .filter(|label| !label.trim().is_empty())
                .unwrap_or_else(default_label),
        };
        if config.token.trim().is_empty()
            || config.org_id.trim().is_empty()
            || config.user_id.trim().is_empty()
        {
            anyhow::bail!("{}: empty token, org or user", path.display());
        }
        if stored.as_ref() != Some(&config) {
            write_private(&path, &config)?;
        }
        // Only the three fields the link encodes make a link new; a port change
        // rewrites the file without changing what would be printed.
        let link_is_new = match stored.as_ref() {
            None => true,
            Some(previous) => {
                previous.token != config.token
                    || previous.public_url != config.public_url
                    || previous.label != config.label
            }
        };
        Ok((config, link_is_new))
    }

    pub fn identity(&self) -> ServerIdentity {
        ServerIdentity {
            org_id: self.org_id.clone(),
            user_id: self.user_id.clone(),
            label: self.label.clone(),
        }
    }
}

/// `komet://pair/…` for the desktop app. Rejected when no public URL is known,
/// and validated through the same decoder the app runs, so a typo in
/// `--public-url` fails here instead of at paste time.
pub fn pair_link(config: &ServeConfig) -> Result<String, EnvError> {
    let url = config.public_url.as_deref().ok_or(EnvError::InvalidLink)?;
    let link = encode_pair_link(&PairPayload {
        v: PAIR_VERSION,
        label: config.label.clone(),
        url: url.to_string(),
        token: config.token.clone(),
    });
    decode_pair_link(&link)?;
    Ok(link)
}

/// Print the link, or — with no public URL — the token plus the reverse-proxy
/// one-liner that would earn one. Never both: a link without TLS is a token in
/// clear text on the wire.
pub fn print_pairing(config: &ServeConfig) {
    match pair_link(config) {
        Ok(link) => {
            println!(
                "\nPairing link (a secret — paste it in Komet → Settings → Sync → Add \
                 device):\n\n{link}\n"
            );
            if let Some(url) = config.public_url.as_deref()
                && komet_sync_server::sync_url_is_insecure(url)
            {
                println!(
                    "Warning: {url} is plain HTTP, so this link's token travels unencrypted and \
                     anyone on that network can read it. Fine on a LAN you already trust, not on \
                     the internet — pasting this link in Komet asks you to confirm that first."
                );
            }
        }
        Err(_) => {
            println!("No --public-url yet, so there is no pairing link to print.");
            println!("Token: {}", config.token);
            println!("Expose this server behind TLS, then run:");
            println!("  komet serve --public-url https://YOUR_HOST --print-link");
            println!("Caddy on the same machine:");
            println!("  YOUR_HOST {{ reverse_proxy 127.0.0.1:{} }}", config.port);
        }
    }
}

/// Announce a link only when there is a link to announce: the token plus the
/// proxy steps come out when no URL is configured yet. A restart whose link is
/// already on disk stays silent unless `--print-link` is passed — the link is a
/// secret, and a supervisor that keeps stdout (`docker logs`) must not collect it
/// on every boot.
pub fn should_print_link(print_link_flag: bool, link_is_new: bool) -> bool {
    print_link_flag || link_is_new
}

/// Sync server on `0.0.0.0:{port}` + the engine that runs this environment's
/// sessions. The engine reaches its own server over loopback, so the token never
/// leaves the process to reach the local registry.
pub async fn run(
    config: ServeConfig,
    data_dir: PathBuf,
    ipc_port: u16,
    harness: komet_engine::HarnessId,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port))
        .await
        .with_context(|| format!("binding the sync server on 0.0.0.0:{}", config.port))?;
    tracing::info!(port = config.port, label = %config.label, "komet serve starting");

    let identity = config.identity();
    let sync = tokio::spawn(komet_sync_server::serve_listener_with_identity(
        listener,
        data_dir.clone(),
        config.token.clone(),
        identity,
    ));
    let engine = tokio::spawn(async move {
        komet_engine::Engine::new(komet_engine::EngineConfig {
            data_dir,
            edge_url: format!("http://127.0.0.1:{}", config.port),
            // Dev auth: the bearer is the shared token, the identity the user id.
            edge_token: Some(config.user_id.clone()),
            sync_token: Some(config.token.clone()),
            ipc_port,
            default_harness: harness,
            org_id: Some(config.org_id.clone()),
        })
        .run()
        .await
    });

    // Either half exiting ends the process; the runtime aborts the other spawn.
    tokio::select! {
        result = sync => result.context("sync server task")??,
        result = engine => result.context("engine task")??,
    }
    Ok(())
}

/// Install this machine's user service as `komet serve` (replacing a headless
/// daemon unit), so the environment survives logout/reboot.
pub fn install_service(data_dir: &Path) -> anyhow::Result<()> {
    if !(cfg!(target_os = "linux") || cfg!(target_os = "macos")) {
        anyhow::bail!(
            "--install-service is only supported on Linux (systemd --user) and macOS (launchd); \
             run `komet serve` under your process manager instead"
        );
    }
    // No port on the command line: serve.json is the single source of truth.
    crate::daemon::install_with_args(data_dir, &["serve"])
}

/// `--lan`: this machine's address on its own network, as `http://{addr}:{port}`.
///
/// Plain HTTP carries the bearer in clear text, which is why this is an opt-in
/// flag and why [`print_pairing`] repeats the warning: it is for a network the
/// operator already trusts, not a substitute for TLS on the public internet.
fn lan_url(port: u16) -> anyhow::Result<String> {
    let addr = primary_lan_address().ok_or_else(|| anyhow::anyhow!(
        "--lan found no private address on this machine's default route. On a VPS with a public \
         IP only, put the server behind TLS and pass --public-url https://YOUR_HOST"
    ))?;
    let host = if addr.is_ipv6() {
        format!("[{addr}]")
    } else {
        addr.to_string()
    };
    Ok(format!("http://{host}:{port}"))
}

/// The source address the kernel would use to reach the network — read off the
/// routing table by an unconnected UDP socket, so no packet is ever sent. Only a
/// private-range answer counts as a LAN address (`--lan` on a host whose default
/// route is the public internet would otherwise print a public URL).
fn primary_lan_address() -> Option<std::net::IpAddr> {
    use std::net::UdpSocket;
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect(("8.8.8.8", 53)).ok()?;
    match probe.local_addr().ok() {
        Some(addr) if is_lan_address(&addr.ip()) => Some(addr.ip()),
        _ => None,
    }
}

/// RFC 1918 v4, ULA v6 — and neither loopback nor link-local, which no other
/// device on the network can reach.
fn is_lan_address(addr: &std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
        }
        std::net::IpAddr::V6(v6) => !v6.is_loopback() && v6.segments()[0] & 0xfe00 == 0xfc00,
    }
}

/// 32 hex chars, same scheme as `komet sync-init`.
pub(crate) fn fresh_token() -> String {
    use std::fmt::Write as _;
    let mut token = String::new();
    for byte in uuid::Uuid::new_v4().as_bytes() {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

fn default_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|host| !host.trim().is_empty())
        .unwrap_or_else(|| "komet VPS".into())
}

fn write_private(path: &Path, config: &ServeConfig) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(config)?;
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&bytes)?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(public_url: Option<&str>) -> ServeConfig {
        ServeConfig {
            token: "t0ken".into(),
            org_id: "org-1".into(),
            user_id: "user-1".into(),
            port: DEFAULT_PORT,
            public_url: public_url.map(Into::into),
            label: "VPS ovh".into(),
        }
    }

    #[test]
    fn serve_config_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let (first, link_is_new) = ServeConfig::load_or_create(
            dir.path(),
            Some("https://sync.example.com/".into()),
            Some(9000),
            Some("VPS ovh".into()),
            false,
        )
        .unwrap();
        assert!(link_is_new);
        assert_eq!(first.token.len(), 32);
        assert_eq!(
            first.public_url.as_deref(),
            Some("https://sync.example.com")
        );
        assert_eq!(first.port, 9000);

        let (reloaded, link_is_new) =
            ServeConfig::load_or_create(dir.path(), None, None, None, false).unwrap();
        assert!(
            !link_is_new,
            "an existing file is a restart, not a first run"
        );
        assert_eq!(reloaded, first, "token, org, user and port are stable");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(SERVE_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn lan_never_overrides_a_configured_public_url() {
        // Deterministic: with a URL already known, --lan must not probe the
        // network at all — the operator's choice (or the stored one) is it.
        let dir = tempfile::tempdir().unwrap();
        let (explicit, _) = ServeConfig::load_or_create(
            dir.path(),
            Some("https://sync.example.com".into()),
            Some(8787),
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            explicit.public_url.as_deref(),
            Some("https://sync.example.com")
        );

        let (stored, _) = ServeConfig::load_or_create(dir.path(), None, None, None, true).unwrap();
        assert_eq!(
            stored.public_url.as_deref(),
            Some("https://sync.example.com"),
            "a second --lan run must not re-detect away the saved URL"
        );
        assert_eq!(stored.token, explicit.token, "the pairing stays valid");
    }

    #[test]
    fn lan_address_is_a_private_reachable_one_only() {
        let v4 = |a: u8, b: u8| std::net::IpAddr::V4(format!("{a}.{b}.1.5").parse().unwrap());
        assert!(is_lan_address(&v4(10, 0)));
        assert!(is_lan_address(&v4(172, 16)));
        assert!(is_lan_address(&v4(172, 31)));
        assert!(is_lan_address(&v4(192, 168)));
        // Not private, or not reachable from another device on the network.
        assert!(!is_lan_address(&v4(172, 32)));
        assert!(!is_lan_address(&v4(203, 0)));
        assert!(!is_lan_address(&v4(127, 0)));
        assert!(!is_lan_address(&v4(169, 254)));
        let v6 = |first: u16| {
            std::net::IpAddr::V6(
                format!("{first:x}::1")
                    .parse::<std::net::Ipv6Addr>()
                    .expect("v6 test address"),
            )
        };
        assert!(is_lan_address(&v6(0xfd12)));
        assert!(!is_lan_address(&v6(0xfe12)));
        assert!(!is_lan_address(&std::net::IpAddr::V6(
            std::net::Ipv6Addr::LOCALHOST
        )));
    }

    #[test]
    fn pair_link_requires_public_url() {
        assert_eq!(pair_link(&config(None)), Err(EnvError::InvalidLink));
        assert!(pair_link(&config(Some("ftp://nope"))).is_err());
    }

    #[test]
    fn pair_link_round_trips_through_decoder() {
        let link = pair_link(&config(Some("https://sync.example.com"))).unwrap();
        let payload = decode_pair_link(&link).unwrap();
        assert_eq!(payload.label, "VPS ovh");
        assert_eq!(payload.url, "https://sync.example.com");
        assert_eq!(payload.token, "t0ken");
    }

    #[test]
    fn print_link_is_not_emitted_on_plain_restart() {
        assert!(!should_print_link(false, false));
        assert!(should_print_link(false, true));
        assert!(should_print_link(true, false));
    }

    /// The one-liner gets re-run with the same flags, and anything that keeps
    /// stdout collects the secret: only a link that actually changed is new.
    #[test]
    fn re_running_the_same_public_url_is_not_a_new_link() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://sync.example.com";
        let (_, fresh) = ServeConfig::load_or_create(
            dir.path(),
            Some(url.into()),
            None,
            Some("vm1".into()),
            false,
        )
        .unwrap();
        assert!(fresh, "the first run has no link on disk yet");
        let (_, again) = ServeConfig::load_or_create(
            dir.path(),
            Some(url.into()),
            None,
            Some("vm1".into()),
            false,
        )
        .unwrap();
        assert!(!again, "same URL, same label — nothing new to print");
        let (_, moved) = ServeConfig::load_or_create(
            dir.path(),
            Some("https://sync-other.example.com".into()),
            None,
            Some("vm1".into()),
            false,
        )
        .unwrap();
        assert!(moved, "a new address makes a new link, so it must be shown");
    }
}
