//! The hidden commands `run` starts: `fx-shim` and `sandbox-exec`.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;

#[derive(clap::Args, Debug)]
pub(crate) struct SandboxExecArgs {
    #[arg(long)]
    write: Vec<PathBuf>,
    #[arg(long)]
    deny_write: Vec<PathBuf>,
    #[arg(long)]
    deny_read: Vec<PathBuf>,
    #[arg(last = true, required = true)]
    command: Vec<std::ffi::OsString>,
}

/// `tenaz fx-shim <fx> <args>`: fx with stdin and stderr inherited and its
/// stdout normalized line by line; exits with fx's code.
pub(crate) fn fx_shim(command: &[String]) -> anyhow::Result<ExitCode> {
    use std::io::{BufRead as _, Write as _};
    let (program, args) = command
        .split_first()
        .context("fx-shim needs the fx binary")?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start {program}"))?;
    let stdout = child.stdout.take().context("no stdout from fx")?;
    let mut out = std::io::stdout().lock();
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line?;
        writeln!(out, "{}", tenaz_acp::fx::normalize_line(&line))?;
        out.flush()?;
    }
    let status = child.wait()?;
    Ok(ExitCode::from(
        u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
    ))
}

/// `tenaz sandbox-exec`: becomes the command under the policy; returns
/// only when that fails, with the shell's "cannot execute" code.
pub(crate) fn sandbox_exec(args: SandboxExecArgs) -> ExitCode {
    let SandboxExecArgs {
        write,
        deny_write,
        deny_read,
        command,
    } = args;
    let policy = tenaz_adapters::sandbox::Policy {
        write,
        deny_write,
        deny_read,
    };
    let error = policy.exec(&command);
    eprintln!(
        "tenaz sandbox-exec: cannot run {}: {error}",
        command
            .first()
            .map(|c| c.to_string_lossy())
            .unwrap_or_default()
    );
    ExitCode::from(126)
}
