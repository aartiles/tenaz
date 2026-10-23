//! `tenaz doctor`: what a real run needs on this machine (git, a coding
//! agent, an engine and a model with enough context), what is missing, and
//! how to install or fix it. The CLI finds the facts; this module only
//! judges them and words the advice, which the setup wizard reuses.

use std::fmt::Write as _;

/// The operating systems the advice differs for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Other,
}

impl Os {
    /// From `std::env::consts::OS`.
    pub fn from_name(name: &str) -> Self {
        match name {
            "macos" => Self::MacOs,
            "linux" => Self::Linux,
            _ => Self::Other,
        }
    }
}

/// A program found on `PATH`, with the first line of its `--version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    pub version: String,
}

/// An engine that answered, with the models it lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineSeen {
    /// What to call it (`LM Studio`, `Ollama`, ...).
    pub label: String,
    /// The provider name (`lmstudio`, `ollama`, ...), which picks the advice.
    pub provider: String,
    pub base_url: String,
    pub models: Vec<ModelSeen>,
}

/// One model an engine lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSeen {
    pub id: String,
    pub context_window: Option<u64>,
}

/// What was found on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub os: Os,
    /// `git --version`, or `None` when git is not on `PATH`.
    pub git: Option<String>,
    pub agents: Vec<Tool>,
    pub engines: Vec<EngineSeen>,
    /// Where engines were looked for, as shown to the user (`1234 LM Studio`).
    pub looked_at: Vec<String>,
}

/// How one check came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Usable, but worth a look.
    Warn,
    /// A real run cannot start without it.
    Missing,
}

/// One line of the report and, when it is not ok, what to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// `git`, `agent`, `engine` or `model`.
    pub what: &'static str,
    pub level: Level,
    pub found: String,
    pub fix: Vec<String>,
}

/// Every check, in the order to fix them.
pub fn examine(machine: &Machine, context_floor: u64) -> Vec<Check> {
    vec![
        git_check(machine),
        agent_check(machine),
        engine_check(machine),
        model_check(machine, context_floor),
    ]
}

/// Whether a real run can start: nothing is missing.
pub fn ready(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.level != Level::Missing)
}

/// How to install git.
pub fn git_fix(os: Os) -> Vec<String> {
    match os {
        Os::MacOs => vec![
            "Install Apple's command line tools, which include git: xcode-select --install"
                .to_owned(),
        ],
        Os::Linux | Os::Other => vec![
            "Install git with your system's package manager, for example: sudo apt install git"
                .to_owned(),
        ],
    }
}

/// How to install a coding agent. Pi's installer also installs Node.js
/// when it is missing.
pub fn agent_fix() -> Vec<String> {
    vec![
        "Install one of the coding agents Tenaz drives:".to_owned(),
        "  Pi:  curl -fsSL https://pi.dev/install.sh | sh".to_owned(),
        "  fx:  curl -fsSL https://fx.sh/setup.sh | bash".to_owned(),
        "Then open a new terminal so it is on PATH.".to_owned(),
    ]
}

/// How to get an engine running.
pub fn engine_fix() -> Vec<String> {
    vec![
        "Start an engine that serves a model over an OpenAI-compatible API. The easiest:"
            .to_owned(),
        "  LM Studio (https://lmstudio.ai): download a model, load it, and start the server \
         in the Developer tab (it listens on port 1234)."
            .to_owned(),
        "  Ollama (https://ollama.com): ollama pull <model>, then \
         OLLAMA_CONTEXT_LENGTH=32768 ollama serve"
            .to_owned(),
        "Pick a model that calls tools (most coding models do) and fits in your memory.".to_owned(),
    ]
}

/// How to give the model at least `floor` tokens of context on the engine
/// `provider` names.
pub fn context_fix(provider: &str, floor: u64) -> String {
    match provider {
        "lmstudio" => format!(
            "In LM Studio, load the model again with Context Length set to {floor} or more."
        ),
        "ollama" => format!(
            "Start Ollama with OLLAMA_CONTEXT_LENGTH={floor} (or more): \
             OLLAMA_CONTEXT_LENGTH={floor} ollama serve"
        ),
        "llamacpp" => format!("Start llama-server with -c {floor} (or more)."),
        "vllm" => format!("Start vLLM with --max-model-len {floor} (or more)."),
        _ => {
            format!("Give the model a context of {floor} tokens or more in the engine's settings.")
        }
    }
}

fn git_check(machine: &Machine) -> Check {
    match &machine.git {
        Some(version) => ok("git", version.clone()),
        None => Check {
            what: "git",
            level: Level::Missing,
            found: "git is not on PATH".to_owned(),
            fix: git_fix(machine.os),
        },
    }
}

fn agent_check(machine: &Machine) -> Check {
    if machine.agents.is_empty() {
        return Check {
            what: "agent",
            level: Level::Missing,
            found: "neither Pi nor fx is on PATH".to_owned(),
            fix: agent_fix(),
        };
    }
    let found: Vec<String> = machine
        .agents
        .iter()
        .map(|a| format!("{} ({})", a.name, a.version))
        .collect();
    ok("agent", found.join(", "))
}

fn engine_check(machine: &Machine) -> Check {
    if machine.engines.is_empty() {
        return Check {
            what: "engine",
            level: Level::Missing,
            found: format!(
                "nothing answers on the usual ports ({})",
                machine.looked_at.join(", ")
            ),
            fix: engine_fix(),
        };
    }
    let found: Vec<String> = machine
        .engines
        .iter()
        .map(|e| format!("{} at {}", e.label, e.base_url))
        .collect();
    ok("engine", found.join(", "))
}

/// Missing when no engine lists a model, or every model's context is known
/// and too small. A model whose context the engine does not report is a
/// warning: the wizard asks for it.
fn model_check(machine: &Machine, floor: u64) -> Check {
    let missing = |found: String, fix: Vec<String>| Check {
        what: "model",
        level: Level::Missing,
        found,
        fix,
    };
    let Some(first) = machine.engines.first() else {
        return missing("no engine to ask".to_owned(), Vec::new());
    };
    let models: Vec<(&EngineSeen, &ModelSeen)> = machine
        .engines
        .iter()
        .flat_map(|e| e.models.iter().map(move |m| (e, m)))
        .collect();
    if models.is_empty() {
        return missing(
            format!("{} lists no model", first.label),
            vec!["Load a model in the engine (one that calls tools).".to_owned()],
        );
    }
    let big_enough = models
        .iter()
        .find(|(_, m)| m.context_window.is_some_and(|c| c >= floor));
    if let Some((_, model)) = big_enough {
        let ctx = model.context_window.unwrap_or_default();
        let others = models.len() - 1;
        let more = if others == 0 {
            String::new()
        } else {
            format!(" (and {others} more)")
        };
        return Check {
            what: "model",
            level: Level::Ok,
            found: format!("{}, {ctx} tokens of context{more}", model.id),
            fix: vec!["tenaz run checks that it calls tools before the first session.".to_owned()],
        };
    }
    if let Some((engine, model)) = models.iter().find(|(_, m)| m.context_window.is_none()) {
        return Check {
            what: "model",
            level: Level::Warn,
            found: format!("{}, context not reported by {}", model.id, engine.label),
            fix: vec![
                format!("Make sure the engine serves at least {floor} tokens of context."),
                context_fix(&engine.provider, floor),
            ],
        };
    }
    let (engine, model) = models[0];
    missing(
        format!(
            "{}, {} tokens of context; Tenaz needs at least {floor}",
            model.id,
            model.context_window.unwrap_or_default()
        ),
        vec![context_fix(&engine.provider, floor)],
    )
}

fn ok(what: &'static str, found: String) -> Check {
    Check {
        what,
        level: Level::Ok,
        found,
        fix: Vec::new(),
    }
}

/// The report as `tenaz doctor` prints it, ending with what to do next.
pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let level = match check.level {
            Level::Ok => "ok",
            Level::Warn => "check",
            Level::Missing => "missing",
        };
        let _ = writeln!(out, "  {level:<8} {:<7} {}", check.what, check.found);
        for line in &check.fix {
            let _ = writeln!(out, "  {:<8} {:<7} {line}", "", "");
        }
    }
    out.push('\n');
    if ready(checks) {
        out.push_str("Ready. Next: tenaz init, in the folder where the work should happen.\n");
    } else {
        out.push_str("Not ready yet. Fix what is missing, then run tenaz doctor again.\n");
        out.push_str("Meanwhile, tenaz demo shows a whole run without a model or an agent.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: u64 = 32768;

    fn machine() -> Machine {
        Machine {
            os: Os::MacOs,
            git: Some("git version 2.54.0".to_owned()),
            agents: vec![Tool {
                name: "pi".to_owned(),
                version: "1.1.0".to_owned(),
            }],
            engines: vec![engine("lmstudio", &[("qwen", Some(65536))])],
            looked_at: vec!["1234 LM Studio".to_owned()],
        }
    }

    fn engine(provider: &str, models: &[(&str, Option<u64>)]) -> EngineSeen {
        EngineSeen {
            label: provider.to_owned(),
            provider: provider.to_owned(),
            base_url: "http://127.0.0.1:1234/v1".to_owned(),
            models: models
                .iter()
                .map(|(id, ctx)| ModelSeen {
                    id: (*id).to_owned(),
                    context_window: *ctx,
                })
                .collect(),
        }
    }

    fn level(checks: &[Check], what: &str) -> Level {
        checks.iter().find(|c| c.what == what).unwrap().level
    }

    #[test]
    fn a_machine_with_everything_is_ready() {
        let checks = examine(&machine(), FLOOR);
        assert!(checks.iter().all(|c| c.level == Level::Ok));
        assert!(ready(&checks));
        assert!(render(&checks).contains("Ready. Next: tenaz init"));
    }

    #[test]
    fn missing_git_and_agent_say_how_to_install_them() {
        let mut m = machine();
        m.git = None;
        m.agents.clear();
        let checks = examine(&m, FLOOR);
        assert!(!ready(&checks));
        let text = render(&checks);
        assert!(text.contains("xcode-select --install"));
        assert!(text.contains("https://pi.dev/install.sh"));
        assert!(text.contains("Not ready yet"));
        assert!(text.contains("tenaz demo"));
        m.os = Os::Linux;
        assert!(render(&examine(&m, FLOOR)).contains("sudo apt install git"));
    }

    #[test]
    fn no_engine_means_no_model_either() {
        let mut m = machine();
        m.engines.clear();
        let checks = examine(&m, FLOOR);
        assert_eq!(level(&checks, "engine"), Level::Missing);
        assert_eq!(level(&checks, "model"), Level::Missing);
        let text = render(&checks);
        assert!(text.contains("1234 LM Studio"));
        assert!(text.contains("https://lmstudio.ai"));
    }

    #[test]
    fn an_engine_without_models_is_missing_a_model() {
        let mut m = machine();
        m.engines = vec![engine("ollama", &[])];
        let checks = examine(&m, FLOOR);
        assert_eq!(level(&checks, "engine"), Level::Ok);
        assert_eq!(level(&checks, "model"), Level::Missing);
    }

    #[test]
    fn one_model_with_enough_context_is_enough() {
        let mut m = machine();
        m.engines = vec![engine(
            "lmstudio",
            &[("small", Some(8192)), ("big", Some(FLOOR))],
        )];
        let check = &examine(&m, FLOOR)[3];
        assert_eq!(check.level, Level::Ok);
        assert!(check.found.starts_with("big, 32768 tokens"));
        assert!(check.found.contains("(and 1 more)"));
    }

    #[test]
    fn a_small_context_is_missing_with_the_engine_specific_fix() {
        let mut m = machine();
        m.engines = vec![engine("ollama", &[("qwen", Some(4096))])];
        let check = &examine(&m, FLOOR)[3];
        assert_eq!(check.level, Level::Missing);
        assert!(check.fix[0].contains("OLLAMA_CONTEXT_LENGTH=32768"));
        m.engines = vec![engine("llamacpp", &[("qwen", Some(4096))])];
        assert!(examine(&m, FLOOR)[3].fix[0].contains("-c 32768"));
    }

    #[test]
    fn an_unreported_context_is_a_warning_not_a_blocker() {
        let mut m = machine();
        m.engines = vec![engine("ollama", &[("qwen", None)])];
        let checks = examine(&m, FLOOR);
        assert_eq!(level(&checks, "model"), Level::Warn);
        assert!(ready(&checks));
    }

    #[test]
    fn os_names_come_from_the_standard_library() {
        assert_eq!(Os::from_name("macos"), Os::MacOs);
        assert_eq!(Os::from_name("linux"), Os::Linux);
        assert_eq!(Os::from_name("windows"), Os::Other);
    }
}
