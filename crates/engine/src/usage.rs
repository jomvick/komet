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

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use komet_proto::{AgentUsageWindow, HarnessId, UsageTotals};

/// Rolling retention for both files.
pub const RETENTION_DAYS: i64 = 180;

const TURNS_FILE: &str = "turns.jsonl";
const QUOTA_FILE: &str = "quota.jsonl";

/// One measured turn, derived from `AgentEvent::Usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    /// Epoch millis.
    pub ts: i64,
    pub chat: String,
    pub harness: HarnessId,
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

    /// Per-agent totals for turns at or after `since`.
    pub fn totals(&self, since: DateTime<Utc>) -> HashMap<HarnessId, UsageTotals> {
        let cutoff = since.timestamp_millis();
        let mut totals: HashMap<HarnessId, UsageTotals> = HashMap::new();
        self.for_each::<TurnUsage>(TURNS_FILE, |turn| {
            if turn.ts < cutoff {
                return;
            }
            let entry = totals.entry(turn.harness).or_default();
            entry.runs += 1;
            entry.input_tokens += turn.input_tokens;
            entry.cached_input_tokens += turn.cached_input_tokens;
            entry.output_tokens += turn.output_tokens;
            entry.reasoning_tokens += turn.reasoning_tokens;
        });
        totals
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
        for file in [TURNS_FILE, QUOTA_FILE] {
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
            model: Some("model".into()),
            project: Some("/repo".into()),
            input_tokens: input,
            cached_input_tokens: 10,
            output_tokens: 2,
            reasoning_tokens: 3,
        }
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
}
