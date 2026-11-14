//! Building blocks for Settings › Usage on agents that report no provider quota
//! (issue #53). Nothing here talks to a provider.
//!
//! Three small, independent pieces:
//!
//! * [`Coverage`] — says how complete a number is, so the UI can show
//!   "partial" instead of a confident total built on half the data.
//! * [`ActivationTimeline`] — which provider account was active when. Local
//!   logs written by an agent CLI do not name the account, so a turn is
//!   attributed to the account that was active at its timestamp. When the
//!   active account changed while Komet was not looking (a login done in a
//!   terminal), the stretch in between is [`Attribution::Unknown`] rather than
//!   guessed.
//! * [`LocalTurn`] / [`LocalUsageReader`] — the shape a per-agent log reader
//!   returns, plus [`drop_measured_duplicates`] so a turn run through Komet is
//!   not counted twice (once measured live, once found in the agent's logs).
//!
//! The per-agent readers themselves are written from what the probe script
//! (`scripts/probe-usage-sources.py`) observes on real machines, not from any
//! third-party implementation. See `docs/research/usage-sources.md`.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File under the usage store root holding the activation history.
pub const TIMELINE_FILE: &str = "account_timeline.jsonl";

/// How complete a usage figure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Coverage {
    /// Every record in the range was read and attributed.
    Complete,
    /// Some records were unreadable, unattributed, or outside what we can see.
    Partial,
    /// Nothing usable (no logs, unreadable store, nothing attributed).
    Unavailable,
}

/// Coverage from "how many records could be attributed to the active account".
pub fn coverage_for(attributed: usize, total: usize) -> Coverage {
    if total == 0 || attributed == 0 {
        Coverage::Unavailable
    } else if attributed >= total {
        Coverage::Complete
    } else {
        Coverage::Partial
    }
}

/// One change of the active account for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activation {
    /// Epoch millis the account became active (as far as Komet knows).
    pub ts: i64,
    /// `account_key` now active; `None` = the agent has no Komet-managed account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Set when the change was only noticed later (the live account differed
    /// from the last recorded one at startup). Turns after this instant and
    /// before `ts` cannot be attributed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_from: Option<i64>,
}

/// Who a timestamp belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    Account(String),
    /// The agent has no Komet-managed accounts (matches `TurnUsage.account == None`).
    Untagged,
    /// Before the first observation, or inside an unobserved gap.
    Unknown,
}

/// Activation history of one agent, ordered by time.
#[derive(Debug, Clone, Default)]
pub struct ActivationTimeline {
    entries: Vec<Activation>,
}

impl ActivationTimeline {
    pub fn from_entries(mut entries: Vec<Activation>) -> Self {
        entries.sort_by_key(|e| e.ts);
        Self { entries }
    }

    pub fn entries(&self) -> &[Activation] {
        &self.entries
    }

    /// Record a switch Komet performed itself. Returns the new entry, or `None`
    /// when the account did not actually change.
    pub fn record(&mut self, ts: i64, account: Option<String>) -> Option<Activation> {
        if self
            .entries
            .last()
            .is_some_and(|last| last.account == account)
        {
            return None;
        }
        let ts = self.entries.last().map_or(ts, |last| ts.max(last.ts));
        let entry = Activation {
            ts,
            account,
            gap_from: None,
        };
        self.entries.push(entry.clone());
        Some(entry)
    }

    /// Compare the account that is live now with the last recorded one.
    ///
    /// * no history yet → first observation, nothing before it is attributable;
    /// * same account → nothing to do (`None`);
    /// * different account → the switch happened outside Komet, so the stretch
    ///   since the last known entry is marked as a gap.
    pub fn reconcile(&mut self, now: i64, live: Option<String>) -> Option<Activation> {
        let gap_from = match self.entries.last() {
            None => None,
            Some(last) if last.account == live => return None,
            Some(last) => Some(last.ts),
        };
        let ts = self.entries.last().map_or(now, |last| now.max(last.ts));
        let entry = Activation {
            ts,
            account: live,
            gap_from,
        };
        self.entries.push(entry.clone());
        Some(entry)
    }

    /// Which account owned `ts`.
    pub fn account_at(&self, ts: i64) -> Attribution {
        let idx = self.entries.partition_point(|e| e.ts <= ts);
        if idx == 0 {
            return Attribution::Unknown;
        }
        if let Some(next) = self.entries.get(idx)
            && let Some(gap_from) = next.gap_from
            && ts > gap_from
        {
            return Attribution::Unknown;
        }
        match &self.entries[idx - 1].account {
            Some(account) => Attribution::Account(account.clone()),
            None => Attribution::Untagged,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    harness: String,
    #[serde(flatten)]
    activation: Activation,
}

/// Append-only, per-device store of activation history.
#[derive(Debug, Clone)]
pub struct TimelineStore {
    path: PathBuf,
}

impl TimelineStore {
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        Ok(Self {
            path: root.join(TIMELINE_FILE),
        })
    }

    pub fn append(&self, harness: &str, activation: &Activation) -> io::Result<()> {
        let row = Row {
            harness: harness.to_string(),
            activation: activation.clone(),
        };
        let line = serde_json::to_string(&row).map_err(io::Error::other)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")
    }

    /// Torn or malformed lines are skipped (a crash mid-append may leave one).
    pub fn load(&self) -> HashMap<String, ActivationTimeline> {
        let Ok(file) = fs::File::open(&self.path) else {
            return HashMap::new();
        };
        let mut by_harness: HashMap<String, Vec<Activation>> = HashMap::new();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(row) = serde_json::from_str::<Row>(&line) {
                by_harness
                    .entry(row.harness)
                    .or_default()
                    .push(row.activation);
            }
        }
        by_harness
            .into_iter()
            .map(|(harness, entries)| (harness, ActivationTimeline::from_entries(entries)))
            .collect()
    }
}

/// One model turn found in an agent's own logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTurn {
    /// Epoch millis.
    pub ts: i64,
    pub session: String,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

/// What a reader found, and how much to trust it.
#[derive(Debug, Clone)]
pub struct ReadOutcome {
    pub turns: Vec<LocalTurn>,
    pub coverage: Coverage,
    /// Human-readable cause when coverage is not `Complete`
    /// ("no session directory", "3 files unreadable", …).
    pub reason: Option<String>,
}

impl ReadOutcome {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            turns: Vec::new(),
            coverage: Coverage::Unavailable,
            reason: Some(reason.into()),
        }
    }
}

/// A per-agent reader of the logs that agent leaves on disk. Implementations
/// must be read-only, bounded in memory, and cheap on repeat calls (cache by
/// file mtime/size): the Usage page may open often on a weak machine.
pub trait LocalUsageReader: Send + Sync {
    /// Agent slug, same spelling as `harness_slug`.
    fn agent(&self) -> &'static str;
    /// Short label shown in the UI ("local logs").
    fn source_label(&self) -> &'static str;
    /// Turns with `ts >= since_ms`.
    fn read_since(&self, since_ms: i64) -> ReadOutcome;
}

/// What identifies a turn Komet already measured live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasuredKey {
    pub ts: i64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Remove local turns that Komet already counted when they ran.
///
/// The agent's own log and Komet's live measurement describe the same turn
/// with slightly different timestamps, so two turns match when their input and
/// output token counts are equal and their times are within `tolerance_ms`.
pub fn drop_measured_duplicates(
    local: Vec<LocalTurn>,
    measured: &[MeasuredKey],
    tolerance_ms: i64,
) -> Vec<LocalTurn> {
    let mut sorted: Vec<MeasuredKey> = measured.to_vec();
    sorted.sort_by_key(|m| m.ts);
    local
        .into_iter()
        .filter(|turn| {
            let start = sorted.partition_point(|m| m.ts < turn.ts - tolerance_ms);
            !sorted[start..]
                .iter()
                .take_while(|m| m.ts <= turn.ts + tolerance_ms)
                .any(|m| {
                    m.input_tokens == turn.input_tokens && m.output_tokens == turn.output_tokens
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn before_first_observation_is_unknown() {
        let mut t = ActivationTimeline::default();
        t.record(1_000, acct("a"));
        assert_eq!(t.account_at(999), Attribution::Unknown);
        assert_eq!(t.account_at(1_000), Attribution::Account("a".into()));
    }

    #[test]
    fn switch_attributes_each_side_to_its_own_account() {
        let mut t = ActivationTimeline::default();
        t.record(1_000, acct("a"));
        t.record(2_000, acct("b"));
        assert_eq!(t.account_at(1_999), Attribution::Account("a".into()));
        assert_eq!(t.account_at(2_000), Attribution::Account("b".into()));
        assert_eq!(t.account_at(9_999), Attribution::Account("b".into()));
    }

    #[test]
    fn recording_the_same_account_is_a_no_op() {
        let mut t = ActivationTimeline::default();
        assert!(t.record(1_000, acct("a")).is_some());
        assert!(t.record(2_000, acct("a")).is_none());
        assert_eq!(t.entries().len(), 1);
    }

    #[test]
    fn agent_without_accounts_is_untagged_not_unknown() {
        let mut t = ActivationTimeline::default();
        t.record(1_000, None);
        assert_eq!(t.account_at(5_000), Attribution::Untagged);
    }

    #[test]
    fn external_switch_leaves_an_unknown_gap() {
        let mut t = ActivationTimeline::default();
        t.record(1_000, acct("a"));
        // Komet was closed; at startup the live account is "b".
        let entry = t.reconcile(5_000, acct("b")).expect("changed");
        assert_eq!(entry.gap_from, Some(1_000));
        assert_eq!(t.account_at(1_000), Attribution::Account("a".into()));
        assert_eq!(t.account_at(3_000), Attribution::Unknown);
        assert_eq!(t.account_at(5_000), Attribution::Account("b".into()));
    }

    #[test]
    fn reconcile_with_the_same_account_changes_nothing() {
        let mut t = ActivationTimeline::default();
        t.record(1_000, acct("a"));
        assert!(t.reconcile(9_000, acct("a")).is_none());
        assert_eq!(t.account_at(8_000), Attribution::Account("a".into()));
    }

    #[test]
    fn first_reconcile_does_not_invent_the_past() {
        let mut t = ActivationTimeline::default();
        let entry = t.reconcile(5_000, acct("a")).expect("first observation");
        assert_eq!(entry.gap_from, None);
        assert_eq!(t.account_at(4_999), Attribution::Unknown);
    }

    #[test]
    fn clock_going_backwards_never_reorders_history() {
        let mut t = ActivationTimeline::default();
        t.record(5_000, acct("a"));
        let entry = t.record(1_000, acct("b")).expect("changed");
        assert_eq!(entry.ts, 5_000);
    }

    #[test]
    fn coverage_follows_attribution() {
        assert_eq!(coverage_for(0, 0), Coverage::Unavailable);
        assert_eq!(coverage_for(0, 10), Coverage::Unavailable);
        assert_eq!(coverage_for(7, 10), Coverage::Partial);
        assert_eq!(coverage_for(10, 10), Coverage::Complete);
    }

    fn turn(ts: i64, input: u64, output: u64) -> LocalTurn {
        LocalTurn {
            ts,
            session: "s".into(),
            model: None,
            input_tokens: input,
            cached_input_tokens: 0,
            output_tokens: output,
            reasoning_tokens: 0,
        }
    }

    #[test]
    fn duplicates_need_equal_tokens_and_close_time() {
        let measured = [MeasuredKey {
            ts: 10_000,
            input_tokens: 100,
            output_tokens: 20,
        }];
        let local = vec![
            turn(10_800, 100, 20), // same turn, clock skew → dropped
            turn(10_800, 100, 21), // different output → kept
            turn(30_000, 100, 20), // too far in time → kept
        ];
        let kept = drop_measured_duplicates(local, &measured, 2_000);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|t| t.output_tokens == 21 || t.ts == 30_000));
    }

    #[test]
    fn store_round_trips_and_skips_torn_lines() {
        let dir = std::env::temp_dir().join(format!(
            "komet-usage-sources-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let store = TimelineStore::open(&dir).unwrap();
        store
            .append(
                "grok",
                &Activation {
                    ts: 1_000,
                    account: acct("a"),
                    gap_from: None,
                },
            )
            .unwrap();
        store
            .append(
                "grok",
                &Activation {
                    ts: 2_000,
                    account: acct("b"),
                    gap_from: Some(1_000),
                },
            )
            .unwrap();
        // A crash mid-append can leave a half line.
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(TIMELINE_FILE))
            .unwrap();
        writeln!(file, "{{\"harness\":\"grok\",\"ts\":").unwrap();

        let loaded = store.load();
        let grok = loaded.get("grok").expect("grok timeline");
        assert_eq!(grok.entries().len(), 2);
        assert_eq!(grok.account_at(1_500), Attribution::Unknown);
        assert_eq!(grok.account_at(2_500), Attribution::Account("b".into()));
        let _ = fs::remove_dir_all(&dir);
    }
}
