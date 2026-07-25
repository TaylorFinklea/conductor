//! Per-job attempt reduction tests: `cargo test dashboard::run_source::attempts`.
//!
//! Covers work `running:<attempt-dir>` joins, nested plan stage markers,
//! unresolved opaque profiles, empty consult/review states, duration
//! pairing, and unpaired starts.

use std::fs;

use super::test_support::{
    PATCHSTAND_ATTEMPT_DIR, PATCHSTAND_PROFILE_ID, PATCHSTAND_RUN_ID, TempState,
};
use super::*;

fn events_path(temp: &TempState, run_id: &str) -> PathBuf {
    temp.runs_dir().join(run_id).join("events.jsonl")
}

fn run_dir_for(temp: &TempState, run_id: &str) -> PathBuf {
    temp.runs_dir().join(run_id)
}

fn work_manifest(run_id: &str, job: &str) -> serde_json::Value {
    let details = match job {
        "work" => {
            serde_json::json!({"job": "work", "state": {"cycle_id": "c1", "authorization_sha256": "a".repeat(64), "stage": "implementing"}})
        }
        "review" => serde_json::json!({"job": "review", "state": {}}),
        "consult" => serde_json::json!({"job": "consult", "state": {}}),
        other => panic!("unsupported test job {other}"),
    };
    serde_json::json!({
        "schema": "undertake/run@2",
        "run_id": run_id,
        "job": job,
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
        "details": details,
        "created_at": "2026-07-25T18:39:20Z",
        "updated_at": "2026-07-25T18:39:20Z",
        "approved_profiles": [],
        "limits": {},
        "verifier": {},
        "lifecycle": "running",
    })
}

fn write_events(temp: &TempState, run_id: &str, lines: &[serde_json::Value]) {
    let mut content = String::new();
    for line in lines {
        content.push_str(&line.to_string());
        content.push('\n');
    }
    fs::write(events_path(temp, run_id), content).unwrap();
}

fn attempt_started(
    seq: u64,
    run_id: &str,
    profile_id: &str,
    attempt_dir: &str,
    ts: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schema": "undertake/event@2",
        "event_id": format!("{run_id}-{seq:06}"),
        "run_id": run_id,
        "seq": seq,
        "ts": ts,
        "kind": "attempt_started",
        "job": "work",
        "profile_id": profile_id,
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
        "outcome": format!("running:{attempt_dir}"),
    })
}

fn attempt_finished(
    seq: u64,
    run_id: &str,
    profile_id: &str,
    ts: &str,
    outcome: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schema": "undertake/event@2",
        "event_id": format!("{run_id}-{seq:06}"),
        "run_id": run_id,
        "seq": seq,
        "ts": ts,
        "kind": "attempt_finished",
        "job": "work",
        "profile_id": profile_id,
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
        "outcome": outcome,
    })
}

fn run_started(run_id: &str, job: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "undertake/event@2",
        "event_id": format!("{run_id}-000001"),
        "run_id": run_id,
        "seq": 1,
        "ts": "2026-07-25T18:39:00Z",
        "kind": "run_started",
        "job": job,
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
        "outcome": "started",
    })
}

/// A valid `musterroll/roster@2` snapshot with the given profiles
/// `(profile_id, provider_id, model, harness, dispatch_id)`.
fn roster_snapshot_json(profiles: &[(&str, &str, &str, &str, &str)]) -> serde_json::Value {
    let profile_json: Vec<serde_json::Value> = profiles
        .iter()
        .map(|(profile_id, provider_id, model, harness, dispatch_id)| {
            serde_json::json!({
                "profile_id": profile_id,
                "provider_id": provider_id,
                "model": model,
                "harness": harness,
                "dispatch_id": dispatch_id,
                "reasoning_effort": null,
                "tier": "senior",
                "ceiling": "XL",
                "efficiency": "lean",
                "cost": 0.0,
                "data_policy": "standard",
                "enabled": true,
                "roles": ["default"],
                "state": "healthy",
                "eligible": true,
                "ineligibility_reason": null
            })
        })
        .collect();
    let mut provider_ids: Vec<&str> = profiles.iter().map(|(_, p, ..)| *p).collect();
    provider_ids.sort_unstable();
    provider_ids.dedup();
    let provider_json: Vec<serde_json::Value> = provider_ids
        .iter()
        .map(|provider_id| {
            serde_json::json!({
                "provider_id": provider_id,
                "availability_key": provider_id,
                "enabled": true,
                "state": "healthy",
                "availability": "healthy",
                "checked_at": "2026-07-25T18:00:00Z",
                "data_as_of": null,
                "expires_at": null,
                "reason": null,
                "eligible": true,
                "ineligibility_reason": null
            })
        })
        .collect();
    serde_json::json!({
        "schema": "musterroll/roster@2",
        "generated_at": "2026-07-25T18:00:00Z",
        "source_artifact": {"path": "/fixture/musterroll-roster.toml", "sha256": "a".repeat(64)},
        "policy_sha256": "b".repeat(64),
        "providers": provider_json,
        "profiles": profile_json
    })
}

fn write_roster(temp: &TempState, run_id: &str, profiles: &[(&str, &str, &str, &str, &str)]) {
    let snapshot = roster_snapshot_json(profiles);
    fs::write(
        run_dir_for(temp, run_id).join("roster.json"),
        snapshot.to_string(),
    )
    .unwrap();
}

/// Work `running:<attempt-dir>` joins: the attempt ordinal comes from the
/// leading `<NNN>` of the attempt directory, and provider/harness/model/
/// dispatch-id are resolved only from the run-local roster by exact
/// `profile_id` match — never by parsing the attempt-directory string.
#[test]
fn work_running_attempt_dir_join_resolves_via_roster() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(run_id, &work_manifest(run_id, "work"));
    write_roster(
        &temp,
        run_id,
        &[(
            "openai-codex--codex--gpt-5.6-luna--high",
            "codex",
            "gpt-5.6-luna",
            "codex",
            "openai-codex--codex--gpt-5.6-luna--high",
        )],
    );
    write_events(
        &temp,
        run_id,
        &[
            run_started(run_id, "work"),
            attempt_started(
                2,
                run_id,
                "openai-codex--codex--gpt-5.6-luna--high",
                "001-openai-codex--codex--gpt-5.6-luna--high",
                "2026-07-25T18:39:20Z",
            ),
        ],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.attempts.len(), 1);
    let attempt = &run.attempts[0];
    assert_eq!(attempt.ordinal, 1, "ordinal comes from the leading <NNN>");
    assert_eq!(
        attempt.attempt_dir.as_deref(),
        Some("001-openai-codex--codex--gpt-5.6-luna--high")
    );
    assert_eq!(
        attempt.profile_id.as_deref(),
        Some("openai-codex--codex--gpt-5.6-luna--high")
    );
    assert!(
        attempt.resolved,
        "profile_id exact match resolves via roster"
    );
    assert_eq!(attempt.provider_id.as_deref(), Some("codex"));
    assert_eq!(attempt.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(attempt.harness.as_deref(), Some("codex"));
    assert_eq!(
        attempt.dispatch_id.as_deref(),
        Some("openai-codex--codex--gpt-5.6-luna--high")
    );
}

/// Nested plan stage markers: only events carrying typed `plan_invocation`
/// evidence become stage markers. A stage-marker event such as
/// `planner_authoring` (no `plan_invocation`) is never mistaken for one.
#[test]
fn nested_plan_stage_markers_exclude_bare_marker_events() {
    let temp = TempState::new();
    let run_id = "run-plan-20260725T183920.469500000-p1-000000";
    let plan_manifest = serde_json::json!({
        "schema": "undertake/run@2",
        "run_id": run_id,
        "job": "plan",
        "target": {"repo": "/repo/x"},
        "details": {"job": "plan", "state": {
            "target": {"repo": "/repo/x", "input": {"kind": "bead", "bead_id": "b1", "artifact": {"path": "in.txt", "sha256": "a".repeat(64)}, "tier": "junior", "complexity": "S"}},
            "routes": {"stages": [
                {"stage": "planner", "capability_role": "author", "candidates": [{"profile_id": "planner-profile", "provider_id": "anthropic", "availability_key": "ak", "execution_key": "ek"}], "provider_distinct_from": [], "constraints": {"distinct_execution_from": [], "tier_at_least": [], "provider_diversity": "none"}},
                {"stage": "peer_review", "capability_role": "peer", "candidates": [{"profile_id": "peer-profile", "provider_id": "anthropic", "availability_key": "ak", "execution_key": "ek2"}], "provider_distinct_from": [], "constraints": {"distinct_execution_from": [], "tier_at_least": [], "provider_diversity": "none"}},
                {"stage": "second_opinion", "capability_role": "judge", "candidates": [{"profile_id": "judge-profile", "provider_id": "anthropic", "availability_key": "ak", "execution_key": "ek3"}], "provider_distinct_from": [], "constraints": {"distinct_execution_from": [], "tier_at_least": [], "provider_diversity": "none"}}
            ]},
            "progress": {"state": "authoring", "author": {"profile_id": "planner-profile", "provider_id": "anthropic", "availability_key": "ak", "execution_key": "ek"}, "attempts": 1},
            "stage_attempts": {"planner": 1, "peer_review": 0, "second_opinion": 0},
            "revision_limit": 0,
            "stage_attempt_limit": 1
        }},
        "created_at": "2026-07-25T18:39:20Z",
        "updated_at": "2026-07-25T18:39:20Z",
        "approved_profiles": [],
        "limits": {},
        "verifier": {},
        "lifecycle": "running",
    });
    temp.write_run(run_id, &plan_manifest);
    write_roster(
        &temp,
        run_id,
        &[(
            "planner-profile",
            "anthropic",
            "claude-sonnet-5",
            "claude-code",
            "planner-profile",
        )],
    );
    let marker_event = serde_json::json!({
        "schema": "undertake/event@2",
        "event_id": format!("{run_id}-000002"),
        "run_id": run_id,
        "seq": 2,
        "ts": "2026-07-25T18:39:10Z",
        "kind": "attempt_started",
        "job": "plan",
        "profile_id": "planner-profile",
        "target": {"repo": "/repo/x"},
        "outcome": "planner_authoring",
    });
    let typed_start = serde_json::json!({
        "schema": "undertake/event@2",
        "event_id": format!("{run_id}-000003"),
        "run_id": run_id,
        "seq": 3,
        "ts": "2026-07-25T18:39:15Z",
        "kind": "attempt_started",
        "job": "plan",
        "profile_id": "planner-profile",
        "target": {"repo": "/repo/x"},
        "outcome": "started",
        "plan_invocation": {
            "role": "author",
            "stage": "planner",
            "execution": {"profile_id": "planner-profile", "provider_id": "anthropic", "availability_key": "ak", "execution_key": "ek"},
            "input_sha256": "a".repeat(64),
            "output_sha256": null,
            "attempt": 1,
            "duration_ms": null,
            "tokens": null,
        },
    });
    write_events(
        &temp,
        run_id,
        &[run_started(run_id, "plan"), marker_event, typed_start],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(
        run.stage_markers.len(),
        1,
        "only the typed plan_invocation event becomes a stage marker; \
         the bare planner_authoring marker must not"
    );
    assert_eq!(run.stage_markers[0].stage, "planner");
    assert_eq!(run.stage_markers[0].role.as_deref(), Some("author"));
    assert!(run.attempts.is_empty(), "plan runs have no worker attempts");
}

/// An opaque profile id with no matching run-local roster entry remains
/// visible as the opaque id with an explicit unresolved marker.
#[test]
fn unresolved_opaque_profile_remains_visible() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(run_id, &work_manifest(run_id, "work"));
    // No roster.json written at all.
    write_events(
        &temp,
        run_id,
        &[
            run_started(run_id, "work"),
            attempt_started(
                2,
                run_id,
                "some-unknown-profile",
                "001-some-unknown-profile",
                "2026-07-25T18:39:20Z",
            ),
        ],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.attempts.len(), 1);
    let attempt = &run.attempts[0];
    assert_eq!(attempt.profile_id.as_deref(), Some("some-unknown-profile"));
    assert!(
        !attempt.resolved,
        "unresolvable profile must be marked unresolved"
    );
    assert!(attempt.provider_id.is_none());
    assert!(attempt.model.is_none());
    assert!(attempt.harness.is_none());
    assert!(attempt.dispatch_id.is_none());
}

/// A job with no attempts shows an explicit empty state (an empty `Vec`, no
/// synthetic placeholder entry) for review/consult jobs.
#[test]
fn empty_review_and_consult_states_have_no_attempts() {
    for job in ["review", "consult"] {
        let temp = TempState::new();
        let run_id = "run-work-20260725T183920.469500000-p1-000000";
        temp.write_run(run_id, &work_manifest(run_id, job));
        write_events(&temp, run_id, &[run_started(run_id, job)]);

        let source = temp.source();
        let run = source
            .snapshot_for_run_pub(run_id, "2026-07-25T18:40:00Z".parse().unwrap())
            .expect("snapshot");
        assert!(
            run.attempts.is_empty(),
            "{job} run with no attempt_started events must have an empty, not synthetic, attempts list"
        );
    }
}

/// Duration pairing: an attempt's duration is the matching finish timestamp
/// minus the start timestamp.
#[test]
fn duration_pairing_computes_finish_minus_start() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(run_id, &work_manifest(run_id, "work"));
    write_roster(
        &temp,
        run_id,
        &[(
            "profile-a",
            "anthropic",
            "claude-sonnet-5",
            "claude-code",
            "profile-a",
        )],
    );
    write_events(
        &temp,
        run_id,
        &[
            run_started(run_id, "work"),
            attempt_started(
                2,
                run_id,
                "profile-a",
                "001-profile-a",
                "2026-07-25T18:39:20Z",
            ),
            attempt_finished(3, run_id, "profile-a", "2026-07-25T18:41:50Z", "success"),
        ],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:42:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.attempts.len(), 1);
    let attempt = &run.attempts[0];
    assert_eq!(attempt.duration, Some(Duration::from_secs(150)));
    assert_eq!(attempt.outcome.as_deref(), Some("success"));
}

/// Unpaired starts: a trailing `attempt_started` with no matching
/// `attempt_finished` shows a start with no finish and no duration.
#[test]
fn unpaired_start_has_no_finish_event() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(run_id, &work_manifest(run_id, "work"));
    write_roster(
        &temp,
        run_id,
        &[(
            "profile-a",
            "anthropic",
            "claude-sonnet-5",
            "claude-code",
            "profile-a",
        )],
    );
    write_events(
        &temp,
        run_id,
        &[
            run_started(run_id, "work"),
            attempt_started(
                2,
                run_id,
                "profile-a",
                "001-profile-a",
                "2026-07-25T18:39:20Z",
            ),
        ],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:42:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.attempts.len(), 1);
    let attempt = &run.attempts[0];
    assert!(attempt.started_at.is_some());
    assert!(
        attempt.finished_at.is_none(),
        "unpaired start has no finish event"
    );
    assert!(attempt.duration.is_none());
}

/// Multiple sequential attempts (e.g. a fallback retry) pair the Nth start
/// with the Nth finish by encounter order.
#[test]
fn multiple_sequential_attempts_pair_by_encounter_order() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    temp.write_run(run_id, &work_manifest(run_id, "work"));
    write_roster(
        &temp,
        run_id,
        &[
            (
                "profile-a",
                "anthropic",
                "claude-sonnet-5",
                "claude-code",
                "profile-a",
            ),
            ("profile-b", "codex", "gpt-5.6-luna", "codex", "profile-b"),
        ],
    );
    write_events(
        &temp,
        run_id,
        &[
            run_started(run_id, "work"),
            attempt_started(
                2,
                run_id,
                "profile-a",
                "001-profile-a",
                "2026-07-25T18:39:20Z",
            ),
            attempt_finished(3, run_id, "profile-a", "2026-07-25T18:39:30Z", "failed"),
            attempt_started(
                4,
                run_id,
                "profile-b",
                "002-profile-b",
                "2026-07-25T18:39:31Z",
            ),
            attempt_finished(5, run_id, "profile-b", "2026-07-25T18:40:00Z", "success"),
        ],
    );

    let source = temp.source();
    let run = source
        .snapshot_for_run_pub(run_id, "2026-07-25T18:42:00Z".parse().unwrap())
        .expect("snapshot");
    assert_eq!(run.attempts.len(), 2);
    assert_eq!(run.attempts[0].ordinal, 1);
    assert_eq!(run.attempts[0].profile_id.as_deref(), Some("profile-a"));
    assert_eq!(run.attempts[0].outcome.as_deref(), Some("failed"));
    assert_eq!(run.attempts[1].ordinal, 2);
    assert_eq!(run.attempts[1].profile_id.as_deref(), Some("profile-b"));
    assert_eq!(run.attempts[1].outcome.as_deref(), Some("success"));
}

/// The pilot run's single attempt, read from a byte-shape copy of the real
/// run directory: the `running:<attempt-dir>` outcome supplies the opaque
/// directory and ordinal, and the run-local `roster.json` — parsed by the
/// existing Musterroll snapshot parser — supplies provider/model/harness/
/// dispatch identity. The profile id is matched whole; nothing is split out
/// of the attempt-directory string.
#[test]
fn patchstand_attempt_resolves_identity_through_run_local_roster() {
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
    assert_eq!(run.attempts.len(), 1, "one dispatched attempt");
    let attempt = &run.attempts[0];
    assert_eq!(attempt.ordinal, 1);
    assert_eq!(attempt.attempt_dir.as_deref(), Some(PATCHSTAND_ATTEMPT_DIR));
    assert_eq!(attempt.profile_id.as_deref(), Some(PATCHSTAND_PROFILE_ID));
    assert!(attempt.resolved, "run-local roster resolves the profile");
    assert_eq!(attempt.provider_id.as_deref(), Some("openai-codex"));
    assert_eq!(attempt.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(attempt.harness.as_deref(), Some("codex"));
    assert_eq!(attempt.dispatch_id.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(attempt.outcome.as_deref(), Some("success"));
    assert!(attempt.duration.is_some(), "start and finish pair");
    assert!(
        run.stage_markers.is_empty(),
        "work runs have no plan stages"
    );
}

/// An unparseable run-local `roster.json` must leave the opaque profile id
/// visible *and* state why it could not be resolved. Silently degrading every
/// attempt to `resolved: false` with no reason is indistinguishable from a
/// run that simply has no roster snapshot.
#[test]
fn unparseable_run_local_roster_reports_why_profiles_are_unresolved() {
    let temp = TempState::new();
    let run_dir = temp.write_patchstand_run(
        PATCHSTAND_RUN_ID,
        "2026-07-25T18:39:20.469500+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        "2026-07-25T18:43:36.460155+00:00",
        std::process::id(),
        std::process::id(),
    );
    fs::write(run_dir.join("roster.json"), b"{ not a roster }").expect("clobber roster");

    let run = temp
        .source()
        .select(&RunSelection::Explicit(PATCHSTAND_RUN_ID.to_string()))
        .expect("snapshot");

    assert_eq!(run.attempts.len(), 1);
    let attempt = &run.attempts[0];
    assert!(!attempt.resolved, "a broken roster resolves nothing");
    assert_eq!(
        attempt.profile_id.as_deref(),
        Some(PATCHSTAND_PROFILE_ID),
        "the opaque profile id stays visible"
    );
    assert!(attempt.model.is_none());
    assert!(
        run.roster_error
            .as_deref()
            .is_some_and(|error| error.contains("run-local roster")),
        "got: {:?}",
        run.roster_error
    );
}

/// A run with no run-local roster at all reports no roster error: the
/// per-attempt unresolved marker is already the whole story.
#[test]
fn missing_run_local_roster_is_not_reported_as_an_error() {
    let temp = TempState::new();
    let run_dir = temp.write_patchstand_run(
        PATCHSTAND_RUN_ID,
        "2026-07-25T18:39:20.469500+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        "2026-07-25T18:43:36.460155+00:00",
        std::process::id(),
        std::process::id(),
    );
    fs::remove_file(run_dir.join("roster.json")).expect("remove roster");

    let run = temp
        .source()
        .select(&RunSelection::Explicit(PATCHSTAND_RUN_ID.to_string()))
        .expect("snapshot");

    assert_eq!(run.roster_error, None);
    assert!(!run.attempts[0].resolved);
    assert_eq!(
        run.attempts[0].profile_id.as_deref(),
        Some(PATCHSTAND_PROFILE_ID)
    );
}
