//! `tenaz`: the single-model runner command line.

mod detect;
mod watch;
mod wiring;
mod wizard;
mod worktree;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tenaz_core::ledger::Phase;
use tenaz_core::ledger::{EngineApi, HarnessKind, Launch, ModelConfig};
use tenaz_core::orchestrator::{
    InitOptions, RunOptions, Runner, Templates, init, status_json, status_text,
};
use tenaz_core::params::RunParams;
use tenaz_core::project::Project;
use tenaz_pi::models::ModelEntry;
use tokio_util::sync::CancellationToken;

/// The version and, for a build from a git checkout, its commit
/// (`0.1.0 (816e1d8)`), set by `build.rs`; the commit tells builds of one
/// version apart.
const VERSION: &str = env!("TENAZ_VERSION");

/// The runner's version string, journaled per run.
fn runner_version() -> String {
    format!("tenaz {VERSION}")
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

/// How the commands fit together, after `tenaz --help`.
const WORKFLOW_HELP: &str = "\
Workflow:
  1. Start the engine with the model loaded. tenaz doctor checks that git, an
     agent, the engine and the model are ready, and says how to fix what is not.
  2. tenaz init       writes the brief and pins the setup. In a repository with
                      commits it works on a new branch in a worktree next to it
                      and prints that directory; use it as --project from here on.
  3. tenaz run        works until the goal is met or a task blocks. It can take
                      hours: run it in the background and poll tenaz status.
                      Its exit code says how it ended (tenaz run --help).
  4. tenaz status     shows the phase; safe to call while run works. --json
                      prints the same as one JSON object.
  5. tenaz finish     puts the result in one commit on a new branch of the
                      original repository. Nothing is pushed.

To see a whole run first, without a model or an agent: tenaz demo.

Agents delegating a goal: tenaz guide prints a skill with when to delegate,
how to write the brief and how to check the result.

Without questions (scripts and agents), with llama.cpp on its default port:
  tenaz init --project my-goal --brief brief.md --model my-model \\
    --base-url http://127.0.0.1:8080/v1 --context-window 65536
  tenaz run --project my-goal
  tenaz status --project my-goal --json
  tenaz finish --project my-goal

Without --model, in a terminal, init asks for everything instead.";

/// How a run ends, after `tenaz run --help`.
const RUN_HELP: &str = "\
Exit codes (also when the run had already ended before this call):
  0  COMPLETE: the goal is met; tenaz finish takes the result.
  1  error: a precondition (no project, another tenaz run already working on
     it, failed engine preflight, a changed brief or harness version) or a
     failure mid-run, also saved as last_error in .tenaz/run.json. The
     message says what failed; run again once fixed.
  2  the command line is wrong.
  3  COMPLETE_WITH_GAPS: the final review cycles ran out with work missing.
  4  BLOCKED: tenaz status gives the reason. Fix it, then tenaz run --resume.
  5  paused after Ctrl-C or SIGTERM. Run again, without --resume.";

/// What the phases mean, after `tenaz status --help`.
const WATCH_HELP: &str = "\
Levels:
  summary   the run log: sessions starting and ending, steers, tasks done.
  actions   also every tool call and model reply on one line, with the context
            each turn used (ctx), its output and thinking, and decode speed.
  full      also replies in full and the start of every tool result (the
            stream keeps 200 characters; shell results show their exit code).
  debug     also the harness's own log lines (fx: <label>.fx-acp.log).

Example:
  tenaz watch --project ./my-app --level full

Everything shown is in .tenaz/private/: orchestrator.log and, per session,
<label>.rpc.jsonl (the events; Pi's own records with Pi), <label>.prompt.md, <label>.thinking.md (the
reasoning, with fx) and the harness logs.
";

const STATUS_HELP: &str = "\
Phases:
  BOOTSTRAP            planning: the spec, the verify script and the task cards.
  LOOP                 building tasks one at a time, each verified and reviewed.
  FINAL                the final review of the whole goal.
  COMPLETE             the final review accepted the result and verify passes.
  COMPLETE_WITH_GAPS   the final review cycles ran out with work still missing.
  BLOCKED              the run stopped; the reason follows on the same line.

States (the State line):
  running              a tenaz run is working on it now (its pid follows).
  paused               stopped with Ctrl-C or SIGTERM; run again to continue.
  interrupted          the last run ended without pausing: killed, or failed
                       with an error (see last_error); run again to continue.
  blocked, complete, complete_with_gaps   as the phase of that name.

The details are in .tenaz/: run.json, journal.jsonl, the task cards in tasks/
and the session notes in notes/.

--json fields:
  initialized      false when there is no run here; runs_elsewhere then lists
                   the runs in worktrees of this repository.
  phase            one of the phases above.
  state            one of the states above.
  pid              the process id of the run working on it, or null.
  blocked_reason, last_error, paused_at, ended_at   null when not set.
  tasks            the number of task cards per status (todo, active, done,
                   blocked, split, dropped).
  current          the task being built and its failed attempts, or null.
  inflight         the session in progress, or null.
  sessions, sessions_failed, model, harness
  origin           the repository, branches and worktree of a run on an
                   existing repository, or null.";

#[derive(Parser, Debug)]
#[command(
    name = "tenaz",
    version = VERSION,
    about = "Tenaz, a single-model runner: one resident model, a goal, a verified result.",
    after_help = WORKFLOW_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Which harness runs the sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum HarnessArg {
    /// fx over ACP (default).
    Fx,
    /// Pi in RPC mode, with a models.json Tenaz writes for the run.
    Pi,
    /// The scripted fake harness: exercises the runner without a model.
    Fake,
}

impl HarnessArg {
    fn kind(self) -> HarnessKind {
        match self {
            Self::Fx => HarnessKind::Fx,
            Self::Pi => HarnessKind::Pi,
            Self::Fake => HarnessKind::Fake,
        }
    }
}

/// Which engine API loads and unloads the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum EngineKind {
    /// `/v1/load-model` and `/v1/unload-model` (Sushi, mlx-serve).
    #[value(name = "load-api", alias = "sushi")]
    LoadApi,
    /// Any OpenAI-compatible engine with start and stop commands.
    Generic,
}

impl EngineKind {
    fn api(self) -> EngineApi {
        match self {
            Self::LoadApi => EngineApi::LoadApi,
            Self::Generic => EngineApi::Generic,
        }
    }

    fn from_api(api: EngineApi) -> Self {
        match api {
            EngineApi::LoadApi => Self::LoadApi,
            EngineApi::Generic => Self::Generic,
        }
    }
}

/// The engine API when nothing answered to say which: Sushi on its port
/// (the default before engines were detected), generic elsewhere.
fn guess_api(base_url: Option<&str>) -> EngineApi {
    match base_url {
        None => EngineApi::LoadApi,
        Some(url) if url.contains(":12345") => EngineApi::LoadApi,
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
    harness: Option<HarnessArg>,
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
    /// Base URL of an `OpenAI` Images API (`http://host:port/v1`); enables the
    /// image tool instead of `ComfyUI`.
    #[arg(long)]
    images: Option<String>,
    /// The image model to ask the `--images` API for.
    #[arg(long)]
    image_model: Option<String>,
    /// Media job time limit, in seconds (default 1800).
    #[arg(long)]
    media_timeout: Option<u64>,
    /// Ask for what is missing even when stdin is not a terminal.
    #[arg(long)]
    interactive: bool,
    /// In a repository with commits, work on its current branch, in this
    /// checkout, instead of on a new branch in a worktree; uncommitted
    /// changes go into Tenaz's first commit.
    #[arg(long)]
    in_place: bool,
    /// The worktree directory (default: ../<repo>-tenaz-<slug>).
    #[arg(long, conflicts_with = "in_place")]
    worktree: Option<PathBuf>,
    /// The slug of the run branch tenaz/<slug> (default: from the goal).
    #[arg(long, conflicts_with = "in_place")]
    name: Option<String>,
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
        flag("harness", self.harness.map(|h| h.kind().to_string()));
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
                    EngineKind::LoadApi => "load-api",
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
        flag("images", self.images.clone());
        flag("image-model", self.image_model.clone());
        flag("media-timeout", self.media_timeout.map(|t| t.to_string()));
        if self.vision {
            words.push("--vision".to_owned());
        }
        if self.no_proxy {
            words.push("--no-proxy".to_owned());
        }
        if self.in_place {
            words.push("--in-place".to_owned());
        }
        let mut flag = |name: &str, value: Option<String>| {
            if let Some(value) = value {
                words.push(format!("--{name}"));
                words.push(shell_quote(&value));
            }
        };
        flag("worktree", path(&self.worktree));
        flag("name", self.name.clone());
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
    harness: HarnessArg,
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
    images: Option<String>,
    image_model: Option<String>,
    /// The goal typed in the wizard, written as the brief.
    brief_text: Option<String>,
    /// For what neither path changes: thinking, params, skills, proxy,
    /// timeout, where the run goes.
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
    let harness = args.harness.unwrap_or(HarnessArg::Fx);
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
                (Some(engine), _) => engine.provider.clone(),
                (None, Some(url)) => detect::provider_by_port(url).unwrap_or("local").to_owned(),
                (None, None) => "sushi".to_owned(),
            });
    Ok(InitPlan {
        project,
        brief: args.brief.clone(),
        harness,
        harness_bin: match harness {
            HarnessArg::Fx => args.fx_bin.clone(),
            HarnessArg::Pi => args.pi_bin.clone(),
            HarnessArg::Fake => None,
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
        images: args.images.clone(),
        image_model: args.image_model.clone(),
        brief_text: None,
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

/// The brief's text and the file `init` copies it from: `--brief`, the
/// wizard's goal (through a scratch file), or none (the brief in place).
fn brief_source(plan: &InitPlan) -> anyhow::Result<(Option<String>, Option<PathBuf>)> {
    if let Some(path) = &plan.brief {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the brief {}", path.display()))?;
        return Ok((Some(text), Some(path.clone())));
    }
    if let Some(goal) = &plan.brief_text {
        let text = format!("# Goal\n\n{goal}\n");
        let path = std::env::temp_dir().join(format!("tenaz-brief-{}.md", std::process::id()));
        std::fs::write(&path, &text)?;
        return Ok((Some(text), Some(path)));
    }
    Ok((None, None))
}

fn execute_init(plan: InitPlan) -> anyhow::Result<()> {
    let given = absolute(plan.project.clone());
    let (brief_text, brief) = brief_source(&plan)?;
    let request = worktree::PlaceRequest {
        in_place: plan.args.in_place,
        worktree: plan.args.worktree.clone().map(absolute),
        name: plan.args.name.clone(),
    };
    let placement = worktree::place(&given, &request, brief_text.as_deref(), brief.as_deref())?;
    let scratch = plan.brief.is_none().then(|| brief.clone()).flatten();
    let result = init_at(plan, &given, &placement, brief);
    if let Some(path) = scratch {
        let _ = std::fs::remove_file(path);
    }
    if result.is_err()
        && let worktree::Placement::Worktree(wt) = &placement
    {
        wt.discard();
    }
    result
}

fn init_at(
    plan: InitPlan,
    given: &Path,
    placement: &worktree::Placement,
    brief: Option<PathBuf>,
) -> anyhow::Result<()> {
    let project = Project::new(placement.project_dir(given));
    let mut params = RunParams::default();
    for pair in &plan.args.sets {
        params.apply_override(pair)?;
    }
    let (harness_version, entry) = pin_harness(
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
        harness: plan.harness.kind(),
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
        images_url: plan.images,
        image_model: plan.image_model,
        media_timeout_sec: plan.args.media_timeout,
    };
    let brief = match placement {
        worktree::Placement::Worktree(wt) => brief.or_else(|| wt.untracked_brief.clone()),
        _ => brief,
    };
    match placement {
        worktree::Placement::Worktree(wt) => {
            wt.create()?;
            eprintln!(
                "Created branch {} in the worktree {} (from {}); your checkout stays as it is.",
                wt.branch,
                wt.path.display(),
                wt.base_branch.as_deref().unwrap_or(&wt.base_commit)
            );
        }
        worktree::Placement::InPlace { origin } => eprintln!(
            "--in-place: Tenaz commits on {} in {}, starting with everything uncommitted there.",
            origin.branch.as_deref().unwrap_or("the detached HEAD"),
            origin.repo
        ),
        worktree::Placement::Fresh => {}
    }
    let vcs = tenaz_adapters::git::GitRepo::new(project.root());
    let clock = tenaz_adapters::clock::SystemClock::new();
    let options = InitOptions {
        brief,
        config,
        launch,
        params,
        skills: plan.args.skills,
        runner_version: runner_version(),
    };
    let mut ledger = init(&project, &vcs, &clock, &templates(), options)?;
    let origin = match placement {
        worktree::Placement::Worktree(wt) => Some(wt.origin()),
        worktree::Placement::InPlace { origin } => Some(origin.clone()),
        worktree::Placement::Fresh => None,
    };
    if let Some(mut origin) = origin {
        origin.init_commit.clone_from(&ledger.checkpoint);
        ledger.origin = Some(origin);
        project.save_ledger(&ledger)?;
    }
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
        if self.engine_start.is_some() {
            next.engine_start.clone_from(&self.engine_start);
        }
        if self.engine_stop.is_some() {
            next.engine_stop.clone_from(&self.engine_stop);
        }
        next.no_proxy |= self.no_proxy;
        let bin = match harness {
            HarnessKind::Fx => self.fx_bin.as_deref(),
            HarnessKind::Pi => self.pi_bin.as_deref(),
            HarnessKind::Fake => None,
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
        if self.images.is_some() {
            next.images_url.clone_from(&self.images);
        }
        if self.image_model.is_some() {
            next.image_model.clone_from(&self.image_model);
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
    Init(Box<InitArgs>),
    /// Work on the goal until it is met or a task blocks; can take hours.
    /// Ctrl-C pauses; run again to continue.
    #[command(after_help = RUN_HELP)]
    Run(RunArgs),
    /// Where the run is: phase, tasks, current task (reads files only, safe
    /// while a run works).
    #[command(after_help = STATUS_HELP)]
    Status {
        /// Print one JSON object instead of text (fields described below).
        #[arg(long)]
        json: bool,
        /// The run's directory: the one init printed (a worktree for an existing repository).
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    /// Put the result on a new branch of the original repository, in one
    /// commit. Nothing is pushed.
    ///
    /// The commit sits on top of the commit the run started from, with the project's changes
    /// and none of Tenaz's files (.tenaz/; for runs from older layouts also
    /// the root BRIEF.md, AGENTS.md and .gitignore lines its first commit
    /// added). One commit rather than the
    /// run's series because that series interleaves bookkeeping commits and
    /// steps that later ones rework; the run branch (tenaz/<slug>) keeps it
    /// all.
    Finish(FinishArgs),
    /// Follow what the run is doing, session by session, as it happens
    /// (reads files only, safe while a run works). Ctrl-C stops watching,
    /// not the run.
    #[command(after_help = WATCH_HELP)]
    Watch {
        /// How much to show; each level adds to the one before.
        #[arg(long, short, value_enum, default_value = "actions")]
        level: watch::Level,
        /// Lines of the current session shown before following.
        #[arg(long, default_value_t = 30)]
        history: usize,
        /// Print what is there and exit instead of following.
        #[arg(long)]
        once: bool,
        /// The run's directory: the one init printed (a worktree for an existing repository).
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    /// Check what a real run needs on this machine (git, a coding agent, an
    /// engine and a model with enough context) and say how to install or
    /// fix what is missing. Exits with 0 when ready, 1 when something is
    /// missing.
    Doctor {
        /// Also look for an engine at this address (default: the usual ports).
        #[arg(long)]
        base_url: Option<String>,
    },
    /// See a whole run without a model or an agent: the scripted harness
    /// builds a small goal in a new directory, with engine failures,
    /// watchdog stops and a review rejection on the way.
    Demo {
        /// The directory to create (it must not exist or be empty).
        #[arg(long, default_value = "tenaz-demo")]
        project: PathBuf,
    },
    /// The tunable parameters and their defaults.
    Params,
    /// Print the agent skill for delegating goals to Tenaz (a SKILL.md).
    ///
    /// It matches this binary's version. To install it for Claude Code:
    /// mkdir -p ~/.claude/skills/tenaz && tenaz guide > ~/.claude/skills/tenaz/SKILL.md
    Guide,
    /// Runs fx with the given arguments and passes its stdout through,
    /// putting right what it says outside the ACP protocol. Used by `run`.
    #[command(name = "fx-shim", hide = true)]
    FxShim {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Runs a program in the OS sandbox: it may write only below the
    /// `--write` directories, never to the `--deny-write` paths, and may not
    /// read the `--deny-read` ones. Used by `run` when `sandbox` is on.
    #[command(name = "sandbox-exec", hide = true)]
    SandboxExec(SandboxExecArgs),
}

#[derive(clap::Args, Debug)]
struct SandboxExecArgs {
    #[arg(long)]
    write: Vec<PathBuf>,
    #[arg(long)]
    deny_write: Vec<PathBuf>,
    #[arg(long)]
    deny_read: Vec<PathBuf>,
    #[arg(last = true, required = true)]
    command: Vec<std::ffi::OsString>,
}

/// `tenaz fx-shim <fx> <args>`: fx with stdin and stderr inherited and its
/// stdout normalized line by line; exits with fx's code.
fn fx_shim(command: &[String]) -> anyhow::Result<ExitCode> {
    use std::io::{BufRead as _, Write as _};
    let (program, args) = command
        .split_first()
        .context("fx-shim needs the fx binary")?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start {program}"))?;
    let stdout = child.stdout.take().context("no stdout from fx")?;
    let mut out = std::io::stdout().lock();
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line?;
        writeln!(out, "{}", tenaz_acp::fx::normalize_line(&line))?;
        out.flush()?;
    }
    let status = child.wait()?;
    Ok(ExitCode::from(
        u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
    ))
}

/// `tenaz finish`.
#[derive(clap::Args, Debug)]
struct FinishArgs {
    /// The run's directory: the one init printed (a worktree for an existing repository).
    #[arg(long, default_value = ".")]
    project: PathBuf,
    /// The branch to create (default: the run branch's slug, without tenaz/).
    #[arg(long)]
    branch: Option<String>,
    /// Move the branch when it already exists.
    #[arg(long)]
    force: bool,
    /// Then remove the run's worktree. Its run.json and journal are not
    /// committed and go with it; the committed history stays on the run
    /// branch.
    #[arg(long)]
    remove_worktree: bool,
    /// With --remove-worktree, delete the run branch too.
    #[arg(long, requires = "remove_worktree")]
    delete_run_branch: bool,
}

/// The project at `dir` or, when `dir` has none, in the only one of its
/// subfolders that has one; else an error that points at the runs in
/// worktrees of the repository there.
fn initialized(dir: PathBuf) -> anyhow::Result<Project> {
    let mut project = Project::new(absolute(dir));
    if let Some(note) = project.legacy_layout_note() {
        anyhow::bail!(note);
    }
    if !project.exists(".tenaz/run.json") {
        let runs = runs_below(project.root());
        if let [only] = runs.as_slice() {
            eprintln!("Using the project in {}", only.display());
            project = Project::new(only.clone());
        } else if !runs.is_empty() {
            let mut message = format!(
                "{} has no .tenaz/run.json, but these subfolders do; pick one with --project:",
                project.root().display()
            );
            for run in &runs {
                let _ = write!(message, "\n  {}", run.display());
            }
            anyhow::bail!(message);
        }
    }
    if !project.exists(".tenaz/run.json") {
        let mut message = format!(
            "{} has no .tenaz/run.json; run tenaz init first",
            project.root().display()
        );
        for line in worktree::runs_elsewhere(project.root()) {
            let _ = write!(message, "\n{line}");
        }
        anyhow::bail!(message);
    }
    Ok(project)
}

/// The direct subfolders of `dir` that hold a Tenaz run, sorted.
fn runs_below(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut runs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join(".tenaz/run.json").is_file())
        .collect();
    runs.sort();
    runs
}

fn execute_finish(args: FinishArgs) -> anyhow::Result<()> {
    let project = initialized(args.project)?;
    let request = worktree::FinishRequest {
        branch: args.branch,
        force: args.force,
        remove_worktree: args.remove_worktree,
        delete_run_branch: args.delete_run_branch,
    };
    let ledger = project.load_ledger()?;
    let done = worktree::finish(&project, &request)?;
    println!(
        "Created branch {} ({}): one commit on {}.",
        done.branch,
        &done.commit[..done.commit.len().min(7)],
        done.base.as_deref().map_or_else(
            || "no parent".to_owned(),
            |b| b[..b.len().min(7)].to_owned()
        )
    );
    if !done.stat.trim().is_empty() {
        print!("{}", done.stat);
    }
    for note in &done.notes {
        println!("{note}");
    }
    if let Some(origin) = &ledger.origin {
        if let Some(branch) = &origin.branch {
            println!("The run's full history stays on {branch}.");
        }
        if let Some(path) = &origin.worktree
            && !request.remove_worktree
        {
            println!(
                "To remove the worktree: git -C {} worktree remove {}",
                shell_quote(&origin.repo),
                shell_quote(path)
            );
        }
    }
    println!("Nothing was pushed: git show {} to review it.", done.branch);
    Ok(())
}

/// `tenaz sandbox-exec`: becomes the command under the policy; returns
/// only when that fails, with the shell's "cannot execute" code.
fn sandbox_exec(args: SandboxExecArgs) -> ExitCode {
    let SandboxExecArgs {
        write,
        deny_write,
        deny_read,
        command,
    } = args;
    let policy = tenaz_adapters::sandbox::Policy {
        write,
        deny_write,
        deny_read,
    };
    let error = policy.exec(&command);
    eprintln!(
        "tenaz sandbox-exec: cannot run {}: {error}",
        command
            .first()
            .map(|c| c.to_string_lossy())
            .unwrap_or_default()
    );
    ExitCode::from(126)
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
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
                plan_from_flags(*args).await?
            };
            execute_init(plan)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Run(args) => run(args).await,
        Command::Status { project, json } => {
            let project = Project::new(project);
            if json {
                let mut value = status_json(&project);
                if value["initialized"] == false {
                    value["runs_elsewhere"] = worktree::runs_elsewhere(project.root()).into();
                }
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(ExitCode::SUCCESS);
            }
            print!("{}", status_text(&project));
            match project.load_ledger() {
                Ok(ledger) => {
                    if let Some(origin) = &ledger.origin {
                        println!("{}", worktree::describe(origin));
                    }
                }
                Err(_) => {
                    for line in worktree::runs_elsewhere(project.root()) {
                        println!("{line}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Finish(args) => {
            execute_finish(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Watch {
            level,
            history,
            once,
            project,
        } => {
            let project = initialized(project)?;
            watch::watch(
                &project,
                &watch::WatchOptions {
                    level,
                    history,
                    once,
                },
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::FxShim { command } => fx_shim(&command),
        Command::SandboxExec(args) => Ok(sandbox_exec(args)),
        Command::Guide => {
            print!("{}", include_str!("../../../skills/tenaz/SKILL.md"));
            Ok(ExitCode::SUCCESS)
        }
        Command::Demo { project } => demo(project).await,
        Command::Doctor { base_url } => Ok(doctor(base_url.as_deref()).await),
        Command::Params => {
            let json = serde_json::to_value(RunParams::default())?;
            if let Some(map) = json.as_object() {
                for (key, value) in map {
                    println!("{key}={value}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `tenaz run`: works on the goal until it ends, pauses or fails.
async fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
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

/// The goal `tenaz demo` gives the scripted harness.
const DEMO_GOAL: &str = "Build four trivial files.";

/// `tenaz demo`: a whole run with the scripted harness, which needs no
/// agent and no engine, in a new directory; then what to look at.
async fn demo(project: PathBuf) -> anyhow::Result<ExitCode> {
    if std::fs::read_dir(&project).is_ok_and(|mut entries| entries.next().is_some()) {
        anyhow::bail!(
            "{} already exists and is not empty; remove it or pass another --project",
            project.display()
        );
    }
    let dir = project.display().to_string();
    eprintln!("A scripted run in {dir}: no model and no agent needed. It takes about a minute.\n");
    let init_line = [
        "tenaz",
        "init",
        "--project",
        &dir,
        "--harness",
        "fake",
        "--model",
        "fake",
        "--provider",
        "fake",
        "--set",
        "engineRetryBaseSec=0",
        "--set",
        "capacityRetryWaitSec=0",
        "--set",
        "notify=false",
    ];
    let Command::Init(args) = Cli::try_parse_from(init_line)?.command else {
        anyhow::bail!("the demo's init line did not parse as init");
    };
    let mut plan = plan_from_flags(*args).await?;
    plan.brief_text = Some(DEMO_GOAL.to_owned());
    execute_init(plan)?;
    let Command::Run(args) = Cli::try_parse_from(["tenaz", "run", "--project", &dir])?.command
    else {
        anyhow::bail!("the demo's run line did not parse as run");
    };
    let code = run(args).await?;
    let look = [
        (format!("tenaz status --project {dir}"), "how it ended"),
        (format!("git -C {dir} log --oneline"), "one commit per step"),
        (
            format!("{dir}/.tenaz/"),
            "the brief, spec, task cards, notes and journal",
        ),
    ];
    let width = look.iter().map(|(what, _)| what.len()).max().unwrap_or(0);
    eprintln!(
        "\nThat was a whole run with the scripted harness: no model and no agent, but the \
         same runner, verify script and git commits as a real run. Look at:"
    );
    for (what, why) in &look {
        eprintln!("  {what:<width$}   {why}");
    }
    eprintln!("For a real run, tenaz doctor says what this machine still needs.");
    Ok(code)
}

/// `tenaz doctor`: prints what is ready and what is missing, with how to
/// fix it; succeeds only when a real run can start.
async fn doctor(base_url: Option<&str>) -> ExitCode {
    let checks = tenaz_core::doctor::examine(
        &detect::machine(base_url).await,
        RunParams::default().context_floor,
    );
    print!("{}", tenaz_core::doctor::render(&checks));
    if tenaz_core::doctor::ready(&checks) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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

/// The harness version to pin and, with Pi, the model's entry in
/// `~/.pi/agent/models.json` when that file lists it.
fn pin_harness(
    harness: HarnessArg,
    bin: Option<&Path>,
    provider: &str,
    model: &str,
) -> anyhow::Result<(String, Option<ModelEntry>)> {
    let bin = bin.unwrap_or_else(|| Path::new(harness.kind().as_str()));
    Ok(match harness {
        HarnessArg::Fx => (
            tenaz_acp::fx::fx_version(bin)
                .with_context(|| format!("cannot run {} --version", bin.display()))?,
            None,
        ),
        HarnessArg::Pi => {
            let entry = match tenaz_pi::models::default_models_path() {
                Some(path) => tenaz_pi::models::model_entry(&path, provider, model)?,
                None => None,
            };
            (
                tenaz_pi::pi_version(bin)
                    .with_context(|| format!("cannot run {} --version", bin.display()))?,
                entry,
            )
        }
        HarnessArg::Fake => (
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
