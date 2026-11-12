//! The scripted harness's check that writes into the working tree on every
//! run of `verify`, and the fix a session told why makes (ADR 0035).

use super::{Player, names};

impl Player {
    /// For the script's stamping task: a check that writes
    /// `out/<task>.png` on every run, with new content each time. A session
    /// told that `verify` changed the working tree moves it to `shots/`,
    /// which it lists in `.gitignore`: `out/<task>.png` may be committed by
    /// then, and ignoring a tracked file does not untrack it. True when it
    /// was that task.
    pub(super) async fn stamp_check(&mut self, task: &str) -> bool {
        if !names(self.script.stamping_task.as_deref(), task) {
            return false;
        }
        let folder = if self.told_writes { "shots" } else { "out" };
        let check = format!("checks/{task}.stamp.sh");
        self.write_tool(&check, &format!("stamp {folder}/{task}.png\n"), false)
            .await;
        if self.told_writes {
            let ignore = std::fs::read_to_string(self.cwd.join(".gitignore")).unwrap_or_default();
            if !ignore.lines().any(|line| line == "shots/") {
                self.write_tool(".gitignore", &format!("{ignore}shots/\n"), false)
                    .await;
            }
        }
        true
    }
}
