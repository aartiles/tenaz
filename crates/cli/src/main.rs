//! `smr`: the single-model runner command line.

mod wiring;

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use smr_core::ledger::ModelConfig;
use smr_core::orchestrator::{InitOptions, RunOptions, Runner, Templates, init, status_text};
use smr_core::params::RunParams;
use smr_core::project::Project;
use tokio_util::sync::CancellationToken;

/// The runner's version string, journaled per run.
fn runner_version() -> String {
    format!("smr {}", env!("CARGO_PKG_VERSION"))
}

/// The role prompts and shared rules compiled into the binary.
fn templates() -> Templates {
    use smr_core::role::Role;
    let prompts = [
        (
            Role::Bootstrap,
            include_str!("../templates/prompts/bootstrap.md"),
        ),
        (
            Role::Builder,
            include_str!("../templates/prompts/builder.md"),
        ),
        (
            Role::Reviewer,
            include_str!("../templates/prompts/reviewer.md"),
        ),
        (Role::Split, include_str!("../templates/prompts/split.md")),
        (Role::Replan, include_str!("../templates/prompts/replan.md")),
        (
            Role::FinalReviewer,
            include_str!("../templates/prompts/final-reviewer.md"),
        ),
    ];
    Templates {
        agents_md: include_str!("../templates/AGENTS.md").to_owned(),
        prompts: prompts
            .into_iter()
            .map(|(r, t)| (r, t.to_owned()))
            .collect(),
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "smr",
    version,
    about = "Single-model runner: one resident model, a goal, a verified result."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Which harness runs the sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum HarnessKind {
    /// fx over ACP (default).
    Fx,
    /// The scripted fake harness: exercises the runner without a model.
    Fake,
}

/// Which engine API loads and unloads the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum EngineKind {
    /// Sushi: `/v1/load-model` and `/v1/unload-model`.
    Sushi,
    /// Any OpenAI-compatible engine with start and stop commands.
    Generic,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// A new project from a brief; the model, harness and parameters are pinned in .agent/run.json.
    Init {
        #[arg(long)]
        project: PathBuf,
        /// Copied to BRIEF.md (or put BRIEF.md in the project first).
        #[arg(long)]
        brief: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "fx")]
        harness: HarnessKind,
        /// The fx executable.
        #[arg(long, default_value = "fx")]
        fx_bin: PathBuf,
        /// The model id as the engine lists it.
        #[arg(long)]
        model: String,
        /// The provider name used in the harness configuration.
        #[arg(long, default_value = "sushi")]
        provider: String,
        /// The engine's OpenAI-compatible base URL.
        #[arg(long, default_value = "http://127.0.0.1:12345/v1")]
        base_url: String,
        #[arg(long, default_value_t = 131_072)]
        context_window: u64,
        /// The model reads images.
        #[arg(long)]
        vision: bool,
        #[arg(long, default_value = "medium")]
        thinking: String,
        /// Parameter overrides, `key=value` (see `smr params`).
        #[arg(long = "set")]
        sets: Vec<String>,
        /// Skill directories to pin read-only into .agent/skills/.
        #[arg(long = "skill")]
        skills: Vec<PathBuf>,
    },
    /// Run (resumable). Ctrl-C pauses; run again to continue.
    Run {
        #[arg(long)]
        project: PathBuf,
        /// Continue a BLOCKED run.
        #[arg(long)]
        resume: bool,
        /// Record a failed preflight and run anyway.
        #[arg(long)]
        allow_weak_engine: bool,
        /// Copy the binary's templates into a started run.
        #[arg(long)]
        refresh_templates: bool,
        #[arg(long, default_value = "fx")]
        fx_bin: PathBuf,
        /// Arguments that put fx in ACP mode.
        #[arg(long, default_value = "acp", value_delimiter = ' ')]
        fx_args: Vec<String>,
        #[arg(long, value_enum, default_value = "sushi")]
        engine: EngineKind,
        /// Generic engine: the command that starts it (the whole command line).
        #[arg(long)]
        engine_start: Option<String>,
        /// Generic engine: the command that stops it.
        #[arg(long)]
        engine_stop: Option<String>,
        /// Talk to the engine directly instead of through the proxy.
        #[arg(long)]
        no_proxy: bool,
        /// `ComfyUI` base URL; enables the image tool.
        #[arg(long)]
        comfyui: Option<String>,
        /// Image workflows, `name=path.json` (API-format workflow with {{prompt}} placeholders).
        #[arg(long = "workflow")]
        workflows: Vec<String>,
        /// Media job time limit, in seconds.
        #[arg(long, default_value_t = 1800)]
        media_timeout: u64,
    },
    /// Where the run is (reads files only).
    Status {
        #[arg(long)]
        project: PathBuf,
    },
    /// The tunable parameters and their defaults.
    Params,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Init {
            project,
            brief,
            harness,
            fx_bin,
            model,
            provider,
            base_url,
            context_window,
            vision,
            thinking,
            sets,
            skills,
        } => {
            let project = Project::new(project);
            let mut params = RunParams::default();
            for pair in &sets {
                params.apply_override(pair)?;
            }
            let (harness_name, harness_version) = match harness {
                HarnessKind::Fx => (
                    "fx".to_owned(),
                    smr_acp::fx::fx_version(&fx_bin)
                        .with_context(|| format!("cannot run {} --version", fx_bin.display()))?,
                ),
                HarnessKind::Fake => (
                    "fake".to_owned(),
                    smr_core::testing::ScriptedHarness::default().version(),
                ),
            };
            let mut input = vec!["text".to_owned()];
            if vision {
                input.push("image".to_owned());
            }
            let config = ModelConfig {
                harness: harness_name,
                harness_version,
                model: format!("{provider}/{model}"),
                provider: Some(provider),
                id: model,
                base_url: Some(base_url),
                context_window,
                input,
                thinking: Some(thinking),
            };
            let vcs = smr_adapters::git::GitRepo::new(project.root());
            let clock = smr_adapters::clock::SystemClock::new();
            let options = InitOptions {
                brief,
                config,
                params,
                skills,
                runner_version: runner_version(),
            };
            let ledger = init(&project, &vcs, &clock, &templates(), options)?;
            println!(
                "Initialized {} for {} ({} {})",
                project.root().display(),
                ledger.config.model,
                ledger.config.harness,
                ledger.config.harness_version
            );
            Ok(())
        }
        Command::Run {
            project,
            resume,
            allow_weak_engine,
            refresh_templates,
            fx_bin,
            fx_args,
            engine,
            engine_start,
            engine_stop,
            no_proxy,
            comfyui,
            workflows,
            media_timeout,
        } => {
            let project = Project::new(project);
            let stop = CancellationToken::new();
            spawn_signal_handler(stop.clone());
            let media = wiring::MediaOptions {
                comfyui,
                workflows: wiring::parse_workflows(&workflows)?,
                timeout: std::time::Duration::from_secs(media_timeout),
            };
            let engine = wiring::EngineOptions {
                kind: match engine {
                    EngineKind::Sushi => wiring::EngineChoice::Sushi,
                    EngineKind::Generic => wiring::EngineChoice::Generic {
                        start: engine_start,
                        stop: engine_stop,
                    },
                },
                proxy: !no_proxy,
            };
            let harness = wiring::HarnessOptions { fx_bin, fx_args };
            let wired = wiring::wire(&project, &engine, &harness, &media).await?;
            let mut runner = Runner::open(project, wired.deps, stop)?;
            let options = RunOptions {
                resume,
                allow_weak_engine,
                refresh_templates: refresh_templates.then(templates),
                runner_version: runner_version(),
            };
            let outcome = runner.run(options).await;
            wired.services.stop().await;
            outcome?;
            Ok(())
        }
        Command::Status { project } => {
            print!("{}", status_text(&Project::new(project)));
            Ok(())
        }
        Command::Params => {
            let json = serde_json::to_value(RunParams::default())?;
            if let Some(map) = json.as_object() {
                for (key, value) in map {
                    println!("{key}={value}");
                }
            }
            Ok(())
        }
    }
}

/// Ctrl-C or SIGTERM pause the run; a second signal is left to the OS.
fn spawn_signal_handler(stop: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        tracing::info!("stop requested: the run pauses after the current session");
        stop.cancel();
    });
}
