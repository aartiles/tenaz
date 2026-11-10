//! `tenaz init`: the plan, from flags or from the setup wizard, then the
//! project, its placement (a worktree, in place or fresh) and run.json.

use std::collections::BTreeMap;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tenaz_core::ledger::{EngineApi, HarnessKind, Launch, ModelConfig, swap_refusal};
use tenaz_core::mcp_servers::{check_servers, parse_mcp_servers};
use tenaz_core::orchestrator::{InitOptions, init};
use tenaz_core::params::RunParams;
use tenaz_core::project::Project;
use tenaz_core::role::Role;
use tenaz_core::role_models::{self, Known, RoleFlags, RolePick, RunDefaults};
use tenaz_pi::models::ModelEntry;

use super::harness_bins::HarnessBins;
use super::{
    EngineKind, absolute, pinned_path, pinned_workflows, runner_version, shell_quote, templates,
};
use crate::detect::{self, Engine};
use crate::{wizard, worktree};

/// Which harness runs the sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum HarnessArg {
    /// fx over ACP (default).
    Fx,
    /// Pi in RPC mode, with a models.json Tenaz writes for the run.
    Pi,
    /// `OpenCode` over ACP.
    #[value(name = "opencode")]
    OpenCode,
    /// Qwen Code over ACP.
    Qwen,
    /// dsh over ACP, with a patch Tenaz writes for the run.
    Dsh,
    /// Any other ACP agent: the command --acp-bin names, with --acp-args.
    Acp,
    /// The scripted fake harness: exercises the runner without a model.
    Fake,
}

impl HarnessArg {
    pub(crate) fn kind(self) -> HarnessKind {
        match self {
            Self::Fx => HarnessKind::Fx,
            Self::Pi => HarnessKind::Pi,
            Self::OpenCode => HarnessKind::OpenCode,
            Self::Qwen => HarnessKind::Qwen,
            Self::Dsh => HarnessKind::Dsh,
            Self::Acp => HarnessKind::Acp,
            Self::Fake => HarnessKind::Fake,
        }
    }
}

/// The flag that names `kind`: a detected agent as the wizard writes it
/// back as flags.
impl From<HarnessKind> for HarnessArg {
    fn from(kind: HarnessKind) -> Self {
        match kind {
            HarnessKind::Fx => Self::Fx,
            HarnessKind::Pi => Self::Pi,
            HarnessKind::OpenCode => Self::OpenCode,
            HarnessKind::Qwen => Self::Qwen,
            HarnessKind::Dsh => Self::Dsh,
            HarnessKind::Acp => Self::Acp,
            HarnessKind::Fake => Self::Fake,
        }
    }
}

/// The engine API when nothing answered to say which: Sushi on its port
/// (the default before engines were detected), generic elsewhere.
pub(crate) fn guess_api(base_url: Option<&str>) -> EngineApi {
    match base_url {
        None => EngineApi::LoadApi,
        Some(url) if url.contains(":12345") => EngineApi::LoadApi,
        Some(_) => EngineApi::Generic,
    }
}

/// `tenaz init`. Without `--model`, in a terminal, it asks for what is missing.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct InitArgs {
    /// The project directory (asked when missing).
    #[arg(long)]
    pub(crate) project: Option<PathBuf>,
    /// Copied to .tenaz/brief.md (or put the goal there first).
    #[arg(long)]
    pub(crate) brief: Option<PathBuf>,
    /// The coding agent (default: fx; the wizard offers what is installed).
    #[arg(long, value_enum)]
    pub(crate) harness: Option<HarnessArg>,
    #[command(flatten)]
    pub(crate) bins: HarnessBins,
    /// The model id as the engine lists it; without it, the setup wizard runs.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// The provider name used in the harness configuration (with --harness pi,
    /// the provider in models.json; Qwen Code does not use it).
    #[arg(long)]
    pub(crate) provider: Option<String>,
    /// The engine's OpenAI-compatible base URL (default: Sushi on 127.0.0.1:12345;
    /// with --harness pi, the provider's baseUrl in models.json).
    #[arg(long)]
    pub(crate) base_url: Option<String>,
    /// Default 131072; with --harness pi, the model's contextWindow in models.json.
    #[arg(long)]
    pub(crate) context_window: Option<u64>,
    /// Another model for one role, `ROLE=ID` (repeatable); the engine must
    /// serve it next to --model.
    #[arg(long = "model-for", value_name = "ROLE=ID")]
    pub(crate) models_for: Vec<String>,
    /// The context window of a --model-for model, `ROLE=N` (repeatable;
    /// default: with --harness pi, the model's contextWindow in models.json,
    /// else the --model's window; the same for roles on one model).
    #[arg(long = "context-window-for", value_name = "ROLE=N")]
    pub(crate) context_windows_for: Vec<String>,
    /// The model reads images (with --harness pi, read from models.json when listed there).
    #[arg(long)]
    pub(crate) vision: bool,
    #[arg(long, default_value = "medium")]
    pub(crate) thinking: String,
    /// Parameter overrides, `key=value` (see `tenaz params`).
    #[arg(long = "set")]
    pub(crate) sets: Vec<String>,
    /// Skill directories to pin read-only into .tenaz/skills/.
    #[arg(long = "skill")]
    pub(crate) skills: Vec<PathBuf>,
    /// The engine API (default: detected from the engine at --base-url).
    #[arg(long, value_enum)]
    pub(crate) engine: Option<EngineKind>,
    /// Generic engine: the command that starts it (the whole command line).
    #[arg(long)]
    pub(crate) engine_start: Option<String>,
    /// Generic engine: the command that stops it.
    #[arg(long)]
    pub(crate) engine_stop: Option<String>,
    /// Talk to the engine directly instead of through the proxy.
    #[arg(long)]
    pub(crate) no_proxy: bool,
    /// Keep one of the --model-for models loaded at a time and swap them
    /// before each session, for models that do not fit together. Needs
    /// --engine load-api.
    #[arg(long)]
    pub(crate) swap_models: bool,
    /// `ComfyUI` base URL; enables the image tool.
    #[arg(long)]
    pub(crate) comfyui: Option<String>,
    /// Image workflows, `name=path.json` (API-format workflow with {{prompt}} placeholders).
    #[arg(long = "workflow")]
    pub(crate) workflows: Vec<String>,
    /// Base URL of an `OpenAI` Images API (`http://host:port/v1`); enables the
    /// image tool instead of `ComfyUI`.
    #[arg(long)]
    pub(crate) images: Option<String>,
    /// The image model to ask the `--images` API for.
    #[arg(long)]
    pub(crate) image_model: Option<String>,
    /// Base URL of an image-to-3D API (`http://host:port/v1`, mlx-serve's
    /// `/v1/3d/generations`); enables the 3D tool.
    #[arg(long)]
    pub(crate) meshes: Option<String>,
    /// The 3D model to ask the `--meshes` API for.
    #[arg(long)]
    pub(crate) mesh_model: Option<String>,
    /// Media job time limit, in seconds (default 1800).
    #[arg(long)]
    pub(crate) media_timeout: Option<u64>,
    /// An MCP server of your own the agent is offered, `name=url` (MCP over
    /// HTTP); repeat for more. Refused with Pi or dsh.
    #[arg(long = "mcp")]
    pub(crate) mcp_servers: Vec<String>,
    /// Ask for what is missing even when stdin is not a terminal.
    #[arg(long)]
    pub(crate) interactive: bool,
    /// In a repository with commits, work on its current branch, in this
    /// checkout, instead of on a new branch in a worktree; uncommitted
    /// changes go into Tenaz's first commit.
    #[arg(long)]
    pub(crate) in_place: bool,
    /// The worktree directory (default: ../<repo>-tenaz-<slug>).
    #[arg(long, conflicts_with = "in_place")]
    pub(crate) worktree: Option<PathBuf>,
    /// The slug of the run branch tenaz/<slug> (default: from the goal).
    #[arg(long, conflicts_with = "in_place")]
    pub(crate) name: Option<String>,
}

impl InitArgs {
    /// The `tenaz init` command line that gives these answers without questions.
    pub(crate) fn command_line(&self) -> String {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        let mut flags = vec![
            ("project", path(&self.project)),
            ("brief", path(&self.brief)),
            ("harness", self.harness.map(|h| h.kind().to_string())),
        ];
        flags.extend(self.bins.flags());
        flags.extend([
            ("model", self.model.clone()),
            ("provider", self.provider.clone()),
            ("base-url", self.base_url.clone()),
            ("context-window", self.context_window.map(|c| c.to_string())),
        ]);
        flags.extend(
            self.models_for
                .iter()
                .map(|p| ("model-for", Some(p.clone()))),
        );
        flags.extend(
            self.context_windows_for
                .iter()
                .map(|p| ("context-window-for", Some(p.clone()))),
        );
        flags.push((
            "thinking",
            (self.thinking != "medium").then(|| self.thinking.clone()),
        ));
        flags.extend(self.sets.iter().map(|pair| ("set", Some(pair.clone()))));
        flags.extend(
            self.skills
                .iter()
                .map(|skill| ("skill", Some(skill.display().to_string()))),
        );
        flags.extend([
            ("engine", self.engine.map(|e| e.flag().to_owned())),
            ("engine-start", self.engine_start.clone()),
            ("engine-stop", self.engine_stop.clone()),
        ]);
        flags.extend(self.tool_flags());
        let switches = [
            ("vision", self.vision),
            ("no-proxy", self.no_proxy),
            ("swap-models", self.swap_models),
            ("in-place", self.in_place),
        ];
        let place = [
            ("worktree", path(&self.worktree)),
            ("name", self.name.clone()),
        ];
        let mut words = vec!["tenaz".to_owned(), "init".to_owned()];
        words.extend(flag_words(flags));
        words.extend(
            switches
                .into_iter()
                .filter(|(_, on)| *on)
                .map(|(name, _)| format!("--{name}")),
        );
        words.extend(flag_words(place));
        words.join(" ")
    }

    /// The media and MCP flags, in the order `command_line` gives them.
    fn tool_flags(&self) -> Vec<(&'static str, Option<String>)> {
        let mut flags = vec![("comfyui", self.comfyui.clone())];
        flags.extend(self.workflows.iter().map(|w| ("workflow", Some(w.clone()))));
        flags.extend([
            ("images", self.images.clone()),
            ("image-model", self.image_model.clone()),
            ("meshes", self.meshes.clone()),
            ("mesh-model", self.mesh_model.clone()),
            ("media-timeout", self.media_timeout.map(|t| t.to_string())),
        ]);
        flags.extend(self.mcp_servers.iter().map(|m| ("mcp", Some(m.clone()))));
        flags
    }
}

/// `--name value` for each flag given a value, quoted for the shell.
fn flag_words<'a>(
    flags: impl IntoIterator<Item = (&'a str, Option<String>)>,
) -> impl Iterator<Item = String> {
    flags.into_iter().flat_map(|(name, value)| {
        value
            .map(|value| [format!("--{name}"), shell_quote(&value)])
            .into_iter()
            .flatten()
    })
}

/// Everything `init` needs, from flags or from the wizard: the answers,
/// and what is resolved from them where they say nothing.
pub(crate) struct InitPlan {
    pub(crate) project: PathBuf,
    pub(crate) harness: HarnessKind,
    pub(crate) harness_bin: Option<PathBuf>,
    pub(crate) model: String,
    /// The roles `--model-for` gives another model.
    pub(crate) role_picks: BTreeMap<Role, RolePick>,
    pub(crate) provider: String,
    pub(crate) api: EngineApi,
    /// The `--mcp` servers by name, checked (ADR 0032).
    pub(crate) mcp_servers: BTreeMap<String, String>,
    /// The goal typed in the wizard, written as the brief.
    pub(crate) brief_text: Option<String>,
    /// The answers, as flags: brief, base URL, context window, vision,
    /// engine start and stop commands, media, and what neither path changes (thinking, params, skills,
    /// proxy, timeout, where the run goes).
    pub(crate) args: InitArgs,
}

impl InitPlan {
    /// The plan from `args`: the flags, or the wizard's answers written as
    /// flags. `engine` is the engine that answered at `--base-url`, if one
    /// did; with `brief_text`, the goal typed in the wizard. What neither
    /// says has one default: fx, the engine's API and provider, else the
    /// ones usual for its address.
    pub(crate) fn from_args(
        args: InitArgs,
        engine: Option<&Engine>,
        brief_text: Option<String>,
    ) -> anyhow::Result<Self> {
        let Some(model) = args.model.clone() else {
            anyhow::bail!(
                "--model is required when stdin is not a terminal; run tenaz init in a terminal for the setup wizard"
            );
        };
        let Some(project) = args.project.clone() else {
            anyhow::bail!("--project is required");
        };
        let harness = args.harness.map_or(HarnessKind::Fx, HarnessArg::kind);
        let flags = RoleFlags {
            models: &args.models_for,
            windows: &args.context_windows_for,
        };
        let role_picks = flags.picks(&model)?;
        let provider = args
            .provider
            .clone()
            .unwrap_or_else(|| default_provider(engine, args.base_url.as_deref()));
        let api = engine_api(&args, engine);
        if let Some(why) = swap_refusal(api, args.swap_models, !role_picks.is_empty()) {
            anyhow::bail!("{why}");
        }
        let mcp_servers = mcp_servers_for(&args.mcp_servers, harness)?;
        Ok(Self {
            project,
            harness,
            harness_bin: args.bins.get(harness).map(Path::to_path_buf),
            model,
            role_picks,
            provider,
            api,
            mcp_servers,
            brief_text,
            args,
        })
    }
}

/// The `--mcp` servers, refused when `harness` would not see them.
pub(crate) fn mcp_servers_for(
    specs: &[String],
    harness: HarnessKind,
) -> anyhow::Result<BTreeMap<String, String>> {
    let servers = parse_mcp_servers(specs)?;
    check_servers(&servers, harness).map_err(|why| {
        anyhow::anyhow!("{why}; drop --mcp or use --harness fx, opencode, qwen or acp")
    })?;
    Ok(servers)
}

/// The engine API: `--engine`, else the API of the `engine` that answered,
/// else a guess from `--base-url`.
pub(crate) fn engine_api(args: &InitArgs, engine: Option<&Engine>) -> EngineApi {
    args.engine.map_or_else(
        || engine.map_or_else(|| guess_api(args.base_url.as_deref()), |e| e.api),
        EngineKind::api,
    )
}

/// The provider name when `--provider` does not say: the `engine`'s that
/// answered, else the one usually on `base_url`'s port, else `local`;
/// `sushi` without an address (Sushi's port is the default one).
pub(crate) fn default_provider(engine: Option<&Engine>, base_url: Option<&str>) -> String {
    match (engine, base_url) {
        (Some(engine), _) => engine.provider.clone(),
        (None, Some(url)) => detect::provider_by_port(url).unwrap_or("local").to_owned(),
        (None, None) => "sushi".to_owned(),
    }
}

/// The plan from flags alone. The engine at `--base-url` is asked for its
/// API and provider when the flags do not say them.
pub(super) async fn plan_from_flags(args: InitArgs) -> anyhow::Result<InitPlan> {
    let detected = match &args.base_url {
        Some(url) if args.engine.is_none() || args.provider.is_none() => {
            detect::engine_at(&detect::client(), url).await
        }
        _ => None,
    };
    InitPlan::from_args(args, detected.as_ref(), None)
}

/// `tenaz init`: asks for what is missing (without `--model`, in a
/// terminal or with `--interactive`), then initializes.
pub(crate) async fn execute(args: Box<InitArgs>) -> anyhow::Result<()> {
    let ask = args.model.is_none() && (args.interactive || std::io::stdin().is_terminal());
    let plan = if ask {
        let stdin = std::io::stdin();
        let mut prompter = wizard::Prompter::new(stdin.lock(), std::io::stderr());
        wizard::run(&args, &mut prompter).await?
    } else {
        plan_from_flags(*args).await?
    };
    init_from_plan(plan)
}

/// The brief's text and the file `init` copies it from: `--brief`, the
/// wizard's goal (through a scratch file), or none (the brief in place).
fn brief_source(plan: &InitPlan) -> anyhow::Result<(Option<String>, Option<PathBuf>)> {
    if let Some(path) = &plan.args.brief {
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

/// Places the run (a worktree, in place, or a new folder), initializes it
/// from `plan`, removes a scratch brief, and discards the worktree when
/// that fails.
pub(super) fn init_from_plan(plan: InitPlan) -> anyhow::Result<()> {
    let given = absolute(plan.project.clone());
    let (brief_text, brief) = brief_source(&plan)?;
    let request = worktree::PlaceRequest {
        in_place: plan.args.in_place,
        worktree: plan.args.worktree.clone().map(absolute),
        name: plan.args.name.clone(),
    };
    let placement = worktree::place(&given, &request, brief_text.as_deref(), brief.as_deref())?;
    let scratch = plan.args.brief.is_none().then(|| brief.clone()).flatten();
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
    let config = model_config(&plan)?;
    let launch = launch(&plan)?;
    let brief = match placement {
        worktree::Placement::Worktree(wt) => brief.or_else(|| wt.untracked_brief.clone()),
        _ => brief,
    };
    make_place(placement)?;
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
    if let Some(mut origin) = placement_origin(placement) {
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

/// The model and harness to pin: the harness's version, and what the
/// command line or Pi's models.json says of the model.
fn model_config(plan: &InitPlan) -> anyhow::Result<ModelConfig> {
    let (harness_version, listed) =
        pin_harness(plan.harness, plan.harness_bin.as_deref(), &plan.provider)?;
    let (input, base_url, context_window) = resolve_model(
        listed.entry(&plan.model),
        plan.args.vision,
        plan.args.base_url.clone(),
        plan.args.context_window,
    );
    let run = RunDefaults {
        provider: &plan.provider,
        context_window,
        input: &input,
    };
    let known = |id: &str| listed.known(id, plan.args.vision);
    let role_models = role_models::pin(&plan.role_picks, run, known);
    Ok(ModelConfig {
        harness: plan.harness,
        harness_version,
        model: format!("{}/{}", plan.provider, plan.model),
        provider: Some(plan.provider.clone()),
        id: plan.model.clone(),
        base_url: Some(base_url),
        context_window,
        input,
        thinking: Some(plan.args.thinking.clone()),
        role_models,
    })
}

/// How to start the engine, the harness and the media tools.
fn launch(plan: &InitPlan) -> anyhow::Result<Launch> {
    Ok(Launch {
        engine: plan.api,
        engine_start: plan.args.engine_start.clone(),
        engine_stop: plan.args.engine_stop.clone(),
        no_proxy: plan.args.no_proxy,
        swap_models: plan.args.swap_models,
        harness_bin: plan.harness_bin.as_deref().map(pinned_path),
        fx_args: None,
        acp_args: None,
        comfyui: plan.args.comfyui.clone(),
        workflows: pinned_workflows(&plan.args.workflows)?,
        images_url: plan.args.images.clone(),
        image_model: plan.args.image_model.clone(),
        meshes_url: plan.args.meshes.clone(),
        mesh_model: plan.args.mesh_model.clone(),
        media_timeout_sec: plan.args.media_timeout,
        mcp_servers: plan.mcp_servers.clone(),
    })
}

/// Creates the run's worktree, or says where an in-place run commits.
fn make_place(placement: &worktree::Placement) -> anyhow::Result<()> {
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
    Ok(())
}

/// The repository a run on an existing one came from.
fn placement_origin(placement: &worktree::Placement) -> Option<tenaz_core::ledger::Origin> {
    match placement {
        worktree::Placement::Worktree(wt) => Some(wt.origin()),
        worktree::Placement::InPlace { origin } => Some(origin.clone()),
        worktree::Placement::Fresh => None,
    }
}

/// The harness version to pin and, with Pi, the model's entry in
/// `~/.pi/agent/models.json` when that file lists it. The generic agent
/// has no name to find on PATH: it needs its command.
fn pin_harness(
    harness: HarnessKind,
    bin: Option<&Path>,
    provider: &str,
) -> anyhow::Result<(String, Listed)> {
    if harness == HarnessKind::Acp && bin.is_none() {
        anyhow::bail!("--harness acp needs --acp-bin <command>");
    }
    let bin = bin.unwrap_or_else(|| Path::new(harness.as_str()));
    let version = detect::agent_version(harness, bin)
        .with_context(|| format!("cannot run {} --version", bin.display()))?;
    let listed = match harness {
        HarnessKind::Pi => {
            Listed::read(tenaz_pi::models::default_models_path().as_deref(), provider)?
        }
        HarnessKind::Fx
        | HarnessKind::OpenCode
        | HarnessKind::Qwen
        | HarnessKind::Dsh
        | HarnessKind::Acp
        | HarnessKind::Fake => Listed::default(),
    };
    Ok((version, listed))
}

/// The models the user's Pi `models` file lists under the run's provider:
/// what init reads of `--model` and of each role's model there. Empty for
/// another harness or without the file.
#[derive(Debug, Default)]
struct Listed(Vec<tenaz_pi::models::ListedModel>);

impl Listed {
    fn read(models: Option<&Path>, provider: &str) -> anyhow::Result<Self> {
        let Some(path) = models else {
            return Ok(Self::default());
        };
        let listed = tenaz_pi::models::list_models(path)?;
        Ok(Self(
            listed
                .into_iter()
                .filter(|m| m.provider == provider)
                .collect(),
        ))
    }

    fn entry(&self, id: &str) -> Option<&ModelEntry> {
        self.0.iter().find(|m| m.id == id).map(|m| &m.entry)
    }

    /// What the file says of a role's model, as of `--model`: its window,
    /// and its inputs unless `--vision` says the run's models read images.
    fn known(&self, id: &str, vision: bool) -> Known {
        let Some(entry) = self.entry(id) else {
            return Known::default();
        };
        Known {
            context_window: entry.context_window,
            input: (!vision).then(|| entry.input.clone()),
        }
    }
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

#[cfg(test)]
mod tests;
