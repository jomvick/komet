//! OS network-path monitoring → [`crate::wake::set_path_online`].
//!
//! macOS: `NWPathMonitor` (Network.framework, plain C surface + one ObjC
//! block). Linux: the default route is polled via `/proc/net/route` every 2s
//! (dependency-free; a real netlink subscription would only shave the
//! detection latency below the 2s poll). Windows: not implemented yet —
//! `NotifyNetworkConnectivityHintChange` FFI is the planned backend; until
//! then the empirical signals (suspend detector, sibling-dial successes,
//! focus probe) carry recovery, exactly as before this module existed.

/// Start the process-wide path monitor. Idempotent; call from engine startup.
pub fn spawn_path_monitor() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        #[cfg(target_os = "macos")]
        imp::start();
        #[cfg(target_os = "linux")]
        imp::start();
    });
}

#[cfg(target_os = "linux")]
mod linux_imp {
    use std::time::Duration;

    /// Poll cadence. The kernel pushes netlink events for route/interface
    /// changes, but a poll this coarse is cheap (one /proc read per 2 s),
    /// dependency-free, and only ever wrong for two seconds — the wake/online
    /// signals (a sibling dial succeeding) cover the gap either way.
    const POLL: Duration = Duration::from_secs(2);

    /// `/proc/net/route` row for the default route (hex, destination 0).
    fn has_default_route() -> bool {
        let Ok(table) = std::fs::read_to_string("/proc/net/route") else {
            // No route table readable: assume a path rather than go silent
            // (same bias as the macOS monitor's non-`unsatisfied` statuses).
            return true;
        };
        table.lines().skip(1).any(|line| {
            let mut fields = line.split_whitespace();
            let _iface = fields.next();
            matches!(fields.next(), Some("00000000"))
        })
    }

    pub(super) fn start() {
        std::thread::Builder::new()
            .name("komet.net-path".into())
            .spawn(|| {
                let mut online = has_default_route();
                loop {
                    std::thread::sleep(POLL);
                    let now = has_default_route();
                    if now != online {
                        tracing::info!(
                            "net_path: default route {}, reporting {}",
                            if now { "restored" } else { "gone" },
                            if now { "online" } else { "offline" }
                        );
                        online = now;
                        crate::wake::set_path_online(online);
                    }
                }
            })
            .expect("net_path: spawn Linux monitor thread");
    }
}

#[cfg(target_os = "linux")]
use linux_imp as imp;

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_char, c_void};

    use block2::{Block, RcBlock};

    // nw_path_monitor_t / nw_path_t / dispatch_queue_t are ObjC objects;
    // opaque pointers are all this bridge needs.
    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_path_monitor_create() -> *mut c_void;
        fn nw_path_monitor_set_update_handler(
            monitor: *mut c_void,
            handler: &Block<dyn Fn(*mut c_void)>,
        );
        fn nw_path_monitor_set_queue(monitor: *mut c_void, queue: *mut c_void);
        fn nw_path_monitor_start(monitor: *mut c_void);
        fn nw_path_get_status(path: *mut c_void) -> i32;
    }

    unsafe extern "C" {
        fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
    }

    /// `nw_path_status_unsatisfied`. Only a definitive "no path" parks the
    /// dialers — `invalid`/`satisfiable` stay optimistic, so a confused
    /// monitor can only ever make us dial too much, never go silent.
    const NW_PATH_STATUS_UNSATISFIED: i32 = 2;

    pub(super) fn start() {
        unsafe {
            let monitor = nw_path_monitor_create();
            if monitor.is_null() {
                tracing::warn!("net_path: NWPathMonitor unavailable");
                return;
            }
            let handler = RcBlock::new(|path: *mut c_void| {
                let status = unsafe { nw_path_get_status(path) };
                let online = status != NW_PATH_STATUS_UNSATISFIED;
                crate::wake::set_path_online(online);
                if online {
                    // Interface handovers (wifi→hotspot, VPN up/down) arrive
                    // as satisfied→satisfied updates. Old sockets are dead on
                    // the new path either way, so kick parked dials on every
                    // viable-path report — waiters drain stale events before
                    // arming, so a redundant kick costs nothing.
                    crate::wake::notify_online();
                }
            });
            let queue = dispatch_queue_create(c"komet.net-path".as_ptr(), std::ptr::null_mut());
            nw_path_monitor_set_queue(monitor, queue);
            nw_path_monitor_set_update_handler(monitor, &handler);
            nw_path_monitor_start(monitor);
            // The monitor (and its handler copy) lives for the process.
            std::mem::forget(handler);
        }
    }
}
