//! Pi's provider configuration, `models.json`. The user's own file
//! (`~/.pi/agent/models.json`) is an optional source: init reads the base
//! URL, context window and inputs of a model it lists, and a run copies that
//! model's provider settings. What Pi reads during a run is the file Tenaz
//! writes into the run's own agent directory ([`agent_models_json`]).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelsError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// One model as `models.json` lists it, with its provider's base URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub base_url: Option<String>,
    pub context_window: Option<u64>,
    /// `text`, `image`.
    pub input: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct File {
    providers: std::collections::BTreeMap<String, Provider>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Provider {
    base_url: Option<String>,
    models: Vec<Model>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Model {
    id: String,
    context_window: Option<u64>,
    input: Vec<String>,
}

/// One model of `models.json` with its provider, for choosing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedModel {
    pub provider: String,
    pub id: String,
    pub entry: ModelEntry,
}

/// Every model the file lists, by provider then in file order. An absent
/// file lists nothing.
pub fn list_models(path: &Path) -> Result<Vec<ListedModel>, ModelsError> {
    let Some(file) = read_file(path)? else {
        return Ok(Vec::new());
    };
    Ok(file
        .providers
        .into_iter()
        .flat_map(|(provider, config)| {
            let base_url = config.base_url;
            config.models.into_iter().map(move |model| ListedModel {
                provider: provider.clone(),
                entry: entry_of(base_url.clone(), &model),
                id: model.id,
            })
        })
        .collect())
}

fn read_file(path: &Path) -> Result<Option<File>, ModelsError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ModelsError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| ModelsError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

fn entry_of(base_url: Option<String>, model: &Model) -> ModelEntry {
    ModelEntry {
        base_url,
        context_window: model.context_window,
        input: if model.input.is_empty() {
            vec!["text".to_owned()]
        } else {
            model.input.clone()
        },
    }
}

/// `$HOME/.pi/agent/models.json`.
pub fn default_models_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi/agent/models.json"))
}

/// The entry for `provider/id`; `Ok(None)` when the file, the provider or
/// the model is absent: the run's own `models.json` describes the model
/// from what init knows instead.
pub fn model_entry(
    path: &Path,
    provider: &str,
    id: &str,
) -> Result<Option<ModelEntry>, ModelsError> {
    let Some(file) = read_file(path)? else {
        return Ok(None);
    };
    Ok(file.providers.get(provider).and_then(|config| {
        config
            .models
            .iter()
            .find(|m| m.id == id)
            .map(|model| entry_of(config.base_url.clone(), model))
    }))
}

/// The model a run pins, as Tenaz describes it to Pi.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentModel {
    pub provider: String,
    pub id: String,
    /// Where Pi sends its requests: the engine proxy, or the engine.
    pub base_url: String,
    pub context_window: u64,
    /// `text`, `image`.
    pub input: Vec<String>,
    /// Whether Pi may ask for reasoning (`reasoning_effort`).
    pub reasoning: bool,
}

/// The provider of `provider/id` in the user's file, as written there (API
/// key, headers, compat settings, the model's thinking map), with only that
/// model. `Ok(None)` when the file does not list the model.
pub fn provider_template(
    path: &Path,
    provider: &str,
    id: &str,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, ModelsError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ModelsError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| ModelsError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    let Some(mut config) = json
        .get("providers")
        .and_then(|p| p.get(provider))
        .and_then(serde_json::Value::as_object)
        .cloned()
    else {
        return Ok(None);
    };
    let Some(model) = config
        .get("models")
        .and_then(serde_json::Value::as_array)
        .and_then(|models| {
            models
                .iter()
                .find(|m| m.get("id").and_then(serde_json::Value::as_str) == Some(id))
        })
        .cloned()
    else {
        return Ok(None);
    };
    config.insert("models".to_owned(), serde_json::Value::Array(vec![model]));
    Ok(Some(config))
}

/// The `models.json` of a run: one provider with the one model. Built from
/// `template` (see [`provider_template`]) when the user's file lists the
/// model, otherwise from scratch for an OpenAI-compatible local engine. The
/// base URL, context window and inputs are always the run's.
pub fn agent_models_json(
    model: &AgentModel,
    template: Option<serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    use serde_json::{Value, json};
    let mut provider = template.unwrap_or_else(|| {
        // Local servers do not all know OpenAI's `developer` role, `store`
        // or `max_completion_tokens`; the plain forms work everywhere.
        let mut fresh = serde_json::Map::new();
        fresh.insert(
            "compat".to_owned(),
            json!({
                "supportsDeveloperRole": false,
                "supportsStore": false,
                "maxTokensField": "max_tokens"
            }),
        );
        fresh
    });
    let mut entry = provider
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| models.first())
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    entry.insert("id".to_owned(), Value::String(model.id.clone()));
    entry
        .entry("name")
        .or_insert_with(|| Value::String(model.id.clone()));
    entry
        .entry("reasoning")
        .or_insert(Value::Bool(model.reasoning));
    entry.insert("input".to_owned(), json!(model.input));
    entry.insert("contextWindow".to_owned(), json!(model.context_window));
    entry.entry("maxTokens").or_insert(json!(16_384));
    provider.insert("baseUrl".to_owned(), Value::String(model.base_url.clone()));
    provider
        .entry("api")
        .or_insert_with(|| Value::String("openai-completions".to_owned()));
    // Pi lists a model only when its provider has a key; local engines
    // ignore it. A key the user saved with `/login` lives in their
    // `auth.json`, which the run's agent directory does not have.
    provider
        .entry("apiKey")
        .or_insert_with(|| Value::String("local".to_owned()));
    provider.insert(
        "models".to_owned(),
        Value::Array(vec![Value::Object(entry)]),
    );
    json!({ "providers": { &model.provider: provider } })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "providers": {
        "sushi": {
          "baseUrl": "http://127.0.0.1:12345/v1",
          "api": "openai-completions",
          "apiKey": "local",
          "models": [
            {"id": "Qwen3.8", "name": "Qwen", "reasoning": true, "input": ["text", "image"],
             "contextWindow": 131072, "maxTokens": 16384, "thinkingLevelMap": {"off": "off"}},
            {"id": "Small", "name": "Small"}
          ]
        }
      }
    }"#;

    #[test]
    fn the_entry_carries_base_url_context_window_and_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let entry = model_entry(&path, "sushi", "Qwen3.8").unwrap().unwrap();
        assert_eq!(
            entry,
            ModelEntry {
                base_url: Some("http://127.0.0.1:12345/v1".into()),
                context_window: Some(131072),
                input: vec!["text".into(), "image".into()],
            }
        );
        let small = model_entry(&path, "sushi", "Small").unwrap().unwrap();
        assert_eq!(small.context_window, None);
        assert_eq!(small.input, vec!["text".to_owned()]);
    }

    #[test]
    fn list_models_names_every_model_with_its_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let listed = list_models(&path).unwrap();
        let names: Vec<_> = listed
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert_eq!(names, ["sushi/Qwen3.8", "sushi/Small"]);
        assert_eq!(listed[0].entry.context_window, Some(131072));
        assert!(
            list_models(&dir.path().join("absent.json"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_unlisted_model_has_no_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, SAMPLE).unwrap();
        assert_eq!(model_entry(&path, "sushi", "Missing").unwrap(), None);
        assert_eq!(model_entry(&path, "other", "Missing").unwrap(), None);
        assert_eq!(
            model_entry(&dir.path().join("absent.json"), "sushi", "Qwen3.8").unwrap(),
            None
        );
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(
            model_entry(&path, "sushi", "Qwen3.8"),
            Err(ModelsError::Parse { .. })
        ));
    }

    fn agent_model() -> AgentModel {
        AgentModel {
            provider: "lmstudio".into(),
            id: "qwen3.8-27b".into(),
            base_url: "http://127.0.0.1:4001/v1".into(),
            context_window: 65536,
            input: vec!["text".into()],
            reasoning: true,
        }
    }

    /// With nothing in the user's file, the entry comes from what init
    /// knows, in the plain `OpenAI` forms every local server accepts.
    #[test]
    fn a_model_is_described_from_scratch() {
        let json = agent_models_json(&agent_model(), None);
        assert_eq!(
            json,
            serde_json::json!({"providers": {"lmstudio": {
                "baseUrl": "http://127.0.0.1:4001/v1",
                "api": "openai-completions",
                "apiKey": "local",
                "compat": {
                    "supportsDeveloperRole": false,
                    "supportsStore": false,
                    "maxTokensField": "max_tokens"
                },
                "models": [{
                    "id": "qwen3.8-27b",
                    "name": "qwen3.8-27b",
                    "reasoning": true,
                    "input": ["text"],
                    "contextWindow": 65536,
                    "maxTokens": 16384
                }]
            }}})
        );
    }

    /// A model the user's file lists keeps its provider's settings and its
    /// own (thinking map, output limit), with the run's URL, context and
    /// inputs, and without the provider's other models.
    #[test]
    fn a_listed_model_keeps_the_users_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, SAMPLE).unwrap();
        assert_eq!(provider_template(&path, "sushi", "Missing").unwrap(), None);
        assert_eq!(provider_template(&path, "other", "Qwen3.8").unwrap(), None);
        assert_eq!(
            provider_template(&dir.path().join("absent.json"), "sushi", "Qwen3.8").unwrap(),
            None
        );
        let template = provider_template(&path, "sushi", "Qwen3.8").unwrap();
        let model = AgentModel {
            provider: "sushi".into(),
            id: "Qwen3.8".into(),
            reasoning: false,
            ..agent_model()
        };
        let json = agent_models_json(&model, template);
        let provider = &json["providers"]["sushi"];
        assert_eq!(provider["baseUrl"], "http://127.0.0.1:4001/v1");
        assert_eq!(provider["apiKey"], "local");
        assert!(provider.get("compat").is_none(), "{provider}");
        let models = provider["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["name"], "Qwen");
        assert_eq!(models[0]["reasoning"], true, "the user's own setting wins");
        assert_eq!(models[0]["thinkingLevelMap"]["off"], "off");
        assert_eq!(models[0]["contextWindow"], 65536);
        assert_eq!(models[0]["input"], serde_json::json!(["text"]));
        assert_eq!(models[0]["maxTokens"], 16384);
    }
}
