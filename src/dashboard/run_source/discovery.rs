//! Bounded run discovery tests: `cargo test dashboard::run_source::discovery`.

use super::test_support::TempState;
use super::*;
use std::fs;

#[test]
fn selects_newest_by_created_at_not_directory_name() {
    let temp = TempState::new();
    // Directory names are in reverse of created_at order; the run named
    // "aaa" must lose to "zzz" only if "zzz" is newer, and vice versa.
    temp.write_run(
        "run-work-20260725T100000.000000000-p1-000000",
        &temp.work_manifest(
            "run-work-20260725T100000.000000000-p1-000000",
            "2026-07-25T10:00:00Z",
            "running",
        ),
    );
    temp.write_run(
        "run-work-20260725T120000.000000000-p2-000000",
        &temp.work_manifest(
            "run-work-20260725T120000.000000000-p2-000000",
            "2026-07-25T12:00:00Z",
            "running",
        ),
    );
    // A directory named alphabetically later but with an earlier
    // created_at must NOT win.
    temp.write_run(
        "run-work-20260725T090000.000000000-p3-000000",
        &temp.work_manifest(
            "run-work-20260725T090000.000000000-p3-000000",
            "2026-07-25T09:00:00Z",
            "running",
        ),
    );

    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");
    assert_eq!(
        run.identity.run_id,
        "run-work-20260725T120000.000000000-p2-000000"
    );
}

/// Tie-break: equal `created_at` timestamps break by directory name
/// descending (the directory that sorts later wins).
#[test]
fn tie_breaks_by_directory_name_descending() {
    let temp = TempState::new();
    let ts = "2026-07-25T12:00:00Z";
    temp.write_run(
        "run-work-20260725T120000.000000000-aaa-000000",
        &temp.work_manifest(
            "run-work-20260725T120000.000000000-aaa-000000",
            ts,
            "running",
        ),
    );
    temp.write_run(
        "run-work-20260725T120000.000000000-zzz-000000",
        &temp.work_manifest(
            "run-work-20260725T120000.000000000-zzz-000000",
            ts,
            "running",
        ),
    );
    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");
    assert_eq!(
        run.identity.run_id,
        "run-work-20260725T120000.000000000-zzz-000000"
    );
}

/// The 200-candidate cap: only the 200 most recently modified manifests are
/// The 200-candidate cap: only the 200 most recently modified manifests are
/// scanned. A 201st run with the newest `created_at` but the oldest mtime
/// is excluded from discovery.
#[test]
fn caps_at_two_hundred_candidates() {
    let temp = TempState::new();
    // The 201st run: newest created_at, but written first (oldest mtime).
    let newest_id = "run-work-20260725T235959.000000000-p999-000000";
    temp.write_run(
        newest_id,
        &temp.work_manifest(newest_id, "2026-07-25T23:59:59Z", "running"),
    );
    // 200 runs written after, each with a valid, strictly-older created_at
    // and a newer mtime (so all 200 are within the cap and the 201st is
    // the oldest by mtime).
    for i in 0..200u32 {
        let minute = i % 59;
        let second = i;
        let id = format!("run-work-20260725T12{minute:02}{second:02}.000000000-p{i:03}-000000");
        let ts = format!("2026-07-25T12:{minute:02}:{second:02}Z");
        temp.write_run(&id, &temp.work_manifest(&id, &ts, "running"));
    }

    let source = temp.source();
    // The 201st run (newest created_at, oldest mtime) must be excluded by
    // the 200-candidate cap; it must never be selected.
    let run = source.select(&RunSelection::Newest).expect("select");
    assert_ne!(
        run.identity.run_id, newest_id,
        "the 201st run (oldest mtime) must be excluded by the 200-candidate cap"
    );
}

/// The 128 KiB manifest cap: a manifest larger than 128 KiB is truncated
/// during read; if the truncation breaks JSON it fails the source closed.
#[test]
fn manifest_read_is_capped_at_128_kib() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T120000.000000000-p1-000000";
    let run_dir = temp.write_run(
        run_id,
        &temp.work_manifest(run_id, "2026-07-25T12:00:00Z", "running"),
    );
    // Overwrite manifest with a >128 KiB document whose first 128 KiB is
    // valid JSON (a deeply nested object) but which is truncated mid-value
    // at the cap. Easiest: write a valid manifest followed by 200 KiB of
    // trailing whitespace inside the JSON object via an unknown field
    // with a huge string. The cap reads only the first 128 KiB, cutting
    // the string mid-value, so JSON parse fails.
    let huge = "x".repeat(200 * 1024);
    let manifest = serde_json::json!({
        "schema": "undertake/run@2",
        "run_id": run_id,
        "job": "work",
        "target": {"repo": "/repo", "bead": "b"},
        "details": {"job": "work", "state": {"cycle_id": "c", "authorization_sha256": "a".repeat(64), "stage": "implementing"}},
        "created_at": "2026-07-25T12:00:00Z",
        "updated_at": "2026-07-25T12:00:00Z",
        "approved_profiles": [],
        "limits": {},
        "verifier": {},
        "lifecycle": "running",
        "unknown_blob": huge,
    });
    let bytes = serde_json::to_vec(&manifest).unwrap();
    assert!(bytes.len() > 128 * 1024);
    fs::write(run_dir.join("manifest.json"), &bytes).unwrap();

    let source = temp.source();
    let run = source
        .select(&RunSelection::Explicit(run_id.to_string()))
        .expect("select");
    // The truncated manifest fails closed: the snapshot carries the error.
    assert!(
        run.selection_error
            .as_ref()
            .is_some_and(|e| e.contains("failed to parse manifest")),
        "truncated manifest must surface a parse error, got: {:?}",
        run.selection_error
    );
}

/// A validated explicit id: an explicit run id must pass run-id validation
/// before joining `runs-v2/`.
#[test]
fn explicit_id_is_validated() {
    let temp = TempState::new();
    let source = temp.source();
    // Traversal is rejected.
    let err = source
        .select(&RunSelection::Explicit("../etc/passwd".to_string()))
        .expect_err("traversal id rejected");
    assert!(err.message().contains("invalid run id"));
    // Multi-component rejected.
    let err = source
        .select(&RunSelection::Explicit("a/b".to_string()))
        .expect_err("multi-component rejected");
    assert!(err.message().contains("invalid run id"));
    // Empty rejected.
    let err = source
        .select(&RunSelection::Explicit(String::new()))
        .expect_err("empty rejected");
    assert!(err.message().contains("invalid run id"));
}

/// An unknown explicit id fails closed.
#[test]
fn explicit_unknown_id_fails_closed() {
    let temp = TempState::new();
    let source = temp.source();
    let err = source
        .select(&RunSelection::Explicit(
            "run-work-20260725T120000.000000000-p1-000000".to_string(),
        ))
        .expect_err("unknown id fails closed");
    assert!(err.message().contains("unknown run id"));
}

/// A malformed newest candidate is selected and displayed with its error
/// rather than silently falling back to an older, valid run.
#[test]
fn malformed_newest_candidate_is_selected_with_error() {
    let temp = TempState::new();
    // A valid older run.
    temp.write_run(
        "run-work-20260725T100000.000000000-p1-000000",
        &temp.work_manifest(
            "run-work-20260725T100000.000000000-p1-000000",
            "2026-07-25T10:00:00Z",
            "running",
        ),
    );
    // A malformed newer run (invalid JSON).
    let malformed_dir = temp
        .runs_dir()
        .join("run-work-20260725T120000.000000000-p2-000000");
    fs::create_dir_all(&malformed_dir).unwrap();
    fs::write(malformed_dir.join("manifest.json"), b"{ not valid json").unwrap();

    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");
    assert_eq!(
        run.identity.run_id,
        "run-work-20260725T120000.000000000-p2-000000"
    );
    assert!(
        run.selection_error
            .as_ref()
            .is_some_and(|e| e.contains("failed to parse manifest")),
        "malformed newest must carry a selection error, got: {:?}",
        run.selection_error
    );
}

/// Sets a directory's mtime explicitly (rather than relying on write-order
/// timing) so the malformed-vs-valid ordering tests below are deterministic
/// regardless of filesystem mtime resolution.
fn set_dir_mtime(dir: &std::path::Path, seconds_ago: u64) {
    let file = fs::File::open(dir).expect("open dir for mtime set");
    let target = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds_ago);
    let times = std::fs::FileTimes::new().set_modified(target);
    file.set_times(times).expect("set dir mtime");
}

/// The discriminating regression for the sort-order fix: an *older*
/// malformed run must lose to a *newer* valid nonterminal run, even though
/// the malformed candidate's parsed `created_at` is `None` (which the
/// pre-fix comparison treated as unconditionally "greatest" against every
/// real timestamp). Both directory mtimes are set explicitly so the
/// outcome does not depend on filesystem mtime resolution or write-order
/// timing.
#[test]
fn an_older_malformed_run_loses_to_a_newer_valid_nonterminal_run() {
    let temp = TempState::new();

    // A malformed run, touched an hour ago.
    let malformed_dir = temp
        .runs_dir()
        .join("run-work-20260725T100000.000000000-p1-000000");
    fs::create_dir_all(&malformed_dir).unwrap();
    fs::write(malformed_dir.join("manifest.json"), b"{ not valid json").unwrap();
    set_dir_mtime(&malformed_dir, 3600);

    // A valid nonterminal run, touched just now — newer than the malformed
    // run's directory, even though its own `created_at` predates the
    // malformed run's directory name by a wide margin. That mismatch is
    // the point: valid candidates rank by `created_at`, malformed ones by
    // mtime, and the cross-group decision is mtime vs mtime, never
    // `created_at` vs mtime.
    let valid_id = "run-work-20260101T000000.000000000-p2-000000";
    let valid_dir = temp.write_run(
        valid_id,
        &temp.work_manifest(valid_id, "2026-01-01T00:00:00Z", "running"),
    );
    set_dir_mtime(&valid_dir, 0);

    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");
    assert_eq!(
        run.identity.run_id, valid_id,
        "an older malformed run must lose to a newer valid nonterminal run"
    );
    assert_eq!(
        run.selection_error, None,
        "the winning valid run must carry no selection error"
    );
}

/// The mirror case: when the malformed run's directory genuinely is the
/// most recently touched thing on disk, it must stay visible (with its
/// error) rather than being unconditionally demoted beneath every valid
/// run — the other half of the fix, so neither direction regresses.
#[test]
fn a_newer_malformed_run_still_beats_an_older_valid_nonterminal_run() {
    let temp = TempState::new();

    let valid_id = "run-work-20260101T000000.000000000-p2-000000";
    let valid_dir = temp.write_run(
        valid_id,
        &temp.work_manifest(valid_id, "2026-01-01T00:00:00Z", "running"),
    );
    set_dir_mtime(&valid_dir, 3600);

    let malformed_id = "run-work-20260725T100000.000000000-p1-000000";
    let malformed_dir = temp.runs_dir().join(malformed_id);
    fs::create_dir_all(&malformed_dir).unwrap();
    fs::write(malformed_dir.join("manifest.json"), b"{ not valid json").unwrap();
    set_dir_mtime(&malformed_dir, 0);

    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");
    assert_eq!(
        run.identity.run_id, malformed_id,
        "a genuinely newest malformed candidate must stay visible"
    );
    assert!(run.selection_error.is_some());
}

/// Forward compatibility: unknown extra manifest fields are tolerated.
#[test]
fn unknown_manifest_fields_are_tolerated() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T120000.000000000-p1-000000";
    let mut manifest = temp.work_manifest(run_id, "2026-07-25T12:00:00Z", "running");
    manifest["future_field"] = serde_json::json!({"anything": "here"});
    manifest["another"] = serde_json::json!(42);
    temp.write_run(run_id, &manifest);
    let source = temp.source();
    let run = source
        .select(&RunSelection::Explicit(run_id.to_string()))
        .expect("select");
    assert!(run.selection_error.is_none());
    assert_eq!(run.identity.run_id, run_id);
}

/// An unknown manifest schema fails the source closed.
#[test]
fn unknown_manifest_schema_fails_closed() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T120000.000000000-p1-000000";
    let mut manifest = temp.work_manifest(run_id, "2026-07-25T12:00:00Z", "running");
    manifest["schema"] = serde_json::json!("undertake/run@3");
    temp.write_run(run_id, &manifest);
    let source = temp.source();
    let run = source
        .select(&RunSelection::Explicit(run_id.to_string()))
        .expect("select");
    assert!(
        run.selection_error
            .as_ref()
            .is_some_and(|e| e.contains("unknown schema")),
        "unknown schema must fail closed, got: {:?}",
        run.selection_error
    );
}

/// Default selection prefers the newest nonterminal run even when a newer
/// terminal run exists.
#[test]
fn default_prefers_newest_nonterminal_over_newer_terminal() {
    let temp = TempState::new();
    // An older nonterminal run.
    temp.write_run(
        "run-work-20260725T100000.000000000-p1-000000",
        &temp.work_manifest(
            "run-work-20260725T100000.000000000-p1-000000",
            "2026-07-25T10:00:00Z",
            "running",
        ),
    );
    // A newer terminal run.
    temp.write_run(
        "run-work-20260725T120000.000000000-p2-000000",
        &temp.work_manifest(
            "run-work-20260725T120000.000000000-p2-000000",
            "2026-07-25T12:00:00Z",
            "finished",
        ),
    );
    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");
    // The nonterminal run is preferred despite being older.
    assert_eq!(
        run.identity.run_id,
        "run-work-20260725T100000.000000000-p1-000000"
    );
}

/// When no nonterminal run exists, the newest terminal run is selected.
#[test]
fn falls_back_to_newest_terminal_run() {
    let temp = TempState::new();
    temp.write_run(
        "run-work-20260725T100000.000000000-p1-000000",
        &temp.work_manifest(
            "run-work-20260725T100000.000000000-p1-000000",
            "2026-07-25T10:00:00Z",
            "finished",
        ),
    );
    temp.write_run(
        "run-work-20260725T120000.000000000-p2-000000",
        &temp.work_manifest(
            "run-work-20260725T120000.000000000-p2-000000",
            "2026-07-25T12:00:00Z",
            "finished",
        ),
    );
    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");
    assert_eq!(
        run.identity.run_id,
        "run-work-20260725T120000.000000000-p2-000000"
    );
}

/// Mixed plan and work ids are both eligible for discovery.
#[test]
fn mixed_plan_and_work_ids_are_eligible() {
    let temp = TempState::new();
    let work_id = "run-work-20260725T100000.000000000-p1-000000";
    temp.write_run(
        work_id,
        &temp.work_manifest(work_id, "2026-07-25T10:00:00Z", "running"),
    );
    let plan_id = "run-plan-20260725T120000.000000000-p2-000000";
    let plan_manifest = serde_json::json!({
        "schema": "undertake/run@2",
        "run_id": plan_id,
        "job": "plan",
        "target": {"repo": "/repo/x"},
        "details": {"job": "plan", "state": {
            "target": {"repo": "/repo/x", "input": {"kind": "bead", "bead_id": "b1", "artifact": {"path": "in.txt", "sha256": "a".repeat(64)}, "tier": "junior", "complexity": "S"}},
            "routes": {"stages": [
                {"stage": "planner", "capability_role": "author", "candidates": [{"profile_id": "p", "provider_id": "pr", "availability_key": "ak", "execution_key": "ek"}], "provider_distinct_from": [], "constraints": {"distinct_execution_from": [], "tier_at_least": [], "provider_diversity": "none"}},
                {"stage": "peer_review", "capability_role": "peer", "candidates": [{"profile_id": "p2", "provider_id": "pr", "availability_key": "ak", "execution_key": "ek2"}], "provider_distinct_from": [], "constraints": {"distinct_execution_from": [], "tier_at_least": [], "provider_diversity": "none"}},
                {"stage": "second_opinion", "capability_role": "judge", "candidates": [{"profile_id": "p3", "provider_id": "pr", "availability_key": "ak", "execution_key": "ek3"}], "provider_distinct_from": [], "constraints": {"distinct_execution_from": [], "tier_at_least": [], "provider_diversity": "none"}}
            ]},
            "progress": {"state": "prepared"},
            "stage_attempts": {"planner": 0, "peer_review": 0, "second_opinion": 0},
            "revision_limit": 0,
            "stage_attempt_limit": 1
        }},
        "created_at": "2026-07-25T12:00:00Z",
        "updated_at": "2026-07-25T12:00:00Z",
        "approved_profiles": [],
        "limits": {},
        "verifier": {},
        "lifecycle": "running",
    });
    temp.write_run(plan_id, &plan_manifest);
    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");
    assert_eq!(run.identity.run_id, plan_id);
}

/// An empty runs directory yields a "no runs found" error.
#[test]
fn empty_runs_directory_yields_no_runs_error() {
    let temp = TempState::new();
    let source = temp.source();
    let err = source
        .select(&RunSelection::Newest)
        .expect_err("empty must error");
    assert!(err.message().contains("no runs found"));
}

/// A source that has never produced a value must report `Absent` with its
/// failed attempt and error — never `Stale` with a fabricated `last_ok`.
/// A renderer reading `last_ok == now` would present a source that has
/// never succeeded as freshly read.
#[test]
fn never_succeeded_run_source_is_absent_with_error_not_fake_stale() {
    let temp = TempState::new();
    let now: DateTime<Utc> = "2026-07-25T20:00:00Z".parse().expect("now");
    let snapshot = temp.source().snapshot(
        None,
        &RunSelection::Explicit("no-such-run".to_string()),
        now,
    );

    assert!(
        matches!(snapshot.run, SourceState::Absent { .. }),
        "never-read source must be Absent, got {:?}",
        snapshot.run
    );
    assert_eq!(snapshot.run.last_ok(), None, "nothing ever succeeded");
    assert_eq!(snapshot.run.last_attempt(), Some(now));
    assert!(
        snapshot
            .run
            .error()
            .is_some_and(|error| error.contains("unknown run id")),
        "got: {:?}",
        snapshot.run.error()
    );
    assert!(!snapshot.run.is_fresh());
}

/// Once a source has produced a value, a later failure retains that value
/// and marks it `Stale` with the real `last_ok` — the state the model's
/// docs describe.
#[test]
fn previously_valid_run_source_goes_stale_retaining_its_value() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T120000.000000000-p2-000000";
    temp.write_run(
        run_id,
        &temp.work_manifest(run_id, "2026-07-25T12:00:00Z", "running"),
    );
    let source = temp.source();
    let first: DateTime<Utc> = "2026-07-25T20:00:00Z".parse().expect("first");
    let good = source.snapshot(None, &RunSelection::Newest, first);
    assert!(good.run.is_fresh());
    assert_eq!(
        good.run.value().map(|run| run.identity.run_id.as_str()),
        Some(run_id)
    );

    fs::remove_dir_all(temp.runs_dir().join(run_id)).expect("remove run");
    let second: DateTime<Utc> = "2026-07-25T20:00:05Z".parse().expect("second");
    let degraded = source.snapshot(Some(&good), &RunSelection::Newest, second);

    assert!(
        matches!(degraded.run, SourceState::Stale { .. }),
        "a source with a prior value degrades to Stale, got {:?}",
        degraded.run
    );
    assert_eq!(degraded.run.last_ok(), Some(first), "the real last success");
    assert_eq!(degraded.run.last_attempt(), Some(second));
    assert_eq!(
        degraded.run.value().map(|run| run.identity.run_id.as_str()),
        Some(run_id),
        "the last valid value is retained"
    );
}

/// A run-source tick never samples a service, so it must carry the existing
/// service states forward *by reference*. Deep-copying them would reproduce up
/// to 20,000 retained Afterfact events on every refresh to rebuild a value
/// nothing modified.
#[test]
fn a_run_source_tick_shares_service_states_rather_than_copying_them() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T120000.000000000-p2-000000";
    temp.write_run(
        run_id,
        &temp.work_manifest(run_id, "2026-07-25T12:00:00Z", "running"),
    );
    let source = temp.source();
    let first: DateTime<Utc> = "2026-07-25T20:00:00Z".parse().expect("first");

    let cold = source.snapshot(None, &RunSelection::Newest, first);

    assert!(
        matches!(*cold.musterroll, SourceState::Absent { last_attempt: None, .. }),
        "an unsampled service starts never-read, got {:?}",
        cold.musterroll
    );
    assert!(
        matches!(*cold.cautionlight, SourceState::Deferred { .. }),
        "Cautionlight is deliberately deferred, not merely unread, got {:?}",
        cold.cautionlight
    );

    let second: DateTime<Utc> = "2026-07-25T20:00:05Z".parse().expect("second");
    let next = source.snapshot(Some(&cold), &RunSelection::Newest, second);

    assert!(Arc::ptr_eq(&cold.musterroll, &next.musterroll));
    assert!(Arc::ptr_eq(&cold.afterfact, &next.afterfact));
    assert!(
        Arc::ptr_eq(&cold.cautionlight, &next.cautionlight),
        "the tick must share the service state, not rebuild it"
    );
}

/// The 200-candidate cap bounds *discovery*, not explicit selection. Pinning
/// the dashboard to a named run with `--run <id>` must keep working for a run
/// older than the newest 200 — otherwise the one thing an operator does to
/// inspect a specific stranded run silently reports "unknown run id".
#[test]
fn explicit_id_beyond_the_candidate_cap_still_resolves() {
    let temp = TempState::new();
    let pinned = "run-work-20260101T000000.000000000-p0-000000";
    temp.write_run(
        pinned,
        &temp.work_manifest(pinned, "2026-01-01T00:00:00Z", "running"),
    );
    // Every one of these is newer, so the pinned run falls outside the
    // most-recently-modified window discovery keeps.
    for index in 0..250 {
        let run_id = format!("run-work-20260725T1200{index:02}.000000000-p{index}-000000");
        temp.write_run(
            &run_id,
            &temp.work_manifest(&run_id, "2026-07-25T12:00:00Z", "running"),
        );
    }

    let source = temp.source();
    assert_eq!(
        source.scan_candidates_len_for_tests(),
        200,
        "discovery itself stays capped"
    );
    let run = source
        .select(&RunSelection::Explicit(pinned.to_string()))
        .expect("explicit selection ignores the discovery cap");
    assert_eq!(run.identity.run_id, pinned);
    assert_eq!(run.selection_error, None);
}

/// A run whose manifest cannot be read has an *unknown* identity, not a
/// plausible one. The broken run here is a `plan` run; the snapshot
/// previously carried `job: Work, lifecycle: Started`, so a broken plan run
/// rendered as a started work run and no consumer could tell it apart from
/// the real work run sitting in the same directory.
#[test]
fn malformed_manifest_reports_unknown_identity_instead_of_fabricating_work_started() {
    let temp = TempState::new();
    // A real, valid work run: the exact identity a fabricated one would
    // impersonate.
    let valid = "run-work-20260725T100000.000000000-p1-000000";
    temp.write_run(
        valid,
        &temp.work_manifest(valid, "2026-07-25T10:00:00Z", "started"),
    );
    // A newer plan run whose manifest is unparseable, so it is selected.
    let broken = "run-plan-20260725T120000.000000000-p2-000000";
    let broken_dir = temp.runs_dir().join(broken);
    fs::create_dir_all(&broken_dir).unwrap();
    fs::write(broken_dir.join("manifest.json"), b"{ not valid json").unwrap();

    let source = temp.source();
    let run = source.select(&RunSelection::Newest).expect("select");

    assert_eq!(run.identity.run_id, broken);
    assert!(
        run.selection_error.is_some(),
        "the unreadable manifest must stay visible as an error"
    );
    assert_eq!(
        run.identity.job, None,
        "an unreadable manifest must not claim a job"
    );
    assert_eq!(
        run.identity.lifecycle, None,
        "an unreadable manifest must not claim a lifecycle"
    );
    assert_eq!(run.identity.liveness, RunLiveness::Unknown);

    // `None` means unknown, not "the dashboard stopped reporting identity":
    // the readable run in the same directory still reports its real one.
    let readable = source
        .select(&RunSelection::Explicit(valid.to_string()))
        .expect("select valid");
    assert_eq!(readable.identity.job, Some(RunJob::Work));
    assert_eq!(readable.identity.lifecycle, Some(RunLifecycle::Started));
}

/// One unreadable directory entry must not take the whole source down: the
/// readable run still renders, and the failure stays visible as a bounded
/// discovery warning rather than vanishing.
///
/// The failing entry is synthesized rather than provoked on disk. A
/// per-entry `readdir`/`stat` failure is governed by the *parent*
/// directory's search permission, so no on-disk arrangement breaks exactly
/// one entry of a real listing — which is precisely the case that must be
/// discriminated from "the directory itself is unreadable".
#[test]
fn one_unreadable_dirent_is_skipped_and_the_readable_run_still_renders() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T120000.000000000-p1-000000";
    temp.write_run(
        run_id,
        &temp.work_manifest(run_id, "2026-07-25T12:00:00Z", "running"),
    );
    let source = temp.source();

    let entries = fs::read_dir(temp.runs_dir())
        .expect("read runs dir")
        .chain(std::iter::once(Err(std::io::Error::other(
            "simulated readdir failure",
        ))));
    let scan = DashboardRunSource::scan_entries(entries);

    assert_eq!(
        scan.candidates.len(),
        1,
        "the readable run must survive an unreadable sibling entry"
    );
    let warning = scan
        .warnings
        .message()
        .expect("the skipped entry must stay visible");
    assert!(
        warning.contains("simulated readdir failure"),
        "the warning must say what failed, got: {warning}"
    );

    let now: DateTime<Utc> = "2026-07-25T20:00:00Z".parse().expect("now");
    let snapshot = source.snapshot_from_scan(None, &RunSelection::Newest, now, &scan);
    assert!(
        snapshot.run.is_fresh(),
        "one bad entry must not degrade the run source, got: {:?}",
        snapshot.run
    );
    assert_eq!(
        snapshot.run.value().map(|run| run.identity.run_id.as_str()),
        Some(run_id)
    );
    assert_eq!(
        snapshot.discovery_warning.as_deref(),
        Some(warning.as_str()),
        "the warning must reach the snapshot the renderer consumes"
    );
}

/// The discovery warning is bounded: many unreadable entries collapse into
/// one counted message carrying only the first error, never a per-entry list
/// that grows with the directory.
#[test]
fn many_unreadable_dirents_collapse_into_one_bounded_warning() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T120000.000000000-p1-000000";
    temp.write_run(
        run_id,
        &temp.work_manifest(run_id, "2026-07-25T12:00:00Z", "running"),
    );

    let failures = (0..50).map(|index| Err(std::io::Error::other(format!("failure {index}"))));
    let entries = fs::read_dir(temp.runs_dir())
        .expect("read runs dir")
        .chain(failures);
    let scan = DashboardRunSource::scan_entries(entries);

    assert_eq!(scan.candidates.len(), 1, "the readable run must survive");
    let warning = scan.warnings.message().expect("warning");
    assert!(
        warning.contains("50 run directory entries"),
        "the count must be reported, got: {warning}"
    );
    assert!(
        warning.contains("failure 0"),
        "the first error is retained, got: {warning}"
    );
    assert!(
        !warning.contains("failure 1"),
        "later errors must not accumulate into the message, got: {warning}"
    );
}

/// A clean discovery pass carries no warning, so the warning means
/// "something was unreadable" rather than being permanent decoration.
#[test]
fn a_clean_discovery_pass_carries_no_warning() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T120000.000000000-p1-000000";
    temp.write_run(
        run_id,
        &temp.work_manifest(run_id, "2026-07-25T12:00:00Z", "running"),
    );
    // A non-directory entry and a directory with no manifest are ordinary,
    // not failures: neither may raise a warning.
    fs::write(temp.runs_dir().join("stray-file"), b"not a run").unwrap();
    fs::create_dir_all(temp.runs_dir().join("empty-dir")).unwrap();

    let now: DateTime<Utc> = "2026-07-25T20:00:00Z".parse().expect("now");
    let snapshot = temp.source().snapshot(None, &RunSelection::Newest, now);
    assert_eq!(snapshot.discovery_warning, None);
    assert!(snapshot.run.is_fresh());
}
