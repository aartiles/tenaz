//! What every ACP agent Tenaz drives has in common (ADR 0027): a
//! configuration written for each session's model, a command line that puts
//! it in ACP mode, and an environment. Each agent is one [`AgentProfile`];
//! [`harness`] and [`configured`] turn any of them into a harness.

use std::io;
use std::path::Path;
use std::sync::Arc;

use tenaz_core::ledger::HarnessKind;
use tenaz_core::ports::{HarnessInfo, SessionModel, SessionSpec};
use tenaz_core::util::wrap_command;

use crate::{AcpHarness, ConfiguredHarness};

/// One ACP agent: how it starts and what it reads when it does.
pub trait AgentProfile: Send + Sync + 'static {
    /// The kind `run.json` records for it.
    fn kind(&self) -> HarnessKind;

    /// Its configuration as errors name it: `the fx profile`.
    fn config_name(&self) -> &'static str;

    /// The program started for every session, before the wrapper.
    fn program(&self) -> &Path;

    /// The arguments of one session's process, before the wrapper.
    fn args(&self, spec: &SessionSpec) -> Vec<String>;

    /// The environment it starts with (its isolated `HOME`, at least).
    fn env(&self) -> Vec<(String, String)>;

    /// A program and leading arguments it is started through: the sandbox
    /// launcher, when the run is sandboxed.
    fn wrapper(&self) -> &[String];

    /// Writes its configuration for `model`, before a session on it starts.
    fn write_config(&self, model: &SessionModel) -> io::Result<()>;
}

/// The agent as a subprocess per session, through its wrapper, reporting
/// `version`.
pub fn harness(agent: Arc<dyn AgentProfile>, version: String) -> AcpHarness {
    let info = HarnessInfo {
        name: agent.kind().as_str().to_owned(),
        version,
    };
    let (program, _) = wrap_command(agent.wrapper(), agent.program(), Vec::new());
    let env = agent.env();
    AcpHarness::subprocess_with(
        info,
        program,
        Arc::new(move |spec| wrap_command(agent.wrapper(), agent.program(), agent.args(spec)).1),
        env,
    )
}

/// `acp` with the agent's configuration written for each session's model
/// before it starts, and now for the run's `first`.
pub fn configured(
    agent: Arc<dyn AgentProfile>,
    acp: AcpHarness,
    first: &SessionModel,
) -> io::Result<ConfiguredHarness> {
    let name = agent.config_name();
    ConfiguredHarness::new(
        acp,
        name,
        Arc::new(move |model| agent.write_config(model)),
        first,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tenaz_core::ports::Harness as _;

    use super::*;

    /// An agent that is never started.
    struct Idle {
        program: PathBuf,
        wrapper: Vec<String>,
    }

    impl AgentProfile for Idle {
        fn kind(&self) -> HarnessKind {
            HarnessKind::OpenCode
        }
        fn config_name(&self) -> &'static str {
            "nothing"
        }
        fn program(&self) -> &Path {
            &self.program
        }
        fn args(&self, spec: &SessionSpec) -> Vec<String> {
            vec!["--session".to_owned(), spec.label.clone()]
        }
        fn env(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn wrapper(&self) -> &[String] {
            &self.wrapper
        }
        fn write_config(&self, _model: &SessionModel) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_harness_reports_the_agent_s_kind_and_version() {
        let agent: Arc<dyn AgentProfile> = Arc::new(Idle {
            program: "/opt/agent".into(),
            wrapper: Vec::new(),
        });
        let info = harness(agent, "1.19.5".into()).info();
        assert_eq!(
            (info.name.as_str(), info.version.as_str()),
            ("opencode", "1.19.5")
        );
    }

    /// Each session starts the agent's arguments for it, through the
    /// wrapper (the sandbox launcher) when there is one.
    #[cfg(unix)]
    #[tokio::test]
    async fn each_session_starts_its_arguments_through_the_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let argv = dir.path().join("argv");
        let recorder = dir.path().join("record");
        let record = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 1\n",
            argv.display()
        );
        tenaz_adapters::testing::write_executable(&recorder, &record);
        let agent: Arc<dyn AgentProfile> = Arc::new(Idle {
            program: "/opt/agent".into(),
            wrapper: vec![recorder.display().to_string(), "--sandboxed".into()],
        });
        let spec = SessionSpec {
            label: "001-builder-T001".into(),
            cwd: dir.path().to_path_buf(),
            persist_dir: None,
            raw_log: None,
            thinking_log: None,
            stderr_log: None,
            model: SessionModel {
                name: "p/m".into(),
                id: "m".into(),
                context_window: 65536,
                supports_images: false,
            },
        };
        let opened = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            harness(agent, "1".into()).open(spec),
        )
        .await
        .expect("the launch ends");
        assert!(opened.is_err(), "the recorder is not an ACP agent");
        let args = std::fs::read_to_string(&argv).expect("the recorder ran");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            ["--sandboxed", "/opt/agent", "--session", "001-builder-T001"]
        );
    }
}
