//! `komet update` — check for and apply a newer release, natively (download →
//! verify → symlink swap → service restart). macOS app bundles swap the bundle
//! instead; source builds are report-only.

use anyhow::bail;
use komet_update::{InstallKind, current_version, version_newer};

/// `--check` prints the verdict and exits (nonzero when an update is available,
/// so scripts can gate on it).
pub async fn update(check_only: bool) -> anyhow::Result<()> {
    let manifest = komet_update::fetch_latest().await?;
    let current = current_version();
    if !version_newer(&manifest.version, current) {
        println!(
            "komet {current} is up to date (latest: {}).",
            manifest.version
        );
        return Ok(());
    }
    println!("komet {current} → {} available", manifest.version);
    if check_only {
        std::process::exit(1);
    }

    match komet_update::detect_install() {
        InstallKind::Managed { app_root } => {
            println!(
                "downloading {}…",
                komet_update::headless_artifact(&manifest.version)
            );
            komet_update::stage_headless(&manifest, &app_root).await?;
            komet_update::apply_headless(&app_root, &manifest.version)?;
            println!(
                "installed {} (current → {})",
                app_root.join(&manifest.version).display(),
                manifest.version
            );
            match komet_update::restart_service() {
                Ok(()) => println!("engine service restarted."),
                Err(err) => println!(
                    "note: service restart failed ({err:#}) — restart the engine manually to finish."
                ),
            }
            Ok(())
        }
        InstallKind::MacApp { bundle } => {
            println!(
                "downloading {}…",
                komet_update::mac_app_artifact(&manifest.version)
            );
            let data_dir = std::env::var_os("KOMET_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(super::dirs_data_dir);
            let staged = komet_update::stage_mac_app(&manifest, &data_dir).await?;
            komet_update::apply_mac_app(&staged, &bundle)?;
            println!("updated {} — relaunch Komet to finish.", bundle.display());
            Ok(())
        }
        InstallKind::Unmanaged => {
            bail!(
                 "this binary is not update-managed (source build or hand-copied).\n\
                  Linux: curl -fsSL https://raw.githubusercontent.com/jomvick/komet/main/install.sh | sh\n\
                  macOS: download the new Komet.app dmg, or rebuild from source."
            )
        }
    }
}
