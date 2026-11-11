//! Local, unsynced usage history behind Settings › Usage.
//!
//! Two append-only JSONL files under `{store_root}/usage/`:
//! - `turns.jsonl` — one [`TurnUsage`] per measured turn (no message text,
//!   prompts, credentials, or provider tokens);
//! - `quota.jsonl` — one [`QuotaSample`] per successful provider quota probe,
//!   keyed by device, agent, account and window.
//!
//! Retention is a rolling [`RETENTION_DAYS`]; pruning rewrites the file through
//! a temp file + rename so a crash mid-prune never truncates history. Torn or
//! malformed lines are skipped on read (a crash mid-append may leave one).

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use komet_proto::{
    AgentUsageWindow, HarnessId, UsageBreakdown, UsageDetail, UsagePoint, UsageSeries, UsageTotals,
};

/// Rolling retention for both files.
pub const RETENTION_DAYS: i64 = 180;

/// Rows kept per breakdown dimension (models / projects).
const BREAKDOWN_TOP: usize = 5;

const TURNS_FILE: &str = "turns.jsonl";
const QUOTA_FILE: &str = "quota.jsonl";
const RATE_LIMITS_FILE: &str = "rate_limits.jsonl";

/// One measured turn, derived from `AgentEvent::Usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    /// Epoch millis.
    pub ts: i64,
    pub chat: String,
    pub harness: HarnessId,
    /// `account_key` of the provider account active when the turn ran
    /// (`None` = untagged: legacy rows or a provider without Komet-managed
    /// account switching). Lets the dashboard attribute measured usage to
    /// the account selected in Providers (issue #53).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Session working directory — the project the tokens were spent in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// One provider rate-limit rejection observed on a run (`AgentEvent::Error`
/// with `kind = RateLimit`). Counts only — never the error text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitRecord {
    /// Epoch millis.
    pub ts: i64,
    pub harness: HarnessId,
    /// `account_key` active when the rejection happened (`None` = untagged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// One provider quota probe result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSample {
    /// Epoch millis.
    pub ts: i64,
    /// Device that ran the probe (the store itself is already per-device, but
    /// the sample stays self-describing for exports and tests).
    pub device: String,
    pub harness: HarnessId,
    /// Account identity (email), not a credential.
    pub account: String,
    pub windows: Vec<AgentUsageWindow>,
}

impl QuotaSample {
    /// `(agent, account)` bucket key.
    pub fn bucket(&self) -> String {
        format!("{}:{}", self.harness.as_str(), self.account)
    }
}

/// Path-backed usage store. Writes append; reads parse whole files (bounded by
/// retention and the dashboard's fixed periods).
#[derive(Debug, Clone)]
pub struct UsageStore {
    dir: PathBuf,
}

impl UsageStore {
    /// Open (and create) the store under `root/usage`, pruning expired lines.
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join("usage");
        fs::create_dir_all(&dir)?;
        let store = Self { dir };
        store.retain_since(Utc::now() - Duration::days(RETENTION_DAYS))?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn record_turn(&self, turn: &TurnUsage) -> io::Result<()> {
        append_line(&self.dir.join(TURNS_FILE), turn)
    }

    pub fn record_quota(&self, sample: &QuotaSample) -> io::Result<()> {
        append_line(&self.dir.join(QUOTA_FILE), sample)
    }

    pub fn record_rate_limit(&self, harness: HarnessId, account: Option<String>) -> io::Result<()> {
        append_line(
            &self.dir.join(RATE_LIMITS_FILE),
            &RateLimitRecord {
                ts: Utc::now().timestamp_millis(),
                harness,
                account,
            },
        )
    }

    /// One-shot import of measured usage from the per-chat run journals.
    ///
    /// `turns.jsonl` only records turns that happened after the feature
    /// shipped; every earlier turn still sits in
    /// `{store_root}/journals/{chat}.jsonl` as `usage` events. This walks
    /// those files ONCE (a `journal-import.done` marker guards re-runs), so
    /// the dashboard shows real history on first open instead of a silent
    /// zero that looks like "this agent never reports usage".
    ///
    /// Journal lines carry no timestamp: each row is stamped with its file's
    /// mtime — for the normal single-session chat that IS the last activity
    /// time, right when the final usage events landed. `account` stays
    /// untagged on purpose: which account was active back then is
    /// unknowable, and untagged rows are the legacy-visible kind by design.
    ///
    /// Returns the number of rows appended (0 when the marker already ran).
    pub fn import_journals_once(&self, journals_dir: &Path) -> io::Result<u64> {
        let marker = self.dir.join("journal-import.done");
        if marker.exists() {
            return Ok(0);
        }
        let mut imported = 0u64;
        let entries: Vec<_> = match fs::read_dir(journals_dir) {
            Ok(entries) => entries.flatten().collect(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(chat) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| !stem.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let ts = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or_else(|| Utc::now().timestamp_millis());
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            let mut harness = None;
            let mut model = None;
            let mut project = None;
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                let Ok(line) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                // Journal lines wrap the event: {"seq":N,"event":{…}}.
                let event = line.get("event").unwrap_or(&line);
                match event.get("type").and_then(serde_json::Value::as_str) {
                    Some("sessionStarted") => {
                        harness = event
                            .get("harness")
                            .cloned()
                            .and_then(|value| serde_json::from_value(value).ok());
                        model = event
                            .get("model")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                        project = event
                            .get("cwd")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                    }
                    Some("usage") => {
                        let Some(harness) = harness else {
                            continue;
                        };
                        let field = |key: &str| {
                            event
                                .get(key)
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0)
                        };
                        let turn = TurnUsage {
                            ts,
                            chat: chat.clone(),
                            harness,
                            account: None,
                            model: model.clone(),
                            project: project.clone(),
                            input_tokens: field("inputTokens"),
                            cached_input_tokens: field("cachedInputTokens"),
                            output_tokens: field("outputTokens"),
                            reasoning_tokens: field("reasoningTokens"),
                        };
                        if self.record_turn(&turn).is_ok() {
                            imported += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        // The marker lands even on a partial read: a torn journal must not
        // re-import forever, and live recording covers everything after boot.
        let _ = fs::write(&marker, b"done\n");
        Ok(imported)
    }

    /// Rate-limit rejections per agent at or after `since`. `None` when the
    /// journal file does not exist yet — the feature predates nothing, but a
    /// missing file means "never observed", which the UI must not paint as a
    /// confident zero.
    pub fn rate_limit_counts(
        &self,
        since: DateTime<Utc>,
        accounts: &HashMap<HarnessId, String>,
    ) -> Option<HashMap<HarnessId, u64>> {
        let path = self.dir.join(RATE_LIMITS_FILE);
        if !path.exists() {
            return None;
        }
        let cutoff = since.timestamp_millis();
        let mut counts: HashMap<HarnessId, u64> = HashMap::new();
        self.for_each::<RateLimitRecord>(RATE_LIMITS_FILE, |record| {
            if record.ts >= cutoff
                && account_visible(
                    record.account.as_deref(),
                    accounts.get(&record.harness).map(String::as_str),
                )
            {
                *counts.entry(record.harness).or_default() += 1;
            }
        });
        Some(counts)
    }

    /// Per-agent totals for turns at or after `since`.
    pub fn totals(&self, since: DateTime<Utc>) -> HashMap<HarnessId, UsageTotals> {
        self.summarize(since, &HashMap::new()).totals
    }

    /// Per-agent totals AND model/project leaders for turns at or after
    /// `since`, gathered in a single pass over `turns.jsonl`. `accounts`
    /// maps harness → active `account_key`; rows tagged with a different
    /// account are excluded (untagged legacy rows always count — they cannot
    /// be attributed and hiding them would erase history on upgrade).
    pub fn summarize(
        &self,
        since: DateTime<Utc>,
        accounts: &HashMap<HarnessId, String>,
    ) -> PeriodUsage {
        let cutoff = since.timestamp_millis();
        let mut totals: HashMap<HarnessId, UsageTotals> = HashMap::new();
        let mut details: HashMap<HarnessId, DetailAcc> = HashMap::new();
        self.for_each::<TurnUsage>(TURNS_FILE, |turn| {
            if turn.ts < cutoff
                || !account_visible(
                    turn.account.as_deref(),
                    accounts.get(&turn.harness).map(String::as_str),
                )
            {
                return;
            }
            let entry = totals.entry(turn.harness).or_default();
            entry.runs += 1;
            entry.input_tokens += turn.input_tokens;
            entry.cached_input_tokens += turn.cached_input_tokens;
            entry.output_tokens += turn.output_tokens;
            entry.reasoning_tokens += turn.reasoning_tokens;

            let detail = details.entry(turn.harness).or_default();
            accumulate(&mut detail.models, turn.model.clone(), turn);
            accumulate(&mut detail.projects, turn.project.clone(), turn);
        });
        PeriodUsage {
            totals,
            details: details
                .into_iter()
                .map(|(harness, acc)| (harness, acc.into_detail()))
                .collect(),
        }
    }

    /// Totals for turns in `[start, end)` per agent — the previous-period
    /// half of the dashboard comparison (M6).
    pub fn range_totals(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        accounts: &HashMap<HarnessId, String>,
    ) -> HashMap<HarnessId, UsageTotals> {
        let (start, end) = (start.timestamp_millis(), end.timestamp_millis());
        let mut totals: HashMap<HarnessId, UsageTotals> = HashMap::new();
        self.for_each::<TurnUsage>(TURNS_FILE, |turn| {
            if turn.ts >= start
                && turn.ts < end
                && account_visible(
                    turn.account.as_deref(),
                    accounts.get(&turn.harness).map(String::as_str),
                )
            {
                accumulate_totals(totals.entry(turn.harness).or_default(), turn);
            }
        });
        totals
    }

    /// Hourly buckets for the last 24 h and daily buckets for the last
    /// 30 days, per agent, aligned to UTC boundaries with empty buckets
    /// present as zeros (M6 mini-chart).
    pub fn series(
        &self,
        now: DateTime<Utc>,
        accounts: &HashMap<HarnessId, String>,
    ) -> HashMap<HarnessId, UsageSeries> {
        const HOUR_MS: i64 = 3_600_000;
        const DAY_MS: i64 = 86_400_000;
        let now_ms = now.timestamp_millis();
        let hourly_start = (now_ms - 24 * HOUR_MS).div_euclid(HOUR_MS) * HOUR_MS;
        let daily_start = (now_ms - 30 * DAY_MS).div_euclid(DAY_MS) * DAY_MS;
        let mut hourly: HashMap<HarnessId, BTreeMap<i64, UsageTotals>> = HashMap::new();
        let mut daily: HashMap<HarnessId, BTreeMap<i64, UsageTotals>> = HashMap::new();
        self.for_each::<TurnUsage>(TURNS_FILE, |turn| {
            if turn.ts < daily_start
                || !account_visible(
                    turn.account.as_deref(),
                    accounts.get(&turn.harness).map(String::as_str),
                )
            {
                return;
            }
            accumulate_totals(
                daily
                    .entry(turn.harness)
                    .or_default()
                    .entry(turn.ts.div_euclid(DAY_MS) * DAY_MS)
                    .or_default(),
                turn,
            );
            if turn.ts >= hourly_start {
                accumulate_totals(
                    hourly
                        .entry(turn.harness)
                        .or_default()
                        .entry(turn.ts.div_euclid(HOUR_MS) * HOUR_MS)
                        .or_default(),
                    turn,
                );
            }
        });
        let expand = |buckets: &BTreeMap<i64, UsageTotals>, start: i64, step: i64| {
            (start..=now_ms)
                .step_by(step as usize)
                .map(|at| UsagePoint {
                    at,
                    totals: buckets.get(&at).copied().unwrap_or_default(),
                })
                .collect::<Vec<_>>()
        };
        let mut series: HashMap<HarnessId, UsageSeries> = HashMap::new();
        for (harness, buckets) in hourly {
            let day_buckets = daily.remove(&harness).unwrap_or_default();
            series.insert(
                harness,
                UsageSeries {
                    hourly: expand(&buckets, hourly_start, HOUR_MS),
                    daily: expand(&day_buckets, daily_start, DAY_MS),
                },
            );
        }
        for (harness, buckets) in daily {
            series.entry(harness).or_default().daily = expand(&buckets, daily_start, DAY_MS);
        }
        series
    }

    /// Newest quota sample timestamp per `(agent, account)` bucket.
    pub fn last_quota_at(&self) -> HashMap<String, DateTime<Utc>> {
        let mut latest: HashMap<String, DateTime<Utc>> = HashMap::new();
        self.for_each::<QuotaSample>(QUOTA_FILE, |sample| {
            let key = sample.bucket();
            if let Some(at) = DateTime::from_timestamp_millis(sample.ts)
                && latest.get(&key).is_none_or(|old| at > *old)
            {
                latest.insert(key, at);
            }
        });
        latest
    }

    /// Newest-first quota samples for one `(agent, account)` bucket.
    pub fn quota_samples(
        &self,
        harness: HarnessId,
        account: &str,
        max: usize,
    ) -> Vec<(DateTime<Utc>, Vec<AgentUsageWindow>)> {
        let key = format!("{}:{account}", harness.as_str());
        let mut samples: Vec<(DateTime<Utc>, Vec<AgentUsageWindow>)> = Vec::new();
        self.for_each::<QuotaSample>(QUOTA_FILE, |sample| {
            if sample.bucket() != key {
                return;
            }
            if let Some(at) = DateTime::from_timestamp_millis(sample.ts) {
                samples.push((at, sample.windows.clone()));
            }
        });
        samples.sort_by_key(|sample| std::cmp::Reverse(sample.0));
        samples.truncate(max);
        samples
    }

    /// Drop lines older than `cutoff` (rewrite only when something expired).
    pub fn retain_since(&self, cutoff: DateTime<Utc>) -> io::Result<()> {
        let cutoff = cutoff.timestamp_millis();
        for file in [TURNS_FILE, QUOTA_FILE, RATE_LIMITS_FILE] {
            let path = self.dir.join(file);
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            let mut kept = Vec::with_capacity(bytes.len());
            let mut dropped = false;
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_slice::<serde_json::Value>(line) {
                    Ok(value)
                        if value.get("ts").and_then(|v| v.as_i64()).unwrap_or(0) >= cutoff =>
                    {
                        kept.extend_from_slice(line);
                        kept.push(b'\n');
                    }
                    _ => dropped = true,
                }
            }
            if dropped || kept.len() != bytes.len() {
                write_atomic(&path, &kept)?;
            }
        }
        Ok(())
    }

    fn for_each<T: DeserializeOwned>(&self, file: &str, mut visit: impl FnMut(&T)) {
        let Ok(file) = fs::File::open(self.dir.join(file)) else {
            return;
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<T>(&line) {
                visit(&value);
            }
        }
    }
}

/// One measured window's aggregates: per-agent totals plus the leading
/// models and projects (issue #53, M6).
#[derive(Debug, Default)]
pub struct PeriodUsage {
    pub totals: HashMap<HarnessId, UsageTotals>,
    pub details: HashMap<HarnessId, UsageDetail>,
}

#[derive(Default)]
struct DetailAcc {
    models: HashMap<Option<String>, UsageTotals>,
    projects: HashMap<Option<String>, UsageTotals>,
}

impl DetailAcc {
    fn into_detail(self) -> UsageDetail {
        UsageDetail {
            models: top_breakdown(self.models),
            projects: top_breakdown(self.projects),
        }
    }
}

/// A journal row counts toward its harness's *active* account when it was
/// recorded under that account — or when it carries no account at all
/// (legacy rows predate tagging and providers without Komet-managed account
/// switching never tag). Rows tagged with a different account are excluded.
fn account_visible(recorded: Option<&str>, active: Option<&str>) -> bool {
    match (recorded, active) {
        (_, None) | (None, Some(_)) => true,
        (Some(recorded), Some(active)) => recorded == active,
    }
}

fn accumulate_totals(entry: &mut UsageTotals, turn: &TurnUsage) {
    entry.runs += 1;
    entry.input_tokens += turn.input_tokens;
    entry.cached_input_tokens += turn.cached_input_tokens;
    entry.output_tokens += turn.output_tokens;
    entry.reasoning_tokens += turn.reasoning_tokens;
}

fn accumulate(
    rows: &mut HashMap<Option<String>, UsageTotals>,
    label: Option<String>,
    turn: &TurnUsage,
) {
    accumulate_totals(rows.entry(label).or_default(), turn);
}

/// Rank by tokens actually generated/consumed (input + output; cached input
/// is a subset of input and reasoning a subset of output on most providers),
/// then label for a stable order.
fn top_breakdown(rows: HashMap<Option<String>, UsageTotals>) -> Vec<UsageBreakdown> {
    let mut rows: Vec<(Option<String>, UsageTotals)> = rows.into_iter().collect();
    rows.sort_by(|(a_label, a), (b_label, b)| {
        let a_tokens = a.input_tokens + a.output_tokens;
        let b_tokens = b.input_tokens + b.output_tokens;
        b_tokens.cmp(&a_tokens).then_with(|| a_label.cmp(b_label))
    });
    rows.truncate(BREAKDOWN_TOP);
    rows.into_iter()
        .map(|(label, totals)| UsageBreakdown { label, totals })
        .collect()
}

fn append_line<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&line)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (UsageStore, PathBuf) {
        let root = std::env::temp_dir().join(format!("komet-usage-test-{}", uuid::Uuid::new_v4()));
        let store = UsageStore::open(&root).expect("open");
        (store, root)
    }

    fn turn(ts: i64, harness: HarnessId, input: u64) -> TurnUsage {
        TurnUsage {
            ts,
            chat: "chat-1".into(),
            harness,
            account: None,
            model: Some("model".into()),
            project: Some("/repo".into()),
            input_tokens: input,
            cached_input_tokens: 10,
            output_tokens: 2,
            reasoning_tokens: 3,
        }
    }

    #[test]
    fn journal_import_reads_usage_events_once() {
        let (store, root) = store();
        let journals = root.join("journals");
        fs::create_dir_all(&journals).unwrap();
        fs::write(
            journals.join("chat-a.jsonl"),
            concat!(
                "{\"seq\":1,\"event\":{\"type\":\"sessionStarted\",\"harness\":\"opencode\",\"model\":\"m1\",\"cwd\":\"/repo\"}}\n",
                "{\"seq\":2,\"event\":{\"type\":\"usage\",\"inputTokens\":100,\"cachedInputTokens\":40,\"outputTokens\":5,\"reasoningTokens\":1}}\n",
                "{\"seq\":3,\"event\":{\"type\":\"usage\",\"inputTokens\":50,\"outputTokens\":2}}\n",
                "{\"seq\":4,\"event\":{\"type\":\"textDelta\",\"text\":\"hi\"}}\n",
            ),
        )
        .unwrap();
        // A journal with usage before any sessionStarted is skipped.
        fs::write(
            journals.join("chat-orphan.jsonl"),
            "{\"seq\":1,\"event\":{\"type\":\"usage\",\"inputTokens\":999,\"outputTokens\":1}}\n",
        )
        .unwrap();

        let imported = store.import_journals_once(&journals).unwrap();
        assert_eq!(imported, 2);
        // Second run is a no-op.
        assert_eq!(store.import_journals_once(&journals).unwrap(), 0);

        let totals = store.totals(Utc::now() - Duration::days(365));
        let opencode = totals.get(&HarnessId::Opencode).expect("opencode");
        assert_eq!(opencode.runs, 2);
        assert_eq!(opencode.input_tokens, 150);
        assert_eq!(opencode.cached_input_tokens, 40);
        assert_eq!(opencode.output_tokens, 7);
        assert_eq!(opencode.reasoning_tokens, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn turns_roundtrip_and_aggregate_per_agent() {
        let (store, root) = store();
        let now = Utc::now();
        let ts = now.timestamp_millis();
        store
            .record_turn(&turn(ts, HarnessId::ClaudeCode, 100))
            .unwrap();
        store
            .record_turn(&turn(ts, HarnessId::ClaudeCode, 50))
            .unwrap();
        store.record_turn(&turn(ts, HarnessId::Codex, 7)).unwrap();

        let totals = store.totals(now - Duration::hours(1));
        let claude = totals.get(&HarnessId::ClaudeCode).expect("claude");
        assert_eq!(claude.runs, 2);
        assert_eq!(claude.input_tokens, 150);
        assert_eq!(claude.cached_input_tokens, 20);
        assert_eq!(claude.output_tokens, 4);
        assert_eq!(claude.reasoning_tokens, 6);
        assert_eq!(totals[&HarnessId::Codex].input_tokens, 7);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn totals_respect_the_since_boundary() {
        let (store, root) = store();
        let now = Utc::now();
        store
            .record_turn(&turn(
                (now - Duration::days(2)).timestamp_millis(),
                HarnessId::Codex,
                99,
            ))
            .unwrap();
        store
            .record_turn(&turn(now.timestamp_millis(), HarnessId::Codex, 1))
            .unwrap();
        let day = store.totals(now - Duration::days(1));
        assert_eq!(day[&HarnessId::Codex].runs, 1);
        assert_eq!(day[&HarnessId::Codex].input_tokens, 1);
        let month = store.totals(now - Duration::days(30));
        assert_eq!(month[&HarnessId::Codex].runs, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retention_drops_expired_and_malformed_lines() {
        let root = std::env::temp_dir().join(format!("komet-usage-test-{}", uuid::Uuid::new_v4()));
        let dir = root.join("usage");
        fs::create_dir_all(&dir).unwrap();
        let old = Utc::now() - Duration::days(RETENTION_DAYS + 5);
        let fresh = Utc::now().timestamp_millis();
        let mut body = Vec::new();
        body.extend_from_slice(
            serde_json::to_string(&turn(old.timestamp_millis(), HarnessId::Codex, 5))
                .unwrap()
                .as_bytes(),
        );
        body.push(b'\n');
        body.extend_from_slice(b"{\"ts\": 1, torn");
        body.push(b'\n');
        body.extend_from_slice(
            serde_json::to_string(&turn(fresh, HarnessId::Codex, 6))
                .unwrap()
                .as_bytes(),
        );
        body.push(b'\n');
        fs::write(dir.join(TURNS_FILE), body).unwrap();

        let store = UsageStore::open(&root).unwrap();
        let totals = store.totals(old);
        assert_eq!(totals[&HarnessId::Codex].runs, 1, "old and torn lines gone");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_store_has_no_totals() {
        let (store, root) = store();
        assert!(store.totals(Utc::now() - Duration::days(30)).is_empty());
        assert!(store.last_quota_at().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn breakdown_ranks_models_and_projects_per_agent() {
        let (store, root) = store();
        let now = Utc::now();
        let ts = now.timestamp_millis();
        let mut heavy = turn(ts, HarnessId::ClaudeCode, 100);
        heavy.model = Some("model-big".into());
        heavy.project = Some("/repo/a".into());
        let mut light = turn(ts, HarnessId::ClaudeCode, 10);
        light.model = Some("model-small".into());
        light.project = Some("/repo/b".into());
        let mut other_agent = turn(ts, HarnessId::Codex, 5);
        other_agent.model = None;
        other_agent.project = None;
        store.record_turn(&heavy).unwrap();
        store.record_turn(&light).unwrap();
        store.record_turn(&other_agent).unwrap();

        let usage = store.summarize(now - Duration::hours(1), &HashMap::new());
        let claude = usage.details.get(&HarnessId::ClaudeCode).expect("claude");
        assert_eq!(claude.models[0].label.as_deref(), Some("model-big"));
        assert_eq!(claude.models[0].totals.input_tokens, 100);
        assert_eq!(claude.models[1].label.as_deref(), Some("model-small"));
        assert_eq!(claude.projects[0].label.as_deref(), Some("/repo/a"));
        assert_eq!(claude.projects.len(), 2);
        // A turn without model/project still lands under a `None` bucket.
        let codex = usage.details.get(&HarnessId::Codex).expect("codex");
        assert_eq!(codex.models[0].label, None);
        assert_eq!(codex.models[0].totals.runs, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn breakdown_respects_the_since_boundary() {
        let (store, root) = store();
        let now = Utc::now();
        let mut old = turn(
            (now - Duration::days(2)).timestamp_millis(),
            HarnessId::Codex,
            99,
        );
        old.model = Some("old-model".into());
        store.record_turn(&old).unwrap();
        let usage = store.summarize(now - Duration::days(1), &HashMap::new());
        assert!(usage.details.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn quota_samples_report_latest_per_bucket() {
        let (store, root) = store();
        let now = Utc::now();
        let window = AgentUsageWindow {
            label: "Session".into(),
            used_fraction: 0.5,
            resets_at: None,
            display_value: None,
            window_seconds: Some(18_000),
        };
        let sample = |ts: i64| QuotaSample {
            ts,
            device: "device-1".into(),
            harness: HarnessId::ClaudeCode,
            account: "me@example.com".into(),
            windows: vec![window.clone()],
        };
        store
            .record_quota(&sample((now - Duration::hours(2)).timestamp_millis()))
            .unwrap();
        store.record_quota(&sample(now.timestamp_millis())).unwrap();
        let latest = store.last_quota_at();
        let at = latest["claude-code:me@example.com"];
        assert_eq!(at.timestamp_millis(), now.timestamp_millis());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rate_limit_counts_are_unknown_until_the_journal_exists() {
        let (store, root) = store();
        let now = Utc::now();
        // No rate_limits.jsonl yet → None, never a confident zero.
        assert_eq!(
            store.rate_limit_counts(now - Duration::days(1), &HashMap::new()),
            None
        );
        store
            .record_rate_limit(HarnessId::ClaudeCode, None)
            .unwrap();
        store
            .record_rate_limit(HarnessId::ClaudeCode, None)
            .unwrap();
        store.record_rate_limit(HarnessId::Codex, None).unwrap();
        let day = store
            .rate_limit_counts(now - Duration::days(1), &HashMap::new())
            .unwrap();
        assert_eq!(day[&HarnessId::ClaudeCode], 2);
        assert_eq!(day[&HarnessId::Codex], 1);
        // A window that excludes everything still reports a known empty map.
        let future = store
            .rate_limit_counts(now + Duration::days(1), &HashMap::new())
            .unwrap();
        assert!(future.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn series_buckets_hourly_and_daily_with_empty_slots() {
        let (store, root) = store();
        let now = Utc::now();
        let hour_ms = 3_600_000i64;
        let current_hour = now.timestamp_millis().div_euclid(hour_ms) * hour_ms;
        store
            .record_turn(&turn(current_hour, HarnessId::Codex, 100))
            .unwrap();
        store
            .record_turn(&turn(
                (now - Duration::days(3)).timestamp_millis(),
                HarnessId::Codex,
                7,
            ))
            .unwrap();
        let series = store.series(now, &HashMap::new());
        let codex = series.get(&HarnessId::Codex).expect("codex series");
        assert_eq!(codex.hourly.len(), 25, "24h of hour buckets + edge");
        assert_eq!(codex.daily.len(), 31, "30d of day buckets + edge");
        let last_hour = codex.hourly.last().unwrap();
        assert_eq!(last_hour.at, current_hour);
        assert_eq!(last_hour.totals.input_tokens, 100);
        // The 3-day-old turn lands in its own daily bucket, not the hourly one.
        assert!(codex.hourly.iter().all(|p| p.totals.input_tokens <= 100));
        assert_eq!(
            codex
                .daily
                .iter()
                .map(|p| p.totals.input_tokens)
                .sum::<u64>(),
            107
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn range_totals_cover_exactly_the_previous_window() {
        let (store, root) = store();
        let now = Utc::now();
        store
            .record_turn(&turn(
                (now - Duration::days(1) - Duration::hours(1)).timestamp_millis(),
                HarnessId::Codex,
                10,
            ))
            .unwrap();
        store
            .record_turn(&turn(
                (now - Duration::days(1) + Duration::hours(1)).timestamp_millis(),
                HarnessId::Codex,
                20,
            ))
            .unwrap();
        let prev = store.range_totals(
            now - Duration::days(2),
            now - Duration::days(1),
            &HashMap::new(),
        );
        assert_eq!(prev[&HarnessId::Codex].input_tokens, 10);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_filter_keeps_active_and_untagged_rows_only() {
        let (store, root) = store();
        let now = Utc::now();
        let ts = now.timestamp_millis();
        let mut tagged = turn(ts, HarnessId::Copilot, 100);
        tagged.account = Some("acct-a".into());
        let mut other = turn(ts, HarnessId::Copilot, 50);
        other.account = Some("acct-b".into());
        store.record_turn(&tagged).unwrap();
        store.record_turn(&other).unwrap();
        store.record_turn(&turn(ts, HarnessId::Copilot, 7)).unwrap(); // untagged

        let mut accounts = HashMap::new();
        accounts.insert(HarnessId::Copilot, "acct-a".to_string());
        let totals = store.totals(now - Duration::hours(1));
        // No account map → everything counts.
        assert_eq!(totals[&HarnessId::Copilot].input_tokens, 157);

        let usage = store.summarize(now - Duration::hours(1), &accounts);
        // Active account + untagged legacy row; the other account is hidden.
        assert_eq!(usage.totals[&HarnessId::Copilot].input_tokens, 107);
        let _ = fs::remove_dir_all(root);
    }
}
