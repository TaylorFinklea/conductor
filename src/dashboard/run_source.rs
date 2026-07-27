//! Bounded, forward-compatible run-source readers.
//!
//! [`DashboardRunSource`] reads only from a configured Undertake state root
//! and produces immutable [`RunSnapshot`]s for the renderer. It never writes
//! run, service, or repository state and never receives a mutable
//! [`crate::run::RunHandle`]. All file reads are bounded: manifests at 128 KiB,
//! event tails at 8 MiB / 5,000 events, log tails at 64 KiB. Paths derived from
//! model output or event artifact paths are display-only and never opened;
//! only the fixed allowlist of relative log paths is opened after
//! canonicalization and containment confirmation.
//!
//! The manifest and event mirrors here are dashboard-local and
//! forward-compatible: unknown fields are tolerated, but an unknown schema
//! fails the source closed (retaining the last valid snapshot generation).
//! The strict operational readers in [`crate::run`] (which use
//! `deny_unknown_fields`) are never called for live tailing.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use super::STALE_HEARTBEAT_THRESHOLD;
use crate::dashboard::model::{
    AttemptRecord, DashboardSnapshot, HarnessDeckState, LogTail, RecentRun, RunIdentity,
    RunLiveness, RunSnapshot, SourceState, StageMarker, VerificationRecord, VerificationSource,
};
use crate::dashboard::services::{
    AfterfactSnapshot, CautionlightDashboardSource, CautionlightSnapshot, MusterrollSnapshot,
};
use crate::musterroll::{self, RosterSnapshot};
use crate::run::{RunJob, RunLifecycle};
use crate::sanitize::sanitize_text;

/// Bounded read configuration for the run source. The state root is the
/// configured Undertake state directory (containing `runs-v2/`); the report
/// root is the Harness Deck reports home the run/report join resolves
/// through; the refresh interval governs local artifact polling only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunSourceConfig {
    pub(crate) state_root: PathBuf,
    /// The Harness Deck reports home (`UNDERTAKE_REPORTS_HOME`, else
    /// `$HOME`), resolved by the CLI through the same helper every other
    /// report-writing command uses. Only ever joined through
    /// [`crate::deck::report_run_dir`], never by hand.
    pub(crate) reports_home: PathBuf,
    pub(crate) refresh_interval: Duration,
}

impl RunSourceConfig {
    /// Returns `<state_root>/runs-v2`, the sole active run namespace.
    pub(crate) fn runs_dir(&self) -> PathBuf {
        self.state_root.join("runs-v2")
    }
}

/// How a run is selected for display. `Newest` selects the newest
/// nonterminal run (even when abandoned), falling back to the newest terminal
/// run; `Explicit` selects a validated run id and fails closed if it is
/// unknown or malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunSelection {
    Newest,
    Explicit(String),
}

/// Selects exactly one fixed-allowlist log to open (see [`LOG_ALLOWLIST`]).
/// The attempt-directory component for worker logs is the same opaque
/// directory-name string joined by [`reduce_worker_attempts`] from an
/// `attempt_started` outcome; it still passes single-normal-component
/// validation and canonicalized containment checking before any file is
/// opened — a malformed or attacker-influenced value is never trusted
/// merely because it looks like a legitimate attempt id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LogSelector {
    WorkerStdout(String),
    WorkerStderr(String),
    VerifyStdout,
    VerifyStderr,
}

/// The immutable snapshot the renderer consumes, produced by
/// [`DashboardRunSource::snapshot`]. Task 1 populates the run and recent-run
/// source states; Task 2 adds service sources.
#[derive(Debug, Clone)]
pub(crate) struct DashboardRunSource {
    config: RunSourceConfig,
    /// Per-run incremental event-tail state, keyed by run id. The dashboard
    /// never calls the strict operational `read_events` for live tailing.
    tails: std::cell::RefCell<std::collections::HashMap<String, EventTailState>>,
}

/// Error returned by run-source reads. A source error retains the last valid
/// snapshot generation and marks it stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DashboardError {
    message: String,
}

impl DashboardError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for DashboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DashboardError {}

/// The maximum number of run directories scanned for discovery (the 200-candidate cap).
const DISCOVERY_CANDIDATE_CAP: usize = 200;
/// The maximum manifest size read during discovery (128 KiB), matching
/// [`crate::run`] discovery.
const DISCOVERY_MANIFEST_MAX_BYTES: u64 = 128 * 1024;
/// The maximum bytes read from `events.jsonl` per incremental tail (8 MiB).
const EVENT_TAIL_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// The maximum number of events retained per run (5,000).
const EVENT_TAIL_MAX_EVENTS: usize = 5_000;
/// The maximum log tail size (64 KiB).
const LOG_TAIL_MAX_BYTES: u64 = 64 * 1024;

/// The fixed allowlist of relative log paths that may be opened after
/// canonicalization and containment confirmation. No other path derived from
/// model output, an artifact path string, an opaque profile id, or an
/// event-reported cwd is ever opened.
const LOG_ALLOWLIST: &[&str] = &[
    "attempts/*/worker.stdout.log",
    "attempts/*/worker.stderr.log",
    "artifacts/verify/stdout.log",
    "artifacts/verify/stderr.log",
];

impl DashboardRunSource {
    pub(crate) fn new(config: RunSourceConfig) -> Self {
        Self {
            config,
            tails: std::cell::RefCell::default(),
        }
    }

    /// Returns the source's configuration.
    pub(crate) fn config(&self) -> &RunSourceConfig {
        &self.config
    }

    /// Selects a run per [`RunSelection`] and reads its bounded artifacts.
    /// Never writes; never opens a path derived from model output.
    pub(crate) fn select(&self, selection: &RunSelection) -> Result<RunSnapshot, DashboardError> {
        let run_id = self.resolve_run_id(selection)?;
        self.snapshot_for_run(&run_id, Utc::now())
    }

    /// Resolves the run id for a selection, scanning the runs directory only
    /// when the selection actually needs discovery.
    fn resolve_run_id(&self, selection: &RunSelection) -> Result<String, DashboardError> {
        match selection {
            RunSelection::Explicit(id) => self.validated_explicit_run_id(id),
            RunSelection::Newest => newest_run_id(&self.scan_candidates()?.candidates),
        }
    }

    /// Same resolution against an already-scanned candidate set, so a refresh
    /// tick that also builds the recent-runs panel scans the runs directory
    /// exactly once.
    fn resolve_run_id_with(
        &self,
        candidates: &[DiscoveryCandidate],
        selection: &RunSelection,
    ) -> Result<String, DashboardError> {
        match selection {
            RunSelection::Explicit(id) => self.validated_explicit_run_id(id),
            RunSelection::Newest => newest_run_id(candidates),
        }
    }

    /// Validates an explicit run id and confirms the run directory exists.
    ///
    /// Deliberately independent of [`DISCOVERY_CANDIDATE_CAP`]: the cap
    /// bounds how much work *discovery* does, and pinning the dashboard to a
    /// named run must keep working for a run older than the newest 200.
    ///
    /// Shares its whole body with [`preflight_run_selection`], which the CLI
    /// calls before entering raw mode: `--run <unknown>` must exit 2 from a
    /// plain terminal, and it must fail for exactly the reasons a refresh
    /// tick would fail — one implementation, not two that can drift.
    fn validated_explicit_run_id(&self, run_id: &str) -> Result<String, DashboardError> {
        validated_explicit_run_id(&self.config, run_id)
    }

    /// Reads the recent terminal runs (bounded) for the secondary panel.
    pub(crate) fn recent_runs(&self) -> Result<Vec<RecentRun>, DashboardError> {
        Ok(recent_from_candidates(&self.scan_candidates()?.candidates))
    }

    /// Builds the full dashboard snapshot: the selected run and recent runs.
    /// Each source keeps its own [`SourceState`], so one failing source never
    /// blanks the other.
    ///
    /// `previous` is the last snapshot this source produced, if any. A failed
    /// read degrades that source's prior state — retaining a prior value as
    /// [`SourceState::Stale`] with its real `last_ok`, or staying
    /// [`SourceState::Absent`] when nothing has ever succeeded. A source that
    /// has never produced a value is never presented with a fabricated
    /// success timestamp.
    ///
    /// The runs directory is scanned exactly once per call: selection and the
    /// recent-runs panel share one bounded pass rather than re-reading up to
    /// [`DISCOVERY_CANDIDATE_CAP`] manifests twice per refresh tick.
    pub(crate) fn snapshot(
        &self,
        previous: Option<&DashboardSnapshot>,
        selection: &RunSelection,
        now: DateTime<Utc>,
    ) -> DashboardSnapshot {
        match self.scan_candidates() {
            Ok(scan) => self.snapshot_from_scan(previous, selection, now, &scan),
            Err(error) => {
                // Only the runs directory itself being unopenable gets here;
                // an unreadable entry within it is a bounded warning, not a
                // dead source.
                let message = error.message().to_string();
                let (musterroll, afterfact, cautionlight) = carried_services(previous);
                DashboardSnapshot {
                    run: previous
                        .map_or_else(SourceState::never_read, |previous| previous.run.clone())
                        .degraded(now, message.clone()),
                    recent: previous
                        .map_or_else(SourceState::never_read, |previous| previous.recent.clone())
                        .degraded(now, message),
                    discovery_warning: None,
                    musterroll,
                    afterfact,
                    cautionlight,
                }
            }
        }
    }

    /// Builds the snapshot from one already-completed discovery pass.
    ///
    /// Split from [`Self::snapshot`] so the skip-and-warn policy can be
    /// exercised end to end against a synthesized unreadable directory entry
    /// (see [`Self::scan_entries`]).
    fn snapshot_from_scan(
        &self,
        previous: Option<&DashboardSnapshot>,
        selection: &RunSelection,
        now: DateTime<Utc>,
        scan: &DiscoveryScan,
    ) -> DashboardSnapshot {
        let recent = SourceState::Fresh {
            value: recent_from_candidates(&scan.candidates),
            last_ok: now,
            last_attempt: now,
            truncated: scan.candidates.len() >= DISCOVERY_CANDIDATE_CAP,
        };
        let run = match self
            .resolve_run_id_with(&scan.candidates, selection)
            .and_then(|run_id| self.snapshot_for_run(&run_id, now))
        {
            Ok(snapshot) => SourceState::Fresh {
                truncated: snapshot.events_truncated,
                value: snapshot,
                last_ok: now,
                last_attempt: now,
            },
            Err(error) => previous
                .map_or_else(SourceState::never_read, |previous| previous.run.clone())
                .degraded(now, error.message().to_string()),
        };
        let (musterroll, afterfact, cautionlight) = carried_services(previous);
        DashboardSnapshot {
            run,
            recent,
            discovery_warning: scan.warnings.message(),
            musterroll,
            afterfact,
            cautionlight,
        }
    }
}

/// Carries the three service source states across a run-source tick.
///
/// The run source never samples a service — Task 3's runtime owns their
/// cadence — so a tick must neither reset them nor deep-copy their retained
/// evidence. Sharing keeps a refresh O(1) in the number of retained Afterfact
/// events. Cautionlight starts [`SourceState::Deferred`] rather than absent:
/// v1 deliberately never runs it, which is a different fact from never having
/// managed to.
fn carried_services(previous: Option<&DashboardSnapshot>) -> CarriedServices {
    match previous {
        Some(previous) => (
            Arc::clone(&previous.musterroll),
            Arc::clone(&previous.afterfact),
            Arc::clone(&previous.cautionlight),
        ),
        None => (
            Arc::new(SourceState::never_read()),
            Arc::new(SourceState::never_read()),
            Arc::new(CautionlightDashboardSource::default_state()),
        ),
    }
}

type CarriedServices = (
    Arc<SourceState<MusterrollSnapshot>>,
    Arc<SourceState<AfterfactSnapshot>>,
    Arc<SourceState<CautionlightSnapshot>>,
);

// ---------------------------------------------------------------------------
// Forward-compatible manifest mirror
// ---------------------------------------------------------------------------

/// Dashboard-local forward-compatible mirror of `undertake/run@2`. Unlike
/// the strict operational [`crate::run::RunManifest`], this intentionally
/// omits `deny_unknown_fields`: unknown extra fields are tolerated so a
/// future-compatible manifest still renders. An unknown *schema* still fails
/// the source closed.
///
/// It types *only* the fields the dashboard displays. Every additional typed
/// field is pure parse-failure surface in a read-only mirror: it can reject a
/// real manifest but can never render anything. `approved_profiles`,
/// `artifacts`, and `limits` are deliberately absent for that reason —
/// mirroring `approved_profiles` as a bare `Vec<String>` (it is really the
/// `{"profiles": [...]}` envelope) made every real manifest fail with
/// `invalid type: map, expected a sequence` while synthetic array-shaped
/// fixtures passed. Add a field here only together with the model field and
/// the rendering that consumes it.
#[derive(Debug, Clone, serde::Deserialize)]
struct DashboardManifest {
    schema: String,
    run_id: String,
    job: DashboardJob,
    target: DashboardTarget,
    details: serde_json::Value,
    created_at: String,
    updated_at: String,
    musterroll_roster_artifact: Option<DashboardArtifactRef>,
    roster_snapshot: Option<DashboardRosterSnapshot>,
    roster_policy_sha256: Option<String>,
    /// Required, never defaulted: lifecycle drives the terminal/nonterminal
    /// split that the whole liveness distinction rests on. A manifest missing
    /// it must surface as an unreadable run, not silently render as
    /// `running`.
    lifecycle: DashboardLifecycle,
    outcome: Option<String>,
    #[serde(default)]
    verifier: DashboardVerifier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum DashboardJob {
    Work,
    Review,
    Consult,
    Plan,
}

impl From<DashboardJob> for RunJob {
    fn from(job: DashboardJob) -> Self {
        match job {
            DashboardJob::Work => RunJob::Work,
            DashboardJob::Review => RunJob::Review,
            DashboardJob::Consult => RunJob::Consult,
            DashboardJob::Plan => RunJob::Plan,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
struct DashboardTarget {
    repo: String,
    #[serde(default)]
    bead: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum DashboardLifecycle {
    Started,
    Running,
    Finished,
}

impl From<DashboardLifecycle> for RunLifecycle {
    fn from(lifecycle: DashboardLifecycle) -> Self {
        match lifecycle {
            DashboardLifecycle::Started => RunLifecycle::Started,
            DashboardLifecycle::Running => RunLifecycle::Running,
            DashboardLifecycle::Finished => RunLifecycle::Finished,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct DashboardArtifactRef {
    path: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct DashboardRosterSnapshot {
    path: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
struct DashboardVerifier {
    mechanical: Option<String>,
    qualitative: Option<String>,
}

/// Dashboard-local forward-compatible mirror of `undertake/event@2`. Unlike
/// the strict operational [`crate::run::RunEvent`], this omits
/// `deny_unknown_fields`: unknown extra fields are tolerated. An unknown
/// *schema* fails the source closed.
#[derive(Debug, Clone, serde::Deserialize)]
struct DashboardEvent {
    schema: String,
    event_id: String,
    run_id: String,
    seq: u64,
    ts: String,
    kind: String,
    job: DashboardJob,
    #[serde(default)]
    target: DashboardTarget,
    profile_id: Option<String>,
    #[serde(default)]
    artifact_refs: Vec<DashboardArtifactRef>,
    outcome: Option<String>,
    #[serde(default)]
    plan_invocation: Option<DashboardPlanInvocation>,
}

/// Dashboard-local forward-compatible mirror of `PlanInvocationEvidence`.
/// Typed source data for plan stage markers; a marker event such as
/// `planner_authoring` carries no `plan_invocation` and is therefore never
/// mistaken for a worker attempt.
#[derive(Debug, Clone, serde::Deserialize)]
struct DashboardPlanInvocation {
    role: String,
    stage: String,
    execution: DashboardApprovedExecution,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    attempt: Option<u8>,
}

/// Dashboard-local forward-compatible mirror of `ApprovedExecution`. Only the
/// identity fields needed for roster resolution are typed here.
#[derive(Debug, Clone, serde::Deserialize)]
struct DashboardApprovedExecution {
    profile_id: String,
    provider_id: String,
}

/// Per-run incremental event-tail state: the byte offset of the last
/// successfully consumed newline, the next expected sequence number, and the
/// retained parsed events (bounded at [`EVENT_TAIL_MAX_EVENTS`]). A trailing
/// partial line is an ordinary concurrent append: the offset is not advanced
/// and no error is raised; the next tick retries it.
#[derive(Debug, Clone)]
struct EventTailState {
    offset: u64,
    next_seq: u64,
    events: Vec<DashboardEvent>,
    truncated: bool,
    /// The most recent tick's source error (complete malformed line,
    /// sequence gap, or unknown schema), if any. Cleared once the file
    /// content driving the error has moved on (i.e. before each tick's own
    /// attempt, so a persisting bad line keeps surfacing the same error
    /// rather than silently clearing it).
    error: Option<String>,
}

impl Default for EventTailState {
    fn default() -> Self {
        // The first event in a well-formed log is always seq 1.
        Self {
            offset: 0,
            next_seq: 1,
            events: Vec::new(),
            truncated: false,
            error: None,
        }
    }
}

/// The expected schema tag for dashboard events (matches the operational
/// `undertake/event@2`).
const EXPECTED_EVENT_SCHEMA: &str = "undertake/event@2";
/// A discovery candidate: the parsed manifest (or a parse/schema error) plus
/// the directory name, its filesystem mtime, and the parsed `created_at`
/// for ordering. `created_at` orders *valid* candidates against each other
/// (the spec's "greatest parsed RFC3339 `created_at`"); `mtime` — from the
/// same scan pass that caps candidates by recency — is the only recency
/// signal a malformed candidate has, and is also what decides a
/// malformed-vs-valid tie in [`newest_run_id`], so that decision never
/// mixes a manifest-declared timestamp with a filesystem one.
#[derive(Debug, Clone)]
struct DiscoveryCandidate {
    run_id: String,
    dir_name: String,
    mtime: SystemTime,
    created_at: Option<DateTime<Utc>>,
    manifest: Result<DashboardManifest, DashboardError>,
}

/// Bounded accounting of run directory entries discovery could not read
/// cleanly. One unreadable entry must not blank every panel, and it must not
/// vanish silently either: the entry is skipped (or, for an unreadable mtime,
/// kept with degraded cap ordering) and counted here.
///
/// Bounded by construction — a count and the first error, never a per-entry
/// list. A warning that grew with the failure count would put an unbounded
/// amount of untrusted directory content on the render path, the same failure
/// this module's byte caps exist to prevent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DiscoveryWarnings {
    count: usize,
    first: Option<String>,
}

impl DiscoveryWarnings {
    fn record(&mut self, error: String) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some(error);
        }
    }

    /// The single display message, or `None` when discovery was clean.
    fn message(&self) -> Option<String> {
        let first = self.first.as_deref()?;
        Some(if self.count == 1 {
            format!("discovery could not fully read 1 run directory entry: {first}")
        } else {
            format!(
                "discovery could not fully read {} run directory entries; first: {first}",
                self.count
            )
        })
    }
}

/// One completed discovery pass: the bounded candidate set plus whatever
/// could not be read while producing it.
#[derive(Debug, Clone, Default)]
struct DiscoveryScan {
    candidates: Vec<DiscoveryCandidate>,
    warnings: DiscoveryWarnings,
}

/// The expected schema tag for dashboard manifests (matches the operational
/// `undertake/run@2`).
const EXPECTED_MANIFEST_SCHEMA: &str = "undertake/run@2";

/// Reads a bounded manifest (≤ [`DISCOVERY_MANIFEST_MAX_BYTES`]) and parses it
/// forward-compatibly. Unknown fields are tolerated; an unknown schema fails
/// closed. Returns the parsed manifest and whether truncation occurred.
fn read_dashboard_manifest(path: &Path) -> Result<(DashboardManifest, bool), DashboardError> {
    let file = std::fs::File::open(path).map_err(|error| {
        DashboardError::new(format!(
            "failed to read manifest {}: {error}",
            path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    file.take(DISCOVERY_MANIFEST_MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            DashboardError::new(format!(
                "failed to read manifest {}: {error}",
                path.display()
            ))
        })?;
    let truncated = bytes.len() as u64 >= DISCOVERY_MANIFEST_MAX_BYTES
        && std::fs::metadata(path).is_ok_and(|m| m.len() > DISCOVERY_MANIFEST_MAX_BYTES);
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        DashboardError::new(format!(
            "failed to parse manifest {}: {error}",
            path.display()
        ))
    })?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<missing>");
    if schema != EXPECTED_MANIFEST_SCHEMA {
        return Err(DashboardError::new(format!(
            "unknown schema {schema:?} in {}, expected {EXPECTED_MANIFEST_SCHEMA:?}",
            path.display()
        )));
    }
    let manifest: DashboardManifest = serde_json::from_value(value).map_err(|error| {
        DashboardError::new(format!(
            "failed to parse manifest {}: {error}",
            path.display()
        ))
    })?;
    validate_run_id(&manifest.run_id)?;
    Ok((manifest, truncated))
}

/// Validates a run id is a single normal path component, reusing the
/// operational convention. Rejects traversal and multi-component ids. The
/// CLI calls this directly for `--run` rather than reimplementing it.
pub(crate) fn validate_run_id(run_id: &str) -> Result<(), DashboardError> {
    use std::path::Component;
    let mut components = Path::new(run_id).components();
    if run_id.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(DashboardError::new(format!("invalid run id {run_id:?}")));
    }
    Ok(())
}

/// Validates an explicit run id against a configured state root and confirms
/// the run directory exists. The single implementation behind both
/// [`DashboardRunSource::validated_explicit_run_id`] and
/// [`preflight_run_selection`].
fn validated_explicit_run_id(
    config: &RunSourceConfig,
    run_id: &str,
) -> Result<String, DashboardError> {
    validate_run_id(run_id)?;
    if config.runs_dir().join(run_id).is_dir() {
        Ok(run_id.to_string())
    } else {
        Err(DashboardError::new(format!("unknown run id {run_id:?}")))
    }
}

/// Checks a selection is launchable *before* the dashboard enters raw mode,
/// so a malformed or unknown `--run` id reports on the plain terminal and
/// exits 2 instead of dropping the operator into an alternate screen whose
/// only content is an error.
///
/// [`RunSelection::Newest`] needs no preflight: an empty runs directory is a
/// legitimate state the dashboard renders, not a launch failure.
pub(crate) fn preflight_run_selection(
    config: &RunSourceConfig,
    selection: &RunSelection,
) -> Result<(), DashboardError> {
    match selection {
        RunSelection::Newest => Ok(()),
        RunSelection::Explicit(run_id) => validated_explicit_run_id(config, run_id).map(|_| ()),
    }
}

/// Picks the newest run from one already-scanned candidate set.
///
/// Valid candidates are ranked against each other by parsed `created_at`
/// (see [`newest_valid_candidate`]) — the spec's "greatest parsed RFC3339
/// `created_at`". Malformed candidates have no `created_at` to rank by, so
/// they are ranked — against each other, and against the chosen valid
/// candidate — by the scan's own directory mtime instead (see
/// [`newest_malformed_candidate`]). The final malformed-vs-valid decision
/// always compares mtime to mtime, never a manifest-declared timestamp to a
/// filesystem one: that keeps a genuinely newest malformed candidate
/// visible (with its error), while an *older* malformed run — one whose
/// directory was touched before a valid nonterminal run's — loses to that
/// valid run instead of silently hiding it. The pre-fix code compared
/// `created_at` directly across every candidate: a malformed candidate's
/// unconditional `None` sorted as "greatest" against every real timestamp,
/// so any malformed manifest beat every valid run regardless of actual
/// recency.
fn newest_run_id(candidates: &[DiscoveryCandidate]) -> Result<String, DashboardError> {
    let valid = newest_valid_candidate(candidates);
    let malformed = newest_malformed_candidate(candidates);
    let chosen = match (valid, malformed) {
        (None, None) => None,
        (Some(candidate), None) | (None, Some(candidate)) => Some(candidate),
        (Some(valid), Some(malformed)) => Some(if malformed.mtime > valid.mtime {
            malformed
        } else {
            valid
        }),
    };
    let Some(candidate) = chosen else {
        return Err(DashboardError::new("no runs found"));
    };
    Ok(candidate.run_id.clone())
}

/// The newest *valid* (parseable) candidate: the newest nonterminal run by
/// `created_at`, tie-broken by directory name descending, falling back to
/// the newest terminal run when every valid run has finished. `None` when
/// no candidate parsed. Malformed candidates never enter this comparison —
/// see [`newest_malformed_candidate`].
fn newest_valid_candidate(candidates: &[DiscoveryCandidate]) -> Option<&DiscoveryCandidate> {
    let mut ordered: Vec<&DiscoveryCandidate> = candidates
        .iter()
        .filter(|candidate| candidate.manifest.is_ok())
        .collect();
    ordered.sort_by(|a, b| {
        compare_created_at(a.created_at, b.created_at).then_with(|| a.dir_name.cmp(&b.dir_name))
    });
    // Prefer the newest nonterminal run even when its liveness is
    // abandoned; falling back to the newest candidate covers the
    // all-finished case. Every candidate here already parsed (filtered
    // above), so the `is_ok_and` below is purely a lifecycle test, never a
    // fallback for an unparseable manifest — those rank separately.
    ordered
        .iter()
        .rev()
        .find(|candidate| {
            candidate
                .manifest
                .as_ref()
                .is_ok_and(|manifest| manifest.lifecycle != DashboardLifecycle::Finished)
        })
        .or_else(|| ordered.last())
        .copied()
}

/// The newest *malformed* (unparseable) candidate by the discovery scan's
/// own directory mtime — the only recency signal a manifest that failed to
/// parse has — tie-broken by directory name descending. `None` when every
/// candidate parsed.
fn newest_malformed_candidate(candidates: &[DiscoveryCandidate]) -> Option<&DiscoveryCandidate> {
    candidates
        .iter()
        .filter(|candidate| candidate.manifest.is_err())
        .max_by(|a, b| {
            a.mtime
                .cmp(&b.mtime)
                .then_with(|| a.dir_name.cmp(&b.dir_name))
        })
}

/// Reduces scanned candidates to the terminal runs shown in the secondary
/// "Recent runs" panel, newest first (tie-broken by run id descending).
fn recent_from_candidates(candidates: &[DiscoveryCandidate]) -> Vec<RecentRun> {
    let mut recent: Vec<RecentRun> = candidates
        .iter()
        .filter_map(|candidate| {
            let manifest = candidate.manifest.as_ref().ok()?;
            if manifest.lifecycle != DashboardLifecycle::Finished {
                return None;
            }
            Some(RecentRun {
                run_id: manifest.run_id.clone(),
                job: manifest.job.into(),
                lifecycle: manifest.lifecycle.into(),
                liveness: RunLiveness::Finished,
                target_repo: manifest.target.repo.clone(),
                target_bead: manifest.target.bead.clone(),
                created_at: candidate.created_at,
                created_at_text: manifest.created_at.clone(),
                outcome: manifest.outcome.clone(),
            })
        })
        .collect();
    recent.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.run_id.cmp(&a.run_id))
    });
    recent
}

impl DashboardRunSource {
    /// Scans the runs directory, bounded to the most recently modified
    /// [`DISCOVERY_CANDIDATE_CAP`] candidate manifests. Each manifest is read
    /// at most [`DISCOVERY_MANIFEST_MAX_BYTES`]. Malformed manifests become
    /// candidates with an error (so a malformed newest run is still visible).
    ///
    /// Only failing to open the runs directory itself fails the source; an
    /// unreadable entry inside it is skipped and counted.
    fn scan_candidates(&self) -> Result<DiscoveryScan, DashboardError> {
        let root = self.config.runs_dir();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DiscoveryScan::default());
            }
            Err(error) => {
                return Err(DashboardError::new(format!(
                    "failed to read runs dir {}: {error}",
                    root.display()
                )));
            }
        };
        Ok(Self::scan_entries(entries))
    }

    /// Reduces one directory listing to bounded discovery candidates,
    /// skipping and counting entries that cannot be read.
    ///
    /// Split from [`Self::scan_candidates`] because a per-entry
    /// `readdir`/`stat` failure cannot be provoked at a *single* entry through
    /// Unix permissions — search permission is a property of the parent
    /// directory, so removing it fails every entry and `read_dir` itself. The
    /// only way to test "one bad entry, the rest still render" is to feed a
    /// real listing plus one synthesized failing entry.
    fn scan_entries<I>(entries: I) -> DiscoveryScan
    where
        I: IntoIterator<Item = std::io::Result<std::fs::DirEntry>>,
    {
        let mut warnings = DiscoveryWarnings::default();
        // (mtime, dir_name, path) so we can sort by most recently modified
        // and take the top DISCOVERY_CANDIDATE_CAP.
        let mut dir_entries: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    // The iterator failed before yielding a name or a path,
                    // so the error is all there is to report.
                    warnings.record(format!("failed to read run directory entry: {error}"));
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    warnings.record(format!(
                        "failed to stat run entry {}: {error}",
                        entry.path().display()
                    ));
                    continue;
                }
            };
            if !file_type.is_dir() {
                continue;
            }
            let path = entry.path().join("manifest.json");
            if !path.is_file() {
                continue;
            }
            // An unreadable mtime degrades ordering only: the candidate
            // *cap* (below), and — for a candidate whose manifest turns out
            // to be malformed — its rank in `newest_malformed_candidate`,
            // which has no `created_at` to use instead. A valid candidate is
            // still ordered by its manifest's `created_at`. Sorting oldest is
            // the safe degradation in both, so the entry is kept with the
            // degradation recorded rather than dropped over a fact discovery
            // can survive without.
            let mtime = match entry.metadata().and_then(|metadata| metadata.modified()) {
                Ok(mtime) => mtime,
                Err(error) => {
                    warnings.record(format!(
                        "failed to read mtime of run entry {}: {error}",
                        entry.path().display()
                    ));
                    std::time::UNIX_EPOCH
                }
            };
            let dir_name = entry.file_name().to_string_lossy().to_string();
            dir_entries.push((mtime, dir_name, path));
        }
        // Most recently modified first.
        dir_entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        dir_entries.truncate(DISCOVERY_CANDIDATE_CAP);

        let mut candidates = Vec::with_capacity(dir_entries.len());
        for (mtime, dir_name, manifest_path) in dir_entries {
            let manifest_result = read_dashboard_manifest(&manifest_path);
            let (manifest, created_at) = match manifest_result {
                Ok((m, _truncated)) => {
                    let created_at = parse_rfc3339(&m.created_at);
                    (Ok(m), created_at)
                }
                Err(error) => (Err(error), None),
            };
            // Derive run_id from dir name for malformed manifests so we can
            // still select and display them.
            let run_id = manifest
                .as_ref()
                .map_or_else(|_| dir_name.clone(), |m| m.run_id.clone());
            candidates.push(DiscoveryCandidate {
                run_id,
                dir_name,
                mtime,
                created_at,
                manifest,
            });
        }
        DiscoveryScan {
            candidates,
            warnings,
        }
    }

    /// Reads the full snapshot for a run id. The manifest is re-read here (the
    /// discovery pass only keeps enough to order candidates). A malformed
    /// manifest produces a snapshot with a `selection_error` rather than a
    /// hard failure, so a malformed newest run is visible.
    fn snapshot_for_run(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
    ) -> Result<RunSnapshot, DashboardError> {
        validate_run_id(run_id)?;
        let run_dir = self.config.runs_dir().join(run_id);
        let manifest_path = run_dir.join("manifest.json");
        let manifest_result = read_dashboard_manifest(&manifest_path);
        let snapshot = match manifest_result {
            Ok((manifest, _truncated)) => self.build_snapshot(run_id, &run_dir, &manifest, now),
            Err(error) => RunSnapshot {
                identity: RunIdentity::unknown(run_id),
                attempts: Vec::new(),
                stage_markers: Vec::new(),
                verification: VerificationRecord {
                    passed: None,
                    source: VerificationSource::NotRun,
                    command: None,
                    event_outcome: None,
                    disagreement: false,
                },
                logs: Vec::new(),
                // An unreadable manifest carries no join key, so the join
                // is genuinely unattemptable — not "absent report".
                harness_deck: HarnessDeckState::Unresolved {
                    reason: "run manifest unreadable".to_string(),
                },
                event_count: 0,
                events_truncated: false,
                selection_error: Some(error.message().to_string()),
                events_error: None,
                roster_error: None,
            },
        };
        Ok(snapshot)
    }

    /// Builds a [`RunSnapshot`] from a parsed manifest, deriving liveness,
    /// attempts, verification, and bounded log tails. (Steps 5–8 fill the
    /// job-specific reduction; Step 4 establishes identity and selection.)
    fn build_snapshot(
        &self,
        run_id: &str,
        run_dir: &Path,
        manifest: &DashboardManifest,
        now: DateTime<Utc>,
    ) -> RunSnapshot {
        let created_at = parse_rfc3339(&manifest.created_at);
        let job: RunJob = manifest.job.into();
        let lifecycle: RunLifecycle = manifest.lifecycle.into();
        // Read before deriving liveness: a retained `run_finished` event
        // can outrun the manifest's own lifecycle field (see
        // `derive_liveness`), so liveness needs the event tail already in
        // hand rather than reading it a second time.
        let event_tail = self.read_event_tail(run_id, run_dir);
        let liveness = derive_liveness(run_dir, manifest, &event_tail.events, now);
        let stage = derive_stage(&manifest.details, job);
        let identity = RunIdentity {
            run_id: run_id.to_string(),
            job: Some(job),
            lifecycle: Some(lifecycle),
            liveness,
            created_at,
            created_at_text: manifest.created_at.clone(),
            updated_at_text: manifest.updated_at.clone(),
            target_repo: manifest.target.repo.clone(),
            target_bead: manifest.target.bead.clone(),
            stage,
            schema: manifest.schema.clone(),
            roster_snapshot: manifest
                .roster_snapshot
                .as_ref()
                .map(|s| (s.path.clone(), s.size_bytes, s.sha256.clone())),
            roster_policy_sha256: manifest.roster_policy_sha256.clone(),
            musterroll_roster_artifact: manifest
                .musterroll_roster_artifact
                .as_ref()
                .map(|a| (a.path.clone(), a.sha256.clone())),
        };
        let event_count = event_tail.events.len();
        let events_truncated = event_tail.truncated;
        let events_error = event_tail.error.clone();
        let (roster, roster_error) = read_run_roster(run_dir);
        let (attempts, stage_markers) = match job {
            RunJob::Work | RunJob::Review | RunJob::Consult => (
                reduce_worker_attempts(&event_tail.events, roster.as_ref()),
                Vec::new(),
            ),
            RunJob::Plan => (
                Vec::new(),
                reduce_plan_stage_markers(&event_tail.events, roster.as_ref()),
            ),
        };
        let verification = derive_verification(manifest, &event_tail.events);
        RunSnapshot {
            identity,
            attempts,
            stage_markers,
            verification,
            // Log tails are opened on demand (see [`Self::read_log`]); a
            // refresh tick never reads worker/verify logs speculatively.
            logs: Vec::new(),
            harness_deck: derive_harness_deck(
                &self.config.reports_home,
                job,
                run_id,
                &manifest.details,
            ),
            event_count,
            events_truncated,
            selection_error: None,
            events_error,
            roster_error,
        }
    }

    /// Reads the incremental event tail for a run, advancing the per-run
    /// newline-offset state. A trailing partial line is retained/retried
    /// (offset not advanced, no error). A complete invalid line, a sequence
    /// gap, or an unknown schema is a source error that retains the last
    /// valid state. Unknown extra fields are tolerated. Input is capped at
    /// [`EVENT_TAIL_MAX_BYTES`] per tick and [`EVENT_TAIL_MAX_EVENTS`] events
    /// retained.
    fn read_event_tail(&self, run_id: &str, run_dir: &Path) -> EventTailState {
        let path = run_dir.join("events.jsonl");
        let file_len = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(_) => return EventTailState::default(),
        };
        let mut state = self
            .tails
            .borrow_mut()
            .get(run_id)
            .cloned()
            .unwrap_or_default();
        // If the file shrank (truncation/rotation), reset to the start.
        if state.offset > file_len {
            state = EventTailState::default();
        }
        // Each tick starts with a clean error slate; a still-bad line sets
        // it again below, so a persisting error keeps surfacing.
        state.error = None;
        // Bounded seek + read: never load more than EVENT_TAIL_MAX_BYTES into
        // memory in one tick, regardless of total file size.
        let Ok(window) = read_bounded_from(&path, state.offset, EVENT_TAIL_MAX_BYTES) else {
            return state;
        };
        // Split on newlines; the last fragment (no trailing newline) is a
        // partial line retained for the next tick.
        let mut new_offset = state.offset;
        let mut next_seq = state.next_seq;
        for line in window.split_inclusive(|b| *b == b'\n') {
            let ends_with_newline = line.ends_with(b"\n");
            let line_bytes = if ends_with_newline {
                &line[..line.len() - 1]
            } else {
                // Partial final line: an ordinary concurrent append. Do not
                // advance the offset, do not error. Retry next tick.
                break;
            };
            if line_bytes.iter().all(u8::is_ascii_whitespace) {
                new_offset += line.len() as u64;
                continue;
            }
            match parse_event_line(line_bytes, next_seq) {
                Ok(event) => {
                    next_seq = event.seq + 1;
                    new_offset += line.len() as u64;
                    state.events.push(event);
                }
                Err(error) => {
                    // A complete invalid line, sequence gap, or unknown
                    // schema is a source error: stop advancing, retain the
                    // last valid state. The offset stays at the start of
                    // this bad line so the next tick re-attempts it (and
                    // surfaces the error again rather than silently
                    // skipping it).
                    state.error = Some(error.message().to_string());
                    break;
                }
            }
        }
        // Trim to the retention cap once per tick rather than shifting the
        // whole retained vector for every event past the cap.
        let excess = state.events.len().saturating_sub(EVENT_TAIL_MAX_EVENTS);
        if excess > 0 {
            state.events.drain(..excess);
            state.truncated = true;
        }
        state.offset = new_offset;
        state.next_seq = next_seq;
        // The byte cap itself was hit this tick when the file still has
        // unread bytes beyond what this tick's full EVENT_TAIL_MAX_BYTES
        // window covered.
        let byte_cap_hit = file_len > state.offset && window.len() as u64 >= EVENT_TAIL_MAX_BYTES;
        if byte_cap_hit {
            state.truncated = true;
        }
        self.tails
            .borrow_mut()
            .insert(run_id.to_string(), state.clone());
        state
    }

    /// Opens exactly one fixed-allowlist log and returns its bounded,
    /// sanitized tail. Canonicalizes the run directory, joins the relative
    /// template, canonicalizes the candidate, and confirms containment
    /// before ever opening a file. An absolute or traversal attempt-directory
    /// component is rejected before any filesystem access — it never reaches
    /// `File::open`. Never derives a read path from model output, an
    /// artifact path string, an opaque profile ID, or an event-reported cwd;
    /// only the four fixed relative templates in [`LOG_ALLOWLIST`] are ever
    /// attempted.
    pub(crate) fn read_log(
        &self,
        run_id: &str,
        selector: &LogSelector,
    ) -> Result<LogTail, DashboardError> {
        validate_run_id(run_id)?;
        let run_dir = self.config.runs_dir().join(run_id);
        let relative = match selector {
            LogSelector::WorkerStdout(attempt_dir) => {
                validate_single_component(attempt_dir)?;
                Path::new("attempts")
                    .join(attempt_dir)
                    .join("worker.stdout.log")
            }
            LogSelector::WorkerStderr(attempt_dir) => {
                validate_single_component(attempt_dir)?;
                Path::new("attempts")
                    .join(attempt_dir)
                    .join("worker.stderr.log")
            }
            LogSelector::VerifyStdout => PathBuf::from("artifacts/verify/stdout.log"),
            LogSelector::VerifyStderr => PathBuf::from("artifacts/verify/stderr.log"),
        };
        let canonical_run_dir = run_dir.canonicalize().map_err(|error| {
            DashboardError::new(format!(
                "canonicalize run dir {}: {error}",
                run_dir.display()
            ))
        })?;
        let candidate = run_dir.join(&relative);
        let canonical_candidate = candidate.canonicalize().map_err(|error| {
            DashboardError::new(format!("log not found {}: {error}", candidate.display()))
        })?;
        if !canonical_candidate.starts_with(&canonical_run_dir) {
            // Containment failure: the candidate resolved outside the run
            // directory (e.g. via a symlink). Never opened.
            return Err(DashboardError::new(format!(
                "log path {} escapes run directory; refused",
                relative.display()
            )));
        }
        let (text, truncated) = read_log_tail_bytes(&canonical_candidate)?;
        Ok(LogTail {
            path: relative.display().to_string(),
            text,
            truncated,
        })
    }

    /// Test-only: how many candidates the bounded discovery pass kept.
    #[cfg(test)]
    pub(crate) fn scan_candidates_len_for_tests(&self) -> usize {
        self.scan_candidates()
            .expect("scan candidates")
            .candidates
            .len()
    }

    /// Test-only entry point exposing the private `snapshot_for_run` with an
    /// injectable clock so liveness tests can assert deterministic stale/fresh
    /// boundaries without relying on wall-clock timing.
    #[cfg(test)]
    pub(crate) fn snapshot_for_run_pub(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
    ) -> Result<RunSnapshot, DashboardError> {
        self.snapshot_for_run(run_id, now)
    }

    /// Test-only entry point exposing the incremental event-tail reader.
    /// Returns `(event_count, truncated, error, seqs)` so tests can assert
    /// partial-line retention, malformed/gap/schema error surfacing, and
    /// cap truncation without depending on private types.
    #[cfg(test)]
    pub(crate) fn read_event_tail_pub(
        &self,
        run_id: &str,
        run_dir: &Path,
    ) -> (usize, bool, Option<String>, Vec<u64>) {
        let state = self.read_event_tail(run_id, run_dir);
        let seqs = state.events.iter().map(|e| e.seq).collect();
        (state.events.len(), state.truncated, state.error, seqs)
    }
}

/// Reads a run's heartbeat file, returning the parsed timestamp if present
/// and parseable. A missing heartbeat returns `None`; an unparseable one
/// returns `None` (a malformed heartbeat does not fail the source, but it
/// is not usable liveness evidence).
fn read_heartbeat_file(run_dir: &Path) -> Option<DateTime<Utc>> {
    let path = run_dir.join("heartbeat");
    let contents = std::fs::read_to_string(&path).ok()?;
    parse_rfc3339(contents.trim())
}

/// Recorded owner pid and worker process-group id from the manifest's
/// job-tagged details, kept separate because each needs a different
/// liveness probe: [`crate::quarantine::process_alive`] for the owner (a
/// single process), [`crate::quarantine::process_group_alive`] for the
/// worker (a whole group, which can outlive the leader a manifest recorded
/// as `worker_pgid` — the group id equals the leader's pid at spawn time,
/// but a descendant can survive the leader's own exit). Folding both into
/// one `process_alive` sweep — the prior bug — misses exactly that case:
/// the leader is gone, `process_alive(pgid)` reads false, and a live
/// descendant in the group goes unseen.
struct RecordedProcessIdentity {
    owner_pid: Option<u32>,
    worker_pgid: Option<u32>,
}

fn recorded_process_identity(details: &serde_json::Value) -> RecordedProcessIdentity {
    let Some(state) = details.get("state") else {
        return RecordedProcessIdentity {
            owner_pid: None,
            worker_pgid: None,
        };
    };
    let as_pid = |key: &str| {
        state
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|&pid| pid > 0)
    };
    RecordedProcessIdentity {
        owner_pid: as_pid("owner_pid"),
        worker_pgid: as_pid("worker_pgid"),
    }
}

/// Derives the current stage label from the manifest's job-tagged details.
/// For work runs, the work stage; for plan runs, the active plan stage; for
/// review/consult, `None`.
fn derive_stage(details: &serde_json::Value, job: RunJob) -> Option<String> {
    let state = details.get("state")?;
    match job {
        RunJob::Work => state
            .get("stage")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        RunJob::Plan => state
            .get("progress")
            .and_then(|p| p.get("state"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        RunJob::Review | RunJob::Consult => None,
    }
}

/// Resolves the Harness Deck report join for a run (spec §105-110).
///
/// Work joins on `details.state.cycle_id`, plan on the run id itself, and
/// consult/review have no report by definition. Presence is a stat of
/// `report.json`; the report itself is never opened, so no report prose
/// reaches the render path.
///
/// The join key is untrusted manifest content, so it clears **two**
/// validators before a path exists: this module's own single-normal-
/// component [`validate_run_id`], then [`crate::deck::report_run_dir`]'s
/// charset rule. Both are needed and neither subsumes the other —
/// `deck`'s rule permits `.` and `..` (legal in a report id's charset, and
/// harmless for the writer, whose ids are self-generated), which would let a
/// `cycle_id` of `".."` name the reports directory one level above the join
/// root. A caller feeding it untrusted bytes owns that containment; a name
/// like `"cycle id with spaces"` is the mirror case, a single normal
/// component that only `deck`'s charset rule rejects.
fn derive_harness_deck(
    reports_home: &Path,
    job: RunJob,
    run_id: &str,
    details: &serde_json::Value,
) -> HarnessDeckState {
    let report_run_id = match job {
        RunJob::Consult | RunJob::Review => return HarnessDeckState::NoReportForJob,
        RunJob::Work => match details
            .get("state")
            .and_then(|state| state.get("cycle_id"))
            .and_then(serde_json::Value::as_str)
        {
            Some(cycle_id) => cycle_id,
            None => {
                return HarnessDeckState::Unresolved {
                    reason: "run state records no cycle id".to_string(),
                };
            }
        },
        RunJob::Plan => run_id,
    };
    if let Err(error) = validate_run_id(report_run_id) {
        return HarnessDeckState::Unresolved {
            reason: error.to_string(),
        };
    }
    match crate::deck::report_run_dir(reports_home, report_run_id) {
        Ok(dir) => HarnessDeckState::Resolved {
            present: dir.join("report.json").is_file(),
            report_dir: dir.display().to_string(),
        },
        Err(error) => HarnessDeckState::Unresolved {
            reason: error.to_string(),
        },
    }
}

/// Derives liveness from heartbeat/`updated_at`, the configured 60-second
/// stale threshold, a retained `run_finished` event, and nonmutating
/// owner-pid/worker-group probes. Lifecycle and liveness are distinct: a
/// `Finished` lifecycle is `Finished` liveness; a nonterminal run is `Live`
/// (fresh heartbeat), `Silent` (stale heartbeat but the owner pid or the
/// worker process group is still alive), `Abandoned` (stale, neither is
/// alive), or `Unknown` (no usable heartbeat/`updated_at` evidence).
fn derive_liveness(
    run_dir: &Path,
    manifest: &DashboardManifest,
    events: &[DashboardEvent],
    now: DateTime<Utc>,
) -> RunLiveness {
    if manifest.lifecycle == DashboardLifecycle::Finished {
        return RunLiveness::Finished;
    }
    // A retained `run_finished` event outruns the manifest: the run's own
    // event log proves it reached a terminal outcome even when `lifecycle`
    // has not (yet, or ever will) catch up — e.g. a crash between the
    // finish event and the manifest rewrite that would have recorded it.
    // Checked before heartbeat freshness so a run that just finished is
    // never reported `Live` off a heartbeat tick that predates its own
    // completion.
    if events.iter().any(|event| event.kind == "run_finished") {
        return RunLiveness::Finished;
    }
    // `last_seen` mirrors the operational `RunHandle::last_seen`:
    // heartbeat wins, else the manifest's `updated_at`.
    let heartbeat = read_heartbeat_file(run_dir);
    let last_seen = match heartbeat {
        Some(ts) => Some(ts),
        None => parse_rfc3339(&manifest.updated_at),
    };
    let Some(last_seen) = last_seen else {
        // No usable heartbeat or updated_at evidence.
        return RunLiveness::Unknown;
    };
    let fresh = now.signed_duration_since(last_seen) < STALE_HEARTBEAT_THRESHOLD;
    if fresh {
        return RunLiveness::Live;
    }
    // Stale: probe the recorded owner pid and worker process group
    // separately and nonmutatingly, each with the probe that matches what
    // it names (see `RecordedProcessIdentity`). `Silent` when either reads
    // alive — PID/PGID reuse makes this evidence, not proof. `Abandoned`
    // when neither does.
    let identity = recorded_process_identity(&manifest.details);
    let owner_live = identity
        .owner_pid
        .is_some_and(crate::quarantine::process_alive);
    let worker_live = identity
        .worker_pgid
        .is_some_and(crate::quarantine::process_group_alive);
    if owner_live || worker_live {
        RunLiveness::Silent
    } else {
        RunLiveness::Abandoned
    }
}

/// Reduces verification precedence: durable `details.state.mechanical`
/// (Work job only) wins over the latest valid `verify_finished` event,
/// which wins over "not run". `verifier.mechanical` supplies the command
/// string regardless of source. Disagreement between the durable
/// mechanical state and the latest event is visible rather than silently
/// reconciled.
fn derive_verification(
    manifest: &DashboardManifest,
    events: &[DashboardEvent],
) -> VerificationRecord {
    let command = manifest.verifier.mechanical.clone();
    let mechanical_passed = manifest
        .details
        .get("state")
        .and_then(|state| state.get("mechanical"))
        .and_then(|mechanical| mechanical.get("passed"))
        .and_then(serde_json::Value::as_bool);
    // The latest valid verify_finished event (last in seq order).
    let latest_verify = events
        .iter()
        .rev()
        .find(|event| event.kind == "verify_finished");
    let event_outcome = latest_verify.and_then(|event| event.outcome.clone());
    let event_passed = match event_outcome.as_deref() {
        Some("passed") => Some(true),
        Some("failed") => Some(false),
        // Any other outcome string is displayed verbatim but never
        // interpreted as a pass/fail determination.
        _ => None,
    };
    let (passed, source) = if let Some(mechanical_passed) = mechanical_passed {
        (Some(mechanical_passed), VerificationSource::Mechanical)
    } else if let Some(event_passed) = event_passed {
        (Some(event_passed), VerificationSource::Event)
    } else {
        (None, VerificationSource::NotRun)
    };
    let disagreement = matches!(
        (mechanical_passed, event_passed),
        (Some(m), Some(e)) if m != e
    );
    VerificationRecord {
        passed,
        source,
        command,
        event_outcome,
        disagreement,
    }
}

/// Reads and parses the run-local `roster.json` snapshot with the existing
/// Musterroll snapshot parser, never the source roster artifact. A missing
/// or malformed roster snapshot leaves attempt profiles unresolved rather
/// than failing the whole snapshot — but a *malformed* one returns its error
/// so the reason is displayed instead of silently degrading every attempt to
/// an unresolved opaque profile.
fn read_run_roster(run_dir: &Path) -> (Option<RosterSnapshot>, Option<String>) {
    let path = run_dir.join("roster.json");
    // No run-local roster at all: nothing to report beyond the
    // per-attempt unresolved marker.
    let Ok(bytes) = std::fs::read(&path) else {
        return (None, None);
    };
    match musterroll::parse_roster_snapshot(&bytes) {
        Ok(roster) => (Some(roster), None),
        Err(error) => (
            None,
            Some(format!(
                "run-local roster {} unusable: {error}",
                path.display()
            )),
        ),
    }
}

/// Resolves an execution identity by exact `profile_id` match against the
/// run-local roster snapshot. Never splits or parses identity out of an
/// opaque profile-id string or attempt-directory name.
fn resolve_roster_profile<'a>(
    roster: Option<&'a RosterSnapshot>,
    profile_id: Option<&str>,
) -> Option<&'a crate::musterroll::RosterProfile> {
    let roster = roster?;
    let profile_id = profile_id?;
    roster.profiles.iter().find(|p| p.profile_id == profile_id)
}

/// Reconstructs work/review/consult attempts by joining `attempt_started`
/// outcomes shaped `running:<attempt-directory>` to the fixed run-local
/// `attempts/<NNN>-<opaque-profile-id>/` directory; the ordinal comes from
/// `<NNN>`. Attempts are dispatched sequentially, so the Nth start pairs with
/// the Nth finish by encounter order; a trailing unpaired start has no finish
/// event. Provider/harness/model/dispatch-id are resolved only from the
/// run-local roster, never by parsing the attempt-directory string.
fn reduce_worker_attempts(
    events: &[DashboardEvent],
    roster: Option<&RosterSnapshot>,
) -> Vec<AttemptRecord> {
    let starts: Vec<&DashboardEvent> = events
        .iter()
        .filter(|event| {
            event.kind == "attempt_started"
                && event
                    .outcome
                    .as_deref()
                    .is_some_and(|outcome| outcome.starts_with("running:"))
        })
        .collect();
    let finishes: Vec<&DashboardEvent> = events
        .iter()
        .filter(|event| event.kind == "attempt_finished")
        .collect();
    starts
        .into_iter()
        .enumerate()
        .map(|(index, start)| {
            let attempt_dir = start
                .outcome
                .as_deref()
                .and_then(|outcome| outcome.strip_prefix("running:"))
                .unwrap_or_default()
                .to_string();
            let ordinal = attempt_dir
                .split('-')
                .next()
                .and_then(|prefix| prefix.parse::<u32>().ok())
                .unwrap_or(0);
            let profile_id = start.profile_id.clone();
            let resolved_profile = resolve_roster_profile(roster, profile_id.as_deref());
            let started_at = parse_rfc3339(&start.ts);
            let finish = finishes.get(index).copied();
            let finished_at = finish.and_then(|event| parse_rfc3339(&event.ts));
            let duration = match (started_at, finished_at) {
                (Some(began), Some(ended)) if ended >= began => (ended - began).to_std().ok(),
                _ => None,
            };
            AttemptRecord {
                ordinal,
                attempt_dir: Some(attempt_dir),
                profile_id,
                provider_id: resolved_profile.map(|p| p.provider_id.clone()),
                model: resolved_profile.map(|p| p.model.clone()),
                harness: resolved_profile.map(|p| p.harness.clone()),
                dispatch_id: resolved_profile.map(|p| p.dispatch_id.clone()),
                resolved: resolved_profile.is_some(),
                started_at,
                finished_at,
                duration,
                outcome: finish.and_then(|event| event.outcome.clone()),
            }
        })
        .collect()
}

/// Reconstructs plan stage markers from typed `plan_invocation` evidence
/// only; stage-marker events such as `planner_authoring` carry no
/// `plan_invocation` and are therefore never mistaken for a stage marker or
/// worker attempt. Start/finish pairing is by exact (stage, profile, attempt)
/// match, not encounter order, since plan stages are typed and unambiguous.
fn reduce_plan_stage_markers(
    events: &[DashboardEvent],
    roster: Option<&RosterSnapshot>,
) -> Vec<StageMarker> {
    let starts: Vec<(&DashboardEvent, &DashboardPlanInvocation)> = events
        .iter()
        .filter(|event| event.kind == "attempt_started")
        .filter_map(|event| event.plan_invocation.as_ref().map(|pi| (event, pi)))
        .collect();
    let finishes: Vec<(&DashboardEvent, &DashboardPlanInvocation)> = events
        .iter()
        .filter(|event| event.kind == "attempt_finished")
        .filter_map(|event| event.plan_invocation.as_ref().map(|pi| (event, pi)))
        .collect();
    starts
        .into_iter()
        .enumerate()
        .map(|(index, (start_event, start_pi))| {
            let finish = finishes.iter().find(|(_, finish_pi)| {
                finish_pi.stage == start_pi.stage
                    && finish_pi.execution.profile_id == start_pi.execution.profile_id
                    && finish_pi.attempt == start_pi.attempt
            });
            let resolved_profile =
                resolve_roster_profile(roster, Some(start_pi.execution.profile_id.as_str()));
            let started_at = parse_rfc3339(&start_event.ts);
            let finished_at = finish.and_then(|(event, _)| parse_rfc3339(&event.ts));
            let duration = finish
                .and_then(|(_, pi)| pi.duration_ms)
                .map(Duration::from_millis)
                .or_else(|| match (started_at, finished_at) {
                    (Some(began), Some(ended)) if ended >= began => (ended - began).to_std().ok(),
                    _ => None,
                });
            StageMarker {
                stage: start_pi.stage.clone(),
                role: Some(start_pi.role.clone()),
                ordinal: u32::try_from(index + 1).unwrap_or(u32::MAX),
                profile_id: Some(start_pi.execution.profile_id.clone()),
                provider_id: Some(start_pi.execution.provider_id.clone()),
                model: resolved_profile.map(|p| p.model.clone()),
                harness: resolved_profile.map(|p| p.harness.clone()),
                dispatch_id: resolved_profile.map(|p| p.dispatch_id.clone()),
                resolved: resolved_profile.is_some(),
                started_at,
                finished_at,
                duration,
                outcome: finish.and_then(|(event, _)| event.outcome.clone()),
            }
        })
        .collect()
}

/// Seeks to `offset` and reads at most `max_bytes` from `path`, never
/// loading more than that into memory regardless of the file's total size.
fn read_bounded_from(path: &Path, offset: u64, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Validates that `value` is a single normal path component: not empty,
/// not absolute, and not a traversal (`.`/`..`) or multi-component path.
/// Shared by log-selector attempt-directory validation.
fn validate_single_component(value: &str) -> Result<(), DashboardError> {
    use std::path::Component;
    let mut components = Path::new(value).components();
    if value.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(DashboardError::new(format!(
            "invalid log path component {value:?}"
        )));
    }
    Ok(())
}

/// Reads a bounded 64 KiB tail of an already-canonicalized, containment-
/// checked log path. Seeks from EOF when the file exceeds the cap, discards
/// through the first newline to realign to a clean line boundary when
/// starting mid-file, decodes lossily, then sanitizes control characters.
/// Sanitizing every control character (not only a "leading" one) is a
/// strictly stronger guarantee than stripping just a leading partial escape
/// sequence, and subsumes it: any ESC byte left dangling by the 64 KiB
/// boundary is removed wherever it falls in the retained text.
fn read_log_tail_bytes(path: &Path) -> Result<(String, bool), DashboardError> {
    let file_len = std::fs::metadata(path)
        .map_err(|error| DashboardError::new(format!("stat log {}: {error}", path.display())))?
        .len();
    let (offset, truncated) = if file_len > LOG_TAIL_MAX_BYTES {
        (file_len - LOG_TAIL_MAX_BYTES, true)
    } else {
        (0, false)
    };
    let mut bytes = read_bounded_from(path, offset, LOG_TAIL_MAX_BYTES)
        .map_err(|error| DashboardError::new(format!("read log {}: {error}", path.display())))?;
    if offset > 0 {
        // Starting mid-file: discard through the first newline for a clean
        // line boundary. A torn UTF-8 or CSI sequence in the discarded
        // prefix never reaches the retained text. If no newline exists in
        // the window at all (one giant unterminated line), keep it as-is —
        // there is no clean boundary to realign to.
        if let Some(pos) = bytes.iter().position(|&byte| byte == b'\n') {
            bytes.drain(..=pos);
        }
    }
    let decoded = String::from_utf8_lossy(&bytes).into_owned();
    Ok((sanitize_text(&decoded), truncated))
}

/// Parses one complete event line, enforcing the schema and sequence
/// contract. Unknown extra fields are tolerated (forward-compatible). An
/// unknown schema, malformed JSON, or a sequence gap returns an error so the
/// caller retains the last valid state.
fn parse_event_line(line: &[u8], expected_seq: u64) -> Result<DashboardEvent, DashboardError> {
    let value: serde_json::Value = serde_json::from_slice(line)
        .map_err(|error| DashboardError::new(format!("malformed event line: {error}")))?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<missing>");
    if schema != EXPECTED_EVENT_SCHEMA {
        return Err(DashboardError::new(format!(
            "unknown event schema {schema:?}, expected {EXPECTED_EVENT_SCHEMA:?}"
        )));
    }
    let event: DashboardEvent = serde_json::from_value(value)
        .map_err(|error| DashboardError::new(format!("malformed event: {error}")))?;
    // Sequence contract: the first event is seq 1; each subsequent event
    // increments by one. Any gap is a source error.
    if event.seq != expected_seq {
        return Err(DashboardError::new(format!(
            "event sequence gap: expected {expected_seq}, found {}",
            event.seq
        )));
    }
    Ok(event)
}

/// Parses an RFC3339 timestamp, returning `None` on failure (used for
/// ordering and display; a malformed timestamp does not fail the source).
fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

/// Orders two optional `created_at` timestamps ascending, with `None`
/// sorting as the *greatest* (newest). A malformed manifest with no parsed
/// `created_at` is therefore treated as potentially-newest so it remains
/// visible rather than silently demoted below older, valid runs.
fn compare_created_at(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod attempts;
#[cfg(test)]
mod discovery;
#[cfg(test)]
mod events;
#[cfg(test)]
mod harness_deck;
#[cfg(test)]
mod liveness;
#[cfg(test)]
mod logs;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod verification;
