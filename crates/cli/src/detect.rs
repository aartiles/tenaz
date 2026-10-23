//! What the setup wizard finds on this machine: coding agents on `PATH`,
//! OpenAI-compatible engines on their usual ports with the models they
//! serve, `ComfyUI`, and git for `tenaz doctor`. Every probe is short and
//! failing to find something is not an error.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tenaz_core::doctor;
use tenaz_core::ledger::EngineApi;

const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// A coding agent Tenaz can drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    /// `fx` or `pi`.
    pub name: &'static str,
    pub version: String,
}

/// The agents on `PATH` that answer `--version`.
pub fn agents() -> Vec<Agent> {
    let mut found = Vec::new();
    if let Ok(version) = tenaz_acp::fx::fx_version(Path::new("fx")) {
        found.push(Agent {
            name: "fx",
            version,
        });
    }
    if let Ok(version) = tenaz_pi::pi_version(Path::new("pi")) {
        found.push(Agent {
            name: "pi",
            version,
        });
    }
    found
}

/// `git --version`, or `None` when git is not on `PATH`.
pub fn git() -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("--version")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// What `tenaz doctor` judges: git, the agents, and the engines on the
/// usual ports plus `base_url` when given.
pub async fn machine(base_url: Option<&str>) -> doctor::Machine {
    let client = client();
    let mut engines = local_engines(&client).await;
    if let Some(url) = base_url
        && !engines
            .iter()
            .any(|e| e.base_url == normalize_base_url(url))
        && let Some(engine) = engine_at(&client, url).await
    {
        engines.push(engine);
    }
    let mut looked_at = ENGINE_PORTS
        .iter()
        .map(|(port, label, _)| format!("{port} {label}"))
        .collect::<Vec<_>>();
    if let Some(url) = base_url {
        looked_at.push(normalize_base_url(url));
    }
    doctor::Machine {
        os: doctor::Os::from_name(std::env::consts::OS),
        git: git(),
        agents: agents()
            .into_iter()
            .map(|a| doctor::Tool {
                name: a.name.to_owned(),
                version: a.version,
            })
            .collect(),
        engines: engines.iter().map(Engine::seen).collect(),
        looked_at,
    }
}

impl Engine {
    /// What `tenaz doctor` needs to know about it.
    pub fn seen(&self) -> doctor::EngineSeen {
        doctor::EngineSeen {
            label: self.label.clone(),
            provider: self.provider.clone(),
            base_url: self.base_url.clone(),
            models: self
                .models
                .iter()
                .map(|m| doctor::ModelSeen {
                    id: m.id.clone(),
                    context_window: m.context_window,
                })
                .collect(),
        }
    }
}

/// Where engines usually listen, with the name to show and the provider
/// name to give the harness.
pub const ENGINE_PORTS: [(u16, &str, &str); 5] = [
    (12345, "Sushi", "sushi"),
    (8080, "llama.cpp", "llamacpp"),
    (1234, "LM Studio", "lmstudio"),
    (11434, "Ollama", "ollama"),
    (8000, "vLLM", "vllm"),
];

/// An engine that answered `GET /v1/models`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Engine {
    /// What to call it (`llama.cpp`, `LM Studio`, ...).
    pub label: String,
    /// The provider name for the harness configuration.
    pub provider: String,
    /// Ends in `/v1`.
    pub base_url: String,
    pub api: EngineApi,
    pub models: Vec<Model>,
}

/// One model an engine lists, with what it says about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub context_window: Option<u64>,
    pub vision: Option<bool>,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// The engines answering on [`ENGINE_PORTS`] on this machine, probed at once.
pub async fn local_engines(client: &reqwest::Client) -> Vec<Engine> {
    let probes = ENGINE_PORTS.iter().map(|(port, _, _)| {
        let url = format!("http://127.0.0.1:{port}/v1");
        async move { engine_at(client, &url).await }
    });
    futures::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// The engine at `base_url` (with or without the trailing `/v1`), or `None`
/// when nothing OpenAI-compatible answers there.
pub async fn engine_at(client: &reqwest::Client, base_url: &str) -> Option<Engine> {
    let base_url = normalize_base_url(base_url);
    let list = get_json(client, &format!("{base_url}/models")).await?;
    let origin = base_url.trim_end_matches("/v1").to_owned();
    let (label, provider) = identify(&list, port_of(&base_url));
    let mut models = parse_models(&list);
    // Engines that keep the details elsewhere.
    match provider.as_str() {
        "llamacpp" => {
            if let Some(props) = get_json(client, &format!("{origin}/props")).await {
                apply_llama_cpp_props(&mut models, &props);
            }
        }
        "lmstudio" => {
            if let Some(v0) = get_json(client, &format!("{origin}/api/v0/models")).await {
                apply_lm_studio_models(&mut models, &v0);
            }
        }
        _ => {}
    }
    Some(Engine {
        api: if provider == "sushi" {
            EngineApi::Sushi
        } else {
            EngineApi::Generic
        },
        label,
        provider,
        base_url,
        models,
    })
}

/// Whether `ComfyUI` answers at `url`.
pub async fn comfyui_at(client: &reqwest::Client, url: &str) -> bool {
    get_json(
        client,
        &format!("{}/system_stats", url.trim_end_matches('/')),
    )
    .await
    .is_some()
}

/// `http://host:port/v1`, whatever of it the user typed.
pub fn normalize_base_url(text: &str) -> String {
    let text = text.trim().trim_end_matches('/');
    let text = if text.contains("://") {
        text.to_owned()
    } else {
        format!("http://{text}")
    };
    if text.ends_with("/v1") {
        text
    } else {
        format!("{text}/v1")
    }
}

/// The provider name of the engine usually on `base_url`'s port.
pub fn provider_by_port(base_url: &str) -> Option<&'static str> {
    let port = port_of(&normalize_base_url(base_url))?;
    ENGINE_PORTS
        .iter()
        .find(|(p, _, _)| *p == port)
        .map(|(_, _, provider)| *provider)
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    let response = client.get(url).timeout(PROBE_TIMEOUT).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

fn port_of(base_url: &str) -> Option<u16> {
    let authority = base_url.split("://").nth(1)?.split('/').next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

/// The engine's name and provider: from the shape of its model list first
/// (Sushi reports a load state, the others an owner), then from the port.
fn identify(list: &Value, port: Option<u16>) -> (String, String) {
    let first = list["data"].get(0);
    let owner = first
        .and_then(|m| m["owned_by"].as_str())
        .unwrap_or_default();
    let by_shape = if first.is_some_and(|m| m.get("loaded").is_some() && m.get("state").is_some()) {
        Some("sushi")
    } else {
        match owner {
            "llamacpp" => Some("llamacpp"),
            "vllm" => Some("vllm"),
            "library" | "ollama" => Some("ollama"),
            "organization_owner" => Some("lmstudio"),
            _ => None,
        }
    };
    let known = by_shape.and_then(|p| ENGINE_PORTS.iter().find(|(_, _, provider)| *provider == p));
    let known =
        known.or_else(|| port.and_then(|port| ENGINE_PORTS.iter().find(|(p, _, _)| *p == port)));
    match known {
        Some((_, label, provider)) => ((*label).to_owned(), (*provider).to_owned()),
        None => ("OpenAI-compatible engine".to_owned(), "local".to_owned()),
    }
}

/// The models of a `/v1/models` answer, with the context window when the
/// engine puts it there (Sushi's `context_length`, vLLM's `max_model_len`,
/// llama.cpp's `meta.n_ctx_train`).
fn parse_models(list: &Value) -> Vec<Model> {
    let Some(items) = list["data"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let id = item["id"].as_str()?.to_owned();
            let context_window = ["context_length", "max_model_len", "max_context_length"]
                .iter()
                .find_map(|key| item[key].as_u64())
                .or_else(|| item["meta"]["n_ctx_train"].as_u64());
            Some(Model {
                id,
                context_window,
                vision: None,
            })
        })
        .collect()
}

/// llama.cpp's `/props`: the context the server actually runs with (which
/// is what fits, unlike the model's training context) and its modalities.
fn apply_llama_cpp_props(models: &mut [Model], props: &Value) {
    let n_ctx = props["default_generation_settings"]["n_ctx"].as_u64();
    let vision = props["modalities"]["vision"].as_bool();
    for model in models {
        if n_ctx.is_some() {
            model.context_window = n_ctx;
        }
        if vision.is_some() {
            model.vision = vision;
        }
    }
}

/// LM Studio's `/api/v0/models`: the loaded context length (or the
/// maximum) and whether the model is a vision model.
fn apply_lm_studio_models(models: &mut [Model], v0: &Value) {
    let Some(items) = v0["data"].as_array() else {
        return;
    };
    for model in models {
        let Some(item) = items.iter().find(|i| i["id"].as_str() == Some(&model.id)) else {
            continue;
        };
        if let Some(ctx) = item["loaded_context_length"]
            .as_u64()
            .or_else(|| item["max_context_length"].as_u64())
        {
            model.context_window = Some(ctx);
        }
        if let Some(kind) = item["type"].as_str() {
            model.vision = Some(kind == "vlm");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base_urls_end_in_v1() {
        assert_eq!(
            normalize_base_url("127.0.0.1:8080"),
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            normalize_base_url("http://box:1234/v1/"),
            "http://box:1234/v1"
        );
        assert_eq!(normalize_base_url("https://box/"), "https://box/v1");
        assert_eq!(provider_by_port("127.0.0.1:8080"), Some("llamacpp"));
        assert_eq!(provider_by_port("http://box:9/v1"), None);
    }

    #[test]
    fn engines_are_told_apart_by_their_model_list_then_their_port() {
        let sushi = json!({"data": [{"id": "q", "loaded": true, "state": "ready", "context_length": 131072}]});
        assert_eq!(identify(&sushi, Some(9999)).1, "sushi");
        let llama = json!({"data": [{"id": "m.gguf", "owned_by": "llamacpp", "meta": {"n_ctx_train": 262144}}]});
        assert_eq!(identify(&llama, Some(12345)).1, "llamacpp");
        let ollama = json!({"data": [{"id": "qwen3:8b", "owned_by": "library"}]});
        assert_eq!(identify(&ollama, None).0, "Ollama");
        let unknown = json!({"data": [{"id": "x", "owned_by": "someone"}]});
        assert_eq!(identify(&unknown, Some(1234)).1, "lmstudio");
        assert_eq!(identify(&unknown, Some(9)).1, "local");
    }

    #[test]
    fn the_context_window_comes_from_whichever_field_the_engine_uses() {
        let list = json!({"data": [
            {"id": "a", "context_length": 131072},
            {"id": "b", "max_model_len": 32768},
            {"id": "c", "meta": {"n_ctx_train": 262144}},
            {"id": "d"},
            {"no_id": true}
        ]});
        let models = parse_models(&list);
        let windows: Vec<_> = models
            .iter()
            .map(|m| (m.id.as_str(), m.context_window))
            .collect();
        assert_eq!(
            windows,
            [
                ("a", Some(131072)),
                ("b", Some(32768)),
                ("c", Some(262144)),
                ("d", None)
            ]
        );
    }

    #[test]
    fn llama_cpp_props_give_the_running_context_and_vision() {
        let mut models =
            parse_models(&json!({"data": [{"id": "m", "meta": {"n_ctx_train": 262144}}]}));
        apply_llama_cpp_props(
            &mut models,
            &json!({"default_generation_settings": {"n_ctx": 65536}, "modalities": {"vision": true}}),
        );
        assert_eq!(models[0].context_window, Some(65536));
        assert_eq!(models[0].vision, Some(true));
    }

    #[test]
    fn lm_studio_reports_loaded_context_and_model_type() {
        let mut models = parse_models(&json!({"data": [{"id": "a"}, {"id": "b"}]}));
        apply_lm_studio_models(
            &mut models,
            &json!({"data": [
                {"id": "a", "type": "vlm", "max_context_length": 131072, "loaded_context_length": 32768},
                {"id": "b", "type": "llm", "max_context_length": 8192}
            ]}),
        );
        assert_eq!(models[0].context_window, Some(32768));
        assert_eq!(models[0].vision, Some(true));
        assert_eq!(models[1].context_window, Some(8192));
        assert_eq!(models[1].vision, Some(false));
    }
}
