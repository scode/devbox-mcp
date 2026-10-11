//! Containment for agent processes: starting them so they can be cleaned up, and cleaning them up completely.
//!
//! `LIF-1` requires that nothing an agent started keeps running once devbox-mcp stops tracking it. Killing the agent's
//! process group is not enough for real agents: Codex starts every shell command in a new session (and so a new process
//! group), and only its direct children get Codex's own parent-death signal. A command's children therefore escape a
//! group kill. Observed with codex-cli 0.162.1: `bash -c "sleep 303 & sleep 120"` left both sleeps running after Codex
//! was killed.
//!
//! So an abandoned agent is cleaned up by walking its process tree through `/proc`, which follows parent links across
//! sessions and groups (see [`kill_tree`] for how that is done without signalling the wrong process). Remaining known
//! gaps, which need a cgroup to close (TODO.md):
//!
//! - A process that was already orphaned (its parent exited, so it was reparented away from the agent's tree) before
//!   cleanup cannot be found. Codex kills a finished command's process group itself, so this needs something like a
//!   double-forking daemon.
//! - If devbox-mcp itself dies abruptly, only the agent's direct parent-death signal fires.
//!
//! Agent-agnostic: adapters use [`contain`] when building the command, [`TreeGuard`] around the running child, and
//! [`exited`] to notice the child's exit without reaping it.
//!
//! Linux only in substance: the tree walk reads `/proc` and the exit watch uses `waitid`. Elsewhere it degrades to
//! killing the agent's process group.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::process::Command;

/// Prepares an agent command for containment: its own process group, killed if its `Child` handle is dropped, and (on
/// Linux) killed if devbox-mcp dies.
pub fn contain(cmd: &mut Command) {
    cmd.kill_on_drop(true).process_group(0);
    set_parent_death_signal(cmd);
}

/// Asks the kernel to SIGKILL the child if devbox-mcp dies, so an abrupt exit does not leave the agent running.
///
/// Linux delivers this signal when the *thread* that spawned the child exits, not the process. devbox-mcp runs a
/// current-thread tokio runtime on the main thread, so children are always spawned from the main thread, which lives as
/// long as the process. Switching to a multi-threaded runtime would silently break this.
///
/// If devbox-mcp dies between fork and the `prctl` call, the child has already been reparented and the signal will never
/// come, so the child checks that its parent is still the process that spawned it and fails the exec otherwise.
#[cfg(target_os = "linux")]
fn set_parent_death_signal(cmd: &mut Command) {
    let parent = libc::pid_t::try_from(std::process::id()).expect("PID fits in pid_t");
    // SAFETY: the closure runs in the forked child before exec and only calls prctl and getppid, both of which are
    // async-signal-safe, and builds an io::Error from a raw code without allocating.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn set_parent_death_signal(_cmd: &mut Command) {}

/// Waits until our child `pid` has exited, *without reaping it*.
///
/// An exited but unreaped child is a zombie that still owns its PID and process group ID, so they can be signalled
/// safely until the child is reaped (for example by `tokio::process::Child::wait`). That is the window
/// [`TreeGuard::exited`] needs. The wait runs on a blocking thread; if the turn is abandoned first, the thread finishes
/// as soon as the guard kills the child.
#[cfg(target_os = "linux")]
pub async fn exited(pid: u32) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        loop {
            // SAFETY: waitid writes only into `info`, which is a valid, zeroed siginfo_t.
            let result = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Kills an agent and everything it started when dropped, unless disarmed first.
///
/// Create it right after spawning a child prepared with [`contain`]. While it is armed, the child must not have been
/// reaped: its PID (and process group ID) then still belong to it, so signalling them cannot hit an unrelated process.
/// Once [`exited`] reports the child's exit, call [`TreeGuard::exited`] before reaping the child.
pub struct TreeGuard {
    pid: Option<i32>,
}

impl TreeGuard {
    pub fn new(pid: Option<u32>) -> Self {
        Self {
            pid: pid.and_then(|p| i32::try_from(p).ok()),
        }
    }

    /// Cleans up after a child that exited on its own, then disarms the guard. Call it after [`exited`] returned and
    /// before the child is reaped.
    ///
    /// The child's descendants were reparented away when it exited, so there is no tree left to walk; this kills
    /// whatever is still in the child's process group. The unreaped child still owns the group ID, so the signal cannot
    /// reach an unrelated group.
    pub fn exited(&mut self) {
        if let Some(pid) = self.pid.take() {
            // SAFETY: killpg has no memory-safety preconditions. ESRCH (an empty group) is the normal case.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }

    /// Disarms the guard without signalling anything, for when the child can no longer be signalled safely (it was
    /// reaped by someone else).
    pub fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_tree(pid);
        }
    }
}

/// Upper bound on how long one round of [`kill_tree`] waits for its stop signals to take effect.
const STOP_WAIT: Duration = Duration::from_millis(500);

/// Upper bound on the number of generations [`kill_tree`] descends.
const MAX_GENERATIONS: usize = 64;

/// Kills `root` (an unreaped child of ours), every descendant of it whatever their session or process group, and
/// `root`'s process group.
///
/// Signalling a PID found in `/proc` is only safe while that PID cannot be reused, so the walk only ever looks at
/// children of processes that are already stopped: a stopped process cannot reap its children, so their PIDs stay
/// theirs (as live processes or zombies) until we are done. `root` itself is ours and unreaped. Each generation is
/// therefore stopped, and confirmed stopped, before its children are read.
///
/// Killing then goes deepest first. Killing a parent before its stopped children can make the children's process
/// group orphaned, and the kernel then sends that group SIGHUP and SIGCONT, which resumes processes we stopped and lets
/// them fork new ones before their own SIGKILL arrives. With children killed before their parents, no stopped process
/// is ever left with a dead parent.
fn kill_tree(root: i32) {
    // Every process found, ancestors before descendants.
    let mut found = vec![root];
    signal(root, libc::SIGSTOP);
    wait_until_stopped(&[root]);
    #[cfg(target_os = "linux")]
    for _ in 0..MAX_GENERATIONS {
        let children = children_by_parent();
        let next: Vec<i32> = found
            .iter()
            .flat_map(|p| children.get(p).into_iter().flatten().copied())
            .filter(|p| !found.contains(p))
            .collect();
        if next.is_empty() {
            break;
        }
        for &pid in &next {
            signal(pid, libc::SIGSTOP);
        }
        wait_until_stopped(&next);
        found.extend(next);
    }
    for &pid in found.iter().rev() {
        signal(pid, libc::SIGKILL);
    }
    // Anything left in the root's own group that is no longer in its tree. The unreaped root still owns the group ID.
    // SAFETY: killpg has no memory-safety preconditions.
    unsafe {
        libc::killpg(root, libc::SIGKILL);
    }
}

fn signal(pid: i32, sig: i32) {
    // SAFETY: kill has no memory-safety preconditions; failures (the process is already gone) are expected.
    unsafe {
        libc::kill(pid, sig);
    }
}

/// Waits, up to [`STOP_WAIT`], until every process in `pids` is stopped, a zombie, or gone.
///
/// SIGSTOP takes effect asynchronously, and a process that is still running can fork or reap. A process in
/// uninterruptible sleep only stops when it wakes, which is why the wait is bounded: past it, the walk continues on a
/// best-effort basis rather than hanging cleanup.
fn wait_until_stopped(pids: &[i32]) {
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        let settled = pids
            .iter()
            .all(|&pid| matches!(read_stat(pid), None | Some((b'T' | b't' | b'Z' | b'X', _))));
        if settled {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Every process's children, keyed by parent PID, as of one pass over `/proc`.
#[cfg(target_os = "linux")]
fn children_by_parent() -> HashMap<i32, Vec<i32>> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return children;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some((_, ppid)) = read_stat(pid) {
            children.entry(ppid).or_default().push(pid);
        }
    }
    children
}

/// The state and parent PID of `pid`, or `None` if it no longer exists.
fn read_stat(pid: i32) -> Option<(u8, i32)> {
    parse_stat(&std::fs::read(format!("/proc/{pid}/stat")).ok()?)
}

/// Extracts the state character and parent PID from the contents of `/proc/<pid>/stat`.
///
/// The second field is the command name in parentheses. The process chooses it, so it may contain spaces, parentheses,
/// and bytes that are not UTF-8 (the kernel also truncates it at a byte boundary, which can split a UTF-8 character).
/// Parsing therefore works on bytes and starts after the *last* `)`; everything after it is ASCII.
pub fn parse_stat(stat: &[u8]) -> Option<(u8, i32)> {
    let rest = &stat[stat.iter().rposition(|&b| b == b')')? + 1..];
    let rest = std::str::from_utf8(rest).ok()?;
    let mut fields = rest.split_ascii_whitespace();
    let state = *fields.next()?.as_bytes().first()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Command names are chosen by the process and may contain spaces, parentheses, and non-UTF-8 bytes. A parser that
    /// misreads or rejects them makes the tree walk lose that process and everything below it.
    #[test]
    fn parses_stat_with_awkward_command_names() {
        assert_eq!(parse_stat(b"42 (sleep) S 7 42 42 0 -1"), Some((b'S', 7)));
        assert_eq!(parse_stat(b"42 (a) b (c) Z 9 42 42"), Some((b'Z', 9)));
        assert_eq!(parse_stat(b"42 (worker\xff) T 9 42 42"), Some((b'T', 9)));
        assert_eq!(parse_stat(b"garbage"), None);
    }
}
