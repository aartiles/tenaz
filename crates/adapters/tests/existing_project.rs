//! `init` in a repository that already has code, its own agent instructions
//! and its own `.gitignore`: everything Tenaz adds goes under `.tenaz/`.

use std::path::Path;
use std::process::Command;

use tenaz_adapters::git::GitRepo;
use tenaz_core::ledger::{Launch, ModelConfig};
use tenaz_core::orchestrator::{InitOptions, Templates, init};
use tenaz_core::params::RunParams;
use tenaz_core::project::Project;
use tenaz_core::testing::FastClock;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn init_leaves_the_projects_own_files_alone() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("app");
    let own = [
        ("AGENTS.md", "# House rules\n\nUse pnpm.\n"),
        ("CLAUDE.md", "See AGENTS.md.\n"),
        (".gitignore", "node_modules\n"),
        ("BRIEF.md", "An unrelated document of the project.\n"),
        ("src/main.js", "console.log(1);\n"),
    ];
    for (path, text) in own {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "app"]);

    let brief = work.path().join("goal.md");
    std::fs::write(&brief, "# Goal\n\nAdd a flag.\n").unwrap();
    let project = Project::new(&root);
    let templates =
        Templates::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/templates")).unwrap();
    let options = InitOptions {
        brief: Some(brief),
        config: ModelConfig {
            harness: "fake".into(),
            harness_version: "fake 0".into(),
            model: "fake/fake".into(),
            provider: Some("fake".into()),
            id: "fake".into(),
            base_url: None,
            context_window: 65536,
            input: vec!["text".into()],
            thinking: None,
        },
        launch: Launch::default(),
        params: RunParams::default(),
        skills: vec![],
        runner_version: "test".into(),
    };
    init(
        &project,
        &GitRepo::new(&root),
        &FastClock::new(1),
        &templates,
        options,
    )
    .unwrap();

    for (path, text) in own {
        assert_eq!(project.read(path).unwrap(), text, "{path} changed");
    }
    assert_eq!(project.brief().unwrap(), "# Goal\n\nAdd a flag.\n");
    assert_eq!(project.protocol().unwrap(), templates.protocol);
    let added = git(&root, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(
        added.lines().all(|p| p.starts_with(".tenaz/")),
        "init committed outside .tenaz/: {added}"
    );
    assert!(added.contains(".tenaz/.gitignore"), "{added}");
    assert!(!added.contains(".tenaz/run.json"), "{added}");
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
}
