//! `tenaz init` without a model, in a terminal: asks for what the flags did
//! not say, offering what [`crate::detect`] finds, and ends with the same
//! [`InitPlan`] the flags would have produced. Questions go to stderr and
//! answers come one per line, so the wizard also runs with a piped stdin.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::bail;
use tenaz_core::ledger::EngineApi;

use crate::detect::{self, Engine};
use crate::{HarnessKind, InitArgs, InitPlan, worktree};

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

/// The image tool and the commands that free memory for it.
struct Media {
    comfyui: Option<String>,
    workflows: Vec<String>,
    engine_start: Option<String>,
    engine_stop: Option<String>,
}

/// Asks for whatever `args` leaves open and returns the plan to init with.
pub async fn run<R: BufRead, W: Write>(
    args: &InitArgs,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<InitPlan> {
    p.say("Tenaz setup. Press Enter to take the [default].\n")?;
    let client = detect::client();
    let goal = ask_goal(args, p)?;
    let project = ask_project(args, &goal, p)?;
    let args = &ask_placement(args, &goal, &project, p)?;
    let harness = ask_agent(args, p)?;
    let choice = if harness == HarnessKind::Pi && args.model.is_none() {
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
    let api = args.engine.map_or_else(
        || {
            choice
                .engine
                .as_ref()
                .map_or_else(|| crate::guess_api(choice.base_url.as_deref()), |e| e.api)
        },
        crate::EngineKind::api,
    );
    let media = ask_media(args, api, &client, p).await?;
    let plan = InitPlan {
        brief: match &goal {
            Goal::File(path) => Some(path.clone()),
            Goal::InProject | Goal::Text(_) => None,
        },
        harness_bin: match harness {
            HarnessKind::Fx => args.fx_bin.clone(),
            HarnessKind::Pi => args.pi_bin.clone(),
            HarnessKind::Fake => None,
        },
        provider: choice
            .provider
            .clone()
            .or_else(|| choice.engine.as_ref().map(|e| e.provider.clone()))
            .unwrap_or_else(|| "local".to_owned()),
        project,
        harness,
        model,
        base_url: choice.base_url.clone(),
        context_window: Some(context_window),
        vision,
        api,
        engine_start: media.engine_start,
        engine_stop: media.engine_stop,
        comfyui: media.comfyui,
        workflows: media.workflows,
        brief_text: match &goal {
            Goal::Text(text) => Some(text.clone()),
            Goal::InProject | Goal::File(_) => None,
        },
        args: args.clone(),
    };
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
        &crate::absolute(project.to_path_buf()),
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
) -> anyhow::Result<HarnessKind> {
    if let Some(harness) = args.harness {
        return Ok(harness);
    }
    let agents = detect::agents();
    match agents.as_slice() {
        [] => bail!(
            "No coding agent found on PATH. Install fx (https://github.com/vercel-labs/fx) \
             or Pi (https://github.com/earendil-works/pi), then run tenaz init again."
        ),
        [only] => {
            p.say(&format!("Agent: {} ({})", only.name, only.version))?;
            Ok(harness_kind(only.name))
        }
        several => {
            let options: Vec<String> = several
                .iter()
                .map(|a| format!("{} ({})", a.name, a.version))
                .collect();
            let i = p.choose("Which coding agent should run the sessions?", &options, 0)?;
            Ok(harness_kind(several[i].name))
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
            "  Warning: below {floor} tokens the sessions will not fit; raise the engine's context if you can."
        ))?;
    }
    Ok(context_window)
}

/// `ComfyUI` when it is given or running, a workflow for it, and for a
/// generic engine the commands that free the memory while images render.
async fn ask_media<R: BufRead, W: Write>(
    args: &InitArgs,
    api: EngineApi,
    client: &reqwest::Client,
    p: &mut Prompter<R, W>,
) -> anyhow::Result<Media> {
    let mut comfyui = args.comfyui.clone();
    if comfyui.is_none() {
        let local = "http://127.0.0.1:8188";
        if detect::comfyui_at(client, local).await
            && p.confirm(
                &format!("ComfyUI is running at {local}. Give the agent an image tool?"),
                true,
            )?
        {
            comfyui = Some(local.to_owned());
        }
    }
    let mut workflows = args.workflows.clone();
    if comfyui.is_some() && workflows.is_empty() {
        loop {
            let answer = p.ask(
                "API-format ComfyUI workflow for images (empty: no image tool)",
                Some(""),
            )?;
            if answer.is_empty() {
                comfyui = None;
                break;
            }
            match tenaz_mcp::workflow::WorkflowTemplate::from_file(Path::new(&answer)) {
                Ok(_) => {
                    workflows.push(format!("default={answer}"));
                    break;
                }
                Err(e) => p.say(&format!("  Cannot use it: {e}"))?,
            }
        }
    }
    if comfyui.is_none() {
        workflows.clear();
    }
    let mut engine_start = args.engine_start.clone();
    let mut engine_stop = args.engine_stop.clone();
    if comfyui.is_some()
        && api == EngineApi::Generic
        && engine_start.is_none()
        && engine_stop.is_none()
    {
        p.say(
            "To free the memory for images, Tenaz stops the engine and starts it again after. \
             Leave both empty to keep the model loaded.",
        )?;
        engine_stop =
            Some(p.ask("Command that stops the engine", Some(""))?).filter(|s| !s.is_empty());
        engine_start =
            Some(p.ask("Command that starts the engine", Some(""))?).filter(|s| !s.is_empty());
    }
    Ok(Media {
        comfyui,
        workflows,
        engine_start,
        engine_stop,
    })
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
    p.say(&format!(
        "  Agent     {}",
        crate::harness_name(plan.harness)
    ))?;
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
        plan.context_window.unwrap_or_default(),
        if plan.vision { ", reads images" } else { "" }
    ))?;
    if let Some(url) = &plan.comfyui {
        p.say(&format!("  Images    ComfyUI at {url}"))?;
    }
    let flags = InitArgs {
        project: Some(plan.project.clone()),
        brief: match goal {
            Goal::Text(_) | Goal::InProject => Some(plan.project.join(tenaz_core::project::BRIEF)),
            Goal::File(path) => Some(path.clone()),
        },
        harness: Some(plan.harness),
        model: Some(plan.model.clone()),
        provider: Some(plan.provider.clone()),
        base_url: plan.base_url.clone(),
        context_window: plan.context_window,
        vision: plan.vision,
        engine: Some(crate::EngineKind::from_api(plan.api)),
        engine_start: plan.engine_start.clone(),
        engine_stop: plan.engine_stop.clone(),
        comfyui: plan.comfyui.clone(),
        workflows: plan.workflows.clone(),
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
            "No engine answers on the usual ports ({}). Start yours, or type its address.",
            ports.join(", ")
        ))?;
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
            return Ok(Some(Engine {
                label: "An engine not answering yet".to_owned(),
                provider: "local".to_owned(),
                api: crate::guess_api(Some(&url)),
                base_url: url,
                models: Vec::new(),
            }));
        }
    }
}

fn harness_kind(name: &str) -> HarnessKind {
    if name == "pi" {
        HarnessKind::Pi
    } else {
        HarnessKind::Fx
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
    fn slugs_come_from_the_first_words() {
        assert_eq!(
            slug("Build a CLI for my notes, with tests"),
            "build-a-cli-for"
        );
        assert_eq!(slug("¿?"), "tenaz-project");
    }
}
