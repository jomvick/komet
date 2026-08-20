//! Claude Code catalog: models, effort ladders, and the Ultrathink prompt
//! convention. The protocol adapter itself is the shared ACP harness
//! ([`crate::AcpHarness::claude`], via the org-maintained `claude-agent-acp`
//! adapter on the Claude Agent SDK) — the bespoke stream-json harness this
//! module used to hold was retired with the ACP conversion
//! (docs/research/acp.md).

pub mod catalog;
