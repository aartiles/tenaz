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
        "--swap-models",
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
         --media-timeout 60 --vision --no-proxy --swap-models --worktree ../wt --name slug"
    );
}

#[test]
fn init_pins_the_mcp_servers_and_gives_them_back_on_its_command_line() {
    let line = with(&[
        "--mcp",
        "blender=http://127.0.0.1:9191/",
        "--mcp",
        "docs=https://d/mcp",
    ]);
    let pinned = launch(&plan(&line, None)).unwrap();
    assert_eq!(pinned.mcp_servers.len(), 2);
    assert_eq!(pinned.mcp_servers["blender"], "http://127.0.0.1:9191/");
    let command = init_args(&line).command_line();
    assert!(
        command.ends_with("--mcp blender=http://127.0.0.1:9191/ --mcp docs=https://d/mcp"),
        "{command}"
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
    const BINS: [&str; 12] = [
        "--fx-bin",
        "/opt/fx",
        "--pi-bin",
        "/opt/pi",
        "--opencode-bin",
        "/opt/opencode",
        "--qwen-bin",
        "/opt/qwen",
        "--dsh-bin",
        "/opt/dsh",
        "--acp-bin",
        "/opt/acp",
    ];
    let fx = plan(&with(&BINS), None);
    assert_eq!(
        (fx.harness, fx.harness_bin),
        (HarnessKind::Fx, Some(PathBuf::from("/opt/fx")))
    );
    for (name, kind) in [
        ("pi", HarnessKind::Pi),
        ("opencode", HarnessKind::OpenCode),
        ("qwen", HarnessKind::Qwen),
        ("dsh", HarnessKind::Dsh),
        ("acp", HarnessKind::Acp),
    ] {
        let line = with(&[&["--harness", name][..], &BINS].concat());
        let got = plan(&line, None);
        assert_eq!(
            (got.harness, got.harness_bin),
            (kind, Some(PathBuf::from(format!("/opt/{name}"))))
        );
    }
}

#[test]
fn the_generic_agent_needs_its_command() {
    let error = pin_harness(HarnessKind::Acp, None, "engine").unwrap_err();
    assert_eq!(error.to_string(), "--harness acp needs --acp-bin <command>");
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

#[test]
fn the_command_line_gives_the_role_models_back_after_the_context_window() {
    let args = init_args(&with(&[
        "--context-window",
        "65536",
        "--model-for",
        "reviewer=r",
        "--model-for",
        "split=s",
        "--context-window-for",
        "reviewer=32768",
        "--thinking",
        "high",
    ]));
    assert_eq!(
        args.command_line(),
        "tenaz init --project p --model m --context-window 65536 \
         --model-for reviewer=r --model-for split=s --context-window-for reviewer=32768 \
         --thinking high"
    );
}

fn plan_error(extra: &[&'static str]) -> String {
    InitPlan::from_args(init_args(&with(extra)), None, None)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

/// A bad `--mcp`, or one the harness would not see, stops `init`
/// before it creates anything.
#[test]
fn the_plan_refuses_a_bad_mcp_server_or_one_pi_and_dsh_would_not_see() {
    assert!(plan_error(&["--mcp", "blender"]).contains("name=url"));
    for harness in ["pi", "dsh"] {
        let why = plan_error(&["--harness", harness, "--mcp", "b=http://h/"]);
        assert!(
            why.starts_with(&format!("{harness} is not given")) && why.contains("drop --mcp"),
            "{why}"
        );
    }
    assert_eq!(plan_error(&["--mcp", "b=http://h/"]), "");
}

#[test]
fn a_plan_keeps_the_role_picks() {
    let line = with(&[
        "--model-for",
        "reviewer=r",
        "--context-window-for",
        "reviewer=32768",
    ]);
    assert_eq!(
        plan(&line, None).role_picks.into_iter().collect::<Vec<_>>(),
        vec![(
            Role::Reviewer,
            RolePick {
                id: "r".into(),
                context_window: Some(32768),
            }
        )]
    );
}

/// The user's Pi `models` file, with `critic` under the run's provider
/// and another window under another provider.
fn listed(dir: &Path) -> Listed {
    let path = dir.join("models.json");
    let file = r#"{"providers": {
            "other": {"models": [{"id": "critic", "contextWindow": 8192}]},
            "sushi": {"models": [{"id": "critic", "contextWindow": 32768, "input": ["text"]}]}
        }}"#;
    std::fs::write(&path, file).unwrap();
    Listed::read(Some(&path), "sushi").unwrap()
}

/// With Pi, a role's model takes the window and inputs the user's
/// `models.json` lists for it under the run's provider (ADR 0011).
#[test]
fn pi_s_models_file_says_a_role_s_window_and_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let known = listed(dir.path()).known("critic", false);
    let expected = Known {
        context_window: Some(32768),
        input: Some(vec!["text".to_owned()]),
    };
    assert_eq!(known, expected);
}

/// `--vision` keeps the run's inputs; a model the file does not list,
/// or no file, says nothing.
#[test]
fn pi_s_models_file_says_nothing_of_what_it_does_not_list() {
    let dir = tempfile::tempdir().unwrap();
    let listed = listed(dir.path());
    assert_eq!(listed.known("critic", true).input, None);
    assert_eq!(listed.known("unlisted", false), Known::default());
    let none = Listed::read(None, "sushi").unwrap();
    assert_eq!(none.known("critic", false), Known::default());
}

#[test]
fn swapping_is_refused_without_a_map_or_with_a_generic_engine() {
    assert!(plan_error(&["--swap-models"]).contains("needs a --model-for map"));
    let generic = [
        "--swap-models",
        "--model-for",
        "reviewer=r",
        "--engine",
        "generic",
    ];
    assert!(plan_error(&generic).contains("needs --engine load-api"));
}

/// The picks are checked against `--model` as given.
#[test]
fn a_role_pick_of_the_run_s_own_model_is_refused() {
    assert_eq!(
        plan_error(&["--model-for", "reviewer=m"]),
        "--model-for reviewer=m names the run's --model; leave it out"
    );
}
