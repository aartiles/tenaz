//! fx specifics: its profile files in an isolated `HOME` (settings and MCP
//! servers) and the harness that launches it.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use smr_core::ports::HarnessInfo;

use crate::AcpHarness;

/// `~/.fx/settings.json` as the spike used it: one OpenAI-compatible provider
/// (the engine proxy) with the model's metadata, and `ask` permissions so
/// the runner is the control point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FxSettings {
    pub provider: String,
    /// The proxy's base URL (`http://127.0.0.1:<port>/v1`; `http` is accepted only for localhost).
    pub base_url: String,
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub supports_vision: bool,
    /// `ask`: fx asks the client before every tool call.
    pub permission_mode: String,
}

impl FxSettings {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "providers": {
                &self.provider: {
                    "protocol": "openai-chat-completions",
                    "base_url": self.base_url,
                    "auth": { "type": "none" },
                    "model_metadata": {
                        &self.model: {
                            "context_window": self.context_window,
                            "max_output_tokens": self.max_output_tokens,
                            "supports_tool_use": true,
                            "supports_vision": self.supports_vision
                        }
                    }
                }
            },
            "provider": self.provider,
            "models": { &self.provider: self.model },
            "permission_mode": self.permission_mode
        })
    }
}

/// An MCP server in `~/.fx/mcp.json`. Profile servers accept a long
/// `operation_timeout_ms`; servers passed in `session/new` are cut at 60 s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileMcpServer {
    pub name: String,
    pub url: String,
    pub operation_timeout_ms: u64,
}

/// Writes `settings.json` and `mcp.json` under `<home>/.fx/`.
pub fn write_profile(
    home: &Path,
    settings: &FxSettings,
    mcp: &[ProfileMcpServer],
) -> io::Result<()> {
    let dir = home.join(".fx");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&settings.to_json())?,
    )?;
    let servers: serde_json::Map<String, serde_json::Value> = mcp
        .iter()
        .map(|s| (s.name.clone(), serde_json::json!({ "type": "http", "url": s.url, "enabled": true, "operation_timeout_ms": s.operation_timeout_ms })))
        .collect();
    std::fs::write(
        dir.join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "mcp": servers }))?,
    )?;
    Ok(())
}

/// The first line of `fx --version`.
pub fn fx_version(binary: &Path) -> io::Result<String> {
    let output = Command::new(binary).arg("--version").output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{} --version exited with {}",
            binary.display(),
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned())
}

/// How fx is launched for ACP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FxLaunch {
    pub binary: PathBuf,
    /// The isolated `HOME` holding `.fx/settings.json` and `.fx/mcp.json`.
    pub home: PathBuf,
    /// Arguments that put fx in ACP mode. Pinned per fx version.
    pub args: Vec<String>,
}

impl FxLaunch {
    pub fn new(binary: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            home: home.into(),
            args: vec!["acp".to_owned()],
        }
    }

    /// The harness: fx as a subprocess with `HOME` set to the profile.
    pub fn harness(&self, version: String) -> AcpHarness {
        let info = HarnessInfo {
            name: "fx".to_owned(),
            version,
        };
        AcpHarness::subprocess(
            info,
            self.binary.clone(),
            self.args.clone(),
            vec![("HOME".to_owned(), self.home.display().to_string())],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smr_core::ports::Harness as _;

    #[test]
    fn profile_files_match_the_spike_layout() {
        let home = tempfile::tempdir().unwrap();
        let settings = FxSettings {
            provider: "sushi".into(),
            base_url: "http://127.0.0.1:4001/v1".into(),
            model: "Qwen3.8".into(),
            context_window: 131072,
            max_output_tokens: 16384,
            supports_vision: true,
            permission_mode: "ask".into(),
        };
        let mcp = vec![ProfileMcpServer {
            name: "lease".into(),
            url: "http://127.0.0.1:4002/mcp".into(),
            operation_timeout_ms: 600_000,
        }];
        write_profile(home.path(), &settings, &mcp).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".fx/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            written["providers"]["sushi"]["base_url"],
            "http://127.0.0.1:4001/v1"
        );
        assert_eq!(
            written["providers"]["sushi"]["model_metadata"]["Qwen3.8"]["context_window"],
            131072
        );
        assert_eq!(written["models"]["sushi"], "Qwen3.8");
        assert_eq!(written["permission_mode"], "ask");
        let servers: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".fx/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(servers["mcp"]["lease"]["operation_timeout_ms"], 600_000);
        assert_eq!(servers["mcp"]["lease"]["type"], "http");
    }

    #[test]
    fn version_of_a_missing_binary_is_an_error() {
        assert!(fx_version(Path::new("/nonexistent/fx")).is_err());
        let launch = FxLaunch::new("/usr/local/bin/fx", "/tmp/home");
        assert_eq!(launch.args, vec!["acp"]);
        assert_eq!(launch.harness("0.0.14".into()).info().version, "0.0.14");
    }
}
