//! Harness Deck report-join tests:
//! `cargo test dashboard::run_source::harness_deck`.
//!
//! Spec §105-110: work joins on `details.state.cycle_id`, plan joins on its
//! own `run_id`, and consult/review have no report at all. The join resolves
//! only through [`crate::deck::report_run_dir`] — the same validated
//! report-root helper the report writer uses — and reports presence without
//! ever opening the report.

use std::fs;

use super::test_support::{PATCHSTAND_RUN_ID, TempState};
use super::*;

/// Materializes a Harness Deck report for `report_run_id` under the fixture's
/// reports home, exactly where the writer would put it.
fn write_report(temp: &TempState, report_run_id: &str) -> PathBuf {
    let dir = crate::deck::report_run_dir(&temp.config().reports_home, report_run_id)
        .expect("a well-formed report run id");
    fs::create_dir_all(&dir).expect("mkdir report dir");
    fs::write(
        dir.join("report.json"),
        b"{\"schema\":\"harness-deck/report@1\"}\n",
    )
    .expect("write report");
    dir
}

fn snapshot_of(temp: &TempState, run_id: &str) -> RunSnapshot {
    temp.source()
        .select(&RunSelection::Explicit(run_id.to_string()))
        .expect("the run resolves")
}

fn plan_manifest(run_id: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "undertake/run@2",
        "run_id": run_id,
        "job": "plan",
        "target": {"repo": "/repo/patchstand"},
        "details": {"job": "plan", "state": {"progress": {"state": "authoring"}}},
        "created_at": "2026-07-25T18:39:20.469500+00:00",
        "updated_at": "2026-07-25T18:39:20.469500+00:00",
        "lifecycle": "running",
    })
}

fn jobless_manifest(run_id: &str, job: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "undertake/run@2",
        "run_id": run_id,
        "job": job,
        "target": {"repo": "/repo/patchstand"},
        "details": {"job": job, "state": {}},
        "created_at": "2026-07-25T18:39:20.469500+00:00",
        "updated_at": "2026-07-25T18:39:20.469500+00:00",
        "lifecycle": "running",
    })
}

/// The pilot run's own join: `cycle-20260725-183823` is a real report
/// directory, and the dashboard finds it.
#[test]
fn a_work_run_joins_its_cycle_id_to_an_existing_report() {
    let temp = TempState::new();
    temp.write_patchstand_run(
        PATCHSTAND_RUN_ID,
        "2026-07-25T18:39:20.469500+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        45813,
        46133,
    );
    let report_dir = write_report(&temp, "cycle-20260725-183823");

    assert_eq!(
        snapshot_of(&temp, PATCHSTAND_RUN_ID).harness_deck,
        HarnessDeckState::Resolved {
            report_dir: report_dir.display().to_string(),
            present: true,
        }
    );
}

/// A resolved directory with nothing in it is not a report. The two must
/// stay distinguishable: "here is the report" and "the report was never
/// written" are different operational facts.
#[test]
fn a_resolved_directory_without_a_report_is_not_reported_as_one() {
    let temp = TempState::new();
    temp.write_patchstand_run(
        PATCHSTAND_RUN_ID,
        "2026-07-25T18:39:20.469500+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        45813,
        46133,
    );

    let HarnessDeckState::Resolved {
        report_dir,
        present,
    } = snapshot_of(&temp, PATCHSTAND_RUN_ID).harness_deck
    else {
        panic!("a work run with a cycle id resolves a directory");
    };
    assert!(!present, "no report.json exists yet");
    assert!(report_dir.ends_with("/.harness/reports/undertake/cycle-20260725-183823"));
}

/// A work run whose state records no cycle id has no join key, which is a
/// different fact from a missing report — and must not be silently rendered
/// as one.
#[test]
fn a_work_run_without_a_cycle_id_is_unresolved_not_absent() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T000000.000000000-p1-000000";
    let mut manifest = temp.work_manifest(run_id, "2026-07-25T00:00:00+00:00", "running");
    manifest["details"]["state"] = serde_json::json!({"stage": "implementing"});
    temp.write_run(run_id, &manifest);

    assert_eq!(
        snapshot_of(&temp, run_id).harness_deck,
        HarnessDeckState::Unresolved {
            reason: "run state records no cycle id".to_string(),
        }
    );
}

/// Plan runs join on their own run id, not on a cycle id they never have.
#[test]
fn a_plan_run_joins_on_its_own_run_id() {
    let temp = TempState::new();
    let run_id = "plan-20260725T011401598940000-p72391-000000";
    temp.write_run(run_id, &plan_manifest(run_id));
    let report_dir = write_report(&temp, run_id);

    assert_eq!(
        snapshot_of(&temp, run_id).harness_deck,
        HarnessDeckState::Resolved {
            report_dir: report_dir.display().to_string(),
            present: true,
        }
    );
}

/// Consult and review publish no report. This is a static job fact: it holds
/// even when a directory named after the run exists and contains a report,
/// so the state can never be a lookup that merely happened to fail.
#[test]
fn consult_and_review_have_no_report_by_definition() {
    let temp = TempState::new();
    for (job, run_id) in [
        ("consult", "run-consult-20260725T183837.579080000-p1-000000"),
        ("review", "run-review-20260725T183837.579080000-p2-000000"),
    ] {
        temp.write_run(run_id, &jobless_manifest(run_id, job));
        write_report(&temp, run_id);
        assert_eq!(
            snapshot_of(&temp, run_id).harness_deck,
            HarnessDeckState::NoReportForJob,
            "{job} must never claim a Harness Deck report"
        );
    }
}

/// The join key is untrusted manifest content. Every candidate here either
/// fails the report-run-id validator outright or is a name that stays inside
/// the reports home; none may produce a path that escapes it.
#[test]
fn a_traversal_cycle_id_never_escapes_the_reports_home() {
    let temp = TempState::new();
    let reports_home = temp.config().reports_home;
    let run_id = "run-work-20260725T000000.000000000-p1-000000";

    for hostile in [
        "../../../../etc",
        "..",
        "/etc/passwd",
        "cycle/../../escape",
        "",
        "cycle id with spaces",
    ] {
        let mut manifest = temp.work_manifest(run_id, "2026-07-25T00:00:00+00:00", "running");
        manifest["details"]["state"] =
            serde_json::json!({"stage": "implementing", "cycle_id": hostile});
        temp.write_run(run_id, &manifest);

        match snapshot_of(&temp, run_id).harness_deck {
            HarnessDeckState::Unresolved { reason } => {
                assert!(
                    reason.contains("invalid run id"),
                    "{hostile:?} must be refused by report-run-id validation, got {reason:?}"
                );
            }
            other => panic!("{hostile:?} must not resolve a report directory, got {other:?}"),
        }
    }

    // The negative control: a legal key resolves, and the path it resolves
    // to is inside the reports home.
    let mut manifest = temp.work_manifest(run_id, "2026-07-25T00:00:00+00:00", "running");
    manifest["details"]["state"] =
        serde_json::json!({"stage": "implementing", "cycle_id": "cycle-20260725-183823"});
    temp.write_run(run_id, &manifest);
    let HarnessDeckState::Resolved { report_dir, .. } = snapshot_of(&temp, run_id).harness_deck
    else {
        panic!("a legal cycle id resolves");
    };
    assert!(
        Path::new(&report_dir).starts_with(&reports_home),
        "{report_dir} escaped {}",
        reports_home.display()
    );
}

/// An unreadable manifest carries no join key at all, so the join is
/// genuinely unattemptable — not "the report is missing".
#[test]
fn an_unreadable_manifest_leaves_the_join_unresolved() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T000000.000000000-p1-000000";
    let run_dir = temp.runs_dir().join(run_id);
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(run_dir.join("manifest.json"), b"{ not json").unwrap();

    assert_eq!(
        snapshot_of(&temp, run_id).harness_deck,
        HarnessDeckState::Unresolved {
            reason: "run manifest unreadable".to_string(),
        }
    );
}

/// Resolving the join stats `report.json` and stops there. A report's own
/// bytes are untrusted prose; nothing in the dashboard opens them, so no
/// report content can reach the render path.
#[test]
fn resolving_the_join_never_opens_the_report() {
    let source = include_str!("../run_source.rs");
    let body = source
        .split("fn derive_harness_deck")
        .nth(1)
        .expect("the join exists")
        .split("\nfn ")
        .next()
        .expect("the join has a body");
    for forbidden in ["read", "File", "open", "serde_json::from"] {
        assert!(
            !body.contains(forbidden),
            "the Harness Deck join must not {forbidden}"
        );
    }
    assert!(body.contains("is_file()"), "presence is a stat");
}
