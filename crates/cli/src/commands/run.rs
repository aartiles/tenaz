//! `tenaz run`: the launch settings, the wiring, and the run until it
//! ends, pauses or fails, with its exit code.

use std::path::PathBuf;
use std::process::ExitCode;

use tenaz_core::ledger::{EngineApi, HarnessKind, Launch, Phase, swap_refusal};
use tenaz_core::mcp_servers::{check_servers, parse_mcp_servers};
use tenaz_core::orchestrator::{RunOptions, Runner};
use tokio_util::sync::CancellationToken;

use super::harness_bins::HarnessBins;
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
    /// Record a failed preflight and run anyway; not when the engine could
    /// not take its requests (out of memory, busy, not reached).
    #[arg(long)]
    allow_weak_engine: bool,
    /// Copy the binary's templates into a started run.
    #[arg(long)]
    refresh_templates: bool,
    #[command(flatten)]
    bins: HarnessBins,
    /// Arguments that put fx in ACP mode (default: acp).
    #[arg(long, value_delimiter = ' ')]
    fx_args: Option<Vec<String>>,
    /// Arguments that put the generic agent (--harness acp) in ACP mode
    /// (default: none). It may start with `-`.
    #[arg(long, value_delimiter = ' ', allow_hyphen_values = true)]
    acp_args: Option<Vec<String>>,
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
    /// Keep one of the run's models loaded at a time and swap them before
    /// each session (needs a --model-for map and --engine load-api).
    #[arg(long, conflicts_with = "no_swap_models")]
    swap_models: bool,
    /// Keep every model of the run loaded at once again.
    #[arg(long)]
    no_swap_models: bool,
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
    /// Base URL of an image-to-3D API; replaces the pinned one.
    #[arg(long)]
    meshes: Option<String>,
    /// The 3D model to ask the `--meshes` API for.
    #[arg(long)]
    mesh_model: Option<String>,
    /// Media job time limit, in seconds.
    #[arg(long)]
    media_timeout: Option<u64>,
    /// MCP servers of your own, `name=url`; replaces the pinned ones.
    #[arg(long = "mcp")]
    mcp_servers: Vec<String>,
    /// Drop the pinned --mcp servers.
    #[arg(long, conflicts_with = "mcp_servers")]
    no_mcp: bool,
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
        if self.swap_models || self.no_swap_models {
            next.swap_models = self.swap_models;
        }
        if let Some(bin) = self.bins.get(harness) {
            next.harness_bin = Some(pinned_path(bin));
        }
        replace(&mut next.fx_args, self.fx_args.as_ref());
        replace(&mut next.acp_args, self.acp_args.as_ref());
        replace(&mut next.comfyui, self.comfyui.as_ref());
        if !self.workflows.is_empty() {
            next.workflows = pinned_workflows(&self.workflows)?;
        }
        replace(&mut next.images_url, self.images.as_ref());
        replace(&mut next.image_model, self.image_model.as_ref());
        replace(&mut next.meshes_url, self.meshes.as_ref());
        replace(&mut next.mesh_model, self.mesh_model.as_ref());
        replace(&mut next.media_timeout_sec, self.media_timeout.as_ref());
        if self.no_mcp {
            next.mcp_servers.clear();
        } else if !self.mcp_servers.is_empty() {
            next.mcp_servers = parse_mcp_servers(&self.mcp_servers)?;
        }
        Ok(next)
    }

    /// The run's pinned launch with these flags applied, refused when it
    /// swaps models the run cannot swap or names MCP servers its harness
    /// would not see.
    fn launch_for(&self, ledger: &tenaz_core::ledger::Ledger) -> anyhow::Result<Launch> {
        let launch = self.apply(&ledger.launch, ledger.config.harness)?;
        let role_models = !ledger.config.role_models.is_empty();
        if let Some(why) = swap_refusal(launch.engine, launch.swap_models, role_models) {
            anyhow::bail!("{why}; tenaz run --no-swap-models turns swapping off");
        }
        check_servers(&launch.mcp_servers, ledger.config.harness).map_err(|why| {
            let fix = if self.mcp_servers.is_empty() {
                "tenaz run --no-mcp drops the pinned servers"
            } else {
                "drop --mcp: a run keeps the harness init gave it"
            };
            anyhow::anyhow!("{why}; {fix}")
        })?;
        Ok(launch)
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
    let launch = args.launch_for(&ledger)?;
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
        meshes_url: launch.meshes_url.clone(),
        mesh_model: launch.mesh_model.clone(),
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
        swap: launch.swap_models,
    };
    let harness = wiring::HarnessOptions {
        bin: PathBuf::from(
            launch
                .harness_bin
                .clone()
                .unwrap_or_else(|| ledger.config.harness.as_str().to_owned()),
        ),
        fx_args: launch
            .fx_args
            .clone()
            .unwrap_or_else(|| vec!["acp".to_owned()]),
        acp_args: launch.acp_args.clone().unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use clap::Parser as _;
    use tenaz_core::mcp_servers::McpServerError;

    use super::*;

    fn run_args(line: &[&str]) -> RunArgs {
        let crate::Command::Run(args) = crate::Cli::parse_from(line).command else {
            panic!("not a run line: {line:?}");
        };
        *args
    }

    /// `run` turns swapping on and off; without either flag the pinned
    /// value stays.
    #[test]
    fn swap_models_is_turned_on_and_off_by_run() {
        let swap = |line: &[&str], pinned: bool| {
            let launch = Launch {
                swap_models: pinned,
                ..Launch::default()
            };
            let next = run_args(line).apply(&launch, HarnessKind::Fx).unwrap();
            next.swap_models
        };
        assert!(swap(&["tenaz", "run", "--swap-models"], false));
        assert!(!swap(&["tenaz", "run", "--no-swap-models"], true));
        assert!(swap(&["tenaz", "run"], true));
        assert!(!swap(&["tenaz", "run"], false));
    }

    #[test]
    fn run_pins_the_generic_agent_s_arguments() {
        let launch = Launch::default();
        let line = ["tenaz", "run", "--acp-args", "--profile acp"];
        let next = run_args(&line).apply(&launch, HarnessKind::Acp).unwrap();
        assert_eq!(
            next.acp_args,
            Some(vec!["--profile".to_owned(), "acp".to_owned()])
        );
        let kept = run_args(&["tenaz", "run"])
            .apply(&next, HarnessKind::Acp)
            .unwrap();
        assert_eq!(kept.acp_args, next.acp_args);
    }

    #[test]
    fn run_replaces_the_pinned_binary_only_with_its_harness_s_flag() {
        let launch = Launch {
            harness_bin: Some("/pinned/qwen".into()),
            ..Launch::default()
        };
        let bin = |line: &[&str]| {
            let next = run_args(line).apply(&launch, HarnessKind::Qwen).unwrap();
            next.harness_bin
        };
        let both = [
            "tenaz",
            "run",
            "--fx-bin",
            "/opt/fx",
            "--qwen-bin",
            "/opt/qwen",
        ];
        assert_eq!(bin(&both), Some("/opt/qwen".to_owned()));
        assert_eq!(
            bin(&["tenaz", "run", "--fx-bin", "/opt/fx"]),
            Some("/pinned/qwen".to_owned())
        );
    }

    fn servers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(n, u)| ((*n).to_owned(), (*u).to_owned()))
            .collect()
    }

    /// The servers `line` leaves the run with when `pinned` were pinned.
    fn mcp_after(
        line: &[&str],
        pinned: &[(&str, &str)],
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let launch = Launch {
            mcp_servers: servers(pinned),
            ..Launch::default()
        };
        Ok(run_args(line).apply(&launch, HarnessKind::Fx)?.mcp_servers)
    }

    const PINNED: [(&str, &str); 1] = [("old", "http://o/")];

    #[test]
    fn run_keeps_the_pinned_mcp_servers_without_a_flag() {
        assert_eq!(
            mcp_after(&["tenaz", "run"], &PINNED).unwrap(),
            servers(&PINNED)
        );
    }

    #[test]
    fn run_mcp_replaces_the_pinned_servers() {
        let line = ["tenaz", "run", "--mcp", "blender=http://127.0.0.1:9191/"];
        assert_eq!(
            mcp_after(&line, &PINNED).unwrap(),
            servers(&[("blender", "http://127.0.0.1:9191/")])
        );
    }

    #[test]
    fn run_no_mcp_clears_the_pinned_servers() {
        assert!(
            mcp_after(&["tenaz", "run", "--no-mcp"], &PINNED)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn run_no_mcp_conflicts_with_mcp() {
        let both = ["tenaz", "run", "--mcp", "a=http://h/", "--no-mcp"];
        let error = crate::Cli::try_parse_from(both).err().unwrap();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn run_refuses_a_bad_mcp_server() {
        let error = mcp_after(&["tenaz", "run", "--mcp", "media=http://m/"], &[]).unwrap_err();
        assert_eq!(error.to_string(), McpServerError::Reserved.to_string());
    }

    /// A ledger for `harness` with two servers pinned.
    fn pinned_on(harness: HarnessKind) -> tenaz_core::ledger::Ledger {
        let mut ledger = crate::testing::ledger_for(harness);
        ledger.launch.mcp_servers = servers(&PINNED);
        ledger
    }

    #[test]
    fn pinned_servers_on_pi_or_dsh_are_refused_with_the_no_mcp_hint() {
        for harness in [HarnessKind::Dsh, HarnessKind::Pi] {
            let why = run_args(&["tenaz", "run"])
                .launch_for(&pinned_on(harness))
                .unwrap_err();
            assert_eq!(
                why.to_string(),
                format!(
                    "{}; tenaz run --no-mcp drops the pinned servers",
                    McpServerError::NotPassed(harness)
                ),
                "{harness}"
            );
        }
    }

    #[test]
    fn no_mcp_lets_a_pi_or_dsh_run_start() {
        let cleared = run_args(&["tenaz", "run", "--no-mcp"]);
        for harness in [HarnessKind::Dsh, HarnessKind::Pi] {
            assert!(cleared.launch_for(&pinned_on(harness)).is_ok(), "{harness}");
        }
    }

    #[test]
    fn mcp_given_to_pi_or_dsh_is_refused_with_drop_mcp() {
        let given = run_args(&["tenaz", "run", "--mcp", "a=http://h/"]);
        for harness in [HarnessKind::Dsh, HarnessKind::Pi] {
            let why = given
                .launch_for(&crate::testing::ledger_for(harness))
                .unwrap_err();
            assert_eq!(
                why.to_string(),
                format!(
                    "{}; drop --mcp: a run keeps the harness init gave it",
                    McpServerError::NotPassed(harness)
                ),
                "{harness}"
            );
        }
    }

    #[test]
    fn pinned_servers_on_fx_are_kept() {
        assert!(
            run_args(&["tenaz", "run"])
                .launch_for(&pinned_on(HarnessKind::Fx))
                .is_ok()
        );
    }

    /// A server edited into `run.json` by hand is checked again at `run`.
    #[test]
    fn run_checks_the_pinned_servers_again() {
        let mut ledger = crate::testing::ledger_for(HarnessKind::Fx);
        ledger.launch.mcp_servers = servers(&[("media", "http://m/")]);
        let why = run_args(&["tenaz", "run"]).launch_for(&ledger).unwrap_err();
        assert_eq!(
            why.to_string(),
            format!(
                "{}; tenaz run --no-mcp drops the pinned servers",
                McpServerError::Reserved
            )
        );
    }

    /// The error names the pinned server, not `--mcp`, which was not given.
    #[test]
    fn a_bad_pinned_server_is_named_without_the_flag() {
        let mut ledger = crate::testing::ledger_for(HarnessKind::Fx);
        ledger.launch.mcp_servers = servers(&[("b", "ftp://x")]);
        let why = run_args(&["tenaz", "run"]).launch_for(&ledger).unwrap_err();
        assert_eq!(
            why.to_string(),
            "MCP server b needs an http:// or https:// URL with a host and no spaces, \
             got \"ftp://x\"; tenaz run --no-mcp drops the pinned servers"
        );
    }
}
