//! `tenaz run`: the launch settings, the wiring, and the run until it
//! ends, pauses or fails, with its exit code.

use std::path::PathBuf;
use std::process::ExitCode;

use tenaz_core::ledger::{EngineApi, HarnessKind, Launch, Phase};
use tenaz_core::orchestrator::{RunOptions, Runner};
use tokio_util::sync::CancellationToken;

use super::{EngineKind, initialized, pinned_path, pinned_workflows, runner_version, templates};
use crate::{wiring, worktree};

/// `tenaz run`. Every launch flag is optional: without it the value pinned
/// in run.json is used; with it, the new value is used and saved there.
#[derive(clap::Args, Debug)]
pub(crate) struct RunArgs {
    /// The run's directory: the one init printed (a worktree for an existing repository).
    #[arg(long, default_value = ".")]
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
    /// The fx executable (default: the one pinned at init, else fx on PATH).
    #[arg(long)]
    fx_bin: Option<PathBuf>,
    /// Arguments that put fx in ACP mode (default: acp).
    #[arg(long, value_delimiter = ' ')]
    fx_args: Option<Vec<String>>,
    /// The pi executable (default: the one pinned at init, else pi on PATH).
    #[arg(long)]
    pi_bin: Option<PathBuf>,
    /// The engine API (default: the one pinned at init).
    #[arg(long, value_enum)]
    engine: Option<EngineKind>,
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
    /// Image workflows, `name=path.json`; replaces the pinned ones.
    #[arg(long = "workflow")]
    workflows: Vec<String>,
    /// Base URL of an `OpenAI` Images API; replaces the pinned one.
    #[arg(long)]
    images: Option<String>,
    /// The image model to ask the `--images` API for.
    #[arg(long)]
    image_model: Option<String>,
    /// Media job time limit, in seconds.
    #[arg(long)]
    media_timeout: Option<u64>,
}

impl RunArgs {
    /// `launch` with this command line's flags applied.
    fn apply(&self, launch: &Launch, harness: HarnessKind) -> anyhow::Result<Launch> {
        let mut next = launch.clone();
        if let Some(engine) = self.engine {
            next.engine = engine.api();
        }
        replace(&mut next.engine_start, self.engine_start.as_ref());
        replace(&mut next.engine_stop, self.engine_stop.as_ref());
        next.no_proxy |= self.no_proxy;
        if let Some(bin) = self.harness_bin(harness) {
            next.harness_bin = Some(pinned_path(bin));
        }
        replace(&mut next.fx_args, self.fx_args.as_ref());
        replace(&mut next.comfyui, self.comfyui.as_ref());
        if !self.workflows.is_empty() {
            next.workflows = pinned_workflows(&self.workflows)?;
        }
        replace(&mut next.images_url, self.images.as_ref());
        replace(&mut next.image_model, self.image_model.as_ref());
        replace(&mut next.media_timeout_sec, self.media_timeout.as_ref());
        Ok(next)
    }

    /// The `--fx-bin` or `--pi-bin` given for `harness`.
    fn harness_bin(&self, harness: HarnessKind) -> Option<&std::path::Path> {
        match harness {
            HarnessKind::Fx => self.fx_bin.as_deref(),
            HarnessKind::Pi => self.pi_bin.as_deref(),
            HarnessKind::Fake => None,
        }
    }
}

/// `slot` takes the value of `flag` when the flag was given.
fn replace<T: Clone>(slot: &mut Option<T>, flag: Option<&T>) {
    if let Some(value) = flag {
        *slot = Some(value.clone());
    }
}

/// `tenaz run`: works on the goal until it ends, pauses or fails.
pub(crate) async fn execute(args: RunArgs) -> anyhow::Result<ExitCode> {
    let project = initialized(args.project.clone())?;
    let mut ledger = project.load_ledger()?;
    if let Some(origin) = &ledger.origin {
        tracing::info!("{}", worktree::describe(origin));
    }
    let launch = args.apply(&ledger.launch, ledger.config.harness)?;
    if launch != ledger.launch {
        ledger.launch = launch.clone();
        project.save_ledger(&ledger)?;
        tracing::info!("saved the new launch settings in .tenaz/run.json");
    }
    let stop = CancellationToken::new();
    spawn_signal_handler(stop.clone());
    let workflow_specs: Vec<String> = launch
        .workflows
        .iter()
        .map(|(name, path)| format!("{name}={path}"))
        .collect();
    let media = wiring::MediaOptions {
        comfyui: launch.comfyui.clone(),
        workflows: wiring::parse_workflows(&workflow_specs)?,
        images_url: launch.images_url.clone(),
        image_model: launch.image_model.clone(),
        timeout: std::time::Duration::from_secs(launch.media_timeout_sec.unwrap_or(1800)),
    };
    let engine = wiring::EngineOptions {
        kind: match launch.engine {
            EngineApi::LoadApi => wiring::EngineChoice::LoadApi,
            EngineApi::Generic => wiring::EngineChoice::Generic {
                start: launch.engine_start.clone(),
                stop: launch.engine_stop.clone(),
            },
        },
        proxy: !launch.no_proxy,
    };
    let bin = |kind: HarnessKind| {
        PathBuf::from(
            launch
                .harness_bin
                .clone()
                .filter(|_| ledger.config.harness == kind)
                .unwrap_or_else(|| kind.as_str().to_owned()),
        )
    };
    let harness = wiring::HarnessOptions {
        fx_bin: bin(HarnessKind::Fx),
        fx_args: launch
            .fx_args
            .clone()
            .unwrap_or_else(|| vec!["acp".to_owned()]),
        pi_bin: bin(HarnessKind::Pi),
    };
    let wired = wiring::wire(&project, &engine, &harness, &media).await?;
    let mut runner = Runner::open(project.clone(), wired.deps, stop)?;
    let options = RunOptions {
        resume: args.resume,
        allow_weak_engine: args.allow_weak_engine,
        refresh_templates: args.refresh_templates.then(templates),
        runner_version: runner_version(),
    };
    let outcome = runner.run(options).await;
    wired.services.stop().await;
    outcome?;
    Ok(run_exit_code(&project.load_ledger()?))
}

/// `run`'s exit code once it returns without an error: 0 when the goal is
/// met, then one code per way it can stop short. 1 is any error and 2 a
/// usage error.
fn run_exit_code(ledger: &tenaz_core::ledger::Ledger) -> ExitCode {
    match ledger.phase {
        Phase::Complete => ExitCode::SUCCESS,
        Phase::CompleteWithGaps => ExitCode::from(3),
        Phase::Blocked => ExitCode::from(4),
        // A run that returns without an error and still runs paused.
        _ => ExitCode::from(5),
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
