//! The Pi harness adapter: runs `pi --mode rpc` as a subprocess per session
//! and turns its JSON event stream into the runner's session events. Pi
//! runs with an agent directory of its own ([`write_agent_dir`],
//! `PI_CODING_AGENT_DIR`) whose `models.json` points it at the engine proxy
//! or the engine, so the user's `~/.pi/agent` is neither needed nor touched.

pub mod models;
mod session;
pub mod translate;

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use tenaz_core::ports::{Harness, HarnessError, HarnessInfo, Session, SessionSpec};

pub use translate::Translator;

/// Pi's agent directory: settings, models, credentials (Pi 1.1.0).
pub const AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// Writes the run's agent directory: `models.json` with the one model (see
/// [`models::agent_models_json`]) and a `settings.json` with an HTTP idle
/// timeout long enough for a local engine's prefill and install telemetry
/// off. Pi keeps its own `auth.json` and catalog cache there too.
pub fn write_agent_dir(
    dir: &Path,
    model: &models::AgentModel,
    template: Option<serde_json::Map<String, serde_json::Value>>,
) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join("models.json"),
        serde_json::to_string_pretty(&models::agent_models_json(model, template))?,
    )?;
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "httpIdleTimeoutMs": 900_000,
            "enableInstallTelemetry": false
        }))?,
    )?;
    Ok(())
}

/// The built-in tools a session may use, as the task loop allowed them.
pub const PI_TOOLS: &str = "read,bash,write,edit,grep,find,ls";

/// How Pi is launched for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiLaunch {
    pub binary: PathBuf,
    /// `provider/id`, as Pi names models.
    pub model: String,
    /// `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`.
    pub thinking: String,
    /// Comma-separated tool allowlist.
    pub tools: String,
    /// Skill directories loaded explicitly.
    pub skills: Vec<PathBuf>,
    /// Pi also discovers skills in its usual folders (`--no-skills` is left out).
    pub discover_skills: bool,
    /// Extra arguments appended after the pinned ones.
    pub extra_args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// A program and leading arguments Pi is started through: the sandbox
    /// launcher, when the run is sandboxed.
    pub wrapper: Vec<String>,
}

impl PiLaunch {
    pub fn new(binary: impl Into<PathBuf>, model: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            model: model.into(),
            thinking: "medium".to_owned(),
            tools: PI_TOOLS.to_owned(),
            skills: Vec::new(),
            discover_skills: false,
            extra_args: Vec::new(),
            env: Vec::new(),
            wrapper: Vec::new(),
        }
    }

    #[must_use]
    pub fn thinking(mut self, level: impl Into<String>) -> Self {
        self.thinking = level.into();
        self
    }

    #[must_use]
    pub fn skills(mut self, skills: Vec<PathBuf>) -> Self {
        self.skills = skills;
        self
    }

    #[must_use]
    pub fn discover_skills(mut self, discover: bool) -> Self {
        self.discover_skills = discover;
        self
    }

    #[must_use]
    pub fn extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    #[must_use]
    pub fn wrapper(mut self, wrapper: Vec<String>) -> Self {
        self.wrapper = wrapper;
        self
    }

    #[must_use]
    pub fn env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// Runs Pi with `dir` as its agent directory (made absolute: Pi runs in
    /// the project).
    #[must_use]
    pub fn agent_dir(mut self, dir: &Path) -> Self {
        self.env.retain(|(key, _)| key != AGENT_DIR_ENV);
        self.env.push((AGENT_DIR_ENV.to_owned(), absolute(dir)));
        self
    }

    /// The command line of one session: RPC mode,
    /// a session directory or no persistence, the model and thinking level,
    /// the tool allowlist, no MCP, no extensions, no skill discovery unless
    /// asked for (pinned skills are passed explicitly), project files
    /// trusted, offline.
    /// Paths are made absolute against the runner's directory: Pi runs in
    /// the project, where a relative path would point somewhere else.
    pub fn args(&self, session_dir: Option<&Path>) -> Vec<String> {
        let mut args = vec!["--mode".to_owned(), "rpc".to_owned()];
        match session_dir {
            Some(dir) => {
                args.push("--session-dir".to_owned());
                args.push(absolute(dir));
            }
            None => args.push("--no-session".to_owned()),
        }
        args.extend([
            "--model".to_owned(),
            self.model.clone(),
            "--thinking".to_owned(),
            self.thinking.clone(),
            "--tools".to_owned(),
            self.tools.clone(),
            "--no-mcp".to_owned(),
            "--no-extensions".to_owned(),
        ]);
        if !self.discover_skills {
            args.push("--no-skills".to_owned());
        }
        for skill in &self.skills {
            args.push("--skill".to_owned());
            args.push(absolute(skill));
        }
        args.extend(["--approve".to_owned(), "--offline".to_owned()]);
        args.extend(self.extra_args.iter().cloned());
        args
    }

    /// The harness, with the version pinned in the ledger.
    pub fn harness(self, version: String) -> PiHarness {
        PiHarness {
            info: HarnessInfo {
                name: "pi".to_owned(),
                version,
            },
            launch: self,
        }
    }
}

/// `path` against the current directory; unchanged when that cannot be read.
fn absolute(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// The first line of `pi --version`.
pub fn pi_version(binary: &Path) -> io::Result<String> {
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

/// The harness: one Pi process per session.
#[derive(Clone, Debug)]
pub struct PiHarness {
    info: HarnessInfo,
    launch: PiLaunch,
}

impl PiHarness {
    pub fn launch(&self) -> &PiLaunch {
        &self.launch
    }
}

#[async_trait]
impl Harness for PiHarness {
    fn info(&self) -> HarnessInfo {
        self.info.clone()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        // Pi takes its model at launch; a session asking for another is
        // refused rather than run on the wrong one (ADR 0010).
        let launched = self.launch.model.split_once('/').map(|(_, id)| id);
        if launched != Some(spec.model.id.as_str()) {
            return Err(HarnessError::Spawn(format!(
                "Pi was set up for {}, not {}; a model per role is not supported with Pi yet",
                self.launch.model, spec.model.name
            )));
        }
        let (program, args) = tenaz_core::util::wrap_command(
            &self.launch.wrapper,
            &self.launch.binary,
            self.launch.args(spec.persist_dir.as_deref()),
        );
        let session = session::PiSession::spawn(&program, &args, &self.launch.env, &spec)?;
        Ok(Box::new(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_discovery_stays_off_unless_asked_for() {
        let launch = PiLaunch::new("pi", "m");
        assert!(launch.args(None).iter().any(|a| a == "--no-skills"));
        let discovering = launch.discover_skills(true).args(None);
        assert!(!discovering.iter().any(|a| a == "--no-skills"));
    }

    #[test]
    fn args_follow_the_task_loops_pi_args() {
        let launch = PiLaunch::new("pi", "sushi/Qwen3.8")
            .thinking("high")
            .skills(vec![PathBuf::from("/p/.tenaz/skills/rust")]);
        let with_dir = launch.args(Some(Path::new("/p/.tenaz/private/sessions")));
        assert_eq!(
            with_dir,
            [
                "--mode",
                "rpc",
                "--session-dir",
                "/p/.tenaz/private/sessions",
                "--model",
                "sushi/Qwen3.8",
                "--thinking",
                "high",
                "--tools",
                "read,bash,write,edit,grep,find,ls",
                "--no-mcp",
                "--no-extensions",
                "--no-skills",
                "--skill",
                "/p/.tenaz/skills/rust",
                "--approve",
                "--offline",
            ]
        );
        let ephemeral = PiLaunch::new("pi", "fake/fake").args(None);
        assert_eq!(&ephemeral[..3], ["--mode", "rpc", "--no-session"]);
        assert!(!ephemeral.iter().any(|a| a == "--skill"));
        assert_eq!(launch.harness("1.1.0".into()).info().version, "1.1.0");
    }

    /// Pi runs in the project directory; a relative `--project` must not
    /// make it write its sessions under `<project>/<project>/`.
    #[test]
    fn relative_paths_are_resolved_against_the_runners_directory() {
        let cwd = std::env::current_dir().unwrap();
        let args = PiLaunch::new("pi", "fake/fake")
            .skills(vec![PathBuf::from("proj/.tenaz/skills/rust")])
            .args(Some(Path::new("proj/.tenaz/private/sessions")));
        let after = |flag: &str| {
            let at = args.iter().position(|a| a == flag).unwrap();
            PathBuf::from(&args[at + 1])
        };
        assert_eq!(
            after("--session-dir"),
            cwd.join("proj/.tenaz/private/sessions")
        );
        assert_eq!(after("--skill"), cwd.join("proj/.tenaz/skills/rust"));
    }

    #[test]
    fn the_agent_directory_holds_the_model_and_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("pi-agent");
        let model = models::AgentModel {
            provider: "ollama".into(),
            id: "qwen3.8:27b".into(),
            base_url: "http://127.0.0.1:4001/v1".into(),
            context_window: 32768,
            input: vec!["text".into(), "image".into()],
            reasoning: true,
        };
        write_agent_dir(&agent, &model, None).unwrap();
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(agent.join(name)).unwrap()).unwrap()
        };
        let models = read("models.json");
        assert_eq!(
            models["providers"]["ollama"]["models"][0]["id"],
            "qwen3.8:27b"
        );
        assert_eq!(
            models["providers"]["ollama"]["baseUrl"],
            "http://127.0.0.1:4001/v1"
        );
        assert_eq!(read("settings.json")["httpIdleTimeoutMs"], 900_000);

        let launch = PiLaunch::new("pi", "ollama/qwen3.8:27b")
            .env(vec![("A".into(), "1".into())])
            .agent_dir(Path::new("rel/pi-agent"))
            .agent_dir(&agent);
        assert_eq!(
            launch.env,
            [
                ("A".to_owned(), "1".to_owned()),
                (AGENT_DIR_ENV.to_owned(), agent.display().to_string()),
            ]
        );
    }

    #[test]
    fn version_of_a_missing_binary_is_an_error() {
        assert!(pi_version(Path::new("/nonexistent/pi")).is_err());
    }

    /// Against the installed Pi when there is one; skipped otherwise.
    #[test]
    fn version_of_the_installed_pi_is_a_version_number() {
        let Ok(version) = pi_version(Path::new("pi")) else {
            eprintln!("pi not in PATH; skipping");
            return;
        };
        assert!(
            version.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "{version}"
        );
    }
}
