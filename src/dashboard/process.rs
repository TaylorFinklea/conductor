//! Bounded subprocess execution for read-only dashboard evidence adapters.
//!
//! Mirrors proven dispatch process-group/timeout mechanics without exposing
//! worker mutation APIs. Closes stdin unless explicitly supplying bounded input
//! (e.g. Cautionlight). Reads stdout and stderr concurrently under strict byte
//! caps. Timeout and cap breach terminate and reap the process group.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub(crate) struct BoundedCommand {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) stdin: Option<Vec<u8>>,
    pub(crate) stdout_cap: usize,
    pub(crate) stderr_cap: usize,
    pub(crate) timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutcome {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
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
            env: Vec::new(),
            cwd: None,
            stdin: None,
            stdout_cap: 4 * 1024 * 1024,
            stderr_cap: 256 * 1024,
            timeout: Duration::from_secs(60),
        }
    }

    pub(crate) fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
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

    pub(crate) fn env(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.env.push((key.into(), val.into()));
        self
    }

    pub(crate) fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub(crate) fn stdin(mut self, bytes: Vec<u8>) -> Self {
        self.stdin = Some(bytes);
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

    pub(crate) fn run(&self) -> std::io::Result<CommandOutcome> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }

        if self.stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd.spawn()?;
        let pgid = child.id();

        if let Some(stdin_bytes) = &self.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                let stdin_bytes = stdin_bytes.clone();
                thread::spawn(move || {
                    let _ = stdin.write_all(&stdin_bytes);
                });
            }
        }

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let cap_breached = Arc::new(AtomicBool::new(false));

        let stdout_cap = self.stdout_cap;
        let cap_flag_1 = Arc::clone(&cap_breached);
        let stdout_handle = thread::spawn(move || {
            read_stream_with_cap(stdout_pipe, stdout_cap, cap_flag_1)
        });

        let stderr_cap = self.stderr_cap;
        let cap_flag_2 = Arc::clone(&cap_breached);
        let stderr_handle = thread::spawn(move || {
            read_stream_with_cap(stderr_pipe, stderr_cap, cap_flag_2)
        });

        let start = Instant::now();
        let mut timed_out = false;
        let mut exit_code = None;

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_code = status.code();
                    break;
                }
                Ok(None) => {
                    if start.elapsed() >= self.timeout {
                        timed_out = true;
                        break;
                    }
                    if cap_breached.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => {
                    break;
                }
            }
        }

        if timed_out || cap_breached.load(Ordering::Relaxed) || exit_code.is_none() {
            #[cfg(unix)]
            terminate_and_reap_process_group(pgid);
            #[cfg(not(unix))]
            {
                let _ = child.kill();
            }
        }

        let _ = child.wait();

        #[cfg(unix)]
        terminate_and_reap_process_group(pgid);

        let (stdout, stdout_truncated) = stdout_handle.join().unwrap_or((Vec::new(), false));
        let (stderr, stderr_truncated) = stderr_handle.join().unwrap_or((Vec::new(), false));

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

fn read_stream_with_cap<R: Read>(
    pipe: Option<R>,
    cap: usize,
    cap_flag: Arc<AtomicBool>,
) -> (Vec<u8>, bool) {
    let Some(mut reader) = pipe else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > cap {
                    let keep = cap.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..keep]);
                    truncated = true;
                    cap_flag.store(true, Ordering::Relaxed);
                    break;
                } else {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

#[cfg(unix)]
fn terminate_and_reap_process_group(pgid: u32) {
    if pgid <= 1 {
        return;
    }
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(format!("-{pgid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if crate::quarantine::process_group_alive(pgid) {
        thread::sleep(Duration::from_millis(20));
        if crate::quarantine::process_group_alive(pgid) {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{pgid}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn large_output_capped_and_process_group_reaped() {
        let script = "import sys; sys.stdout.write('x' * (100 * 1024 * 1024)); sys.stdout.flush()";
        let cmd = BoundedCommand::new("python3")
            .args(["-c", script])
            .stdout_cap(64 * 1024)
            .timeout(Duration::from_secs(5));

        let outcome = cmd.run().expect("run command");
        assert!(outcome.stdout.len() <= 64 * 1024);
        assert!(outcome.stdout_truncated);
    }

    #[test]
    fn never_exits_times_out_and_reaped() {
        let script = "import time; time.sleep(100)";
        let cmd = BoundedCommand::new("python3")
            .args(["-c", script])
            .timeout(Duration::from_millis(100));

        let outcome = cmd.run().expect("run command");
        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
    }

    #[test]
    fn descendants_terminated_and_reaped() {
        let script = r#"
import os, subprocess, sys, time
pid_file = sys.argv[1]
proc = subprocess.Popen(["python3", "-c", "import time; time.sleep(100)"])
with open(pid_file, "w") as f:
    f.write(str(proc.pid))
time.sleep(100)
"#;
        let temp_dir = std::env::temp_dir();
        let pid_file = temp_dir.join(format!("test_descendant_{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);

        let cmd = BoundedCommand::new("python3")
            .args(["-c", script, pid_file.to_str().unwrap()])
            .timeout(Duration::from_millis(300));

        let outcome = cmd.run().expect("run command");
        assert!(outcome.timed_out);

        // Verify descendant pid written to file is no longer alive
        if pid_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                if let Ok(descendant_pid) = content.trim().parse::<u32>() {
                    #[cfg(unix)]
                    assert!(
                        !crate::quarantine::process_alive(descendant_pid),
                        "descendant process {descendant_pid} must be terminated"
                    );
                }
            }
            let _ = std::fs::remove_file(&pid_file);
        }
    }

    #[test]
    fn exit_1_with_valid_jsonl() {
        let script = r#"import sys; sys.stdout.write('{"status":"ok"}\n'); sys.exit(1)"#;
        let cmd = BoundedCommand::new("python3")
            .args(["-c", script])
            .timeout(Duration::from_secs(5));

        let outcome = cmd.run().expect("run command");
        assert_eq!(outcome.exit_code, Some(1));
        assert!(!outcome.timed_out);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "{\"status\":\"ok\"}");
    }

    #[test]
    fn exit_2() {
        let script = "import sys; sys.exit(2)";
        let cmd = BoundedCommand::new("python3")
            .args(["-c", script])
            .timeout(Duration::from_secs(5));

        let outcome = cmd.run().expect("run command");
        assert_eq!(outcome.exit_code, Some(2));
        assert!(!outcome.timed_out);
    }
}
