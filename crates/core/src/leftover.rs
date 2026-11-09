//! What `verify` or a heavy job left running outside the process group
//! Tenaz kills when it ends (ADR 0031).
//!
//! A check may start a server in a session of its own (Node's
//! `spawn(..., { detached: true })`, `setsid`) and never stop it. When the
//! check ends, the server's parent is gone and it is adopted: by `launchd`
//! or `init` (pid 1), or on Linux by a `systemd --user` above Tenaz. Such a
//! process is left behind when it was born while the command ran, leads
//! a process group of its own (`setsid` makes it lead one; a shell's `&`
//! job does not), its parent is one of those adopters or a member of the
//! command's own group, it has no controlling terminal, it runs as the
//! same user as Tenaz, and its working directory is inside the project.
//! It is stopped with its descendants. `git` is left alone: `git gc
//! --auto` detaches the same way, and one killed mid-write leaves its
//! locks behind.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ports::ProcessTable;

/// One line of the process table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub uid: u32,
    /// How long ago it started, in whole seconds as `ps` reports it.
    pub age: Duration,
    /// It has a controlling terminal: a job of a shell, not a daemon.
    pub terminal: bool,
    /// The executable's file name.
    pub name: String,
}

/// A command Tenaz started: from Tenaz's process (`own`), in process
/// group `group`, from `root`.
#[derive(Clone, Debug)]
pub struct Started<'a> {
    pub own: u32,
    pub group: u32,
    pub root: &'a Path,
}

/// How many times the table is read again: a process stopped in one round
/// may have started another just before.
const ROUNDS: usize = 3;

/// The one process name never stopped: `git gc --auto` detaches as a
/// daemon does, and one killed mid-write leaves its locks behind.
const NEVER_STOPPED: &str = "git";

/// The subreaper that adopts orphans on Linux: `systemd --user`.
const SUBREAPER: &str = "systemd";

/// Stops what the command in `started` left behind, as long as rounds find
/// more; the pids it stopped. `ran` says how long ago the command started.
pub fn sweep(
    table: &dyn ProcessTable,
    started: &Started<'_>,
    ran: impl Fn() -> Duration,
) -> Vec<u32> {
    let mut stopped = BTreeSet::new();
    for _ in 0..ROUNDS {
        let tree = round(table, started, ran(), &stopped);
        if tree.is_empty() || !table.kill(&tree) {
            break;
        }
        stopped.extend(tree);
    }
    stopped.into_iter().collect()
}

/// What one reading of the table finds left behind and not yet in
/// `stopped`: each process whose directory is in the project, with
/// Tenaz's user's processes under it (another user's cannot be killed).
fn round(
    table: &dyn ProcessTable,
    started: &Started<'_>,
    ran: Duration,
    stopped: &BTreeSet<u32>,
) -> Vec<u32> {
    let rows = table.list();
    let Some(own) = rows.iter().find(|r| r.pid == started.own) else {
        return Vec::new();
    };
    let found: Vec<u32> = candidates(&rows, own, started.group, ran)
        .into_iter()
        .filter(|pid| !stopped.contains(pid))
        .collect();
    let roots: Vec<u32> = table
        .cwds(&found)
        .into_iter()
        .filter(|(_, cwd)| cwd.starts_with(started.root))
        .map(|(pid, _)| pid)
        .collect();
    with_descendants(&rows, &roots, |r| r.uid == own.uid)
        .into_iter()
        .filter(|pid| !stopped.contains(pid))
        .collect()
}

/// The processes that may have been left behind, before their working
/// directory is known: born within `ran` (rounded up to whole seconds,
/// as `ps` rounds ages down), detached (see [`detached`]), run by
/// Tenaz's user, and with a parent that adopts orphans or is in the
/// command's `group`. `own` is Tenaz's row.
fn candidates(rows: &[ProcessRow], own: &ProcessRow, group: u32, ran: Duration) -> Vec<u32> {
    let by_pid: HashMap<u32, &ProcessRow> = rows.iter().map(|r| (r.pid, r)).collect();
    let adopters = adopters(&by_pid, own);
    let window = Duration::from_secs(ran.as_secs() + u64::from(ran.subsec_nanos() > 0));
    let in_group = |pid: u32| by_pid.get(&pid).is_some_and(|p| p.pgid == group);
    rows.iter()
        .filter(|r| {
            r.age <= window
                && r.pid != own.pid
                && detached(r)
                && r.uid == own.uid
                && (adopters.contains(&r.ppid) || in_group(r.ppid))
        })
        .map(|r| r.pid)
        .collect()
}

/// It leads a process group of its own and has no terminal, as a daemon
/// does, and is not [`NEVER_STOPPED`].
fn detached(row: &ProcessRow) -> bool {
    row.pgid == row.pid && !row.terminal && row.name != NEVER_STOPPED
}

/// Where an orphan of Tenaz's commands goes: pid 1, which the table may
/// not list, and a [`SUBREAPER`] above Tenaz.
/// Never Tenaz itself, which may be pid 1 in a container.
fn adopters(by_pid: &HashMap<u32, &ProcessRow>, own: &ProcessRow) -> BTreeSet<u32> {
    let mut found = BTreeSet::from([1]);
    let mut seen = BTreeSet::new();
    let mut next = by_pid.get(&own.ppid);
    while let Some(row) = next.filter(|r| r.pid > 1 && seen.insert(r.pid)) {
        if row.name == SUBREAPER {
            found.insert(row.pid);
        }
        next = by_pid.get(&row.ppid);
    }
    found.remove(&own.pid);
    found
}

/// `roots` and every process under them in `rows` that `keep` accepts.
fn with_descendants(
    rows: &[ProcessRow],
    roots: &[u32],
    keep: impl Fn(&ProcessRow) -> bool,
) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for row in rows.iter().filter(|r| keep(r)) {
        children.entry(row.ppid).or_default().push(row.pid);
    }
    let mut found: BTreeSet<u32> = BTreeSet::new();
    let mut pending = roots.to_vec();
    while let Some(pid) = pending.pop() {
        if found.insert(pid) {
            pending.extend(children.get(&pid).into_iter().flatten());
        }
    }
    found.into_iter().collect()
}

/// The columns [`parse_ps`] reads, for `ps -A -o`.
pub const PS_COLUMNS: &str = "pid=,ppid=,pgid=,uid=,etime=,tty=,comm=";

/// The rows of `ps -A -o` [`PS_COLUMNS`]; a line that does not parse is
/// skipped. The name is the file name of the rest of the line, which may
/// hold spaces.
pub fn parse_ps(text: &str) -> Vec<ProcessRow> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let mut number = || fields.next()?.parse::<u32>().ok();
            let (pid, ppid, pgid, uid) = (number()?, number()?, number()?, number()?);
            let age = parse_etime(fields.next()?)?;
            let terminal = !matches!(fields.next()?, "?" | "??");
            let command = fields.collect::<Vec<_>>().join(" ");
            let name = command.rsplit('/').next().unwrap_or_default().to_owned();
            Some(ProcessRow {
                pid,
                ppid,
                pgid,
                uid,
                age,
                terminal,
                name,
            })
        })
        .collect()
}

/// `ps`'s elapsed time, `[[dd-]hh:]mm:ss`.
pub fn parse_etime(text: &str) -> Option<Duration> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let parts = clock
        .split(':')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let (hours, minutes, seconds) = match parts[..] {
        [m, s] => (0, m, s),
        [h, m, s] => (h, m, s),
        _ => return None,
    };
    Some(Duration::from_secs(
        ((days * 24 + hours) * 60 + minutes) * 60 + seconds,
    ))
}

/// The working directories in `lsof -a -d cwd -Fpn -p ...` output: a `p`
/// line names a process, the `n` line after it its directory.
pub fn parse_lsof_cwds(text: &str) -> Vec<(u32, PathBuf)> {
    let mut found = Vec::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(number) = line.strip_prefix('p') {
            pid = number.parse::<u32>().ok();
        } else if let (Some(path), Some(p)) = (line.strip_prefix('n'), pid) {
            found.push((p, PathBuf::from(path)));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScriptedProcesses;

    const TENAZ: u32 = 500;
    const SHELL: u32 = 400;
    const GROUP: u32 = 700;
    const ME: u32 = 501;
    const RAN: Duration = Duration::from_secs(4);

    /// A daemon born `age` seconds ago, run by Tenaz's user.
    fn row(pid: u32, ppid: u32, pgid: u32, age: u64) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            pgid,
            uid: ME,
            age: Duration::from_secs(age),
            terminal: false,
            name: "node".to_owned(),
        }
    }

    /// launchd, the user's shell, Tenaz under it, and verify's group.
    fn machine() -> Vec<ProcessRow> {
        vec![
            ProcessRow {
                uid: 0,
                name: "launchd".to_owned(),
                ..row(1, 0, 1, 90_000)
            },
            ProcessRow {
                terminal: true,
                name: "zsh".to_owned(),
                ..row(SHELL, 1, SHELL, 9_000)
            },
            ProcessRow {
                terminal: true,
                ..row(TENAZ, SHELL, TENAZ, 3_000)
            },
            row(GROUP, TENAZ, GROUP, 4),
        ]
    }

    fn started(root: &Path) -> Started<'_> {
        Started {
            own: TENAZ,
            group: GROUP,
            root,
        }
    }

    /// The candidates in `rows`, which list Tenaz.
    fn candidates_in(rows: &[ProcessRow], ran: Duration) -> Vec<u32> {
        let own = rows
            .iter()
            .find(|r| r.pid == TENAZ)
            .expect("Tenaz is listed");
        candidates(rows, own, GROUP, ran)
    }

    fn with(extra: Vec<ProcessRow>) -> Vec<ProcessRow> {
        let mut rows = machine();
        rows.extend(extra);
        rows
    }

    fn sweep_p(table: &ScriptedProcesses) -> Vec<u32> {
        sweep(table, &started(Path::new("/p")), || RAN)
    }

    #[test]
    fn a_process_in_a_session_of_its_own_adopted_by_launchd_is_a_candidate() {
        let rows = with(vec![row(800, 1, 800, 3)]);
        assert_eq!(candidates_in(&rows, RAN), vec![800]);
    }

    /// Linux reparents an orphan to the nearest subreaper above it, such
    /// as `systemd --user`.
    #[test]
    fn a_systemd_above_tenaz_adopts() {
        let mut rows = with(vec![row(800, 300, 800, 3)]);
        rows.push(ProcessRow {
            name: "systemd".to_owned(),
            ..row(300, 1, 300, 9_000)
        });
        rows.iter_mut().find(|r| r.pid == SHELL).unwrap().ppid = 300;
        assert_eq!(candidates_in(&rows, RAN), vec![800]);
    }

    /// A `systemd` beside Tenaz, not above it, adopts nothing of Tenaz's.
    #[test]
    fn a_systemd_elsewhere_does_not_adopt() {
        let rows = with(vec![
            ProcessRow {
                name: "systemd".to_owned(),
                ..row(300, 1, 300, 9_000)
            },
            row(800, 300, 800, 3),
        ]);
        assert!(candidates_in(&rows, RAN).is_empty());
    }

    /// Its parent was killed with the group but has not exited yet.
    #[test]
    fn a_child_of_a_dying_group_member_is_a_candidate() {
        let rows = with(vec![row(710, GROUP, GROUP, 3), row(800, 710, 800, 2)]);
        assert_eq!(candidates_in(&rows, RAN), vec![800]);
    }

    /// Each is born in the window with the project as its directory, and
    /// left alone for one reason.
    #[test]
    fn what_verify_did_not_leave_behind_is_not_a_candidate() {
        let cases = [
            (row(800, 1, 800, 6), "born before verify started"),
            (
                ProcessRow {
                    uid: 0,
                    ..row(801, 1, 801, 1)
                },
                "another user's",
            ),
            (row(802, TENAZ, 802, 1), "a child of Tenaz: another command"),
            (
                ProcessRow {
                    name: "git".to_owned(),
                    ..row(803, 1, 803, 1)
                },
                "`git gc --auto`, detached by git",
            ),
            (
                row(804, SHELL, 804, 1),
                "a child of the user's shell, which adopts nothing",
            ),
            (
                ProcessRow {
                    terminal: true,
                    ..row(805, 1, 805, 1)
                },
                "a job with a terminal",
            ),
            (
                row(806, 1, 900, 1),
                "an orphaned `&` job of another shell, in that shell's group",
            ),
        ];
        for (process, why) in cases {
            let pid = process.pid;
            let rows = with(vec![process]);
            assert!(!candidates_in(&rows, RAN).contains(&pid), "{why}");
        }
    }

    #[test]
    fn tenaz_itself_is_not_a_candidate() {
        let mut rows = machine();
        let tenaz = rows.iter_mut().find(|r| r.pid == TENAZ).unwrap();
        tenaz.ppid = 1;
        tenaz.terminal = false;
        tenaz.age = Duration::from_secs(1);
        assert!(candidates_in(&rows, RAN).is_empty());
    }

    /// In a container Tenaz may be pid 1: what it starts is its own
    /// child, not an orphan.
    #[test]
    fn tenaz_as_pid_1_adopts_nothing() {
        let rows = vec![
            ProcessRow {
                pid: 1,
                ppid: 0,
                ..row(1, 0, 1, 3_000)
            },
            row(800, 1, 800, 1),
        ];
        assert!(candidates(&rows, &rows[0], GROUP, RAN).is_empty());
    }

    /// `ps` rounds ages down: after 4.0 s an age of 4 counts and 5 does
    /// not; after 4.2 s an age of 5 counts and 6 does not.
    #[test]
    fn the_window_is_rounded_up_to_whole_seconds() {
        for (ran_ms, inside, outside) in [(4_000, 4, 5), (4_200, 5, 6)] {
            let rows = with(vec![row(800, 1, 800, inside), row(801, 1, 801, outside)]);
            let ran = Duration::from_millis(ran_ms);
            assert_eq!(candidates_in(&rows, ran), vec![800], "after {ran_ms} ms");
        }
    }

    /// Without Tenaz's row its user and adopters are unknown: nothing
    /// is stopped.
    #[test]
    fn nothing_without_tenaz_in_the_table() {
        let table = ScriptedProcesses::new(vec![vec![row(800, 1, 800, 1)]]).cwd(800, "/p");
        assert!(sweep_p(&table).is_empty());
        assert!(table.killed().is_empty());
    }

    #[test]
    fn descendants_are_found_at_any_depth_once() {
        let rows = vec![
            row(800, 1, 800, 1),
            row(801, 800, 800, 1),
            row(802, 801, 800, 1),
            row(900, 1, 900, 1),
        ];
        assert_eq!(
            with_descendants(&rows, &[800, 801], |_| true),
            vec![800, 801, 802]
        );
    }

    /// `npx vite preview` started detached, in the project, and `node`
    /// under it; a server started the same way outside the project stays,
    /// and so does another user's process under the first, which cannot
    /// be killed.
    #[test]
    fn sweep_stops_what_was_left_in_the_project_with_its_children() {
        let rows = with(vec![
            row(800, 1, 800, 2),
            row(801, 800, 800, 2),
            ProcessRow {
                uid: 0,
                ..row(802, 800, 800, 2)
            },
            row(900, 1, 900, 2),
        ]);
        let table = ScriptedProcesses::new(vec![rows])
            .cwd(800, "/p")
            .cwd(900, "/elsewhere");
        assert_eq!(sweep_p(&table), vec![800, 801]);
        assert_eq!(table.killed(), vec![vec![800, 801]]);
    }

    /// `/p` contains `/p/web`, not `/pages`.
    #[test]
    fn a_directory_inside_the_project_counts_and_a_sibling_does_not() {
        let rows = with(vec![row(800, 1, 800, 2), row(801, 1, 801, 2)]);
        let table = ScriptedProcesses::new(vec![rows])
            .cwd(800, "/p/web")
            .cwd(801, "/pages");
        assert_eq!(sweep_p(&table), vec![800]);
    }

    /// `lsof` could not read it, or it exited: it is left alone.
    #[test]
    fn a_process_whose_directory_cannot_be_read_is_left_alone() {
        let table = ScriptedProcesses::new(vec![with(vec![row(800, 1, 800, 2)])]);
        assert!(sweep_p(&table).is_empty());
        assert!(table.killed().is_empty());
    }

    /// One left behind started another just before it was stopped. The
    /// table still lists the first for a moment after the kill: it is not
    /// stopped twice.
    #[test]
    fn sweep_reads_the_table_again_until_nothing_new_is_left() {
        let first = with(vec![row(800, 1, 800, 2)]);
        let second = with(vec![row(800, 1, 800, 2), row(810, 1, 810, 0)]);
        let table = ScriptedProcesses::new(vec![first, second.clone(), second])
            .cwd(800, "/p")
            .cwd(810, "/p");
        assert_eq!(sweep_p(&table), vec![800, 810]);
        assert_eq!(table.killed(), vec![vec![800], vec![810]]);
        assert_eq!(table.listed(), 3);
    }

    #[test]
    fn sweep_gives_up_after_a_few_rounds() {
        let rows = (0..10)
            .map(|i| with(vec![row(800 + i, 1, 800 + i, 0)]))
            .collect();
        let mut table = ScriptedProcesses::new(rows);
        for i in 0..10 {
            table = table.cwd(800 + i, "/p");
        }
        assert_eq!(sweep_p(&table).len(), ROUNDS);
    }

    /// Past its deadline the table kills nothing: none is reported as
    /// stopped, and no other round is tried.
    #[test]
    fn what_the_table_could_not_kill_is_not_reported() {
        let table = ScriptedProcesses::new(vec![with(vec![row(800, 1, 800, 2)])])
            .cwd(800, "/p")
            .refusing_kills();
        assert!(sweep_p(&table).is_empty());
        assert_eq!(table.listed(), 1);
    }

    #[test]
    fn sweep_with_nothing_left_kills_nothing() {
        let table = ScriptedProcesses::new(vec![machine()]);
        assert!(sweep_p(&table).is_empty());
        assert!(table.killed().is_empty());
    }

    #[test]
    fn parses_ps_rows_and_skips_the_rest() {
        let text = "    1     0     1     0 15-04:45:15 ??       /sbin/launchd\n  159 98599 98599   501    06:15:45 ttys003  /Applications/1Password Helper (Renderer)\nPID PPID\n  7 1 7 501 00:03 ? systemd\n";
        assert_eq!(
            parse_ps(text),
            vec![
                ProcessRow {
                    uid: 0,
                    name: "launchd".to_owned(),
                    ..row(1, 0, 1, ((15 * 24 + 4) * 60 + 45) * 60 + 15)
                },
                ProcessRow {
                    terminal: true,
                    name: "1Password Helper (Renderer)".to_owned(),
                    ..row(159, 98_599, 98_599, (6 * 60 + 15) * 60 + 45)
                },
                ProcessRow {
                    name: "systemd".to_owned(),
                    ..row(7, 1, 7, 3)
                },
            ]
        );
    }

    #[test]
    fn parses_elapsed_times() {
        assert_eq!(parse_etime("00:03"), Some(Duration::from_secs(3)));
        assert_eq!(parse_etime("01:02:03"), Some(Duration::from_secs(3_723)));
        assert_eq!(
            parse_etime("2-00:00:01"),
            Some(Duration::from_secs(172_801))
        );
        for bad in ["", "3", "a:b", "1:2:3:4", "x-00:01"] {
            assert_eq!(parse_etime(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_lsof_working_directories() {
        let text = "p99385\nfcwd\nn/private/tmp\np12\nfcwd\nn/Users/me/my project\n";
        assert_eq!(
            parse_lsof_cwds(text),
            vec![
                (99_385, PathBuf::from("/private/tmp")),
                (12, PathBuf::from("/Users/me/my project")),
            ]
        );
    }
}
