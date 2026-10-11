//! Shared CLI-error classification: raw CLI text (+ optional numeric code)
//! → [`komet_proto::ErrorKind`] + retry flag. Every harness normalizer routes
//! its failures through [`classify_error`] so the transcript shows typed
//! errors with the raw text behind Details instead of opaque strings.

use komet_proto::{AgentEvent, ErrorKind};

/// Classify raw CLI failure text (+ optional numeric code) into a typed kind
/// and retry flag. Matching is case-insensitive, first hit wins in the order
/// below; numeric codes only refine an otherwise-`Unknown` verdict. Never
/// panics — unknown input yields `(Unknown, false)`.
pub fn classify_error(text: &str, code: Option<i64>) -> (ErrorKind, bool) {
    let lower = text.to_lowercase();
    let t = lower.as_str();
    let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    // Standalone-token match for bare status codes ("4000ms" must not read
    // as a 400).
    let has_token = |tok: &str| {
        t.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w == tok)
    };
    let kind = if has(&[
        "authentication_failed",
        "oauth_org_not_allowed",
        "sign in",
        "unauthorized",
        "invalid api key",
        "login required",
        "not authenticated",
        "auth error",
        "not logged in",
        "please log in",
    ]) || has_token("401")
    {
        ErrorKind::Auth
    } else if has(&[
        "billing_error",
        "billing",
        "insufficient balance",
        "credits exhausted",
        "out of credits",
        "payment",
        "top up",
        "topup",
        "quota exceeded",
    ]) || has_token("402")
    {
        ErrorKind::Billing
    } else if has(&[
        "rate_limit",
        "rate limit",
        "too many requests",
        "limit resets",
        "usage limit",
        "slow down",
    ]) || has_token("429")
    {
        ErrorKind::RateLimit
    } else if has(&[
        "overloaded",
        "over capacity",
        "server_error",
        "server error",
        "temporarily unavailable",
    ]) || has_token("503")
    {
        ErrorKind::Overloaded
    } else if has(&[
        "model_not_found",
        "model not found",
        "unknown model",
        "model unavailable",
        "no such model",
    ]) || has_token("404")
    {
        ErrorKind::ModelUnavailable
    } else if has(&[
        "network",
        "connection",
        "econnreset",
        "etimedout",
        "econnrefused",
        "socket hang up",
        "dns",
        "fetch failed",
        "getaddrinfo",
        "connection reset",
        "broken pipe",
    ]) {
        ErrorKind::Network
    } else if has(&[
        "invalid_request",
        "invalid argument",
        "bad request",
        "unknown method",
        "invalid params",
    ]) || has_token("400")
    {
        ErrorKind::Config
    } else if has(&[
        "crashed",
        "killed",
        "sigkill",
        "panicked",
        "exit code",
        "exited unexpectedly",
    ]) {
        ErrorKind::CliCrash
    } else if has(&["timed out", "timeout", "deadline exceeded"]) {
        ErrorKind::Timeout
    } else if has(&["refused", "refusal", "content policy"]) {
        ErrorKind::Refusal
    } else if has(&[
        "max_tokens",
        "max_turn_requests",
        "max output",
        "max_output",
        "truncated",
        "hit the maximum",
        "maximum output",
    ]) {
        ErrorKind::Truncated
    } else {
        ErrorKind::Unknown
    };
    // Numeric JSON-RPC-ish codes refine the verdict when the text is silent —
    // as a parameter, or embedded by the reader as `[rpc <code>]`.
    let kind = match code {
        Some(-32000) if kind == ErrorKind::Unknown => ErrorKind::Timeout,
        Some(-32601) | Some(-32602) if kind == ErrorKind::Unknown => ErrorKind::Config,
        _ if kind == ErrorKind::Unknown && has_token("32000") => ErrorKind::Timeout,
        _ if kind == ErrorKind::Unknown && (has_token("32601") || has_token("32602")) => {
            ErrorKind::Config
        }
        _ => kind,
    };
    (kind, kind.retryable_default())
}

/// The `detail` for a terminal failure: the last 4096 bytes of the CLI's
/// stderr on a char boundary (cut tails are `…`-prefixed), `None` when the
/// CLI said nothing on stderr — the visible `message` already carries the
/// raw text in that case, so no empty Details toggle appears.
pub fn tail_detail(stderr_tail: Option<String>) -> Option<String> {
    const MAX: usize = 4096;
    let trimmed = stderr_tail
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())?;
    if trimmed.len() <= MAX {
        return Some(trimmed);
    }
    let mut end = MAX;
    while end > 0 && !trimmed.is_char_boundary(trimmed.len() - end) {
        end -= 1;
    }
    Some(format!("…{}", trimmed[trimmed.len() - end..].trim_start()))
}

/// Build the typed [`AgentEvent::Error`] for a raw CLI failure: classified
/// kind + retry flag, `detail` carried verbatim (`tail_detail` output at
/// terminal failures, `None` when the message itself is the raw text).
pub fn typed_error(message: String, code: Option<i64>, detail: Option<String>) -> AgentEvent {
    let (kind, retryable) = classify_error(&message, code);
    AgentEvent::Error {
        message,
        kind,
        detail,
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komet_proto::ErrorKind;

    #[test]
    fn classify_error_table() {
        let cases: &[(&str, Option<i64>, ErrorKind, bool)] = &[
            (
                "Cline credits exhausted (Insufficient balance): pick a free model",
                None,
                ErrorKind::Billing,
                false,
            ),
            ("authentication_failed", None, ErrorKind::Auth, false),
            ("billing_error", None, ErrorKind::Billing, false),
            ("rate_limit", None, ErrorKind::RateLimit, true),
            ("Claude is overloaded", None, ErrorKind::Overloaded, true),
            ("model_not_found", None, ErrorKind::ModelUnavailable, false),
            (
                "request timed out after 10s",
                Some(-32000),
                ErrorKind::Timeout,
                true,
            ),
            (
                "The agent refused to continue.",
                None,
                ErrorKind::Refusal,
                false,
            ),
            (
                "hit the maximum output length",
                None,
                ErrorKind::Truncated,
                false,
            ),
            ("weird new failure", Some(-32603), ErrorKind::Unknown, false),
            ("", None, ErrorKind::Unknown, false),
            // Codes embedded by the JSON-RPC reader (`[rpc <code>]`) classify
            // even when the text itself is silent.
            (
                "session/prompt: [rpc -32000] request failed",
                None,
                ErrorKind::Timeout,
                true,
            ),
            (
                "session/prompt: [rpc -32601] method not found",
                None,
                ErrorKind::Config,
                false,
            ),
        ];
        for (text, code, kind, retryable) in cases {
            assert_eq!(
                classify_error(text, *code),
                (*kind, *retryable),
                "text={text:?}"
            );
        }
    }

    #[test]
    fn tail_detail_keeps_short_tails_drops_empty_caps_long() {
        assert_eq!(tail_detail(None), None);
        assert_eq!(tail_detail(Some("   ".to_owned())), None);
        assert_eq!(
            tail_detail(Some("boom".to_owned())),
            Some("boom".to_owned())
        );
        let long = "x".repeat(5000);
        let capped = tail_detail(Some(long)).expect("non-empty tail");
        assert!(capped.len() <= 4096 + 3, "len={}", capped.len());
        assert!(capped.starts_with("…"), "cut tail is marked");
    }

    #[test]
    fn typed_error_classifies_and_carries_detail() {
        let ev = typed_error("billing_error".to_owned(), None, Some("raw".to_owned()));
        assert!(matches!(
            ev,
            komet_proto::AgentEvent::Error {
                kind: ErrorKind::Billing,
                retryable: false,
                ..
            }
        ));
        let AgentEvent::Error { detail, .. } = ev else {
            panic!("must be an Error event");
        };
        assert_eq!(detail.as_deref(), Some("raw"));
    }
}
