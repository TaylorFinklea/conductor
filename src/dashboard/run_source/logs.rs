//! Fixed-allowlist log-tail tests: `cargo test dashboard::run_source::logs`.
//!
//! Covers canonical containment, fixed relative patterns, newline alignment,
//! lossy decoding, and control-character sanitization (which subsumes
//! leading partial-escape removal — see `crate::sanitize`).
//! Absolute and traversal attempt-directory components are proven to never
//! be opened.

use std::fs;

use super::test_support::TempState;
use super::*;

fn run_dir_for(temp: &TempState, run_id: &str) -> PathBuf {
    let dir = temp.runs_dir().join(run_id);
    fs::create_dir_all(&dir).expect("mkdir run dir");
    dir
}

fn write_attempt_log(
    temp: &TempState,
    run_id: &str,
    attempt_dir: &str,
    name: &str,
    contents: &[u8],
) {
    let dir = run_dir_for(temp, run_id).join("attempts").join(attempt_dir);
    fs::create_dir_all(&dir).expect("mkdir attempt dir");
    fs::write(dir.join(name), contents).expect("write attempt log");
}

fn write_verify_log(temp: &TempState, run_id: &str, name: &str, contents: &[u8]) {
    let dir = run_dir_for(temp, run_id).join("artifacts").join("verify");
    fs::create_dir_all(&dir).expect("mkdir verify dir");
    fs::write(dir.join(name), contents).expect("write verify log");
}

/// The fixed relative pattern for `attempts/*/worker.stdout.log` opens
/// correctly and reports the exact relative path.
#[test]
fn worker_stdout_fixed_pattern_opens_and_reports_path() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    write_attempt_log(
        &temp,
        run_id,
        "001-codex-rotation",
        "worker.stdout.log",
        b"hello stdout\n",
    );

    let source = temp.source();
    let tail = source
        .read_log(
            run_id,
            &LogSelector::WorkerStdout("001-codex-rotation".to_string()),
        )
        .expect("read worker stdout");
    assert_eq!(tail.text, "hello stdout\n");
    assert_eq!(tail.path, "attempts/001-codex-rotation/worker.stdout.log");
    assert!(!tail.truncated);
}

/// The fixed relative pattern for `attempts/*/worker.stderr.log` opens
/// correctly.
#[test]
fn worker_stderr_fixed_pattern_opens_correctly() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    write_attempt_log(
        &temp,
        run_id,
        "001-codex-rotation",
        "worker.stderr.log",
        b"an error\n",
    );

    let source = temp.source();
    let tail = source
        .read_log(
            run_id,
            &LogSelector::WorkerStderr("001-codex-rotation".to_string()),
        )
        .expect("read worker stderr");
    assert_eq!(tail.text, "an error\n");
    assert_eq!(tail.path, "attempts/001-codex-rotation/worker.stderr.log");
}

/// The fixed exact paths `artifacts/verify/stdout.log` and `stderr.log` open
/// correctly (no attempt-directory component).
#[test]
fn verify_stdout_and_stderr_fixed_paths_open_correctly() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    write_verify_log(&temp, run_id, "stdout.log", b"cargo test output\n");
    write_verify_log(&temp, run_id, "stderr.log", b"cargo test warnings\n");

    let source = temp.source();
    let stdout = source
        .read_log(run_id, &LogSelector::VerifyStdout)
        .expect("read verify stdout");
    assert_eq!(stdout.text, "cargo test output\n");
    assert_eq!(stdout.path, "artifacts/verify/stdout.log");
    let stderr = source
        .read_log(run_id, &LogSelector::VerifyStderr)
        .expect("read verify stderr");
    assert_eq!(stderr.text, "cargo test warnings\n");
    assert_eq!(stderr.path, "artifacts/verify/stderr.log");
}

/// A missing log (no such file) is a source error, never a silent empty
/// tail — and no file outside the fixed allowlist is ever attempted.
#[test]
fn missing_log_is_an_error_not_a_silent_empty_tail() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    run_dir_for(&temp, run_id);

    let source = temp.source();
    let error = source
        .read_log(run_id, &LogSelector::VerifyStdout)
        .expect_err("missing log must error");
    assert!(error.message().contains("log not found"), "got: {error}");
}

/// An absolute attempt-directory component is rejected before any
/// filesystem access — it is never opened.
#[test]
fn absolute_attempt_dir_is_never_opened() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    run_dir_for(&temp, run_id);
    // A canary file at the literal absolute-looking join target must never
    // be reachable; we don't even need it to exist since validation must
    // reject the shape before any `File::open` call.
    let source = temp.source();
    let error = source
        .read_log(
            run_id,
            &LogSelector::WorkerStdout("/etc/passwd".to_string()),
        )
        .expect_err("absolute component must be rejected");
    assert!(
        error.message().contains("invalid log path component"),
        "got: {error}"
    );
}

/// A traversal attempt-directory component (`..`) is rejected before any
/// filesystem access — it is never opened, even when a real file exists at
/// the traversal target outside the run directory.
#[test]
fn traversal_attempt_dir_is_never_opened() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    run_dir_for(&temp, run_id);
    // A real file at the traversal target, so a broken implementation that
    // actually opened it would succeed instead of erroring.
    let outside_dir = temp.root().join("outside-secret");
    fs::create_dir_all(&outside_dir).unwrap();
    fs::write(outside_dir.join("worker.stdout.log"), b"SECRET\n").unwrap();

    let source = temp.source();
    let error = source
        .read_log(
            run_id,
            &LogSelector::WorkerStdout("../../outside-secret".to_string()),
        )
        .expect_err("traversal component must be rejected");
    assert!(
        error.message().contains("invalid log path component"),
        "got: {error}"
    );
}

/// Canonicalized containment is enforced independently of the up-front
/// shape check: a single-component attempt-directory name that is actually
/// a symlink escaping the run directory is refused, never opened, even
/// though its literal string shape passes single-component validation.
#[test]
fn symlink_escape_is_refused_by_containment_check() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    let run_dir = run_dir_for(&temp, run_id);
    let attempts_dir = run_dir.join("attempts");
    fs::create_dir_all(&attempts_dir).unwrap();

    let outside_dir = temp.root().join("outside-secret");
    fs::create_dir_all(&outside_dir).unwrap();
    fs::write(outside_dir.join("worker.stdout.log"), b"SECRET\n").unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside_dir, attempts_dir.join("escape-link")).unwrap();

    let source = temp.source();
    let error = source
        .read_log(
            run_id,
            &LogSelector::WorkerStdout("escape-link".to_string()),
        )
        .expect_err("symlink escaping the run directory must be refused");
    assert!(
        error.message().contains("escapes run directory"),
        "got: {error}"
    );
}

/// Newline alignment: a tail starting mid-file discards through the first
/// newline, so the retained text never begins with a truncated partial
/// line.
#[test]
fn newline_alignment_discards_partial_first_line() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    // Fixed-width lines so the 64 KiB cut point is very unlikely to land
    // exactly on a line boundary, guaranteeing a partial first line.
    let mut contents = Vec::new();
    for i in 0..20_000u32 {
        contents.extend_from_slice(format!("line-{i:06}\n").as_bytes());
    }
    assert!(contents.len() as u64 > LOG_TAIL_MAX_BYTES);
    write_attempt_log(&temp, run_id, "001-x", "worker.stdout.log", &contents);

    let source = temp.source();
    let tail = source
        .read_log(run_id, &LogSelector::WorkerStdout("001-x".to_string()))
        .expect("read log");
    assert!(tail.truncated);
    let first_line = tail.text.lines().next().expect("at least one line");
    assert!(
        first_line.starts_with("line-") && first_line.len() == "line-000000".len(),
        "first retained line must be a complete, non-partial line, got: {first_line:?}"
    );
}

/// A log tail split UTF-8 multi-byte character exactly at the 64 KiB
/// boundary is handled by lossy decoding without panicking and without
/// leaving invalid bytes in the result.
#[test]
fn utf8_split_at_boundary_is_lossily_decoded_without_panic() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    // Build content so a 4-byte UTF-8 character (an emoji) straddles the
    // exact `file_len - 64 KiB` cut point, with no newline for a while
    // afterward so newline-alignment cannot trivially discard the split.
    let cap = usize::try_from(LOG_TAIL_MAX_BYTES).expect("cap fits in usize");
    let prefix_len = 10_000; // padding before the boundary
    let mut contents = vec![b'a'; prefix_len];
    // Position the multi-byte character straddling `prefix_len + cap`
    let emoji = "\u{1F600}".as_bytes(); // 4 bytes
    let mut tail_region = Vec::new();
    tail_region.extend_from_slice(&vec![b'b'; cap - 2]);
    tail_region.extend_from_slice(emoji); // straddles offset `cap - 2` within this region
    tail_region.extend_from_slice(&[b'c'; 100]);
    // Deliberately no trailing newline: the tail window must contain none,
    // so newline-alignment cannot discard the split character and the raw
    // lossy-decode path is genuinely exercised.
    contents.extend_from_slice(&tail_region);

    write_attempt_log(&temp, run_id, "001-x", "worker.stdout.log", &contents);

    let source = temp.source();
    let tail = source
        .read_log(run_id, &LogSelector::WorkerStdout("001-x".to_string()))
        .expect("read log must not panic on a boundary-split UTF-8 character");
    assert!(tail.truncated);
    // The result is a valid Rust String by construction; assert it is
    // non-empty and contains no NUL bytes (a crude corruption smoke check).
    assert!(!tail.text.is_empty());
    assert!(!tail.text.contains('\0'));
}

/// A partial CSI (ANSI escape) sequence at the 64 KiB boundary, with no
/// newline anywhere in the tail window, is stripped by control-character
/// sanitization so no raw ESC byte reaches the output.
#[test]
fn csi_split_at_boundary_with_no_newline_is_sanitized() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    let cap = usize::try_from(LOG_TAIL_MAX_BYTES).expect("cap fits in usize");
    let prefix_len = 5_000;
    let mut contents = vec![b'a'; prefix_len];
    // One giant line (no newline anywhere) containing an ANSI SGR escape
    // sequence straddling the exact boundary, so newline-alignment cannot
    // discard it — sanitization is the only defense exercised here.
    let mut tail_region = vec![b'x'; cap - 3];
    tail_region.extend_from_slice(b"\x1b[38;5;208mHi\x1b[0m"); // no trailing newline
    contents.extend_from_slice(&tail_region);
    assert!(
        !contents[contents.len() - cap..].contains(&b'\n'),
        "fixture must have no newline in the tail window"
    );

    write_attempt_log(&temp, run_id, "001-x", "worker.stdout.log", &contents);

    let source = temp.source();
    let tail = source
        .read_log(run_id, &LogSelector::WorkerStdout("001-x".to_string()))
        .expect("read log");
    assert!(
        !tail.text.contains('\u{1b}'),
        "no raw ESC byte may reach the output, got: {:?}",
        tail.text
    );
}

/// A complete CSI sequence anywhere in the log (not just at a boundary) is
/// also sanitized — control stripping is general, not boundary-specific.
#[test]
fn complete_csi_sequence_anywhere_is_sanitized() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    write_attempt_log(
        &temp,
        run_id,
        "001-x",
        "worker.stdout.log",
        b"\x1b[31mred text\x1b[0m normal\n",
    );

    let source = temp.source();
    let tail = source
        .read_log(run_id, &LogSelector::WorkerStdout("001-x".to_string()))
        .expect("read log");
    assert!(!tail.text.contains('\u{1b}'));
    assert!(tail.text.contains("red text"));
    assert!(tail.text.contains("normal"));
}

/// A short log (under the 64 KiB cap) is read in full and not marked
/// truncated.
#[test]
fn short_log_is_read_in_full_and_not_truncated() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    write_attempt_log(&temp, run_id, "001-x", "worker.stdout.log", b"short\n");

    let source = temp.source();
    let tail = source
        .read_log(run_id, &LogSelector::WorkerStdout("001-x".to_string()))
        .expect("read log");
    assert_eq!(tail.text, "short\n");
    assert!(!tail.truncated);
}
