//! What `verify` and heavy jobs share when Tenaz starts them: the command
//! as a model would see it run, in a process group of its own, and the
//! group killed when it ends, with what the command left running outside
//! it (ADR 0031).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tenaz_core::leftover::{self, ProcessRow, Started};
use tenaz_core::ports::ProcessTable;

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

/// A command Tenaz started from `root`, with its process group and when
/// it started.
pub(crate) struct Spawned {
    child: tokio::process::Child,
    group: Option<u32>,
    root: PathBuf,
    at: Instant,
}

impl Spawned {
    pub(crate) fn child(&mut self) -> &mut tokio::process::Child {
        &mut self.child
    }
}

/// Starts `command`, made by [`command`], noting the time first: what it
/// leaves behind is born after that (ADR 0031). Its root is the command's
/// working directory; a command without one is refused, as the search
/// for what it left would have no project to keep to.
pub(crate) fn spawn(command: &mut tokio::process::Command) -> std::io::Result<Spawned> {
    let root = command
        .as_std()
        .get_current_dir()
        .map(Path::to_path_buf)
        .ok_or_else(|| std::io::Error::other("no working directory to run from"))?;
    let at = Instant::now();
    let child = command.spawn()?;
    Ok(Spawned {
        group: child.id(),
        child,
        root,
        at,
    })
}

/// How long finding what a command left behind may take: `ps` and
/// `lsof` can hang (`lsof` on a stale network mount), and the run waits.
const SWEEP_LIMIT: Duration = Duration::from_secs(10);

/// Kills the command if it still runs, and whatever it left running in
/// its process group: a server a check started, a render a job left
/// behind. Then stops what it left from its root in a session of its own
/// (ADR 0031), giving up after [`SWEEP_LIMIT`].
pub(crate) async fn reap(spawned: &mut Spawned) {
    let _ = spawned.child.kill().await;
    let Some(group) = spawned.group else { return };
    kill_group(group).await;
    let (root, at) = (spawned.root.clone(), spawned.at);
    let table = SystemProcesses {
        until: Instant::now() + SWEEP_LIMIT,
    };
    let swept = tokio::task::spawn_blocking(move || {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let started = Started {
            own: std::process::id(),
            group,
            root: &root,
        };
        let swept = leftover::sweep(&table, &started, || at.elapsed());
        (swept, table.out_of_time())
    })
    .await;
    match swept {
        Ok((swept, _)) if !swept.is_empty() => tracing::warn!(
            pids = ?swept,
            "stopped processes a command left running outside its process group"
        ),
        Ok((_, true)) => tracing::warn!("gave up looking for what a command left running"),
        _ => {}
    }
}

/// The machine's processes, through `ps`, `kill` and, on macOS, `lsof`,
/// until `until`: a command still running then is killed and read as
/// empty, and nothing is killed after it, as the pids it would kill may
/// have been reused since they were listed.
struct SystemProcesses {
    until: Instant,
}

impl SystemProcesses {
    fn out_of_time(&self) -> bool {
        Instant::now() >= self.until
    }
}

impl ProcessTable for SystemProcesses {
    fn list(&self) -> Vec<ProcessRow> {
        output("ps", &["-A", "-o", leftover::PS_COLUMNS], self.until)
            .map(|text| leftover::parse_ps(&text))
            .unwrap_or_default()
    }

    #[cfg(target_os = "linux")]
    fn cwds(&self, pids: &[u32]) -> Vec<(u32, PathBuf)> {
        pids.iter()
            .filter_map(|&pid| Some((pid, std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?)))
            .collect()
    }

    #[cfg(not(target_os = "linux"))]
    fn cwds(&self, pids: &[u32]) -> Vec<(u32, PathBuf)> {
        if pids.is_empty() {
            return Vec::new();
        }
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        // `lsof` exits 1 when one of the processes is already gone. `-b`
        // and `-w`: no call that can block on a mount, no warnings.
        let args = ["-b", "-w", "-a", "-d", "cwd", "-Fpn", "-p", &list];
        output("lsof", &args, self.until)
            .map(|text| leftover::parse_lsof_cwds(&text))
            .unwrap_or_default()
    }

    fn kill(&self, pids: &[u32]) -> bool {
        if self.out_of_time() {
            return false;
        }
        std::process::Command::new("kill")
            .arg("-KILL")
            .args(pids.iter().map(u32::to_string))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            // Ran: its exit status is 1 when one had already exited.
            .is_ok()
    }
}

/// What `program args` printed, whatever its exit status; `None` when it
/// could not run or was still running at `until`, when it is killed.
fn output(program: &str, args: &[&str], until: Instant) -> Option<String> {
    use std::io::Read as _;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read while it runs, so a full pipe does not stop it.
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return reader.join().ok(),
            Ok(None) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gone_within;

    /// Without a working directory there is no project to keep the
    /// search for leftovers to: the command does not start.
    #[test]
    fn a_command_without_a_working_directory_is_refused() {
        let mut bare = tokio::process::Command::new("true");
        let refused = spawn(&mut bare).err().expect("refused");
        assert!(
            refused.to_string().contains("working directory"),
            "{refused}"
        );
    }

    /// A program still running at the deadline is killed and read as
    /// nothing, so a hung `lsof` cannot hold the run.
    #[test]
    fn a_command_past_the_deadline_is_killed_and_reads_as_nothing() {
        let started = Instant::now();
        let until = started + Duration::from_millis(200);
        assert_eq!(output("sleep", &["30"], until), None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_command_that_ends_in_time_is_read() {
        let until = Instant::now() + SWEEP_LIMIT;
        assert_eq!(output("echo", &["hi"], until).as_deref(), Some("hi\n"));
    }

    /// Past the deadline the pids may have been reused: nothing is killed.
    #[test]
    fn nothing_is_killed_past_the_deadline() {
        let mut sleep = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let late = SystemProcesses {
            until: Instant::now(),
        };
        assert!(!late.kill(&[sleep.id()]));
        let survived = !gone_within(sleep.id(), Duration::from_millis(200));
        let _ = sleep.kill();
        let _ = sleep.wait();
        assert!(survived, "the process was killed after the deadline");
    }
}
