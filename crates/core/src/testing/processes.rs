//! A process table read from listings a test gives it, with no `ps`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::leftover::ProcessRow;
use crate::ports::ProcessTable;

/// Answers `list` with the next of its listings (the last one again once
/// they run out), and records what it was asked to kill. A killed process
/// stays in later listings that name it, as one not yet exited does in
/// `ps`.
#[derive(Debug, Default)]
pub struct ScriptedProcesses {
    listings: Vec<Vec<ProcessRow>>,
    cwds: HashMap<u32, PathBuf>,
    listed: AtomicUsize,
    killed: Mutex<Vec<Vec<u32>>>,
    refuses: bool,
}

impl ScriptedProcesses {
    pub fn new(listings: Vec<Vec<ProcessRow>>) -> Self {
        Self {
            listings,
            ..Self::default()
        }
    }

    /// Process `pid` runs in `cwd`; one with none set has no readable
    /// directory.
    #[must_use]
    pub fn cwd(mut self, pid: u32, cwd: &str) -> Self {
        self.cwds.insert(pid, PathBuf::from(cwd));
        self
    }

    /// Its `kill` does nothing and says so, as one past its deadline.
    #[must_use]
    pub fn refusing_kills(mut self) -> Self {
        self.refuses = true;
        self
    }

    /// How many times the table was listed.
    pub fn listed(&self) -> usize {
        self.listed.load(Ordering::Relaxed)
    }

    /// The pids of each `kill` call, in order.
    pub fn killed(&self) -> Vec<Vec<u32>> {
        self.killed.lock().unwrap().clone()
    }
}

impl ProcessTable for ScriptedProcesses {
    fn list(&self) -> Vec<ProcessRow> {
        let listed = self.listed.fetch_add(1, Ordering::Relaxed);
        let index = listed.min(self.listings.len().saturating_sub(1));
        self.listings.get(index).cloned().unwrap_or_default()
    }

    fn cwds(&self, pids: &[u32]) -> Vec<(u32, PathBuf)> {
        pids.iter()
            .filter_map(|pid| Some((*pid, self.cwds.get(pid)?.clone())))
            .collect()
    }

    fn kill(&self, pids: &[u32]) -> bool {
        if !self.refuses {
            self.killed.lock().unwrap().push(pids.to_vec());
        }
        !self.refuses
    }
}
