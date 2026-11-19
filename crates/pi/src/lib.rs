//! The Pi harness adapter: runs `pi --mode rpc` as a subprocess per session
//! and turns its JSON event stream into the runner's session events. Pi
//! runs with an agent directory of its own ([`AgentDir`],
//! `PI_CODING_AGENT_DIR`) whose `models.json` points it at the engine proxy
//! or the engine, so the user's `~/.pi/agent` is neither needed nor touched.

pub mod models;
mod session;
pub mod translate;

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tenaz_core::ports::{Harness, HarnessError, HarnessInfo, Session, SessionSpec};

pub use translate::Translator;

/// Pi's agent directory: settings, models, credentials (Pi 1.1.0).
pub const AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// The run's agent directory: `models.json` with the run's models, its
/// own first (see [`models::agent_models_json`]), and a `settings.json`
/// with an HTTP idle timeout long enough for a local engine's prefill and
/// install telemetry off. Pi keeps its own `auth.json` and catalog cache
/// there too. The harness writes it again before every session: Pi exits
/// at once with `Model "<provider>/<id>" not found` without it, and a
/// shell command of the agent's can remove it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDir {
    dir: PathBuf,
    models: String,
    settings: String,
}

impl AgentDir {
    /// The files of `dir` for `models`, on the user's `template`.
    pub fn new(
        dir: &Path,
        models: &models::AgentModels,
        template: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Self {
        // `{:#}` prints a JSON value pretty, as `to_string_pretty` would,
        // and cannot fail.
        Self {
            dir: dir.to_path_buf(),
            models: format!("{:#}", models::agent_models_json(models, template)),
            settings: format!(
                "{:#}",
                serde_json::json!({
                    "httpIdleTimeoutMs": 900_000,
                    "enableInstallTelemetry": false
                })
            ),
        }
    }

    /// Writes both files, creating the directory.
    pub fn write(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.dir.join("models.json"), &self.models)?;
        std::fs::write(self.dir.join("settings.json"), &self.settings)
    }
}

/// The built-in tools a session may use, as the task loop allowed them.
pub const PI_TOOLS: &str = "read,bash,write,edit,grep,find,ls";

/// How Pi is launched for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiLaunch {
    pub binary: PathBuf,
    /// `provider/id`, as Pi names models: the run's model.
    pub model: String,
    /// The other models the run's `models.json` lists (`provider/id`),
    /// which a role's session may ask for (ADR 0011).
    pub other_models: Vec<String>,
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
    /// The agent directory, written before each session.
    pub agent_dir: Option<AgentDir>,
}

impl PiLaunch {
    pub fn new(binary: impl Into<PathBuf>, model: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            model: model.into(),
            other_models: Vec::new(),
            thinking: "medium".to_owned(),
            tools: PI_TOOLS.to_owned(),
            skills: Vec::new(),
            discover_skills: false,
            extra_args: Vec::new(),
            env: Vec::new(),
            wrapper: Vec::new(),
            agent_dir: None,
        }
    }

    /// Lets sessions ask for `models` (`provider/id`) besides the run's.
    #[must_use]
    pub fn other_models(mut self, models: Vec<String>) -> Self {
        self.other_models = models;
        self
    }

    /// This launch for a session of the model `id`, by the name the run's
    /// `models.json` lists it under; `None` when it does not list it.
    pub fn for_model(&self, id: &str) -> Option<Self> {
        let name = self
            .listed()
            .into_iter()
            .find(|name| name.split_once('/').is_some_and(|(_, listed)| listed == id))?;
        Some(Self {
            model: name,
            ..self.clone()
        })
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

    /// Runs Pi with `files` as its agent directory (made absolute: Pi runs
    /// in the project), written before each session.
    #[must_use]
    pub fn agent_dir(mut self, files: AgentDir) -> Self {
        self.env.retain(|(key, _)| key != AGENT_DIR_ENV);
        self.env
            .push((AGENT_DIR_ENV.to_owned(), absolute(&files.dir)));
        self.agent_dir = Some(files);
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

    /// The models a session may ask for, the run's first.
    fn listed(&self) -> Vec<String> {
        std::iter::once(self.model.clone())
            .chain(self.other_models.iter().cloned())
            .collect()
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
        // Pi takes its model at launch, one process per session: each is
        // launched with its role's model (ADR 0011). One the run's
        // `models.json` does not list is refused rather than run on another.
        let Some(launch) = self.launch.for_model(&spec.model.id) else {
            return Err(HarnessError::Spawn(format!(
                "the run's models.json for Pi does not list {}; it lists {}",
                spec.model.name,
                self.launch.listed().join(", ")
            )));
        };
        if let Some(dir) = &launch.agent_dir {
            dir.write().map_err(|e| {
                HarnessError::Spawn(format!("cannot write Pi's agent directory: {e}"))
            })?;
        }
        let (program, args) = tenaz_core::util::wrap_command(
            &launch.wrapper,
            &launch.binary,
            launch.args(spec.persist_dir.as_deref()),
        );
        let session = session::PiSession::spawn(&program, &args, &launch.env, &spec)?;
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

    /// A session is launched with its role's model when the run's
    /// `models.json` lists it, and refused otherwise (ADR 0011).
    #[test]
    fn a_session_runs_its_role_s_model_when_the_run_lists_it() {
        let launch = PiLaunch::new("pi", "sushi/Qwen")
            .other_models(vec!["sushi/Critic".into()])
            .thinking("high")
            .skills(vec![PathBuf::from("/s/rust")])
            .agent_dir(agent_dir(Path::new("/a/pi-agent")))
            .wrapper(vec!["sandbox".into()]);
        let model_of = |id: &str| {
            let args = launch.for_model(id)?.args(None);
            let at = args.iter().position(|a| a == "--model")?;
            Some(args[at + 1].clone())
        };
        assert_eq!(model_of("Critic").as_deref(), Some("sushi/Critic"));
        assert_eq!(model_of("Qwen").as_deref(), Some("sushi/Qwen"));
        assert_eq!(model_of("Other"), None);
        // Only the model changes: the rest of the launch is the run's.
        let critic = launch.for_model("Critic").unwrap();
        let as_run = PiLaunch {
            model: launch.model.clone(),
            ..critic
        };
        assert_eq!(as_run, launch);
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

    /// The run's model `m` on `engine`.
    fn engine_models() -> models::AgentModels {
        models::AgentModels {
            provider: "engine".into(),
            base_url: "http://127.0.0.1:4001/v1".into(),
            run: models::AgentModel {
                id: "m".into(),
                context_window: 32768,
                input: vec!["text".into()],
                reasoning: false,
            },
            others: vec![],
        }
    }

    /// The agent directory `dir` for [`engine_models`].
    fn agent_dir(dir: &Path) -> AgentDir {
        AgentDir::new(dir, &engine_models(), None)
    }

    /// A harness for `engine/m` with its agent directory at `agent`, whose
    /// Pi cannot start; a builder session's spec in `cwd`.
    fn unstartable(cwd: &Path, agent: &Path) -> (PiHarness, SessionSpec) {
        let harness = PiLaunch::new(cwd.join("no-such-pi"), "engine/m")
            .agent_dir(agent_dir(agent))
            .harness("1.4.0".into());
        let spec = SessionSpec {
            label: "001-builder-T001".into(),
            cwd: cwd.to_path_buf(),
            persist_dir: None,
            raw_log: None,
            thinking_log: None,
            stderr_log: None,
            model: tenaz_core::ports::SessionModel {
                name: "engine/m".into(),
                id: "m".into(),
                context_window: 32768,
                supports_images: false,
            },
        };
        (harness, spec)
    }

    /// A session whose agent directory was removed, as a shell command of
    /// the agent's removed `.tenaz/private` in the Raycaster Shooter run of
    /// 2026-11-19, gets it back before Pi starts: without it Pi exits at
    /// once with `Model "<provider>/<id>" not found`.
    #[tokio::test]
    async fn each_session_writes_the_agent_directory_again() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("pi-agent");
        agent_dir(&agent).write().unwrap();
        std::fs::remove_dir_all(&agent).unwrap();
        let (harness, spec) = unstartable(dir.path(), &agent);
        // No Pi to start: the open fails after the directory is written.
        assert!(harness.open(spec).await.is_err());
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(agent.join(name)).unwrap()).unwrap()
        };
        assert_eq!(
            read("models.json")["providers"]["engine"]["models"][0]["id"],
            "m"
        );
        assert_eq!(read("settings.json")["httpIdleTimeoutMs"], 900_000);
    }

    /// An agent directory that cannot be written stops the session before
    /// Pi starts, and says so.
    #[tokio::test]
    async fn an_agent_directory_that_cannot_be_written_stops_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("pi-agent");
        std::fs::write(&agent, "a file where the directory goes").unwrap();
        let (harness, spec) = unstartable(dir.path(), &agent);
        let Err(HarnessError::Spawn(error)) = harness.open(spec).await else {
            panic!("the session started");
        };
        assert!(
            error.starts_with("cannot write Pi's agent directory: "),
            "{error}"
        );
    }

    #[test]
    fn the_agent_directory_holds_the_model_and_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("pi-agent");
        let run = models::AgentModel {
            id: "qwen3.8:27b".into(),
            context_window: 32768,
            input: vec!["text".into(), "image".into()],
            reasoning: true,
        };
        let models = models::AgentModels {
            provider: "ollama".into(),
            base_url: "http://127.0.0.1:4001/v1".into(),
            run,
            others: vec![],
        };
        AgentDir::new(&agent, &models, None).write().unwrap();
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
            .agent_dir(agent_dir(Path::new("rel/pi-agent")))
            .agent_dir(agent_dir(&agent));
        assert_eq!(
            launch.env,
            [
                ("A".to_owned(), "1".to_owned()),
                (AGENT_DIR_ENV.to_owned(), agent.display().to_string()),
            ]
        );
    }
}
