//! Verification precedence reduction tests:
//! `cargo test dashboard::run_source::verification`.
//!
//! Precedence: durable `details.state.mechanical` wins over the latest valid
//! `verify_finished` event, which wins over "not run". `verifier.mechanical`
//! supplies the command string regardless of source. Disagreement between
//! the durable state and the latest event is visible, not silently
//! reconciled.

use std::fs;

use super::test_support::{PATCHSTAND_RUN_ID, TempState};
use super::*;

fn events_path(temp: &TempState, run_id: &str) -> PathBuf {
    temp.runs_dir().join(run_id).join("events.jsonl")
}

fn write_events(temp: &TempState, run_id: &str, lines: &[serde_json::Value]) {
    let mut content = String::new();
    for line in lines {
        content.push_str(&line.to_string());
        content.push('\n');
    }
    fs::write(events_path(temp, run_id), content).unwrap();
}

fn run_started(run_id: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "undertake/event@2",
        "event_id": format!("{run_id}-000001"),
        "run_id": run_id,
        "seq": 1,
        "ts": "2026-07-25T18:39:00Z",
        "kind": "run_started",
        "job": "work",
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
        "outcome": "started",
    })
}

fn verify_finished(seq: u64, run_id: &str, outcome: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "undertake/event@2",
        "event_id": format!("{run_id}-{seq:06}"),
        "run_id": run_id,
        "seq": seq,
        "ts": "2026-07-25T18:39:30Z",
        "kind": "verify_finished",
        "job": "work",
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
        "outcome": outcome,
    })
}

/// A work manifest with an optional durable `mechanical` verification
/// evidence block and a configured `verifier.mechanical` command.
fn work_manifest_with_mechanical(
    run_id: &str,
    verifier_command: Option<&str>,
    mechanical: Option<(&str, bool)>,
) -> serde_json::Value {
    let mut state = serde_json::json!({
        "cycle_id": "c1",
        "authorization_sha256": "a".repeat(64),
        "stage": "pending_review",
    });
    if let Some((command, passed)) = mechanical {
        state["mechanical"] = serde_json::json!({
            "command": command,
            "passed": passed,
            "artifact_refs": [],
        });
    }
    let mut verifier = serde_json::json!({});
    if let Some(command) = verifier_command {
        verifier["mechanical"] = serde_json::json!(command);
    }
    serde_json::json!({
        "schema": "undertake/run@2",
        "run_id": run_id,
        "job": "work",
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
        "details": {"job": "work", "state": state},
        "created_at": "2026-07-25T18:39:20Z",
        "updated_at": "2026-07-25T18:39:20Z",
        "approved_profiles": [],
        "limits": {},
        "verifier": verifier,
        "lifecycle": "running",
    })
}

/// Precedence 1: durable `details.state.mechanical` wins even when a
/// `verify_finished` event disagrees.
#[test]
fn mechanical_state_takes_precedence_over_event() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(
        run_id,
        &work_manifest_with_mechanical(run_id, Some("pnpm check"), Some(("pnpm check", true))),
    );
    write_events(
        &temp,
        run_id,
        &[run_started(run_id), verify_finished(2, run_id, "failed")],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.verification.passed, Some(true));
    assert_eq!(run.verification.source, VerificationSource::Mechanical);
    assert_eq!(run.verification.command.as_deref(), Some("pnpm check"));
    assert_eq!(run.verification.event_outcome.as_deref(), Some("failed"));
    assert!(
        run.verification.disagreement,
        "mechanical=true vs event=failed must be visible as disagreement"
    );
}

/// Precedence 2: no durable mechanical state, so the latest valid
/// `verify_finished` event wins.
#[test]
fn event_wins_when_no_mechanical_state() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(
        run_id,
        &work_manifest_with_mechanical(run_id, Some("cargo test"), None),
    );
    write_events(
        &temp,
        run_id,
        &[run_started(run_id), verify_finished(2, run_id, "passed")],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.verification.passed, Some(true));
    assert_eq!(run.verification.source, VerificationSource::Event);
    assert_eq!(run.verification.command.as_deref(), Some("cargo test"));
    assert!(!run.verification.disagreement);
}

/// Precedence 3: no mechanical state and no `verify_finished` event is "not
/// run".
#[test]
fn not_run_when_no_mechanical_state_and_no_event() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(
        run_id,
        &work_manifest_with_mechanical(run_id, Some("cargo test"), None),
    );
    write_events(&temp, run_id, &[run_started(run_id)]);

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.verification.passed, None);
    assert_eq!(run.verification.source, VerificationSource::NotRun);
    // The configured command still displays even though verification has
    // not run.
    assert_eq!(run.verification.command.as_deref(), Some("cargo test"));
}

/// The latest of multiple `verify_finished` events wins (not the first).
#[test]
fn latest_verify_finished_event_wins_over_earlier_ones() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(
        run_id,
        &work_manifest_with_mechanical(run_id, Some("cargo test"), None),
    );
    write_events(
        &temp,
        run_id,
        &[
            run_started(run_id),
            verify_finished(2, run_id, "failed"),
            verify_finished(3, run_id, "passed"),
        ],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.verification.passed, Some(true));
    assert_eq!(run.verification.event_outcome.as_deref(), Some("passed"));
}

/// Agreement between mechanical state and the latest event shows no
/// disagreement.
#[test]
fn mechanical_and_event_agreement_shows_no_disagreement() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(
        run_id,
        &work_manifest_with_mechanical(run_id, Some("pnpm check"), Some(("pnpm check", false))),
    );
    write_events(
        &temp,
        run_id,
        &[run_started(run_id), verify_finished(2, run_id, "failed")],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.verification.passed, Some(false));
    assert_eq!(run.verification.source, VerificationSource::Mechanical);
    assert!(!run.verification.disagreement);
}

/// An unknown outcome string on a `verify_finished` event is never
/// interpreted as success; it is displayed verbatim.
#[test]
fn unknown_verify_outcome_is_not_interpreted_as_success() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(
        run_id,
        &work_manifest_with_mechanical(run_id, Some("cargo test"), None),
    );
    write_events(
        &temp,
        run_id,
        &[
            run_started(run_id),
            verify_finished(2, run_id, "provider_limited:rate_limit"),
        ],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(
        run.verification.passed, None,
        "an unrecognized outcome string must never be interpreted as success"
    );
    assert_eq!(run.verification.source, VerificationSource::NotRun);
    assert_eq!(
        run.verification.event_outcome.as_deref(),
        Some("provider_limited:rate_limit"),
        "the unknown outcome is still displayed verbatim"
    );
}

/// The pilot run has no durable `details.state.mechanical`, so its
/// verification state comes from the `failed` `verify_finished` event while
/// the command string still comes from the manifest's `verifier.mechanical`
/// (`pnpm check`). Read from a byte-shape copy of the real run directory.
#[test]
fn patchstand_verification_is_failed_pnpm_check_from_event() {
    let temp = TempState::new();
    temp.write_patchstand_run(
        PATCHSTAND_RUN_ID,
        "2026-07-25T18:39:20.469500+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        "2026-07-25T18:43:36.460155+00:00",
        std::process::id(),
        std::process::id(),
    );
    let run = temp
        .source()
        .select(&RunSelection::Explicit(PATCHSTAND_RUN_ID.to_string()))
        .expect("snapshot");

    assert_eq!(run.selection_error, None, "real manifest shape must parse");
    assert_eq!(run.verification.passed, Some(false));
    assert_eq!(run.verification.source, VerificationSource::Event);
    assert_eq!(run.verification.command.as_deref(), Some("pnpm check"));
    assert_eq!(run.verification.event_outcome.as_deref(), Some("failed"));
    assert!(!run.verification.disagreement);
    assert_eq!(run.event_count, 5, "all five pilot events are retained");
}
