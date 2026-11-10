// Sonde de timing : rejoue la séquence exacte de run_session (komet) pour
// mesurer le délai AVANT LA PREMIÈRE IMAGE.
//
// Variante A (code actuel) : pas de Page.navigate initial (le target a déjà
//   l'URL : createTarget / argument de lancement).
// Variante B (ancien code) : Page.navigate initial après startScreencast.
// Variante C (correctif candidat) : Chrome démarre sur about:blank, on
//   attache + startScreencast, PUIS on navigue (un seul chargement, et le
//   screencast capture la page dès le début).
//
// Usage: cargo run --example cdp_timing -- <url> [A|B|C|both]
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:3000/".into());
    let mode = std::env::args().nth(2).unwrap_or_else(|| "all".into());
    if mode == "A" || mode == "all" || mode == "both" {
        run(&url, Variant::A);
    }
    if mode == "B" || mode == "all" || mode == "both" {
        run(&url, Variant::B);
    }
    if mode == "C" || mode == "all" {
        run(&url, Variant::C);
    }
}

#[derive(Clone, Copy)]
enum Variant {
    A,
    B,
    C,
}

fn run(url: &str, variant: Variant) {
    let t0 = Instant::now();
    let profile = format!("/tmp/opencode/cdp-timing-{}", std::process::id());
    // variante C : Chrome démarre sur about:blank (le navigate arrive après)
    let launch_url = if matches!(variant, Variant::C) {
        "about:blank"
    } else {
        url
    };
    let mut cmd = Command::new("/usr/bin/chromium-browser");
    cmd.env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--headless=new")
        .arg("--ozone-platform=headless")
        .arg("--remote-debugging-port=0")
        .arg("--hide-scrollbars")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .arg("--disable-sync")
        .arg("--disable-background-networking")
        .arg("--disable-default-apps")
        .arg("--disable-extensions")
        .arg("--disable-translate")
        .arg("--disable-breakpad")
        .arg("--disable-crash-reporter")
        .arg("--disable-component-update")
        .arg("--mute-audio")
        .arg("--password-store=basic")
        .arg("--use-mock-keychain")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--allow-insecure-localhost")
        .arg(format!("--user-data-dir={profile}"))
        .arg(launch_url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn chromium");
    let stderr = child.stderr.take().unwrap();
    let mut browser_ws = None;
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap();
        if let Some(i) = line.find("DevTools listening on ") {
            browser_ws = Some(line[i + "DevTools listening on ".len()..].trim().to_owned());
            break;
        }
    }
    let browser_ws = browser_ws.expect("no CDP endpoint");
    let t_endpoint = t0.elapsed();

    // resolve_page_ws_url : Target.getTargets → premier target "page"
    let port: u16 = browser_ws
        .split(':')
        .nth(2)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let (mut browser_socket, _) = connect(&browser_ws).unwrap();
    if let MaybeTlsStream::Plain(stream) = browser_socket.get_ref() {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    }
    browser_socket
        .send(Message::Text(
            r#"{"id":1,"method":"Target.getTargets"}"#.into(),
        ))
        .unwrap();
    let mut tid = None;
    for _ in 0..5 {
        if let Ok(Message::Text(text)) = browser_socket.read()
            && let Ok(v) = serde_json::from_str::<Value>(text.as_ref())
            && let Some(targets) = v.pointer("/result/targetInfos").and_then(Value::as_array)
        {
            for t in targets {
                if t["type"] == "page" {
                    tid = Some(t["targetId"].as_str().unwrap().to_owned());
                }
            }
            break;
        }
    }
    let tid = tid.expect("no page target");
    let t_target = t0.elapsed();
    let page_ws = format!("ws://127.0.0.1:{port}/devtools/page/{tid}");

    let (mut socket, _) = connect(&page_ws).unwrap();
    let mut next_id: u64 = 1;
    send(&mut socket, next_id, "Page.enable", json!({}));
    next_id += 1;
    send(&mut socket, next_id, "Runtime.enable", json!({}));
    next_id += 1;
    send(
        &mut socket,
        next_id,
        "Target.setAutoAttach",
        json!({"autoAttach": true, "flatten": true, "waitForDebuggerOnStart": false}),
    );
    next_id += 1;
    send(
        &mut socket,
        next_id,
        "Emulation.setDeviceMetricsOverride",
        json!({"width": 1280u32, "height": 800u32, "deviceScaleFactor": 1.0, "mobile": false}),
    );
    next_id += 1;
    send(
        &mut socket,
        next_id,
        "Page.startScreencast",
        json!({"format": "jpeg", "quality": 90, "maxWidth": 1280u32, "maxHeight": 800u32, "everyNthFrame": 1}),
    );
    next_id += 1;
    // variante B : navigate comme l'ancien code ; variante C : navigate
    // APRÈS le startScreencast (le screencast capture le chargement)
    if matches!(variant, Variant::B | Variant::C) {
        send(&mut socket, next_id, "Page.navigate", json!({"url": url}));
        next_id += 1;
    }
    let t_init = t0.elapsed();

    if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    }
    let mut frames = 0u32;
    let mut first_frame = None;
    let mut loading_stopped = None;
    let deadline = t0 + Duration::from_secs(12);
    let mut last_session = None;
    while Instant::now() < deadline {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(v) = serde_json::from_str::<Value>(text.as_ref()) else {
                    continue;
                };
                match v.get("method").and_then(Value::as_str) {
                    Some("Page.screencastFrame") => {
                        frames += 1;
                        if first_frame.is_none() {
                            first_frame = Some(t0.elapsed());
                        }
                        if let Some(sid) = v.pointer("/params/sessionId").and_then(Value::as_u64) {
                            let ack = json!({"id": next_id, "method": "Page.screencastFrameAck", "params": {"sessionId": sid}});
                            next_id += 1;
                            socket.send(Message::Text(ack.to_string())).unwrap();
                        }
                    }
                    Some("Page.frameStoppedLoading") => {
                        if loading_stopped.is_none() {
                            loading_stopped = Some(t0.elapsed());
                        }
                    }
                    Some(m) if m.starts_with("Page.") => {
                        println!("  event {m} @ {:?}", t0.elapsed());
                    }
                    _ => {}
                }
                if let Some(sid) = v.pointer("/params/sessionId").and_then(Value::as_u64) {
                    last_session = Some(sid);
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                println!("  WS_ERR {e}");
                break;
            }
        }
    }
    println!(
        "VARIANTE {} ({}) : endpoint={:?} target={:?} init={:?} 1re image={:?} chargement_fini={:?} images={frames} (session {:?})",
        match variant {
            Variant::A => "A sans navigate",
            Variant::B => "B navigate",
            Variant::C => "C about:blank+navigate",
        },
        url,
        t_endpoint,
        t_target,
        t_init,
        first_frame,
        loading_stopped,
        last_session
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&profile);
}

fn send(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    id: u64,
    method: &str,
    params: Value,
) {
    let msg = json!({"id": id, "method": method, "params": params}).to_string();
    socket.send(Message::Text(msg)).expect("send");
}
