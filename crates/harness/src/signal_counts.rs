//! Per-harness signal accounting (transparency plan Task 12, Step 1).
//!
//! WHY this exists: a drop that only writes a debug line cannot be asserted by
//! anything — the absence of a signal reads as "this agent sends none". So every
//! drop site in an adapter also increments a receipt/forward/drop counter pair
//! keyed by `(harness, family)`, which turns "silently dropped" into a number a
//! test can pin (`crates/harness/tests/surfaces.rs::
//! no_silent_drop_without_a_surface_entry`).
//!
//! A static scan of `continue`/`Vec::new()` arms was rejected on purpose: a test
//! cannot read the source reliably, and a reformat would silently un-guard the
//! code. The check is runtime counters driven by fixtures instead.
//!
//! The ledger is process-wide on purpose (one app process, one ledger per
//! harness): counters must survive a session ending, because the interesting
//! question is "what has this CLI ever sent us that we threw away".

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use komet_proto::HarnessId;

/// The families an interaction signal can belong to. The first five are exactly
/// the columns of [`komet_proto::HarnessInteractionSurface`] — the closed set is
/// the point: mapping a new family to a matrix column becomes a compile error,
/// not a silently uncategorised drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SignalFamily {
    /// Questions asked by the agent and how they reach the user.
    Questions,
    /// Context-compaction lifecycle and summary signals.
    Compaction,
    /// Spawned subagent lifecycle / attribution signals.
    Subagents,
    /// The agent's plan.
    Plans,
    /// Tool execution visibility (calls, updates, partial output).
    ToolProgress,
    /// An update kind this build maps to no event and no column — legitimately
    /// always dropped, but never uncounted. Note it counts what *this map* did:
    /// grok's `subagent_*` kinds also land here because the normalizer returns
    /// nothing for them (the subagent observer forwards them on its own
    /// channel), so an `UnknownMethod` row is not by itself a lost affordance.
    UnknownMethod,
    /// A twin: the same fact arriving twice (an echoed prompt, a replayed
    /// update), where forwarding both copies would render it twice. Also
    /// legitimately always dropped.
    Duplicate,
}

impl SignalFamily {
    /// Stable name for logs and the settings debug line.
    pub fn as_str(&self) -> &'static str {
        match self {
            SignalFamily::Questions => "questions",
            SignalFamily::Compaction => "compaction",
            SignalFamily::Subagents => "subagents",
            SignalFamily::Plans => "plans",
            SignalFamily::ToolProgress => "tool_progress",
            SignalFamily::UnknownMethod => "unknown_method",
            SignalFamily::Duplicate => "duplicate",
        }
    }
}

/// Receipt/forward/drop tallies for one `(harness, family)` pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SignalCounts {
    pub received: u64,
    pub forwarded: u64,
    pub dropped: u64,
}

const FAMILIES: &[SignalFamily] = &[
    SignalFamily::Questions,
    SignalFamily::Compaction,
    SignalFamily::Subagents,
    SignalFamily::Plans,
    SignalFamily::ToolProgress,
    SignalFamily::UnknownMethod,
    SignalFamily::Duplicate,
];

/// The closed family list. Anything iterating accounting (tests, the settings
/// debug line) walks this rather than guessing at variants.
pub fn known_families() -> &'static [SignalFamily] {
    FAMILIES
}

/// The [`komet_proto::HarnessInteractionSurface`] column that covers a family,
/// or `None` for the two families that are legitimately always-droppable.
///
/// Exhaustive on purpose: a new `SignalFamily` forces a new matrix mapping here,
/// which is what makes "a drop with no surface entry" unrepresentable.
pub fn surface_column(family: SignalFamily) -> Option<&'static str> {
    match family {
        SignalFamily::Questions => Some("questions"),
        SignalFamily::Compaction => Some("compaction"),
        SignalFamily::Subagents => Some("subagents"),
        SignalFamily::Plans => Some("plans"),
        SignalFamily::ToolProgress => Some("tool_progress"),
        SignalFamily::UnknownMethod | SignalFamily::Duplicate => None,
    }
}

type Key = (HarnessId, SignalFamily);

#[derive(Default)]
struct Ledger {
    counts: HashMap<Key, SignalCounts>,
    /// `(harness, family, detail)` already logged: the no-spam set.
    logged: HashSet<(Key, String)>,
}

/// A hot agent loop must not grow an unbounded set of logged keys, and a log
/// line per dropped signal would be the spam this module is meant to avoid. So
/// past this many distinct keys counting continues but logging stops — the
/// counters stay exact, only the log line is capped.
const LOG_DEDUPE_CAP: usize = 512;

fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(Ledger::default()))
}

/// A ledger poisoned by a panicking run must not take accounting down with it:
/// the numbers are diagnostics, and losing them hides the very drop we look for.
fn lock() -> std::sync::MutexGuard<'static, Ledger> {
    ledger().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn bump(key: Key, field: fn(&mut SignalCounts) -> &mut u64) {
    *field(lock().counts.entry(key).or_default()) += 1;
}

/// An update of this family arrived from the agent.
pub fn record_received(harness: HarnessId, family: SignalFamily) {
    bump((harness, family), |c| &mut c.received);
}

/// ... and reached the user as an event.
pub fn record_forwarded(harness: HarnessId, family: SignalFamily) {
    bump((harness, family), |c| &mut c.forwarded);
}

/// ... and was discarded here. Logs `debug!` with the family and the harness
/// name, at most once per distinct `(harness, family, detail)` key — returns
/// whether this call produced that line (so the behaviour is testable without a
/// tracing subscriber).
pub fn record_dropped(harness: HarnessId, family: SignalFamily, detail: &str) -> bool {
    let key = (harness, family);
    bump(key, |c| &mut c.dropped);
    let mut ledger = lock();
    if ledger.logged.len() >= LOG_DEDUPE_CAP {
        return false;
    }
    if !ledger.logged.insert((key, detail.to_owned())) {
        return false;
    }
    drop(ledger);
    tracing::debug!(
        harness = harness.as_str(),
        family = family.as_str(),
        detail,
        "signal dropped (counted, see komet_harness::signal_counts)"
    );
    true
}

/// One signal that arrived and was discarded without reaching any column: the
/// pair every drop site records, kept to one call so a site cannot log a drop it
/// never counted.
pub fn record_drop_of_received(harness: HarnessId, family: SignalFamily, detail: &str) -> bool {
    record_received(harness, family);
    record_dropped(harness, family, detail)
}

/// The recorded counters for one harness, in `SignalFamily` declaration order
/// (not arrival order) so tests and UI can rely on the sequence. Families that
/// never appeared are absent rather than zero-filled: "not observed" is not
/// "observed as nothing".
pub fn snapshot(harness: HarnessId) -> Vec<(SignalFamily, SignalCounts)> {
    let ledger = lock();
    FAMILIES
        .iter()
        .filter_map(|family| ledger.counts.get(&(harness, *family)).map(|c| (*family, *c)))
        .collect()
}

/// Every harness with recorded accounting, `(harness, family, counts)` sorted by
/// harness then family — the view the settings debug line uses.
pub fn all_snapshots() -> Vec<(HarnessId, Vec<(SignalFamily, SignalCounts)>)> {
    let ledger = lock();
    let mut harnesses: Vec<HarnessId> = ledger.counts.keys().map(|(harness, _)| *harness).collect();
    harnesses.sort_by_key(|harness| harness.as_str());
    harnesses.dedup();
    harnesses
        .into_iter()
        .map(|harness| {
            let rows = FAMILIES
                .iter()
                .filter_map(|family| ledger.counts.get(&(harness, *family)).map(|c| (*family, *c)))
                .collect();
            (harness, rows)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ACP entry (the accounting-aware view of the normalizer)
// ---------------------------------------------------------------------------

/// Map one ACP `session/update` payload through the real normalizer, with the
/// counters fed by the arms that actually dropped or forwarded it.
///
/// This is the runtime path (`crate::acp`'s session loop calls the same
/// function) and the guard fixtures' entry: a fixture that only exercised a
/// test-local copy of the mapping would prove nothing about production drops.
pub fn observe_acp_update(
    harness: HarnessId,
    update: &serde_json::Value,
) -> Vec<komet_proto::AgentEvent> {
    crate::acp::normalize::map_update_for(harness, update)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinct id per test: the ledger is process-wide and lib tests share
    /// it. `Mock` is deliberately unused — the normalizer's own unit tests
    /// record under it (see `acp::normalize`'s test shim).
    fn counts(harness: HarnessId, family: SignalFamily) -> SignalCounts {
        snapshot(harness)
            .into_iter()
            .find(|(f, _)| *f == family)
            .map(|(_, c)| c)
            .unwrap_or_default()
    }

    #[test]
    fn received_forwarded_and_dropped_are_counted_separately() {
        let harness = HarnessId::Devin;
        let before = counts(harness, SignalFamily::Plans);
        // One signal that rendered, one that was discarded at the drop site.
        record_received(harness, SignalFamily::Plans);
        record_forwarded(harness, SignalFamily::Plans);
        record_drop_of_received(harness, SignalFamily::Plans, "no entries");
        let after = counts(harness, SignalFamily::Plans);
        assert_eq!(after.received - before.received, 2);
        assert_eq!(after.forwarded - before.forwarded, 1);
        assert_eq!(after.dropped - before.dropped, 1);
        // The ledger must explain itself: nothing received is unaccounted for.
        assert_eq!(
            after.received - after.forwarded - after.dropped,
            before.received - before.forwarded - before.dropped
        );
    }

    #[test]
    fn record_dropped_logs_once_per_distinct_detail() {
        let harness = HarnessId::Copilot;
        assert!(
            record_drop_of_received(harness, SignalFamily::Compaction, "no compactionId"),
            "the first drop for a key must log"
        );
        assert!(
            !record_drop_of_received(harness, SignalFamily::Compaction, "no compactionId"),
            "a repeat of the same key must not log again"
        );
        assert!(
            record_drop_of_received(harness, SignalFamily::Compaction, "no summary delta"),
            "a distinct detail is a distinct key"
        );
        // Counting keeps going whether or not the line was emitted.
        assert_eq!(counts(harness, SignalFamily::Compaction).dropped, 3);
    }

    #[test]
    fn snapshot_is_ordered_by_family_not_by_arrival() {
        let harness = HarnessId::Pi;
        for family in [
            SignalFamily::ToolProgress,
            SignalFamily::Questions,
            SignalFamily::UnknownMethod,
            SignalFamily::Plans,
        ] {
            record_received(harness, family);
        }
        let seen: Vec<SignalFamily> = snapshot(harness).into_iter().map(|(f, _)| f).collect();
        // Recorded out of order, read back in declaration order — the same
        // sequence whatever run interleaved the arrivals.
        assert_eq!(
            seen,
            vec![
                SignalFamily::Questions,
                SignalFamily::Plans,
                SignalFamily::ToolProgress,
                SignalFamily::UnknownMethod,
            ]
        );
    }

    #[test]
    fn all_snapshots_returns_one_row_per_harness_and_every_family() {
        let harness = HarnessId::Qoder;
        record_received(harness, SignalFamily::Questions);
        record_received(harness, SignalFamily::Plans);
        let all = all_snapshots();
        let mut ids: Vec<HarnessId> = all.iter().map(|(h, _)| *h).collect();
        let total = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), total, "one snapshot per harness");
        let rows: Vec<SignalFamily> = all
            .iter()
            .find(|(h, _)| *h == harness)
            .map(|(_, rows)| rows.iter().map(|(f, _)| *f).collect())
            .unwrap_or_default();
        assert!(rows.contains(&SignalFamily::Questions));
        assert!(rows.contains(&SignalFamily::Plans));
    }

    #[test]
    fn every_known_family_maps_to_a_column_or_is_always_droppable() {
        // The totality half of the invariant, checked where the enum lives: the
        // two always-droppable families are named, everything else is a matrix
        // column.
        let mut columns: Vec<&str> = Vec::new();
        for family in known_families() {
            match surface_column(*family) {
                Some(column) => columns.push(column),
                None => {
                    assert!(
                        matches!(family, SignalFamily::UnknownMethod | SignalFamily::Duplicate),
                        "{family:?} is neither a matrix column nor an always-droppable family"
                    );
                }
            }
        }
        columns.sort_unstable();
        assert_eq!(
            columns,
            vec!["compaction", "plans", "questions", "subagents", "tool_progress"],
            "the five surface columns, exactly once each"
        );
    }
}
