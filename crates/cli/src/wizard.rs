//! `tenaz init` without a model, in a terminal: asks for what the flags did
//! not say, offering what [`crate::detect`] finds, and ends with the same
//! [`InitPlan`] the flags would have produced. Questions go to stderr and
//! answers come one per line, so the wizard also runs with a piped stdin.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::bail;
use tenaz_core::doctor;
use tenaz_core::ledger::EngineApi;

use crate::commands::EngineKind;
use crate::commands::init::{
    HarnessArg, InitArgs, InitPlan, default_provider, engine_api, guess_api,
};
use crate::detect::{self, Engine};
use crate::worktree;

/// Numbered choices, free text and yes/no over a line-based terminal.
pub struct Prompter<R, W> {
    input: R,
    output: W,
}

impl<R: BufRead, W: Write> Prompter<R, W> {
    pub fn new(input: R, output: W) -> Self {
        Self { input, output }
    }

    pub fn say(&mut self, text: &str) -> anyhow::Result<()> {
        writeln!(self.output, "{text}")?;
        Ok(())
    }

    fn line(&mut self) -> anyhow::Result<String> {
        self.output.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            bail!("the input ended before the setup was complete");
        }
        Ok(line.trim().to_owned())
    }

    /// Free text; an empty answer takes `default` when there is one.
    pub fn ask(&mut self, question: &str, default: Option<&str>) -> anyhow::Result<String> {
        loop {
            match default {
                Some(d) => write!(self.output, "{question} [{d}]: ")?,
                None => write!(self.output, "{question}: ")?,
            }
            let answer = self.line()?;
            match (answer.is_empty(), default) {
                (false, _) => return Ok(answer),
                (true, Some(d)) => return Ok(d.to_owned()),
                (true, None) => self.say("  An answer is needed.")?,
            }
        }
    }

    /// One of `options` by its number; an empty answer takes `default`.
    pub fn choose(
        &mut self,
        question: &str,
        options: &[String],
        default: usize,
    ) -> anyhow::Result<usize> {
        self.say(question)?;
        for (i, option) in options.iter().enumerate() {
            self.say(&format!("  {}) {option}", i + 1))?;
        }
        loop {
            write!(
                self.output,
                "Choose 1-{} [{}]: ",
                options.len(),
                default + 1
            )?;
            let answer = self.line()?;
            if answer.is_empty() {
                return Ok(default);
            }
            match answer.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
                _ => self.say(&format!("  Type a number from 1 to {}.", options.len()))?,
            }
        }
    }

    pub fn confirm(&mut self, question: &str, default: bool) -> anyhow::Result<bool> {
        let hint = if default { "Y/n" } else { "y/N" };
        loop {
            write!(self.output, "{question} [{hint}]: ")?;
            match self.line()?.to_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" | "s" | "si" | "sí" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("  Answer y or n.")?,
            }
        }
    }
}

/// The goal: a brief file to copy, or a line typed in the wizard.
enum Goal {
    InProject,
    File(PathBuf),
    Text(String),
}

/// What is known about the model as the questions go.
#[derive(Default)]
struct ModelChoice {
    id: Option<String>,
    provider: Option<String>,
    base_url: Option<String>,
    context_window: Option<u64>,
    vision: Option<bool>,
    engine: Option<Engine>,
}

impl ModelChoice {
    /// The address the model is served at: the one chosen, else the
    /// engine's.
    fn model_url(&self) -> Option<&str> {
        self.base_url
            .as_deref()
            .or_else(|| self.engine.as_ref().map(|e| e.base_url.as_str()))
    }
}

/// The image tool and the commands that free memory for it.
struct Media {
    comfyui: Option<String>,
    workflows: Vec<String>,
    images: Option<String>,
    image_model: Option<String>,
    engine_start: Option<String>,
    engine_stop: Option<String>,
}

impl Media {
    /// Writes these answers into `args`, as their flags would.
    fn answer(self, args: &mut InitArgs) {
        args.engine_start = self.engine_start;
        args.engine_stop = self.engine_stop;
        args.comfyui = self.comfyui;
        args.workflows = self.workflows;
        args.images = self.images;
        args.image_model = self.image_model;
    }
}

/// The engine that serves the model: its API and address.
#[derive(Clone, Copy)]
struct ModelEngine<'a> {
    api: EngineApi,
    base_url: Option<&'a str>,
}

impl ModelEngine<'_> {
    /// Whether `url` is this engine's address.
    fn serves(&self, url: &str) -> bool {
        self.base_url
            .is_some_and(|own| detect::normalize_base_url(own) == detect::normalize_base_url(url))
    }
}

/// Asks for whatever `args` leaves open and returns the plan to init with.
pub async fn run<R: BufRead, W: Write>(
    args: &InitArgs,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<InitPlan> {
    if args.project.is_none()
        && let Ok(here) = std::env::current_dir()
        && here.join(".tenaz/run.json").exists()
    {
        bail!(
            "{} already has a Tenaz run. `tenaz status` shows where it is; `tenaz run` \
             continues it (`tenaz run --resume` after a block). To start over here, \
             remove .tenaz/ first; to start another project, run tenaz init in another \
             folder or pass --project.",
            here.display()
        );
    }
    if detect::git().is_none() {
        bail!(
            "Tenaz needs git, and git is not on PATH. {}",
            doctor::git_fix(doctor::Os::from_name(std::env::consts::OS)).join(" ")
        );
    }
    p.say("Tenaz setup. Press Enter to take the [default].\n")?;
    let client = detect::client();
    let goal = ask_goal(args, p)?;
    let project = ask_project(args, &goal, p)?;
    let args = &ask_placement(args, &goal, &project, p)?;
    let harness = ask_agent(args, p)?;
    let choice = if harness == HarnessArg::Pi && args.model.is_none() {
        pi_model(args, &client, p).await?
    } else {
        engine_model(args, &client, p).await?
    };
    let model = choice.id.clone().unwrap_or_default();
    let detected = choice
        .engine
        .as_ref()
        .and_then(|e| e.models.iter().find(|m| m.id == model));
    let context_window = ask_context(
        choice
            .context_window
            .or(detected.and_then(|m| m.context_window)),
        choice.engine.as_ref(),
        p,
    )?;
    let vision = match choice.vision.or(detected.and_then(|m| m.vision)) {
        Some(v) => v,
        None => p.confirm("Does the model read images?", false)?,
    };
    let mut answers = InitArgs {
        project: Some(project),
        brief: match &goal {
            Goal::File(path) => Some(path.clone()),
            Goal::InProject | Goal::Text(_) => None,
        },
        harness: Some(harness),
        model: Some(model),
        provider: choice.provider.clone(),
        base_url: choice.base_url.clone(),
        context_window: Some(context_window),
        vision,
        ..args.clone()
    };
    // The API the plan will have: the media question depends on it.
    let llm = ModelEngine {
        api: engine_api(&answers, choice.engine.as_ref()),
        base_url: choice.model_url(),
    };
    ask_media(&answers, llm, &client, p)
        .await?
        .answer(&mut answers);
    let brief_text = match &goal {
        Goal::Text(text) => Some(text.clone()),
        Goal::InProject | Goal::File(_) => None,
    };
    let plan = InitPlan::from_args(answers, choice.engine.as_ref(), brief_text)?;
    show(&plan, &goal, choice.engine.as_ref(), p)?;
    if !p.confirm("Create the project?", true)? {
        bail!("Nothing was created.");
    }
    Ok(plan)
}

/// In a repository with commits: a new branch in a worktree (the default)
/// or the current branch in place. Checks the worktree can be made before
/// the other questions.
fn ask_placement<R: BufRead, W: Write>(
    args: &InitArgs,
    goal: &Goal,
    project: &Path,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<InitArgs> {
    let mut args = args.clone();
    if args.in_place || !worktree::is_existing_repo(project) {
        return Ok(args);
    }
    if args.worktree.is_none() && args.name.is_none() {
        let options = [
            "On a new branch tenaz/<goal>, in a worktree next to it (your checkout stays as it is)"
                .to_owned(),
            "In place, on the current branch (every step is a commit there)".to_owned(),
        ];
        let question = format!(
            "{} is a git repository. Where should Tenaz work?",
            project.display()
        );
        args.in_place = p.choose(&question, &options, 0)? == 1;
    }
    let brief = match goal {
        Goal::Text(text) => Some(format!("# Goal\n\n{text}\n")),
        Goal::File(path) => std::fs::read_to_string(path).ok(),
        Goal::InProject => None,
    };
    let request = worktree::PlaceRequest {
        in_place: args.in_place,
        worktree: args.worktree.clone(),
        name: args.name.clone(),
    };
    let placement = worktree::place(
        &crate::commands::absolute(project.to_path_buf()),
        &request,
        brief.as_deref(),
        match goal {
            Goal::File(path) => Some(path.as_path()),
            Goal::Text(_) | Goal::InProject => None,
        },
    )?;
    if let worktree::Placement::Worktree(wt) = &placement {
        p.say(&format!(
            "  Tenaz will work on {} in {}.",
            wt.branch,
            wt.path.display()
        ))?;
    }
    Ok(args)
}

fn ask_goal<R: BufRead, W: Write>(args: &InitArgs, p: &mut Prompter<R, W>) -> anyhow::Result<Goal> {
    if let Some(brief) = &args.brief {
        return Ok(Goal::File(brief.clone()));
    }
    if args
        .project
        .as_ref()
        .is_some_and(|dir| dir.join(tenaz_core::project::BRIEF).is_file())
    {
        return Ok(Goal::InProject);
    }
    let answer = p.ask(
        "What should it build? Type the goal in one line, or the path to a brief file",
        None,
    )?;
    Ok(if Path::new(&answer).is_file() {
        Goal::File(PathBuf::from(answer))
    } else {
        Goal::Text(answer)
    })
}

fn ask_project<R: BufRead, W: Write>(
    args: &InitArgs,
    goal: &Goal,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<PathBuf> {
    if let Some(dir) = &args.project {
        return Ok(dir.clone());
    }
    let suggested = match goal {
        Goal::Text(text) => slug(text),
        _ => "tenaz-project".to_owned(),
    };
    if let Ok(here) = std::env::current_dir()
        && let Some(count) = here_entries(&here)
    {
        let options = [
            format!(
                "Here, in {} ({count} {})",
                here.display(),
                if count == 1 { "item" } else { "items" }
            ),
            "In a new folder".to_owned(),
        ];
        if p.choose("Where should Tenaz work?", &options, 0)? == 0 {
            if !here.join(".git").exists() {
                p.say(
                    "  This folder is not a git repository: Tenaz makes it one and commits \
                     what is in it now as the starting point, so every change can be undone.",
                )?;
            }
            return Ok(here);
        }
    }
    loop {
        let dir = PathBuf::from(p.ask("Project directory", Some(&suggested))?);
        if !dir.join(".tenaz/run.json").exists() && !dir.join(".agent/run.json").exists() {
            return Ok(dir);
        }
        p.say("  That directory already has a Tenaz run; pick another.")?;
    }
}

fn ask_agent<R: BufRead, W: Write>(
    args: &InitArgs,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<HarnessArg> {
    if let Some(harness) = args.harness {
        return Ok(harness);
    }
    let agents = detect::agents();
    match agents.as_slice() {
        [] => bail!(
            "No coding agent found on PATH.\n{}\nAfter that, run tenaz init again \
             (tenaz doctor shows anything else missing).",
            doctor::agent_fix().join("\n")
        ),
        [only] => {
            p.say(&format!("Agent: {} ({})", only.harness, only.version))?;
            Ok(HarnessArg::from(only.harness))
        }
        several => {
            let options: Vec<String> = several
                .iter()
                .map(|a| format!("{} ({})", a.harness, a.version))
                .collect();
            let i = p.choose("Which coding agent should run the sessions?", &options, 0)?;
            Ok(HarnessArg::from(several[i].harness))
        }
    }
}

/// With Pi, a model its own `models.json` lists (at `--base-url`, when
/// given), or one found the way fx finds it: Tenaz writes Pi's model
/// settings for the run either way.
async fn pi_model<R: BufRead, W: Write>(
    args: &InitArgs,
    client: &reqwest::Client,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<ModelChoice> {
    let listed = match tenaz_pi::models::default_models_path() {
        Some(path) => tenaz_pi::models::list_models(&path).unwrap_or_else(|error| {
            tracing::warn!(%error, "ignoring Pi's models.json");
            Vec::new()
        }),
        None => Vec::new(),
    };
    let listed: Vec<_> = listed
        .into_iter()
        .filter(|m| {
            args.base_url.is_none()
                || m.entry.base_url.as_deref().map(detect::normalize_base_url)
                    == args.base_url.as_deref().map(detect::normalize_base_url)
        })
        .collect();
    if listed.is_empty() {
        return engine_model(args, client, p).await;
    }
    let mut options: Vec<String> = listed
        .iter()
        .map(|m| {
            let ctx = m
                .entry
                .context_window
                .map(|c| format!(", {c} tokens"))
                .unwrap_or_default();
            format!("{}/{}{ctx}", m.provider, m.id)
        })
        .collect();
    options.push("Another model, from a running engine".to_owned());
    let i = p.choose("Which model? These are in Pi's models.json.", &options, 0)?;
    let Some(chosen) = listed.get(i) else {
        return engine_model(args, client, p).await;
    };
    let base_url = chosen
        .entry
        .base_url
        .clone()
        .or_else(|| args.base_url.clone());
    let engine = match &base_url {
        Some(url) => detect::engine_at(client, url).await,
        None => None,
    };
    Ok(ModelChoice {
        id: Some(chosen.id.clone()),
        provider: Some(chosen.provider.clone()),
        base_url,
        context_window: args.context_window.or(chosen.entry.context_window),
        vision: Some(args.vision || chosen.entry.input.iter().any(|i| i == "image")),
        engine,
    })
}

/// The engine is found (or given) first, then its model chosen.
async fn engine_model<R: BufRead, W: Write>(
    args: &InitArgs,
    client: &reqwest::Client,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<ModelChoice> {
    let engine = if let Some(url) = &args.base_url {
        let found = detect::engine_at(client, url).await;
        if found.is_none() {
            p.say(&format!(
                "Nothing answers at {url} now; start the engine before tenaz run."
            ))?;
        }
        found
    } else {
        choose_engine(client, p).await?
    };
    let id = match (
        &args.model,
        engine.as_ref().map_or(&[][..], |e| &e.models[..]),
    ) {
        (Some(id), _) => id.clone(),
        (None, []) => p.ask("Model id, as the engine lists it", None)?,
        (None, [only]) => {
            p.say(&format!("Model: {}", only.id))?;
            only.id.clone()
        }
        (None, several) => {
            let options: Vec<String> = several.iter().map(|m| m.id.clone()).collect();
            several[p.choose("Which model?", &options, 0)?].id.clone()
        }
    };
    Ok(ModelChoice {
        id: Some(id),
        provider: args.provider.clone(),
        base_url: engine
            .as_ref()
            .map(|e| e.base_url.clone())
            .or_else(|| args.base_url.clone()),
        context_window: args.context_window,
        vision: args.vision.then_some(true),
        engine,
    })
}

fn ask_context<R: BufRead, W: Write>(
    known: Option<u64>,
    engine: Option<&Engine>,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<u64> {
    let context_window = if let Some(ctx) = known {
        p.say(&format!("Context window: {ctx} tokens"))?;
        ctx
    } else {
        if engine.is_some_and(|e| e.provider == "ollama") {
            p.say(
                "  Ollama serves a small context unless OLLAMA_CONTEXT_LENGTH (or the model's \
                 num_ctx) says otherwise; give the context it really runs with.",
            )?;
        }
        loop {
            let answer = p.ask("Context window the engine serves, in tokens", Some("32768"))?;
            match answer.replace(['_', ','], "").parse::<u64>() {
                Ok(ctx) if ctx > 0 => break ctx,
                _ => p.say("  Type a number of tokens, like 65536.")?,
            }
        }
    };
    let floor = tenaz_core::params::RunParams::default().context_floor;
    if context_window < floor {
        p.say(&format!(
            "  Warning: below {floor} tokens the sessions will not fit. {}",
            doctor::context_fix(engine.map_or("", |e| e.provider.as_str()), floor)
        ))?;
    }
    Ok(context_window)
}

/// The image engine: a server with the `OpenAI` Images API that lists an
/// image model (the LLM's own engine first), else `ComfyUI` when it is given
/// or running, with a workflow for it. For a generic engine, also the
/// commands that free the memory while images render on another server.
async fn ask_media<R: BufRead, W: Write>(
    args: &InitArgs,
    llm: ModelEngine<'_>,
    client: &reqwest::Client,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<Media> {
    let (images, image_model) = ask_image_server(args, llm.base_url, client, p).await?;
    let (comfyui, workflows) = ask_comfyui(args, images.is_some(), client, p)
        .await?
        .unzip();
    let workflows = workflows.unwrap_or_default();
    let mut media = Media {
        comfyui,
        workflows,
        images,
        image_model,
        engine_start: args.engine_start.clone(),
        engine_stop: args.engine_stop.clone(),
    };
    ask_engine_commands(&mut media, llm, p)?;
    Ok(media)
}

/// A server with the `OpenAI` Images API and its image model: the flags',
/// else one found and accepted, the LLM's own engine first.
async fn ask_image_server<R: BufRead, W: Write>(
    args: &InitArgs,
    llm_base_url: Option<&str>,
    client: &reqwest::Client,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let mut images = args.images.clone();
    let mut image_model = args.image_model.clone();
    if images.is_none() && args.comfyui.is_none() {
        let engines = engines_with(llm_base_url, client).await;
        let servers = detect::image_servers(client, &engines).await;
        for server in detect::image_offers(servers, llm_base_url) {
            let model = server.models[0].clone();
            if p.confirm(
                &format!(
                    "{} at {} serves the image model {model}. Give the agent an image tool through it?",
                    server.label, server.url
                ),
                true,
            )? {
                images = Some(server.url);
                image_model = Some(model);
                break;
            }
        }
    }
    if images.is_some() && image_model.is_none() {
        image_model = Some(p.ask("Image model to ask it for", None)?);
    }
    Ok((images, image_model))
}

/// The engines on the usual ports, and the LLM's own at `llm_base_url`
/// when it is elsewhere and answers.
async fn engines_with(llm_base_url: Option<&str>, client: &reqwest::Client) -> Vec<Engine> {
    let mut engines = detect::local_engines(client).await;
    let Some(url) = llm_base_url else {
        return engines;
    };
    if engines
        .iter()
        .any(|e| e.base_url == detect::normalize_base_url(url))
    {
        return engines;
    }
    if let Some(engine) = detect::engine_at(client, url).await {
        engines.push(engine);
    }
    engines
}

/// `ComfyUI` and its workflows: the flags', else the local one when it
/// runs, the user wants it and gives a workflow; `None` without one, and
/// when `has_images`.
async fn ask_comfyui<R: BufRead, W: Write>(
    args: &InitArgs,
    has_images: bool,
    client: &reqwest::Client,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<Option<(String, Vec<String>)>> {
    let comfyui = match &args.comfyui {
        Some(url) => Some(url.clone()),
        None if has_images => None,
        None => offer_local_comfyui(client, p).await?,
    };
    let Some(comfyui) = comfyui else {
        return Ok(None);
    };
    let mut workflows = args.workflows.clone();
    if workflows.is_empty() {
        let Some(workflow) = ask_workflow(p)? else {
            return Ok(None);
        };
        workflows.push(workflow);
    }
    Ok(Some((comfyui, workflows)))
}

/// The local `ComfyUI` when it runs and the user wants it.
async fn offer_local_comfyui<R: BufRead, W: Write>(
    client: &reqwest::Client,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<Option<String>> {
    let local = detect::COMFYUI_URL;
    if !detect::probing() || !detect::comfyui_at(client, local).await {
        return Ok(None);
    }
    let wanted = p.confirm(
        &format!("ComfyUI is running at {local}. Give the agent an image tool?"),
        true,
    )?;
    Ok(wanted.then(|| local.to_owned()))
}

/// An API-format `ComfyUI` workflow that loads, as `default=<path>`; `None`
/// when the answer is empty (no image tool).
fn ask_workflow<R: BufRead, W: Write>(p: &mut Prompter<R, W>) -> anyhow::Result<Option<String>> {
    loop {
        let answer = p.ask(
            "API-format ComfyUI workflow for images (empty: no image tool)",
            Some(""),
        )?;
        if answer.is_empty() {
            return Ok(None);
        }
        match tenaz_mcp::workflow::WorkflowTemplate::from_file(Path::new(&answer)) {
            Ok(_) => return Ok(Some(format!("default={answer}"))),
            Err(e) => p.say(&format!("  Cannot use it: {e}"))?,
        }
    }
}

/// Whether the engine has to be stopped while images render: a generic
/// engine with images on another server, and no commands given for it.
fn needs_engine_commands(media: &Media, llm: ModelEngine<'_>) -> bool {
    if llm.api != EngineApi::Generic {
        return false;
    }
    let given = media.engine_start.is_some() || media.engine_stop.is_some();
    !given && renders_elsewhere(media, llm)
}

/// Images render on `ComfyUI`, or on an images server other than `llm`.
fn renders_elsewhere(media: &Media, llm: ModelEngine<'_>) -> bool {
    media.comfyui.is_some() || media.images.as_deref().is_some_and(|url| !llm.serves(url))
}

/// For a generic engine whose images render on another server, the
/// commands that stop and start it to free the memory, unless given.
fn ask_engine_commands<R: BufRead, W: Write>(
    media: &mut Media,
    llm: ModelEngine<'_>,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<()> {
    if !needs_engine_commands(media, llm) {
        return Ok(());
    }
    p.say(
        "To free the memory for images, Tenaz stops the engine and starts it again after. \
         Leave both empty to keep the model loaded next to the image model, \
         if the memory holds both.",
    )?;
    media.engine_stop =
        Some(p.ask("Command that stops the engine", Some(""))?).filter(|s| !s.is_empty());
    media.engine_start =
        Some(p.ask("Command that starts the engine", Some(""))?).filter(|s| !s.is_empty());
    Ok(())
}

/// The answers, and the `tenaz init` line that gives them without questions.
fn show<R: BufRead, W: Write>(
    plan: &InitPlan,
    goal: &Goal,
    engine: Option<&Engine>,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<()> {
    let goal_text = match goal {
        Goal::InProject => plan
            .project
            .join(tenaz_core::project::BRIEF)
            .display()
            .to_string(),
        Goal::File(path) => path.display().to_string(),
        Goal::Text(text) => text.clone(),
    };
    p.say("")?;
    p.say(&format!("  Project   {}", plan.project.display()))?;
    p.say(&format!("  Goal      {goal_text}"))?;
    p.say(&format!("  Agent     {}", plan.harness))?;
    if let Some(engine) = engine {
        p.say(&format!(
            "  Engine    {} at {}",
            engine.label, engine.base_url
        ))?;
    }
    p.say(&format!(
        "  Model     {}/{}, {} tokens{}",
        plan.provider,
        plan.model,
        plan.args.context_window.unwrap_or_default(),
        if plan.args.vision {
            ", reads images"
        } else {
            ""
        }
    ))?;
    if let Some(url) = &plan.args.comfyui {
        p.say(&format!("  Images    ComfyUI at {url}"))?;
    }
    if let (Some(url), Some(model)) = (&plan.args.images, &plan.args.image_model) {
        p.say(&format!("  Images    {model} at {url}"))?;
    }
    let flags = InitArgs {
        brief: match goal {
            Goal::Text(_) | Goal::InProject => Some(plan.project.join(tenaz_core::project::BRIEF)),
            Goal::File(path) => Some(path.clone()),
        },
        provider: Some(plan.provider.clone()),
        engine: Some(EngineKind::from_api(plan.api)),
        interactive: false,
        ..plan.args.clone()
    };
    p.say("\nThe same without questions:")?;
    p.say(&format!("  {}\n", flags.command_line()))?;
    Ok(())
}

/// Offers the engines found on the usual ports, or asks for an address.
async fn choose_engine<R: BufRead, W: Write>(
    client: &reqwest::Client,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<Option<Engine>> {
    let found = detect::local_engines(client).await;
    if found.is_empty() {
        let ports: Vec<String> = detect::ENGINE_PORTS
            .iter()
            .map(|(port, label, _)| format!("{port} {label}"))
            .collect();
        p.say(&format!(
            "No engine answers on the usual ports ({}).",
            ports.join(", ")
        ))?;
        for line in doctor::engine_fix() {
            p.say(&format!("  {line}"))?;
        }
        p.say("Start it now and type its address, or type the address it will have.")?;
    } else {
        let mut options: Vec<String> = found
            .iter()
            .map(|e| {
                let n = e.models.len();
                format!(
                    "{} at {} ({n} model{})",
                    e.label,
                    e.base_url,
                    if n == 1 { "" } else { "s" }
                )
            })
            .collect();
        options.push("Another address".to_owned());
        let i = p.choose("Which engine serves the model?", &options, 0)?;
        if let Some(engine) = found.get(i) {
            return Ok(Some(engine.clone()));
        }
    }
    loop {
        let url =
            detect::normalize_base_url(&p.ask("Engine address", Some("http://127.0.0.1:8080/v1"))?);
        if let Some(engine) = detect::engine_at(client, &url).await {
            return Ok(Some(engine));
        }
        if p.confirm(&format!("Nothing answers at {url}. Use it anyway?"), false)? {
            return Ok(Some(unanswered_engine(url)));
        }
    }
}

/// The engine at `url`, which does not answer yet: its API and provider
/// are the ones usual for its address.
fn unanswered_engine(url: String) -> Engine {
    Engine {
        label: "An engine not answering yet".to_owned(),
        provider: default_provider(None, Some(&url)),
        api: guess_api(Some(&url)),
        base_url: url,
        models: Vec::new(),
    }
}

/// How many visible files and folders `dir` holds, when it is a folder the
/// user could mean as the project: not empty, not already a Tenaz run, and
/// neither the home folder nor the filesystem root.
fn here_entries(dir: &Path) -> Option<usize> {
    if dir.parent().is_none()
        || std::env::home_dir().is_some_and(|home| home == dir)
        || dir.join(".tenaz/run.json").exists()
    {
        return None;
    }
    let count = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .count();
    (count > 0).then_some(count)
}

/// A directory name from the first words of the goal.
fn slug(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(4)
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        "tenaz-project".to_owned()
    } else {
        words.join("-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompter(input: &str) -> Prompter<&[u8], Vec<u8>> {
        Prompter::new(input.as_bytes(), Vec::new())
    }

    #[test]
    fn choose_takes_the_default_on_enter_and_asks_again_on_nonsense() {
        let options = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(prompter("\n").choose("?", &options, 1).unwrap(), 1);
        let mut p = prompter("9\nx\n3\n");
        assert_eq!(p.choose("?", &options, 0).unwrap(), 2);
        let shown = String::from_utf8(p.output).unwrap();
        assert_eq!(shown.matches("Type a number from 1 to 3").count(), 2);
    }

    #[test]
    fn ask_needs_an_answer_without_a_default() {
        assert_eq!(prompter("\nhello\n").ask("?", None).unwrap(), "hello");
        assert_eq!(prompter("\n").ask("?", Some("d")).unwrap(), "d");
        assert!(prompter("").ask("?", None).is_err());
    }

    #[test]
    fn confirm_reads_yes_and_no() {
        assert!(prompter("\n").confirm("?", true).unwrap());
        assert!(!prompter("maybe\nn\n").confirm("?", true).unwrap());
        assert!(prompter("Y\n").confirm("?", false).unwrap());
    }

    #[test]
    fn an_engine_not_answering_yet_is_named_after_its_port() {
        let engine = unanswered_engine("http://127.0.0.1:8080/v1".to_owned());
        assert_eq!(
            (engine.provider.as_str(), engine.api),
            ("llamacpp", EngineApi::Generic)
        );
        assert_eq!(
            unanswered_engine("http://127.0.0.1:9999/v1".to_owned()).provider,
            "local"
        );
    }

    #[test]
    fn slugs_come_from_the_first_words() {
        assert_eq!(
            slug("Build a CLI for my notes, with tests"),
            "build-a-cli-for"
        );
        assert_eq!(slug("¿?"), "tenaz-project");
    }
}
