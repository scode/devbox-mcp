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
use std::ffi::OsString;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
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

    /// Kills the child and its whole process tree now, returning whether that could be confirmed (see [`kill_tree`]).
    ///
    /// This sleeps while it waits for processes to stop, so async code must call it on a blocking thread
    /// (`spawn_blocking`): on devbox-mcp's single runtime thread it would stall every other call.
    pub fn kill(mut self) -> bool {
        match self.pid.take() {
            Some(pid) => kill_tree(pid),
            None => true,
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
///
/// Returns whether the whole tree could be confirmed killed: false when some process did not stop within
/// [`STOP_WAIT`] (so it may have forked or reaped meanwhile), when the generation limit was reached, or where the
/// `/proc` walk is unavailable. Callers report that as uncertain cleanup rather than claiming success (`LIF-2`).
fn kill_tree(root: i32) -> bool {
    // Every process found, ancestors before descendants.
    let mut found = vec![root];
    signal(root, libc::SIGSTOP);
    let mut confirmed = wait_until_stopped(&[root]);
    #[cfg(target_os = "linux")]
    {
        let mut walked_to_the_end = false;
        for _ in 0..MAX_GENERATIONS {
            let (children, complete) = children_by_parent();
            confirmed &= complete;
            let next: Vec<i32> = found
                .iter()
                .flat_map(|p| children.get(p).into_iter().flatten().copied())
                .filter(|p| !found.contains(p))
                .collect();
            if next.is_empty() {
                walked_to_the_end = true;
                break;
            }
            for &pid in &next {
                signal(pid, libc::SIGSTOP);
            }
            confirmed &= wait_until_stopped(&next);
            found.extend(next);
        }
        confirmed &= walked_to_the_end;
    }
    #[cfg(not(target_os = "linux"))]
    {
        confirmed = false;
    }
    for &pid in found.iter().rev() {
        signal(pid, libc::SIGKILL);
    }
    // Anything left in the root's own group that is no longer in its tree. The unreaped root still owns the group ID.
    // SAFETY: killpg has no memory-safety preconditions.
    unsafe {
        libc::killpg(root, libc::SIGKILL);
    }
    confirmed
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
/// best-effort basis rather than hanging cleanup. Returns whether everything settled in time.
fn wait_until_stopped(pids: &[i32]) -> bool {
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        let settled = pids.iter().all(|&pid| {
            matches!(
                read_stat(pid),
                Ok(None | Some((b'T' | b't' | b'Z' | b'X', _)))
            )
        });
        if settled {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

/// Every process's children, keyed by parent PID, as of one pass over `/proc`, and whether the pass saw everything.
///
/// An incomplete pass (`/proc` unreadable, or a process's record unreadable for a reason other than the process having
/// exited) does not stop the cleanup, but it means the cleanup cannot be confirmed.
#[cfg(target_os = "linux")]
fn children_by_parent() -> (HashMap<i32, Vec<i32>>, bool) {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut complete = true;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return (children, false);
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        match read_stat(pid) {
            Ok(Some((_, ppid))) => children.entry(ppid).or_default().push(pid),
            Ok(None) => {}
            Err(()) => complete = false,
        }
    }
    (children, complete)
}

/// The state and parent PID of `pid`, `Ok(None)` if it no longer exists, or `Err` if its record could not be read or
/// parsed (which must not be mistaken for the process being gone).
fn read_stat(pid: i32) -> Result<Option<(u8, i32)>, ()> {
    match std::fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_stat(&stat).map(Some).ok_or(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
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

// ---------------------------------------------------------------------------------------------------------------------
// Running an agent process
// ---------------------------------------------------------------------------------------------------------------------

/// Upper bound on how much of an agent's stderr is kept for error reporting.
///
/// stderr is drained concurrently so a chatty child can never block on a full pipe, but only the head is kept: it is
/// diagnostic text for a failed turn, not something a caller needs in full.
const MAX_STDERR_BYTES: usize = 16 * 1024;

/// How long to keep reading an exited agent's output pipes before giving up on them.
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// One agent process to run: what an agent module decides, with nothing agent-specific left in it.
#[derive(Debug, Clone)]
pub struct AgentCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Working directory of the agent process.
    pub cwd: PathBuf,
    /// Written to the agent's stdin, which is then closed. Prompts go here so their size and content never touch the
    /// command line.
    pub stdin: Vec<u8>,
}

/// How an agent run ended.
#[derive(Debug)]
pub enum Ended {
    /// The agent exited on its own; whatever was left in its process group has been killed and it has been reaped.
    Exited {
        status: std::process::ExitStatus,
        /// The head of its stderr, for explaining failures.
        stderr: String,
    },
    /// The run was stopped (interrupted, or the line handler asked to stop): the agent and its whole process tree were
    /// killed and the agent reaped. `cleanup_confirmed` is false when the tree walk could not confirm that everything
    /// stopped (see [`kill_tree`]).
    Stopped { cleanup_confirmed: bool },
}

/// Runs one contained agent process to its end, passing each line of its stdout to `on_line` as it arrives.
///
/// `on_line` returns [`ControlFlow::Break`] to have the run stopped (an agent module uses this when the agent does
/// something it must not continue with). When `interrupt` completes first, the run is stopped the same way.
///
/// Two shapes of agent behavior must not hang a run. Stdin is written while stdout and stderr are already being read,
/// so an agent that writes a lot before reading its input cannot deadlock against a large prompt. And the run ends
/// when the agent exits, not when its output pipes close, because something the agent started in the background may
/// hold them open indefinitely; the pipes are then drained for at most [`PIPE_DRAIN_TIMEOUT`].
///
/// If this future is dropped before it finishes (devbox-mcp shutting down), the guard kills the agent's tree as it is
/// dropped (`LIF-1`).
pub async fn run_agent(
    command: AgentCommand,
    mut on_line: impl FnMut(&str) -> ControlFlow<()>,
    interrupt: impl Future<Output = ()>,
) -> anyhow::Result<Ended> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .current_dir(&command.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    contain(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to start {}", command.program.display()))?;
    let mut guard = TreeGuard::new(child.id());
    let pid = child
        .id()
        .context("agent exited before it could be tracked")?;

    let stderr = child.stderr.take().context("child stderr missing")?;
    let mut stderr_task = AbortOnDrop(tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES)));
    let mut stdin = child.stdin.take().context("child stdin missing")?;
    let input = command.stdin;
    // A write error here means the agent exited without reading its input; that shows up in how the run ends.
    let _stdin_task = AbortOnDrop(tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
        // Dropping stdin closes it, which tells the agent its input is complete.
    }));

    let mut lines = BufReader::new(child.stdout.take().context("child stdout missing")?).lines();
    // Watch for the agent's exit without reaping it, so its PID and process group still belong to it while the guard
    // cleans up; reaping happens only afterwards.
    let exited = exited(pid);
    tokio::pin!(exited);
    tokio::pin!(interrupt);
    enum Next {
        Exit(std::io::Result<()>),
        Stop,
    }
    let mut stdout_open = true;
    let next = loop {
        tokio::select! {
            line = lines.next_line(), if stdout_open => match line.context("failed to read agent output")? {
                Some(line) => {
                    if on_line(&line).is_break() {
                        break Next::Stop;
                    }
                }
                // The agent closed stdout, but it may keep running: keep watching for its exit and for interrupts.
                None => stdout_open = false,
            },
            result = &mut exited => break Next::Exit(result),
            () = &mut interrupt => break Next::Stop,
        }
    };

    let ended = match next {
        Next::Stop => {
            // The child goes to the cleanup thread together with the guard. If it stayed here and this future were
            // dropped mid-cleanup, `kill_on_drop` would kill the root before the tree walk had found its descendants,
            // and the child could be reaped while the guard still signals its PID.
            let (mut child, cleanup_confirmed) = tokio::task::spawn_blocking(move || {
                let confirmed = guard.kill();
                (child, confirmed)
            })
            .await
            .context("process cleanup task failed")?;
            child.wait().await.context("failed to reap agent")?;
            Ended::Stopped { cleanup_confirmed }
        }
        Next::Exit(Err(err)) => {
            // Without a confirmed unreaped exit, the PID may no longer be the agent's, so it must not be signalled.
            guard.disarm();
            return Err(err).context("failed to wait for agent");
        }
        Next::Exit(Ok(())) => {
            guard.exited();
            let status = child.wait().await.context("failed to reap agent")?;
            Ended::Exited {
                status,
                stderr: String::new(),
            }
        }
    };

    // The agent is gone, so everything it wrote is already in the pipes or in the reader's buffer; read the rest, so
    // output written just before an exit or an interrupt is not lost. Only briefly, though: a background process it
    // started in another session may still hold the pipes open. Both pipes share one deadline so such a leftover delays
    // the result by at most PIPE_DRAIN_TIMEOUT.
    let deadline = tokio::time::Instant::now() + PIPE_DRAIN_TIMEOUT;
    let drain = async {
        while let Some(line) = lines.next_line().await? {
            // The agent is gone, so a stop request has nothing left to stop.
            let _ = on_line(&line);
        }
        anyhow::Ok(())
    };
    if let Ok(result) = tokio::time::timeout_at(deadline, drain).await {
        result.context("failed to read agent output")?;
    }
    Ok(match ended {
        Ended::Exited { status, .. } => Ended::Exited {
            status,
            stderr: tokio::time::timeout_at(deadline, &mut stderr_task.0)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default(),
        },
        stopped => stopped,
    })
}

/// Aborts a spawned task when dropped, so helper tasks never outlive the run that started them.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Reads at most `limit` bytes of a stream as lossy UTF-8, then keeps draining (and discarding) the rest so the writer
/// never blocks on a full pipe.
pub async fn read_bounded(mut reader: impl tokio::io::AsyncRead + Unpin, limit: usize) -> String {
    let mut kept = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
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

    /// A small shell "agent" that starts a background `sleep` in a new session (as Codex does with shell commands),
    /// prints its PID, and then waits forever.
    fn hanging_agent() -> AgentCommand {
        AgentCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                "setsid sleep 1000 & echo $!; read -r line; echo \"got $line\"; wait".into(),
            ],
            cwd: PathBuf::from("/"),
            stdin: b"hello\n".to_vec(),
        }
    }

    fn is_running(pid: i32) -> bool {
        matches!(read_stat(pid), Ok(Some((state, _))) if state != b'Z')
    }

    /// An interrupt stops the run, kills the agent's whole tree (including what it started in another session), and
    /// reports the cleanup as confirmed. This is the path `session_interrupt` relies on (`TRN-4`).
    #[tokio::test]
    async fn interrupt_kills_the_tree() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut tx = Some(tx);
        let mut lines = Vec::new();
        let ended = run_agent(
            hanging_agent(),
            |line| {
                lines.push(line.to_owned());
                if line.starts_with("got") {
                    tx.take().map(|tx| tx.send(()));
                }
                ControlFlow::Continue(())
            },
            async {
                let _ = rx.await;
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(
                ended,
                Ended::Stopped {
                    cleanup_confirmed: true
                }
            ),
            "{ended:?}"
        );
        assert_eq!(lines[1], "got hello", "stdin was not delivered");
        let sleep_pid: i32 = lines[0].parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_running(sleep_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !is_running(sleep_pid),
            "background process survived the interrupt"
        );
    }

    /// A line handler that asks to stop gets the same treatment as an interrupt, which is how an agent module stops a
    /// turn it must not continue (a resume that landed in the wrong conversation).
    #[tokio::test]
    async fn line_handler_can_stop_the_run() {
        let ended = run_agent(
            hanging_agent(),
            |_| ControlFlow::Break(()),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert!(matches!(ended, Ended::Stopped { .. }), "{ended:?}");
    }

    /// An agent that closes its stdout but keeps running must still be interruptible (`TRN-4`); stdout ending is not
    /// the agent ending.
    #[tokio::test]
    async fn interrupt_works_after_stdout_closes() {
        let command = AgentCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exec 1>&-; sleep 1000".into()],
            cwd: PathBuf::from("/"),
            stdin: Vec::new(),
        };
        let started = Instant::now();
        let ended = run_agent(
            command,
            |_| ControlFlow::Continue(()),
            tokio::time::sleep(Duration::from_millis(200)),
        )
        .await
        .unwrap();
        assert!(matches!(ended, Ended::Stopped { .. }), "{ended:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// Output the agent wrote before an interrupt is still delivered, so an interrupted turn does not lose events (for
    /// example the native session ID) that were already in the pipe.
    #[tokio::test]
    async fn interrupt_delivers_buffered_output() {
        let command = AgentCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "seq 1 100; sleep 1000".into()],
            cwd: PathBuf::from("/"),
            stdin: Vec::new(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut tx = Some(tx);
        let mut lines = Vec::new();
        let ended = run_agent(
            command,
            |line| {
                lines.push(line.to_owned());
                tx.take().map(|tx| tx.send(()));
                ControlFlow::Continue(())
            },
            async {
                let _ = rx.await;
            },
        )
        .await
        .unwrap();
        assert!(matches!(ended, Ended::Stopped { .. }), "{ended:?}");
        assert_eq!(lines.len(), 100);
    }

    /// An agent that exits on its own is reported with its exit status and stderr.
    #[tokio::test]
    async fn reports_a_normal_exit() {
        let command = AgentCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "echo out; echo err >&2; exit 3".into()],
            cwd: PathBuf::from("/"),
            stdin: Vec::new(),
        };
        let mut lines = Vec::new();
        let ended = run_agent(
            command,
            |line| {
                lines.push(line.to_owned());
                ControlFlow::Continue(())
            },
            std::future::pending(),
        )
        .await
        .unwrap();
        let Ended::Exited { status, stderr } = ended else {
            panic!("{ended:?}");
        };
        assert_eq!(status.code(), Some(3));
        assert_eq!(stderr, "err\n");
        assert_eq!(lines, ["out"]);
    }
}
