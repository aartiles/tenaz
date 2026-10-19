//! Pi's provider configuration, `~/.pi/agent/models.json`: the base URL,
//! context window and inputs of the model a run pins, read at init.

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
    /// Without the entry the context window, and so every context budget,
    /// would be a guess.
    #[error("Model {id} is not listed under provider {provider} in {path}")]
    UnknownModel {
        provider: String,
        id: String,
        path: PathBuf,
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

/// `$HOME/.pi/agent/models.json`.
pub fn default_models_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi/agent/models.json"))
}

/// The entry for `provider/id`. `Ok(None)` when the file or the provider
/// is absent (Pi may know the model from elsewhere); an error when the
/// provider is configured but does not list the model.
pub fn model_entry(
    path: &Path,
    provider: &str,
    id: &str,
) -> Result<Option<ModelEntry>, ModelsError> {
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
    let file: File = serde_json::from_str(&text).map_err(|source| ModelsError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let Some(config) = file.providers.get(provider) else {
        return Ok(None);
    };
    let model =
        config
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| ModelsError::UnknownModel {
                provider: provider.to_owned(),
                id: id.to_owned(),
                path: path.to_path_buf(),
            })?;
    Ok(Some(ModelEntry {
        base_url: config.base_url.clone(),
        context_window: model.context_window,
        input: if model.input.is_empty() {
            vec!["text".to_owned()]
        } else {
            model.input.clone()
        },
    }))
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
    fn a_configured_provider_must_list_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let error = model_entry(&path, "sushi", "Missing").unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("Model Missing is not listed under provider sushi in "),
            "{error}"
        );
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
}
