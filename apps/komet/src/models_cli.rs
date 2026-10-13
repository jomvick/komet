//! `komet models <harness>`: list models for an agent harness.
//! Formats output as an ASCII table, JSON (`--json`), or plain IDs (`-q / --quiet`).

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::ValueEnum;
use komet_proto::{HarnessId, Model, ReasoningLevel};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum HarnessIdArg {
    #[value(alias = "claude", alias = "anthropic")]
    ClaudeCode,
    Cline,
    Codex,
    #[value(alias = "open-code")]
    Opencode,
    Cursor,
    Grok,
    Hermes,
    Pi,
    #[value(alias = "gemini")]
    Antigravity,
    Mock,
}

impl From<HarnessIdArg> for HarnessId {
    fn from(arg: HarnessIdArg) -> Self {
        match arg {
            HarnessIdArg::ClaudeCode => HarnessId::ClaudeCode,
            HarnessIdArg::Cline => HarnessId::Cline,
            HarnessIdArg::Codex => HarnessId::Codex,
            HarnessIdArg::Opencode => HarnessId::Opencode,
            HarnessIdArg::Cursor => HarnessId::Cursor,
            HarnessIdArg::Grok => HarnessId::Grok,
            HarnessIdArg::Hermes => HarnessId::Hermes,
            HarnessIdArg::Pi => HarnessId::Pi,
            HarnessIdArg::Antigravity => HarnessId::Antigravity,
            HarnessIdArg::Mock => HarnessId::Mock,
        }
    }
}

pub async fn run(
    ipc_port: u16,
    harness_arg: HarnessIdArg,
    json: bool,
    quiet: bool,
    reasoning: bool,
    data_dir: PathBuf,
) -> anyhow::Result<()> {
    let harness: HarnessId = harness_arg.into();

    // 1. Attempt RPC call to running engine
    let models = match fetch_models_via_rpc(ipc_port, harness).await {
        Ok(models) => models,
        Err(_rpc_err) => {
            // Standalone fallback: resolve via engine registry (uses valid cache, live probe, or static catalog)
            let registry = komet_engine::default_registry();
            registry.load_models_cache(&data_dir);
            match registry.models(harness, false).await {
                Ok(discovered) if !discovered.is_empty() => discovered,
                _ => {
                    return Err(anyhow::anyhow!(
                        "could not discover models for {harness_arg:?}.\n\
                         Make sure the corresponding agent CLI is installed and configured."
                    ));
                }
            }
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(());
    }

    if quiet {
        for m in &models {
            println!("{}", m.id);
        }
        return Ok(());
    }

    if models.is_empty() {
        println!("No models found for {harness_arg:?}.");
        return Ok(());
    }

    render_models_table(harness, &models, reasoning);
    Ok(())
}

/// Live-vs-cache drift check over every installed harness (Mock excluded):
/// exit code is non-zero on drift or on any failed probe. Never serves a
/// hardcoded list as success — both sides come from the registry/sondes.
pub async fn run_check(data_dir: PathBuf) -> anyhow::Result<()> {
    let registry = komet_engine::default_registry();
    registry.load_models_cache(&data_dir);
    let installed: Vec<HarnessId> = registry
        .descriptors()
        .into_iter()
        .filter(|d| d.installed && d.id != HarnessId::Mock)
        .map(|d| d.id)
        .collect();
    if installed.is_empty() {
        println!("No agent harness installed.");
        return Ok(());
    }
    let mut failed = false;
    // Snapshot the cache BEFORE any live probe — `probe_live` persists a
    // successful answer (and even `models(id, false)` would re-probe on a
    // stale/missing entry), which would mask the very drift we compare.
    let cached_snapshot = read_cached_model_ids(&data_dir);
    for id in installed {
        // Pure live probe — no stale fallback, the error propagates.
        // Cache view comes from the pre-probe snapshot above, never from a
        // call that could itself have refreshed the cache.
        let cached = cached_snapshot.get(&id);
        match registry.probe_live(id).await {
            Err(e) => {
                println!("{id:?}: sonde en échec: {e}");
                failed = true;
            }
            Ok(live) => match cached {
                None => {
                    println!("{id:?}: absent du cache ({} modèles live)", live.len());
                    failed = true;
                }
                Some(cached_ids) => {
                    let live_ids: std::collections::BTreeSet<&str> =
                        live.iter().map(|m| m.id.as_str()).collect();
                    let cached_ids: std::collections::BTreeSet<&str> =
                        cached_ids.iter().map(String::as_str).collect();
                    if cached_ids != live_ids {
                        println!(
                            "{id:?}: drift live != cache (cache: {}, live: {})",
                            cached_ids.len(),
                            live_ids.len()
                        );
                        failed = true;
                    } else {
                        println!("{id:?}: OK ({} modèles)", live_ids.len());
                    }
                }
            },
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Model ids per harness from `{data_dir}/model-cache.json`, read raw (no
/// registry resolve, no probe, no persist). Missing/corrupt file → empty map
/// (every installed harness then reports "absent du cache").
fn read_cached_model_ids(
    data_dir: &std::path::Path,
) -> std::collections::HashMap<HarnessId, std::collections::BTreeSet<String>> {
    use std::collections::{BTreeSet, HashMap};

    let mut out: HashMap<HarnessId, BTreeSet<String>> = HashMap::new();
    let text = std::fs::read_to_string(data_dir.join("model-cache.json"));
    let value: serde_json::Value = match text.ok().and_then(|t| serde_json::from_str(&t).ok()) {
        Some(v) => v,
        None => return out,
    };
    let Some(entries) = value.get("entries").and_then(|e| e.as_object()) else {
        return out;
    };
    for (key, entry) in entries {
        let Ok(id) = serde_json::from_value::<HarnessId>(serde_json::Value::String(key.clone()))
        else {
            continue;
        };
        let ids: BTreeSet<String> = entry
            .get("models")
            .and_then(|m| m.as_array())
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| m.get("id")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !ids.is_empty() {
            out.insert(id, ids);
        }
    }
    out
}

async fn fetch_models_via_rpc(ipc_port: u16, harness: HarnessId) -> anyhow::Result<Vec<Model>> {
    let token = komet_rpc::ipc_token::read_default(ipc_port)
        .map_err(|e| anyhow::anyhow!("token read failed: {e}"))?;
    let client = komet_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"), &token)
        .await
        .map_err(|e| anyhow::anyhow!("connect_ws failed: {e}"))?;
    let res = client
        .call(
            komet_rpc::methods::LIST_MODELS,
            serde_json::json!({ "harness": harness }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("ListModels RPC call failed: {e}"))?;
    let models: Vec<Model> = serde_json::from_value(res)?;
    Ok(models)
}

/// Classify a model into a display provider group used for section headers
/// (`== {provider} ==`) in the CLI table. Intentional DUPLICATE of
/// `komet_ui::pickers::model_provider` (ruling ledger: no move to komet-proto,
/// hors scope) — kept in parity by `model_provider_parity_*` below, which
/// mirror the T3 asserts. Display labels ONLY, never served as a model list.
fn model_provider(harness: HarnessId, model_id: &str) -> &'static str {
    /// Vendor slug → display label. `None` = unknown slug (caller decides
    /// between the aggregator label and "Other"). `&'static str` only.
    fn vendor_label(slug: &str) -> Option<&'static str> {
        match slug {
            "anthropic" => Some("Anthropic"),
            "openai" => Some("OpenAI"),
            "meta-llama" | "meta" => Some("Meta"),
            "google" | "gemini" => Some("Google Gemini"),
            "mistralai" | "mistral" => Some("Mistral"),
            "qwen" => Some("Qwen / Alibaba"),
            "deepseek" => Some("DeepSeek"),
            "nousresearch" | "nous" => Some("Nous Research"),
            "x-ai" | "xai" => Some("xAI"),
            "cohere" => Some("Cohere"),
            "microsoft" => Some("Microsoft"),
            "nvidia" => Some("NVIDIA"),
            "perplexity" => Some("Perplexity"),
            _ => None,
        }
    }
    let id = model_id;
    match harness {
        HarnessId::Antigravity => {
            if id.contains("gemini") {
                "Google Gemini"
            } else if id.contains("claude") {
                "Anthropic"
            } else if id.starts_with("gpt") || id.starts_with("o1") || id.starts_with("o3") {
                "OpenAI"
            } else {
                "Other"
            }
        }
        HarnessId::ClaudeCode => "Anthropic",
        HarnessId::Codex => "OpenAI",
        HarnessId::Cursor => "Cursor Curated",
        // ACP harnesses (Cline / Hermes / OpenCode): provider prefix before `/`
        _ => {
            // Free-tier shortcut — checked before any vendor derivation.
            if id.starts_with("cline-free/") || id.ends_with(":free") {
                return "Free Models";
            }
            if id.starts_with("nous:") {
                return "Nous Research";
            }
            let mut parts = id.split('/');
            let vendor = parts.next().unwrap_or("");
            // Aggregator alias — derive the real vendor behind the prefix.
            // The segment after `openrouter/` is the real vendor, but it is
            // often fused with the model name (`openrouter/deepseek-v3`):
            // try the whole slug, then its leading `-`-delimited token
            // (`deepseek-v3` → `deepseek` → DeepSeek), else stay OpenRouter.
            if vendor == "openrouter" {
                let slug = parts.next().unwrap_or("");
                if let Some(label) =
                    vendor_label(slug).or_else(|| slug.split('-').next().and_then(vendor_label))
                {
                    return label;
                }
                return "OpenRouter";
            }
            // Slash-separated `vendor/model` ids.
            if vendor != id {
                if let Some(label) = vendor_label(vendor) {
                    return label;
                }
                return "Other";
            }
            // Bare ids (no vendor prefix): well-known model-family prefixes.
            if id.starts_with("gpt")
                || id.starts_with("o1")
                || id.starts_with("o3")
                || id.starts_with("openai")
            {
                return "OpenAI";
            }
            if id.contains("gemini") {
                return "Google Gemini";
            }
            if id.contains("claude") {
                return "Anthropic";
            }
            "Other"
        }
    }
}

/// Models sorted by `(provider, label)` so each provider group is contiguous.
fn sorted_by_provider<'a>(harness: HarnessId, models: &'a [Model]) -> Vec<(String, &'a Model)> {
    let mut ordered: Vec<(String, &Model)> = models
        .iter()
        .map(|m| (model_provider(harness, &m.id).to_string(), m))
        .collect();
    ordered.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.label.cmp(&b.1.label))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    ordered
}

fn render_models_table(harness: HarnessId, models: &[Model], show_reasoning: bool) {
    let is_tty = std::io::stdout().is_terminal();

    if show_reasoning {
        let headers = ["ID", "MODEL", "REASONING LEVELS", "OPTIONS"];
        let ordered = sorted_by_provider(harness, models);
        let rows: Vec<(String, [String; 4])> = ordered
            .into_iter()
            .map(|(provider, m)| {
                let reasoning_str = if m.reasoning_levels.is_empty() {
                    "none".into()
                } else {
                    m.reasoning_levels
                        .iter()
                        .map(reasoning_level_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let options_str = if m.options.is_empty() {
                    "none".into()
                } else {
                    m.options
                        .iter()
                        .map(|o| format!("{}: [{}]", o.label, o.default_choice))
                        .collect::<Vec<_>>()
                        .join("; ")
                };
                (
                    provider,
                    [m.id.clone(), m.label.clone(), reasoning_str, options_str],
                )
            })
            .collect();

        let mut widths = [
            headers[0].len(),
            headers[1].len(),
            headers[2].len(),
            headers[3].len(),
        ];
        for row in &rows {
            for i in 0..4 {
                widths[i] = widths[i].max(row.1[i].len());
            }
        }

        // Apply sensible upper bounds for wrapping/readability
        widths[0] = widths[0].clamp(15, 38);
        widths[1] = widths[1].clamp(15, 30);
        widths[2] = widths[2].clamp(16, 32);
        widths[3] = widths[3].clamp(10, 35);

        // Header
        print_row(&headers, &widths, is_tty, true);
        print_separator(&widths);

        // Rows, one `== {provider} ==` section header per group.
        let mut current_group: Option<&str> = None;
        for (provider, row) in &rows {
            if current_group != Some(provider.as_str()) {
                println!("== {provider} ==");
                current_group = Some(provider.as_str());
            }
            print_row(
                &[
                    row[0].as_str(),
                    row[1].as_str(),
                    row[2].as_str(),
                    row[3].as_str(),
                ],
                &widths,
                is_tty,
                false,
            );
        }
    } else {
        let headers = ["ID", "MODEL", "DESCRIPTION"];
        let ordered = sorted_by_provider(harness, models);
        let rows: Vec<(String, [String; 3])> = ordered
            .into_iter()
            .map(|(provider, m)| {
                (
                    provider,
                    [
                        m.id.clone(),
                        m.label.clone(),
                        m.description.clone().unwrap_or_default(),
                    ],
                )
            })
            .collect();

        let mut widths = [headers[0].len(), headers[1].len(), headers[2].len()];
        for row in &rows {
            widths[0] = widths[0].max(row.1[0].len());
            widths[1] = widths[1].max(row.1[1].len());
            widths[2] = widths[2].max(row.1[2].len());
        }

        widths[0] = widths[0].clamp(15, 38);
        widths[1] = widths[1].clamp(15, 30);
        widths[2] = widths[2].clamp(15, 60);

        // Header
        print_row(&headers, &widths, is_tty, true);
        print_separator(&widths);

        // Rows, one `== {provider} ==` section header per group.
        let mut current_group: Option<&str> = None;
        for (provider, row) in &rows {
            if current_group != Some(provider.as_str()) {
                println!("== {provider} ==");
                current_group = Some(provider.as_str());
            }
            print_row(
                &[row[0].as_str(), row[1].as_str(), row[2].as_str()],
                &widths,
                is_tty,
                false,
            );
        }
    }
}

fn print_row(cols: &[&str], widths: &[usize], is_tty: bool, is_header: bool) {
    let mut line = String::new();
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        let width = widths[i];
        let truncated = if col.len() > width {
            if width > 3 {
                format!("{}...", &col[..width - 3])
            } else {
                col[..width].to_string()
            }
        } else {
            col.to_string()
        };
        line.push_str(&format!("{:<width$}", truncated, width = width));
    }

    if is_header && is_tty {
        // ANSI bold for headers
        println!("\x1b[1m{line}\x1b[0m");
    } else {
        println!("{line}");
    }
}

fn print_separator(widths: &[usize]) {
    let mut line = String::new();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(&"-".repeat(*w));
    }
    println!("{line}");
}

fn reasoning_level_str(level: &ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
        ReasoningLevel::Ultracode => "ultracode",
        ReasoningLevel::Ultrathink => "ultrathink",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_harness_arg_conversion() {
        assert_eq!(
            HarnessId::from(HarnessIdArg::ClaudeCode),
            HarnessId::ClaudeCode
        );
        assert_eq!(HarnessId::from(HarnessIdArg::Cline), HarnessId::Cline);
        assert_eq!(HarnessId::from(HarnessIdArg::Codex), HarnessId::Codex);
        assert_eq!(HarnessId::from(HarnessIdArg::Hermes), HarnessId::Hermes);
    }

    /// Parity with `komet_ui::pickers::model_provider` (T3 asserts, adapted):
    /// the CLI duplicate must classify identically. If these drift from the
    /// T3 tests, update the duplicate — never serve a list from these labels.
    #[test]
    fn model_provider_parity_groups_antigravity_hermes_cline_opencode() {
        assert_eq!(
            model_provider(HarnessId::Antigravity, "gemini-3.8-flash-high"),
            "Google Gemini"
        );
        assert_eq!(
            model_provider(HarnessId::Antigravity, "claude-sonnet-4-6"),
            "Anthropic"
        );
        assert_eq!(
            model_provider(HarnessId::Antigravity, "gpt-oss-120b-medium"),
            "OpenAI"
        );
        assert_eq!(
            model_provider(HarnessId::Hermes, "nous:hermes-4-405b"),
            "Nous Research"
        );
        assert_eq!(
            model_provider(HarnessId::Opencode, "openrouter/deepseek-v3"),
            "DeepSeek"
        );
        assert_eq!(
            model_provider(HarnessId::Cline, "cline-free/deepseek-v3:free"),
            "Free Models"
        );
    }

    #[test]
    fn model_provider_parity_derives_vendor_behind_openrouter_and_acp_aliases() {
        // Aggregator alias derives the real vendor; unknown stays OpenRouter.
        assert_eq!(
            model_provider(HarnessId::Opencode, "openrouter/anthropic/claude"),
            "Anthropic"
        );
        assert_eq!(
            model_provider(HarnessId::Opencode, "openrouter/unknown-vendor-x"),
            "OpenRouter"
        );
        // ACP vendor aliases.
        assert_eq!(
            model_provider(HarnessId::Hermes, "nousresearch/hermes-4"),
            "Nous Research"
        );
        assert_eq!(model_provider(HarnessId::Cline, "xai/grok-4"), "xAI");
        assert_eq!(model_provider(HarnessId::Cline, "x-ai/grok-4"), "xAI");
        assert_eq!(
            model_provider(HarnessId::Opencode, "google/gemini-3-flash"),
            "Google Gemini"
        );
        assert_eq!(
            model_provider(HarnessId::Opencode, "gemini/gemini-3-flash"),
            "Google Gemini"
        );
        // Bare (slash-less) family prefixes.
        assert_eq!(model_provider(HarnessId::Opencode, "gpt-4o"), "OpenAI");
        assert_eq!(model_provider(HarnessId::Opencode, "o3-mini"), "OpenAI");
        assert_eq!(
            model_provider(HarnessId::Opencode, "gemini-2.5-flash"),
            "Google Gemini"
        );
        assert_eq!(
            model_provider(HarnessId::Opencode, "claude-sonnet-4"),
            "Anthropic"
        );
        // Unknown stays Other — never a fabricated vendor label.
        assert_eq!(
            model_provider(HarnessId::Opencode, "some-vendor/model-x"),
            "Other"
        );
    }

    #[test]
    fn model_provider_parity_extra_labels_and_free_rule() {
        // Extra labels mirrored from T3 review.
        assert_eq!(
            model_provider(HarnessId::Opencode, "cohere/command-r"),
            "Cohere"
        );
        assert_eq!(
            model_provider(HarnessId::Opencode, "microsoft/phi-4"),
            "Microsoft"
        );
        assert_eq!(
            model_provider(HarnessId::Opencode, "nvidia/nemotron-x"),
            "NVIDIA"
        );
        assert_eq!(
            model_provider(HarnessId::Opencode, "perplexity/sonar-x"),
            "Perplexity"
        );
        assert_eq!(
            model_provider(HarnessId::Cursor, "any-model"),
            "Cursor Curated"
        );
        // `:free` suffix wins over vendor derivation.
        assert_eq!(
            model_provider(HarnessId::Opencode, "openrouter/deepseek-v3:free"),
            "Free Models"
        );
        assert_eq!(
            model_provider(HarnessId::Opencode, "anthropic/claude-x:free"),
            "Free Models"
        );
    }

    #[test]
    fn sorted_by_provider_groups_contiguously() {
        let mk = |id: &str, label: &str| Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        };
        let models = vec![
            mk("openrouter/deepseek-v3", "DeepSeek V3"),
            mk("gemini-3.8-flash-high", "Gemini Flash"),
            mk("claude-sonnet-4-6", "Claude Sonnet"),
        ];
        let ordered = sorted_by_provider(HarnessId::Opencode, &models);
        let groups: Vec<&str> = ordered.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(groups, vec!["Anthropic", "DeepSeek", "Google Gemini"]);
    }
}
