//! The model and harness a run is pinned to, and the model each role plays.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::HarnessKind;
use crate::ports::SessionModel;
use crate::role::Role;
use crate::role_models::{IMAGE_INPUT, RoleModel};

/// The model and harness a run is pinned to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub harness: HarnessKind,
    pub harness_version: String,
    /// The model as the harness names it (`sushi/Qwen3.8-Flash-Next-Sushi-2.6bpw`).
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub context_window: u64,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Roles played by another model than this one (ADR 0011).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_models: BTreeMap<Role, RoleModel>,
}

impl ModelConfig {
    pub fn supports_images(&self) -> bool {
        self.input.iter().any(|i| i == IMAGE_INPUT)
    }

    /// The model `role` plays: its own in `role_models`, else the run's.
    pub fn model_for(&self, role: Role) -> SessionModel {
        self.role_models
            .get(&role)
            .map_or_else(|| self.run_model(), RoleModel::session_model)
    }

    /// The run's own model, the one `--model` named.
    pub fn run_model(&self) -> SessionModel {
        SessionModel {
            name: self.model.clone(),
            id: self.id.clone(),
            context_window: self.context_window,
            supports_images: self.supports_images(),
        }
    }

    /// The ids of the models the run's sessions play, the run's first,
    /// each once.
    pub fn model_ids(&self) -> Vec<String> {
        let mut ids = vec![self.id.clone()];
        for model in self.role_models.values() {
            if !ids.contains(&model.id) {
                ids.push(model.id.clone());
            }
        }
        ids
    }

    /// The largest context window of the run's models.
    pub fn largest_context_window(&self) -> u64 {
        self.role_models
            .values()
            .map(|m| m.context_window)
            .fold(self.context_window, u64::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::{config, role_model};

    #[test]
    fn a_role_plays_its_own_model_and_the_others_the_run_s() {
        let mut config = config();
        config
            .role_models
            .insert(Role::Reviewer, role_model("r", 65536));
        assert_eq!(
            config.model_for(Role::Reviewer),
            SessionModel {
                name: "sushi/r".into(),
                id: "r".into(),
                context_window: 65536,
                supports_images: false,
            }
        );
        assert_eq!(
            config.model_for(Role::Builder),
            SessionModel {
                name: "sushi/m".into(),
                id: "m".into(),
                context_window: 131_072,
                supports_images: true,
            }
        );
    }

    #[test]
    fn the_run_s_models_are_listed_once_each_the_run_s_first() {
        let mut config = config();
        config
            .role_models
            .insert(Role::Reviewer, role_model("r", 65536));
        config
            .role_models
            .insert(Role::FinalReviewer, role_model("r", 65536));
        config
            .role_models
            .insert(Role::Bootstrap, role_model("b", 65536));
        config
            .role_models
            .insert(Role::Split, role_model("m", 65536));
        // The roles in their order after the run's: bootstrap before reviewer.
        assert_eq!(config.model_ids(), ["m", "b", "r"]);
    }
}
