//! The flags that name each agent's executable, shared by `init` and
//! `run`.

use std::path::{Path, PathBuf};

use tenaz_core::ledger::HarnessKind;

/// `--fx-bin`, `--pi-bin`, `--opencode-bin` and `--qwen-bin`. `run` uses
/// the one pinned at `init` when its flag is not given.
#[derive(clap::Args, Clone, Debug, Default)]
pub(crate) struct HarnessBins {
    /// The fx executable (default: fx on PATH); init pins it, and run uses
    /// the pinned one unless this is given.
    #[arg(long = "fx-bin", value_name = "FX_BIN")]
    pub(crate) fx: Option<PathBuf>,
    /// The pi executable (default: pi on PATH); init pins it, and run uses
    /// the pinned one unless this is given.
    #[arg(long = "pi-bin", value_name = "PI_BIN")]
    pub(crate) pi: Option<PathBuf>,
    /// The opencode executable (default: opencode on PATH); init pins it, and run uses
    /// the pinned one unless this is given.
    #[arg(long = "opencode-bin", value_name = "OPENCODE_BIN")]
    pub(crate) opencode: Option<PathBuf>,
    /// The qwen executable (default: qwen on PATH); init pins it, and run uses
    /// the pinned one unless this is given.
    #[arg(long = "qwen-bin", value_name = "QWEN_BIN")]
    pub(crate) qwen: Option<PathBuf>,
}

impl HarnessBins {
    /// The executable given for `harness`; the fake harness has none.
    pub(crate) fn get(&self, harness: HarnessKind) -> Option<&Path> {
        match harness {
            HarnessKind::Fx => self.fx.as_deref(),
            HarnessKind::Pi => self.pi.as_deref(),
            HarnessKind::OpenCode => self.opencode.as_deref(),
            HarnessKind::Qwen => self.qwen.as_deref(),
            HarnessKind::Fake => None,
        }
    }

    /// Each flag's name and the path it was given, for a command line.
    pub(crate) fn flags(&self) -> [(&'static str, Option<String>); 4] {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        [
            ("fx-bin", path(&self.fx)),
            ("pi-bin", path(&self.pi)),
            ("opencode-bin", path(&self.opencode)),
            ("qwen-bin", path(&self.qwen)),
        ]
    }
}
