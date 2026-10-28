//! `tenaz init`: the plan, from flags or from the setup wizard, then the
//! project, its placement (a worktree, in place or fresh) and run.json.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tenaz_core::ledger::{EngineApi, HarnessKind, Launch, ModelConfig};
use tenaz_core::orchestrator::{InitOptions, init};
use tenaz_core::params::RunParams;
use tenaz_core::project::Project;
use tenaz_pi::models::ModelEntry;

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
    /// The scripted fake harness: exercises the runner without a model.
    Fake,
}

impl HarnessArg {
    pub(crate) fn kind(self) -> HarnessKind {
        match self {
            Self::Fx => HarnessKind::Fx,
            Self::Pi => HarnessKind::Pi,
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
    /// The fx executable.
    #[arg(long)]
    pub(crate) fx_bin: Option<PathBuf>,
    /// The pi executable.
    #[arg(long)]
    pub(crate) pi_bin: Option<PathBuf>,
    /// The model id as the engine lists it; without it, the setup wizard runs.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// The provider name used in the harness configuration (with --harness pi,
    /// the provider in models.json).
    #[arg(long)]
    pub(crate) provider: Option<String>,
    /// The engine's OpenAI-compatible base URL (default: Sushi on 127.0.0.1:12345;
    /// with --harness pi, the provider's baseUrl in models.json).
    #[arg(long)]
    pub(crate) base_url: Option<String>,
    /// Default 131072; with --harness pi, the model's contextWindow in models.json.
    #[arg(long)]
    pub(crate) context_window: Option<u64>,
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
    /// Media job time limit, in seconds (default 1800).
    #[arg(long)]
    pub(crate) media_timeout: Option<u64>,
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
    /// The `--fx-bin` or `--pi-bin` given for `harness`.
    pub(crate) fn harness_bin(&self, harness: HarnessKind) -> Option<PathBuf> {
        match harness {
            HarnessKind::Fx => self.fx_bin.clone(),
            HarnessKind::Pi => self.pi_bin.clone(),
            HarnessKind::Fake => None,
        }
    }

    /// The `tenaz init` command line that gives these answers without questions.
    pub(crate) fn command_line(&self) -> String {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        let mut flags = vec![
            ("project", path(&self.project)),
            ("brief", path(&self.brief)),
            ("harness", self.harness.map(|h| h.kind().to_string())),
            ("fx-bin", path(&self.fx_bin)),
            ("pi-bin", path(&self.pi_bin)),
            ("model", self.model.clone()),
            ("provider", self.provider.clone()),
            ("base-url", self.base_url.clone()),
            ("context-window", self.context_window.map(|c| c.to_string())),
            (
                "thinking",
                (self.thinking != "medium").then(|| self.thinking.clone()),
            ),
        ];
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
            ("comfyui", self.comfyui.clone()),
        ]);
        flags.extend(self.workflows.iter().map(|w| ("workflow", Some(w.clone()))));
        flags.extend([
            ("images", self.images.clone()),
            ("image-model", self.image_model.clone()),
            ("media-timeout", self.media_timeout.map(|t| t.to_string())),
        ]);
        let switches = [
            ("vision", self.vision),
            ("no-proxy", self.no_proxy),
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
    pub(crate) provider: String,
    pub(crate) api: EngineApi,
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
        let provider = args
            .provider
            .clone()
            .unwrap_or_else(|| default_provider(engine, args.base_url.as_deref()));
        Ok(Self {
            project,
            harness,
            harness_bin: args.harness_bin(harness),
            model,
            provider,
            api: engine_api(&args, engine),
            brief_text,
            args,
        })
    }
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
    let (harness_version, entry) = pin_harness(
        plan.harness,
        plan.harness_bin.as_deref(),
        &plan.provider,
        &plan.model,
    )?;
    let (input, base_url, context_window) = resolve_model(
        entry.as_ref(),
        plan.args.vision,
        plan.args.base_url.clone(),
        plan.args.context_window,
    );
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
    })
}

/// How to start the engine, the harness and the media tools.
fn launch(plan: &InitPlan) -> anyhow::Result<Launch> {
    Ok(Launch {
        engine: plan.api,
        engine_start: plan.args.engine_start.clone(),
        engine_stop: plan.args.engine_stop.clone(),
        no_proxy: plan.args.no_proxy,
        harness_bin: plan.harness_bin.as_deref().map(pinned_path),
        fx_args: None,
        comfyui: plan.args.comfyui.clone(),
        workflows: pinned_workflows(&plan.args.workflows)?,
        images_url: plan.args.images.clone(),
        image_model: plan.args.image_model.clone(),
        media_timeout_sec: plan.args.media_timeout,
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
/// `~/.pi/agent/models.json` when that file lists it.
fn pin_harness(
    harness: HarnessKind,
    bin: Option<&Path>,
    provider: &str,
    model: &str,
) -> anyhow::Result<(String, Option<ModelEntry>)> {
    let bin = bin.unwrap_or_else(|| Path::new(harness.as_str()));
    Ok(match harness {
        HarnessKind::Fx => (
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
                tenaz_pi::pi_version(bin)
                    .with_context(|| format!("cannot run {} --version", bin.display()))?,
                entry,
            )
        }
        HarnessKind::Fake => (
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn init_args(line: &[&str]) -> InitArgs {
        let crate::Command::Init(args) = crate::Cli::parse_from(line).command else {
            panic!("not an init line: {line:?}");
        };
        *args
    }

    #[test]
    fn the_command_line_gives_every_answer_back_in_flag_order() {
        let args = init_args(&[
            "tenaz",
            "init",
            "--project",
            "bob's goal",
            "--brief",
            "b.md",
            "--harness",
            "pi",
            "--fx-bin",
            "/opt/fx",
            "--pi-bin",
            "/opt/pi",
            "--model",
            "m",
            "--provider",
            "p",
            "--base-url",
            "http://h:1/v1",
            "--context-window",
            "65536",
            "--vision",
            "--thinking",
            "high",
            "--set",
            "a=1",
            "--set",
            "b=2",
            "--skill",
            "s1",
            "--skill",
            "s2",
            "--engine",
            "generic",
            "--engine-start",
            "serve it",
            "--engine-stop",
            "stop",
            "--no-proxy",
            "--comfyui",
            "http://c",
            "--workflow",
            "w=w.json",
            "--workflow",
            "v=v.json",
            "--images",
            "http://i/v1",
            "--image-model",
            "im",
            "--media-timeout",
            "60",
            "--worktree",
            "../wt",
            "--name",
            "slug",
        ]);
        assert_eq!(
            args.command_line(),
            "tenaz init --project 'bob'\\''s goal' --brief b.md --harness pi --fx-bin /opt/fx \
             --pi-bin /opt/pi --model m --provider p --base-url http://h:1/v1 \
             --context-window 65536 --thinking high --set a=1 --set b=2 --skill s1 --skill s2 \
             --engine generic --engine-start 'serve it' --engine-stop stop \
             --comfyui http://c --workflow w=w.json --workflow v=v.json --images http://i/v1 --image-model im \
             --media-timeout 60 --vision --no-proxy --worktree ../wt --name slug"
        );
    }

    #[test]
    fn the_command_line_leaves_out_defaults_and_keeps_in_place() {
        let args = init_args(&[
            "tenaz",
            "init",
            "--model",
            "m",
            "--thinking",
            "medium",
            "--interactive",
            "--in-place",
        ]);
        assert_eq!(args.command_line(), "tenaz init --model m --in-place");
    }

    fn plan(line: &[&str], engine: Option<&Engine>) -> InitPlan {
        InitPlan::from_args(init_args(line), engine, None).unwrap()
    }

    const BASE: [&str; 6] = ["tenaz", "init", "--project", "p", "--model", "m"];

    fn with(extra: &[&'static str]) -> Vec<&'static str> {
        BASE.iter().chain(extra).copied().collect()
    }

    /// LM Studio, answering on a port no engine usually uses, with the
    /// load API (the guess for that port would be generic).
    fn lm_studio() -> Engine {
        Engine {
            label: "LM Studio".to_owned(),
            provider: "lmstudio".to_owned(),
            base_url: "http://127.0.0.1:9999/v1".to_owned(),
            api: EngineApi::LoadApi,
            models: Vec::new(),
        }
    }

    #[test]
    fn the_provider_and_api_are_the_ones_of_the_engine_that_answered() {
        let line = with(&["--base-url", "http://127.0.0.1:9999/v1"]);
        let got = plan(&line, Some(&lm_studio()));
        assert_eq!(
            (got.provider.as_str(), got.api),
            ("lmstudio", EngineApi::LoadApi)
        );
    }

    #[test]
    fn without_an_engine_the_provider_is_the_one_usual_on_the_port() {
        let line = with(&["--base-url", "http://127.0.0.1:8080/v1"]);
        assert_eq!(plan(&line, None).provider, "llamacpp");
        let line = with(&["--base-url", "http://127.0.0.1:9999/v1"]);
        assert_eq!(plan(&line, None).provider, "local");
        assert_eq!(plan(&BASE, None).provider, "sushi");
    }

    #[test]
    fn a_given_provider_and_engine_api_win_over_the_engine() {
        let line = with(&["--provider", "mine", "--engine", "generic"]);
        let got = plan(&line, Some(&lm_studio()));
        assert_eq!(
            (got.provider.as_str(), got.api),
            ("mine", EngineApi::Generic)
        );
    }

    #[test]
    fn the_harness_is_fx_unless_given_and_keeps_only_its_own_binary() {
        let line = with(&["--fx-bin", "/opt/fx", "--pi-bin", "/opt/pi"]);
        let fx = plan(&line, None);
        assert_eq!(
            (fx.harness, fx.harness_bin),
            (HarnessKind::Fx, Some(PathBuf::from("/opt/fx")))
        );
        let line = with(&[
            "--harness",
            "pi",
            "--fx-bin",
            "/opt/fx",
            "--pi-bin",
            "/opt/pi",
        ]);
        let pi = plan(&line, None);
        assert_eq!(
            (pi.harness, pi.harness_bin),
            (HarnessKind::Pi, Some(PathBuf::from("/opt/pi")))
        );
    }

    #[test]
    fn the_fake_harness_has_no_binary() {
        let line = with(&["--harness", "fake", "--fx-bin", "/opt/fx"]);
        let fake = plan(&line, None);
        assert_eq!((fake.harness, fake.harness_bin), (HarnessKind::Fake, None));
    }

    #[test]
    fn a_plan_needs_a_model_and_a_project() {
        let error = |line: &[&str]| {
            InitPlan::from_args(init_args(line), None, None)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        assert!(error(&["tenaz", "init", "--project", "p"]).contains("--model"));
        assert!(error(&["tenaz", "init", "--model", "m"]).contains("--project"));
    }
}
