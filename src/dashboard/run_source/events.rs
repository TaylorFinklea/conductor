//! Incremental bounded event-tail tests: `cargo test dashboard::run_source::events`.

use super::test_support::TempState;
use super::*;
use std::fs;

/// Builds one valid `undertake/event@2` JSON line (no trailing newline).
fn event_line(run_id: &str, seq: u64, kind: &str, outcome: Option<&str>) -> String {
    let mut value = serde_json::json!({
        "schema": "undertake/event@2",
        "event_id": format!("{run_id}-{seq:06}"),
        "run_id": run_id,
        "seq": seq,
        "ts": "2026-07-25T18:39:20Z",
        "kind": kind,
        "job": "work",
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
    });
    if let Some(outcome) = outcome {
        value["outcome"] = serde_json::json!(outcome);
    }
    value.to_string()
}

fn events_path(temp: &TempState, run_id: &str) -> PathBuf {
    temp.runs_dir().join(run_id).join("events.jsonl")
}

fn run_dir_for(temp: &TempState, run_id: &str) -> PathBuf {
    temp.runs_dir().join(run_id)
}

fn setup_run(temp: &TempState, run_id: &str) {
    temp.write_run(
        run_id,
        &temp.work_manifest(run_id, "2026-07-25T18:39:20Z", "running"),
    );
}

/// A partial final line (no trailing newline) is retained and retried
/// without error; it produces no event yet.
#[test]
fn partial_final_line_is_retained_without_error() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let path = events_path(&temp, run_id);
    // One complete event, then a partial (no-newline) second line.
    let mut content = event_line(run_id, 1, "run_started", Some("started"));
    content.push('\n');
    content.push_str(r#"{"schema":"undertake/event@2","event_id":"partial"#);
    fs::write(&path, &content).unwrap();

    let source = temp.source();
    let run_dir = run_dir_for(&temp, run_id);
    let (count, truncated, error, seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert_eq!(count, 1, "only the complete line is parsed");
    assert_eq!(seqs, vec![1]);
    assert!(!truncated);
    assert!(error.is_none(), "a partial trailing line is not an error");
}

/// Completing a previously partial line makes it appear exactly once
/// (not duplicated with the earlier partial read).
#[test]
fn completed_line_appears_once() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let path = events_path(&temp, run_id);
    let mut content = event_line(run_id, 1, "run_started", Some("started"));
    content.push('\n');
    // Partial second line (no newline yet).
    let partial = r#"{"schema":"undertake/event@2","event_id":"x-2","run_id":"RID","seq":2,"ts":"2026-07-25T18:39:21Z","kind":"attempt_started","job":"work","target":{"repo":"/repo/patchstand","bead":"patchstand-1"}"#
        .replace("RID", run_id);
    content.push_str(&partial);
    fs::write(&path, &content).unwrap();

    let source = temp.source();
    let run_dir = run_dir_for(&temp, run_id);
    let (count, _truncated, error, seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert_eq!(count, 1);
    assert_eq!(seqs, vec![1]);
    assert!(error.is_none());

    // Complete the second line and append a newline.
    let mut completed = content.clone();
    completed.push_str("}\n");
    fs::write(&path, &completed).unwrap();
    let (count, _truncated, error, seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert_eq!(
        count, 2,
        "the completed line appears exactly once, not duplicated"
    );
    assert_eq!(seqs, vec![1, 2]);
    assert!(error.is_none());
}

/// A complete but malformed JSON line is a source error.
#[test]
fn complete_malformed_line_is_source_error() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let path = events_path(&temp, run_id);
    let mut content = event_line(run_id, 1, "run_started", Some("started"));
    content.push('\n');
    content.push_str("{ not valid json }\n");
    fs::write(&path, &content).unwrap();

    let source = temp.source();
    let run_dir = run_dir_for(&temp, run_id);
    let (count, _truncated, error, _seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert_eq!(count, 1, "only the valid first line is retained");
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("malformed event")),
        "got: {error:?}"
    );
}

/// A sequence gap is a source error.
#[test]
fn sequence_gap_is_source_error() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let path = events_path(&temp, run_id);
    let mut content = event_line(run_id, 1, "run_started", Some("started"));
    content.push('\n');
    // Seq jumps from 1 to 3, skipping 2.
    content.push_str(&event_line(
        run_id,
        3,
        "attempt_started",
        Some("running:001"),
    ));
    content.push('\n');
    fs::write(&path, &content).unwrap();

    let source = temp.source();
    let run_dir = run_dir_for(&temp, run_id);
    let (count, _truncated, error, _seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert_eq!(count, 1);
    assert!(
        error.as_deref().is_some_and(|e| e.contains("sequence gap")),
        "got: {error:?}"
    );
}

/// An unknown event schema is a source error.
#[test]
fn unknown_schema_is_source_error() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let path = events_path(&temp, run_id);
    let mut bad = serde_json::from_str::<serde_json::Value>(&event_line(
        run_id,
        1,
        "run_started",
        Some("started"),
    ))
    .unwrap();
    bad["schema"] = serde_json::json!("undertake/event@3");
    let content = format!("{bad}\n");
    fs::write(&path, &content).unwrap();

    let source = temp.source();
    let run_dir = run_dir_for(&temp, run_id);
    let (count, _truncated, error, _seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert_eq!(count, 0);
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("unknown event schema")),
        "got: {error:?}"
    );
}

/// Unknown extra fields on an otherwise-valid event line succeed
/// (forward compatibility).
#[test]
fn unknown_extra_fields_succeed() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let path = events_path(&temp, run_id);
    let mut value = serde_json::from_str::<serde_json::Value>(&event_line(
        run_id,
        1,
        "run_started",
        Some("started"),
    ))
    .unwrap();
    value["future_field"] = serde_json::json!({"anything": "here"});
    value["another_unknown"] = serde_json::json!(42);
    let content = format!("{value}\n");
    fs::write(&path, &content).unwrap();

    let source = temp.source();
    let run_dir = run_dir_for(&temp, run_id);
    let (count, _truncated, error, seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert_eq!(count, 1);
    assert_eq!(seqs, vec![1]);
    assert!(error.is_none());
}

/// The 5,000-event cap truncates visibly: once exceeded, the oldest
/// events are dropped and `truncated` is set.
#[test]
fn five_thousand_event_cap_truncates_visibly() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let path = events_path(&temp, run_id);
    let mut content = String::new();
    // 5,010 events, well past the 5,000 cap.
    for seq in 1..=5_010u64 {
        let kind = if seq == 1 {
            "run_started"
        } else {
            "coverage_gap"
        };
        content.push_str(&event_line(run_id, seq, kind, None));
        content.push('\n');
    }
    fs::write(&path, &content).unwrap();

    let source = temp.source();
    let run_dir = run_dir_for(&temp, run_id);
    let (count, truncated, error, seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert_eq!(count, 5_000, "retained count is capped at 5,000");
    assert!(truncated, "exceeding the cap must mark truncated");
    assert!(error.is_none());
    // The retained window is the newest 5,000 (seqs 11..=5010).
    assert_eq!(*seqs.first().unwrap(), 11);
    assert_eq!(*seqs.last().unwrap(), 5_010);
}

/// The 8 MiB read cap truncates visibly: events beyond the byte cap in a
/// single tick are not read this tick, and truncation is reported.
#[test]
fn eight_mib_read_cap_truncates_visibly() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let path = events_path(&temp, run_id);
    // Build events with a padding field so each line is large; enough
    // lines to exceed 8 MiB in one file.
    let padding = "x".repeat(2000);
    let mut content = String::new();
    let mut seq = 1u64;
    // ~9 MiB of content at ~2KiB/line => ~4700 lines, under the 5,000
    // event cap so byte-cap truncation is isolated from the event cap.
    while content.len() < 9 * 1024 * 1024 {
        let kind = if seq == 1 {
            "run_started"
        } else {
            "coverage_gap"
        };
        let mut value =
            serde_json::from_str::<serde_json::Value>(&event_line(run_id, seq, kind, None))
                .unwrap();
        value["padding"] = serde_json::json!(padding);
        content.push_str(&value.to_string());
        content.push('\n');
        seq += 1;
    }
    fs::write(&path, &content).unwrap();

    let source = temp.source();
    let run_dir = run_dir_for(&temp, run_id);
    let (count, truncated, error, _seqs) = source.read_event_tail_pub(run_id, &run_dir);
    assert!(
        count < usize::try_from(seq - 1).expect("seq fits in usize"),
        "not all events fit in one 8 MiB tick"
    );
    assert!(
        truncated,
        "exceeding the byte cap in one tick must mark truncated"
    );
    assert!(error.is_none());
}

/// An event-tail source error must be visible on the [`RunSnapshot`] the
/// renderer consumes, not only through the reader's internal state. A
/// malformed line that stalls the tail while the panel silently shows the
/// last good event count is exactly the failure the dashboard exists to
/// prevent.
#[test]
fn malformed_line_error_is_visible_on_the_snapshot() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let mut content = event_line(run_id, 1, "run_started", Some("started"));
    content.push('\n');
    content.push_str("{ not valid json }\n");
    fs::write(events_path(&temp, run_id), &content).unwrap();

    let run = temp
        .source()
        .select(&RunSelection::Explicit(run_id.to_string()))
        .expect("snapshot");
    assert_eq!(run.event_count, 1, "only the valid first line is retained");
    assert!(
        run.events_error
            .as_deref()
            .is_some_and(|error| error.contains("malformed event")),
        "got: {:?}",
        run.events_error
    );
}

/// A sequence gap is likewise visible on the snapshot.
#[test]
fn sequence_gap_error_is_visible_on_the_snapshot() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let mut content = event_line(run_id, 1, "run_started", Some("started"));
    content.push('\n');
    content.push_str(&event_line(
        run_id,
        3,
        "attempt_started",
        Some("running:001-p"),
    ));
    content.push('\n');
    fs::write(events_path(&temp, run_id), &content).unwrap();

    let run = temp
        .source()
        .select(&RunSelection::Explicit(run_id.to_string()))
        .expect("snapshot");
    assert!(
        run.events_error
            .as_deref()
            .is_some_and(|error| error.contains("sequence gap")),
        "got: {:?}",
        run.events_error
    );
}

/// A clean event log leaves no error on the snapshot.
#[test]
fn clean_event_log_leaves_no_snapshot_error() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    setup_run(&temp, run_id);
    let mut content = event_line(run_id, 1, "run_started", Some("started"));
    content.push('\n');
    fs::write(events_path(&temp, run_id), &content).unwrap();

    let run = temp
        .source()
        .select(&RunSelection::Explicit(run_id.to_string()))
        .expect("snapshot");
    assert_eq!(run.event_count, 1);
    assert_eq!(run.events_error, None);
}
