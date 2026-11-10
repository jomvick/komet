// Sonde hors-UI : reproduit la séquence CDP exacte de run_session (browser.rs)
// pour voir ce que Chrome renvoie pendant une navigation web (ex. DuckDuckGo).
// Usage: cargo run --example cdp_probe -- https://duckduckgo.com/
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://duckduckgo.com/".into());
    let exe = "/usr/bin/chromium-browser";

    let mut cmd = Command::new(exe);
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
        .arg("--disable-extensions")
        .arg("--mute-audio")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--allow-insecure-localhost")
        .arg(format!(
            "--user-data-dir=/tmp/opencode/cdp-probe-{}",
            std::process::id()
        ))
        .arg("--no-sandbox")
        .arg(&url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn chromium");
    let stderr = child.stderr.take().unwrap();
    let mut ws_url = None;
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap();
        if let Some(i) = line.find("DevTools listening on ") {
            ws_url = Some(line[i + "DevTools listening on ".len()..].trim().to_owned());
            break;
        }
    }
    let browser_ws = ws_url.expect("no CDP endpoint");
    println!("BROWSER_WS {browser_ws}");

    // découvre le page-target via le WS navigateur (comme process.rs::resolve_page_ws_url)
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
    if let Ok(Message::Text(text)) = browser_socket.read()
        && let Ok(v) = serde_json::from_str::<Value>(text.as_ref())
        && let Some(targets) = v.pointer("/result/targetInfos").and_then(Value::as_array)
    {
        for t in targets {
            if t["type"] == "page" {
                tid = Some(t["targetId"].as_str().unwrap().to_owned());
            }
        }
    }
    let tid = tid.expect("no page target");
    println!("PAGE target_id={tid:?}");
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
    send(&mut socket, next_id, "Page.navigate", json!({"url": url}));
    next_id += 1;

    if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let v: Value = serde_json::from_str(text.as_ref()).unwrap();
                match v.get("method").and_then(Value::as_str) {
                    Some("Page.screencastFrame") => {
                        let size = v
                            .pointer("/params/data")
                            .and_then(Value::as_str)
                            .map_or(0, |s| s.len());
                        println!(
                            "EVENT screencastFrame ({} bytes base64) sessionId={:?}  [{:?}]",
                            size,
                            v.pointer("/params/sessionId"),
                            start.elapsed()
                        );
                        let sid = v
                            .pointer("/params/sessionId")
                            .and_then(Value::as_u64)
                            .unwrap();
                        let ack = json!({"id": next_id, "method": "Page.screencastFrameAck", "params": {"sessionId": sid}});
                        next_id += 1;
                        socket.send(Message::Text(ack.to_string())).unwrap();
                    }
                    Some(m) if m.starts_with("Page.") => {
                        println!("EVENT {m}  [{:?}]", start.elapsed())
                    }
                    Some(m) => println!("EVENT {m}  [{:?}]", start.elapsed()),
                    None => {
                        let s: &str = text.as_ref();
                        println!("RESP {}  [{:?}]", &s[..s.len().min(200)], start.elapsed());
                    }
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                println!("WS_ERR {e}");
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn send(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    id: u64,
    method: &str,
    params: Value,
) {
    let msg = json!({"id": id, "method": method, "params": params}).to_string();
    socket.send(Message::Text(msg)).expect("send");
    println!("SENT {method}");
}
