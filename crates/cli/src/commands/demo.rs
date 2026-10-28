//! `tenaz demo`: a whole run with the scripted harness.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser as _;

use super::init::{init_from_plan, plan_from_flags};
use super::run;
use crate::{Cli, Command};

/// The goal `tenaz demo` gives the scripted harness.
const DEMO_GOAL: &str = "Build four trivial files.";

/// `tenaz demo`: a whole run with the scripted harness, which needs no
/// agent and no engine, in a new directory; then what to look at.
pub(crate) async fn execute(project: PathBuf) -> anyhow::Result<ExitCode> {
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
    init_from_plan(plan)?;
    let Command::Run(args) = Cli::try_parse_from(["tenaz", "run", "--project", &dir])?.command
    else {
        anyhow::bail!("the demo's run line did not parse as run");
    };
    let code = run::execute(args).await?;
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
