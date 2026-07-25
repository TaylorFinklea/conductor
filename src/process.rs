//! Bounded subprocess execution for read-only evidence adapters and the
//! Musterroll CLI client.
//!
//! Mirrors the proven dispatch process-group mechanics — spawn as the leader
//! of a fresh group, address the group with a negative pid, and prove death
//! with [`crate::quarantine::process_group_alive`] — without exposing any
//! worker mutation API. Stdin is closed unless the caller explicitly supplies
//! bounded input (Cautionlight). Stdout and stderr are read concurrently
//! under strict byte caps.
//!
//! This module is deliberately feature-independent: `CommandMusterrollClient`
//! uses it in a `--no-default-features` build, so it must not live under the
//! `tui`-gated dashboard subtree.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// How long a signalled process (or group) gets to die before the next
/// escalation step. Matches `dispatch::KILL_GRACE` so a runaway evidence
/// command is given exactly the same benefit of the doubt as a runaway
/// worker.
const KILL_GRACE: Duration = Duration::from_secs(3);

/// Liveness poll interval. Shorter than `dispatch::WAIT_POLL` because the
/// dashboard blocks a refresh on this loop, and every wait here is bounded by
/// a deadline rather than by the poll count.
const WAIT_POLL: Duration = Duration::from_millis(5);

/// Read chunk size for the stdout/stderr pump threads.
const READ_CHUNK: usize = 8192;

#[derive(Debug, Clone)]
pub(crate) struct BoundedCommand {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    /// Bounded stdin payload. `None` means stdin is closed. Shared rather
    /// than owned so building or cloning a command never copies the (up to
    /// 4 MiB) buffer.
    pub(crate) stdin: Option<Arc<Vec<u8>>>,
    pub(crate) stdout_cap: usize,
    pub(crate) stderr_cap: usize,
    pub(crate) timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutcome {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// The clean exit code, or `None` when the command never exited on its
    /// own — it timed out, breached the stdout cap, or died by signal.
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
}

impl BoundedCommand {
    pub(crate) fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            stdin: None,
            stdout_cap: 4 * 1024 * 1024,
            stderr_cap: 256 * 1024,
            timeout: Duration::from_secs(60),
        }
    }

    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for arg in args {
            self.args.push(arg.into());
        }
        self
    }

    /// Supplies a bounded stdin payload. Accepts an `Arc` so a caller piping
    /// one already-bounded buffer (Afterfact stdout into Cautionlight) shares
    /// it instead of copying it on every refresh.
    ///
    /// Only the dashboard's Cautionlight adapter supplies stdin; a
    /// `--no-default-features` build reaches this module through Musterroll
    /// alone, where stdin is always closed.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub(crate) fn stdin(mut self, bytes: impl Into<Arc<Vec<u8>>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    pub(crate) fn stdout_cap(mut self, cap: usize) -> Self {
        self.stdout_cap = cap;
        self
    }

    pub(crate) fn stderr_cap(mut self, cap: usize) -> Self {
        self.stderr_cap = cap;
        self
    }

    pub(crate) fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs the command to completion under the configured caps and returns
    /// its bounded output.
    ///
    /// Returns `Err` only when the command could not be spawned or when the
    /// spawned process group could not be proven dead; a command that fails,
    /// times out, or overruns its caps is a successful *observation* and is
    /// reported through [`CommandOutcome`].
    pub(crate) fn run(&self) -> std::io::Result<CommandOutcome> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if self.stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        #[cfg(unix)]
        {
            // Leader of its own group, exactly like `dispatch`'s workers, so
            // `-pgid` reaches every descendant rather than just this child.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd.spawn()?;
        // The child leads the group it was just placed in, so its pid *is*
        // the group id.
        let pgid = child.id();

        if let Some(bytes) = &self.stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            let bytes = Arc::clone(bytes);
            // Detached on purpose: a child that never drains stdin must not
            // wedge the caller. The write fails with EPIPE once the group is
            // reaped, which releases the thread.
            thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            });
        }

        let stdout_breached = Arc::new(AtomicBool::new(false));
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let stdout_cap = self.stdout_cap;
        let breach_flag = Arc::clone(&stdout_breached);
        let stdout_reader = thread::spawn(move || {
            read_stream(stdout_pipe, stdout_cap, &CapPolicy::Abort(breach_flag))
        });

        let stderr_cap = self.stderr_cap;
        let stderr_reader =
            thread::spawn(move || read_stream(stderr_pipe, stderr_cap, &CapPolicy::Drain));

        let deadline = Instant::now() + self.timeout;
        let mut timed_out = false;
        let mut exit_code = None;
        let mut leader_reaped = false;

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // `try_wait` reaps on success, so the leader no longer
                    // pins `pgid` from here on.
                    exit_code = status.code();
                    leader_reaped = true;
                    break;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        break;
                    }
                    if stdout_breached.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(WAIT_POLL);
                }
                // The child is unwaitable; treat it as a runaway and let
                // termination below deal with the group.
                Err(_) => break,
            }
        }

        // Must precede the joins: a descendant holding the inherited pipe
        // write ends keeps both reader threads blocked until the whole group
        // is gone, so proving the group dead is what makes these joins
        // terminate at all.
        terminate_and_prove_dead(&mut child, pgid, leader_reaped)?;

        let (stdout, stdout_truncated) = stdout_reader.join().unwrap_or((Vec::new(), false));
        let (stderr, stderr_truncated) = stderr_reader.join().unwrap_or((Vec::new(), false));

        Ok(CommandOutcome {
            stdout,
            stderr,
            exit_code,
            timed_out,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

/// What a reader thread does once its byte cap is reached.
enum CapPolicy {
    /// Stop reading and raise the flag so the caller terminates the group.
    /// Used for stdout, where overrunning the cap means the output is
    /// unusable and the command has nothing left to contribute.
    Abort(Arc<AtomicBool>),
    /// Keep draining to EOF and discard the excess. Used for stderr, whose
    /// bytes are only a bounded display summary: leaving a full stderr pipe
    /// unread would block the child, turning an otherwise valid exit-1
    /// partial success into a timeout and discarding the events on stdout.
    Drain,
}

/// Reads `pipe` to EOF, retaining at most `cap` bytes. Returns the retained
/// bytes and whether anything was dropped.
fn read_stream<R: Read>(pipe: Option<R>, cap: usize, policy: &CapPolicy) -> (Vec<u8>, bool) {
    let Some(mut reader) = pipe else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; READ_CHUNK];
    let mut truncated = false;

    loop {
        match reader.read(&mut chunk) {
            // EOF, or a pipe that failed mid-read: either way there is no
            // more output to retain.
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = cap.saturating_sub(buf.len());
                if read > room {
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    match policy {
                        CapPolicy::Abort(flag) => {
                            flag.store(true, Ordering::Relaxed);
                            break;
                        }
                        CapPolicy::Drain => {}
                    }
                } else {
                    buf.extend_from_slice(&chunk[..read]);
                }
            }
        }
    }

    (buf, truncated)
}

/// Terminates whatever survived the wait loop and proves the whole group is
/// gone before the outcome is returned.
///
/// Ordering is the safety property. While the leader is un-reaped its process
/// table entry pins `pgid`, so the escalation below cannot land on a recycled
/// group id. Only after the leader is reaped is the residual sweep run, and
/// that sweep signals nothing unless [`crate::quarantine::process_group_alive`]
/// first reports surviving members — the same guarded shape as
/// `dispatch::ensure_process_group_quiescent`, which narrows the inherent
/// post-reap reuse window to a single liveness probe.
#[cfg(unix)]
fn terminate_and_prove_dead(
    child: &mut Child,
    pgid: u32,
    leader_reaped: bool,
) -> std::io::Result<()> {
    if !leader_reaped {
        signal_group(pgid, "-TERM");
        if !wait_for_leader_exit(child, KILL_GRACE) {
            signal_group(pgid, "-KILL");
            if !wait_for_leader_exit(child, KILL_GRACE) {
                // Deliberately no blocking `wait()`: a leader that survived
                // SIGKILL is uninterruptible, and hanging the caller on it
                // would be worse than reporting the failure.
                return Err(std::io::Error::other(format!(
                    "process {pgid} survived TERM/KILL escalation"
                )));
            }
        }
    }

    // Runs on every path, including a clean exit: descendants stay in the
    // group after the leader dies, and a leader that exits 0 having left one
    // behind is exactly the orphan case this must not miss.
    ensure_process_group_quiescent(pgid)
}

#[cfg(not(unix))]
fn terminate_and_prove_dead(
    child: &mut Child,
    _pgid: u32,
    leader_reaped: bool,
) -> std::io::Result<()> {
    if !leader_reaped {
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok(())
}

/// Proves the group led by `pgid` has no surviving members, escalating
/// TERM → KILL and failing closed if it cannot.
#[cfg(unix)]
fn ensure_process_group_quiescent(pgid: u32) -> std::io::Result<()> {
    if !crate::quarantine::process_group_alive(pgid) {
        return Ok(());
    }

    signal_group(pgid, "-TERM");
    if wait_for_process_group_exit(pgid, KILL_GRACE) {
        return Ok(());
    }

    signal_group(pgid, "-KILL");
    if wait_for_process_group_exit(pgid, KILL_GRACE) {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "process group {pgid} remained alive after TERM/KILL escalation"
        )))
    }
}

/// Sends `signal` to the whole process group led by `pgid`.
///
/// Mirrors `dispatch::send_signal_to_group`: a negative operand addresses the
/// group, and because the crate forbids `unsafe`, the `kill` binary stands in
/// for a direct `kill(2)`. Group ids 0 and 1 are refused outright — `-0` would
/// signal *this* process's own group.
#[cfg(unix)]
fn signal_group(pgid: u32, signal: &str) {
    if pgid <= 1 {
        return;
    }
    let _ = Command::new("kill")
        .arg(signal)
        .arg(format!("-{pgid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Polls until the leader is reaped or `timeout` elapses. Reaping here is what
/// keeps the failure path zombie-free.
#[cfg(unix)]
fn wait_for_leader_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(WAIT_POLL);
    }
}

#[cfg(unix)]
fn wait_for_process_group_exit(pgid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !crate::quarantine::process_group_alive(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(WAIT_POLL);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Emits the caller's own process-group id so a test can prove the group
    /// is gone after `run` returns rather than assuming it.
    const REPORT_PGID: &str = "import os,sys; open(sys.argv[1],'w').write(str(os.getpgrp()))";

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("undertake-proc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn read_pid(path: &std::path::Path) -> u32 {
        std::fs::read_to_string(path)
            .expect("helper must have written a pid file")
            .trim()
            .parse()
            .expect("pid file must hold a pid")
    }

    /// Stdout past the cap is dropped, the retained prefix is exactly the cap,
    /// and the spewing group is provably dead — not merely assumed dead.
    #[test]
    fn stdout_cap_truncates_and_group_is_proven_dead() {
        let pgid_file = scratch("cap-pgid");
        let script =
            format!("{REPORT_PGID}\nimport sys\nwhile True:\n    sys.stdout.write('x' * 65536)\n");
        let cmd = BoundedCommand::new("python3")
            .args(["-c", &script, pgid_file.to_str().unwrap()])
            .stdout_cap(64 * 1024)
            .timeout(Duration::from_secs(30));

        let outcome = cmd.run().expect("run command");

        assert_eq!(outcome.stdout.len(), 64 * 1024, "cap must bound the buffer");
        assert!(outcome.stdout_truncated);
        assert!(
            !outcome.timed_out,
            "cap breach must not be reported as a timeout"
        );
        assert_eq!(
            outcome.exit_code, None,
            "a command killed for overrunning its cap never exited cleanly"
        );
        assert!(
            !crate::quarantine::process_group_alive(read_pid(&pgid_file)),
            "the terminated group must be provably gone"
        );
        let _ = std::fs::remove_file(&pgid_file);
    }

    /// Stderr over its cap is drained rather than abandoned, so a valid exit-1
    /// partial success survives with a clipped summary. Abandoning the pipe
    /// would block the child and turn this into a timeout.
    #[test]
    fn stderr_over_cap_drains_and_preserves_exit_one_partial_success() {
        let script = "import sys\n\
             sys.stderr.write('e' * (512 * 1024))\n\
             sys.stdout.write('{\"schema\":\"x\"}\\n')\n\
             sys.exit(1)\n";
        let cmd = BoundedCommand::new("python3")
            .args(["-c", script])
            .stderr_cap(1024)
            .timeout(Duration::from_secs(30));

        let outcome = cmd.run().expect("run command");

        assert_eq!(outcome.exit_code, Some(1), "partial success must survive");
        assert!(!outcome.timed_out);
        assert_eq!(outcome.stderr.len(), 1024);
        assert!(outcome.stderr_truncated);
        assert!(!outcome.stdout_truncated);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout).trim(),
            "{\"schema\":\"x\"}"
        );
    }

    /// A command that never exits is classified as a timeout, returns near its
    /// deadline rather than near the child's lifetime, and leaves no group.
    #[test]
    fn never_exits_times_out_and_group_is_proven_dead() {
        let pgid_file = scratch("timeout-pgid");
        let script = format!("{REPORT_PGID}\nimport time\ntime.sleep(600)\n");
        let cmd = BoundedCommand::new("python3")
            .args(["-c", &script, pgid_file.to_str().unwrap()])
            .timeout(Duration::from_millis(200));

        let started = Instant::now();
        let outcome = cmd.run().expect("run command");
        let elapsed = started.elapsed();

        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        assert!(
            elapsed < Duration::from_secs(10),
            "must return on its own deadline, took {elapsed:?}"
        );
        assert!(!crate::quarantine::process_group_alive(read_pid(
            &pgid_file
        )));
        let _ = std::fs::remove_file(&pgid_file);
    }

    /// The discriminating orphan case: the leader exits *cleanly* while a
    /// descendant it spawned lives on in the group. Only the unconditional
    /// post-reap sweep can reach that descendant, and until it does, the
    /// descendant holds the inherited stdout pipe open.
    #[test]
    fn descendant_outliving_a_clean_leader_exit_is_terminated() {
        let pid_file = scratch("orphan-descendant");
        let script = "import subprocess, sys\n\
             child = subprocess.Popen(['python3', '-c', 'import time; time.sleep(600)'])\n\
             open(sys.argv[1], 'w').write(str(child.pid))\n\
             sys.stdout.write('done\\n')\n\
             sys.exit(0)\n";
        let cmd = BoundedCommand::new("python3")
            .args(["-c", script, pid_file.to_str().unwrap()])
            .timeout(Duration::from_secs(30));

        let started = Instant::now();
        let outcome = cmd.run().expect("run command");
        let elapsed = started.elapsed();

        assert_eq!(outcome.exit_code, Some(0), "the leader exited cleanly");
        assert!(!outcome.timed_out);
        assert!(
            elapsed < Duration::from_secs(20),
            "the orphan must be reaped instead of holding the pipes open, took {elapsed:?}"
        );
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "done");

        let descendant = read_pid(&pid_file);
        assert!(
            !crate::quarantine::process_alive(descendant),
            "descendant {descendant} outlived its group"
        );
        let _ = std::fs::remove_file(&pid_file);
    }

    /// A descendant of a *timed-out* leader dies with the group too.
    #[test]
    fn descendant_of_a_timed_out_leader_is_terminated() {
        let pid_file = scratch("timeout-descendant");
        let script = "import subprocess, sys, time\n\
             child = subprocess.Popen(['python3', '-c', 'import time; time.sleep(600)'])\n\
             open(sys.argv[1], 'w').write(str(child.pid))\n\
             time.sleep(600)\n";
        let cmd = BoundedCommand::new("python3")
            .args(["-c", script, pid_file.to_str().unwrap()])
            .timeout(Duration::from_millis(500));

        let outcome = cmd.run().expect("run command");

        assert!(outcome.timed_out);
        let descendant = read_pid(&pid_file);
        assert!(
            !crate::quarantine::process_alive(descendant),
            "descendant {descendant} outlived its timed-out group"
        );
        let _ = std::fs::remove_file(&pid_file);
    }

    /// Exit 1 is an observation, not a spawn failure: the JSONL written before
    /// it must survive intact.
    #[test]
    fn exit_1_with_valid_jsonl_is_preserved() {
        let script = r#"import sys; sys.stdout.write('{"status":"ok"}\n'); sys.exit(1)"#;
        let cmd = BoundedCommand::new("python3")
            .args(["-c", script])
            .timeout(Duration::from_secs(30));

        let outcome = cmd.run().expect("run command");

        assert_eq!(outcome.exit_code, Some(1));
        assert!(!outcome.timed_out);
        assert!(!outcome.stdout_truncated);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout).trim(),
            "{\"status\":\"ok\"}"
        );
    }

    #[test]
    fn exit_2_is_reported_verbatim() {
        let cmd = BoundedCommand::new("python3")
            .args(["-c", "import sys; sys.exit(2)"])
            .timeout(Duration::from_secs(30));

        let outcome = cmd.run().expect("run command");

        assert_eq!(outcome.exit_code, Some(2));
        assert!(!outcome.timed_out);
    }

    /// Stdin is closed unless explicitly supplied, so an evidence command that
    /// reads stdin sees EOF instead of inheriting the terminal.
    #[test]
    fn stdin_is_closed_by_default() {
        let cmd = BoundedCommand::new("python3")
            .args(["-c", "import sys; sys.stdout.write(repr(sys.stdin.read()))"])
            .timeout(Duration::from_secs(30));

        let outcome = cmd.run().expect("run command");

        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "''");
    }

    /// Explicitly supplied stdin is delivered whole, shared rather than copied.
    #[test]
    fn supplied_stdin_is_delivered() {
        let payload = Arc::new(b"{\"schema\":\"cautionlight/finding@1\"}\n".to_vec());
        let cmd = BoundedCommand::new("python3")
            .args(["-c", "import sys; sys.stdout.write(sys.stdin.read())"])
            .stdin(Arc::clone(&payload))
            .timeout(Duration::from_secs(30));
        assert_eq!(
            Arc::strong_count(&payload),
            2,
            "the command must share the payload, not copy it"
        );

        let outcome = cmd.run().expect("run command");

        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout, *payload);
    }

    /// A missing program is a spawn error, never a fabricated outcome.
    #[test]
    fn missing_program_is_a_spawn_error() {
        let error = BoundedCommand::new("undertake-no-such-evidence-binary")
            .timeout(Duration::from_secs(5))
            .run()
            .expect_err("spawn must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
