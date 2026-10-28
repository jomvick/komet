//! Environments: a named, isolated workspace backed by its own sync server.
//!
//! The pairing link carries everything a client needs to adopt one:
//! `komet://pair/<base64url-nopad>{"v":1,"label","url","token"}`. It is a secret —
//! the token grants shell on that host — so it is never logged.

use serde::{Deserialize, Serialize};

/// Prefix of a pairing link. Case-sensitive: a link pasted with different casing is
/// rejected rather than silently repaired.
pub const PAIR_SCHEME: &str = "komet://pair/";

/// The only link version this build understands.
pub const PAIR_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairPayload {
    pub v: u8,
    pub label: String,
    pub url: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Namespace {
    pub org_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub id: String,
    pub label: String,
    pub url: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<Namespace>,
    pub added_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvError {
    /// Not a `komet://pair/` link, bad base64, bad JSON, or a url that is not http(s).
    InvalidLink,
    UnsupportedVersion(u8),
    EmptyField(&'static str),
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLink => write!(f, "not a valid komet pairing link"),
            Self::UnsupportedVersion(v) => {
                write!(f, "pairing link version {v} is newer than this build")
            }
            Self::EmptyField(name) => write!(f, "pairing link has an empty {name}"),
        }
    }
}

impl std::error::Error for EnvError {}

/// Encode a pairing link. Total: the caller validates the payload first.
pub fn encode_pair_link(payload: &PairPayload) -> String {
    encode_payload(payload)
}

/// Decode and validate a pairing link. Surrounding whitespace is tolerated: links
/// travel through chat messages.
pub fn decode_pair_link(raw: &str) -> Result<PairPayload, EnvError> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix(PAIR_SCHEME)
        .ok_or(EnvError::InvalidLink)?;
    let payload = decode_payload(body)?;
    if payload.v != PAIR_VERSION {
        return Err(EnvError::UnsupportedVersion(payload.v));
    }
    let label = payload.label.trim();
    if label.is_empty() {
        return Err(EnvError::EmptyField("label"));
    }
    let url = payload.url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err(EnvError::EmptyField("url"));
    }
    if !is_http_url(url) {
        return Err(EnvError::InvalidLink);
    }
    let token = payload.token.trim();
    if token.is_empty() {
        return Err(EnvError::EmptyField("token"));
    }
    Ok(PairPayload {
        v: payload.v,
        label: label.to_string(),
        url: url.to_string(),
        token: token.to_string(),
    })
}

fn is_http_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|rest| !rest.is_empty() && !rest.contains(char::is_whitespace))
}

fn encode_payload(payload: &PairPayload) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let json = serde_json::to_vec(payload).expect("pair payload is serializable");
    format!("{PAIR_SCHEME}{}", URL_SAFE_NO_PAD.encode(json))
}

fn decode_payload(body: &str) -> Result<PairPayload, EnvError> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let bytes = URL_SAFE_NO_PAD
        .decode(body.as_bytes())
        .map_err(|_| EnvError::InvalidLink)?;
    serde_json::from_slice(&bytes).map_err(|_| EnvError::InvalidLink)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> PairPayload {
        PairPayload {
            v: 1,
            label: "VPS ovh".into(),
            url: "https://sync.example.com".into(),
            token: "abc123".into(),
        }
    }

    #[test]
    fn pair_link_round_trips() {
        let payload = valid();
        assert_eq!(
            decode_pair_link(&encode_pair_link(&payload)).unwrap(),
            payload
        );
    }

    #[test]
    fn decode_trims_surrounding_whitespace() {
        let payload = valid();
        let raw = format!("  {}\n", encode_pair_link(&payload));
        assert_eq!(decode_pair_link(&raw).unwrap(), payload);
    }

    #[test]
    fn decode_rejects_wrong_scheme_and_versions() {
        assert_eq!(
            decode_pair_link("http://example.com"),
            Err(EnvError::InvalidLink)
        );
        assert_eq!(
            decode_pair_link(&encode_payload(&PairPayload { v: 2, ..valid() })),
            Err(EnvError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn decode_rejects_empty_fields() {
        let empty_token = encode_payload(&PairPayload {
            token: "  ".into(),
            ..valid()
        });
        assert_eq!(
            decode_pair_link(&empty_token),
            Err(EnvError::EmptyField("token"))
        );
        let empty_label = encode_payload(&PairPayload {
            label: "".into(),
            ..valid()
        });
        assert_eq!(
            decode_pair_link(&empty_label),
            Err(EnvError::EmptyField("label"))
        );
    }

    #[test]
    fn decode_rejects_non_http_urls() {
        for url in ["nope", "ftp://x", "https://", "http://has space"] {
            let raw = encode_payload(&PairPayload {
                url: url.into(),
                ..valid()
            });
            assert_eq!(decode_pair_link(&raw), Err(EnvError::InvalidLink), "{url}");
        }
    }

    #[test]
    fn decode_rejects_garbage_and_trailing_slash_normalizes_url() {
        assert_eq!(
            decode_pair_link("komet://pair/!!!!"),
            Err(EnvError::InvalidLink)
        );
        let raw = encode_payload(&PairPayload {
            url: "https://sync.example.com/".into(),
            ..valid()
        });
        assert_eq!(
            decode_pair_link(&raw).unwrap().url,
            "https://sync.example.com".to_string()
        );
    }
}
