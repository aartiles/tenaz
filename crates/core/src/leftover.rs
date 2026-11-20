//! What `verify`, a heavy job or an agent session left running outside
//! what Tenaz kills when it ends (ADR 0031, ADR 0039).
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
//! locks behind. So is what was born while one of Tenaz's own engine
//! commands ran: the engine they start may detach the same way, from the
//! project, during an agent session.

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
/// group `group`, from `root`. An agent session has none to give: Pi runs
/// in Tenaz's own group, as the engine Tenaz starts may, and an ACP
/// agent's group is killed with it before the sweep.
#[derive(Clone, Debug)]
pub struct Started<'a> {
    pub own: u32,
    pub group: Option<u32>,
    pub root: &'a Path,
}

/// When things happened, as how long ago at one reading of the clock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Window {
    /// How long ago the command started.
    pub ran: Duration,
    /// When Tenaz's own engine commands ran: what was born then is the
    /// engine's.
    pub spared: Vec<Span>,
}

impl Window {
    /// Since the command started `ran` ago, with nothing spared.
    #[cfg(test)]
    fn since(ran: Duration) -> Self {
        Self {
            ran,
            spared: Vec::new(),
        }
    }

    /// Whether a process `age` old (rounded down, as `ps` reports it) may
    /// have been born in one of the spared spans. Each span is widened by
    /// [`SPAN_SLACK`] on both sides, then to whole seconds, as `ps` rounds
    /// ages down: sparing one too many is safe; stopping the engine is
    /// not.
    fn spares(&self, age: Duration) -> bool {
        self.spared.iter().any(|span| {
            let newest = span.ended.saturating_sub(SPAN_SLACK).as_secs();
            newest <= age.as_secs() && age <= round_up(span.began + SPAN_SLACK)
        })
    }
}

/// A span of time, as how long ago it began and ended; one still going
/// ended zero ago.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub began: Duration,
    pub ended: Duration,
}

/// How many times the table is read again: a process stopped in one round
/// may have started another just before.
const ROUNDS: usize = 3;

/// How far a spared span is widened on each side: for rounding, and for
/// small differences between the wall clock and the one `ps` ages come
/// from (time since boot on Linux).
const SPAN_SLACK: Duration = Duration::from_secs(5);

/// The one process name never stopped: `git gc --auto` detaches as a
/// daemon does, and one killed mid-write leaves its locks behind.
const NEVER_STOPPED: &str = "git";

/// The subreaper that adopts orphans on Linux: `systemd --user`.
const SUBREAPER: &str = "systemd";

/// Stops what the command in `started` left behind, as long as rounds find
/// more; the pids it stopped. `window` says when it started, and what is
/// spared. `window` is read before and after each listing of the table,
/// as `ps` may take seconds on a machine short of memory and reads each
/// age somewhere between the two (see [`between`]).
pub fn sweep(
    table: &dyn ProcessTable,
    started: &Started<'_>,
    window: impl Fn() -> Window,
) -> Vec<u32> {
    let mut stopped = BTreeSet::new();
    for _ in 0..ROUNDS {
        let tree = round(table, started, &window, &stopped);
        if tree.is_empty() || !table.kill(&tree) {
            break;
        }
        stopped.extend(tree);
    }
    stopped.into_iter().collect()
}

/// The window for ages read between the readings `before` and `after`.
/// How long ago the command started is the shorter of the two, so
/// nothing older is taken, even with the clock set back; one born in the
/// first moments of the command may be missed when `ps` reads late. Each
/// span runs from its start as `after` has it to its end as `before` has
/// it, so the engine is inside whenever its age was read. A span that began between the two
/// is in `after` alone. With the clock set back, `after` may read too
/// young; `before`'s spans are kept as well, which covers the engine as
/// long as `ps` read soon after `before`.
fn between(before: Window, after: Window) -> Window {
    let drift = after.ran.saturating_sub(before.ran);
    let widened = after.spared.into_iter().map(|span| Span {
        began: span.began,
        ended: span.ended.saturating_sub(drift),
    });
    Window {
        ran: before.ran.min(after.ran),
        spared: before.spared.into_iter().chain(widened).collect(),
    }
}

/// What one reading of the table finds left behind and not yet in
/// `stopped`: each process whose directory is in the project, with
/// Tenaz's user's processes under it (another user's cannot be killed).
fn round(
    table: &dyn ProcessTable,
    started: &Started<'_>,
    window: &dyn Fn() -> Window,
    stopped: &BTreeSet<u32>,
) -> Vec<u32> {
    let before = window();
    let rows = table.list();
    let window = between(before, window());
    let Some(own) = rows.iter().find(|r| r.pid == started.own) else {
        return Vec::new();
    };
    let found: Vec<u32> = candidates(&rows, own, started.group, &window)
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
/// directory is known: born within the window's `ran` (rounded up to
/// whole seconds, as `ps` rounds ages down) and not in one of its spared
/// spans, detached (see [`detached`]), run by Tenaz's user, and with a
/// parent that adopts orphans or is in the command's `group`. `own` is
/// Tenaz's row. Never Tenaz or one of its ancestors: a supervisor that
/// started Tenaz detached, in the project, a moment before its first
/// `verify` looks left behind, and stopping it would stop Tenaz with it.
fn candidates(
    rows: &[ProcessRow],
    own: &ProcessRow,
    group: Option<u32>,
    window: &Window,
) -> Vec<u32> {
    let by_pid: HashMap<u32, &ProcessRow> = rows.iter().map(|r| (r.pid, r)).collect();
    let above = ancestors(&by_pid, own);
    let adopters = adopters(&by_pid, &above, own);
    let spared: BTreeSet<u32> = above.iter().copied().chain([own.pid]).collect();
    let ran = round_up(window.ran);
    let in_group =
        |pid: u32| group.is_some_and(|group| by_pid.get(&pid).is_some_and(|p| p.pgid == group));
    rows.iter()
        .filter(|r| {
            r.age <= ran
                && !window.spares(r.age)
                && !spared.contains(&r.pid)
                && detached(r)
                && r.uid == own.uid
                && (adopters.contains(&r.ppid) || in_group(r.ppid))
        })
        .map(|r| r.pid)
        .collect()
}

/// `time` rounded up to whole seconds.
fn round_up(time: Duration) -> Duration {
    Duration::from_secs(time.as_secs() + u64::from(time.subsec_nanos() > 0))
}

/// It leads a process group of its own and has no terminal, as a daemon
/// does, and is not [`NEVER_STOPPED`].
fn detached(row: &ProcessRow) -> bool {
    row.pgid == row.pid && !row.terminal && row.name != NEVER_STOPPED
}

/// The processes above Tenaz in `by_pid`, its parent first, up to one
/// the table does not list; a loop, or one back to Tenaz, ends the walk.
fn ancestors(by_pid: &HashMap<u32, &ProcessRow>, own: &ProcessRow) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut next = by_pid.get(&own.ppid);
    while let Some(row) = next.filter(|r| r.pid != own.pid && !chain.contains(&r.pid)) {
        chain.push(row.pid);
        next = by_pid.get(&row.ppid);
    }
    chain
}

/// Where an orphan of Tenaz's commands goes: pid 1, which the table may
/// not list, and a [`SUBREAPER`] among the processes `above` Tenaz.
/// Never Tenaz itself, which may be pid 1 in a container.
fn adopters(by_pid: &HashMap<u32, &ProcessRow>, above: &[u32], own: &ProcessRow) -> BTreeSet<u32> {
    let mut found: BTreeSet<u32> = above
        .iter()
        .filter(|pid| **pid > 1 && by_pid.get(pid).is_some_and(|r| r.name == SUBREAPER))
        .copied()
        .collect();
    found.insert(1);
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
    use std::sync::atomic::{AtomicU64, Ordering};

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
            group: Some(GROUP),
            root,
        }
    }

    /// The candidates in `rows`, which list Tenaz.
    fn candidates_in(rows: &[ProcessRow], ran: Duration) -> Vec<u32> {
        let own = rows
            .iter()
            .find(|r| r.pid == TENAZ)
            .expect("Tenaz is listed");
        candidates(rows, own, Some(GROUP), &Window::since(ran))
    }

    fn with(extra: Vec<ProcessRow>) -> Vec<ProcessRow> {
        let mut rows = machine();
        rows.extend(extra);
        rows
    }

    fn sweep_p(table: &ScriptedProcesses) -> Vec<u32> {
        sweep(table, &started(Path::new("/p")), || Window::since(RAN))
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

    /// A supervisor that started Tenaz in a session of its own, from the
    /// project, a moment before Tenaz's first `verify` (as a run resumed
    /// straight into a review does): it looks left behind, but it is
    /// above Tenaz, and stopping it would stop Tenaz.
    /// A shell between them, as a launcher's `sh -c` would be.
    fn supervised() -> Vec<ProcessRow> {
        let mut rows = machine();
        let tenaz = rows.iter_mut().find(|r| r.pid == TENAZ).unwrap();
        tenaz.ppid = 650;
        tenaz.pgid = 600;
        tenaz.terminal = false;
        tenaz.age = Duration::from_secs(1);
        rows.push(ProcessRow {
            name: "Python".to_owned(),
            ..row(600, 1, 600, 1)
        });
        rows.push(ProcessRow {
            name: "sh".to_owned(),
            ..row(650, 600, 600, 1)
        });
        rows
    }

    #[test]
    fn a_process_above_tenaz_is_not_a_candidate() {
        assert!(candidates_in(&supervised(), RAN).is_empty());
    }

    /// The sweep still stops what `verify` left behind beside it.
    #[test]
    fn sweep_spares_the_supervisor_above_tenaz() {
        let mut rows = supervised();
        rows.push(row(800, 1, 800, 1));
        let table = ScriptedProcesses::new(vec![rows])
            .cwd(600, "/p")
            .cwd(800, "/p");
        assert_eq!(sweep_p(&table), vec![800]);
        assert_eq!(table.killed(), vec![vec![800]]);
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
        assert!(candidates(&rows, &rows[0], Some(GROUP), &Window::since(RAN)).is_empty());
    }

    /// An agent session's window, with Tenaz's engine commands that ran
    /// `spans` ago (began, ended), in seconds.
    fn session(ran: u64, spans: &[(u64, u64)]) -> Window {
        Window {
            ran: Duration::from_secs(ran),
            spared: spans
                .iter()
                .map(|&(began, ended)| Span {
                    began: Duration::from_secs(began),
                    ended: Duration::from_secs(ended),
                })
                .collect(),
        }
    }

    /// A session has no group of its own: the harness runs in Tenaz's.
    /// What a process of that group starts detached is not taken for left
    /// behind while its parent lives (the engine Tenaz started may be
    /// one); what pid 1 adopted is.
    #[test]
    fn without_a_group_only_what_was_adopted_is_a_candidate() {
        let rows = with(vec![
            row(600, TENAZ, TENAZ, 300),
            row(810, 600, 810, 30),
            row(800, 1, 800, 30),
        ]);
        let own = rows.iter().find(|r| r.pid == TENAZ).unwrap();
        assert_eq!(candidates(&rows, own, None, &session(600, &[])), vec![800]);
    }

    /// The engine Tenaz restarted mid-session, detached and from the
    /// project, as `setsid nohup splash serve` in an `--engine-start`
    /// script run from inside it: born while the start command ran (from
    /// 120 s to 90 s ago), it is spared, and so is what was born within
    /// [`SPAN_SLACK`] of it; the agent's server, born 30 s ago, is not.
    #[test]
    fn what_was_born_while_an_engine_command_ran_is_spared() {
        let rows = with(vec![
            row(800, 1, 800, 30),
            row(900, 1, 900, 100),
            row(901, 1, 901, 125),
            row(902, 1, 902, 85),
        ]);
        let own = rows.iter().find(|r| r.pid == TENAZ).unwrap();
        let window = session(600, &[(120, 90)]);
        assert_eq!(candidates(&rows, own, None, &window), vec![800]);
    }

    /// The span is widened by [`SPAN_SLACK`], then outward to whole
    /// seconds: one from 120.4 s to 90.7 s ago spares ages 85 to 126, not
    /// 84 or 127.
    #[test]
    fn a_spared_span_is_widened_by_the_slack_and_to_whole_seconds() {
        let rows = with(vec![
            row(800, 1, 800, 84),
            row(801, 1, 801, 85),
            row(802, 1, 802, 126),
            row(803, 1, 803, 127),
        ]);
        let own = rows.iter().find(|r| r.pid == TENAZ).unwrap();
        let window = Window {
            ran: Duration::from_secs(600),
            spared: vec![Span {
                began: Duration::from_millis(120_400),
                ended: Duration::from_millis(90_700),
            }],
        };
        assert_eq!(candidates(&rows, own, None, &window), vec![800, 803]);
    }

    /// A table whose every listing takes `took` seconds of the clock it
    /// shares with the window, as `ps` on a machine short of memory. Its
    /// rows give each process's age when the clock was at zero; `ps`
    /// reads the ages `read` seconds into its run.
    struct Slow {
        table: ScriptedProcesses,
        clock: AtomicU64,
        took: u64,
        read: u64,
    }

    impl Slow {
        fn now(&self) -> u64 {
            self.clock.load(Ordering::SeqCst)
        }
    }

    impl ProcessTable for Slow {
        fn list(&self) -> Vec<ProcessRow> {
            let began = self.clock.fetch_add(self.took, Ordering::SeqCst);
            self.table
                .list()
                .into_iter()
                .map(|r| ProcessRow {
                    age: r.age + Duration::from_secs(began + self.read),
                    ..r
                })
                .collect()
        }
        fn cwds(&self, pids: &[u32]) -> Vec<(u32, PathBuf)> {
            self.table.cwds(pids)
        }
        fn kill(&self, pids: &[u32]) -> bool {
            self.table.kill(pids)
        }
    }

    /// When the sweep begins: a session that began 600 s before, an
    /// engine command from 92 s to 90 s before, the agent's server born
    /// 30 s before, an engine born as the command began and one as it
    /// ended, and a daemon born 2 s before the session. `ps` takes 20 s,
    /// more than the span and twice the slack together, so the two
    /// readings' spans leave a gap; it reads the ages `read` seconds in.
    fn slow_ps(read: u64) -> Vec<u32> {
        let rows = with(vec![
            row(800, 1, 800, 30),
            row(900, 1, 900, 92),
            row(902, 1, 902, 90),
            row(901, 1, 901, 602),
        ]);
        let table = Slow {
            table: ScriptedProcesses::new(vec![rows])
                .cwd(800, "/p")
                .cwd(900, "/p")
                .cwd(901, "/p")
                .cwd(902, "/p"),
            clock: AtomicU64::new(0),
            took: 20,
            read,
        };
        let started = Started {
            own: TENAZ,
            group: None,
            root: Path::new("/p"),
        };
        let swept = sweep(&table, &started, || {
            let now = table.now();
            session(600 + now, &[(92 + now, 90 + now)])
        });
        assert_eq!(table.table.killed(), vec![swept.clone()]);
        swept
    }

    /// Wherever in its run `ps` reads the ages, both engines are spared,
    /// the daemon older than the session is not taken, and the agent's
    /// server is stopped.
    #[test]
    fn a_slow_ps_still_spares_the_engine_and_takes_nothing_older() {
        for read in [0, 10, 20] {
            assert_eq!(slow_ps(read), vec![800], "ages read {read} s in");
        }
    }

    /// A span as how long ago it began and ended, in seconds.
    fn span(began: u64, ended: u64) -> Span {
        Span {
            began: Duration::from_secs(began),
            ended: Duration::from_secs(ended),
        }
    }

    /// The two readings of the window around a listing, combined: each
    /// case is (before, after, the window used).
    #[test]
    fn the_readings_around_a_listing_are_combined() {
        let window = |ran, spared| Window {
            ran: Duration::from_secs(ran),
            spared,
        };
        let cases = [
            (
                "the clock moved on 10 s: the span runs from after's start to before's end",
                window(600, vec![span(120, 90)]),
                window(610, vec![span(130, 100)]),
                window(600, vec![span(120, 90), span(130, 90)]),
            ),
            (
                "a span began between the readings: after alone has it",
                window(600, vec![]),
                window(610, vec![span(4, 0)]),
                window(600, vec![span(4, 0)]),
            ),
            (
                "a span ended between the readings: it ends now",
                window(600, vec![span(30, 0)]),
                window(610, vec![span(40, 5)]),
                window(600, vec![span(30, 0), span(40, 0)]),
            ),
            (
                "the clock was set back 60 s: the shorter `ran`, and before's spans kept",
                window(600, vec![span(120, 90)]),
                window(540, vec![span(60, 30)]),
                window(540, vec![span(120, 90), span(60, 30)]),
            ),
        ];
        for (why, before, after, combined) in cases {
            assert_eq!(between(before, after), combined, "{why}");
        }
    }

    /// An engine command still running spares everything born since it
    /// began.
    #[test]
    fn an_engine_command_still_running_spares_what_it_started_so_far() {
        let rows = with(vec![row(800, 1, 800, 0), row(801, 1, 801, 40)]);
        let own = rows.iter().find(|r| r.pid == TENAZ).unwrap();
        let window = session(600, &[(10, 0)]);
        assert_eq!(candidates(&rows, own, None, &window), vec![801]);
    }

    /// The whole session, through the table: the agent's `vite preview`
    /// and the `node` under it go, and the engine restarted mid-session
    /// from the project stays, with its child.
    #[test]
    fn a_session_sweep_stops_the_agents_server_and_keeps_the_engine() {
        let rows = with(vec![
            row(600, TENAZ, TENAZ, 500),
            row(800, 1, 800, 30),
            row(801, 800, 800, 30),
            row(900, 1, 900, 100),
            row(901, 900, 900, 99),
        ]);
        let table = ScriptedProcesses::new(vec![rows])
            .cwd(600, "/p")
            .cwd(800, "/p")
            .cwd(900, "/p");
        let started = Started {
            own: TENAZ,
            group: None,
            root: Path::new("/p"),
        };
        let swept = sweep(&table, &started, || session(600, &[(120, 90)]));
        assert_eq!(swept, vec![800, 801]);
        assert_eq!(table.killed(), vec![vec![800, 801]]);
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
