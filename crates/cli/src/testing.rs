//! Fixtures the cli crate's tests share.

use std::collections::BTreeMap;

/// A run's ledger for an agent of kind `harness`, with defaults.
pub(crate) fn ledger_for(harness: tenaz_core::ledger::HarnessKind) -> tenaz_core::ledger::Ledger {
    let config = tenaz_core::ledger::ModelConfig {
        harness,
        harness_version: "1".into(),
        model: "engine/m".into(),
        provider: Some("engine".into()),
        id: "m".into(),
        base_url: None,
        context_window: 65_536,
        input: vec!["text".into()],
        thinking: None,
        role_models: BTreeMap::new(),
    };
    tenaz_core::ledger::Ledger::new(
        "t".into(),
        "b".into(),
        "s".into(),
        "0".into(),
        config,
        tenaz_core::params::RunParams::default(),
        vec![],
    )
}
