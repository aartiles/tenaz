//! `tenaz`: the single-model runner command line.

mod detect;
mod wiring;
mod wizard;

use std::collections::BTreeMap;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tenaz_core::ledger::{EngineApi, Launch, ModelConfig};
use tenaz_core::orchestrator::{InitOptions, RunOptions, Runner, Templates, init, status_text};
use tenaz_core::params::RunParams;
use tenaz_core::project::Project;
use tenaz_pi::models::ModelEntry;
use tokio_util::sync::CancellationToken;

/// The runner's version string, journaled per run.
fn runner_version() -> String {
    format!("tenaz {}", env!("CARGO_PKG_VERSION"))
}

/// The project directory as an absolute path: harnesses, `verify` and git
/// run inside it, where a path relative to the runner's directory is wrong.
fn absolute(project: PathBuf) -> PathBuf {
    std::path::absolute(&project).unwrap_or(project)
}

/// The role prompts and shared rules compiled into the binary.
fn templates() -> Templates {
    use tenaz_core::role::Role;
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
        protocol: include_str!("../templates/protocol.md").to_owned(),
        prompts: prompts
            .into_iter()
            .map(|(r, t)| (r, t.to_owned()))
            .collect(),
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "tenaz",
    version,
    about = "Tenaz, a single-model runner: one resident model, a goal, a verified result."
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
    /// Pi in RPC mode; the model's provider comes from ~/.pi/agent/models.json.
    Pi,
    /// The scripted fake harness: exercises the runner without a model.
    Fake,
}

fn harness_name(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::Fx => "fx",
        HarnessKind::Pi => "pi",
        HarnessKind::Fake => "fake",
    }
}

/// Which engine API loads and unloads the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum EngineKind {
    /// Sushi: `/v1/load-model` and `/v1/unload-model`.
    Sushi,
    /// Any OpenAI-compatible engine with start and stop commands.
    Generic,
}

impl EngineKind {
    fn api(self) -> EngineApi {
        match self {
            Self::Sushi => EngineApi::Sushi,
            Self::Generic => EngineApi::Generic,
        }
    }

    fn from_api(api: EngineApi) -> Self {
        match api {
            EngineApi::Sushi => Self::Sushi,
            EngineApi::Generic => Self::Generic,
        }
    }
}

/// The engine API when nothing answered to say which: Sushi on its port
/// (the default before engines were detected), generic elsewhere.
fn guess_api(base_url: Option<&str>) -> EngineApi {
    match base_url {
        None => EngineApi::Sushi,
        Some(url) if url.contains(":12345") => EngineApi::Sushi,
        Some(_) => EngineApi::Generic,
    }
}

/// `tenaz init`. Without `--model`, in a terminal, it asks for what is missing.
#[derive(clap::Args, Clone, Debug)]
struct InitArgs {
    /// The project directory (asked when missing).
    #[arg(long)]
    project: Option<PathBuf>,
    /// Copied to .tenaz/brief.md (or put the goal there first).
    #[arg(long)]
    brief: Option<PathBuf>,
    /// The coding agent (default: fx; the wizard offers what is installed).
    #[arg(long, value_enum)]
    harness: Option<HarnessKind>,
    /// The fx executable.
    #[arg(long)]
    fx_bin: Option<PathBuf>,
    /// The pi executable.
    #[arg(long)]
    pi_bin: Option<PathBuf>,
    /// The model id as the engine lists it; without it, the setup wizard runs.
    #[arg(long)]
    model: Option<String>,
    /// The provider name used in the harness configuration (with --harness pi,
    /// the provider in models.json).
    #[arg(long)]
    provider: Option<String>,
    /// The engine's OpenAI-compatible base URL (default: Sushi on 127.0.0.1:12345;
    /// with --harness pi, the provider's baseUrl in models.json).
    #[arg(long)]
    base_url: Option<String>,
    /// Default 131072; with --harness pi, the model's contextWindow in models.json.
    #[arg(long)]
    context_window: Option<u64>,
    /// The model reads images (with --harness pi, read from models.json when listed there).
    #[arg(long)]
    vision: bool,
    #[arg(long, default_value = "medium")]
    thinking: String,
    /// Parameter overrides, `key=value` (see `tenaz params`).
    #[arg(long = "set")]
    sets: Vec<String>,
    /// Skill directories to pin read-only into .tenaz/skills/.
    #[arg(long = "skill")]
    skills: Vec<PathBuf>,
    /// The engine API (default: detected from the engine at --base-url).
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
    /// Image workflows, `name=path.json` (API-format workflow with {{prompt}} placeholders).
    #[arg(long = "workflow")]
    workflows: Vec<String>,
    /// Media job time limit, in seconds (default 1800).
    #[arg(long)]
    media_timeout: Option<u64>,
    /// Ask for what is missing even when stdin is not a terminal.
    #[arg(long)]
    interactive: bool,
}

impl InitArgs {
    /// The `tenaz init` command line that gives these answers without questions.
    fn command_line(&self) -> String {
        let mut words = vec!["tenaz".to_owned(), "init".to_owned()];
        let mut flag = |name: &str, value: Option<String>| {
            if let Some(value) = value {
                words.push(format!("--{name}"));
                words.push(shell_quote(&value));
            }
        };
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        flag("project", path(&self.project));
        flag("brief", path(&self.brief));
        flag("harness", self.harness.map(|h| harness_name(h).to_owned()));
        flag("fx-bin", path(&self.fx_bin));
        flag("pi-bin", path(&self.pi_bin));
        flag("model", self.model.clone());
        flag("provider", self.provider.clone());
        flag("base-url", self.base_url.clone());
        flag("context-window", self.context_window.map(|c| c.to_string()));
        flag(
            "thinking",
            (self.thinking != "medium").then(|| self.thinking.clone()),
        );
        for pair in &self.sets {
            flag("set", Some(pair.clone()));
        }
        for skill in &self.skills {
            flag("skill", Some(skill.display().to_string()));
        }
        flag(
            "engine",
            self.engine.map(|e| {
                match e {
                    EngineKind::Sushi => "sushi",
                    EngineKind::Generic => "generic",
                }
                .to_owned()
            }),
        );
        flag("engine-start", self.engine_start.clone());
        flag("engine-stop", self.engine_stop.clone());
        flag("comfyui", self.comfyui.clone());
        for workflow in &self.workflows {
            flag("workflow", Some(workflow.clone()));
        }
        flag("media-timeout", self.media_timeout.map(|t| t.to_string()));
        if self.vision {
            words.push("--vision".to_owned());
        }
        if self.no_proxy {
            words.push("--no-proxy".to_owned());
        }
        words.join(" ")
    }
}

/// A word as a POSIX shell reads it back.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// Everything `init` needs, from flags or from the wizard.
struct InitPlan {
    project: PathBuf,
    brief: Option<PathBuf>,
    harness: HarnessKind,
    harness_bin: Option<PathBuf>,
    model: String,
    provider: String,
    base_url: Option<String>,
    context_window: Option<u64>,
    vision: bool,
    api: EngineApi,
    engine_start: Option<String>,
    engine_stop: Option<String>,
    comfyui: Option<String>,
    workflows: Vec<String>,
    /// For what neither path changes: thinking, params, skills, proxy, timeout.
    args: InitArgs,
}

/// The plan from flags alone. The engine API is asked of the engine when
/// `--engine` does not say.
async fn plan_from_flags(args: InitArgs) -> anyhow::Result<InitPlan> {
    let Some(model) = args.model.clone() else {
        anyhow::bail!(
            "--model is required when stdin is not a terminal; run tenaz init in a terminal for the setup wizard"
        );
    };
    let Some(project) = args.project.clone() else {
        anyhow::bail!("--project is required");
    };
    let harness = args.harness.unwrap_or(HarnessKind::Fx);
    let detected = match &args.base_url {
        Some(url) if args.engine.is_none() || args.provider.is_none() => {
            detect::engine_at(&detect::client(), url).await
        }
        _ => None,
    };
    let api = args.engine.map_or_else(
        || {
            detected
                .as_ref()
                .map_or_else(|| guess_api(args.base_url.as_deref()), |e| e.api)
        },
        EngineKind::api,
    );
    let provider =
        args.provider
            .clone()
            .unwrap_or_else(|| match (&detected, args.base_url.as_deref()) {
                _ if harness == HarnessKind::Pi => "sushi".to_owned(),
                (Some(engine), _) => engine.provider.clone(),
                (None, Some(url)) => detect::provider_by_port(url).unwrap_or("local").to_owned(),
                (None, None) => "sushi".to_owned(),
            });
    Ok(InitPlan {
        project,
        brief: args.brief.clone(),
        harness,
        harness_bin: match harness {
            HarnessKind::Fx => args.fx_bin.clone(),
            HarnessKind::Pi => args.pi_bin.clone(),
            HarnessKind::Fake => None,
        },
        model,
        provider,
        base_url: args.base_url.clone(),
        context_window: args.context_window,
        vision: args.vision,
        api,
        engine_start: args.engine_start.clone(),
        engine_stop: args.engine_stop.clone(),
        comfyui: args.comfyui.clone(),
        workflows: args.workflows.clone(),
        args,
    })
}

/// A path to keep in `run.json`: a bare command name stays one (found on
/// `PATH` at run time), anything else is made absolute.
fn pinned_path(path: &Path) -> String {
    if path.components().count() == 1 && !path.as_os_str().is_empty() {
        path.display().to_string()
    } else {
        absolute(path.to_path_buf()).display().to_string()
    }
}

/// `name=path` pairs, checked to load, as absolute paths by name.
fn pinned_workflows(specs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    wiring::parse_workflows(specs)?;
    specs
        .iter()
        .map(|spec| {
            let (name, path) = spec
                .split_once('=')
                .with_context(|| format!("--workflow expects name=path, got {spec}"))?;
            Ok((name.to_owned(), pinned_path(Path::new(path))))
        })
        .collect()
}

fn execute_init(plan: InitPlan) -> anyhow::Result<()> {
    let project = Project::new(absolute(plan.project));
    let mut params = RunParams::default();
    for pair in &plan.args.sets {
        params.apply_override(pair)?;
    }
    let (harness_name, harness_version, entry) = pin_harness(
        plan.harness,
        plan.harness_bin.as_deref(),
        &plan.provider,
        &plan.model,
    )?;
    let (input, base_url, context_window) = resolve_model(
        entry.as_ref(),
        plan.vision,
        plan.base_url,
        plan.context_window,
    );
    let config = ModelConfig {
        harness: harness_name,
        harness_version,
        model: format!("{}/{}", plan.provider, plan.model),
        provider: Some(plan.provider),
        id: plan.model,
        base_url: Some(base_url),
        context_window,
        input,
        thinking: Some(plan.args.thinking.clone()),
    };
    let launch = Launch {
        engine: plan.api,
        engine_start: plan.engine_start,
        engine_stop: plan.engine_stop,
        no_proxy: plan.args.no_proxy,
        harness_bin: plan.harness_bin.as_deref().map(pinned_path),
        fx_args: None,
        comfyui: plan.comfyui,
        workflows: pinned_workflows(&plan.workflows)?,
        media_timeout_sec: plan.args.media_timeout,
    };
    let vcs = tenaz_adapters::git::GitRepo::new(project.root());
    let clock = tenaz_adapters::clock::SystemClock::new();
    let options = InitOptions {
        brief: plan.brief,
        config,
        launch,
        params,
        skills: plan.args.skills,
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
    println!(
        "Next: tenaz run --project {}",
        shell_quote(&project.root().display().to_string())
    );
    Ok(())
}

/// `tenaz run`. Every launch flag is optional: without it the value pinned
/// in run.json is used; with it, the new value is used and saved there.
#[derive(clap::Args, Debug)]
struct RunArgs {
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
    #[arg(long)]
    fx_bin: Option<PathBuf>,
    /// Arguments that put fx in ACP mode (default: acp).
    #[arg(long, value_delimiter = ' ')]
    fx_args: Option<Vec<String>>,
    #[arg(long)]
    pi_bin: Option<PathBuf>,
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
    /// Media job time limit, in seconds.
    #[arg(long)]
    media_timeout: Option<u64>,
}

impl RunArgs {
    /// `launch` with this command line's flags applied.
    fn apply(&self, launch: &Launch, harness: &str) -> anyhow::Result<Launch> {
        let mut next = launch.clone();
        if let Some(engine) = self.engine {
            next.engine = engine.api();
        }
        if self.engine_start.is_some() {
            next.engine_start.clone_from(&self.engine_start);
        }
        if self.engine_stop.is_some() {
            next.engine_stop.clone_from(&self.engine_stop);
        }
        next.no_proxy |= self.no_proxy;
        let bin = match harness {
            "fx" => self.fx_bin.as_deref(),
            "pi" => self.pi_bin.as_deref(),
            _ => None,
        };
        if let Some(bin) = bin {
            next.harness_bin = Some(pinned_path(bin));
        }
        if self.fx_args.is_some() {
            next.fx_args.clone_from(&self.fx_args);
        }
        if self.comfyui.is_some() {
            next.comfyui.clone_from(&self.comfyui);
        }
        if !self.workflows.is_empty() {
            next.workflows = pinned_workflows(&self.workflows)?;
        }
        if self.media_timeout.is_some() {
            next.media_timeout_sec = self.media_timeout;
        }
        Ok(next)
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// A new project from a brief; the model, harness and how to launch them
    /// are pinned in .tenaz/run.json. Without --model, in a terminal, a setup
    /// wizard asks for them.
    Init(InitArgs),
    /// Run (resumable). Ctrl-C pauses; run again to continue.
    Run(RunArgs),
    /// Where the run is (reads files only).
    Status {
        #[arg(long, default_value = ".")]
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
        Command::Init(args) => {
            let ask = args.model.is_none() && (args.interactive || std::io::stdin().is_terminal());
            let plan = if ask {
                let stdin = std::io::stdin();
                let mut prompter = wizard::Prompter::new(stdin.lock(), std::io::stderr());
                wizard::run(&args, &mut prompter).await?
            } else {
                plan_from_flags(args).await?
            };
            execute_init(plan)
        }
        Command::Run(args) => {
            let project = Project::new(absolute(args.project.clone()));
            if let Some(note) = project.legacy_layout_note() {
                anyhow::bail!(note);
            }
            if !project.exists(".tenaz/run.json") {
                anyhow::bail!(
                    "{} has no .tenaz/run.json; run tenaz init first",
                    project.root().display()
                );
            }
            let mut ledger = project.load_ledger()?;
            let launch = args.apply(&ledger.launch, &ledger.config.harness)?;
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
                timeout: std::time::Duration::from_secs(launch.media_timeout_sec.unwrap_or(1800)),
            };
            let engine = wiring::EngineOptions {
                kind: match launch.engine {
                    EngineApi::Sushi => wiring::EngineChoice::Sushi,
                    EngineApi::Generic => wiring::EngineChoice::Generic {
                        start: launch.engine_start.clone(),
                        stop: launch.engine_stop.clone(),
                    },
                },
                proxy: !launch.no_proxy,
            };
            let bin = |name: &str| {
                PathBuf::from(
                    launch
                        .harness_bin
                        .clone()
                        .filter(|_| ledger.config.harness == name)
                        .unwrap_or_else(|| name.to_owned()),
                )
            };
            let harness = wiring::HarnessOptions {
                fx_bin: bin("fx"),
                fx_args: launch
                    .fx_args
                    .clone()
                    .unwrap_or_else(|| vec!["acp".to_owned()]),
                pi_bin: bin("pi"),
            };
            let wired = wiring::wire(&project, &engine, &harness, &media).await?;
            let mut runner = Runner::open(project, wired.deps, stop)?;
            let options = RunOptions {
                resume: args.resume,
                allow_weak_engine: args.allow_weak_engine,
                refresh_templates: args.refresh_templates.then(templates),
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

/// The harness name and version to pin and, with Pi, the model's entry in
/// `~/.pi/agent/models.json`.
fn pin_harness(
    harness: HarnessKind,
    bin: Option<&Path>,
    provider: &str,
    model: &str,
) -> anyhow::Result<(String, String, Option<ModelEntry>)> {
    let name = harness_name(harness);
    let bin = bin.unwrap_or_else(|| Path::new(name));
    Ok(match harness {
        HarnessKind::Fx => (
            name.to_owned(),
            tenaz_acp::fx::fx_version(bin)
                .with_context(|| format!("cannot run {} --version", bin.display()))?,
            None,
        ),
        HarnessKind::Pi => {
            let entry = match tenaz_pi::models::default_models_path() {
                Some(path) => tenaz_pi::models::model_entry(&path, provider, model)?,
                None => None,
            };
            (
                name.to_owned(),
                tenaz_pi::pi_version(bin)
                    .with_context(|| format!("cannot run {} --version", bin.display()))?,
                entry,
            )
        }
        HarnessKind::Fake => (
            name.to_owned(),
            tenaz_core::testing::ScriptedHarness::default().version(),
            None,
        ),
    })
}

/// Inputs, base URL and context window: the command line first, then the
/// models.json entry, then the defaults.
fn resolve_model(
    entry: Option<&ModelEntry>,
    vision: bool,
    base_url: Option<String>,
    context_window: Option<u64>,
) -> (Vec<String>, String, u64) {
    let input = match entry {
        Some(entry) if !vision => entry.input.clone(),
        _ => {
            let mut input = vec!["text".to_owned()];
            if vision {
                input.push("image".to_owned());
            }
            input
        }
    };
    let base_url = base_url
        .or_else(|| entry.and_then(|e| e.base_url.clone()))
        .unwrap_or_else(|| "http://127.0.0.1:12345/v1".to_owned());
    let context_window = context_window
        .or_else(|| entry.and_then(|e| e.context_window))
        .unwrap_or(131_072);
    (input, base_url, context_window)
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
