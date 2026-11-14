//! Aggregation of local agent logs into the Usage report (issue #53, PR3).
//!
//! One call per agent per report: read the agent's [`LocalUsageReader`],
//! attribute each turn to the account that was active at its timestamp via
//! the [`ActivationTimeline`], drop turns Komet already measured live, then
//! bucket the rest into day / 7-day / 30-day totals with API-equivalent cost
//! estimates when the model is priced.
//!
//! Honesty rules carried over from `usage_sources`:
//! - a turn inside an unobserved account-switch gap counts as *unattributed*
//!   and downgrades coverage to `Partial` — never silently assigned;
//! - turns belonging to a *different* account are excluded, not counted;
//! - when the reader reports `Unavailable`, the totals stay empty and the
//!   reason is propagated — no invented zeros.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use komet_proto::{AgentLocalUsage, UsageBreakdown, UsageCoverage, UsageDetail, UsageTotals};

use crate::usage_pricing;
use crate::usage_sources::{
    ActivationTimeline, Attribution, Coverage, LocalTurn, LocalUsageReader, MeasuredKey,
    coverage_for, drop_measured_duplicates,
};

const DAY_MS: i64 = 86_400_000;

/// How far apart a local-log timestamp and Komet's live measurement of the
/// same turn may be and still count as the same turn. Agent CLIs stamp the
/// turn when the model call ends; Komet records the usage event a moment
/// later on the same machine, so seconds of skew are normal — two minutes is
/// generous without risking two genuinely different turns (identical input
/// AND output token counts within the window) being merged.
pub const DEDUP_TOLERANCE_MS: i64 = 120_000;

fn worse(a: Coverage, b: Coverage) -> Coverage {
    use Coverage::*;
    match (a, b) {
        (Unavailable, _) | (_, Unavailable) => Unavailable,
        (Partial, _) | (_, Partial) => Partial,
        _ => Complete,
    }
}

fn to_proto(c: Coverage) -> UsageCoverage {
    match c {
        Coverage::Complete => UsageCoverage::Complete,
        Coverage::Partial => UsageCoverage::Partial,
        Coverage::Unavailable => UsageCoverage::Unavailable,
    }
}

fn accumulate(totals: &mut UsageTotals, turn: &LocalTurn) {
    totals.runs += 1;
    totals.input_tokens += turn.input_tokens;
    totals.cached_input_tokens += turn.cached_input_tokens;
    totals.output_tokens += turn.output_tokens;
    totals.reasoning_tokens += turn.reasoning_tokens;
}

/// Per-model totals inside one window, shaped for `usage_pricing::estimate`.
fn model_detail(turns: &[&LocalTurn]) -> UsageDetail {
    let mut models: HashMap<Option<String>, UsageTotals> = HashMap::new();
    for turn in turns {
        let entry = models.entry(turn.model.clone()).or_default();
        entry.runs += 1;
        entry.input_tokens += turn.input_tokens;
        entry.cached_input_tokens += turn.cached_input_tokens;
        entry.output_tokens += turn.output_tokens;
        entry.reasoning_tokens += turn.reasoning_tokens;
    }
    UsageDetail {
        models: models
            .into_iter()
            .map(|(label, totals)| UsageBreakdown { label, totals })
            .collect(),
        projects: Vec::new(),
    }
}

/// Aggregate one agent's local logs for the Usage report.
///
/// `timeline` is the agent's activation history (`None` when the usage store
/// was never wired — every turn then lands in `Unknown`). `active_account`
/// is the `account_key` currently selected for the agent. `measured` holds
/// the Komet-measured turns visible to that account, used for dedup.
/// `now` is the report timestamp; the reader is asked for the last 30 days
/// and day/week windows are cut from the same set.
pub fn aggregate_local(
    reader: &dyn LocalUsageReader,
    timeline: Option<&ActivationTimeline>,
    active_account: Option<&str>,
    measured: &[MeasuredKey],
    now: DateTime<Utc>,
) -> AgentLocalUsage {
    let now_ms = now.timestamp_millis();
    let month_start = now_ms - 30 * DAY_MS;
    let day_start = now_ms - DAY_MS;
    let week_start = now_ms - 7 * DAY_MS;

    let outcome = reader.read_since(month_start);
    let source_label = reader.source_label().to_string();
    if outcome.coverage == Coverage::Unavailable {
        return AgentLocalUsage {
            source_label,
            coverage: UsageCoverage::Unavailable,
            reason: outcome.reason,
            ..AgentLocalUsage::default()
        };
    }

    // Attribute every turn the reader returned, then keep the active
    // account's share. `placed` = turns whose account era is known (active,
    // other account, or untagged); `unattributed` = turns inside an
    // unobserved gap or before the first observation.
    let mut placed = 0usize;
    let mut unattributed = 0u64;
    let mut attributed: Vec<LocalTurn> = Vec::new();
    for turn in outcome.turns {
        placed += 1;
        match timeline
            .map(|t| t.account_at(turn.ts))
            .unwrap_or(Attribution::Unknown)
        {
            Attribution::Account(key) => {
                if active_account == Some(key.as_str()) {
                    attributed.push(turn);
                }
                // A different account's turn: correctly excluded, not a gap.
            }
            // No Komet-managed account for this agent — the turn is the
            // agent's usage regardless of which login the CLI holds.
            Attribution::Untagged => attributed.push(turn),
            Attribution::Unknown => unattributed += 1,
        }
    }

    let kept = drop_measured_duplicates(attributed, measured, DEDUP_TOLERANCE_MS);

    let mut day = UsageTotals::default();
    let mut week = UsageTotals::default();
    let mut month = UsageTotals::default();
    let mut day_turns = Vec::new();
    let mut week_turns = Vec::new();
    let mut month_turns = Vec::new();
    for turn in &kept {
        if turn.ts >= day_start {
            accumulate(&mut day, turn);
            day_turns.push(turn);
        }
        if turn.ts >= week_start {
            accumulate(&mut week, turn);
            week_turns.push(turn);
        }
        accumulate(&mut month, turn);
        month_turns.push(turn);
    }

    // Coverage = worst of "the reader saw everything" and "every turn could
    // be placed on an account". Zero turns with a working reader is a
    // confident empty result, not missing data.
    let reader_cov = outcome.coverage;
    let attribution_cov = if placed == 0 {
        Coverage::Complete
    } else {
        coverage_for(placed - unattributed as usize, placed)
    };
    let coverage = worse(reader_cov, attribution_cov);
    let reason = outcome.reason.or_else(|| {
        (unattributed > 0)
            .then(|| format!("{unattributed} turn(s) could not be attributed to an account"))
    });

    AgentLocalUsage {
        day,
        last_7_days: week,
        last_30_days: month,
        unattributed_turns: unattributed,
        source_label,
        coverage: to_proto(coverage),
        reason: (coverage != Coverage::Complete).then_some(reason).flatten(),
        day_cost: usage_pricing::estimate(&model_detail(&day_turns)),
        week_cost: usage_pricing::estimate(&model_detail(&week_turns)),
        month_cost: usage_pricing::estimate(&model_detail(&month_turns)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_sources::{Activation, LocalUsageReader, ReadOutcome};
    use std::sync::Mutex;

    /// Reader stub returning canned turns — the aggregation under test does
    /// not care how they were parsed.
    struct StubReader {
        outcome: Mutex<ReadOutcome>,
    }

    impl StubReader {
        fn new(turns: Vec<LocalTurn>) -> Self {
            Self {
                outcome: Mutex::new(ReadOutcome {
                    turns,
                    coverage: Coverage::Complete,
                    reason: None,
                }),
            }
        }
    }

    impl LocalUsageReader for StubReader {
        fn agent(&self) -> &'static str {
            "stub"
        }
        fn source_label(&self) -> &'static str {
            "local logs"
        }
        fn read_since(&self, _since_ms: i64) -> ReadOutcome {
            self.outcome.lock().unwrap().clone()
        }
    }

    fn turn(ts: i64, input: u64, output: u64) -> LocalTurn {
        LocalTurn {
            ts,
            session: "s".into(),
            model: Some("gpt-5".into()),
            input_tokens: input,
            cached_input_tokens: 0,
            output_tokens: output,
            reasoning_tokens: 0,
        }
    }

    fn timeline(entries: &[(i64, Option<&str>)]) -> ActivationTimeline {
        ActivationTimeline::from_entries(
            entries
                .iter()
                .map(|(ts, account)| Activation {
                    ts: *ts,
                    account: account.map(str::to_string),
                    gap_from: None,
                })
                .collect(),
        )
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_800_000_000_000).unwrap()
    }

    #[test]
    fn keeps_only_the_active_accounts_turns() {
        let now_ms = now().timestamp_millis();
        let tl = timeline(&[(now_ms - 100 * DAY_MS, Some("acct-a"))]);
        let reader = StubReader::new(vec![
            turn(now_ms - DAY_MS, 100, 10),    // day window
            turn(now_ms - 5 * DAY_MS, 50, 5),  // week window
            turn(now_ms - 20 * DAY_MS, 25, 2), // month window
        ]);
        let report = aggregate_local(&reader, Some(&tl), Some("acct-a"), &[], now());
        assert_eq!(report.coverage, UsageCoverage::Complete);
        assert_eq!(report.day.input_tokens, 100);
        assert_eq!(report.last_7_days.input_tokens, 150);
        assert_eq!(report.last_30_days.input_tokens, 175);
        assert_eq!(report.last_30_days.runs, 3);
        assert_eq!(report.unattributed_turns, 0);
    }

    #[test]
    fn other_accounts_turns_are_excluded_not_counted() {
        let now_ms = now().timestamp_millis();
        let mut tl = timeline(&[(now_ms - 100 * DAY_MS, Some("acct-a"))]);
        tl.record(now_ms - 5 * DAY_MS, Some("acct-b".into()));
        let reader = StubReader::new(vec![
            turn(now_ms - 10 * DAY_MS, 100, 10), // acct-a era
            turn(now_ms - 2 * DAY_MS, 999, 99),  // acct-b era
        ]);
        let report = aggregate_local(&reader, Some(&tl), Some("acct-a"), &[], now());
        assert_eq!(report.last_30_days.input_tokens, 100);
        assert_eq!(report.coverage, UsageCoverage::Complete);
        assert_eq!(report.unattributed_turns, 0);
    }

    #[test]
    fn unknown_gap_turns_downgrade_coverage_and_are_counted() {
        let now_ms = now().timestamp_millis();
        let mut tl = timeline(&[(now_ms - 100 * DAY_MS, Some("acct-a"))]);
        // External switch noticed now: the whole stretch since acct-a's last
        // known entry is a gap — turns there belong to nobody we can name.
        tl.reconcile(now_ms, Some("acct-b".into()));
        let reader = StubReader::new(vec![
            turn(now_ms - DAY_MS, 20, 2), // inside the unknown gap
            turn(now_ms, 30, 3),          // after the observation → acct-b
        ]);
        let report = aggregate_local(&reader, Some(&tl), Some("acct-b"), &[], now());
        assert_eq!(report.coverage, UsageCoverage::Partial);
        assert_eq!(report.unattributed_turns, 1);
        // Only the post-observation turn counts for acct-b.
        assert_eq!(report.last_30_days.input_tokens, 30);
        assert_eq!(report.last_30_days.runs, 1);
        assert!(report.reason.is_some());
    }

    #[test]
    fn measured_duplicates_are_dropped_within_tolerance() {
        let now_ms = now().timestamp_millis();
        let tl = timeline(&[(now_ms - 100 * DAY_MS, Some("acct-a"))]);
        let reader = StubReader::new(vec![
            turn(now_ms - DAY_MS - 5_000, 100, 10), // same turn, 5 s skew
            turn(now_ms - 2 * DAY_MS, 100, 10),     // genuinely different turn
        ]);
        let measured = [MeasuredKey {
            ts: now_ms - DAY_MS,
            input_tokens: 100,
            output_tokens: 10,
        }];
        let report = aggregate_local(&reader, Some(&tl), Some("acct-a"), &measured, now());
        assert_eq!(report.last_30_days.runs, 1);
        assert_eq!(report.last_30_days.input_tokens, 100);
    }

    #[test]
    fn unavailable_reader_propagates_reason_without_inventing_zeros() {
        struct Gone;
        impl LocalUsageReader for Gone {
            fn agent(&self) -> &'static str {
                "gone"
            }
            fn source_label(&self) -> &'static str {
                "local logs"
            }
            fn read_since(&self, _since_ms: i64) -> ReadOutcome {
                ReadOutcome::unavailable("no session directory")
            }
        }
        let report = aggregate_local(&Gone, None, None, &[], now());
        assert_eq!(report.coverage, UsageCoverage::Unavailable);
        assert_eq!(report.reason.as_deref(), Some("no session directory"));
        assert_eq!(report.last_30_days.runs, 0);
    }

    #[test]
    fn untagged_agent_counts_all_its_turns() {
        let now_ms = now().timestamp_millis();
        // Agent without Komet-managed accounts: timeline records account=None.
        let tl = timeline(&[(now_ms - 100 * DAY_MS, None)]);
        let reader = StubReader::new(vec![turn(now_ms - DAY_MS, 10, 1)]);
        let report = aggregate_local(&reader, Some(&tl), None, &[], now());
        assert_eq!(report.coverage, UsageCoverage::Complete);
        assert_eq!(report.last_30_days.runs, 1);
    }

    #[test]
    fn cost_is_estimated_only_for_priced_models() {
        let now_ms = now().timestamp_millis();
        let tl = timeline(&[(now_ms - 100 * DAY_MS, Some("acct-a"))]);
        let mut priced = turn(now_ms - DAY_MS, 1_000_000, 0);
        priced.model = Some("gpt-5".into());
        let mut unpriced = turn(now_ms - DAY_MS, 1_000_000, 0);
        unpriced.model = Some("mystery-model".into());
        let reader = StubReader::new(vec![priced, unpriced]);
        let report = aggregate_local(&reader, Some(&tl), Some("acct-a"), &[], now());
        let cost = report.day_cost.expect("priced model present");
        assert!(cost.usd > 0.0);
        assert_eq!(cost.unpriced_models, vec!["mystery-model".to_string()]);
    }
}
