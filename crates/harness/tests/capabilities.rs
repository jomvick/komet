//! Phase-0 account/identity/usage matrix ("fiabiliser les agents Komet").
//!
//! These assertions are deliberately exhaustive: a new `HarnessId` without an
//! explicit decision fails here, and a row that silently drifts from the
//! reviewed sources fails too. Source strings are asserted non-empty rather
//! than pinned word-for-word so wording can improve without weakening the
//! contract.

use komet_harness::capabilities::{
    capabilities, has_local_usage_log, reports_provider_quota, supports_account_switching,
};
use komet_proto::{AccountMode, HarnessId, IdentityDetection, UsageSurface};

const GOLDEN: &[(HarnessId, AccountMode, IdentityDetection, UsageSurface)] = &[
    (
        HarnessId::ClaudeCode,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::ProviderQuota,
    ),
    (
        HarnessId::Codex,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::ProviderQuota,
    ),
    (
        HarnessId::Cursor,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::ProviderQuota,
    ),
    (
        HarnessId::Devin,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::MeasuredOnly,
    ),
    (
        HarnessId::Grok,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::LocalLogs,
    ),
    (
        HarnessId::Hermes,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::MeasuredOnly,
    ),
    (
        HarnessId::Pi,
        AccountMode::MonoAccount,
        IdentityDetection::NotExposed,
        UsageSurface::LocalLogs,
    ),
    (
        HarnessId::Opencode,
        AccountMode::MonoAccount,
        IdentityDetection::Detectable,
        UsageSurface::LocalLogs,
    ),
    (
        HarnessId::Cline,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::MeasuredOnly,
    ),
    (
        HarnessId::Qoder,
        AccountMode::MultiAccount,
        IdentityDetection::NotExposed,
        UsageSurface::MeasuredOnly,
    ),
    (
        HarnessId::Copilot,
        AccountMode::MonoAccount,
        IdentityDetection::Detectable,
        UsageSurface::ProviderQuota,
    ),
    (
        HarnessId::Antigravity,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::ProviderQuota,
    ),
    (
        HarnessId::Muse,
        AccountMode::MultiAccount,
        IdentityDetection::Detectable,
        UsageSurface::LocalLogs,
    ),
    (
        HarnessId::Mock,
        AccountMode::MonoAccount,
        IdentityDetection::NotExposed,
        UsageSurface::MeasuredOnly,
    ),
];

#[test]
fn capability_matrix_is_explicit_for_every_agent() {
    for (id, accounts, detection, usage) in GOLDEN {
        let declared = capabilities(*id);
        assert_eq!(declared.accounts, *accounts, "{} accounts", id.as_str());
        assert_eq!(
            declared.identity.detection,
            *detection,
            "{} identity detection",
            id.as_str()
        );
        assert_eq!(declared.usage.surface, *usage, "{} usage", id.as_str());
        assert!(
            !declared.identity.source.trim().is_empty(),
            "{} must name the identity source it actually reads",
            id.as_str()
        );
        assert!(
            !declared.usage.source.trim().is_empty(),
            "{} must name the usage source it actually reads",
            id.as_str()
        );
        assert_eq!(
            supports_account_switching(*id),
            *accounts == AccountMode::MultiAccount,
            "{} switching follows its declared account mode",
            id.as_str()
        );
        assert_eq!(
            reports_provider_quota(*id),
            *usage == UsageSurface::ProviderQuota
        );
        assert_eq!(has_local_usage_log(*id), *usage == UsageSurface::LocalLogs);
    }
}

#[test]
fn every_harness_id_has_a_golden_row() {
    let mut ids: Vec<&str> = GOLDEN.iter().map(|(id, ..)| id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        GOLDEN.len(),
        "duplicate row in the golden matrix"
    );
    // `HarnessId::ALL` is the completeness trap: adding an id without a row
    // fails here, and the match in `capabilities` would not compile either.
    assert_eq!(ids.len(), HarnessId::ALL.len());
    for id in HarnessId::ALL {
        assert!(ids.contains(&id.as_str()), "missing golden row for {id:?}");
    }
}

#[test]
fn absent_decision_falls_back_to_unsupported() {
    let unknown = komet_proto::HarnessCapabilities::unknown();
    assert_eq!(unknown.accounts, AccountMode::MonoAccount);
    assert_eq!(unknown.identity.detection, IdentityDetection::NotExposed);
    assert_eq!(unknown.usage.surface, UsageSurface::MeasuredOnly);
    assert_eq!(unknown, komet_proto::HarnessCapabilities::default());
}
