//! Pi's provider configuration, `models.json`. The user's own file
//! (`~/.pi/agent/models.json`) is an optional source: init reads the base
//! URL, context window and inputs of the run's models it lists, and a run
//! copies their provider settings. What Pi reads during a run is the file
//! Tenaz writes into the run's own agent directory ([`agent_models_json`]).

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

/// The run's models as Tenaz describes them to Pi: one provider at one
/// base URL, the run's own model, and the other models its roles play
/// (ADR 0011).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentModels {
    pub provider: String,
    /// Where Pi sends its requests: the engine proxy, or the engine.
    pub base_url: String,
    pub run: AgentModel,
    pub others: Vec<AgentModel>,
}

impl AgentModels {
    /// The run's model first, then the others.
    pub fn all(&self) -> impl Iterator<Item = &AgentModel> {
        std::iter::once(&self.run).chain(&self.others)
    }

    pub fn ids(&self) -> Vec<String> {
        self.all().map(|m| m.id.clone()).collect()
    }

    /// The run's model as Pi names it, `provider/id`.
    pub fn run_name(&self) -> String {
        self.name(&self.run)
    }

    /// The other models as Pi names them.
    pub fn other_names(&self) -> Vec<String> {
        self.others.iter().map(|m| self.name(m)).collect()
    }

    fn name(&self, model: &AgentModel) -> String {
        format!("{}/{}", self.provider, model.id)
    }
}

/// One of the run's models, as Tenaz describes it to Pi.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentModel {
    pub id: String,
    pub context_window: u64,
    /// `text`, `image`.
    pub input: Vec<String>,
    /// Whether Pi may ask for reasoning (`reasoning_effort`).
    pub reasoning: bool,
}

/// The provider of the run's models in the user's file, as written there
/// (API key, headers, compat settings, each model's thinking map), with
/// only those of `ids` it lists. `Ok(None)` when the file lists none of
/// them.
pub fn provider_template(
    path: &Path,
    provider: &str,
    ids: &[String],
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
    let listed: Vec<serde_json::Value> = config
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| {
            m.get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| ids.iter().any(|wanted| wanted == id))
        })
        .cloned()
        .collect();
    if listed.is_empty() {
        return Ok(None);
    }
    config.insert("models".to_owned(), serde_json::Value::Array(listed));
    Ok(Some(config))
}

/// The `models.json` of a run: one provider with the run's models, its
/// own first. Built from `template` (see [`provider_template`]) for the
/// models the user's file lists, otherwise from scratch for an
/// OpenAI-compatible local engine. The base URL, context windows and inputs
/// are always the run's.
pub fn agent_models_json(
    models: &AgentModels,
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
    let listed = provider
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entries = models
        .all()
        .map(|model| model_json(model, &listed))
        .collect();
    provider.insert("baseUrl".to_owned(), Value::String(models.base_url.clone()));
    provider
        .entry("api")
        .or_insert_with(|| Value::String("openai-completions".to_owned()));
    // Pi lists a model only when its provider has a key; local engines
    // ignore it. A key the user saved with `/login` lives in their
    // `auth.json`, which the run's agent directory does not have.
    provider
        .entry("apiKey")
        .or_insert_with(|| Value::String("local".to_owned()));
    provider.insert("models".to_owned(), Value::Array(entries));
    json!({ "providers": { &models.provider: provider } })
}

/// `model`'s entry: the user's own from `listed`, if any, with the run's
/// context window and inputs.
fn model_json(model: &AgentModel, listed: &[serde_json::Value]) -> serde_json::Value {
    use serde_json::{Value, json};
    let mut entry = listed
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(model.id.as_str()))
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
    Value::Object(entry)
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

    /// The entry of the model `id` under `sushi` in `path`.
    fn entry(path: &Path, id: &str) -> Option<ModelEntry> {
        let listed = list_models(path).unwrap();
        listed
            .into_iter()
            .find(|m| m.provider == "sushi" && m.id == id)
            .map(|m| m.entry)
    }

    #[test]
    fn the_entry_carries_base_url_context_window_and_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, SAMPLE).unwrap();
        assert_eq!(
            entry(&path, "Qwen3.8"),
            Some(ModelEntry {
                base_url: Some("http://127.0.0.1:12345/v1".into()),
                context_window: Some(131072),
                input: vec!["text".into(), "image".into()],
            })
        );
        let small = entry(&path, "Small").unwrap();
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
    fn a_file_that_is_not_json_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(list_models(&path), Err(ModelsError::Parse { .. })));
    }

    fn agent_model() -> AgentModel {
        AgentModel {
            id: "qwen3.8-27b".into(),
            context_window: 65536,
            input: vec!["text".into()],
            reasoning: true,
        }
    }

    /// The run's models on `provider`, at the proxy's address.
    fn on(provider: &str, run: AgentModel, others: Vec<AgentModel>) -> AgentModels {
        AgentModels {
            provider: provider.into(),
            base_url: "http://127.0.0.1:4001/v1".into(),
            run,
            others,
        }
    }

    /// With nothing in the user's file, the entry comes from what init
    /// knows, in the plain `OpenAI` forms every local server accepts.
    #[test]
    fn a_model_is_described_from_scratch() {
        let json = agent_models_json(&on("lmstudio", agent_model(), vec![]), None);
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
        let ids = |id: &str| vec![id.to_owned()];
        assert_eq!(
            provider_template(&path, "sushi", &ids("Missing")).unwrap(),
            None
        );
        assert_eq!(
            provider_template(&path, "other", &ids("Qwen3.8")).unwrap(),
            None
        );
        let absent = dir.path().join("absent.json");
        assert_eq!(
            provider_template(&absent, "sushi", &ids("Qwen3.8")).unwrap(),
            None
        );
        let template = provider_template(&path, "sushi", &ids("Qwen3.8")).unwrap();
        let model = AgentModel {
            id: "Qwen3.8".into(),
            reasoning: false,
            ..agent_model()
        };
        let json = agent_models_json(&on("sushi", model, vec![]), template);
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

    /// A model per role: every model of the run is listed, the run's first,
    /// each with its own window and inputs; one the user's file lists keeps
    /// its settings, one it does not is described from scratch.
    #[test]
    fn every_model_of_the_run_is_listed_with_its_own_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let ids = ["Qwen3.8", "Small", "Unlisted"].map(str::to_owned);
        let template = provider_template(&path, "sushi", &ids).unwrap();
        let model = |id: &str, context_window, input: &[&str]| AgentModel {
            id: id.into(),
            context_window,
            input: input.iter().map(|i| (*i).to_owned()).collect(),
            ..agent_model()
        };
        let models = on(
            "sushi",
            model("Qwen3.8", 131_072, &["text", "image"]),
            vec![
                model("Small", 32768, &["text"]),
                model("Unlisted", 65536, &["text"]),
            ],
        );
        assert_eq!(models.run_name(), "sushi/Qwen3.8");
        assert_eq!(models.other_names(), ["sushi/Small", "sushi/Unlisted"]);
        let json = agent_models_json(&models, template);
        let listed = json["providers"]["sushi"]["models"].as_array().unwrap();
        let summary: Vec<String> = listed
            .iter()
            .map(|m| {
                format!(
                    "{} {} {} {}",
                    m["id"], m["name"], m["contextWindow"], m["input"]
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                r#""Qwen3.8" "Qwen" 131072 ["text","image"]"#,
                r#""Small" "Small" 32768 ["text"]"#,
                r#""Unlisted" "Unlisted" 65536 ["text"]"#,
            ]
        );
        assert_eq!(listed[0]["thinkingLevelMap"]["off"], "off");
    }
}
