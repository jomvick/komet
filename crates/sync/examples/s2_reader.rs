//! Fresh RUST reader for an s2 loro room — the exact client path every komet
//! device uses (RoomClient + loro 1.13 import). Diagnosing the 2026-08-10
//! fresh-reader black screens: a JS reader converges on fold+trimmed rooms;
//! does the Rust import of the wasm's shallow export also survive?
//! Usage: cargo run -p komet-sync --example s2_reader -- <wsUrl> <roomId>
use loro::LoroDoc;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (url, room_id) = (&args[1], &args[2]);
    let doc = LoroDoc::new();
    let client = komet_sync::RoomClient::connect(url, room_id, doc.clone())
        .await
        .expect("connect");

    // Wait for backfill to land content (or time out empty).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if !doc.get_list("messages").is_empty() || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    // settle: let trailing rows import
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let msgs = doc.get_list("messages");
    let mut parts_total = 0u64;
    let mut bad_kind = 0u64;
    let mut entries_parsed = 0u64;
    let session = komet_doc::SessionDoc::from_doc(doc.clone());
    let parsed = session.read_entries().map(|e| e.len() as u64).unwrap_or(0);
    for i in 0..msgs.len() {
        if let Some(loro::ValueOrContainer::Container(loro::Container::Map(m))) = msgs.get(i) {
            if let Some(loro::ValueOrContainer::Container(loro::Container::List(parts))) =
                m.get("parts")
            {
                for j in 0..parts.len() {
                    parts_total += 1;
                    if let Some(loro::ValueOrContainer::Container(loro::Container::Map(p))) =
                        parts.get(j)
                    {
                        match p.get("kind") {
                            Some(loro::ValueOrContainer::Value(loro::LoroValue::String(_))) => {}
                            _ => bad_kind += 1,
                        }
                    } else {
                        bad_kind += 1;
                    }
                }
            }
            entries_parsed += 1;
        }
    }
    println!(
        "RESULT:{{\"rawMessages\":{},\"entriesScanned\":{},\"parts\":{},\"badKind\":{},\"readEntriesParsed\":{}}}",
        msgs.len(),
        entries_parsed,
        parts_total,
        bad_kind,
        parsed
    );
    let _ = client.shutdown().await;
}
