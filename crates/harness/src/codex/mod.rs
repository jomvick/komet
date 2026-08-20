//! Codex catalog: models, effort ladders, and service tiers. The protocol
//! adapter itself is the shared ACP harness ([`crate::AcpHarness::codex`],
//! via the org-maintained `codex-acp` adapter wrapping the codex app-server)
//! — the bespoke app-server harness this module used to hold was retired
//! with the ACP conversion (docs/research/acp.md).

pub(crate) mod catalog;
