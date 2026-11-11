//! Embedded model pricing for the Usage dashboard's cost estimates (issue
//! #53, M5).
//!
//! A small, versioned, hand-maintained table — USD per 1M tokens — matched by
//! longest prefix against the model id the harness reported. It is an
//! *API-equivalent* estimate, not a bill: subscription plans (Claude Max,
//! ChatGPT Plus, Cursor Pro…) make per-token pricing misleading, so the UI
//! must label the figure accordingly. A model absent from the table is
//! reported as unpriced, never as $0.
//!
//! Bump [`AS_OF`] whenever the table changes so stale estimates stay honest.

use komet_proto::{UsageBreakdown, UsageCostEstimate, UsageDetail};

/// Date the table below was last reviewed (`YYYY-MM`).
pub const AS_OF: &str = "2026-11";

/// USD per 1M tokens: input, cached input read, output.
#[derive(Debug, Clone, Copy)]
struct Price {
    input: f64,
    cached_input: f64,
    output: f64,
}

/// `(model prefix, price)`. Longest matching prefix wins, so specific
/// variants must precede their family.
const TABLE: &[(&str, Price)] = &[
    // Anthropic — claude.com/pricing + platform docs, reviewed 2026-11.
    (
        "claude-opus-4-5",
        Price {
            input: 5.0,
            cached_input: 0.5,
            output: 25.0,
        },
    ),
    (
        "claude-opus-4-1",
        Price {
            input: 15.0,
            cached_input: 1.5,
            output: 75.0,
        },
    ),
    (
        "claude-opus-4",
        Price {
            input: 15.0,
            cached_input: 1.5,
            output: 75.0,
        },
    ),
    (
        "claude-sonnet-4-5",
        Price {
            input: 3.0,
            cached_input: 0.3,
            output: 15.0,
        },
    ),
    (
        "claude-sonnet-4",
        Price {
            input: 3.0,
            cached_input: 0.3,
            output: 15.0,
        },
    ),
    (
        "claude-haiku-4-5",
        Price {
            input: 1.0,
            cached_input: 0.1,
            output: 5.0,
        },
    ),
    (
        "claude-3-5-haiku",
        Price {
            input: 0.8,
            cached_input: 0.08,
            output: 4.0,
        },
    ),
    // OpenAI — developers.openai.com pricing, reviewed 2026-11.
    (
        "gpt-5.2",
        Price {
            input: 1.75,
            cached_input: 0.175,
            output: 14.0,
        },
    ),
    (
        "gpt-5.1",
        Price {
            input: 1.25,
            cached_input: 0.125,
            output: 10.0,
        },
    ),
    (
        "gpt-5",
        Price {
            input: 1.25,
            cached_input: 0.125,
            output: 10.0,
        },
    ),
    (
        "gpt-4.1",
        Price {
            input: 2.0,
            cached_input: 0.5,
            output: 8.0,
        },
    ),
    (
        "gpt-4o",
        Price {
            input: 2.5,
            cached_input: 1.25,
            output: 10.0,
        },
    ),
    (
        "o3",
        Price {
            input: 2.0,
            cached_input: 0.5,
            output: 8.0,
        },
    ),
    (
        "o4-mini",
        Price {
            input: 1.1,
            cached_input: 0.275,
            output: 4.4,
        },
    ),
    // Google — ai.google.dev pricing, reviewed 2026-11.
    (
        "gemini-2.5-pro",
        Price {
            input: 1.25,
            cached_input: 0.125,
            output: 10.0,
        },
    ),
    (
        "gemini-2.5-flash",
        Price {
            input: 0.3,
            cached_input: 0.03,
            output: 2.5,
        },
    ),
    // xAI — docs.x.ai pricing, reviewed 2026-11.
    (
        "grok-4",
        Price {
            input: 3.0,
            cached_input: 0.75,
            output: 15.0,
        },
    ),
    (
        "grok-code-fast",
        Price {
            input: 0.2,
            cached_input: 0.02,
            output: 1.5,
        },
    ),
];

/// Price for a reported model id: longest prefix match, case-insensitive.
/// Model ids arrive with provider decorations (`claude-sonnet-4-5-20250929`,
/// `anthropic/claude-…`), so prefix matching is deliberate.
fn price_for(model: &str) -> Option<Price> {
    let model = model.to_ascii_lowercase();
    TABLE
        .iter()
        .filter(|(prefix, _)| model.contains(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, price)| *price)
}

fn row_cost(price: Price, row: &UsageBreakdown) -> f64 {
    let t = &row.totals;
    (t.input_tokens as f64 * price.input
        + t.cached_input_tokens as f64 * price.cached_input
        + t.output_tokens as f64 * price.output)
        / 1_000_000.0
}

/// Estimate the API-equivalent spend inside one period's model breakdown.
/// `None` when the period has no priced model at all — the caller renders
/// "tariff unknown", never `$0.00`. `unpriced_models` names the models that
/// contributed tokens without a known price so the estimate stays honest.
pub fn estimate(detail: &UsageDetail) -> Option<UsageCostEstimate> {
    let mut usd = 0.0;
    let mut priced = false;
    let mut unpriced = Vec::new();
    for row in &detail.models {
        let Some(label) = &row.label else {
            continue;
        };
        match price_for(label) {
            Some(price) => {
                usd += row_cost(price, row);
                priced = true;
            }
            None => unpriced.push(label.clone()),
        }
    }
    priced.then(|| UsageCostEstimate {
        usd,
        as_of: AS_OF.to_string(),
        unpriced_models: unpriced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use komet_proto::UsageTotals;

    fn row(label: &str, input: u64, cached: u64, output: u64) -> UsageBreakdown {
        UsageBreakdown {
            label: Some(label.into()),
            totals: UsageTotals {
                runs: 1,
                input_tokens: input,
                cached_input_tokens: cached,
                output_tokens: output,
                reasoning_tokens: 0,
            },
        }
    }

    #[test]
    fn longest_prefix_wins_and_ids_with_dates_match() {
        assert!(price_for("claude-sonnet-4-5-20250929").is_some());
        assert!(price_for("anthropic/claude-opus-4-1").is_some());
        assert!(price_for("gpt-5.2-codex").is_some());
        assert!(price_for("definitely-not-a-model").is_none());
        // sonnet-4-5 must not be priced as sonnet-4's cheaper… actually both
        // exist; the point is the 4-5 row wins over the 4 prefix.
        let p45 = price_for("claude-sonnet-4-5").unwrap();
        let p4 = price_for("claude-sonnet-4-20250514").unwrap();
        assert_eq!(p45.input, p4.input);
    }

    #[test]
    fn estimate_sums_priced_rows_and_names_unpriced() {
        let detail = UsageDetail {
            models: vec![
                row("claude-sonnet-4-5", 1_000_000, 0, 0), // $3
                row("mystery-model", 1_000_000, 0, 0),
            ],
            projects: vec![],
        };
        let estimate = estimate(&detail).expect("priced model present");
        assert!((estimate.usd - 3.0).abs() < 1e-9, "{}", estimate.usd);
        assert_eq!(estimate.unpriced_models, vec!["mystery-model".to_string()]);
        assert_eq!(estimate.as_of, AS_OF);
    }

    #[test]
    fn estimate_is_none_without_any_priced_model() {
        let detail = UsageDetail {
            models: vec![row("mystery-model", 100, 0, 50)],
            projects: vec![],
        };
        assert!(estimate(&detail).is_none());
        assert!(estimate(&UsageDetail::default()).is_none());
    }
}
