//! Containment for agent processes: starting them so they can be cleaned up, and cleaning them up completely.
//!
//! `LIF-1` requires that nothing an agent started keeps running once devbox-mcp stops tracking it. Killing the agent's
//! process group is not enough for real agents: Codex starts every shell command in a new session (and so a new process
//! group), and only its direct children get Codex's own parent-death signal. A command's children therefore escape a
//! group kill. Observed with codex-cli 0.162.1: `bash -c "sleep 303 & sleep 120"` left both sleeps running after Codex
//! was killed.
//!
//! So an abandoned agent is cleaned up by walking its process tree through `/proc`, which follows parent links across
//! sessions and groups. Remaining known gaps, which need a cgroup to close:
//!
//! - A process that was already orphaned (its parent exited, so it was reparented away from the agent's tree) before
//!   cleanup cannot be found. Codex kills a finished command's process group itself, so this needs something like a
//!   double-forking daemon.
//! - If devbox-mcp itself dies abruptly, only the agent's direct parent-death signal fires.
//!
//! Agent-agnostic: adapters use [`contain`] when building the command and [`TreeGuard`] around the running child.

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

/// Kills an agent and everything it started when dropped, unless disarmed first.
///
/// Create it right after spawning a child prepared with [`contain`]. While it is armed, the child must not have been
/// reaped: its PID (and process group ID) then still belong to it, so signalling them cannot hit an unrelated process.
/// After reaping the child, call [`TreeGuard::reaped`] immediately, with no `.await` in between.
pub struct TreeGuard {
    pid: Option<i32>,
}

impl TreeGuard {
    pub fn new(pid: Option<u32>) -> Self {
        Self {
            pid: pid.and_then(|p| i32::try_from(p).ok()),
        }
    }

    /// Cleans up after a child that exited on its own and has just been reaped, then disarms the guard.
    ///
    /// The child's descendants were reparented away when it exited, so there is no tree left to walk; this kills
    /// whatever is still in the child's process group. The group ID cannot have been reused yet if the group still has
    /// members, and if it has none, the PID was reaped only moments ago; doing this without an intervening `.await`
    /// keeps that window as small as it can be without pidfds.
    pub fn reaped(&mut self) {
        if let Some(pid) = self.pid.take() {
            // SAFETY: killpg has no memory-safety preconditions. ESRCH (an empty group) is the normal case.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_tree(pid);
        }
    }
}

/// Kills `root`, every descendant of it (whatever their session or process group), and `root`'s process group.
///
/// Everything is stopped before anything is killed: a killed process's children are reparented away from the tree at
/// once, and a running one can fork new children while the tree is being walked. Stopped processes can do neither, so
/// stopping until a walk finds nothing new and then killing what was found catches the whole tree.
fn kill_tree(root: i32) {
    let mut found = vec![root];
    signal(root, libc::SIGSTOP);
    #[cfg(target_os = "linux")]
    for _ in 0..16 {
        let new: Vec<i32> = descendants(root)
            .into_iter()
            .filter(|p| !found.contains(p))
            .collect();
        if new.is_empty() {
            break;
        }
        for &pid in &new {
            signal(pid, libc::SIGSTOP);
        }
        found.extend(new);
    }
    for &pid in &found {
        signal(pid, libc::SIGKILL);
    }
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

/// All current descendants of `root`, found by reading every process's parent from `/proc`.
#[cfg(target_os = "linux")]
fn descendants(root: i32) -> Vec<i32> {
    let mut children: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some(ppid) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| parse_stat(&stat))
            .map(|(_, ppid)| ppid)
        {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut out = Vec::new();
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            out.push(child);
            queue.push(child);
        }
    }
    out
}

/// Extracts the state character and parent PID from the contents of `/proc/<pid>/stat`.
///
/// The second field is the command name in parentheses, which may itself contain spaces and parentheses, so parsing
/// starts after the *last* `)`.
pub fn parse_stat(stat: &str) -> Option<(char, i32)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Command names are chosen by the process and may contain spaces and parentheses; a naive split would misread the
    /// parent PID and make the tree walk miss or misattribute processes.
    #[test]
    fn parses_stat_with_awkward_command_names() {
        assert_eq!(parse_stat("42 (sleep) S 7 42 42 0 -1"), Some(('S', 7)));
        assert_eq!(parse_stat("42 (a) b (c) Z 9 42 42"), Some(('Z', 9)));
        assert_eq!(parse_stat("garbage"), None);
    }
}
