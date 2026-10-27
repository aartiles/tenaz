//! Stamps the binary with the git commit it was built from, when it is built
//! from a git checkout, so `run.json` can tell builds of one version apart.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn main() {
    // Rebuild the stamp when HEAD moves: a new commit, a checkout or a reset.
    for path in ["HEAD", "packed-refs"] {
        if let Some(file) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={file}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(file) = git(&["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={file}");
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let stamped = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(commit) => format!("{version} ({commit})"),
        None => version,
    };
    println!("cargo:rustc-env=TENAZ_VERSION={stamped}");
}
