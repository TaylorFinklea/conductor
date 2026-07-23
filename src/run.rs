//! `conductor/run@2` manifest + `conductor/event@2` JSONL run artifacts.
//!
//! Every active run lives under `<state_dir>/runs-v2/<run-id>/`: a whole-file
//! atomic `manifest.json` replacement and an append-only `events.jsonl`.
//! Finished `runs/` artifacts are legacy history and are never scanned by the
//! active v2 reader.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::bursar::RuntimeLimitEvidence;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Schema tag stamped on every manifest written by this module.
pub(crate) const RUN_SCHEMA: &str = "conductor/run@2";
/// Schema tag stamped on every event line written by this module.
pub(crate) const EVENT_SCHEMA: &str = "conductor/event@2";
const TERMINAL_TRANSITION_PATH: &str = "artifacts/terminal-transition.json";

pub(crate) type Result<T> = std::result::Result<T, RunError>;

/// Read-only deployment gate over legacy `runs/` artifacts. Active v2 code
/// never opens a v1 run; this classifier merely blocks cutover while recovery
/// work still exists and leaves all legacy bytes untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LegacyV1Preflight {
    pub(crate) pending: usize,
    pub(crate) implementing: usize,
    pub(crate) reclaimable: usize,
}

impl LegacyV1Preflight {
    pub(crate) const fn actionable(self) -> usize {
        self.pending + self.implementing + self.reclaimable
    }

    pub(crate) const fn activation_allowed(self) -> bool {
        self.actionable() == 0
    }
}

/// Error returned by run-artifact reads and writes.
#[derive(Debug, Clone)]
pub(crate) struct RunError {
    message: String,
}

impl RunError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn into_message(self) -> String {
        self.message
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RunError {}

/// The closed job kinds from the core-consolidation spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RunJob {
    Work,
    Review,
    Consult,
    Plan,
}

/// Run lifecycle state pinned on the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunLifecycle {
    Started,
    Running,
    Finished,
}

/// One event kind from the spec's stable `conductor/event@2` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    RunStarted,
    AttemptStarted,
    AttemptFinished,
    VerifyFinished,
    ReviewFinished,
    RunFinished,
    CoverageGap,
}

/// `{"path": ..., "sha256": ...}` artifact identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactRef {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

/// The content-addressed identity of the exact Bursar snapshot copied into a
/// v2 run directory before profile selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RosterSnapshotArtifact {
    pub(crate) path: String,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: String,
}

/// The raw snapshot envelope supplied when preparing a run. This never
/// serializes into a manifest; [`RunHandle::create`] copies the bytes to
/// `roster.json` and persists only its run-local identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RosterSnapshotInput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) policy_sha256: String,
}

/// `{"repo": ..., "bead": ...}` run/event target identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunTarget {
    pub(crate) repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bead: Option<String>,
}

/// Approved profile/fallback envelope pinned into the manifest at run start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovedProfileEnvelope {
    pub(crate) profiles: Vec<String>,
}

/// Runtime limits pinned into the manifest at run start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) item_wall_clock_mins: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_attempts: Option<u64>,
}

/// Verifier configuration pinned into the manifest before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunVerifier {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mechanical: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) qualitative: Option<String>,
}

/// Durable work-stage boundary for a mechanically verified commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkStage {
    Implementing,
    PendingReview,
    Completed,
}

/// Immutable mechanical-verifier evidence pinned before qualitative review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MechanicalVerification {
    pub(crate) command: String,
    pub(crate) passed: bool,
    pub(crate) artifact_refs: Vec<ArtifactRef>,
}

/// The one Bead mutation a terminal work run may still owe after its durable
/// evidence is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalTransitionAction {
    Close,
    Release,
}

/// Metadata that must be applied before a released review revision is made
/// eligible for another approved cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalTransitionMetadata {
    pub(crate) key: String,
    pub(crate) value: String,
}

/// Content-addressed intent for the Bead mutation following `run_finished`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalTransition {
    pub(crate) action: TerminalTransitionAction,
    pub(crate) reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<TerminalTransitionMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) comment: Option<String>,
}

/// Work-only progress persisted inside the canonical run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkState {
    pub(crate) cycle_id: String,
    pub(crate) authorization_sha256: String,
    /// HEAD recorded immediately before the first worker attempt. Absent on
    /// manifests written before this field existed; recovery logic that
    /// depends on an exact match must treat that absence as weaker evidence,
    /// not as a match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) before_head: Option<String>,
    /// pid of the `conductor` process that created this run, recorded once
    /// at creation and never mutated — the same OS process drives worker
    /// dispatch, mechanical verification, and qualitative review for a run's
    /// entire lifetime, so this single value authenticates ownership across
    /// all of those stages. Absent on manifests written before this field
    /// existed; recovery logic must treat that absence as weaker evidence,
    /// not as proof of death (mirrors `before_head`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner_pid: Option<u32>,
    /// Process-group id of the currently dispatched worker, recorded via
    /// [`RunHandle::record_worker_group`] immediately after each worker is
    /// spawned (workers lead their own process group, so the group id equals
    /// the worker pid) and before that worker can meaningfully mutate the
    /// repository. A dead `conductor` owner is *not* proof that a separately
    /// grouped worker it launched has also died: an orphaned worker survives
    /// its parent and can keep writing. Stale-claim recovery therefore refuses
    /// to reclaim until this worker group is provably gone, in addition to the
    /// owner. Cleared to `None` via [`RunHandle::invalidate_worker_group`]
    /// immediately before each fallback attempt is spawned, then re-bound to
    /// that attempt's pgid once it starts — so a crash between those two
    /// calls never leaves a superseded (already-dead) attempt's identity
    /// standing in for a new, still-unrecorded one. Absent before the first
    /// spawn (or on older manifests), where recovery treats a missing worker
    /// identity as unprovable and fails closed rather than reclaiming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worker_pgid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) worker_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) worker_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mechanical: Option<MechanicalVerification>,
    /// A fresh review-only allowance that an operator policy explicitly
    /// approved for a `--resume` invocation. It is written before any resumed
    /// reviewer can spawn; absent means recovery may revalidate only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) review_resume_budget_secs: Option<u64>,
    pub(crate) stage: WorkStage,
}

/// The only durable plan-routing stages. The serde spelling is shared by
/// manifests, events, configuration adapters, and ledger evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanStage {
    Planner,
    PeerReview,
    SecondOpinion,
}

/// Immutable exact execution identity approved for a plan stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovedExecution {
    pub(crate) profile_id: String,
    pub(crate) provider_id: String,
    pub(crate) availability_key: String,
    pub(crate) execution_key: String,
}

/// One immutable constrained route, selected before the run can advance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanStageRoute {
    pub(crate) stage: PlanStage,
    pub(crate) capability_role: String,
    pub(crate) candidates: Vec<ApprovedExecution>,
    pub(crate) provider_distinct_from: Vec<PlanStage>,
}

/// Immutable route envelope for all legal plan stages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanRoutes {
    pub(crate) stages: Vec<PlanStageRoute>,
}

/// Tier declared or derived from a captured plan target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PlanTier {
    Junior,
    Senior,
    Lead,
}

/// Bounded target complexity declared or derived from a captured plan target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PlanComplexity {
    S,
    M,
    L,
    XL,
}

/// Immutable input copied into a plan run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum PlanInput {
    Bead {
        bead_id: String,
        artifact: ArtifactRef,
        tier: PlanTier,
        complexity: PlanComplexity,
    },
    Artifact {
        artifact: ArtifactRef,
        tier: PlanTier,
        complexity: PlanComplexity,
    },
}

/// A plan run has exactly one canonical repository and one tagged input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanTarget {
    pub(crate) repo: String,
    pub(crate) input: PlanInput,
}

/// Bounded revision counter. Values outside 0..=3 are rejected on deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RevisionLimit(u8);

impl RevisionLimit {
    pub(crate) fn new(value: u8) -> Result<Self> {
        if value > 3 {
            return Err(RunError::new("plan revision limit must be in 0..=3"));
        }
        Ok(Self(value))
    }

    pub(crate) const fn value(self) -> u8 {
        self.0
    }

    fn consume(&mut self) -> Result<()> {
        if self.0 == 3 {
            return Err(RunError::new("plan revision limit exhausted"));
        }
        self.0 += 1;
        Ok(())
    }
}

/// A stage can never be attempted zero times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct StageAttemptLimit(u8);

impl StageAttemptLimit {
    pub(crate) fn new(value: u8) -> Result<Self> {
        if value == 0 {
            return Err(RunError::new("plan stage-attempt limit must be nonzero"));
        }
        Ok(Self(value))
    }

    pub(crate) const fn value(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PeerVerdict {
    Approve,
    Revise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SecondOpinionVerdict {
    Accept,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanTerminalVerdict {
    Accepted,
    Rejected,
    Blocked,
    NeedsInput,
}

/// Mutable plan progress; every binding becomes immutable at its legal stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PlanProgress {
    Blocked {
        cancellable: bool,
    },
    Prepared,
    Authoring {
        author: ApprovedExecution,
        attempts: u8,
    },
    AwaitingPeer {
        author: ApprovedExecution,
        #[serde(skip_serializing_if = "Option::is_none")]
        peer: Option<ApprovedExecution>,
        artifact: ArtifactRef,
        revisions: RevisionLimit,
    },
    Revising {
        author: ApprovedExecution,
        peer: ApprovedExecution,
        artifact: ArtifactRef,
        revisions: RevisionLimit,
    },
    AwaitingSecondOpinion {
        author: ApprovedExecution,
        peer: ApprovedExecution,
        artifact: ArtifactRef,
        revisions: RevisionLimit,
    },
    Terminal {
        verdict: PlanTerminalVerdict,
    },
}

impl PlanProgress {
    pub(crate) fn start_authoring(
        &mut self,
        author: ApprovedExecution,
        _attempt_limit: StageAttemptLimit,
    ) -> Result<()> {
        if !matches!(self, Self::Prepared) {
            return Err(RunError::new("plan author can bind only from prepared"));
        }
        *self = Self::Authoring {
            author,
            attempts: 0,
        };
        Ok(())
    }

    pub(crate) fn block_before_authoring(&mut self) -> Result<()> {
        if !matches!(self, Self::Prepared) {
            return Err(RunError::new(
                "only an unstarted plan can become cancellably blocked",
            ));
        }
        *self = Self::Blocked { cancellable: true };
        Ok(())
    }

    pub(crate) fn replace_author_before_artifact(
        &mut self,
        author: ApprovedExecution,
    ) -> Result<()> {
        let Self::Authoring {
            author: persisted, ..
        } = self
        else {
            return Err(RunError::new(
                "plan author fallback requires active authoring",
            ));
        };
        *persisted = author;
        Ok(())
    }

    fn record_author_attempt(&mut self, attempt_limit: StageAttemptLimit) -> Result<()> {
        let Self::Authoring { attempts, .. } = self else {
            return Err(RunError::new(
                "plan author attempt is not legal in this state",
            ));
        };
        *attempts = attempts
            .checked_add(1)
            .ok_or_else(|| RunError::new("plan author attempt counter overflow"))?;
        if *attempts > attempt_limit.value() {
            return Err(RunError::new("plan author attempt limit exhausted"));
        }
        Ok(())
    }

    pub(crate) fn await_peer(&mut self, artifact: ArtifactRef) -> Result<()> {
        let Self::Authoring { author, .. } = self else {
            return Err(RunError::new("plan can await peer only after authoring"));
        };
        *self = Self::AwaitingPeer {
            author: author.clone(),
            peer: None,
            artifact,
            revisions: RevisionLimit::new(0)?,
        };
        Ok(())
    }

    pub(crate) fn record_peer_verdict(
        &mut self,
        peer: ApprovedExecution,
        verdict: PeerVerdict,
    ) -> Result<()> {
        let Self::AwaitingPeer {
            author,
            peer: bound_peer,
            artifact,
            revisions,
        } = self
        else {
            return Err(RunError::new(
                "peer verdict is not legal in this plan state",
            ));
        };
        if peer.profile_id == author.profile_id || peer.provider_id == author.provider_id {
            return Err(RunError::new(
                "peer binding must be distinct from immutable author binding",
            ));
        }
        if let Some(bound) = bound_peer {
            if bound != &peer {
                return Err(RunError::new(
                    "peer binding cannot change after the first peer verdict",
                ));
            }
        }
        let author = author.clone();
        let artifact = artifact.clone();
        let revisions = *revisions;
        *self = match verdict {
            PeerVerdict::Approve => Self::AwaitingSecondOpinion {
                author,
                peer,
                artifact,
                revisions,
            },
            PeerVerdict::Revise => Self::Revising {
                author,
                peer,
                artifact,
                revisions,
            },
        };
        Ok(())
    }

    pub(crate) fn complete_revision(&mut self, artifact: ArtifactRef) -> Result<()> {
        let Self::Revising {
            author,
            peer,
            revisions,
            ..
        } = self
        else {
            return Err(RunError::new("revision is not legal in this plan state"));
        };
        let author = author.clone();
        let peer = peer.clone();
        let mut revisions = *revisions;
        revisions.consume()?;
        *self = Self::AwaitingPeer {
            author,
            peer: Some(peer),
            artifact,
            revisions,
        };
        Ok(())
    }

    pub(crate) fn record_second_opinion(&mut self, verdict: SecondOpinionVerdict) -> Result<()> {
        if !matches!(self, Self::AwaitingSecondOpinion { .. }) {
            return Err(RunError::new(
                "second opinion verdict is not legal in this plan state",
            ));
        }
        *self = Self::Terminal {
            verdict: match verdict {
                SecondOpinionVerdict::Accept => PlanTerminalVerdict::Accepted,
                SecondOpinionVerdict::Reject => PlanTerminalVerdict::Rejected,
            },
        };
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanRunDetails {
    pub(crate) target: PlanTarget,
    pub(crate) routes: PlanRoutes,
    pub(crate) progress: PlanProgress,
    pub(crate) revision_limit: RevisionLimit,
    pub(crate) stage_attempt_limit: StageAttemptLimit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewRunDetails {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConsultRunDetails {}

/// The job-tagged state space prevents any job from serializing another job's
/// mutable state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum RunDetails {
    Work { state: Option<WorkState> },
    Review { state: ReviewRunDetails },
    Consult { state: ConsultRunDetails },
    Plan { state: PlanRunDetails },
}

/// `conductor/run@2` — the atomic, versioned run manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunManifest {
    pub(crate) schema: String,
    pub(crate) run_id: String,
    pub(crate) job: RunJob,
    pub(crate) target: RunTarget,
    pub(crate) details: RunDetails,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) approved_profiles: ApprovedProfileEnvelope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bursar_roster_artifact: Option<ArtifactRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) roster_snapshot: Option<RosterSnapshotArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) roster_policy_sha256: Option<String>,
    pub(crate) limits: RunLimits,
    pub(crate) verifier: RunVerifier,
    #[serde(default)]
    pub(crate) artifacts: Vec<ArtifactRef>,
    pub(crate) lifecycle: RunLifecycle,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<String>,
}

impl RunManifest {
    pub(crate) fn work(&self) -> Option<&WorkState> {
        match &self.details {
            RunDetails::Work { state } => state.as_ref(),
            RunDetails::Review { .. } | RunDetails::Consult { .. } | RunDetails::Plan { .. } => {
                None
            }
        }
    }
}

/// `conductor/event@2` — one append-only event line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunEvent {
    pub(crate) schema: String,
    pub(crate) event_id: String,
    pub(crate) run_id: String,
    pub(crate) seq: u64,
    pub(crate) ts: String,
    pub(crate) kind: EventKind,
    pub(crate) job: RunJob,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) profile_id: Option<String>,
    pub(crate) target: RunTarget,
    #[serde(default)]
    pub(crate) artifact_refs: Vec<ArtifactRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider_limit: Option<RuntimeLimitEvidence>,
}

/// Fields pinned into a new run's manifest at creation.
#[derive(Debug, Clone, Default)]
pub(crate) struct NewRun {
    pub(crate) target: RunTarget,
    pub(crate) approved_profiles: Vec<String>,
    pub(crate) bursar_roster_artifact: Option<ArtifactRef>,
    pub(crate) roster_snapshot: Option<RosterSnapshotInput>,
    pub(crate) limits: RunLimits,
    pub(crate) verifier: RunVerifier,
    pub(crate) work: Option<WorkState>,
    pub(crate) approval: Option<serde_json::Value>,
}

/// Fields pinned into a new native plan run before approval. The input bytes
/// are copied once to the path declared by its immutable [`PlanTarget`].
#[derive(Debug, Clone)]
pub(crate) struct NewPlanRun {
    pub(crate) run_id: String,
    pub(crate) target: RunTarget,
    pub(crate) details: PlanRunDetails,
    pub(crate) approved_profiles: Vec<String>,
    pub(crate) bursar_roster_artifact: Option<ArtifactRef>,
    pub(crate) roster_snapshot: RosterSnapshotInput,
    pub(crate) limits: RunLimits,
    pub(crate) verifier: RunVerifier,
    pub(crate) approval: serde_json::Value,
    pub(crate) input_bytes: Vec<u8>,
}

/// Fields for one `conductor/event@2` row; `run_id`, `seq`, `ts`, `job`, and
/// `target` are filled in by the owning [`RunHandle`].
#[derive(Debug, Clone, Default)]
pub(crate) struct EventInput {
    pub(crate) profile_id: Option<String>,
    pub(crate) artifact_refs: Vec<ArtifactRef>,
    pub(crate) outcome: Option<String>,
    pub(crate) provider_limit: Option<RuntimeLimitEvidence>,
}

/// Handle to one created (or reopened) run directory; owns the manifest and
/// the append-only event log's next sequence number.
pub(crate) struct RunHandle {
    dir: PathBuf,
    manifest: RunManifest,
    next_seq: u64,
}

/// Monotonic per-process disambiguator used in addition to the process id and
/// exclusive directory creation. Correctness comes from `create_dir`, not the
/// clock or this counter.
static RUN_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn plan_input_artifact(input: &PlanInput) -> &ArtifactRef {
    match input {
        PlanInput::Bead { artifact, .. } | PlanInput::Artifact { artifact, .. } => artifact,
    }
}
fn new_run_id(job: RunJob, now: DateTime<Utc>, counter: u64) -> String {
    format!(
        "run-{}-{}-p{}-{counter:06}",
        job_label(job),
        now.format("%Y%m%dT%H%M%S%.9f"),
        std::process::id(),
    )
}

const fn job_label(job: RunJob) -> &'static str {
    match job {
        RunJob::Work => "work",
        RunJob::Review => "review",
        RunJob::Consult => "consult",
        RunJob::Plan => "plan",
    }
}

impl RunHandle {
    /// Creates a new run directory under `<state_dir>/runs/<run-id>/` and
    /// writes the initial `manifest.json`.
    pub(crate) fn create(state_dir: &Path, job: RunJob, request: NewRun) -> Result<Self> {
        Self::create_at(state_dir, job, request, Utc::now())
    }

    /// Same as [`Self::create`] with an explicit clock, so callers that need
    /// deterministic `created_at` ordering (e.g. legacy-run recovery tests)
    /// do not have to depend on real wall-clock spacing between calls.
    #[expect(
        clippy::too_many_lines,
        reason = "atomic run creation validates, persists, and records the initial immutable evidence"
    )]
    pub(crate) fn create_at(
        state_dir: &Path,
        job: RunJob,
        request: NewRun,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        require_v2_activation_preflight(state_dir)?;
        let NewRun {
            target,
            approved_profiles,
            bursar_roster_artifact,
            roster_snapshot,
            limits,
            verifier,
            work,
            approval,
        } = request;
        if let Some(artifact) = bursar_roster_artifact.as_ref() {
            validate_artifact_ref(artifact, "bursar roster artifact")?;
        }
        if let Some(snapshot) = roster_snapshot.as_ref() {
            validate_sha256(&snapshot.policy_sha256, "roster policy")?;
            let parsed = crate::bursar::parse_roster_snapshot(&snapshot.bytes)
                .map_err(|error| RunError::new(format!("invalid roster snapshot: {error}")))?;
            if parsed.policy_sha256() != snapshot.policy_sha256 {
                return Err(RunError::new(
                    "roster snapshot policy_sha256 does not match prepared policy",
                ));
            }
        }
        let details = match job {
            RunJob::Work => RunDetails::Work { state: work },
            RunJob::Review => RunDetails::Review {
                state: ReviewRunDetails::default(),
            },
            RunJob::Consult => RunDetails::Consult {
                state: ConsultRunDetails::default(),
            },
            RunJob::Plan => {
                return Err(RunError::new(
                    "plan runs require explicit structural PlanRunDetails and are not activated",
                ));
            }
        };
        let root = runs_dir(state_dir);
        std::fs::create_dir_all(&root).map_err(|e| {
            RunError::new(format!("failed to create runs dir {}: {e}", root.display()))
        })?;
        let (run_id, dir) = loop {
            let counter = RUN_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
            let run_id = new_run_id(job, now, counter);
            let dir = root.join(&run_id);
            match std::fs::create_dir(&dir) {
                Ok(()) => break (run_id, dir),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(RunError::new(format!(
                        "failed to create run dir {}: {error}",
                        dir.display()
                    )));
                }
            }
        };
        let created_at = now.to_rfc3339();
        let manifest = RunManifest {
            schema: RUN_SCHEMA.to_string(),
            run_id,
            job,
            target,
            details,
            created_at: created_at.clone(),
            updated_at: created_at,
            approved_profiles: ApprovedProfileEnvelope {
                profiles: approved_profiles,
            },
            bursar_roster_artifact,
            roster_snapshot: None,
            roster_policy_sha256: None,
            limits,
            verifier,
            artifacts: Vec::new(),
            lifecycle: RunLifecycle::Started,
            outcome: None,
        };
        let mut handle = Self {
            dir,
            manifest,
            next_seq: 1,
        };
        let cleanup_dir = handle.dir.clone();
        let setup = (|| {
            std::fs::create_dir(handle.dir.join("attempts")).map_err(|error| {
                RunError::new(format!(
                    "failed to create attempts dir {}: {error}",
                    handle.dir.join("attempts").display()
                ))
            })?;
            std::fs::create_dir(handle.dir.join("artifacts")).map_err(|error| {
                RunError::new(format!(
                    "failed to create artifacts dir {}: {error}",
                    handle.dir.join("artifacts").display()
                ))
            })?;
            let mut initial_refs = Vec::new();
            if let Some(approval) = approval.as_ref() {
                initial_refs.push(handle.write_approval(approval)?);
            }
            if let Some(snapshot) = roster_snapshot.as_ref() {
                let relative = Path::new("roster.json");
                write_new_file(&handle.dir.join(relative), &snapshot.bytes)?;
                let copied = RosterSnapshotArtifact {
                    path: "roster.json".to_string(),
                    size_bytes: u64::try_from(snapshot.bytes.len())
                        .map_err(|_| RunError::new("roster snapshot exceeds u64"))?,
                    sha256: format!("{:x}", Sha256::digest(&snapshot.bytes)),
                };
                handle.manifest.roster_policy_sha256 = Some(snapshot.policy_sha256.clone());
                handle.manifest.roster_snapshot = Some(copied.clone());
                initial_refs.push(ArtifactRef {
                    path: copied.path,
                    sha256: copied.sha256,
                });
            }
            if let Some(roster) = handle.manifest.bursar_roster_artifact.clone() {
                initial_refs.push(roster);
            }
            handle.write_manifest()?;
            handle.append_event_at(
                EventKind::RunStarted,
                EventInput {
                    artifact_refs: initial_refs,
                    outcome: Some("started".to_string()),
                    ..EventInput::default()
                },
                now,
            )?;
            if handle.manifest.roster_snapshot.is_none()
                && handle.manifest.bursar_roster_artifact.is_none()
            {
                handle.append_event_at(
                    EventKind::CoverageGap,
                    EventInput {
                        outcome: Some("bursar_roster_artifact_unavailable".to_string()),
                        ..EventInput::default()
                    },
                    now,
                )?;
            }
            Ok(handle)
        })();
        if setup.is_err() {
            let _ = std::fs::remove_dir_all(cleanup_dir);
        }
        setup
    }

    /// Creates the canonical durable state for an approval-gated native plan.
    /// Unlike generic runs, every plan is born with its structural details,
    /// copied input, exact roster snapshot, and immutable authorization.
    #[expect(
        clippy::too_many_lines,
        reason = "plan creation persists all immutable artifacts before the initial event"
    )]
    pub(crate) fn create_plan(state_dir: &Path, request: NewPlanRun) -> Result<Self> {
        require_v2_activation_preflight(state_dir)?;
        let NewPlanRun {
            run_id,
            target,
            details,
            approved_profiles,
            bursar_roster_artifact,
            roster_snapshot,
            limits,
            verifier,
            approval,
            input_bytes,
        } = request;
        validate_run_id(&run_id)?;
        validate_sha256(&roster_snapshot.policy_sha256, "roster policy")?;
        let parsed = crate::bursar::parse_roster_snapshot(&roster_snapshot.bytes)
            .map_err(|error| RunError::new(format!("invalid roster snapshot: {error}")))?;
        if parsed.policy_sha256() != roster_snapshot.policy_sha256 {
            return Err(RunError::new(
                "roster snapshot policy_sha256 does not match prepared policy",
            ));
        }
        if let Some(artifact) = bursar_roster_artifact.as_ref() {
            validate_artifact_ref(artifact, "bursar roster artifact")?;
        }
        let input_artifact = plan_input_artifact(&details.target.input).clone();
        validate_artifact_ref(&input_artifact, "plan target input")?;
        if format!("{:x}", Sha256::digest(&input_bytes)) != input_artifact.sha256 {
            return Err(RunError::new(
                "copied plan input bytes do not match the immutable plan target digest",
            ));
        }
        let root = runs_dir(state_dir);
        std::fs::create_dir_all(&root).map_err(|error| {
            RunError::new(format!(
                "failed to create runs dir {}: {error}",
                root.display()
            ))
        })?;
        let dir = root.join(&run_id);
        std::fs::create_dir(&dir).map_err(|error| {
            RunError::new(format!(
                "failed to create plan run dir {}: {error}",
                dir.display()
            ))
        })?;
        let created_at = Utc::now().to_rfc3339();
        let manifest = RunManifest {
            schema: RUN_SCHEMA.to_string(),
            run_id,
            job: RunJob::Plan,
            target,
            details: RunDetails::Plan { state: details },
            created_at: created_at.clone(),
            updated_at: created_at,
            approved_profiles: ApprovedProfileEnvelope {
                profiles: approved_profiles,
            },
            bursar_roster_artifact,
            roster_snapshot: None,
            roster_policy_sha256: None,
            limits,
            verifier,
            artifacts: Vec::new(),
            lifecycle: RunLifecycle::Started,
            outcome: None,
        };
        let mut handle = Self {
            dir,
            manifest,
            next_seq: 1,
        };
        let cleanup_dir = handle.dir.clone();
        let setup = (|| {
            std::fs::create_dir(handle.dir.join("attempts")).map_err(|error| {
                RunError::new(format!(
                    "failed to create attempts dir {}: {error}",
                    handle.dir.join("attempts").display()
                ))
            })?;
            std::fs::create_dir(handle.dir.join("artifacts")).map_err(|error| {
                RunError::new(format!(
                    "failed to create artifacts dir {}: {error}",
                    handle.dir.join("artifacts").display()
                ))
            })?;
            let input_relative = Path::new(&input_artifact.path);
            write_new_file(&handle.dir.join(input_relative), &input_bytes)?;
            validate_plan_details(
                &handle.manifest_path(),
                match &handle.manifest.details {
                    RunDetails::Plan { state } => state,
                    _ => return Err(RunError::new("native plan run lost plan details")),
                },
            )?;
            let approval_ref = handle.write_approval(&approval)?;
            let roster_relative = Path::new("roster.json");
            write_new_file(&handle.dir.join(roster_relative), &roster_snapshot.bytes)?;
            let copied_roster = RosterSnapshotArtifact {
                path: "roster.json".to_string(),
                size_bytes: u64::try_from(roster_snapshot.bytes.len())
                    .map_err(|_| RunError::new("roster snapshot exceeds u64"))?,
                sha256: format!("{:x}", Sha256::digest(&roster_snapshot.bytes)),
            };
            handle.manifest.roster_policy_sha256 = Some(roster_snapshot.policy_sha256);
            handle.manifest.roster_snapshot = Some(copied_roster.clone());
            let mut initial_refs = vec![
                input_artifact.clone(),
                approval_ref,
                ArtifactRef {
                    path: copied_roster.path,
                    sha256: copied_roster.sha256,
                },
            ];
            if let Some(roster) = handle.manifest.bursar_roster_artifact.clone() {
                initial_refs.push(roster);
            }
            handle.write_manifest()?;
            handle.append_event(
                EventKind::RunStarted,
                EventInput {
                    artifact_refs: initial_refs,
                    outcome: Some("awaiting_approval".to_string()),
                    ..EventInput::default()
                },
            )?;
            Ok(handle)
        })();
        if setup.is_err() {
            let _ = std::fs::remove_dir_all(cleanup_dir);
        }
        setup
    }

    /// Reopens an existing run directory, validating the manifest schema and
    /// resuming the event sequence counter after the last recorded event.
    pub(crate) fn open(state_dir: &Path, run_id: &str) -> Result<Self> {
        validate_run_id(run_id)?;
        let dir = runs_dir(state_dir).join(run_id);
        let mut manifest = read_manifest(&dir.join("manifest.json"))?;
        if manifest.run_id != run_id {
            return Err(RunError::new(format!(
                "manifest run_id {:?} does not match directory {run_id:?}",
                manifest.run_id
            )));
        }
        let events_path = dir.join("events.jsonl");
        let events = read_events(&events_path)?;
        if events.is_empty() {
            return Err(RunError::new("run event log is empty"));
        }
        for event in &events {
            if event.run_id != manifest.run_id
                || event.job != manifest.job
                || event.target != manifest.target
            {
                return Err(RunError::new(format!(
                    "event identity does not match manifest at sequence {}",
                    event.seq
                )));
            }
        }
        let last = events.last().expect("non-empty events checked above");
        if matches!(last.kind, EventKind::RunFinished) {
            validate_terminal_event(last)?;
            if !matches!(manifest.lifecycle, RunLifecycle::Finished) {
                reconcile_terminal_manifest(&dir, &mut manifest, last)?;
            }
        }
        if matches!(manifest.lifecycle, RunLifecycle::Finished)
            != matches!(last.kind, EventKind::RunFinished)
        {
            return Err(RunError::new(
                "manifest lifecycle does not match terminal event state",
            ));
        }
        if matches!(last.kind, EventKind::RunFinished) && manifest.outcome != last.outcome {
            return Err(RunError::new(
                "manifest outcome does not match terminal event outcome",
            ));
        }
        validate_work_events(&manifest, &events)?;
        let next_seq = last.seq + 1;
        Ok(Self {
            dir,
            manifest,
            next_seq,
        })
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.manifest.run_id
    }

    pub(crate) fn manifest(&self) -> &RunManifest {
        &self.manifest
    }

    /// The pid recorded at creation for this run's owning `conductor`
    /// process, if any (see [`WorkState::owner_pid`]).
    pub(crate) fn owner_pid(&self) -> Option<u32> {
        self.work().and_then(|work| work.owner_pid)
    }

    /// The process-group id of the most recently spawned worker, if one has
    /// been recorded yet (see [`WorkState::worker_pgid`]).
    pub(crate) fn worker_pgid(&self) -> Option<u32> {
        self.work().and_then(|work| work.worker_pgid)
    }

    /// Durably clears any worker-group identity recorded by an earlier
    /// attempt, before a new attempt's worker is ever spawned (see
    /// `WorkerHooks::on_pre_spawn`). Pairs with [`RunHandle::record_worker_group`]
    /// to make each fallback attempt a two-phase protocol — invalidate, then
    /// spawn, then (only on success) bind — so a crash between the spawn and
    /// the matching `record_worker_group` call leaves the manifest holding no
    /// identity at all, never a superseded attempt's already-dead one that
    /// stale-claim recovery could mistake for proof this new, still-unrecorded
    /// attempt died too (recovery already fails closed on a missing
    /// `worker_pgid`). A no-op in effect, but still durably re-persisted, when
    /// there is no prior identity to clear — first and later attempts share
    /// the same protocol. Only valid while the run is still implementing, and
    /// never on a finished run.
    pub(crate) fn invalidate_worker_group(&mut self) -> Result<()> {
        if matches!(self.manifest.lifecycle, RunLifecycle::Finished) {
            return Err(RunError::new(
                "cannot invalidate a worker group on a finished run",
            ));
        }
        let work = self.work_mut("invalidating a worker group")?;
        if work.stage != WorkStage::Implementing {
            return Err(RunError::new(
                "worker group can only be invalidated while implementing",
            ));
        }
        work.worker_pgid = None;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()
    }

    /// Durably binds this run to the process group of a just-spawned worker so
    /// stale-claim recovery can later prove that worker (and every descendant
    /// still in its group) is gone before reclaiming the bd claim. Called once
    /// per worker attempt, immediately after the spawn and before the worker
    /// can meaningfully mutate the repository, and only after
    /// [`RunHandle::invalidate_worker_group`] has already cleared any prior
    /// attempt's identity — it overwrites the `None` that invalidation just
    /// persisted so the recorded identity always tracks the latest live
    /// worker. Only valid while the run is still implementing — a worker is
    /// only ever spawned in that stage — and never on a finished run.
    pub(crate) fn record_worker_group(&mut self, pgid: u32) -> Result<()> {
        if matches!(self.manifest.lifecycle, RunLifecycle::Finished) {
            return Err(RunError::new(
                "cannot record a worker group on a finished run",
            ));
        }
        let work = self.work_mut("recording a worker group")?;
        if work.stage != WorkStage::Implementing {
            return Err(RunError::new(
                "worker group can only be recorded while implementing",
            ));
        }
        work.worker_pgid = Some(pgid);
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()
    }

    pub(crate) fn work(&self) -> Option<&WorkState> {
        work_state(&self.manifest)
    }

    fn work_mut(&mut self, operation: &str) -> Result<&mut WorkState> {
        match &mut self.manifest.details {
            RunDetails::Work { state: Some(work) } => Ok(work),
            RunDetails::Work { state: None } => {
                Err(RunError::new(format!("{operation} requires work state")))
            }
            _ => Err(RunError::new(format!("{operation} requires a work run"))),
        }
    }

    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    pub(crate) fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn approval(&self) -> Result<serde_json::Value> {
        let path = self.dir.join("approval.json");
        let bytes = std::fs::read(&path).map_err(|error| {
            RunError::new(format!(
                "failed to read approval {}: {error}",
                path.display()
            ))
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            RunError::new(format!(
                "failed to parse approval {}: {error}",
                path.display()
            ))
        })
    }

    /// Persists the exact post-terminal Bead mutation before the transition is
    /// attached to the terminal event. Repeating the same intent after a crash
    /// is harmless; a different intent fails closed.
    pub(crate) fn write_terminal_transition(
        &self,
        transition: &TerminalTransition,
    ) -> Result<ArtifactRef> {
        if matches!(self.manifest.lifecycle, RunLifecycle::Finished) {
            return Err(RunError::new(
                "cannot write a terminal transition on a finished run",
            ));
        }
        validate_terminal_transition(transition)?;
        let mut bytes = serde_json::to_vec_pretty(transition).map_err(|error| {
            RunError::new(format!("failed to serialize terminal transition: {error}"))
        })?;
        bytes.push(b'\n');
        let relative = Path::new(TERMINAL_TRANSITION_PATH);
        let path = self.dir.join(relative);
        match std::fs::read(&path) {
            Ok(existing) if existing == bytes => Ok(artifact_ref(relative, &bytes)),
            Ok(_) => Err(RunError::new(
                "terminal transition already exists with different contents",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_new_file(&path, &bytes)?;
                Ok(artifact_ref(relative, &bytes))
            }
            Err(error) => Err(RunError::new(format!(
                "failed to read terminal transition {}: {error}",
                path.display()
            ))),
        }
    }

    /// Reads the content-addressed Bead transition pinned to this run's final
    /// event. An unreferenced transition file is deliberately ignored: it was
    /// not made terminal evidence before the process stopped.
    pub(crate) fn terminal_transition(&self) -> Result<Option<TerminalTransition>> {
        if !matches!(self.manifest.lifecycle, RunLifecycle::Finished) {
            return Ok(None);
        }
        let events = read_events(&self.events_path())?;
        let terminal = events
            .last()
            .filter(|event| event.kind == EventKind::RunFinished)
            .ok_or_else(|| RunError::new("finished run has no terminal event"))?;
        validate_terminal_event(terminal)?;
        if !terminal
            .artifact_refs
            .iter()
            .any(|artifact| artifact.path == TERMINAL_TRANSITION_PATH)
        {
            return Ok(None);
        }
        let path = self.dir.join(TERMINAL_TRANSITION_PATH);
        let bytes = std::fs::read(&path).map_err(|error| {
            RunError::new(format!(
                "failed to read terminal transition {}: {error}",
                path.display()
            ))
        })?;
        let transition = serde_json::from_slice(&bytes).map_err(|error| {
            RunError::new(format!(
                "failed to parse terminal transition {}: {error}",
                path.display()
            ))
        })?;
        validate_terminal_transition(&transition)?;
        Ok(Some(transition))
    }

    /// Copies an existing output into this run using create-new semantics and
    /// returns its content-addressed identity.
    pub(crate) fn capture_artifact(
        &self,
        source: &Path,
        relative_destination: &Path,
    ) -> Result<ArtifactRef> {
        validate_relative_artifact_path(relative_destination)?;
        let bytes = std::fs::read(source).map_err(|error| {
            RunError::new(format!(
                "failed to read artifact {}: {error}",
                source.display()
            ))
        })?;
        let destination = self.dir.join(relative_destination);
        write_new_file(&destination, &bytes)?;
        Ok(artifact_ref(relative_destination, &bytes))
    }

    /// Appends one stable-schema event and updates the manifest's lifecycle,
    /// `updated_at`, and (for `run_finished`) final `outcome`.
    pub(crate) fn append_event(&mut self, kind: EventKind, input: EventInput) -> Result<()> {
        self.append_event_at(kind, input, Utc::now())
    }

    /// Durably advances a work run to the pending-review boundary after its
    /// exact commit and immutable verifier artifacts are known.
    pub(crate) fn checkpoint_pending_review(
        &mut self,
        worker_profile: &str,
        worker_commit: &str,
        verifier_command: &str,
        artifact_refs: Vec<ArtifactRef>,
    ) -> Result<()> {
        validate_commit_id(worker_commit)?;
        if worker_profile.trim().is_empty() {
            return Err(RunError::new("worker profile must not be empty"));
        }
        if self.manifest.verifier.mechanical.as_deref() != Some(verifier_command) {
            return Err(RunError::new(
                "mechanical verifier command does not match run manifest",
            ));
        }
        if artifact_refs.is_empty() {
            return Err(RunError::new(
                "pending review requires mechanical verifier artifacts",
            ));
        }
        let work = self.work_mut("pending review")?;
        if work.stage != WorkStage::Implementing
            || work.worker_commit.is_some()
            || work.mechanical.is_some()
        {
            return Err(RunError::new(
                "work run is not at the implementing checkpoint",
            ));
        }
        work.worker_profile = Some(worker_profile.to_string());
        work.worker_commit = Some(worker_commit.to_string());
        work.mechanical = Some(MechanicalVerification {
            command: verifier_command.to_string(),
            passed: true,
            artifact_refs: artifact_refs.clone(),
        });
        work.stage = WorkStage::PendingReview;
        for artifact in &artifact_refs {
            if !self.manifest.artifacts.contains(artifact) {
                self.manifest.artifacts.push(artifact.clone());
            }
        }
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.append_event(
            EventKind::VerifyFinished,
            EventInput {
                artifact_refs,
                outcome: Some("passed".to_string()),
                ..EventInput::default()
            },
        )
    }

    /// Returns the immutable and mutable structural state of a native plan.
    pub(crate) fn plan(&self) -> Result<&PlanRunDetails> {
        match &self.manifest.details {
            RunDetails::Plan { state } => Ok(state),
            _ => Err(RunError::new("operation requires a plan run")),
        }
    }

    fn plan_mut(&mut self, operation: &str) -> Result<&mut PlanRunDetails> {
        match &mut self.manifest.details {
            RunDetails::Plan { state } => Ok(state),
            _ => Err(RunError::new(format!("{operation} requires a plan run"))),
        }
    }

    /// Copies generated plan bytes into the run exactly once and returns their
    /// content-addressed identity.
    pub(crate) fn capture_plan_artifact(
        &self,
        relative_destination: &Path,
        bytes: &[u8],
    ) -> Result<ArtifactRef> {
        validate_relative_artifact_path(relative_destination)?;
        let destination = self.dir.join(relative_destination);
        match std::fs::read(&destination) {
            Ok(existing) if existing == bytes => Ok(artifact_ref(relative_destination, bytes)),
            Ok(_) => Err(RunError::new(
                "plan artifact already exists with different bytes",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_new_file(&destination, bytes)?;
                Ok(artifact_ref(relative_destination, bytes))
            }
            Err(error) => Err(RunError::new(format!(
                "failed to read plan artifact {}: {error}",
                destination.display()
            ))),
        }
    }

    /// Durably records the selected immutable author before an invocation.
    pub(crate) fn start_plan_authoring(&mut self, author: ApprovedExecution) -> Result<()> {
        let attempt_limit = self.plan()?.stage_attempt_limit;
        self.plan_mut("starting plan authoring")?
            .progress
            .start_authoring(author.clone(), attempt_limit)?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.append_event(
            EventKind::AttemptStarted,
            EventInput {
                profile_id: Some(author.profile_id),
                outcome: Some("planner_authoring".to_string()),
                ..EventInput::default()
            },
        )
    }

    pub(crate) fn replace_plan_author_before_artifact(
        &mut self,
        author: ApprovedExecution,
    ) -> Result<()> {
        self.plan_mut("replacing unavailable plan author")?
            .progress
            .replace_author_before_artifact(author.clone())?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.append_event(
            EventKind::AttemptStarted,
            EventInput {
                profile_id: Some(author.profile_id),
                outcome: Some("planner_fallback".to_string()),
                ..EventInput::default()
            },
        )
    }

    pub(crate) fn block_plan_before_authoring(&mut self) -> Result<()> {
        self.plan_mut("blocking unstarted plan")?
            .progress
            .block_before_authoring()?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.append_event(
            EventKind::AttemptFinished,
            EventInput {
                outcome: Some("planner_unavailable_before_start".to_string()),
                ..EventInput::default()
            },
        )
    }

    pub(crate) fn finish_plan_blocked(&mut self) -> Result<()> {
        if !matches!(self.plan()?.progress, PlanProgress::Authoring { .. }) {
            return Err(RunError::new(
                "terminal plan block requires active authoring",
            ));
        }
        self.plan_mut("blocking started plan")?.progress = PlanProgress::Terminal {
            verdict: PlanTerminalVerdict::Blocked,
        };
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.finish("blocked")
    }

    /// Persists each author invocation before the harness starts, so a crash
    /// can resume only within the approved bounded attempt budget.
    pub(crate) fn record_plan_author_attempt(&mut self) -> Result<()> {
        let attempt_limit = self.plan()?.stage_attempt_limit;
        self.plan_mut("recording plan author attempt")?
            .progress
            .record_author_attempt(attempt_limit)?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()
    }

    /// Atomically checkpoints a validated author artifact at the structural
    /// peer boundary. Peer verdict execution is intentionally a later phase.
    pub(crate) fn await_plan_peer(&mut self, artifact: ArtifactRef) -> Result<()> {
        validate_artifact_ref(&artifact, "plan author artifact")?;
        validate_local_artifact(&self.manifest_path(), &artifact)?;
        self.plan_mut("awaiting plan peer")?
            .progress
            .await_peer(artifact.clone())?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.append_event(
            EventKind::AttemptFinished,
            EventInput {
                artifact_refs: vec![artifact],
                outcome: Some("plan_document_ready".to_string()),
                ..EventInput::default()
            },
        )
    }

    /// Ends an unstarted or explicitly cancellable blocked plan without
    /// rewinding its scheduler rotation.
    pub(crate) fn cancel_prepared_plan(&mut self) -> Result<()> {
        if !matches!(
            self.plan()?.progress,
            PlanProgress::Prepared | PlanProgress::Blocked { cancellable: true }
        ) {
            return Err(RunError::new(
                "plan cancellation is allowed only before authoring begins",
            ));
        }
        self.plan_mut("canceling plan")?.progress = PlanProgress::Terminal {
            verdict: PlanTerminalVerdict::Blocked,
        };
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.finish("canceled")
    }

    /// Marks a plan document with unresolved author questions as terminal
    /// needs-input rather than allowing a schema-shaped artifact to proceed.
    pub(crate) fn finish_plan_needs_input(&mut self, artifact: ArtifactRef) -> Result<()> {
        if !matches!(self.plan()?.progress, PlanProgress::Authoring { .. }) {
            return Err(RunError::new(
                "needs-input plan completion requires active authoring",
            ));
        }
        self.plan_mut("finishing plan needs input")?.progress = PlanProgress::Terminal {
            verdict: PlanTerminalVerdict::NeedsInput,
        };
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.finish_with_artifacts("needs_input", vec![artifact])
    }

    /// Records a policy-approved fresh budget before resuming a verifier or
    /// review. The manifest write is deliberately before any resumed spawn.
    pub(crate) fn record_review_resume_budget(&mut self, budget: Duration) -> Result<()> {
        let seconds = budget.as_secs();
        if seconds == 0 {
            return Err(RunError::new(
                "review resume budget must include at least one whole second",
            ));
        }
        let work = self.work_mut("recording review resume budget")?;
        if !matches!(
            work.stage,
            WorkStage::Implementing | WorkStage::PendingReview
        ) {
            return Err(RunError::new(
                "review resume budget requires resumable work",
            ));
        }
        work.review_resume_budget_secs = Some(seconds);
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()
    }

    /// Repairs the event-journal half of a checkpoint if the process stopped
    /// after the atomic manifest replace but before the event append.
    pub(crate) fn ensure_pending_review_event(&mut self) -> Result<()> {
        let Some(work) = self.work() else {
            return Err(RunError::new("pending review requires work state"));
        };
        if work.stage != WorkStage::PendingReview {
            return Err(RunError::new("work run is not pending review"));
        }
        let mechanical = work
            .mechanical
            .as_ref()
            .ok_or_else(|| RunError::new("pending review has no mechanical evidence"))?;
        let events = read_events(&self.events_path())?;
        let verify_events = events
            .iter()
            .filter(|event| event.kind == EventKind::VerifyFinished)
            .collect::<Vec<_>>();
        if verify_events.len() == 1 {
            return Ok(());
        }
        if !verify_events.is_empty() {
            return Err(RunError::new(
                "pending-review event log has duplicate verifier events",
            ));
        }
        let artifact_refs = mechanical.artifact_refs.clone();
        self.append_event(
            EventKind::VerifyFinished,
            EventInput {
                artifact_refs,
                outcome: Some("passed".to_string()),
                ..EventInput::default()
            },
        )
    }

    fn append_event_at(
        &mut self,
        kind: EventKind,
        input: EventInput,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if matches!(self.manifest.lifecycle, RunLifecycle::Finished) {
            return Err(RunError::new("cannot append to a finished run"));
        }
        for artifact in &input.artifact_refs {
            validate_artifact_ref(artifact, "event artifact")?;
        }
        let seq = self.next_seq;
        let event = RunEvent {
            schema: EVENT_SCHEMA.to_string(),
            event_id: format!("{}-{seq:06}", self.manifest.run_id),
            run_id: self.manifest.run_id.clone(),
            seq,
            ts: now.to_rfc3339(),
            kind,
            job: self.manifest.job,
            profile_id: input.profile_id,
            target: self.manifest.target.clone(),
            artifact_refs: input.artifact_refs,
            outcome: input.outcome.clone(),
            provider_limit: input.provider_limit,
        };
        append_event_line(&self.events_path(), &event)?;
        self.next_seq += 1;

        for artifact in &event.artifact_refs {
            if !self.manifest.artifacts.contains(artifact) {
                self.manifest.artifacts.push(artifact.clone());
            }
        }

        if matches!(kind, EventKind::RunFinished) {
            self.manifest.lifecycle = RunLifecycle::Finished;
            self.manifest.outcome = input.outcome;
        } else if matches!(self.manifest.lifecycle, RunLifecycle::Started) {
            self.manifest.lifecycle = RunLifecycle::Running;
        }
        self.manifest.updated_at = now.to_rfc3339();
        self.write_manifest()
    }

    /// Records the terminal `run_finished` event and pins the final outcome.
    pub(crate) fn finish(&mut self, outcome: impl Into<String>) -> Result<()> {
        self.finish_with_artifacts(outcome, Vec::new())
    }

    pub(crate) fn finish_with_artifacts(
        &mut self,
        outcome: impl Into<String>,
        artifact_refs: Vec<ArtifactRef>,
    ) -> Result<()> {
        if let Ok(work) = self.work_mut("finishing") {
            work.stage = WorkStage::Completed;
        }
        self.append_event(
            EventKind::RunFinished,
            EventInput {
                outcome: Some(outcome.into()),
                artifact_refs,
                profile_id: None,
                provider_limit: None,
            },
        )
    }

    /// Touches this run's heartbeat file, recording that the `conductor`
    /// process driving it is still alive. Read back by
    /// [`find_implementing_work_run`] staleness checks so a `dispatch
    /// --resume` invocation can tell an actively-running worker from one
    /// whose owning process died (e.g. `kill -9`) without releasing its bd
    /// claim.
    pub(crate) fn touch_heartbeat(&self) -> Result<()> {
        atomic_replace(
            &heartbeat_path(&self.dir),
            Utc::now().to_rfc3339().as_bytes(),
        )
    }

    /// Returns this run's last observed sign of life: its heartbeat file if
    /// one has been recorded, otherwise the manifest's `updated_at` (the
    /// process may have died before its first heartbeat tick).
    pub(crate) fn last_seen(&self) -> Result<DateTime<Utc>> {
        if let Some(heartbeat) = read_heartbeat(&self.dir)? {
            return Ok(heartbeat);
        }
        parse_rfc3339(&self.manifest.updated_at, "run updated_at")
    }

    fn write_approval(&self, approval: &serde_json::Value) -> Result<ArtifactRef> {
        let mut bytes = serde_json::to_vec_pretty(approval)
            .map_err(|error| RunError::new(format!("failed to serialize approval: {error}")))?;
        bytes.push(b'\n');
        let relative = Path::new("approval.json");
        write_new_file(&self.dir.join(relative), &bytes)?;
        Ok(artifact_ref(relative, &bytes))
    }

    fn write_manifest(&self) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(&self.manifest)
            .map_err(|e| RunError::new(format!("failed to serialize run manifest: {e}")))?;
        bytes.push(b'\n');
        atomic_replace(&self.manifest_path(), &bytes)
    }
}

fn validate_terminal_event(event: &RunEvent) -> Result<()> {
    let outcome = event
        .outcome
        .as_deref()
        .filter(|outcome| !outcome.trim().is_empty())
        .ok_or_else(|| RunError::new("terminal event has no outcome"))?;
    if outcome != outcome.trim() {
        return Err(RunError::new(
            "terminal event outcome has surrounding whitespace",
        ));
    }
    if event.profile_id.is_some() {
        return Err(RunError::new("terminal event must not name a profile"));
    }
    Ok(())
}

fn validate_terminal_transition(transition: &TerminalTransition) -> Result<()> {
    if transition.reason.trim().is_empty() || transition.reason != transition.reason.trim() {
        return Err(RunError::new(
            "terminal transition must have a non-empty trimmed reason",
        ));
    }
    if let Some(metadata) = transition.metadata.as_ref() {
        if metadata.key.trim().is_empty()
            || metadata.value.trim().is_empty()
            || metadata.key != metadata.key.trim()
        {
            return Err(RunError::new(
                "terminal transition metadata must have a non-empty key and value",
            ));
        }
    }
    if let Some(comment) = transition.comment.as_deref() {
        if comment.trim().is_empty() {
            return Err(RunError::new(
                "terminal transition comment must not be empty",
            ));
        }
    }
    Ok(())
}

fn reconcile_terminal_manifest(
    dir: &Path,
    manifest: &mut RunManifest,
    terminal: &RunEvent,
) -> Result<()> {
    if manifest.outcome.is_some() && manifest.outcome != terminal.outcome {
        return Err(RunError::new(
            "unfinished manifest outcome conflicts with terminal event outcome",
        ));
    }
    if let RunDetails::Work { state: Some(work) } = &mut manifest.details {
        work.stage = WorkStage::Completed;
    }
    for artifact in &terminal.artifact_refs {
        if !manifest.artifacts.contains(artifact) {
            manifest.artifacts.push(artifact.clone());
        }
    }
    manifest.lifecycle = RunLifecycle::Finished;
    manifest.outcome.clone_from(&terminal.outcome);
    manifest.updated_at = Utc::now().to_rfc3339();
    let mut bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        RunError::new(format!("failed to serialize recovered manifest: {error}"))
    })?;
    bytes.push(b'\n');
    atomic_replace(&dir.join("manifest.json"), &bytes)
}

fn work_state(manifest: &RunManifest) -> Option<&WorkState> {
    manifest.work()
}

/// Classifies legacy v1 artifacts without interpreting them as active runs.
/// Missing legacy storage is the normal, ready-to-activate case.
pub(crate) fn legacy_v1_preflight(state_dir: &Path) -> Result<LegacyV1Preflight> {
    let legacy_dir = state_dir.join("runs");
    let entries = match std::fs::read_dir(&legacy_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LegacyV1Preflight::default());
        }
        Err(error) => {
            return Err(RunError::new(format!(
                "failed to inspect legacy runs {}: {error}",
                legacy_dir.display()
            )));
        }
    };
    let mut result = LegacyV1Preflight::default();
    for entry in entries {
        let entry = entry.map_err(|error| {
            RunError::new(format!("failed to inspect legacy run entry: {error}"))
        })?;
        if !entry
            .file_type()
            .map_err(|error| RunError::new(format!("failed to stat legacy run entry: {error}")))?
            .is_dir()
        {
            continue;
        }
        let manifest = entry.path().join("manifest.json");
        let Ok(bytes) = std::fs::read(&manifest) else {
            result.reclaimable += 1;
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            result.reclaimable += 1;
            continue;
        };
        if value.get("schema").and_then(serde_json::Value::as_str) != Some("conductor/run@1") {
            continue;
        }
        if value.get("lifecycle").and_then(serde_json::Value::as_str) == Some("finished") {
            continue;
        }
        match value
            .get("work")
            .and_then(|work| work.get("stage"))
            .and_then(serde_json::Value::as_str)
        {
            Some("pending_review") => result.pending += 1,
            Some("implementing") => result.implementing += 1,
            _ => result.reclaimable += 1,
        }
    }
    Ok(result)
}

fn require_v2_activation_preflight(state_dir: &Path) -> Result<()> {
    let legacy = legacy_v1_preflight(state_dir)?;
    if legacy.activation_allowed() {
        return Ok(());
    }
    Err(RunError::new(format!(
        "v2 activation blocked by legacy recovery: pending={}, implementing={}, reclaimable={}",
        legacy.pending, legacy.implementing, legacy.reclaimable
    )))
}

/// Returns `<state_dir>/runs-v2`, the sole active run namespace.
pub(crate) fn runs_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("runs-v2")
}

fn heartbeat_path(run_dir: &Path) -> PathBuf {
    run_dir.join("heartbeat")
}

/// Reads a run's heartbeat file, if one has been recorded.
pub(crate) fn read_heartbeat(run_dir: &Path) -> Result<Option<DateTime<Utc>>> {
    match std::fs::read_to_string(heartbeat_path(run_dir)) {
        Ok(contents) => Ok(Some(parse_rfc3339(contents.trim(), "heartbeat")?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(RunError::new(format!(
            "failed to read heartbeat {}: {error}",
            heartbeat_path(run_dir).display()
        ))),
    }
}

fn parse_rfc3339(value: &str, label: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|error| RunError::new(format!("malformed {label} timestamp {value:?}: {error}")))
}

/// Reads and validates `manifest.json`, rejecting an unknown schema before
/// attempting to interpret the rest of the shape (a future/foreign schema
/// version may not share this struct's fields at all).
pub(crate) fn read_manifest(path: &Path) -> Result<RunManifest> {
    let bytes = std::fs::read(path)
        .map_err(|e| RunError::new(format!("failed to read manifest {}: {e}", path.display())))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| RunError::new(format!("failed to parse manifest {}: {e}", path.display())))?;
    check_schema(&value, RUN_SCHEMA, path)?;
    let manifest: RunManifest = serde_json::from_value(value)
        .map_err(|e| RunError::new(format!("failed to parse manifest {}: {e}", path.display())))?;
    validate_run_id(&manifest.run_id)?;
    if let Some(artifact) = manifest.bursar_roster_artifact.as_ref() {
        validate_artifact_ref(artifact, "manifest bursar roster artifact")?;
    }
    if let Some(snapshot) = manifest.roster_snapshot.as_ref() {
        validate_roster_snapshot(path, snapshot, manifest.roster_policy_sha256.as_deref())?;
    } else if manifest.roster_policy_sha256.is_some() {
        return Err(RunError::new(
            "manifest has roster policy_sha256 without copied roster snapshot",
        ));
    }
    validate_run_details(path, &manifest)?;
    for artifact in &manifest.artifacts {
        validate_artifact_ref(artifact, "manifest artifact")?;
        validate_local_artifact(path, artifact)?;
    }
    validate_work_manifest(path, &manifest)?;
    Ok(manifest)
}

/// Finds the one unfinished work run waiting for qualitative review for an
/// exact approved cycle/repository/Bead identity.
pub(crate) fn find_pending_work_run(
    state_dir: &Path,
    cycle_id: &str,
    repo: &str,
    bead: &str,
) -> Result<Option<String>> {
    PendingWorkIndex::scan(state_dir)?.find_pending_work_run(cycle_id, repo, bead)
}

/// A single, untrusted manifest pass over the active run namespace for one
/// dispatch cycle. It filters only likely pending-review candidates; every
/// selected candidate is still fully authenticated before it can be resumed.
#[derive(Debug, Default)]
pub(crate) struct PendingWorkIndex {
    candidates: BTreeMap<PendingWorkKey, Vec<PathBuf>>,
    malformed: BTreeMap<PendingWorkKey, Vec<PathBuf>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PendingWorkKey {
    cycle_id: String,
    repo: String,
    bead: String,
}

impl PendingWorkKey {
    fn new(cycle_id: &str, repo: &str, bead: &str) -> Self {
        Self {
            cycle_id: cycle_id.to_string(),
            repo: repo.to_string(),
            bead: bead.to_string(),
        }
    }
}

impl PendingWorkIndex {
    /// Reads each lightweight manifest once. This intentionally does not
    /// validate artifact hashes: those can be large and are authentication
    /// work reserved for candidates matching an approved target.
    pub(crate) fn scan(state_dir: &Path) -> Result<Self> {
        let root = runs_dir(state_dir);
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(RunError::new(format!(
                    "failed to read runs dir {}: {error}",
                    root.display()
                )));
            }
        };
        let mut run_dirs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                RunError::new(format!("failed to read run directory entry: {error}"))
            })?;
            if entry
                .file_type()
                .map_err(|error| RunError::new(format!("failed to stat run entry: {error}")))?
                .is_dir()
            {
                run_dirs.push(entry.path());
            }
        }
        run_dirs.sort();

        let mut index = Self::default();
        for run_dir in run_dirs {
            let path = run_dir.join("manifest.json");
            let Ok(bytes) = read_discovery_manifest(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                if let Some(key) = partial_pending_work_key(&bytes) {
                    index.malformed.entry(key).or_default().push(path);
                }
                continue;
            };
            let Some(key) = pending_work_key_from_untrusted_manifest(&value) else {
                continue;
            };
            index.candidates.entry(key).or_default().push(path);
        }
        for paths in index
            .candidates
            .values_mut()
            .chain(index.malformed.values_mut())
        {
            paths.sort();
        }
        Ok(index)
    }

    /// Authenticates all candidates for one exact target. The index evidence
    /// only chooses what to authenticate; it is never sufficient to resume.
    pub(crate) fn find_pending_work_run(
        &self,
        cycle_id: &str,
        repo: &str,
        bead: &str,
    ) -> Result<Option<String>> {
        let key = PendingWorkKey::new(cycle_id, repo, bead);
        if let Some(paths) = self.malformed.get(&key) {
            let paths = paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            return Err(RunError::new(format!(
                "pending-review evidence for {cycle_id} {repo}/{bead} is malformed: {}",
                paths.join(", ")
            )));
        }

        let Some(paths) = self.candidates.get(&key) else {
            return Ok(None);
        };
        let mut run_ids = Vec::with_capacity(paths.len());
        for path in paths {
            let manifest = read_manifest(path).map_err(|error| {
                RunError::new(format!(
                    "pending-review candidate {} failed authentication: {}",
                    path.display(),
                    error.into_message()
                ))
            })?;
            if pending_work_key_from_manifest(&manifest).as_ref() != Some(&key) {
                return Err(RunError::new(format!(
                    "pending-review candidate {} changed identity after discovery",
                    path.display()
                )));
            }
            run_ids.push(manifest.run_id);
        }
        run_ids.sort();
        if run_ids.len() > 1 {
            return Err(RunError::new(format!(
                "multiple pending-review runs found for {cycle_id} {repo}/{bead}: {}",
                run_ids.join(", ")
            )));
        }
        Ok(run_ids.pop())
    }
}

fn pending_work_key_from_manifest(manifest: &RunManifest) -> Option<PendingWorkKey> {
    let work = work_state(manifest)?;
    (manifest.job == RunJob::Work
        && manifest.lifecycle != RunLifecycle::Finished
        && work.stage == WorkStage::PendingReview)
        .then(|| {
            PendingWorkKey::new(
                &work.cycle_id,
                &manifest.target.repo,
                manifest.target.bead.as_deref().unwrap_or_default(),
            )
        })
}

fn pending_work_key_from_untrusted_manifest(value: &serde_json::Value) -> Option<PendingWorkKey> {
    let target = value.get("target")?;
    let repo = target.get("repo")?.as_str()?;
    let bead = target.get("bead")?.as_str()?;
    let details = value.get("details")?;
    let state = details.get("state")?;
    let cycle_id = state.get("cycle_id")?.as_str()?;
    (value.get("job")?.as_str() == Some("work")
        && details.get("job")?.as_str() == Some("work")
        && value.get("lifecycle")?.as_str() != Some("finished")
        && state.get("stage")?.as_str() == Some("pending_review"))
    .then(|| PendingWorkKey::new(cycle_id, repo, bead))
}

const DISCOVERY_MANIFEST_MAX_BYTES: u64 = 128 * 1024;

fn read_discovery_manifest(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(DISCOVERY_MANIFEST_MAX_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[derive(Default)]
struct PartialPendingWorkKey {
    cycle_id: Option<String>,
    repo: Option<String>,
    bead: Option<String>,
}

impl PartialPendingWorkKey {
    fn into_key(self) -> Option<PendingWorkKey> {
        Some(PendingWorkKey {
            cycle_id: self.cycle_id?,
            repo: self.repo?,
            bead: self.bead?,
        })
    }
}

#[derive(Clone, Copy)]
enum PartialScope {
    Root,
    Target,
    Details,
    WorkState,
    Other,
}

fn partial_pending_work_key(bytes: &[u8]) -> Option<PendingWorkKey> {
    let mut parser = PartialJsonParser { bytes, position: 0 };
    let mut fields = PartialPendingWorkKey::default();
    let _ = parser.parse_object(PartialScope::Root, &mut fields);
    fields.into_key()
}

struct PartialJsonParser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl PartialJsonParser<'_> {
    fn parse_object(
        &mut self,
        scope: PartialScope,
        fields: &mut PartialPendingWorkKey,
    ) -> Option<()> {
        self.expect(b'{')?;
        loop {
            self.skip_whitespace();
            if self.consume(b'}') {
                return Some(());
            }
            let key = self.parse_string()?;
            self.expect(b':')?;
            if let Some(child_scope) = partial_child_scope(scope, &key) {
                self.parse_object(child_scope, fields)?;
            } else {
                self.parse_field(scope, &key, fields)?;
            }
            self.skip_whitespace();
            if self.consume(b'}') {
                return Some(());
            }
            self.expect(b',')?;
        }
    }

    fn parse_field(
        &mut self,
        scope: PartialScope,
        key: &str,
        fields: &mut PartialPendingWorkKey,
    ) -> Option<()> {
        let value = match (scope, key) {
            (PartialScope::Target, "repo" | "bead") | (PartialScope::WorkState, "cycle_id") => {
                self.parse_string()?
            }
            _ => return self.skip_value(fields),
        };
        match (scope, key) {
            (PartialScope::Target, "repo") => fields.repo = Some(value),
            (PartialScope::Target, "bead") => fields.bead = Some(value),
            (PartialScope::WorkState, "cycle_id") => fields.cycle_id = Some(value),
            _ => {}
        }
        Some(())
    }

    fn skip_value(&mut self, fields: &mut PartialPendingWorkKey) -> Option<()> {
        self.skip_whitespace();
        match self.peek()? {
            b'"' => {
                self.parse_string()?;
                Some(())
            }
            b'{' => self.parse_object(PartialScope::Other, fields),
            b'[' => self.skip_array(fields),
            _ => {
                let start = self.position;
                while let Some(byte) = self.peek() {
                    if matches!(byte, b',' | b']' | b'}') || byte.is_ascii_whitespace() {
                        break;
                    }
                    self.position += 1;
                }
                (self.position > start).then_some(())
            }
        }
    }

    fn skip_array(&mut self, fields: &mut PartialPendingWorkKey) -> Option<()> {
        self.expect(b'[')?;
        loop {
            self.skip_whitespace();
            if self.consume(b']') {
                return Some(());
            }
            self.skip_value(fields)?;
            self.skip_whitespace();
            if self.consume(b']') {
                return Some(());
            }
            self.expect(b',')?;
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        self.skip_whitespace();
        let start = self.position;
        self.expect(b'"')?;
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.position += 1;
                    return serde_json::from_slice(&self.bytes[start..self.position]).ok();
                }
                b'\\' => {
                    self.position += 1;
                    self.position += usize::from(self.peek().is_some());
                }
                _ => self.position += 1,
            }
        }
        None
    }

    fn expect(&mut self, expected: u8) -> Option<()> {
        self.skip_whitespace();
        self.consume(expected).then_some(())
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }
}

fn partial_child_scope(scope: PartialScope, key: &str) -> Option<PartialScope> {
    match (scope, key) {
        (PartialScope::Root, "target") => Some(PartialScope::Target),
        (PartialScope::Root, "details") => Some(PartialScope::Details),
        (PartialScope::Details, "state") => Some(PartialScope::WorkState),
        _ => None,
    }
}

/// Finds the one unfinished work run still mid-implementation for an exact
/// approved cycle/repository/Bead identity — a stale-claim reclaim candidate
/// for `dispatch --resume` when its heartbeat has gone quiet.
pub(crate) fn find_implementing_work_run(
    state_dir: &Path,
    cycle_id: &str,
    repo: &str,
    bead: &str,
) -> Result<Option<String>> {
    find_work_run_at_stage(state_dir, cycle_id, repo, bead, WorkStage::Implementing)
}

/// The stale-claim reclaim target selected by [`find_reclaimable_work_run`]
/// from a target's full generation history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReclaimCandidate {
    /// The one unfinished Work run for the target — the current generation,
    /// still mid-implementation (or otherwise not yet finished). Its liveness
    /// and HEAD must be revalidated before it can be reclaimed.
    Unfinished(String),
    /// No unfinished run exists; this is the latest-generation *finished* run,
    /// retained as audit history. Consulted only to complete a bd release that
    /// a prior reclaim durably finished but crashed before releasing — and
    /// only when that run's exact outcome authorizes the transition.
    FinishedLatest(String),
}

/// Selects the stale-claim reclaim target for an exact approved
/// cycle/repository/Bead identity from that target's *full* generation
/// history. Repeated crashes accumulate one finished
/// `stale_claim_reaped` run per reap plus, at most, one still-unfinished
/// current generation; this never conflates them:
///
/// - Exactly one unfinished run (any stage) ⇒ [`ReclaimCandidate::Unfinished`]
///   — the current generation to revalidate and possibly reap. Older finished
///   generations are ignored, not counted, so an arbitrarily long crash
///   history stays recoverable while its audit trail is preserved.
/// - No unfinished run but some finished history ⇒
///   [`ReclaimCandidate::FinishedLatest`] with the newest-generation finished
///   run — the only run whose release a crash-after-finish could still owe.
/// - More than one unfinished run ⇒ an invariant violation (a fresh run is
///   only ever created after its predecessor was durably finished), so this
///   fails closed with an error rather than guessing which is current.
pub(crate) fn find_reclaimable_work_run(
    state_dir: &Path,
    cycle_id: &str,
    repo: &str,
    bead: &str,
) -> Result<Option<ReclaimCandidate>> {
    let root = runs_dir(state_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RunError::new(format!(
                "failed to read runs dir {}: {error}",
                root.display()
            )));
        }
    };
    let mut unfinished = Vec::new();
    // (created_at, run_id) of every finished generation, so the newest can be
    // chosen deterministically for a release-retry.
    let mut finished: Vec<(DateTime<Utc>, String)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            RunError::new(format!("failed to read run directory entry: {error}"))
        })?;
        if !entry
            .file_type()
            .map_err(|error| RunError::new(format!("failed to stat run entry: {error}")))?
            .is_dir()
        {
            continue;
        }
        let manifest = read_manifest(&entry.path().join("manifest.json"))?;
        let Some(work) = work_state(&manifest) else {
            continue;
        };
        if manifest.job != RunJob::Work
            || work.cycle_id != cycle_id
            || manifest.target.repo != repo
            || manifest.target.bead.as_deref() != Some(bead)
        {
            continue;
        }
        if manifest.lifecycle == RunLifecycle::Finished {
            let created_at = parse_rfc3339(&manifest.created_at, "run created_at")?;
            finished.push((created_at, manifest.run_id));
        } else {
            unfinished.push(manifest.run_id);
        }
    }
    if unfinished.len() > 1 {
        unfinished.sort();

        return Err(RunError::new(format!(
            "multiple unfinished work runs found for {cycle_id} {repo}/{bead}: {}",
            unfinished.join(", ")
        )));
    }
    if let Some(run_id) = unfinished.pop() {
        return Ok(Some(ReclaimCandidate::Unfinished(run_id)));
    }
    // Newest generation wins; the run_id tie-breaks equal timestamps (it
    // collision-resistant).
    finished.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(finished
        .pop()
        .map(|(_, run_id)| ReclaimCandidate::FinishedLatest(run_id)))
}
fn validate_run_details(path: &Path, manifest: &RunManifest) -> Result<()> {
    match (&manifest.job, &manifest.details) {
        (RunJob::Work, RunDetails::Work { .. })
        | (RunJob::Review, RunDetails::Review { .. })
        | (RunJob::Consult, RunDetails::Consult { .. }) => {}
        (RunJob::Plan, RunDetails::Plan { state }) => validate_plan_details(path, state)?,
        _ => {
            return Err(RunError::new(
                "manifest job does not match tagged run details",
            ));
        }
    }
    Ok(())
}

fn validate_plan_details(path: &Path, plan: &PlanRunDetails) -> Result<()> {
    if !Path::new(&plan.target.repo).is_absolute() {
        return Err(RunError::new(
            "plan target repo must be canonical absolute path",
        ));
    }
    let target_artifact = match &plan.target.input {
        PlanInput::Bead {
            bead_id, artifact, ..
        } => {
            if !is_identifier(bead_id) {
                return Err(RunError::new("plan Bead target has invalid Bead id"));
            }
            artifact
        }
        PlanInput::Artifact { artifact, .. } => artifact,
    };
    validate_artifact_ref(target_artifact, "plan target artifact")?;
    validate_local_artifact(path, target_artifact)?;
    if plan.routes.stages.len() != 3 {
        return Err(RunError::new(
            "plan routes must contain planner, peer_review, and second_opinion",
        ));
    }
    let mut seen_stages = HashSet::new();
    let mut seen_executions = HashSet::new();
    for route in &plan.routes.stages {
        if !seen_stages.insert(route.stage)
            || route.capability_role.is_empty()
            || route.candidates.is_empty()
        {
            return Err(RunError::new("invalid immutable plan route"));
        }
        for candidate in &route.candidates {
            if !is_identifier(&candidate.profile_id)
                || !is_identifier(&candidate.provider_id)
                || !is_identifier(&candidate.availability_key)
                || candidate.execution_key.trim().is_empty()
            {
                return Err(RunError::new("invalid approved plan execution identity"));
            }
            if !seen_executions.insert((
                route.stage,
                candidate.profile_id.as_str(),
                candidate.execution_key.as_str(),
            )) {
                return Err(RunError::new("duplicate approved execution in plan route"));
            }
        }
    }
    if !seen_stages.contains(&PlanStage::Planner)
        || !seen_stages.contains(&PlanStage::PeerReview)
        || !seen_stages.contains(&PlanStage::SecondOpinion)
    {
        return Err(RunError::new("plan routes omit a required stage"));
    }
    RevisionLimit::new(plan.revision_limit.value())?;
    StageAttemptLimit::new(plan.stage_attempt_limit.value())?;
    validate_plan_progress(path, &plan.progress)
}

fn validate_plan_progress(path: &Path, progress: &PlanProgress) -> Result<()> {
    let artifact = match progress {
        PlanProgress::AwaitingPeer { artifact, .. }
        | PlanProgress::Revising { artifact, .. }
        | PlanProgress::AwaitingSecondOpinion { artifact, .. } => Some(artifact),
        PlanProgress::Prepared
        | PlanProgress::Blocked { .. }
        | PlanProgress::Authoring { .. }
        | PlanProgress::Terminal { .. } => None,
    };
    if let Some(artifact) = artifact {
        validate_artifact_ref(artifact, "plan progress artifact")?;
        validate_local_artifact(path, artifact)?;
    }
    Ok(())
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn find_work_run_at_stage(
    state_dir: &Path,
    cycle_id: &str,
    repo: &str,
    bead: &str,
    stage: WorkStage,
) -> Result<Option<String>> {
    let root = runs_dir(state_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RunError::new(format!(
                "failed to read runs dir {}: {error}",
                root.display()
            )));
        }
    };
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            RunError::new(format!("failed to read run directory entry: {error}"))
        })?;
        if !entry
            .file_type()
            .map_err(|error| RunError::new(format!("failed to stat run entry: {error}")))?
            .is_dir()
        {
            continue;
        }
        let manifest = read_manifest(&entry.path().join("manifest.json"))?;
        let Some(work) = work_state(&manifest) else {
            continue;
        };
        if manifest.job == RunJob::Work
            && manifest.lifecycle != RunLifecycle::Finished
            && work.stage == stage
            && work.cycle_id == cycle_id
            && manifest.target.repo == repo
            && manifest.target.bead.as_deref() == Some(bead)
        {
            matches.push(manifest.run_id);
        }
    }
    if matches.len() > 1 {
        return Err(RunError::new(format!(
            "multiple {stage:?} runs found for {cycle_id} {repo}/{bead}"
        )));
    }
    Ok(matches.pop())
}

fn check_schema(value: &serde_json::Value, expected: &str, path: &Path) -> Result<()> {
    let schema = value.get("schema").and_then(serde_json::Value::as_str);
    if schema != Some(expected) {
        return Err(RunError::new(format!(
            "unknown schema {:?} in {}, expected {expected:?}",
            schema.unwrap_or("<missing>"),
            path.display()
        )));
    }
    Ok(())
}

/// Reads and validates every line of `events.jsonl`, rejecting an unknown
/// schema or a malformed (e.g. partially written) line. Fails closed on the
/// first bad line rather than silently dropping it.
pub(crate) fn read_events(path: &Path) -> Result<Vec<RunEvent>> {
    let bytes = std::fs::read(path)
        .map_err(|e| RunError::new(format!("failed to read events {}: {e}", path.display())))?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(RunError::new(format!(
            "{}: malformed event (partial final line)",
            path.display()
        )));
    }
    let content = std::str::from_utf8(&bytes).map_err(|error| {
        RunError::new(format!(
            "{}: malformed event log encoding: {error}",
            path.display()
        ))
    })?;
    let mut events = Vec::new();
    let mut identity: Option<(String, RunJob, RunTarget)> = None;
    for (idx, line) in content.split_terminator('\n').enumerate() {
        if line.trim().is_empty() {
            return Err(RunError::new(format!(
                "{} line {}: blank event line",
                path.display(),
                idx + 1
            )));
        }
        let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            RunError::new(format!(
                "{} line {}: malformed event (partial write?): {e}",
                path.display(),
                idx + 1
            ))
        })?;
        check_schema(&value, EVENT_SCHEMA, path)
            .map_err(|e| RunError::new(format!("{e} (line {})", idx + 1)))?;
        let event: RunEvent = serde_json::from_value(value).map_err(|e| {
            RunError::new(format!(
                "{} line {}: malformed event (partial write?): {e}",
                path.display(),
                idx + 1
            ))
        })?;
        let expected_seq =
            u64::try_from(idx).map_err(|_| RunError::new("event sequence exceeds u64"))? + 1;
        if event.seq != expected_seq {
            return Err(RunError::new(format!(
                "{} line {}: event sequence gap, expected {expected_seq}, found {}",
                path.display(),
                idx + 1,
                event.seq
            )));
        }
        let expected_event_id = format!("{}-{expected_seq:06}", event.run_id);
        if event.event_id != expected_event_id {
            return Err(RunError::new(format!(
                "{} line {}: event_id {:?} does not match {:?}",
                path.display(),
                idx + 1,
                event.event_id,
                expected_event_id
            )));
        }
        validate_run_id(&event.run_id)?;
        for artifact in &event.artifact_refs {
            validate_artifact_ref(artifact, "event artifact")?;
            validate_local_artifact(path, artifact)?;
        }
        match &identity {
            None => {
                if !matches!(event.kind, EventKind::RunStarted) {
                    return Err(RunError::new(format!(
                        "{} line 1: first event must be run_started",
                        path.display()
                    )));
                }
                identity = Some((event.run_id.clone(), event.job, event.target.clone()));
            }
            Some((run_id, job, target)) => {
                if event.run_id != *run_id || event.job != *job || event.target != *target {
                    return Err(RunError::new(format!(
                        "{} line {}: event run_id/job/target identity mismatch",
                        path.display(),
                        idx + 1
                    )));
                }
            }
        }
        events.push(event);
    }
    Ok(events)
}

fn validate_run_id(run_id: &str) -> Result<()> {
    let mut components = Path::new(run_id).components();
    if run_id.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(RunError::new(format!("invalid run_id {run_id:?}")));
    }
    Ok(())
}

fn validate_artifact_ref(artifact: &ArtifactRef, label: &str) -> Result<()> {
    if artifact.path.trim().is_empty() {
        return Err(RunError::new(format!("{label} has an empty path")));
    }
    if artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RunError::new(format!(
            "{label} has malformed sha256 {:?}",
            artifact.sha256
        )));
    }
    Ok(())
}

fn validate_commit_id(commit: &str) -> Result<()> {
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RunError::new(format!(
            "worker commit has malformed identity {commit:?}"
        )));
    }
    Ok(())
}

fn validate_work_manifest(path: &Path, manifest: &RunManifest) -> Result<()> {
    let Some(work) = work_state(manifest) else {
        return Ok(());
    };
    if manifest.job != RunJob::Work {
        return Err(RunError::new("non-work run cannot carry work state"));
    }
    if work.cycle_id.trim().is_empty() {
        return Err(RunError::new("work state has empty cycle id"));
    }
    validate_sha256(&work.authorization_sha256, "work authorization")?;
    if let Some(commit) = work.worker_commit.as_deref() {
        validate_commit_id(commit)?;
    }
    if let Some(head) = work.before_head.as_deref() {
        validate_commit_id(head)?;
    }
    if let Some(mechanical) = work.mechanical.as_ref() {
        if mechanical.command.trim().is_empty() {
            return Err(RunError::new("mechanical verification has empty command"));
        }
        for artifact in &mechanical.artifact_refs {
            validate_artifact_ref(artifact, "mechanical verifier artifact")?;
            validate_local_artifact(path, artifact)?;
            if !manifest.artifacts.contains(artifact) {
                return Err(RunError::new(
                    "mechanical verifier artifact is not pinned in manifest artifacts",
                ));
            }
        }
    }
    if work.stage == WorkStage::PendingReview {
        let profile = work.worker_profile.as_deref().unwrap_or_default();
        let commit = work.worker_commit.as_deref().unwrap_or_default();
        let mechanical = work.mechanical.as_ref().ok_or_else(|| {
            RunError::new("pending-review work state is missing mechanical evidence")
        })?;
        if profile.is_empty() || commit.is_empty() || !mechanical.passed {
            return Err(RunError::new(
                "pending-review work state is missing a verified worker identity",
            ));
        }
        if manifest.verifier.mechanical.as_deref() != Some(mechanical.command.as_str()) {
            return Err(RunError::new(
                "pending-review verifier command does not match manifest",
            ));
        }
        if mechanical.artifact_refs.is_empty() {
            return Err(RunError::new(
                "pending-review work state has no verifier artifacts",
            ));
        }
    }
    Ok(())
}

fn validate_work_events(manifest: &RunManifest, events: &[RunEvent]) -> Result<()> {
    let Some(work) = work_state(manifest) else {
        return Ok(());
    };
    if work.stage != WorkStage::PendingReview {
        return Ok(());
    }
    let mechanical = work
        .mechanical
        .as_ref()
        .expect("pending-review manifest validation requires mechanical evidence");
    let verify_events = events
        .iter()
        .filter(|event| event.kind == EventKind::VerifyFinished)
        .collect::<Vec<_>>();
    if verify_events.is_empty() {
        return Ok(());
    }
    if verify_events.len() != 1 {
        return Err(RunError::new(
            "pending-review event log has duplicate verifier events",
        ));
    }
    let verify = verify_events[0];
    if verify.outcome.as_deref() != Some("passed") {
        return Err(RunError::new(
            "pending-review verifier event is not a passing result",
        ));
    }
    if verify.artifact_refs != mechanical.artifact_refs {
        return Err(RunError::new(
            "pending-review verifier event evidence does not match manifest",
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RunError::new(format!("{label} has malformed sha256")));
    }
    Ok(())
}

fn validate_local_artifact(contract_path: &Path, artifact: &ArtifactRef) -> Result<()> {
    let relative = Path::new(&artifact.path);
    let is_local = relative == Path::new("approval.json")
        || relative.starts_with("attempts")
        || relative.starts_with("artifacts");
    if !is_local {
        return Ok(());
    }
    validate_relative_artifact_path(relative)?;
    let run_dir = contract_path
        .parent()
        .ok_or_else(|| RunError::new("contract path has no run directory"))?;
    let bytes = std::fs::read(run_dir.join(relative)).map_err(|error| {
        RunError::new(format!(
            "failed to read referenced artifact {}: {error}",
            relative.display()
        ))
    })?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != artifact.sha256 {
        return Err(RunError::new(format!(
            "artifact hash mismatch for {}",
            relative.display()
        )));
    }
    Ok(())
}

fn validate_roster_snapshot(
    manifest_path: &Path,
    snapshot: &RosterSnapshotArtifact,
    policy_sha256: Option<&str>,
) -> Result<()> {
    if snapshot.path != "roster.json" {
        return Err(RunError::new(
            "copied roster snapshot must use the run-local roster.json path",
        ));
    }
    validate_sha256(&snapshot.sha256, "copied roster snapshot")?;
    let policy_sha256 = policy_sha256.ok_or_else(|| {
        RunError::new("copied roster snapshot is missing its pinned policy_sha256")
    })?;
    validate_sha256(policy_sha256, "copied roster policy")?;
    let run_dir = manifest_path
        .parent()
        .ok_or_else(|| RunError::new("manifest path has no run directory"))?;
    let bytes = std::fs::read(run_dir.join(&snapshot.path)).map_err(|error| {
        RunError::new(format!("failed to read copied roster snapshot: {error}"))
    })?;
    let size = u64::try_from(bytes.len())
        .map_err(|_| RunError::new("copied roster snapshot exceeds u64"))?;
    if size != snapshot.size_bytes {
        return Err(RunError::new("copied roster snapshot size mismatch"));
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != snapshot.sha256 {
        return Err(RunError::new("copied roster snapshot hash mismatch"));
    }
    let parsed = crate::bursar::parse_roster_snapshot(&bytes)
        .map_err(|error| RunError::new(format!("copied roster snapshot invalid: {error}")))?;
    if parsed.policy_sha256() != policy_sha256 {
        return Err(RunError::new(
            "copied roster snapshot policy_sha256 mismatch",
        ));
    }
    Ok(())
}

fn validate_relative_artifact_path(path: &Path) -> Result<()> {
    let mut saw_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => saw_component = true,
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => {
                return Err(RunError::new(format!(
                    "artifact destination must be relative and contained: {}",
                    path.display()
                )));
            }
        }
    }
    if !saw_component {
        return Err(RunError::new("artifact destination must not be empty"));
    }
    Ok(())
}

fn artifact_ref(path: &Path, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        path: path.to_string_lossy().replace('\\', "/"),
        sha256: format!("{:x}", Sha256::digest(bytes)),
    }
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            RunError::new(format!(
                "failed to create artifact dir {}: {error}",
                parent.display()
            ))
        })?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(path).map_err(|error| {
        RunError::new(format!(
            "failed to create immutable artifact {}: {error}",
            path.display()
        ))
    })?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(path);
        return Err(RunError::new(format!(
            "failed to write immutable artifact {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

/// Whole-file atomic replace: write to a sibling temp file, then rename over
/// the original. Mirrors `ratchet.rs::save`.
fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            RunError::new(format!("failed to create dir {}: {e}", parent.display()))
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|e| RunError::new(format!("failed to write temp {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RunError::new(format!(
            "failed to rename temp {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })
}

/// Append-only atomic replace: read the existing file, append the new line
/// in memory, write the full new contents to a sibling temp file, then
/// rename over the original. Mirrors `ledger.rs::append_serialized`.
fn append_event_line(path: &Path, event: &RunEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            RunError::new(format!("failed to create dir {}: {e}", parent.display()))
        })?;
    }
    let mut new_line = serde_json::to_vec(event)
        .map_err(|e| RunError::new(format!("failed to serialize event: {e}")))?;
    new_line.push(b'\n');

    let existing = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(RunError::new(format!(
                "failed to read events {}: {e}",
                path.display()
            )));
        }
    };
    let mut contents = existing;
    contents.extend_from_slice(&new_line);

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &contents)
        .map_err(|e| RunError::new(format!("failed to write temp {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RunError::new(format!(
            "failed to rename temp {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::process::{Command, Stdio};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-run-{label}-{nanos}"));
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

    fn fixed_now() -> DateTime<Utc> {
        "2026-07-16T12:00:00Z".parse().expect("fixed timestamp")
    }

    fn new_run_request() -> NewRun {
        NewRun {
            target: RunTarget {
                repo: "/repo/conductor".to_string(),
                bead: Some("conductor-run-contract".to_string()),
            },
            approved_profiles: vec!["claude-sonnet-5".to_string(), "gpt-5.6-luna".to_string()],
            bursar_roster_artifact: Some(ArtifactRef {
                path: "/home/.config/bursar/roster.toml".to_string(),
                sha256: "a".repeat(64),
            }),
            roster_snapshot: None,
            limits: RunLimits {
                item_wall_clock_mins: Some(45),
                max_attempts: Some(3),
            },
            verifier: RunVerifier {
                mechanical: Some("cargo test".to_string()),
                qualitative: Some("lead-review".to_string()),
            },
            work: None,
            approval: Some(serde_json::json!({
                "schema": "test/approval@1",
                "decision": "approved"
            })),
        }
    }

    #[test]
    fn run_event_manifest_pins_target_job_profiles_roster_hash_limits_and_lifecycle() {
        let temp = TempDir::new("manifest-pins");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");

        let manifest = read_manifest(&handle.manifest_path()).expect("read manifest");
        assert_eq!(manifest.schema, RUN_SCHEMA);
        assert_eq!(manifest.job, RunJob::Work);
        assert_eq!(manifest.target.repo, "/repo/conductor");
        assert_eq!(
            manifest.target.bead.as_deref(),
            Some("conductor-run-contract")
        );
        assert_eq!(
            manifest.approved_profiles.profiles,
            vec!["claude-sonnet-5".to_string(), "gpt-5.6-luna".to_string()]
        );
        assert_eq!(
            manifest
                .bursar_roster_artifact
                .as_ref()
                .map(|a| a.sha256.clone()),
            Some("a".repeat(64))
        );
        assert_eq!(manifest.limits.item_wall_clock_mins, Some(45));
        assert_eq!(manifest.limits.max_attempts, Some(3));
        assert_eq!(manifest.verifier.mechanical.as_deref(), Some("cargo test"));
        assert_eq!(manifest.lifecycle, RunLifecycle::Running);
        assert!(manifest.outcome.is_none());
        assert!(handle.dir().join("approval.json").is_file());
        assert!(handle.dir().join("attempts").is_dir());
        assert!(handle.dir().join("artifacts").is_dir());

        let events = read_events(&handle.events_path()).expect("read initial events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::RunStarted);
        assert_eq!(events[0].artifact_refs[0].path, "approval.json");
    }

    #[test]
    fn run_event_kinds_cover_attempt_verify_review_coverage_gap_and_terminal_outcome() {
        let temp = TempDir::new("event-kinds");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");

        for kind in [
            EventKind::AttemptStarted,
            EventKind::AttemptFinished,
            EventKind::VerifyFinished,
            EventKind::ReviewFinished,
            EventKind::CoverageGap,
        ] {
            handle
                .append_event_at(
                    kind,
                    EventInput {
                        profile_id: Some("claude-sonnet-5".to_string()),
                        ..EventInput::default()
                    },
                    fixed_now(),
                )
                .expect("append event");
        }
        handle.finish("verified").expect("finish run");

        let events = read_events(&handle.events_path()).expect("read events");
        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::RunStarted,
                EventKind::AttemptStarted,
                EventKind::AttemptFinished,
                EventKind::VerifyFinished,
                EventKind::ReviewFinished,
                EventKind::CoverageGap,
                EventKind::RunFinished,
            ]
        );
        assert!(events.iter().all(|e| e.schema == EVENT_SCHEMA));
        let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6, 7]);

        let manifest = read_manifest(&handle.manifest_path()).expect("read manifest");
        assert_eq!(manifest.lifecycle, RunLifecycle::Finished);
        assert_eq!(manifest.outcome.as_deref(), Some("verified"));
    }

    #[test]
    fn run_event_rejects_unknown_manifest_schema() {
        let temp = TempDir::new("bad-manifest-schema");
        let path = temp.path().join("manifest.json");
        // Otherwise-complete manifest so the failure is unambiguously the
        // schema check, not a missing-field parse error.
        let mut manifest = serde_json::to_value(RunManifest {
            schema: "conductor/run@2".to_string(),
            run_id: "x".to_string(),
            job: RunJob::Work,
            target: RunTarget {
                repo: "/repo".to_string(),
                bead: None,
            },
            details: RunDetails::Work { state: None },
            created_at: "2026-07-16T12:00:00Z".to_string(),
            updated_at: "2026-07-16T12:00:00Z".to_string(),
            approved_profiles: ApprovedProfileEnvelope::default(),
            bursar_roster_artifact: None,
            roster_snapshot: None,
            roster_policy_sha256: None,
            limits: RunLimits::default(),
            verifier: RunVerifier::default(),
            artifacts: Vec::new(),
            lifecycle: RunLifecycle::Started,
            outcome: None,
        })
        .unwrap();
        manifest["schema"] = serde_json::json!("conductor/run@1");
        std::fs::write(&path, manifest.to_string()).unwrap();

        let err = read_manifest(&path).expect_err("unknown schema must fail closed");
        assert!(err.to_string().contains("unknown schema"));
    }

    #[test]
    fn run_event_rejects_unknown_event_schema() {
        let temp = TempDir::new("bad-event-schema");
        let path = temp.path().join("events.jsonl");
        let bad_line = serde_json::json!({
            "schema": "conductor/event@1",
            "event_id": "x-1",
            "run_id": "x",
            "seq": 1,
            "ts": "2026-07-16T12:00:00Z",
            "kind": "run_started",
            "job": "work",
            "target": {"repo": "/repo"},
        });
        std::fs::write(&path, format!("{bad_line}\n")).unwrap();

        let err = read_events(&path).expect_err("unknown schema must fail closed");
        assert!(err.to_string().contains("unknown schema"));
    }

    #[test]
    fn run_event_detects_partial_write() {
        let temp = TempDir::new("partial-write");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        handle
            .append_event_at(
                EventKind::AttemptStarted,
                EventInput::default(),
                fixed_now(),
            )
            .expect("append first event");

        // Simulate a crash mid-write: a truncated JSON line appended directly,
        // bypassing the atomic append helper.
        let mut raw = std::fs::read_to_string(handle.events_path()).unwrap();
        raw.push_str("{\"schema\":\"conductor/event@1\",\"event_id\":\"trunc");
        std::fs::write(handle.events_path(), raw).unwrap();

        let err = read_events(&handle.events_path()).expect_err("partial line must fail closed");
        assert!(err.to_string().contains("malformed event"));
    }

    #[test]
    fn run_event_run_ids_do_not_collide_within_the_same_second() {
        let now = fixed_now();
        let mut ids = HashSet::new();
        for counter in 0..500 {
            assert!(
                ids.insert(new_run_id(RunJob::Work, now, counter)),
                "run id collided"
            );
        }
    }

    #[test]
    fn run_event_manifest_and_events_writes_leave_no_temp_file_behind() {
        let temp = TempDir::new("no-temp-leftover");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Review, new_run_request(), fixed_now())
                .expect("create run");
        handle
            .append_event_at(
                EventKind::VerifyFinished,
                EventInput::default(),
                fixed_now(),
            )
            .expect("append event");
        handle.finish("passed").expect("finish run");

        assert!(!handle.manifest_path().with_extension("json.tmp").exists());
        assert!(!handle.events_path().with_extension("json.tmp").exists());
        assert!(handle.manifest_path().is_file());
        assert!(handle.events_path().is_file());
    }

    #[test]
    fn run_event_open_resumes_sequence_and_rejects_unknown_schema_on_reopen() {
        let temp = TempDir::new("reopen");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Consult, new_run_request(), fixed_now())
                .expect("create run");
        handle
            .append_event_at(
                EventKind::AttemptStarted,
                EventInput::default(),
                fixed_now(),
            )
            .expect("append event");
        let run_id = handle.run_id().to_string();
        drop(handle);

        let mut reopened = RunHandle::open(temp.path(), &run_id).expect("reopen run");
        reopened
            .append_event_at(
                EventKind::AttemptFinished,
                EventInput::default(),
                fixed_now(),
            )
            .expect("append second event");
        let events = read_events(&reopened.events_path()).expect("read events");
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].seq, 3);

        // Corrupt the manifest after the fact and confirm reopen fails closed.
        std::fs::write(reopened.manifest_path(), br#"{"schema":"conductor/run@9"}"#).unwrap();
        assert!(RunHandle::open(temp.path(), &run_id).is_err());
    }

    #[test]
    fn run_event_resume_repairs_interrupted_pending_review_checkpoint() {
        let temp = TempDir::new("resume-pending-review-event");
        let mut request = new_run_request();
        request.work = Some(WorkState {
            cycle_id: "cycle-20260717-015903".to_string(),
            authorization_sha256: "b".repeat(64),
            before_head: Some("d".repeat(40)),
            owner_pid: None,
            worker_pgid: None,
            worker_profile: None,
            worker_commit: None,
            mechanical: None,
            stage: WorkStage::Implementing,
            review_resume_budget_secs: None,
        });
        let mut handle = RunHandle::create_at(temp.path(), RunJob::Work, request, fixed_now())
            .expect("create work run");
        let verifier_log = temp.path().join("verifier.log");
        std::fs::write(&verifier_log, b"mechanical verification passed\n")
            .expect("write verifier log");
        let artifact = handle
            .capture_artifact(&verifier_log, Path::new("artifacts/mechanical.log"))
            .expect("capture verifier evidence");
        handle
            .checkpoint_pending_review(
                "gpt-5.6-luna",
                &"c".repeat(40),
                "cargo test",
                vec![artifact.clone()],
            )
            .expect("checkpoint pending review");
        let persisted = read_manifest(&handle.manifest_path()).expect("read checkpointed manifest");
        let RunDetails::Work { state: Some(state) } = persisted.details else {
            panic!("work details own the mutable work state");
        };
        assert_eq!(
            state.stage,
            WorkStage::PendingReview,
            "the tagged work state must persist the checkpoint, not a stale creation copy"
        );

        let events_path = handle.events_path();
        let mut rows = event_values(&events_path);
        assert_eq!(
            rows.pop()
                .and_then(|row| row["kind"].as_str().map(str::to_string)),
            Some("verify_finished".to_string())
        );
        write_event_values(&events_path, &rows);
        let run_id = handle.run_id().to_string();
        drop(handle);

        let mut reopened = RunHandle::open(temp.path(), &run_id)
            .expect("manifest checkpoint survives missing event");

        reopened
            .ensure_pending_review_event()
            .expect("repair verifier event");
        reopened
            .ensure_pending_review_event()
            .expect("repair is idempotent");
        reopened
            .record_review_resume_budget(Duration::from_secs(17))
            .expect("record an explicitly approved review-resume budget");
        assert_eq!(
            reopened
                .work()
                .and_then(|work| work.review_resume_budget_secs),
            Some(17),
            "a resumed review may use only a budget durably recorded in its manifest",
        );

        let events = read_events(&events_path).expect("read repaired events");
        let verify_events = events
            .iter()
            .filter(|event| event.kind == EventKind::VerifyFinished)
            .collect::<Vec<_>>();
        assert_eq!(verify_events.len(), 1);
        assert_eq!(verify_events[0].outcome.as_deref(), Some("passed"));
        assert_eq!(verify_events[0].artifact_refs, vec![artifact]);
    }
    fn pending_work_run(
        temp: &TempDir,
        cycle_id: &str,
        repo: &str,
        bead: &str,
        label: &str,
    ) -> RunHandle {
        let mut request = new_run_request();
        request.target = RunTarget {
            repo: repo.to_string(),
            bead: Some(bead.to_string()),
        };
        request.work = Some(WorkState {
            cycle_id: cycle_id.to_string(),
            authorization_sha256: "b".repeat(64),
            before_head: Some("d".repeat(40)),
            owner_pid: None,
            worker_pgid: None,
            worker_profile: None,
            worker_commit: None,
            mechanical: None,
            stage: WorkStage::Implementing,
            review_resume_budget_secs: None,
        });
        let mut handle = RunHandle::create_at(temp.path(), RunJob::Work, request, fixed_now())
            .expect("create work run");
        let verifier_log = temp.path().join(format!("{label}.log"));
        std::fs::write(&verifier_log, b"mechanical verification passed\n")
            .expect("write verifier log");
        let artifact = handle
            .capture_artifact(&verifier_log, Path::new("artifacts/mechanical.log"))
            .expect("capture verifier evidence");
        handle
            .checkpoint_pending_review(
                "gpt-5.6-luna",
                &"c".repeat(40),
                "cargo test",
                vec![artifact],
            )
            .expect("checkpoint pending review");
        handle
    }

    fn truncate_pending_manifest(handle: &RunHandle, cycle_id: &str, repo: &str, bead: &str) {
        let bytes = format!(
            r#"{{"target":{{"repo":{},"bead":{}}},"details":{{"job":"work","state":{{"cycle_id":{},"stage":"pending_review""#,
            serde_json::to_string(repo).expect("serialize repo"),
            serde_json::to_string(bead).expect("serialize bead"),
            serde_json::to_string(cycle_id).expect("serialize cycle id"),
        );
        std::fs::write(handle.manifest_path(), bytes).expect("write truncated manifest");
    }

    #[test]
    fn find_pending_work_run_ignores_an_unrelated_run_with_corrupt_artifacts() {
        let temp = TempDir::new("pending-unrelated-corrupt");
        let unrelated =
            pending_work_run(&temp, "cycle-other", "/repo/other", "other-1", "unrelated");
        std::fs::remove_file(unrelated.dir().join("artifacts/mechanical.log"))
            .expect("prune unrelated artifact");
        let matching = pending_work_run(
            &temp,
            "cycle-target",
            "/repo/target",
            "target-1",
            "matching",
        );

        let found = find_pending_work_run(temp.path(), "cycle-target", "/repo/target", "target-1")
            .expect("unrelated corruption must not block discovery");

        assert_eq!(found.as_deref(), Some(matching.run_id()));
    }

    #[test]
    fn find_pending_work_run_fails_closed_when_the_matching_candidate_is_corrupt() {
        let temp = TempDir::new("pending-matching-corrupt");
        let matching = pending_work_run(
            &temp,
            "cycle-target",
            "/repo/target",
            "target-1",
            "matching",
        );
        std::fs::remove_file(matching.dir().join("artifacts/mechanical.log"))
            .expect("prune matching artifact");

        let error = find_pending_work_run(temp.path(), "cycle-target", "/repo/target", "target-1")
            .expect_err("matching corruption must fail closed");

        assert!(
            error.to_string().contains("pending-review candidate"),
            "matching authentication failure must identify its candidate: {error}"
        );
    }

    #[test]
    fn find_pending_work_run_reports_matching_duplicates_in_run_id_order() {
        let temp = TempDir::new("pending-duplicates");
        let first = pending_work_run(&temp, "cycle-target", "/repo/target", "target-1", "first");
        let second = pending_work_run(&temp, "cycle-target", "/repo/target", "target-1", "second");
        let mut expected = [first.run_id().to_string(), second.run_id().to_string()];
        expected.sort();

        let error = find_pending_work_run(temp.path(), "cycle-target", "/repo/target", "target-1")
            .expect_err("multiple matching pending runs must fail closed");

        assert_eq!(
            error.to_string(),
            format!(
                "multiple pending-review runs found for cycle-target /repo/target/target-1: {}",
                expected.join(", ")
            )
        );
    }

    #[test]
    fn find_pending_work_run_ignores_pruned_finished_history_for_another_target() {
        let temp = TempDir::new("pending-pruned-finished");
        let mut finished =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create finished history");
        let history_log = temp.path().join("history.log");
        std::fs::write(&history_log, b"historic evidence\n").expect("write history log");
        finished
            .capture_artifact(&history_log, Path::new("artifacts/history.log"))
            .expect("capture history artifact");
        finished.finish("verified").expect("finish history");
        std::fs::remove_file(finished.dir().join("artifacts/history.log"))
            .expect("prune finished history artifact");
        let matching = pending_work_run(
            &temp,
            "cycle-target",
            "/repo/target",
            "target-1",
            "matching",
        );

        let found = find_pending_work_run(temp.path(), "cycle-target", "/repo/target", "target-1")
            .expect("pruned finished history must not block discovery");

        assert_eq!(found.as_deref(), Some(matching.run_id()));
    }

    #[test]
    fn find_pending_work_run_ignores_truncated_history_from_a_prefix_colliding_cycle() {
        let temp = TempDir::new("pending-truncated-prefix");
        let matching = pending_work_run(
            &temp,
            "cycle-20260722-120000",
            "/repo/target",
            "target-1",
            "matching",
        );
        let colliding = pending_work_run(
            &temp,
            "cycle-20260722-120000-2",
            "/repo/target",
            "target-1",
            "colliding",
        );
        truncate_pending_manifest(
            &colliding,
            "cycle-20260722-120000-2",
            "/repo/target",
            "target-1",
        );

        let found = find_pending_work_run(
            temp.path(),
            "cycle-20260722-120000",
            "/repo/target",
            "target-1",
        )
        .expect("prefix-colliding malformed history must remain unrelated");

        assert_eq!(found.as_deref(), Some(matching.run_id()));
    }

    #[test]
    fn find_pending_work_run_fails_closed_on_a_truncated_matching_manifest() {
        let temp = TempDir::new("pending-truncated-matching");
        let matching = pending_work_run(
            &temp,
            "cycle-20260722-120000-2",
            "/repo/target",
            "target-1",
            "matching",
        );
        truncate_pending_manifest(
            &matching,
            "cycle-20260722-120000-2",
            "/repo/target",
            "target-1",
        );

        let error = find_pending_work_run(
            temp.path(),
            "cycle-20260722-120000-2",
            "/repo/target",
            "target-1",
        )
        .expect_err("matching truncated evidence must fail closed");

        assert!(error.to_string().contains("is malformed"));
    }

    #[test]
    fn find_pending_work_run_ignores_truncated_history_from_a_genuinely_unrelated_cycle() {
        let temp = TempDir::new("pending-truncated-unrelated");
        let matching = pending_work_run(
            &temp,
            "cycle-20260722-120000",
            "/repo/target",
            "target-1",
            "matching",
        );
        let unrelated = pending_work_run(
            &temp,
            "cycle-20260722-120001",
            "/repo/target",
            "target-1",
            "unrelated",
        );
        truncate_pending_manifest(
            &unrelated,
            "cycle-20260722-120001",
            "/repo/target",
            "target-1",
        );

        let found = find_pending_work_run(
            temp.path(),
            "cycle-20260722-120000",
            "/repo/target",
            "target-1",
        )
        .expect("genuinely unrelated malformed history must not block discovery");

        assert_eq!(found.as_deref(), Some(matching.run_id()));
    }

    #[test]
    fn pending_work_index_authenticates_multiple_targets_without_rescanning_history() {
        let temp = TempDir::new("pending-index");
        let first = pending_work_run(&temp, "cycle-target", "/repo/first", "first-1", "first");
        let second = pending_work_run(&temp, "cycle-target", "/repo/second", "second-1", "second");
        let index = PendingWorkIndex::scan(temp.path()).expect("build per-cycle discovery index");
        let unrelated =
            pending_work_run(&temp, "cycle-other", "/repo/other", "other-1", "unrelated");
        std::fs::remove_file(unrelated.dir().join("artifacts/mechanical.log"))
            .expect("corrupt post-index unrelated history");

        let found_first = index
            .find_pending_work_run("cycle-target", "/repo/first", "first-1")
            .expect("first indexed lookup");
        let found_second = index
            .find_pending_work_run("cycle-target", "/repo/second", "second-1")
            .expect("second indexed lookup");

        assert_eq!(found_first.as_deref(), Some(first.run_id()));
        assert_eq!(found_second.as_deref(), Some(second.run_id()));
    }
    #[test]
    fn run_event_terminal_transition_is_hashed_and_read_only_after_finish() {
        let temp = TempDir::new("terminal-transition");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let transition = TerminalTransition {
            action: TerminalTransitionAction::Close,
            reason: "conductor cycle-1: verified via cargo test".to_string(),
            metadata: None,
            comment: None,
        };
        let artifact = handle
            .write_terminal_transition(&transition)
            .expect("persist terminal transition before the terminal event");
        handle
            .finish_with_artifacts("verified", vec![artifact])
            .expect("finish run with transition evidence");
        let run_id = handle.run_id().to_string();
        drop(handle);

        let reopened = RunHandle::open(temp.path(), &run_id).expect("open finished run");
        assert_eq!(
            reopened.terminal_transition().expect("read transition"),
            Some(transition)
        );
        assert!(
            reopened
                .write_terminal_transition(&TerminalTransition {
                    action: TerminalTransitionAction::Close,
                    reason: "different reason".to_string(),
                    metadata: None,
                    comment: None,
                })
                .is_err()
        );
    }

    #[test]
    fn run_event_open_recovers_authenticated_terminal_event_before_manifest_rewrite() {
        let temp = TempDir::new("terminal-event-before-manifest");
        let mut handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        handle
            .finish("verified")
            .expect("append terminal evidence before the simulated crash");
        let run_id = handle.run_id().to_string();
        let manifest_path = handle.manifest_path();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
                .expect("parse manifest");
        manifest["lifecycle"] = serde_json::json!("running");
        manifest["outcome"] = serde_json::Value::Null;
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize interrupted manifest"),
        )
        .expect("persist interrupted manifest");
        drop(handle);

        let reopened =
            RunHandle::open(temp.path(), &run_id).expect("recover authenticated terminal event");
        assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(reopened.manifest().outcome.as_deref(), Some("verified"));
        drop(reopened);

        let reopened_again =
            RunHandle::open(temp.path(), &run_id).expect("terminal recovery is idempotent");
        assert_eq!(reopened_again.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(
            reopened_again.manifest().outcome.as_deref(),
            Some("verified")
        );
    }

    #[test]
    fn run_event_run_dir_is_collision_resistant_under_state_dir() {
        let temp = TempDir::new("run-dir-layout");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Consult, new_run_request(), fixed_now())
                .expect("create run");
        assert!(
            handle
                .manifest_path()
                .starts_with(runs_dir(temp.path()).join(handle.run_id()))
        );
        assert!(handle.run_id().starts_with("run-consult-"));
        assert!(handle.dir().join("attempts").is_dir());
        assert!(handle.dir().join("artifacts").is_dir());
    }

    #[test]
    fn run_event_missing_bursar_roster_emits_explicit_coverage_gap() {
        let temp = TempDir::new("bursar-gap");
        let mut request = new_run_request();
        request.bursar_roster_artifact = None;
        let handle = RunHandle::create_at(temp.path(), RunJob::Work, request, fixed_now())
            .expect("create run");

        let events = read_events(&handle.events_path()).expect("read events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, EventKind::RunStarted);
        assert_eq!(events[1].kind, EventKind::CoverageGap);
        assert_eq!(
            events[1].outcome.as_deref(),
            Some("bursar_roster_artifact_unavailable")
        );
    }

    #[test]
    fn run_event_approval_and_captured_artifacts_are_immutable_and_hashed() {
        let temp = TempDir::new("immutable-artifacts");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let source = temp.path().join("source.log");
        std::fs::write(&source, b"artifact bytes\n").expect("write source");
        let relative = Path::new("attempts/001/stdout.log");

        let artifact = handle
            .capture_artifact(&source, relative)
            .expect("capture artifact");
        assert_eq!(artifact.path, "attempts/001/stdout.log");
        assert_eq!(artifact.sha256.len(), 64);
        assert!(handle.capture_artifact(&source, relative).is_err());
        assert!(
            handle
                .capture_artifact(&source, Path::new("approval.json"))
                .is_err()
        );
        std::fs::write(handle.dir().join("approval.json"), b"tampered\n").expect("tamper approval");
        assert!(read_manifest(&handle.manifest_path()).is_err());
        assert!(read_events(&handle.events_path()).is_err());
    }

    #[test]
    fn run_event_read_rejects_sequence_and_identity_corruption() {
        for corruption in ["seq", "event_id", "run_id", "job", "arena_job", "target"] {
            let temp = TempDir::new(corruption);
            let mut handle =
                RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                    .expect("create run");
            handle
                .append_event_at(
                    EventKind::AttemptStarted,
                    EventInput::default(),
                    fixed_now(),
                )
                .expect("append event");
            let path = handle.events_path();
            let mut rows = event_values(&path);
            match corruption {
                "seq" => {
                    rows[1]["seq"] = serde_json::json!(3);
                    rows[1]["event_id"] = serde_json::json!(format!("{}-000003", handle.run_id()));
                }
                "event_id" => rows[1]["event_id"] = serde_json::json!("wrong-000002"),
                "run_id" => {
                    rows[1]["run_id"] = serde_json::json!("run-work-other");
                    rows[1]["event_id"] = serde_json::json!("run-work-other-000002");
                }
                "job" => rows[1]["job"] = serde_json::json!("review"),
                "arena_job" => rows[1]["job"] = serde_json::json!("arena"),
                "target" => rows[1]["target"]["repo"] = serde_json::json!("/other/repo"),
                _ => unreachable!(),
            }
            write_event_values(&path, &rows);

            assert!(
                read_events(&path).is_err(),
                "{corruption} corruption must fail closed"
            );
        }
    }

    #[test]
    fn run_event_read_rejects_malformed_hash_and_valid_json_without_newline() {
        let temp = TempDir::new("bad-hash");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let path = handle.events_path();
        let mut rows = event_values(&path);
        let original_rows = rows.clone();
        rows[0]["artifact_refs"][0]["sha256"] = serde_json::json!("not-a-sha256");
        write_event_values(&path, &rows);
        assert!(read_events(&path).is_err());

        write_event_values(&path, &original_rows);
        let mut bytes = std::fs::read(&path).expect("read events");
        assert_eq!(bytes.pop(), Some(b'\n'));
        std::fs::write(&path, bytes).expect("remove final newline");
        assert!(read_events(&path).is_err());
    }

    #[test]
    fn run_event_manifest_rejects_malformed_hash() {
        let temp = TempDir::new("manifest-bad-hash");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let path = handle.manifest_path();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["artifacts"][0]["sha256"] = serde_json::json!("bad");
        std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

        assert!(read_manifest(&path).is_err());
    }

    #[test]
    fn run_event_open_rejects_manifest_event_identity_mismatch() {
        let temp = TempDir::new("manifest-event-mismatch");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");
        let run_id = handle.run_id().to_string();
        let manifest_path = handle.manifest_path();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["target"]["repo"] = serde_json::json!("/different/repo");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        assert!(RunHandle::open(temp.path(), &run_id).is_err());
    }

    fn implementing_request_with_worker_pgid(worker_pgid: Option<u32>) -> NewRun {
        let mut request = new_run_request();
        request.work = Some(WorkState {
            cycle_id: "cycle-1".to_string(),
            authorization_sha256: "b".repeat(64),
            before_head: None,
            owner_pid: None,
            worker_pgid,
            worker_profile: None,
            worker_commit: None,
            mechanical: None,
            stage: WorkStage::Implementing,
            review_resume_budget_secs: None,
        });
        request
    }

    #[test]
    fn invalidate_worker_group_clears_a_recorded_identity_and_persists_across_reopen() {
        let temp = TempDir::new("invalidate-clears");
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            implementing_request_with_worker_pgid(Some(111)),
            fixed_now(),
        )
        .expect("create run with attempt one's identity already recorded");
        assert_eq!(handle.worker_pgid(), Some(111));

        handle
            .invalidate_worker_group()
            .expect("invalidate attempt one's identity ahead of attempt two's spawn");
        assert_eq!(
            handle.worker_pgid(),
            None,
            "a superseded attempt's identity must not survive invalidation"
        );

        let reopened = RunHandle::open(temp.path(), handle.run_id()).expect("reopen run from disk");
        assert_eq!(
            reopened.worker_pgid(),
            None,
            "invalidation must be durable, not just in-memory"
        );
    }

    #[test]
    fn invalidate_worker_group_is_a_durable_no_op_before_any_attempt_has_spawned() {
        let temp = TempDir::new("invalidate-noop");
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            implementing_request_with_worker_pgid(None),
            fixed_now(),
        )
        .expect("create run before any worker has spawned");

        handle
            .invalidate_worker_group()
            .expect("invalidating with no prior identity must still succeed");
        assert_eq!(handle.worker_pgid(), None);
    }

    #[test]
    fn invalidate_worker_group_then_record_binds_only_the_new_attempt() {
        let temp = TempDir::new("invalidate-then-record");
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            implementing_request_with_worker_pgid(Some(111)),
            fixed_now(),
        )
        .expect("create run with attempt one's identity already recorded");

        handle
            .invalidate_worker_group()
            .expect("invalidate attempt one's identity ahead of attempt two's spawn");
        handle
            .record_worker_group(222)
            .expect("bind attempt two's identity once it spawns");

        assert_eq!(
            handle.worker_pgid(),
            Some(222),
            "the manifest must only ever reflect the latest attempt's identity"
        );
    }

    #[test]
    fn invalidate_worker_group_fails_closed_on_a_finished_run() {
        let temp = TempDir::new("invalidate-finished");
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            implementing_request_with_worker_pgid(Some(111)),
            fixed_now(),
        )
        .expect("create run");
        handle.finish("verified").expect("finish run");

        let err = handle
            .invalidate_worker_group()
            .expect_err("a finished run's worker identity must never be mutated");
        assert!(err.to_string().contains("finished run"));
    }

    #[test]
    fn run_event_cross_process_same_second_creation_is_exclusive() {
        const STATE_ENV: &str = "CONDUCTOR_RUN_TEST_CHILD_STATE";
        const RESULT_ENV: &str = "CONDUCTOR_RUN_TEST_CHILD_RESULT";
        if let (Some(state), Some(result)) =
            (std::env::var_os(STATE_ENV), std::env::var_os(RESULT_ENV))
        {
            let handle = RunHandle::create_at(
                Path::new(&state),
                RunJob::Work,
                new_run_request(),
                fixed_now(),
            )
            .expect("child creates run");
            std::fs::write(result, handle.run_id()).expect("child writes run id");
            return;
        }

        let temp = TempDir::new("cross-process");
        let current_exe = std::env::current_exe().expect("current test binary");
        let result_one = temp.path().join("child-one.id");
        let result_two = temp.path().join("child-two.id");
        let spawn = |result: &Path| {
            Command::new(&current_exe)
                .args([
                    "--exact",
                    "run::tests::run_event_cross_process_same_second_creation_is_exclusive",
                    "--nocapture",
                ])
                .env(STATE_ENV, temp.path())
                .env(RESULT_ENV, result)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn child test")
        };
        let mut one = spawn(&result_one);
        let mut two = spawn(&result_two);
        assert!(one.wait().expect("wait child one").success());
        assert!(two.wait().expect("wait child two").success());

        let id_one = std::fs::read_to_string(result_one).expect("read child one id");
        let id_two = std::fs::read_to_string(result_two).expect("read child two id");
        assert_ne!(id_one, id_two);
        assert!(runs_dir(temp.path()).join(id_one).is_dir());
        assert!(runs_dir(temp.path()).join(id_two).is_dir());
    }

    #[test]
    fn run_event_touch_heartbeat_is_read_back_and_wins_over_manifest_updated_at() {
        let temp = TempDir::new("heartbeat");
        let handle =
            RunHandle::create_at(temp.path(), RunJob::Work, new_run_request(), fixed_now())
                .expect("create run");

        assert_eq!(
            read_heartbeat(handle.dir()).expect("no heartbeat yet"),
            None
        );
        assert_eq!(
            handle.last_seen().expect("falls back to manifest"),
            fixed_now()
        );

        // touch_heartbeat always stamps Utc::now(); overwrite the file
        // directly afterward so the assertion below is exact rather than
        // merely "close to now".
        handle
            .touch_heartbeat()
            .expect("touch heartbeat writes a timestamp");
        let touched_at: DateTime<Utc> = "2026-07-17T00:00:00Z".parse().expect("fixed heartbeat");
        std::fs::write(handle.dir().join("heartbeat"), touched_at.to_rfc3339())
            .expect("pin heartbeat");

        assert_eq!(
            read_heartbeat(handle.dir()).expect("heartbeat reads back"),
            Some(touched_at)
        );
        assert_eq!(handle.last_seen().expect("heartbeat wins"), touched_at);
    }

    #[test]
    fn run_event_find_implementing_work_run_ignores_finished_runs() {
        let temp = TempDir::new("find-implementing");
        let cycle_id = "cycle-resume-20260717";
        let make_request = |bead: &str| {
            let mut request = new_run_request();
            request.target.bead = Some(bead.to_string());
            request.work = Some(WorkState {
                cycle_id: cycle_id.to_string(),
                authorization_sha256: "b".repeat(64),
                before_head: None,
                owner_pid: None,
                worker_pgid: None,
                worker_profile: None,
                worker_commit: None,
                mechanical: None,
                stage: WorkStage::Implementing,
                review_resume_budget_secs: None,
            });
            request
        };

        let implementing = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            make_request("impl-bead"),
            fixed_now(),
        )
        .expect("create implementing run");

        let mut finished = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            make_request("finished-bead"),
            fixed_now(),
        )
        .expect("create finished run");
        finished.finish("stale_claim_reaped").expect("finish run");

        assert_eq!(
            find_implementing_work_run(temp.path(), cycle_id, "/repo/conductor", "impl-bead")
                .expect("lookup implementing"),
            Some(implementing.run_id().to_string())
        );
        assert_eq!(
            find_implementing_work_run(temp.path(), cycle_id, "/repo/conductor", "finished-bead")
                .expect("lookup finished bead"),
            None,
            "a finished implementing-stage run must never be reclaimable"
        );
    }

    #[test]
    fn run_event_find_reclaimable_selects_latest_generation_and_keeps_finished_history() {
        let temp = TempDir::new("find-reclaimable-generations");
        let cycle_id = "cycle-resume-generations";
        let make_request = || {
            let mut request = new_run_request();
            request.target.bead = Some("gen-bead".to_string());
            request.work = Some(WorkState {
                cycle_id: cycle_id.to_string(),
                authorization_sha256: "b".repeat(64),
                before_head: Some("d".repeat(40)),
                owner_pid: Some(123),
                worker_pgid: Some(456),
                worker_profile: None,
                worker_commit: None,
                mechanical: None,
                stage: WorkStage::Implementing,
                review_resume_budget_secs: None,
            });
            request
        };
        let repo = "/repo/conductor";

        // No runs yet.
        assert_eq!(
            find_reclaimable_work_run(temp.path(), cycle_id, repo, "gen-bead")
                .expect("empty lookup"),
            None
        );

        // Generation 1 was reaped by a prior stale-claim recovery; it is
        // durable audit history, not a reclaim candidate.
        let base = fixed_now();
        let mut gen1 = RunHandle::create_at(temp.path(), RunJob::Work, make_request(), base)
            .expect("create gen1");
        gen1.finish("stale_claim_reaped").expect("finish gen1");

        // With no unfinished generation, only the finished latest is offered
        // — for a release retry, never a fresh reclaim.
        assert_eq!(
            find_reclaimable_work_run(temp.path(), cycle_id, repo, "gen-bead")
                .expect("finished-only lookup"),
            Some(ReclaimCandidate::FinishedLatest(gen1.run_id().to_string()))
        );

        // A second crash: generation 2 is created fresh and left unfinished.
        let gen2 = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            make_request(),
            base + chrono::Duration::seconds(60),
        )
        .expect("create gen2");
        assert_eq!(
            find_reclaimable_work_run(temp.path(), cycle_id, repo, "gen-bead")
                .expect("second-generation lookup"),
            Some(ReclaimCandidate::Unfinished(gen2.run_id().to_string())),
            "a repeated crash must select the one unfinished generation, not error on history"
        );

        // The finished generation-1 history is still present and readable.
        assert_eq!(
            read_manifest(
                &runs_dir(temp.path())
                    .join(gen1.run_id())
                    .join("manifest.json")
            )
            .expect("gen1 manifest survives")
            .outcome
            .as_deref(),
            Some("stale_claim_reaped")
        );
    }

    #[test]
    fn run_event_find_reclaimable_fails_closed_on_two_unfinished_generations() {
        let temp = TempDir::new("find-reclaimable-conflict");
        let cycle_id = "cycle-resume-conflict";
        let make_request = || {
            let mut request = new_run_request();
            request.target.bead = Some("conflict-bead".to_string());
            request.work = Some(WorkState {
                cycle_id: cycle_id.to_string(),
                authorization_sha256: "b".repeat(64),
                before_head: None,
                owner_pid: None,
                worker_pgid: None,
                worker_profile: None,
                worker_commit: None,
                mechanical: None,
                stage: WorkStage::Implementing,
                review_resume_budget_secs: None,
            });
            request
        };
        RunHandle::create_at(temp.path(), RunJob::Work, make_request(), fixed_now())
            .expect("create first unfinished");
        RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            make_request(),
            fixed_now() + chrono::Duration::seconds(1),
        )
        .expect("create second unfinished");

        assert!(
            find_reclaimable_work_run(temp.path(), cycle_id, "/repo/conductor", "conflict-bead")
                .is_err(),
            "two unfinished generations is an invariant violation and must fail closed"
        );
    }

    #[test]
    fn v2_manifests_reject_v1_schema_and_unknown_fields() {
        let temp = TempDir::new("strict-v2-schema");
        let path = temp.path().join("manifest.json");
        let mut value = serde_json::to_value(RunManifest {
            schema: "conductor/run@1".to_string(),
            run_id: "run-work-20260716T120000.000000000-p1-000000".to_string(),
            job: RunJob::Work,
            target: RunTarget {
                repo: "/repo/conductor".to_string(),
                bead: Some("conductor-run-v2".to_string()),
            },
            details: RunDetails::Work { state: None },
            created_at: "2026-07-16T12:00:00Z".to_string(),
            updated_at: "2026-07-16T12:00:00Z".to_string(),
            approved_profiles: ApprovedProfileEnvelope::default(),
            bursar_roster_artifact: None,
            roster_snapshot: None,
            roster_policy_sha256: None,
            limits: RunLimits::default(),
            verifier: RunVerifier::default(),
            artifacts: Vec::new(),
            lifecycle: RunLifecycle::Started,
            outcome: None,
        })
        .expect("serialize v1 manifest");
        std::fs::write(&path, value.to_string()).expect("write v1 manifest");
        assert!(
            read_manifest(&path).is_err(),
            "the v2 reader must not parse a v1 manifest"
        );

        value["schema"] = serde_json::json!("conductor/run@2");
        value["unexpected"] = serde_json::json!(true);
        std::fs::write(&path, value.to_string()).expect("write malformed v2 manifest");
        assert!(
            read_manifest(&path).is_err(),
            "strict v2 manifests must reject unknown fields"
        );
    }

    #[test]
    fn prepared_run_copies_and_pins_exact_roster_snapshot_bytes() {
        let temp = TempDir::new("copied-roster-snapshot");
        let snapshot_bytes = br#"{
          "schema":"bursar/roster@2",
          "generated_at":"2026-07-16T12:00:00Z",
          "source_artifact":{"path":"/source/roster.toml","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
          "policy_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
          "providers":[],
          "profiles":[]
        }"#
        .to_vec();
        for job in [RunJob::Review, RunJob::Consult] {
            let mut request = new_run_request();
            request.roster_snapshot = Some(RosterSnapshotInput {
                bytes: snapshot_bytes.clone(),
                policy_sha256: "b".repeat(64),
            });
            let handle = RunHandle::create_at(temp.path(), job, request, fixed_now()).expect("run");
            let roster = handle
                .manifest()
                .roster_snapshot
                .as_ref()
                .expect("copied snapshot identity");
            assert_eq!(roster.path, "roster.json");
            assert_eq!(roster.size_bytes, snapshot_bytes.len() as u64);
            assert!(handle.dir().join(&roster.path).is_file());

            std::fs::write(handle.dir().join(&roster.path), b"altered").expect("tamper snapshot");
            assert!(
                RunHandle::open(temp.path(), handle.run_id()).is_err(),
                "{job:?} resume must reject an altered copied roster snapshot"
            );
        }
    }

    #[test]
    fn plan_progress_binds_distinct_roles_and_preserves_peer_across_revision() {
        let execution = |profile_id: &str, provider_id: &str| ApprovedExecution {
            profile_id: profile_id.to_string(),
            provider_id: provider_id.to_string(),
            availability_key: provider_id.to_string(),
            execution_key: format!("{provider_id}/{profile_id}"),
        };
        let artifact = |name: &str| ArtifactRef {
            path: format!("artifacts/{name}.md"),
            sha256: "a".repeat(64),
        };
        let author = execution("planner", "anthropic");
        let peer = execution("peer", "openai");
        let different_peer = execution("other-peer", "codex");
        let mut progress = PlanProgress::Prepared;

        progress
            .start_authoring(author.clone(), StageAttemptLimit::new(1).expect("nonzero"))
            .expect("bind author");
        assert!(
            progress
                .start_authoring(author.clone(), StageAttemptLimit::new(1).expect("nonzero"))
                .is_err(),
            "author binding is immutable"
        );
        progress
            .await_peer(artifact("draft"))
            .expect("submit draft");
        assert!(
            progress
                .record_peer_verdict(author, PeerVerdict::Revise)
                .is_err(),
            "peer must be provider-distinct from the author"
        );
        progress
            .record_peer_verdict(peer.clone(), PeerVerdict::Revise)
            .expect("peer requests revision");
        progress
            .complete_revision(artifact("revision"))
            .expect("bounded revision");
        match &progress {
            PlanProgress::AwaitingPeer {
                peer: Some(bound),
                revisions,
                ..
            } => {
                assert_eq!(bound, &peer);
                assert_eq!(revisions.value(), 1);
            }
            other => panic!("unexpected progress after revision: {other:?}"),
        }
        assert!(
            progress
                .record_peer_verdict(different_peer, PeerVerdict::Approve)
                .is_err(),
            "a revision cannot replace its immutable peer"
        );
        progress
            .record_peer_verdict(peer, PeerVerdict::Approve)
            .expect("same peer approves");
        progress
            .record_second_opinion(SecondOpinionVerdict::Accept)
            .expect("second opinion terminates plan");
        assert!(matches!(
            progress,
            PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::Accepted
            }
        ));
        assert!(RevisionLimit::new(4).is_err());
        assert!(StageAttemptLimit::new(0).is_err());
    }

    #[test]
    fn unstarted_provider_block_is_explicitly_cancellable() {
        let mut progress = PlanProgress::Prepared;

        progress
            .block_before_authoring()
            .expect("block before an author invocation");

        assert!(matches!(
            progress,
            PlanProgress::Blocked { cancellable: true }
        ));
    }

    #[test]
    fn v2_activation_preflight_blocks_actionable_v1_and_leaves_finished_history_inert() {
        let temp = TempDir::new("legacy-v1-preflight");
        let legacy = temp.path().join("runs");
        let pending = legacy.join("pending");
        let implementing = legacy.join("implementing");
        let finished = legacy.join("finished");
        for (dir, lifecycle, stage) in [
            (&pending, "started", "pending_review"),
            (&implementing, "started", "implementing"),
            (&finished, "finished", "completed"),
        ] {
            std::fs::create_dir_all(dir).expect("legacy dir");
            std::fs::write(
                dir.join("manifest.json"),
                serde_json::json!({
                    "schema": "conductor/run@1",
                    "lifecycle": lifecycle,
                    "work": { "stage": stage }
                })
                .to_string(),
            )
            .expect("legacy manifest");
        }
        let finished_bytes = std::fs::read(finished.join("manifest.json")).expect("read history");
        let preflight = legacy_v1_preflight(temp.path()).expect("classify v1");
        assert_eq!(preflight.pending, 1);
        assert_eq!(preflight.implementing, 1);
        assert_eq!(preflight.reclaimable, 0);
        assert!(!preflight.activation_allowed());
        assert!(
            RunHandle::create_at(temp.path(), RunJob::Consult, new_run_request(), fixed_now())
                .is_err(),
            "v2 activation must refuse unfinished legacy recovery"
        );
        assert_eq!(
            std::fs::read(finished.join("manifest.json")).expect("history retained"),
            finished_bytes,
            "preflight must never mutate finished v1 history"
        );

        std::fs::remove_dir_all(&pending).expect("remove fixture pending");
        std::fs::remove_dir_all(&implementing).expect("remove fixture implementing");
        assert!(
            legacy_v1_preflight(temp.path())
                .expect("classify inert history")
                .activation_allowed()
        );
        RunHandle::create_at(temp.path(), RunJob::Consult, new_run_request(), fixed_now())
            .expect("finished v1 history is inert to v2");
    }

    fn event_values(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .expect("read events")
            .lines()
            .map(|line| serde_json::from_str(line).expect("event JSON"))
            .collect()
    }

    fn write_event_values(path: &Path, rows: &[serde_json::Value]) {
        let mut content = rows
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        content.push('\n');
        std::fs::write(path, content).expect("write events");
    }
}
