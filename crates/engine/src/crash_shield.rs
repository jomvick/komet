//! Crash shield — a process-wide panic hook for the engine.
//!
//! What it does: on any panic, log the payload + location via `tracing` and
//! leave a `last-crash.log` marker in the data dir. The next boot reads (and
//! clears) the marker so the crash is visible in logs instead of silent.
//!
//! What it deliberately does NOT do: touch the sessions engine or journals
//! from inside the hook. A panicking thread may hold poisoned locks, and the
//! hook has no async context — interrupting runs there would risk deadlock.
//! In-flight journals are already covered by boot-time `recover_stale()`
//! (aborted stamps + synthetic `Done{EngineRestart}`); the shield's job is
//! observation (log + marker), not recovery.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

const MARKER_NAME: &str = "last-crash.log";

static HOOK_ONCE: Once = Once::new();
static CRASH_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Install the panic hook (idempotent; the hook itself is process-wide).
/// Every call refreshes the marker directory so headed + headless engines
/// sharing a process still write the marker next to the active data dir.
/// Also reports — and clears — a marker left by a previous crashed run.
pub fn install(data_dir: &Path) {
    {
        let mut dir = CRASH_DIR.lock().unwrap_or_else(|e| e.into_inner());
        *dir = Some(data_dir.to_path_buf());
    }
    HOOK_ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic payload>");
            let location = info
                .location()
                .map(|l| l.to_string())
                .unwrap_or_else(|| "<unknown location>".to_string());
            tracing::error!(payload, %location, "engine panic (crash shield)");
            if let Some(dir) = CRASH_DIR.lock().ok().and_then(|d| d.clone()) {
                write_marker(&dir, payload, &location);
            }
            previous(info);
        }));
    });
    if let Some(marker) = take_crash_marker(data_dir) {
        tracing::warn!(marker = %marker, "previous engine run crashed");
    }
}

fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARKER_NAME)
}

fn write_marker(data_dir: &Path, payload: &str, location: &str) {
    let body = format!(
        "crashed_at={}\npayload={}\nlocation={}\n",
        chrono::Utc::now().to_rfc3339(),
        payload.lines().next().unwrap_or(""),
        location
    );
    if let Some(parent) = marker_path(data_dir).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::File::create(marker_path(data_dir))
        .and_then(|mut f| f.write_all(body.as_bytes()))
    {
        Ok(()) => {}
        Err(err) => tracing::warn!(error = %err, "crash marker write failed"),
    }
}

/// Read and clear a crash marker left by a previous run. `None` when the
/// previous shutdown was clean (or the marker is unreadable).
pub fn take_crash_marker(data_dir: &Path) -> Option<String> {
    let path = marker_path(data_dir);
    let bytes = std::fs::read(&path).ok()?;
    // Always clear, even when unreadable — a stale file must not re-trigger
    // on every boot.
    let _ = std::fs::remove_file(&path);
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_shutdown_leaves_no_marker() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(take_crash_marker(dir.path()), None);
    }

    #[test]
    fn marker_round_trip_and_cleared_on_take() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), "boom", "src/lib.rs:1");
        let body = take_crash_marker(dir.path()).expect("marker present");
        assert!(body.contains("payload=boom"), "{body}");
        assert!(body.contains("location=src/lib.rs:1"), "{body}");
        assert_eq!(take_crash_marker(dir.path()), None);
        assert!(!marker_path(dir.path()).exists());
    }

    #[test]
    fn corrupt_marker_still_clears() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(marker_path(dir.path()), vec![0xff, 0xfe, 0x00]).unwrap();
        // Unreadable-as-UTF8 → None, but the stale file must be gone so it
        // cannot re-trigger on every boot.
        assert_eq!(take_crash_marker(dir.path()), None);
        assert!(!marker_path(dir.path()).exists());
    }

    #[test]
    fn panic_hook_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path());
        let _ = std::panic::catch_unwind(|| panic!("shield-test-panic"));
        let body = take_crash_marker(dir.path()).expect("hook wrote marker");
        assert!(body.contains("payload=shield-test-panic"), "{body}");
    }
}
