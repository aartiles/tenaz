//! What `verify` and heavy jobs share when Tenaz starts them: the command
//! as a model would see it run, in a process group of its own, and the
//! group killed when it ends.

use std::path::Path;
use std::process::Stdio;

/// `program args` through `wrapper` (the sandbox launcher, or nothing),
/// from `root`, with `CI=1` and `NO_COLOR=1`, no stdin, in a process group
/// of its own, killed when dropped. The caller sets stdout and stderr.
pub(crate) fn command(
    root: &Path,
    wrapper: &[String],
    program: &Path,
    args: Vec<String>,
) -> tokio::process::Command {
    let (program, args) = tenaz_core::util::wrap_command(wrapper, program, args);
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env("CI", "1")
        // `CI` turns colours on in some tools (picocolors, so Vite); a
        // check that reads their output would see escape codes the
        // builder's shell never shows it.
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    command
}

/// Kills `child` if it still runs, and whatever it left running in its
/// process group: a server a check started, a render a job left behind.
pub(crate) async fn reap(child: &mut tokio::process::Child, group: Option<u32>) {
    let _ = child.kill().await;
    if let Some(group) = group {
        kill_group(group).await;
    }
}

/// Kills process group `group`. Through `kill(1)`, as the crate has no
/// unsafe code; a group already empty is no error.
async fn kill_group(group: u32) {
    let _ = tokio::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{group}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}
