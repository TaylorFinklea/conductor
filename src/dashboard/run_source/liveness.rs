//! Liveness evidence tests: `cargo test dashboard::run_source::liveness`.

use super::test_support::{PATCHSTAND_RUN_ID, TempState};
use super::*;

/// Returns a pid that is provably dead: spawn `true`, wait for it to
/// exit, then return its pid. `process_alive` fails closed (reads
/// ambiguous probes as alive), so only a confirmed-exited child gives a
/// dead pid.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn true");
    let pid = child.id();
    child.wait().expect("wait true");
    pid
}

/// The current test process's pid is provably alive.
fn live_pid() -> u32 {
    std::process::id()
}

fn write_heartbeat(run_dir: &Path, ts: &str) {
    std::fs::write(run_dir.join("heartbeat"), ts).expect("write heartbeat");
}

fn work_manifest_with_pids(
    run_id: &str,
    created_at: &str,
    owner_pid: Option<u32>,
    worker_pgid: Option<u32>,
) -> serde_json::Value {
    let mut state = serde_json::json!({
        "cycle_id": "c1",
        "authorization_sha256": "a".repeat(64),
        "stage": "implementing"
    });
    if let Some(pid) = owner_pid {
        state["owner_pid"] = serde_json::json!(pid);
    }
    if let Some(pgid) = worker_pgid {
        state["worker_pgid"] = serde_json::json!(pgid);
    }
    serde_json::json!({
        "schema": "undertake/run@2",
        "run_id": run_id,
        "job": "work",
        "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
        "details": {"job": "work", "state": state},
        "created_at": created_at,
        "updated_at": created_at,
        "approved_profiles": [],
        "limits": {},
        "verifier": {},
        "lifecycle": "running",
    })
}

fn select_with_now(temp: &TempState, run_id: &str, now: DateTime<Utc>) -> RunSnapshot {
    let source = temp.source();
    source.snapshot_for_run_pub(run_id, now).expect("snapshot")
}

/// A fresh heartbeat renders `Live`.
#[test]
fn fresh_heartbeat_is_live() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    let run_dir = temp.write_run(
        run_id,
        &work_manifest_with_pids(run_id, "2026-07-25T18:39:20Z", None, None),
    );
    // Heartbeat 10 seconds ago is younger than the 60-second threshold.
    write_heartbeat(&run_dir, "2026-07-25T18:39:10Z");
    let now: DateTime<Utc> = "2026-07-25T18:39:20Z".parse().expect("now");
    let run = select_with_now(&temp, run_id, now);
    assert_eq!(run.identity.liveness, RunLiveness::Live);
}

/// A stale heartbeat with a live recorded PID renders `Silent`.
#[test]
fn stale_heartbeat_live_pid_is_silent() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    let run_dir = temp.write_run(
        run_id,
        &work_manifest_with_pids(run_id, "2026-07-25T18:00:00Z", Some(live_pid()), None),
    );
    // Heartbeat 10 minutes ago is well past the 60-second threshold.
    write_heartbeat(&run_dir, "2026-07-25T18:00:00Z");
    let now: DateTime<Utc> = "2026-07-25T18:39:20Z".parse().expect("now");
    let run = select_with_now(&temp, run_id, now);
    assert_eq!(run.identity.liveness, RunLiveness::Silent);
}

/// A stale heartbeat with dead recorded PIDs and no finish event renders
/// `Abandoned`. This is the Patchstand pilot regression: a stranded work
/// run whose owner pid is provably dead and whose heartbeat has gone
/// silent must never render as `running`/`Live`.
#[test]
fn stale_heartbeat_dead_pids_no_finish_is_abandoned() {
    let temp = TempState::new();
    // Copy the shape of the Patchstand pilot run id.
    let run_id = "run-work-20260725T183920.469500000-p45813-000000";
    let run_dir = temp.write_run(
        run_id,
        &work_manifest_with_pids(run_id, "2026-07-25T18:39:20Z", Some(dead_pid()), None),
    );
    // Heartbeat is well past the 60-second stale threshold.
    write_heartbeat(&run_dir, "2026-07-25T18:39:20Z");
    let now: DateTime<Utc> = "2026-07-25T19:39:20Z".parse().expect("now");
    let run = select_with_now(&temp, run_id, now);
    assert_eq!(
        run.identity.liveness,
        RunLiveness::Abandoned,
        "Patchstand-shape run with stale heartbeat and dead owner pid must be Abandoned, not Live"
    );
}

/// A stale heartbeat with a dead owner pid but a *live* worker process
/// group renders `Silent` (an orphaned worker survives its parent).
#[test]
fn stale_heartbeat_dead_owner_live_worker_is_silent() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    let run_dir = temp.write_run(
        run_id,
        &work_manifest_with_pids(
            run_id,
            "2026-07-25T18:00:00Z",
            Some(dead_pid()),
            Some(live_pid()),
        ),
    );
    write_heartbeat(&run_dir, "2026-07-25T18:00:00Z");
    let now: DateTime<Utc> = "2026-07-25T18:39:20Z".parse().expect("now");
    let run = select_with_now(&temp, run_id, now);
    assert_eq!(run.identity.liveness, RunLiveness::Silent);
}

/// Missing heartbeat with an unparseable `updated_at` (no usable evidence)
/// renders `Unknown`.
#[test]
fn missing_heartbeat_and_unparseable_updated_at_is_unknown() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    let run_dir = temp.write_run(
        run_id,
        &work_manifest_with_pids(run_id, "2026-07-25T18:39:20Z", None, None),
    );
    // Overwrite updated_at with an unparseable value and write no
    // heartbeat, so there is no usable last-seen evidence.
    let mut manifest = work_manifest_with_pids(run_id, "2026-07-25T18:39:20Z", None, None);
    manifest["updated_at"] = serde_json::json!("not-a-timestamp");
    let mut bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    bytes.push(b'\n');
    std::fs::write(run_dir.join("manifest.json"), bytes).unwrap();
    let now: DateTime<Utc> = "2026-07-25T18:39:20Z".parse().expect("now");
    let run = select_with_now(&temp, run_id, now);
    assert_eq!(run.identity.liveness, RunLiveness::Unknown);
}

/// A terminal lifecycle renders `Finished` regardless of heartbeat.
#[test]
fn terminal_lifecycle_is_finished() {
    let temp = TempState::new();
    let run_id = "run-work-20260725T183920.469500000-p1-000000";
    let run_dir = temp.write_run(
        run_id,
        &temp.work_manifest(run_id, "2026-07-25T18:39:20Z", "finished"),
    );
    // A stale heartbeat must not override terminal liveness.
    write_heartbeat(&run_dir, "2026-07-25T18:39:20Z");
    let now: DateTime<Utc> = "2026-07-25T19:39:20Z".parse().expect("now");
    let run = select_with_now(&temp, run_id, now);
    assert_eq!(run.identity.liveness, RunLiveness::Finished);
}

/// The Patchstand pilot regression, driven by a byte-shape copy of the real
/// `run-work-20260725T183920.469500000-p45813-000000` directory rather than
/// a simplified manifest: real `approved_profiles` envelope, real work-state
/// keys, real roster snapshot, real five-line event log.
///
/// A stranded run whose heartbeat has gone silent and whose recorded pids are
/// provably dead must render `Abandoned`, never `running`/`Live`, and every
/// surrounding identity field must survive the read.
#[test]
fn patchstand_pilot_shape_is_abandoned() {
    let temp = TempState::new();
    temp.write_patchstand_run(
        PATCHSTAND_RUN_ID,
        "2026-07-25T18:39:20.469500+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        "2026-07-25T18:43:36.460155+00:00",
        dead_pid(),
        dead_pid(),
    );
    let now: DateTime<Utc> = "2026-07-25T20:00:00Z".parse().expect("now");
    let run = select_with_now(&temp, PATCHSTAND_RUN_ID, now);

    assert_eq!(run.selection_error, None, "real manifest shape must parse");
    assert_eq!(run.identity.run_id, PATCHSTAND_RUN_ID);
    assert_eq!(run.identity.job, RunJob::Work);
    assert_eq!(run.identity.lifecycle, RunLifecycle::Running);
    assert_eq!(run.identity.liveness, RunLiveness::Abandoned);
    assert_eq!(run.identity.stage.as_deref(), Some("implementing"));
    assert_eq!(run.identity.target_bead.as_deref(), Some("patchstand-thk"));
}

/// The same run, selected as the newest nonterminal run rather than by
/// explicit id: discovery must be able to parse a real manifest too.
#[test]
fn patchstand_pilot_shape_is_discoverable_as_newest() {
    let temp = TempState::new();
    temp.write_patchstand_run(
        PATCHSTAND_RUN_ID,
        "2026-07-25T18:39:20.469500+00:00",
        "2026-07-25T18:43:44.617226+00:00",
        "2026-07-25T18:43:36.460155+00:00",
        dead_pid(),
        dead_pid(),
    );
    let run = temp
        .source()
        .select(&RunSelection::Newest)
        .expect("newest run");
    assert_eq!(run.identity.run_id, PATCHSTAND_RUN_ID);
    assert_eq!(run.selection_error, None, "real manifest shape must parse");
}
