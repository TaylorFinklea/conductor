//! Shared test fixtures for `run_source`'s per-step test modules
//! (`discovery`, `liveness`, `events`, `attempts`, `verification`, `logs`).
//! Each lives in its own file directly under `run_source` so the module path
//! matches the plan's exact `cargo test dashboard::run_source::<step>`
//! commands (no intermediate wrapping `tests` module).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The pilot run id whose regression this fixture set pins.
pub(crate) const PATCHSTAND_RUN_ID: &str = "run-work-20260725T183920.469500000-p45813-000000";
/// The one approved profile the pilot run dispatched, resolved through the
/// run-local roster snapshot. Never split into parts.
pub(crate) const PATCHSTAND_PROFILE_ID: &str = "openai-codex--codex--gpt-5.6-luna--high";
/// The attempt directory the `attempt_started` outcome names.
pub(crate) const PATCHSTAND_ATTEMPT_DIR: &str = "001-openai-codex--codex--gpt-5.6-luna--high";
/// That attempt directory's `running:<attempt-dir>` outcome string.
pub(crate) const PATCHSTAND_ATTEMPT_OUTCOME: &str =
    "running:001-openai-codex--codex--gpt-5.6-luna--high";
/// The fixed-allowlist worker stdout path inside the pilot run.
pub(crate) const PATCHSTAND_WORKER_STDOUT: &str =
    "attempts/001-openai-codex--codex--gpt-5.6-luna--high/worker.stdout.log";

/// A temporary state root with helpers for synthesizing run directories.
/// Mirrors the manual `TempDir` pattern used in `src/run.rs` tests (no
/// `tempfile` dev-dependency in this crate).
pub(crate) struct TempState {
    dir: PathBuf,
}

impl TempState {
    pub(crate) fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let counter = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "undertake-dashboard-{}-{nanos}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("mkdir temp");
        Self { dir }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn config(&self) -> RunSourceConfig {
        RunSourceConfig {
            state_root: self.root().to_path_buf(),
            // A scratch reports home under the same temp root: the Harness
            // Deck join resolves, finds no report, and never reaches the
            // real `$HOME`.
            reports_home: self.root().join("reports-home"),
            refresh_interval: Duration::from_secs(1),
        }
    }

    pub(crate) fn source(&self) -> DashboardRunSource {
        DashboardRunSource::new(self.config())
    }

    pub(crate) fn runs_dir(&self) -> PathBuf {
        self.root().join("runs-v2")
    }

    pub(crate) fn write_run(&self, run_id: &str, manifest: &serde_json::Value) -> PathBuf {
        let run_dir = self.runs_dir().join(run_id);
        fs::create_dir_all(&run_dir).expect("mkdir run");
        let mut bytes = serde_json::to_vec_pretty(manifest).expect("serialize manifest");
        bytes.push(b'\n');
        fs::write(run_dir.join("manifest.json"), bytes).expect("write manifest");
        run_dir
    }

    /// Writes a work manifest with the given `created_at` and lifecycle.
    /// Deliberately kept as a method (rather than an associated function)
    /// for ergonomic `temp.work_manifest(...)` call-site syntax across the
    /// per-step test files, even though the body itself doesn't need
    /// `self`.
    #[allow(clippy::unused_self)]
    pub(crate) fn work_manifest(
        &self,
        run_id: &str,
        created_at: &str,
        lifecycle: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "schema": "undertake/run@2",
            "run_id": run_id,
            "job": "work",
            "target": {"repo": "/repo/patchstand", "bead": "patchstand-1"},
            "details": {"job": "work", "state": {"cycle_id": "c1", "authorization_sha256": "a".repeat(64), "stage": "implementing"}},
            "created_at": created_at,
            "updated_at": created_at,
            "approved_profiles": [],
            "limits": {},
            "verifier": {"mechanical": "pnpm check", "qualitative": "lead-review"},
            "lifecycle": lifecycle,
        })
    }

    /// The exact on-disk shape of the Patchstand pilot run
    /// `run-work-20260725T183920.469500000-p45813-000000`, with its
    /// timestamps and pids parameterized so tests stay deterministic.
    ///
    /// This is deliberately a faithful copy rather than a convenient
    /// simplification: `approved_profiles` is the `{"profiles": [...]}`
    /// envelope a real manifest carries, not a bare array, and `details`
    /// carries the real work-state keys. A fixture that "cleans up" these
    /// shapes lets a mirror that cannot parse a real manifest pass its
    /// tests.
    #[allow(clippy::unused_self)]
    pub(crate) fn patchstand_manifest(
        &self,
        run_id: &str,
        created_at: &str,
        updated_at: &str,
        owner_pid: u32,
        worker_pgid: u32,
    ) -> serde_json::Value {
        serde_json::json!({
            "schema": "undertake/run@2",
            "run_id": run_id,
            "job": "work",
            "target": {"repo": "/repo/patchstand", "bead": "patchstand-thk"},
            "details": {
                "job": "work",
                "state": {
                    "cycle_id": "cycle-20260725-183823",
                    "authorization_sha256": "91".repeat(32),
                    "before_head": "93eaa3b43471e94a4f7956ead644b874f3e173e9",
                    "owner_pid": owner_pid,
                    "worker_pgid": worker_pgid,
                    "stage": "implementing",
                },
            },
            "created_at": created_at,
            "updated_at": updated_at,
            "approved_profiles": {"profiles": [PATCHSTAND_PROFILE_ID]},
            "musterroll_roster_artifact": {
                "path": "/repo/musterroll/roster.toml",
                "sha256": "52".repeat(32),
            },
            "roster_snapshot": {
                "path": "roster.json",
                "size_bytes": 23_377,
                "sha256": "e7".repeat(32),
            },
            "roster_policy_sha256": "2c".repeat(32),
            "limits": {"item_wall_clock_mins": 45, "max_attempts": 1},
            "verifier": {
                "mechanical": "pnpm check",
                "qualitative": "tiered-qualitative-review:min_tier_gap=1",
            },
            "artifacts": [
                {"path": "approval.json", "sha256": "45".repeat(32)},
                {"path": "roster.json", "sha256": "e7".repeat(32)},
                {"path": PATCHSTAND_WORKER_STDOUT, "sha256": "57".repeat(32)},
                {"path": "artifacts/verify/stdout.log", "sha256": "a9".repeat(32)},
            ],
            "lifecycle": "running",
        })
    }

    /// The run-local `musterroll/roster@2` snapshot the pilot run copied,
    /// reduced to the one provider and one profile the attempt resolves
    /// through. Field-for-field the real shape: `parse_roster_snapshot` uses
    /// `deny_unknown_fields` and then a strict fail-closed validation pass,
    /// so an approximation would silently fail to resolve any profile.
    #[allow(clippy::unused_self)]
    pub(crate) fn patchstand_roster(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": "musterroll/roster@2",
            "generated_at": "2026-07-25T18:39:17.350083000Z",
            "source_artifact": {
                "path": "/repo/musterroll/roster.toml",
                "sha256": "52".repeat(32),
            },
            "policy_sha256": "2c".repeat(32),
            "providers": [{
                "provider_id": "openai-codex",
                "availability_key": "openai-codex",
                "enabled": true,
                "state": "healthy",
                "availability": "healthy",
                "checked_at": "2026-07-25T18:39:17.095186000Z",
                "data_as_of": null,
                "expires_at": null,
                "reason": null,
                "eligible": true,
                "ineligibility_reason": null,
            }],
            "profiles": [{
                "profile_id": PATCHSTAND_PROFILE_ID,
                "provider_id": "openai-codex",
                "model": "gpt-5.6-luna",
                "harness": "codex",
                "dispatch_id": "gpt-5.6-luna",
                "reasoning_effort": "high",
                "tier": "senior",
                "ceiling": "L",
                "efficiency": "std",
                "cost": 1.0,
                "data_policy": "standard",
                "enabled": true,
                "roles": ["advisor", "default", "task"],
                "state": "healthy",
                "eligible": true,
                "ineligibility_reason": null,
            }],
        })
    }

    /// The pilot run's five `undertake/event@2` lines, verbatim in shape:
    /// `run_started`, the `running:<attempt-dir>` `attempt_started`, a
    /// `success` `attempt_finished`, the `failed` `verify_finished`, and
    /// the `coverage_gap` marker.
    #[allow(clippy::unused_self)]
    pub(crate) fn patchstand_events(&self, run_id: &str) -> String {
        let target = serde_json::json!({"repo": "/repo/patchstand", "bead": "patchstand-thk"});
        let lines = [
            (
                1_u64,
                "2026-07-25T18:39:20.469500+00:00",
                "run_started",
                None,
                Some("started"),
            ),
            (
                2,
                "2026-07-25T18:39:20.888354+00:00",
                "attempt_started",
                Some(PATCHSTAND_PROFILE_ID),
                Some(PATCHSTAND_ATTEMPT_OUTCOME),
            ),
            (
                3,
                "2026-07-25T18:43:40.728519+00:00",
                "attempt_finished",
                Some(PATCHSTAND_PROFILE_ID),
                Some("success"),
            ),
            (
                4,
                "2026-07-25T18:43:44.616574+00:00",
                "verify_finished",
                None,
                Some("failed"),
            ),
            (
                5,
                "2026-07-25T18:43:44.617226+00:00",
                "coverage_gap",
                None,
                Some("qualitative_review_not_run"),
            ),
        ];
        let mut out = String::new();
        for (seq, ts, kind, profile_id, outcome) in lines {
            let event = serde_json::json!({
                "schema": "undertake/event@2",
                "event_id": format!("{run_id}-{seq:06}"),
                "run_id": run_id,
                "seq": seq,
                "ts": ts,
                "kind": kind,
                "job": "work",
                "profile_id": profile_id,
                "target": target,
                "artifact_refs": [],
                "outcome": outcome,
            });
            out.push_str(&serde_json::to_string(&event).expect("serialize event"));
            out.push('\n');
        }
        out
    }

    /// Materializes the whole pilot run directory — manifest, run-local
    /// roster snapshot, event log, heartbeat, and the two fixed-allowlist
    /// log paths — and returns its path.
    pub(crate) fn write_patchstand_run(
        &self,
        run_id: &str,
        created_at: &str,
        updated_at: &str,
        heartbeat: &str,
        owner_pid: u32,
        worker_pgid: u32,
    ) -> PathBuf {
        let manifest =
            self.patchstand_manifest(run_id, created_at, updated_at, owner_pid, worker_pgid);
        let run_dir = self.write_run(run_id, &manifest);
        fs::write(
            run_dir.join("roster.json"),
            serde_json::to_vec_pretty(&self.patchstand_roster()).expect("serialize roster"),
        )
        .expect("write roster");
        fs::write(run_dir.join("events.jsonl"), self.patchstand_events(run_id))
            .expect("write events");
        fs::write(run_dir.join("heartbeat"), heartbeat).expect("write heartbeat");
        let attempt_dir = run_dir.join("attempts").join(PATCHSTAND_ATTEMPT_DIR);
        fs::create_dir_all(&attempt_dir).expect("mkdir attempt");
        fs::write(attempt_dir.join("worker.stdout.log"), "worker stdout\n")
            .expect("write worker stdout");
        let verify_dir = run_dir.join("artifacts").join("verify");
        fs::create_dir_all(&verify_dir).expect("mkdir verify");
        fs::write(verify_dir.join("stdout.log"), "pnpm check failed\n")
            .expect("write verify stdout");
        run_dir
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
