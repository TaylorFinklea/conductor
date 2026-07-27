//! model-bench.jsonl appender

#![allow(dead_code)]

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;

use serde::Serialize;
use fs2::FileExt as _;

pub(crate) type Result<T> = std::result::Result<T, LedgerError>;

#[derive(Debug, Clone)]
pub(crate) struct LedgerError {
    message: String,
}

impl LedgerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LedgerError {}

/// One Undertake dispatch row for `~/.claude/model-bench.jsonl`.
///
/// The row carries dispatch identity and verifier evidence. Review-specific
/// evidence lives in `AdversarialLedgerRow`; Arena result fields are retired.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LedgerRow {
    pub(crate) date: String,
    pub(crate) model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) role: String,
    /// Generic Undertake job category, when this row records a staged run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) job: Option<String>,
    /// Generic stage within a staged run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stage: Option<String>,
    /// Immutable approved execution identity, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) execution_key: Option<String>,
    /// Provider selected for this execution, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<String>,
    /// Digest of the exact backend input, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) input_sha256: Option<String>,
    /// Digest of the backend output, including malformed output when returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_sha256: Option<String>,
    /// Monotonic per-stage invocation attempt, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attempt: Option<u8>,
    /// Observed backend outcome for an invocation, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<String>,
    /// Harness-reported token evidence. Unknown telemetry is represented as null.
    pub(crate) tokens: Option<u64>,
    pub(crate) task: String,
    pub(crate) verify_passed: bool,
    pub(crate) complexity: String,
    pub(crate) project: String,
    pub(crate) notes: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failure_reason: Option<String>,
    pub(crate) duration_ms: Option<u64>,
}

/// Structured adversarial-review metadata layered onto the shared model-bench row.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AdversarialLedgerRow {
    #[serde(flatten)]
    pub(crate) base: LedgerRow,
    pub(crate) review_id: String,
    pub(crate) provider: String,
    pub(crate) attempt_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reviewer_id: Option<String>,
    pub(crate) schema_valid: bool,
}

/// Appends one JSON row and trailing newline to `path`, creating parent dirs.
pub(crate) fn append(path: &Path, row: &LedgerRow) -> Result<()> {
    append_serialized(path, row)
}

/// Appends one adversarial attempt row with structured review metadata.
pub(crate) fn append_adversarial(path: &Path, row: &AdversarialLedgerRow) -> Result<()> {
    append_serialized(path, row)
}

fn append_serialized(path: &Path, row: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| LedgerError::new(format!("ledger path {} has no parent", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        LedgerError::new(format!(
            "failed to create ledger dir {}: {error}",
            parent.display()
        ))
    })?;
    let created = match std::fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(LedgerError::new(format!(
                "failed to inspect ledger {}: {error}",
                path.display()
            )));
        }
    };

    let mut new_row = serde_json::to_vec(row)
        .map_err(|error| LedgerError::new(format!("failed to serialize ledger row: {error}")))?;
    new_row.push(b'\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| {
            LedgerError::new(format!("failed to open ledger {}: {error}", path.display()))
        })?;
    file.lock_exclusive().map_err(|error| {
        LedgerError::new(format!("failed to lock ledger {}: {error}", path.display()))
    })?;
    let original_len = file.metadata().map_err(|error| {
        LedgerError::new(format!("failed to stat ledger {}: {error}", path.display()))
    })?.len();

    let append_result = write_one_row(&mut file, &new_row).and_then(|()| file.sync_data());
    if let Err(error) = append_result {
        let rollback = file.set_len(original_len).and_then(|()| file.sync_data());
        return Err(match rollback {
            Ok(()) => LedgerError::new(format!(
                "failed to append complete ledger row to {}: {error}",
                path.display()
            )),
            Err(rollback_error) => LedgerError::new(format!(
                "failed to append complete ledger row to {}: {error}; \
                 rollback to {original_len} bytes also failed: {rollback_error}",
                path.display()
            )),
        });
    }
    if created {
        sync_directory(parent).map_err(|error| {
            LedgerError::new(format!(
                "failed to sync ledger directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    Ok(())
}

fn write_one_row(file: &mut File, row: &[u8]) -> io::Result<()> {
    loop {
        match file.write(row) {
            Ok(written) if written == row.len() => return Ok(()),
            Ok(written) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("ledger row write stopped after {written} of {} bytes", row.len()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn append_writes_one_dispatch_row() {
        let temp = TempDir::new("ledger");
        let path = temp.path().join("model-bench.jsonl");
        let row = LedgerRow {
            date: "2026-07-02".to_string(),
            model: "fake-worker".to_string(),
            harness: None,
            profile: None,
            reasoning_effort: None,
            role: "implement".to_string(),
            job: None,
            stage: None,
            execution_key: None,
            provider: None,
            input_sha256: None,
            output_sha256: None,
            attempt: None,
            outcome: None,
            tokens: None,
            task: "sandbox-1".to_string(),
            verify_passed: true,
            complexity: "S".to_string(),
            project: "sandbox-repo".to_string(),
            notes: "undertake cycle-1: verified".to_string(),
            failure_reason: None,
            duration_ms: None,
        };

        append(&path, &row).expect("append ledger");

        let content = std::fs::read_to_string(&path).expect("read ledger");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).expect("json row");
        assert_eq!(parsed["date"], json!("2026-07-02"));
        assert_eq!(parsed["model"], json!("fake-worker"));
        assert_eq!(parsed["role"], json!("implement"));
        assert_eq!(parsed["task"], json!("sandbox-1"));
        assert_eq!(parsed["verify_passed"], json!(true));
        assert_eq!(parsed["complexity"], json!("S"));
        assert_eq!(parsed["project"], json!("sandbox-repo"));
        assert_eq!(parsed["notes"], json!("undertake cycle-1: verified"));
        assert!(parsed.get("reasoning_effort").is_none());
    }

    #[test]
    fn adversarial_append_serializes_structured_attempt_metadata() {
        let temp = TempDir::new("ledger-adversarial");
        let path = temp.path().join("model-bench.jsonl");
        let row = AdversarialLedgerRow {
            base: LedgerRow {
                date: "2026-07-15".to_string(),
                model: "openai-codex/gpt-5.6-luna".to_string(),
                harness: Some("pi".to_string()),
                profile: Some("luna-reviewer".to_string()),
                reasoning_effort: Some("high".to_string()),
                role: "adversarial-reviewer".to_string(),
                job: None,
                stage: None,
                execution_key: None,
                provider: None,
                input_sha256: None,
                output_sha256: None,
                attempt: None,
                outcome: None,
                tokens: None,
                task: "review-123".to_string(),
                verify_passed: false,
                complexity: "L".to_string(),
                project: "undertake".to_string(),
                notes: "reviewer schema failure".to_string(),
                failure_reason: Some("invalid JSON".to_string()),
                duration_ms: Some(17),
            },
            review_id: "review-123".to_string(),
            provider: "openai".to_string(),
            attempt_kind: "repair".to_string(),
            reviewer_id: Some("R1".to_string()),
            schema_valid: false,
        };

        append_adversarial(&path, &row).expect("append adversarial row");

        let parsed: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(parsed["role"], json!("adversarial-reviewer"));
        assert_eq!(parsed["review_id"], json!("review-123"));
        assert_eq!(parsed["provider"], json!("openai"));
        assert_eq!(parsed["attempt_kind"], json!("repair"));
        assert_eq!(parsed["reviewer_id"], json!("R1"));
        assert_eq!(parsed["schema_valid"], json!(false));
        assert_eq!(parsed["failure_reason"], json!("invalid JSON"));
    }

    fn minimal_row(task: &str) -> LedgerRow {
        LedgerRow {
            date: "2026-07-16".to_string(),
            model: "fake-worker".to_string(),
            harness: None,
            profile: None,
            reasoning_effort: None,
            role: "implement".to_string(),
            job: None,
            stage: None,
            execution_key: None,
            provider: None,
            input_sha256: None,
            output_sha256: None,
            attempt: None,
            outcome: None,
            tokens: None,
            task: task.to_string(),
            verify_passed: true,
            complexity: "S".to_string(),
            project: "sandbox-repo".to_string(),
            notes: String::new(),
            failure_reason: None,
            duration_ms: None,
        }
    }

    #[test]
    fn append_preserves_complete_existing_rows() {
        let temp = TempDir::new("ledger-complete-rows");
        let path = temp.path().join("model-bench.jsonl");

        append(&path, &minimal_row("sandbox-1")).expect("first append");
        append(&path, &minimal_row("sandbox-2")).expect("second append");

        let content = std::fs::read_to_string(&path).expect("read ledger");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "both rows must survive the append");
        let tasks = lines
            .iter()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .expect("each line is a complete JSON row")["task"]
                    .as_str()
                    .expect("task is a string")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(tasks, ["sandbox-1", "sandbox-2"]);
    }

    #[cfg(unix)]
    #[test]
    fn append_does_not_read_or_rewrite_existing_history() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new("ledger-write-only-history");
        let path = temp.path().join("model-bench.jsonl");
        let original = serde_json::to_vec(&minimal_row("existing")).expect("serialize seed row");
        std::fs::write(&path, [original.as_slice(), b"\n"].concat()).expect("seed ledger");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200))
            .expect("make ledger append-only to this process");

        let result = append(&path, &minimal_row("new"));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("restore readable ledger");
        result.expect("append must not require reading history");
        assert_eq!(
            std::fs::read_to_string(path)
                .expect("read ledger")
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn append_serializes_concurrent_process_writers_without_lost_or_partial_rows() {
        const LEDGER_ENV: &str = "UNDERTAKE_LEDGER_TEST_PATH";
        const WRITER_ENV: &str = "UNDERTAKE_LEDGER_TEST_WRITER";
        const READY_ENV: &str = "UNDERTAKE_LEDGER_TEST_READY";
        const GO_ENV: &str = "UNDERTAKE_LEDGER_TEST_GO";
        const WRITERS: usize = 16;

        if let (Some(path), Some(writer), Some(ready), Some(go)) = (
            std::env::var_os(LEDGER_ENV),
            std::env::var_os(WRITER_ENV),
            std::env::var_os(READY_ENV),
            std::env::var_os(GO_ENV),
        ) {
            std::fs::write(Path::new(&ready).join(&writer), b"ready").expect("signal ready");
            let deadline = Instant::now() + Duration::from_secs(10);
            while !Path::new(&go).exists() {
                assert!(Instant::now() < deadline, "timed out waiting for writers");
                std::thread::sleep(Duration::from_millis(5));
            }
            append(Path::new(&path), &minimal_row(&writer.to_string_lossy()))
                .expect("child appends one row");
            return;
        }

        let temp = TempDir::new("ledger-concurrent-processes");
        let path = temp.path().join("model-bench.jsonl");
        let ready = temp.path().join("ready");
        let go = temp.path().join("go");
        std::fs::create_dir(&ready).expect("create ready directory");
        let current_exe = std::env::current_exe().expect("current test binary");
        let mut children = (0..WRITERS)
            .map(|writer| {
                Command::new(&current_exe)
                    .args([
                        "--exact",
                        "ledger::tests::append_serializes_concurrent_process_writers_without_lost_or_partial_rows",
                        "--nocapture",
                    ])
                    .env(LEDGER_ENV, &path)
                    .env(WRITER_ENV, writer.to_string())
                    .env(READY_ENV, &ready)
                    .env(GO_ENV, &go)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn concurrent ledger writer")
            })
            .collect::<Vec<_>>();
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_dir(&ready).expect("read ready directory").count() < WRITERS {
            assert!(Instant::now() < deadline, "writers did not become ready");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(&go, b"go").expect("release writers");
        for child in &mut children {
            assert!(child.wait().expect("wait for writer").success());
        }

        let content = std::fs::read_to_string(path).expect("read concurrent ledger");
        let tasks = content
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .expect("every appended line is complete JSON")["task"]
                    .as_str()
                    .expect("task is a string")
                    .to_string()
            })
            .collect::<HashSet<_>>();
        assert_eq!(tasks.len(), WRITERS, "every writer contributes exactly one row");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("undertake-{label}-{nanos}"));
            std::fs::create_dir_all(&path).expect("mkdir temp");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
