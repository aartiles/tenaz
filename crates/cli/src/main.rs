//! `tenaz`: the single-model runner command line.

mod commands;
mod detect;
mod watch;
mod wiring;
mod wizard;
mod worktree;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use commands::finish::FinishArgs;
use commands::init::InitArgs;
use commands::run::RunArgs;
use commands::shims::SandboxExecArgs;

/// The version and, for a build from a git checkout, its commit
/// (`0.1.0 (816e1d8)`), set by `build.rs`; the commit tells builds of one
/// version apart.
const VERSION: &str = env!("TENAZ_VERSION");

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

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    dispatch(Cli::parse().command).await
}

/// Runs one command; those with nothing else to say exit with 0.
async fn dispatch(command: Command) -> anyhow::Result<ExitCode> {
    let done = |()| ExitCode::SUCCESS;
    match command {
        Command::Init(args) => commands::init::execute(args).await.map(done),
        Command::Run(args) => commands::run::execute(args).await,
        Command::Status { project, json } => commands::status::execute(project, json).map(done),
        Command::Finish(args) => commands::finish::execute(args).map(done),
        Command::Watch {
            level,
            history,
            once,
            project,
        } => {
            let options = watch::WatchOptions {
                level,
                history,
                once,
            };
            commands::watch::execute(project, &options).map(done)
        }
        Command::FxShim { command } => commands::shims::fx_shim(&command),
        Command::SandboxExec(args) => Ok(commands::shims::sandbox_exec(args)),
        Command::Guide => {
            commands::info::guide();
            Ok(ExitCode::SUCCESS)
        }
        Command::Demo { project } => commands::demo::execute(project).await,
        Command::Doctor { base_url } => Ok(commands::doctor::execute(base_url.as_deref()).await),
        Command::Params => commands::info::params().map(done),
    }
}
