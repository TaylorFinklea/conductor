//! `undertake/run@2` manifest + `undertake/event@2` JSONL run artifacts.
//!
//! Every active run lives under `<state_dir>/runs-v2/<run-id>/`: a whole-file
//! atomic `manifest.json` replacement and an append-only `events.jsonl`.
//! Finished `runs/` artifacts are legacy history and are never scanned by the
//! active v2 reader.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::musterroll::RuntimeLimitEvidence;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Schema tag stamped on every manifest written by this module.
pub(crate) const RUN_SCHEMA: &str = "undertake/run@2";
/// Schema tag historically stamped on every event line written by this
/// module. Retained as the read-compatibility value: `read_events` still
/// accepts it, but no code path writes it anymore.
pub(crate) const EVENT_SCHEMA: &str = "undertake/event@2";
/// Schema tag stamped on every event line written by this module. Adds the
/// generic `invocation` evidence field; `@2` journals remain readable via
/// `read_events`'s dual-schema check.
pub(crate) const EVENT_SCHEMA_V3: &str = "undertake/event@3";
const TERMINAL_TRANSITION_PATH: &str = "artifacts/terminal-transition.json";
const WORKER_COMMIT_HOOK_REF_PATH: &str = "worker-commit-hook";

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

/// One event kind from the spec's stable `undertake/event@2` list, extended
/// by `undertake/event@3`.
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
    /// `@3` and later only (bead `conductor-v37z`): one stage's durable
    /// outcome, written by [`crate::runner::AttemptRunner::run`] immediately
    /// after `JobPolicy::transition` returns, before the ledger is used to
    /// pick the next stage. This is what lets a resumed run reconstruct its
    /// [`crate::runner::StageLedger`] from the journal instead of restarting
    /// it empty and replaying already-completed stages. See
    /// [`StageProgress`].
    StageFinished,
}

/// `{"path": ..., "sha256": ...}` artifact identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactRef {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

/// The content-addressed identity of the exact Musterroll snapshot copied into a
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

/// A `(pgid, generation)` worker-group identity recorded for one
/// concurrently in-flight attempt slot, tagged by its zero-based slot index.
/// `work`, `plan`, and `consult` dispatch exactly one slot and continue to
/// record it via the legacy `worker_pgid`/`worker_pgid_generation` pair on
/// [`WorkState`]; a stage that fans out several concurrent workers — the
/// `review` job's reviewer panel — records one entry per slot in
/// [`WorkState::worker_slots`] instead. See
/// [`quarantine::worker_slots_authenticated_live`](crate::quarantine::worker_slots_authenticated_live)
/// for the composed reclaim check: every entry must be provably dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerSlotIdentity {
    pub(crate) slot: u32,
    pub(crate) pgid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) generation: Option<u64>,
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
    /// pid of the `undertake` process that created this run, recorded once
    /// at creation and never mutated — the same OS process drives worker
    /// dispatch, mechanical verification, and qualitative review for a run's
    /// entire lifetime, so this single value authenticates ownership across
    /// all of those stages. Absent on manifests written before this field
    /// existed; recovery logic must treat that absence as weaker evidence,
    /// not as proof of death (mirrors `before_head`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner_pid: Option<u32>,
    /// The kernel-reported process generation (start time) `owner_pid` had
    /// when it was recorded — see `quarantine::process_generation`. A bare
    /// pid cannot tell "the owner I recorded is still running" apart from
    /// "the OS has since handed this pid number to an unrelated process
    /// after the owner crashed"; binding a generation closes that hole.
    /// Absent on manifests written before this field existed, or when the
    /// owner's own generation could not be determined at creation — either
    /// way, authentication conservatively treats the pid alone as it did
    /// before this field existed (mirrors `before_head`/`owner_pid`), it
    /// never invents liveness or death from a missing generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner_pid_generation: Option<u64>,
    /// Process-group id of the currently dispatched worker, recorded via
    /// [`RunHandle::record_worker_group`] immediately after each worker is
    /// spawned (workers lead their own process group, so the group id equals
    /// the worker pid) and before that worker can meaningfully mutate the
    /// repository. A dead `undertake` owner is *not* proof that a separately
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
    /// The kernel-reported process generation `worker_pgid`'s leader had
    /// when the group was recorded (see `owner_pid_generation` for why a
    /// bare pgid alone cannot authenticate a recorded owner). Cleared and
    /// re-bound in lockstep with `worker_pgid` by
    /// [`RunHandle::invalidate_worker_group`] and
    /// [`RunHandle::record_worker_group`]. Absent on manifests written
    /// before this field existed, or when the leader's generation could not
    /// be determined at spawn time — authentication then falls back to the
    /// pre-generation behavior, never inventing liveness or death.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worker_pgid_generation: Option<u64>,
    /// Per-slot worker-group identities for a stage that dispatches more
    /// than one concurrent worker — additive to, and never written
    /// alongside, `worker_pgid`/`worker_pgid_generation` above. A single-slot
    /// job (`work`, `plan`, `consult`) leaves this empty and keeps recording
    /// its one identity through the legacy pair, exactly as before this
    /// field existed; a multi-slot job records one [`WorkerSlotIdentity`]
    /// per slot here and leaves the legacy pair `None`. Empty on every
    /// manifest written before this field existed. See
    /// [`WorkState::effective_worker_slots`] for the read-side rule that
    /// reconciles the two representations, and
    /// `quarantine::worker_slots_authenticated_live` for the reclaim
    /// composition: every recorded entry, from whichever representation is
    /// in use, must be provably dead before a stranded run's identity can be
    /// treated as gone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) worker_slots: Vec<WorkerSlotIdentity>,
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

impl WorkState {
    /// The effective per-slot worker-group identity set for this run's
    /// current attempt: `worker_slots` verbatim when it has any entries,
    /// otherwise the legacy `worker_pgid`/`worker_pgid_generation` pair
    /// reinterpreted as a single slot 0 (or empty when neither has ever been
    /// recorded). This is the one place the two representations are
    /// reconciled — every caller that needs "the worker identity/identities
    /// recorded for this run" reads through here rather than choosing
    /// between the fields itself, so a caller can never accidentally check
    /// only one representation and miss the other.
    pub(crate) fn effective_worker_slots(&self) -> Vec<WorkerSlotIdentity> {
        if !self.worker_slots.is_empty() {
            return self.worker_slots.clone();
        }
        self.worker_pgid
            .map(|pgid| {
                vec![WorkerSlotIdentity {
                    slot: 0,
                    pgid,
                    generation: self.worker_pgid_generation,
                }]
            })
            .unwrap_or_default()
    }
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
/// Provider-diversity policy pinned for a delayed plan stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanProviderDiversity {
    None,
    /// Prefer a cross-provider peer; a different exact execution on the
    /// author's provider is legal only when no cross-provider candidate lives.
    CrossProviderOrDegraded,
    /// A spec's author, peer, and final opinion must use pairwise-distinct
    /// providers. There is no degraded success path.
    PairwiseDistinct,
}

/// Immutable relationship constraints for a delayed plan stage. Referenced
/// stages are bound only once their concrete execution is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanStageConstraints {
    pub(crate) distinct_execution_from: Vec<PlanStage>,
    pub(crate) tier_at_least: Vec<PlanStage>,
    pub(crate) provider_diversity: PlanProviderDiversity,
}

impl PlanStageConstraints {
    pub(crate) const fn unconstrained() -> Self {
        Self {
            distinct_execution_from: Vec::new(),
            tier_at_least: Vec::new(),
            provider_diversity: PlanProviderDiversity::None,
        }
    }
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
    pub(crate) constraints: PlanStageConstraints,
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

/// Persisted per-stage call counters. Schema repairs and backend failures are
/// calls too, so every transition can enforce the approved bounded budget after
/// a crash or resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanStageAttempts {
    pub(crate) planner: u8,
    pub(crate) peer_review: u8,
    pub(crate) second_opinion: u8,
}

impl PlanStageAttempts {
    fn record(&mut self, stage: PlanStage, limit: StageAttemptLimit) -> Result<u8> {
        let count = match stage {
            PlanStage::Planner => &mut self.planner,
            PlanStage::PeerReview => &mut self.peer_review,
            PlanStage::SecondOpinion => &mut self.second_opinion,
        };
        *count = count
            .checked_add(1)
            .ok_or_else(|| RunError::new("plan stage attempt counter overflow"))?;
        if *count > limit.value() {
            return Err(RunError::new("plan stage attempt limit exhausted"));
        }
        Ok(*count)
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_binding_run_id: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        second: Option<Box<ApprovedExecution>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        second_binding_run_id: Option<String>,
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
            peer_binding_run_id: None,
            artifact,
            revisions: RevisionLimit::new(0)?,
        };
        Ok(())
    }
    pub(crate) fn bind_peer(
        &mut self,
        peer: ApprovedExecution,
        binding_run_id: String,
    ) -> Result<()> {
        let Self::AwaitingPeer {
            author,
            peer: bound_peer,
            peer_binding_run_id: bound_binding_run_id,
            ..
        } = self
        else {
            return Err(RunError::new(
                "peer can bind only while awaiting peer review",
            ));
        };
        if peer.execution_key == author.execution_key {
            return Err(RunError::new(
                "peer binding must use a distinct exact execution from immutable author",
            ));
        }
        if let Some(bound) = bound_peer {
            if bound != &peer {
                return Err(RunError::new(
                    "peer binding cannot change after reservation",
                ));
            }
            if bound_binding_run_id.as_deref() != Some(binding_run_id.as_str()) {
                return Err(RunError::new(
                    "peer binding reservation cannot change after reservation",
                ));
            }
        } else {
            *bound_peer = Some(peer);
            *bound_binding_run_id = Some(binding_run_id);
        }
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
            ..
        } = self
        else {
            return Err(RunError::new(
                "peer verdict is not legal in this plan state",
            ));
        };
        if peer.execution_key == author.execution_key {
            return Err(RunError::new(
                "peer binding must use a distinct exact execution from immutable author",
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
                second: None,
                second_binding_run_id: None,
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
            peer_binding_run_id: None,
            artifact,
            revisions,
        };
        Ok(())
    }
    pub(crate) fn bind_second_opinion(
        &mut self,
        second: ApprovedExecution,
        binding_run_id: String,
    ) -> Result<()> {
        let Self::AwaitingSecondOpinion {
            author,
            peer,
            second: bound,
            second_binding_run_id: bound_binding_run_id,
            ..
        } = self
        else {
            return Err(RunError::new(
                "second opinion can bind only while awaiting second opinion",
            ));
        };
        if second.execution_key == author.execution_key
            || second.execution_key == peer.execution_key
        {
            return Err(RunError::new(
                "second opinion binding must use a distinct exact execution",
            ));
        }
        if second.provider_id == author.provider_id || second.provider_id == peer.provider_id {
            return Err(RunError::new(
                "second opinion binding must use a pairwise-distinct provider",
            ));
        }
        if let Some(existing) = bound {
            if existing.as_ref() != &second {
                return Err(RunError::new(
                    "second opinion binding cannot change after reservation",
                ));
            }
            if bound_binding_run_id.as_deref() != Some(binding_run_id.as_str()) {
                return Err(RunError::new(
                    "second opinion reservation cannot change after reservation",
                ));
            }
        } else {
            *bound = Some(Box::new(second));
            *bound_binding_run_id = Some(binding_run_id);
        }
        Ok(())
    }

    pub(crate) fn record_second_opinion(
        &mut self,
        second: &ApprovedExecution,
        verdict: SecondOpinionVerdict,
    ) -> Result<()> {
        let Self::AwaitingSecondOpinion {
            second: Some(bound),
            ..
        } = self
        else {
            return Err(RunError::new(
                "second opinion verdict requires a durable second-opinion binding",
            ));
        };
        if bound.as_ref() != second {
            return Err(RunError::new(
                "second opinion verdict must use the durable bound identity",
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
    #[serde(default)]
    pub(crate) stage_attempts: PlanStageAttempts,
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

/// `undertake/run@2` — the atomic, versioned run manifest.
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
    pub(crate) musterroll_roster_artifact: Option<ArtifactRef>,
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

/// Typed evidence for one plan backend invocation. Start and finish events
/// share the immutable identity and input digest; the finish event adds the
/// observed output digest, duration, and any harness-reported token count.
///
/// Read-compatibility only as of `undertake/event@3`: historical `@2`
/// journals carry this shape under [`RunEvent::plan_invocation`], but no
/// code path writes it anymore — new writes (plan included) use the
/// generic [`InvocationEvidence`] under [`RunEvent::invocation`] instead.
/// Kept byte-for-byte so those journals keep deserializing under
/// `#[serde(deny_unknown_fields)]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanInvocationEvidence {
    pub(crate) role: String,
    pub(crate) stage: PlanStage,
    pub(crate) execution: ApprovedExecution,
    pub(crate) input_sha256: String,
    pub(crate) output_sha256: Option<String>,
    pub(crate) attempt: u8,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) tokens: Option<u64>,
}

/// Generic per-invocation evidence for one attempt, shared by every job
/// (work, review, consult, plan). Introduced in `undertake/event@3` as the
/// job-agnostic replacement for [`PlanInvocationEvidence`], which stays
/// plan-shaped and read-only. `stage` is a `snake_case` stage id rather
/// than a job-specific enum so no job's evidence type leaks into `run.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvocationEvidence {
    pub(crate) stage: String,
    pub(crate) slot: u32,
    pub(crate) attempt: u32,
    pub(crate) execution: ApprovedExecution,
    pub(crate) input_sha256: String,
    pub(crate) output_sha256: Option<String>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) tokens: Option<u64>,
    /// `event_id` of the attempt this one retries; `None` on a first
    /// attempt.
    pub(crate) retry_of: Option<String>,
}

/// Generic, job-agnostic terminal-state discriminator carried on a
/// `run_finished` event, `@3` and later. This is what makes reconciliation
/// (see [`reconcile_terminal_manifest`]) able to reconstruct *any* job's
/// terminal `RunDetails`, not just `work`'s: `outcome` is free text chosen
/// per call site for human/Bead-facing display, and different call sites
/// legitimately use different strings for the *same* structural verdict
/// (`finish_plan_blocked` writes `"blocked"`, `cancel_prepared_plan` and
/// `cancel_failed_authoring_plan` write `"canceled"`, and all three set the
/// identical [`PlanTerminalVerdict::Blocked`]) — so reconciliation must
/// never parse `outcome` as a discriminator. This enum is that
/// discriminator instead, mirrored from the `Terminal` shape in
/// `.docs/ai/phases/undertake-runner-contract.md`'s "`Terminal` — replaces
/// `Completed | Failed`" section (which additionally carries a `reason` on
/// `Blocked`/`NeedsInput`; that reason already lives in `outcome`, so it is
/// not duplicated here). Every job maps its own verdict type onto this set;
/// `plan`'s mapping is `Accepted -> Completed`, `Rejected -> Failed`,
/// `Blocked -> Blocked`, `NeedsInput -> NeedsInput` (see
/// [`plan_terminal_verdict_from_generic`]). `Canceled` is unused by `plan`
/// today (its two "canceled" call sites are structurally `Blocked`) and is
/// carried here only because the contract names it as part of the shared
/// set; `work`, `review`, and `consult` do not populate this field yet —
/// only `plan`'s `RunDetails` has verdict-shaped mutable state today, so
/// [`reconcile_terminal_manifest`] requires it only for `plan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalVerdict {
    Completed,
    Failed,
    Blocked,
    NeedsInput,
    Canceled,
}

/// Which of the runner's two [`crate::runner::Transition`] arms produced the
/// [`StageProgress`] this accompanies. `@3` and later only (bead
/// `conductor-v37z`).
///
/// A typed discriminator rather than free text: the `44hc` work (the prior
/// generic-terminal-reconciliation bead) established that `RunEvent::outcome`
/// is chosen per call site for human/Bead-facing display and is therefore
/// unsafe to parse as a structural discriminator (see [`TerminalVerdict`]'s
/// own doc comment, which exists for exactly that reason). The same
/// objection applies here: a resumed run must be able to tell whether the
/// last recorded stage ended via `Transition::Continue` (so `JobPolicy::
/// next_stage` should be asked what runs next) or `Transition::Terminal` (so
/// it must not be asked at all -- see [`StageProgress`]'s doc comment) without
/// ever guessing that distinction from prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StageTransitionKind {
    Continue,
    Terminal,
}

/// Durable evidence of one stage's completion, carried on an
/// [`EventKind::StageFinished`] event's [`RunEvent::stage_progress`]. `@3`
/// and later only (bead `conductor-v37z`). `stage` is a `snake_case` stage id
/// string, mirroring [`InvocationEvidence::stage`], rather than a job-specific
/// enum, so no job's stage vocabulary leaks into `run.rs`.
///
/// The event's own `artifact_refs` (already a general-purpose field on
/// [`RunEvent`]) carry the stage's output identities; this struct adds only
/// what `artifact_refs` cannot express: which stage they belong to, and
/// which transition produced them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageProgress {
    pub(crate) stage: String,
    pub(crate) transition: StageTransitionKind,
}

/// `undertake/event@2` / `undertake/event@3` — one append-only event line.
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
    /// `@2` read-compatibility only; new writes always leave this `None`.
    /// See [`PlanInvocationEvidence`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plan_invocation: Option<PlanInvocationEvidence>,
    /// Generic per-invocation evidence, `@3` and later. `#[serde(default)]`
    /// is what lets `@2` lines, which never had this key, keep
    /// deserializing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) invocation: Option<InvocationEvidence>,
    /// Generic terminal-state discriminator, `@3` and later; only ever
    /// `Some` on a [`EventKind::RunFinished`] event. See [`TerminalVerdict`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_verdict: Option<TerminalVerdict>,
    /// One stage's durable completion evidence, `@3` and later; only ever
    /// `Some` on a [`EventKind::StageFinished`] event. See [`StageProgress`].
    /// `#[serde(default)]` is what lets `@2` lines, and any `@3` line written
    /// before this bead, keep deserializing with no `StageFinished` events
    /// at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stage_progress: Option<StageProgress>,
}

/// Fields pinned into a new run's manifest at creation.
#[derive(Debug, Clone, Default)]
pub(crate) struct NewRun {
    pub(crate) target: RunTarget,
    pub(crate) approved_profiles: Vec<String>,
    pub(crate) musterroll_roster_artifact: Option<ArtifactRef>,
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
    pub(crate) musterroll_roster_artifact: Option<ArtifactRef>,
    pub(crate) roster_snapshot: RosterSnapshotInput,
    pub(crate) limits: RunLimits,
    pub(crate) verifier: RunVerifier,
    pub(crate) approval: serde_json::Value,
    pub(crate) input_bytes: Vec<u8>,
}

/// Fields for one event row; `run_id`, `seq`, `ts`, `job`, and `target` are
/// filled in by the owning [`RunHandle`]. Never carries `plan_invocation` —
/// that field is `@2` read-compatibility only, so [`RunEvent`] always
/// writes it as `None`; use `invocation` instead.
#[derive(Debug, Clone, Default)]
pub(crate) struct EventInput {
    pub(crate) profile_id: Option<String>,
    pub(crate) artifact_refs: Vec<ArtifactRef>,
    pub(crate) outcome: Option<String>,
    pub(crate) provider_limit: Option<RuntimeLimitEvidence>,
    pub(crate) invocation: Option<InvocationEvidence>,
    /// See [`RunEvent::terminal_verdict`]; only meaningful on a
    /// [`EventKind::RunFinished`] input, set via [`RunHandle::finish_with_verdict`].
    pub(crate) terminal_verdict: Option<TerminalVerdict>,
    /// See [`RunEvent::stage_progress`]; only meaningful on a
    /// [`EventKind::StageFinished`] input.
    pub(crate) stage_progress: Option<StageProgress>,
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
            musterroll_roster_artifact,
            roster_snapshot,
            limits,
            verifier,
            work,
            approval,
        } = request;
        if let Some(artifact) = musterroll_roster_artifact.as_ref() {
            validate_artifact_ref(artifact, "musterroll roster artifact")?;
        }
        if let Some(snapshot) = roster_snapshot.as_ref() {
            validate_sha256(&snapshot.policy_sha256, "roster policy")?;
            let parsed = crate::musterroll::parse_roster_snapshot(&snapshot.bytes)
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
            musterroll_roster_artifact,
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
            if let Some(roster) = handle.manifest.musterroll_roster_artifact.clone() {
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
                && handle.manifest.musterroll_roster_artifact.is_none()
            {
                handle.append_event_at(
                    EventKind::CoverageGap,
                    EventInput {
                        outcome: Some("musterroll_roster_artifact_unavailable".to_string()),
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
            musterroll_roster_artifact,
            roster_snapshot,
            limits,
            verifier,
            approval,
            input_bytes,
        } = request;
        validate_run_id(&run_id)?;
        validate_sha256(&roster_snapshot.policy_sha256, "roster policy")?;
        let parsed = crate::musterroll::parse_roster_snapshot(&roster_snapshot.bytes)
            .map_err(|error| RunError::new(format!("invalid roster snapshot: {error}")))?;
        if parsed.policy_sha256() != roster_snapshot.policy_sha256 {
            return Err(RunError::new(
                "roster snapshot policy_sha256 does not match prepared policy",
            ));
        }
        if let Some(artifact) = musterroll_roster_artifact.as_ref() {
            validate_artifact_ref(artifact, "musterroll roster artifact")?;
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
            musterroll_roster_artifact,
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
            if let Some(roster) = handle.manifest.musterroll_roster_artifact.clone() {
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

    /// The pid recorded at creation for this run's owning `undertake`
    /// process, if any (see [`WorkState::owner_pid`]).
    pub(crate) fn owner_pid(&self) -> Option<u32> {
        self.work().and_then(|work| work.owner_pid)
    }

    /// The process generation `owner_pid` had when it was recorded, if any
    /// (see [`WorkState::owner_pid_generation`]).
    pub(crate) fn owner_pid_generation(&self) -> Option<u64> {
        self.work().and_then(|work| work.owner_pid_generation)
    }

    /// The process-group id of the most recently spawned worker, if one has
    /// been recorded yet (see [`WorkState::worker_pgid`]).
    pub(crate) fn worker_pgid(&self) -> Option<u32> {
        self.work().and_then(|work| work.worker_pgid)
    }

    /// The process generation `worker_pgid`'s leader had when the group was
    /// recorded, if any (see [`WorkState::worker_pgid_generation`]).
    pub(crate) fn worker_pgid_generation(&self) -> Option<u64> {
        self.work().and_then(|work| work.worker_pgid_generation)
    }

    /// Durably names the one commit hook the next worker attempt may execute.
    ///
    /// The reference is persisted before the hook directory is created, so a
    /// concurrent collector cannot mistake an attempt being prepared for stale
    /// history. Replacing it also returns the superseded hook for prompt
    /// bounded cleanup after the prior worker has been proven quiescent.
    pub(crate) fn prepare_worker_commit_hook(
        &mut self,
        hook_name: &str,
    ) -> Result<Option<String>> {
        validate_worker_commit_hook_name(hook_name)?;
        let previous = read_worker_commit_hook(&self.dir)?;
        self.invalidate_worker_group()?;
        let mut bytes = hook_name.as_bytes().to_vec();
        bytes.push(b'\n');
        atomic_replace(&self.dir.join(WORKER_COMMIT_HOOK_REF_PATH), &bytes)?;
        Ok(previous.filter(|name| name != hook_name))
    }

    /// Durably releases the current hook only after dispatch has proven the
    /// worker and its descendants quiescent.
    pub(crate) fn clear_worker_commit_hook(&mut self, hook_name: &str) -> Result<()> {
        validate_worker_commit_hook_name(hook_name)?;
        if matches!(self.manifest.lifecycle, RunLifecycle::Finished) {
            return Err(RunError::new(
                "cannot clear a worker commit hook on a finished run",
            ));
        }
        let work = self
            .work()
            .ok_or_else(|| RunError::new("clearing a worker commit hook requires work state"))?;
        if work.stage != WorkStage::Implementing {
            return Err(RunError::new(
                "worker commit hook can only be cleared while implementing",
            ));
        }
        let current = read_worker_commit_hook(&self.dir)?;
        if current.as_deref() != Some(hook_name) {
            return Err(RunError::new(format!(
                "worker commit hook {hook_name:?} is not the current attempt hook"
            )));
        }
        atomic_replace(&self.dir.join(WORKER_COMMIT_HOOK_REF_PATH), b"\n")
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
        work.worker_pgid_generation = None;
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
    ///
    /// Also captures the leader's process generation at this instant (see
    /// [`WorkState::worker_pgid_generation`]) — probing it here, immediately
    /// after the spawn this method is documented to follow, is race-free:
    /// the pid cannot yet have been recycled.
    pub(crate) fn record_worker_group(&mut self, pgid: u32) -> Result<()> {
        if matches!(self.manifest.lifecycle, RunLifecycle::Finished) {
            return Err(RunError::new(
                "cannot record a worker group on a finished run",
            ));
        }
        let generation = crate::quarantine::process_generation(pgid);
        let work = self.work_mut("recording a worker group")?;
        if work.stage != WorkStage::Implementing {
            return Err(RunError::new(
                "worker group can only be recorded while implementing",
            ));
        }
        work.worker_pgid = Some(pgid);
        work.worker_pgid_generation = generation;
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
        if !matches!(
            self.plan()?.progress,
            PlanProgress::Authoring { .. }
                | PlanProgress::AwaitingPeer { .. }
                | PlanProgress::Revising { .. }
                | PlanProgress::AwaitingSecondOpinion { .. }
        ) {
            return Err(RunError::new(
                "terminal plan block requires an active plan stage",
            ));
        }
        self.plan_mut("blocking active plan")?.progress = PlanProgress::Terminal {
            verdict: PlanTerminalVerdict::Blocked,
        };
        self.finish_with_verdict("blocked", TerminalVerdict::Blocked, Vec::new())
    }

    /// Persists each author invocation before the harness starts, so a crash
    /// can resume only within the approved bounded attempt budget.
    pub(crate) fn record_plan_author_attempt(&mut self) -> Result<()> {
        let attempt_limit = self.plan()?.stage_attempt_limit;
        let plan = self.plan_mut("recording plan author attempt")?;
        plan.stage_attempts
            .record(PlanStage::Planner, attempt_limit)?;
        plan.progress.record_author_attempt(attempt_limit)?;
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

    /// Persists an authorized peer, second-opinion, or revision call before it
    /// starts. This is the crash/resume budget boundary for every non-initial
    /// plan backend invocation.
    pub(crate) fn record_plan_stage_attempt(&mut self, stage: PlanStage) -> Result<u8> {
        let attempt_limit = self.plan()?.stage_attempt_limit;
        let progress = &self.plan()?.progress;
        let legal = matches!(
            (stage, progress),
            (PlanStage::Planner, PlanProgress::Revising { .. })
                | (PlanStage::PeerReview, PlanProgress::AwaitingPeer { .. })
                | (
                    PlanStage::SecondOpinion,
                    PlanProgress::AwaitingSecondOpinion { .. }
                )
        );
        if !legal {
            return Err(RunError::new(
                "plan stage attempt is not legal in the current plan state",
            ));
        }
        let attempt = self
            .plan_mut("recording plan stage attempt")?
            .stage_attempts
            .record(stage, attempt_limit)?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        Ok(attempt)
    }

    /// Applies a validated peer verdict and either advances to the required
    /// next gate, starts the same-author revision, or terminally preserves the
    /// rejected artifact when the authorized revision budget is exhausted.
    pub(crate) fn record_plan_peer_verdict(
        &mut self,
        peer: ApprovedExecution,
        verdict: PeerVerdict,
        require_second_opinion: bool,
    ) -> Result<()> {
        let (artifact, exhausted) = match self.plan()?.progress.clone() {
            PlanProgress::AwaitingPeer {
                artifact,
                revisions,
                ..
            } => (
                artifact,
                matches!(verdict, PeerVerdict::Revise)
                    && revisions.value() >= self.plan()?.revision_limit.value(),
            ),
            _ => {
                return Err(RunError::new(
                    "peer verdict requires a plan awaiting peer review",
                ));
            }
        };
        // The non-terminal `ReviewFinished` event is appended with progress
        // still in its pre-verdict shape -- the mutation to a terminal (or
        // `Revising`/`AwaitingSecondOpinion`) shape happens only after, so a
        // crash between the two event appends below never lets a terminal
        // `PlanProgress` reach disk ahead of the `run_finished` event that
        // must accompany it (see `finish_with_verdict`'s doc comment).
        if !exhausted {
            self.plan_mut("recording plan peer verdict")?
                .progress
                .record_peer_verdict(peer.clone(), verdict)?;
        }
        self.append_event(
            EventKind::ReviewFinished,
            EventInput {
                profile_id: Some(peer.profile_id),
                artifact_refs: vec![artifact.clone()],
                outcome: Some(match verdict {
                    PeerVerdict::Approve if require_second_opinion => "peer_approved".to_string(),
                    PeerVerdict::Approve => "accepted".to_string(),
                    PeerVerdict::Revise if exhausted => "revision_exhausted".to_string(),
                    PeerVerdict::Revise => "revision_required".to_string(),
                }),
                ..EventInput::default()
            },
        )?;
        if exhausted {
            self.plan_mut("finishing exhausted plan revision")?.progress = PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::Rejected,
            };
            self.finish_with_verdict("rejected", TerminalVerdict::Failed, vec![artifact])
        } else if matches!(verdict, PeerVerdict::Approve) && !require_second_opinion {
            self.plan_mut("accepting implementation plan")?.progress = PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::Accepted,
            };
            self.finish_with_verdict("accepted", TerminalVerdict::Completed, vec![artifact])
        } else {
            Ok(())
        }
    }

    /// Checkpoints one validated same-author revision and returns to the
    /// already-bound peer. The revision count changes only at this durable
    /// successful transition.
    pub(crate) fn complete_plan_revision(&mut self, artifact: ArtifactRef) -> Result<()> {
        validate_artifact_ref(&artifact, "plan revision artifact")?;
        validate_local_artifact(&self.manifest_path(), &artifact)?;
        self.plan_mut("completing plan revision")?
            .progress
            .complete_revision(artifact.clone())?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()?;
        self.append_event(
            EventKind::AttemptFinished,
            EventInput {
                artifact_refs: vec![artifact],
                outcome: Some("plan_revision_ready".to_string()),
                ..EventInput::default()
            },
        )
    }
    /// Persists the exact peer identity before its first invocation so a later
    /// resume cannot select a different reviewer.
    pub(crate) fn bind_plan_peer(
        &mut self,
        peer: ApprovedExecution,
        binding_run_id: String,
    ) -> Result<()> {
        self.plan_mut("binding plan peer")?
            .progress
            .bind_peer(peer, binding_run_id)?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()
    }

    /// Persists the exact pairwise-distinct second-opinion identity before its
    /// first invocation so a later resume cannot select a different reviewer.
    pub(crate) fn bind_plan_second_opinion(
        &mut self,
        second: ApprovedExecution,
        binding_run_id: String,
    ) -> Result<()> {
        self.plan_mut("binding plan second opinion")?
            .progress
            .bind_second_opinion(second, binding_run_id)?;
        self.manifest.updated_at = Utc::now().to_rfc3339();
        self.write_manifest()
    }

    /// Records a strict final spec opinion. A final reject is terminal and
    /// never re-opens the peer/revision loop.
    pub(crate) fn record_plan_second_opinion(
        &mut self,
        second: &ApprovedExecution,
        verdict: SecondOpinionVerdict,
    ) -> Result<()> {
        let PlanProgress::AwaitingSecondOpinion {
            author,
            peer,
            artifact,
            ..
        } = self.plan()?.progress.clone()
        else {
            return Err(RunError::new(
                "second opinion requires a plan awaiting second opinion",
            ));
        };
        if second.execution_key == author.execution_key
            || second.execution_key == peer.execution_key
        {
            return Err(RunError::new(
                "second opinion must use a distinct exact execution",
            ));
        }
        if second.provider_id == author.provider_id || second.provider_id == peer.provider_id {
            return Err(RunError::new(
                "second opinion must use a pairwise-distinct provider",
            ));
        }
        // Append the non-terminal `ReviewFinished` event before mutating
        // progress to its terminal shape (`record_second_opinion` always
        // lands on `PlanProgress::Terminal`, unlike the peer-verdict path),
        // so a crash between the two event appends below never lets that
        // terminal mutation reach disk ahead of the `run_finished` event
        // that must accompany it (see `finish_with_verdict`'s doc comment).
        self.append_event(
            EventKind::ReviewFinished,
            EventInput {
                profile_id: Some(second.profile_id.clone()),
                artifact_refs: vec![artifact.clone()],
                outcome: Some(match verdict {
                    SecondOpinionVerdict::Accept => "accepted".to_string(),
                    SecondOpinionVerdict::Reject => "rejected".to_string(),
                }),
                ..EventInput::default()
            },
        )?;
        self.plan_mut("recording plan second opinion")?
            .progress
            .record_second_opinion(second, verdict)?;
        self.finish_with_verdict(
            match verdict {
                SecondOpinionVerdict::Accept => "accepted",
                SecondOpinionVerdict::Reject => "rejected",
            },
            match verdict {
                SecondOpinionVerdict::Accept => TerminalVerdict::Completed,
                SecondOpinionVerdict::Reject => TerminalVerdict::Failed,
            },
            vec![artifact],
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
        self.finish_with_verdict("canceled", TerminalVerdict::Blocked, Vec::new())
    }

    pub(crate) fn cancel_failed_authoring_plan(&mut self) -> Result<()> {
        if !matches!(self.plan()?.progress, PlanProgress::Authoring { .. }) {
            return Err(RunError::new(
                "failed-authoring cancellation requires active authoring state",
            ));
        }
        self.plan_mut("canceling failed authoring plan")?.progress =
            PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::Blocked,
            };
        self.finish_with_verdict("canceled", TerminalVerdict::Blocked, Vec::new())
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
        self.finish_with_verdict("needs_input", TerminalVerdict::NeedsInput, vec![artifact])
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
        if input.terminal_verdict.is_some() && !matches!(kind, EventKind::RunFinished) {
            return Err(RunError::new(
                "terminal verdict is only legal on a run_finished event",
            ));
        }
        if input.stage_progress.is_some() && !matches!(kind, EventKind::StageFinished) {
            return Err(RunError::new(
                "stage progress is only legal on a stage_finished event",
            ));
        }
        let seq = self.next_seq;
        let event = RunEvent {
            schema: EVENT_SCHEMA_V3.to_string(),
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
            plan_invocation: None,
            invocation: input.invocation,
            terminal_verdict: input.terminal_verdict,
            stage_progress: input.stage_progress,
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
        self.finish_terminal(outcome, None, artifact_refs)
    }

    /// Same terminal write as [`Self::finish_with_artifacts`], additionally
    /// pinning the generic [`TerminalVerdict`] a job needs to reconstruct
    /// its own `RunDetails` terminal shape from the journal alone (see
    /// [`reconcile_terminal_manifest`]). Use this instead of
    /// `finish`/`finish_with_artifacts` whenever the caller's `RunDetails`
    /// variant carries verdict-shaped mutable state — today, `plan` only.
    pub(crate) fn finish_with_verdict(
        &mut self,
        outcome: impl Into<String>,
        terminal_verdict: TerminalVerdict,
        artifact_refs: Vec<ArtifactRef>,
    ) -> Result<()> {
        self.finish_terminal(outcome, Some(terminal_verdict), artifact_refs)
    }

    /// The uniform terminal write order for every job (work, review,
    /// consult, plan): the durable `run_finished` journal event is appended
    /// FIRST (`append_event` -> `append_event_line`, fsynced), and only
    /// then does the single atomic `write_manifest()` at the end of
    /// `append_event_at` persist the terminal projection — lifecycle,
    /// outcome, and (via the in-memory mutation a caller made just before
    /// calling this) any job-specific terminal state, e.g.
    /// `PlanProgress::Terminal`. That "just before calling this" ordering is
    /// load-bearing: a caller must mutate its own `RunDetails` to its
    /// terminal shape no earlier than immediately before invoking
    /// `finish`/`finish_with_artifacts`/`finish_with_verdict`, and must
    /// never call `write_manifest()` on that mutation itself. Doing so would
    /// let the terminal mutation reach disk ahead of the journal event that
    /// is supposed to prove it — exactly the defect this method closes: six
    /// `plan` call sites used to write `progress = Terminal { .. }` via
    /// their own explicit `write_manifest()` before ever appending
    /// `run_finished` (some even before an intervening non-terminal event
    /// like `ReviewFinished`), so a crash in that window left a manifest
    /// claiming a terminal verdict under a `Running` lifecycle with no
    /// terminal event to justify it — a skew `RunHandle::open` could not
    /// detect (it only reconciles when the *last* journaled event is
    /// `run_finished`) and therefore passed through as silently resumable.
    /// Under this order, that same crash instead leaves the *journal* ahead
    /// of the *manifest* (event durable, terminal `write_manifest()` never
    /// ran) — a case `open` already detects via `reconcile_terminal_manifest`,
    /// which this change extends to repair every `RunDetails` variant, not
    /// only `work`.
    fn finish_terminal(
        &mut self,
        outcome: impl Into<String>,
        terminal_verdict: Option<TerminalVerdict>,
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
                invocation: None,
                terminal_verdict,
                stage_progress: None,
            },
        )
    }

    /// Touches this run's heartbeat file, recording that the `undertake`
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
    if event.job == RunJob::Plan && event.terminal_verdict.is_none() {
        return Err(RunError::new(
            "plan terminal event is missing its generic terminal verdict",
        ));
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

/// Repairs a manifest that lags the journal's terminal `run_finished` event
/// -- the crash window [`RunHandle::finish_with_verdict`]'s doc comment
/// (see the private `finish_terminal` it and `finish`/`finish_with_artifacts`
/// share) names as the one this whole write-order fix targets: the process
/// stopped after the terminal event was durably appended but before the
/// terminal `write_manifest()` that must follow it landed. Every
/// `RunDetails` variant is repaired here, not only `work`'s: `review` and
/// `consult` carry no verdict-shaped mutable state yet, so the generic
/// lifecycle/outcome/artifact repair below already fully reconciles them;
/// `plan`'s `PlanProgress` is verdict-shaped, so it is rebuilt from the
/// event's [`TerminalVerdict`] via [`plan_terminal_verdict_from_generic`] --
/// [`validate_terminal_event`] (already run by every caller of this
/// function) guarantees a `plan` terminal event carries one.
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
    match &mut manifest.details {
        RunDetails::Work { state: Some(work) } => {
            work.stage = WorkStage::Completed;
        }
        RunDetails::Work { state: None }
        | RunDetails::Review { .. }
        | RunDetails::Consult { .. } => {}
        RunDetails::Plan { state } => {
            let verdict = terminal.terminal_verdict.ok_or_else(|| {
                RunError::new("plan terminal event is missing its generic terminal verdict")
            })?;
            state.progress = PlanProgress::Terminal {
                verdict: plan_terminal_verdict_from_generic(verdict)?,
            };
        }
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

/// The inverse of the mapping documented on [`TerminalVerdict`], used by
/// [`reconcile_terminal_manifest`] to rebuild [`PlanProgress::Terminal`].
/// `Canceled` is unreachable because no `plan` terminal call site ever
/// writes it -- both of `plan`'s "canceled" outcomes are structurally
/// [`PlanTerminalVerdict::Blocked`].
fn plan_terminal_verdict_from_generic(verdict: TerminalVerdict) -> Result<PlanTerminalVerdict> {
    match verdict {
        TerminalVerdict::Completed => Ok(PlanTerminalVerdict::Accepted),
        TerminalVerdict::Failed => Ok(PlanTerminalVerdict::Rejected),
        TerminalVerdict::Blocked => Ok(PlanTerminalVerdict::Blocked),
        TerminalVerdict::NeedsInput => Ok(PlanTerminalVerdict::NeedsInput),
        TerminalVerdict::Canceled => Err(RunError::new(
            "plan runs never reach the generic canceled terminal verdict",
        )),
    }
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
        if value.get("schema").and_then(serde_json::Value::as_str) != Some("undertake/run@1") {
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

/// Returns whether one unfinished Work run still names `hook_name` as the
/// hook its current attempt may execute. Missing and finished runs cannot
/// retain hook storage; malformed liveness evidence fails closed with `Err`.
pub(crate) fn worker_commit_hook_is_current(
    state_dir: &Path,
    run_id: &str,
    hook_name: &str,
) -> Result<bool> {
    validate_run_id(run_id)?;
    validate_worker_commit_hook_name(hook_name)?;
    let run_dir = runs_dir(state_dir).join(run_id);
    let manifest_path = run_dir.join("manifest.json");
    let manifest = match std::fs::symlink_metadata(&manifest_path) {
        Ok(_) => read_manifest(&manifest_path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(RunError::new(format!(
                "failed to inspect manifest {}: {error}",
                manifest_path.display()
            )));
        }
    };
    if manifest.run_id != run_id {
        return Err(RunError::new(format!(
            "manifest run_id {:?} does not match hook owner {run_id:?}",
            manifest.run_id
        )));
    }
    if manifest.job != RunJob::Work || manifest.lifecycle == RunLifecycle::Finished {
        return Ok(false);
    }
    Ok(read_worker_commit_hook(&run_dir)?.as_deref() == Some(hook_name))
}

fn read_worker_commit_hook(run_dir: &Path) -> Result<Option<String>> {
    let path = run_dir.join(WORKER_COMMIT_HOOK_REF_PATH);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RunError::new(format!(
                "failed to read worker commit hook reference {}: {error}",
                path.display()
            )));
        }
    };
    let hook_name = contents.trim();
    if hook_name.is_empty() {
        return Ok(None);
    }
    validate_worker_commit_hook_name(hook_name)?;
    Ok(Some(hook_name.to_string()))
}

fn validate_worker_commit_hook_name(hook_name: &str) -> Result<()> {
    if hook_name.len() != 32
        || !hook_name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RunError::new(format!(
            "invalid worker commit hook name {hook_name:?}"
        )));
    }
    Ok(())
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
    if let Some(artifact) = manifest.musterroll_roster_artifact.as_ref() {
        validate_artifact_ref(artifact, "manifest musterroll roster artifact")?;
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

/// Event-log-only schema gate: unlike [`check_schema`] (still an exact
/// match, e.g. for the manifest), this accepts either the historical `@2`
/// value or the current `@3` value, since `read_events` must keep opening
/// journals written by the prior binary.
fn check_event_schema(value: &serde_json::Value, path: &Path) -> Result<()> {
    let schema = value.get("schema").and_then(serde_json::Value::as_str);
    if schema != Some(EVENT_SCHEMA) && schema != Some(EVENT_SCHEMA_V3) {
        return Err(RunError::new(format!(
            "unknown schema {:?} in {}, expected {EVENT_SCHEMA:?} or {EVENT_SCHEMA_V3:?}",
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
        check_event_schema(&value, path)
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
    let parsed = crate::musterroll::parse_roster_snapshot(&bytes)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DurableReplaceStep {
    FileSynced,
    Renamed,
    ParentSynced,
}

/// Crash-durable whole-file replacement shared by critical state stores.
///
/// A unique sibling is fully written and synced before rename; syncing the
/// parent afterward makes the renamed directory entry durable as well.
pub(crate) fn durable_atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    durable_atomic_replace_with_observer(path, bytes, |_| Ok(()))
}

fn durable_atomic_replace_with_observer<F>(
    path: &Path,
    bytes: &[u8],
    mut observer: F,
) -> io::Result<()>
where
    F: FnMut(DurableReplaceStep) -> io::Result<()>,
{
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let base = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let (temporary, mut file) = (0_u8..100)
        .find_map(|attempt| {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".{base}.{}.{}.{attempt}.tmp",
                std::process::id(),
                sequence
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => Some(Ok((candidate, file))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("failed to allocate a temporary file for {}", path.display()),
            )
        })?;

    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = observer(DurableReplaceStep::FileSynced) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    observer(DurableReplaceStep::Renamed)?;
    sync_parent_directory(parent)?;
    observer(DurableReplaceStep::ParentSynced)
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    durable_atomic_replace(path, bytes).map_err(|error| {
        RunError::new(format!(
            "failed to durably replace {}: {error}",
            path.display()
        ))
    })
}

/// Append-only journal update: preserve the existing bytes, add one line, then
/// durably replace the whole file. Run journals have one owning process.
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

    atomic_replace(path, &contents)
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
            let path = std::env::temp_dir().join(format!("undertake-run-{label}-{nanos}"));
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

    #[test]
    fn durable_atomic_replace_syncs_file_before_rename_and_parent_afterward() {
        let temp = TempDir::new("durable-replace-order");
        let path = temp.path().join("state.json");
        let mut steps = Vec::new();

        durable_atomic_replace_with_observer(&path, b"new", |step| {
            steps.push(step);
            Ok(())
        })
        .expect("durable replacement");

        assert_eq!(
            steps,
            [
                DurableReplaceStep::FileSynced,
                DurableReplaceStep::Renamed,
                DurableReplaceStep::ParentSynced,
            ]
        );
    }

    #[test]
    fn durable_atomic_replace_keeps_old_value_at_after_file_sync_failpoint() {
        let temp = TempDir::new("durable-replace-before-rename");
        let path = temp.path().join("state.json");
        std::fs::write(&path, b"old").expect("seed old value");

        let error = durable_atomic_replace_with_observer(&path, b"new", |step| {
            if step == DurableReplaceStep::FileSynced {
                Err(std::io::Error::other("modeled crash after file sync"))
            } else {
                Ok(())
            }
        })
        .expect_err("failpoint must stop before rename");

        assert!(error.to_string().contains("modeled crash"));
        assert_eq!(std::fs::read(&path).expect("read visible value"), b"old");
        assert_eq!(
            std::fs::read_dir(temp.path())
                .expect("list replacement directory")
                .count(),
            1,
            "failed staging file is removed"
        );
    }

    #[test]
    fn durable_atomic_replace_exposes_only_complete_new_value_at_after_rename_failpoint() {
        let temp = TempDir::new("durable-replace-after-rename");
        let path = temp.path().join("state.json");
        std::fs::write(&path, b"old").expect("seed old value");

        let error = durable_atomic_replace_with_observer(&path, b"complete-new", |step| {
            if step == DurableReplaceStep::Renamed {
                Err(std::io::Error::other("modeled crash after rename"))
            } else {
                Ok(())
            }
        })
        .expect_err("failpoint must stop before parent sync");

        assert!(error.to_string().contains("modeled crash"));
        assert_eq!(
            std::fs::read(&path).expect("read visible value"),
            b"complete-new"
        );
        assert_eq!(
            std::fs::read_dir(temp.path())
                .expect("list replacement directory")
                .count(),
            1,
            "renamed staging file no longer exists"
        );
    }

    fn fixed_now() -> DateTime<Utc> {
        "2026-07-16T12:00:00Z".parse().expect("fixed timestamp")
    }

    fn new_run_request() -> NewRun {
        NewRun {
            target: RunTarget {
                repo: "/repo/undertake".to_string(),
                bead: Some("undertake-run-contract".to_string()),
            },
            approved_profiles: vec!["claude-sonnet-5".to_string(), "gpt-5.6-luna".to_string()],
            musterroll_roster_artifact: Some(ArtifactRef {
                path: "/home/.config/musterroll/roster.toml".to_string(),
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

    /// Real `undertake/event@2` bytes, captured by running the prior
    /// (pre-`@3`) binary's own `RunHandle::create_at` + `append_event_at` +
    /// `finish` -- not synthesized from the code below, since that would
    /// prove nothing about the actual historical wire shape. Includes two
    /// events with real `plan_invocation` evidence, mirroring what plan
    /// wrote before this bead switched it to the generic `invocation`
    /// field.
    const CAPTURED_EVENTS_V2: &str = include_str!("../tests/fixtures/run-events-v2.jsonl");
    /// The matching manifest for [`CAPTURED_EVENTS_V2`], captured from the
    /// same run.
    const CAPTURED_MANIFEST_V2: &str = include_str!("../tests/fixtures/run-manifest-v2.json");
    /// The `approval.json` artifact the first event's `artifact_refs`
    /// pins by path and sha256; `read_events` fails closed if a
    /// referenced local artifact is missing, so the fixture directory
    /// needs it too.
    const CAPTURED_APPROVAL_V2: &str = include_str!("../tests/fixtures/run-approval-v2.json");
    const CAPTURED_V2_RUN_ID: &str = "run-work-20260716T120000.000000000-p15606-000000";

    #[test]
    fn v2_fixture_journal_opens_reads_and_resumes_under_the_new_binary() {
        // read_events alone: every line keeps its historical `@2` schema
        // tag, and the new `invocation` field the `@3` writer added
        // defaults to `None` via `#[serde(default)]` rather than tripping
        // `deny_unknown_fields` on a key that was never present.
        let temp = TempDir::new("v2-fixture-resume");
        let run_dir = runs_dir(temp.path()).join(CAPTURED_V2_RUN_ID);
        std::fs::create_dir_all(&run_dir).expect("mkdir fixture run dir");
        std::fs::write(run_dir.join("manifest.json"), CAPTURED_MANIFEST_V2)
            .expect("write fixture manifest");
        std::fs::write(run_dir.join("events.jsonl"), CAPTURED_EVENTS_V2)
            .expect("write fixture events");
        std::fs::write(run_dir.join("approval.json"), CAPTURED_APPROVAL_V2)
            .expect("write fixture approval");

        let events = read_events(&run_dir.join("events.jsonl")).expect("read v2 fixture events");
        assert_eq!(events.len(), 7);
        assert!(events.iter().all(|event| event.schema == EVENT_SCHEMA));
        assert!(events.iter().all(|event| event.invocation.is_none()));
        let plan_evidence = events
            .iter()
            .filter_map(|event| event.plan_invocation.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(plan_evidence.len(), 2, "the two plan-shaped fixture events");
        assert!(plan_evidence
            .iter()
            .all(|evidence| evidence.stage == PlanStage::Planner && evidence.attempt == 1));

        // Full resume: `RunHandle::open` reads manifest + events together
        // and must accept this run exactly as the prior binary left it --
        // finished, with its pinned outcome.
        let handle = RunHandle::open(temp.path(), CAPTURED_V2_RUN_ID).expect("open v2 fixture run");
        let manifest = read_manifest(&handle.manifest_path()).expect("read reopened manifest");
        assert_eq!(manifest.lifecycle, RunLifecycle::Finished);
        assert_eq!(manifest.outcome.as_deref(), Some("verified"));
    }

    /// Every production `EventKind::AttemptStarted` emitter in the
    /// codebase, found by scanning each file's own *production* source
    /// (everything before its `mod tests {` boundary, so test-only
    /// call sites -- e.g. `plan_job.rs`'s fixtures that invoke
    /// `append_plan_invocation` directly to hand-build a scenario -- don't
    /// inflate the count) for the literal call-site pattern
    /// `EventKind::AttemptStarted,`. The trailing comma is what
    /// distinguishes an emitter's first positional argument from a `==`
    /// comparison in a filter or reader, which is never followed by a
    /// comma at that position. A new emitter anywhere changes one of these
    /// counts and fails this test, forcing a deliberate decision about
    /// whether it attaches `invocation` evidence -- so budget
    /// reconstruction (built on that evidence) cannot silently regress.
    /// This test only proves the *count* of call sites; whether each one
    /// attaches evidence is proven separately by the behavioral tests
    /// cited alongside each entry below.
    ///
    /// The needle is assembled at runtime, split across two literals, so
    /// this test's own source (it scans `run.rs` from disk along with every
    /// other source file) does not match itself.
    ///
    /// Discovery is DYNAMIC: the test walks `src/` at runtime rather than
    /// holding a hardcoded file list. The first revision of this test used a
    /// fixed `include_str!` array, and the very next new emitter
    /// (`probe.rs`, `conductor-bxb`) silently escaped it — the exact failure
    /// mode the test exists to prevent. A file absent from the inventory
    /// below must contain zero production emitters, so a brand-new module
    /// with an emitter fails the test until deliberately triaged here.
    #[test]
    fn every_attempt_started_emitter_is_accounted_for() {
        let needle = format!("{}{}", "EventKind::AttemptStarted", ",");
        let production_source = |source: &str| -> String {
            source
                .split_once("\nmod tests {")
                .map_or(source, |(production, _)| production)
                .to_string()
        };
        // file (relative to src/) -> expected production emitter count.
        // Every entry's invocation-attach decision is proven by the
        // behavioral test cited beside it.
        let inventory: std::collections::BTreeMap<&str, usize> = [
            // review: reviewer + judge, both attach invocation. Verified by
            // `cli::tests::adversarial_successful_dispatch_keeps_all_mutation_sentinels_untouched`.
            ("cli.rs", 2),
            // work (legacy fleet path, untouched until Phase 6). Attaches
            // invocation. Verified by `dispatch_cycle::tests::e2e_sandbox`.
            ("dispatch_cycle.rs", 1),
            // The generic runner's single emitter (`write_attempt_events`),
            // live since `work` migrated (`conductor-vd3y`). Attaches
            // invocation. Verified by `work_policy::tests::
            // work_policy_end_to_end_commits_verifies_and_closes_the_bead`.
            ("runner.rs", 1),
            // plan: three call sites through `append_plan_invocation`, all
            // attach invocation. Verified by
            // `plan_job::tests::assert_successful_plan_ledger_and_events`.
            ("plan_job.rs", 3),
            // run.rs itself: `start_plan_authoring` and
            // `replace_plan_author_before_artifact` are binding-only and
            // must NOT attach invocation. Verified by plan_job's
            // planner_authoring split-check.
            ("run.rs", 2),
            // bootstrap provider probe (`conductor-bxb`): one emitter in its
            // own dedicated run, stage `provider_probe`, attaches invocation
            // so scorecards see probe evidence as probe evidence. Verified
            // by `probe::tests`.
            ("probe.rs", 1),
        ]
        .into_iter()
        .collect();

        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut pending = vec![src_root.clone()];
        let mut seen = std::collections::BTreeMap::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("dirent").path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let relative = path
                    .strip_prefix(&src_root)
                    .expect("under src/")
                    .to_string_lossy()
                    .into_owned();
                let source = std::fs::read_to_string(&path).expect("read source file");
                let total = production_source(&source).matches(&needle).count();
                seen.insert(relative, total);
            }
        }
        for (file, total) in &seen {
            let expected = inventory.get(file.as_str()).copied().unwrap_or(0);
            assert_eq!(
                *total, expected,
                "{file}: production AttemptStarted emitter count changed -- update this \
                 inventory and confirm the new site's invocation-attach decision"
            );
        }
        for file in inventory.keys() {
            assert!(
                seen.contains_key(*file),
                "{file}: listed in the emitter inventory but not found under src/ -- \
                 remove the stale entry"
            );
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
        assert_eq!(manifest.target.repo, "/repo/undertake");
        assert_eq!(
            manifest.target.bead.as_deref(),
            Some("undertake-run-contract")
        );
        assert_eq!(
            manifest.approved_profiles.profiles,
            vec!["claude-sonnet-5".to_string(), "gpt-5.6-luna".to_string()]
        );
        assert_eq!(
            manifest
                .musterroll_roster_artifact
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
        assert!(events.iter().all(|e| e.schema == EVENT_SCHEMA_V3));
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
            schema: "undertake/run@2".to_string(),
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
            musterroll_roster_artifact: None,
            roster_snapshot: None,
            roster_policy_sha256: None,
            limits: RunLimits::default(),
            verifier: RunVerifier::default(),
            artifacts: Vec::new(),
            lifecycle: RunLifecycle::Started,
            outcome: None,
        })
        .unwrap();
        manifest["schema"] = serde_json::json!("conductor/run@2");
        std::fs::write(&path, manifest.to_string()).unwrap();

        let err = read_manifest(&path).expect_err("unknown schema must fail closed");
        assert!(err.to_string().contains("unknown schema"));
    }

    #[test]
    fn run_event_rejects_unknown_event_schema() {
        let temp = TempDir::new("bad-event-schema");
        let path = temp.path().join("events.jsonl");
        let bad_line = serde_json::json!({
            "schema": "conductor/event@2",
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
        raw.push_str("{\"schema\":\"undertake/event@1\",\"event_id\":\"trunc");
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
        std::fs::write(reopened.manifest_path(), br#"{"schema":"undertake/run@9"}"#).unwrap();
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
            owner_pid_generation: None,
            worker_pgid: None,
            worker_pgid_generation: None,
            worker_slots: Vec::new(),
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
            owner_pid_generation: None,
            worker_pgid: None,
            worker_pgid_generation: None,
            worker_slots: Vec::new(),
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
            reason: "undertake cycle-1: verified via cargo test".to_string(),
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
        assert!(reopened
            .write_terminal_transition(&TerminalTransition {
                action: TerminalTransitionAction::Close,
                reason: "different reason".to_string(),
                metadata: None,
                comment: None,
            })
            .is_err());
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
        assert!(handle
            .manifest_path()
            .starts_with(runs_dir(temp.path()).join(handle.run_id())));
        assert!(handle.run_id().starts_with("run-consult-"));
        assert!(handle.dir().join("attempts").is_dir());
        assert!(handle.dir().join("artifacts").is_dir());
    }

    #[test]
    fn run_event_missing_musterroll_roster_emits_explicit_coverage_gap() {
        let temp = TempDir::new("musterroll-gap");
        let mut request = new_run_request();
        request.musterroll_roster_artifact = None;
        let handle = RunHandle::create_at(temp.path(), RunJob::Work, request, fixed_now())
            .expect("create run");

        let events = read_events(&handle.events_path()).expect("read events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, EventKind::RunStarted);
        assert_eq!(events[1].kind, EventKind::CoverageGap);
        assert_eq!(
            events[1].outcome.as_deref(),
            Some("musterroll_roster_artifact_unavailable")
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
        assert!(handle
            .capture_artifact(&source, Path::new("approval.json"))
            .is_err());
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
            owner_pid_generation: None,
            worker_pgid,
            worker_pgid_generation: None,
            worker_slots: Vec::new(),
            worker_profile: None,
            worker_commit: None,
            mechanical: None,
            stage: WorkStage::Implementing,
            review_resume_budget_secs: None,
        });
        request
    }

    #[test]
    fn prepare_worker_commit_hook_supersedes_only_this_runs_prior_hook() {
        let temp = TempDir::new("worker-hook-supersede");
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            implementing_request_with_worker_pgid(Some(111)),
            fixed_now(),
        )
        .expect("create implementing run");
        let first = "1".repeat(32);
        let second = "2".repeat(32);

        assert_eq!(
            handle
                .prepare_worker_commit_hook(&first)
                .expect("prepare first hook"),
            None
        );
        assert!(
            worker_commit_hook_is_current(temp.path(), handle.run_id(), &first)
                .expect("check first hook")
        );

        assert_eq!(
            handle
                .prepare_worker_commit_hook(&second)
                .expect("prepare fallback hook"),
            Some(first.clone())
        );
        assert!(
            !worker_commit_hook_is_current(temp.path(), handle.run_id(), &first)
                .expect("old hook is superseded")
        );
        assert!(
            worker_commit_hook_is_current(temp.path(), handle.run_id(), &second)
                .expect("new hook is current")
        );
    }

    #[test]
    fn clear_worker_commit_hook_releases_only_the_quiescent_current_hook() {
        let temp = TempDir::new("worker-hook-clear");
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            implementing_request_with_worker_pgid(None),
            fixed_now(),
        )
        .expect("create implementing run");
        let hook = "c".repeat(32);
        handle
            .prepare_worker_commit_hook(&hook)
            .expect("prepare hook");

        handle
            .clear_worker_commit_hook(&hook)
            .expect("clear quiescent hook");

        assert!(
            !worker_commit_hook_is_current(temp.path(), handle.run_id(), &hook)
                .expect("cleared hook is not current")
        );
    }

    #[test]
    fn worker_commit_hook_is_not_current_after_run_finishes() {
        let temp = TempDir::new("worker-hook-finished");
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            implementing_request_with_worker_pgid(None),
            fixed_now(),
        )
        .expect("create implementing run");
        let hook = "a".repeat(32);
        handle
            .prepare_worker_commit_hook(&hook)
            .expect("prepare hook");
        handle.finish("failed").expect("finish run");

        assert!(
            !worker_commit_hook_is_current(temp.path(), handle.run_id(), &hook)
                .expect("finished run cannot reference a hook")
        );
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

    fn work_state_with_identity(
        worker_pgid: Option<u32>,
        worker_pgid_generation: Option<u64>,
        worker_slots: Vec<WorkerSlotIdentity>,
    ) -> WorkState {
        WorkState {
            cycle_id: "cycle-1".to_string(),
            authorization_sha256: "b".repeat(64),
            before_head: None,
            owner_pid: None,
            owner_pid_generation: None,
            worker_pgid,
            worker_pgid_generation,
            worker_slots,
            worker_profile: None,
            worker_commit: None,
            mechanical: None,
            stage: WorkStage::Implementing,
            review_resume_budget_secs: None,
        }
    }

    #[test]
    fn effective_worker_slots_is_empty_when_no_identity_has_ever_been_recorded() {
        let work = work_state_with_identity(None, None, Vec::new());
        assert_eq!(
            work.effective_worker_slots(),
            Vec::new(),
            "a run with no recorded worker identity has nothing to reclaim against"
        );
    }

    #[test]
    fn effective_worker_slots_falls_back_to_the_legacy_single_pgid_as_slot_zero() {
        // Every manifest written before per-slot identities existed -- and
        // every single-slot `work`/`plan`/`consult` run going forward --
        // records only `worker_pgid`/`worker_pgid_generation`. That legacy
        // pair must keep behaving exactly as it does today: reinterpreted as
        // one slot 0 entry, nothing invented.
        let work = work_state_with_identity(Some(111), Some(42), Vec::new());
        assert_eq!(
            work.effective_worker_slots(),
            vec![WorkerSlotIdentity {
                slot: 0,
                pgid: 111,
                generation: Some(42),
            }]
        );
    }

    #[test]
    fn effective_worker_slots_prefers_the_recorded_set_over_the_legacy_pair() {
        // A multi-slot record is authoritative once it has any entries; the
        // legacy pair is never consulted alongside it.
        let recorded = vec![
            WorkerSlotIdentity {
                slot: 0,
                pgid: 111,
                generation: Some(1),
            },
            WorkerSlotIdentity {
                slot: 1,
                pgid: 222,
                generation: Some(2),
            },
        ];
        let work = work_state_with_identity(Some(999), Some(999), recorded.clone());
        assert_eq!(work.effective_worker_slots(), recorded);
    }

    #[test]
    fn run_event_cross_process_same_second_creation_is_exclusive() {
        const STATE_ENV: &str = "UNDERTAKE_RUN_TEST_CHILD_STATE";
        const RESULT_ENV: &str = "UNDERTAKE_RUN_TEST_CHILD_RESULT";
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
                owner_pid_generation: None,
                worker_pgid: None,
                worker_pgid_generation: None,
                worker_slots: Vec::new(),
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
            find_implementing_work_run(temp.path(), cycle_id, "/repo/undertake", "impl-bead")
                .expect("lookup implementing"),
            Some(implementing.run_id().to_string())
        );
        assert_eq!(
            find_implementing_work_run(temp.path(), cycle_id, "/repo/undertake", "finished-bead")
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
                owner_pid_generation: None,
                worker_pgid: Some(456),
                worker_pgid_generation: None,
                worker_slots: Vec::new(),
                worker_profile: None,
                worker_commit: None,
                mechanical: None,
                stage: WorkStage::Implementing,
                review_resume_budget_secs: None,
            });
            request
        };
        let repo = "/repo/undertake";

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
                owner_pid_generation: None,
                worker_pgid: None,
                worker_pgid_generation: None,
                worker_slots: Vec::new(),
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
            find_reclaimable_work_run(temp.path(), cycle_id, "/repo/undertake", "conflict-bead")
                .is_err(),
            "two unfinished generations is an invariant violation and must fail closed"
        );
    }

    #[test]
    fn v2_manifests_reject_v1_schema_and_unknown_fields() {
        let temp = TempDir::new("strict-v2-schema");
        let path = temp.path().join("manifest.json");
        let mut value = serde_json::to_value(RunManifest {
            schema: "undertake/run@1".to_string(),
            run_id: "run-work-20260716T120000.000000000-p1-000000".to_string(),
            job: RunJob::Work,
            target: RunTarget {
                repo: "/repo/undertake".to_string(),
                bead: Some("undertake-run-v2".to_string()),
            },
            details: RunDetails::Work { state: None },
            created_at: "2026-07-16T12:00:00Z".to_string(),
            updated_at: "2026-07-16T12:00:00Z".to_string(),
            approved_profiles: ApprovedProfileEnvelope::default(),
            musterroll_roster_artifact: None,
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

        value["schema"] = serde_json::json!("undertake/run@2");
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
          "schema":"musterroll/roster@2",
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
            .bind_peer(peer.clone(), "peer-bind".to_string())
            .expect("persist peer before its first invocation");
        assert!(
            progress
                .bind_peer(different_peer.clone(), "peer-bind".to_string())
                .is_err(),
            "a persisted peer binding cannot change after a crash boundary"
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
        let second = execution("second", "opencode-go");
        progress
            .bind_second_opinion(second.clone(), "second-bind".to_string())
            .expect("persist pairwise-distinct second opinion");
        progress
            .record_second_opinion(&second, SecondOpinionVerdict::Accept)
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
                    "schema": "undertake/run@1",
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
        assert!(legacy_v1_preflight(temp.path())
            .expect("classify inert history")
            .activation_allowed());
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

    // -- Terminal-window crash coverage (prep 2: job-generic reconciliation) --
    //
    // Every terminal write now goes through `finish_terminal`
    // (`finish`/`finish_with_artifacts`/`finish_with_verdict`), which
    // appends the durable `run_finished` journal event before the atomic
    // manifest write that follows it in the same call. The tests below
    // simulate a crash in exactly that window -- the journal fully durable,
    // the matching manifest write never having landed -- for every
    // `RunDetails` variant, and assert `RunHandle::open` reconciles each to
    // one defined, `Finished` state rather than passing the skew through as
    // resumable.

    /// Rolls `manifest.json` back to its state immediately before `terminal`
    /// runs, simulating a crash in which every event `terminal` appends
    /// becomes durable in the journal but none of the matching manifest
    /// writes land -- the window `reconcile_terminal_manifest` exists to
    /// repair. Mirrors
    /// `run_event_open_recovers_authenticated_terminal_event_before_manifest_rewrite`'s
    /// pattern, generalized to every job.
    fn simulate_crash_after_terminal_event<F>(handle: &mut RunHandle, terminal: F)
    where
        F: FnOnce(&mut RunHandle) -> Result<()>,
    {
        let manifest_path = handle.manifest_path();
        let pre_terminal_bytes =
            std::fs::read(&manifest_path).expect("read pre-terminal manifest snapshot");
        terminal(handle).expect("perform terminal transition");
        std::fs::write(&manifest_path, &pre_terminal_bytes)
            .expect("roll manifest back to its pre-terminal snapshot");
    }

    fn plan_execution(profile_id: &str, provider_id: &str) -> ApprovedExecution {
        ApprovedExecution {
            profile_id: profile_id.to_string(),
            provider_id: provider_id.to_string(),
            availability_key: provider_id.to_string(),
            execution_key: format!("{provider_id}/{profile_id}"),
        }
    }

    fn plan_stage_route(stage: PlanStage, candidate: ApprovedExecution) -> PlanStageRoute {
        PlanStageRoute {
            stage,
            capability_role: "senior".to_string(),
            candidates: vec![candidate],
            provider_distinct_from: Vec::new(),
            constraints: PlanStageConstraints::unconstrained(),
        }
    }

    fn new_plan_run_request(run_id: &str) -> NewPlanRun {
        let input_bytes = br#"{"summary":"terminal-window crash coverage fixture"}"#.to_vec();
        let input_artifact = ArtifactRef {
            path: "target-input.json".to_string(),
            sha256: format!("{:x}", Sha256::digest(&input_bytes)),
        };
        let target = PlanTarget {
            repo: "/repo/undertake".to_string(),
            input: PlanInput::Artifact {
                artifact: input_artifact,
                tier: PlanTier::Senior,
                complexity: PlanComplexity::M,
            },
        };
        let routes = PlanRoutes {
            stages: vec![
                plan_stage_route(PlanStage::Planner, plan_execution("author", "anthropic")),
                plan_stage_route(PlanStage::PeerReview, plan_execution("peer", "openai")),
                plan_stage_route(
                    PlanStage::SecondOpinion,
                    plan_execution("second", "opencode-go"),
                ),
            ],
        };
        let details = PlanRunDetails {
            target,
            routes,
            progress: PlanProgress::Prepared,
            stage_attempts: PlanStageAttempts::default(),
            revision_limit: RevisionLimit::new(1).expect("revision limit"),
            stage_attempt_limit: StageAttemptLimit::new(2).expect("stage attempt limit"),
        };
        NewPlanRun {
            run_id: run_id.to_string(),
            target: RunTarget {
                repo: "/repo/undertake".to_string(),
                bead: Some("undertake-run-contract".to_string()),
            },
            details,
            approved_profiles: vec!["author".to_string()],
            musterroll_roster_artifact: None,
            roster_snapshot: RosterSnapshotInput {
                bytes: br#"{
                  "schema":"musterroll/roster@2",
                  "generated_at":"2026-07-16T12:00:00Z",
                  "source_artifact":{"path":"/source/roster.toml","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                  "policy_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                  "providers":[],
                  "profiles":[]
                }"#
                .to_vec(),
                policy_sha256: "b".repeat(64),
            },
            limits: RunLimits {
                item_wall_clock_mins: Some(30),
                max_attempts: Some(3),
            },
            verifier: RunVerifier::default(),
            approval: serde_json::json!({
                "schema": "test/approval@1",
                "decision": "approved"
            }),
            input_bytes,
        }
    }

    #[test]
    fn terminal_window_crash_reconciles_work_run() {
        let temp = TempDir::new("terminal-window-work");
        let mut request = new_run_request();
        request.work = Some(WorkState {
            cycle_id: "cycle-1".to_string(),
            authorization_sha256: "b".repeat(64),
            before_head: None,
            owner_pid: None,
            owner_pid_generation: None,
            worker_pgid: None,
            worker_pgid_generation: None,
            worker_slots: Vec::new(),
            worker_profile: None,
            worker_commit: None,
            mechanical: None,
            stage: WorkStage::Implementing,
            review_resume_budget_secs: None,
        });
        let mut handle = RunHandle::create_at(temp.path(), RunJob::Work, request, fixed_now())
            .expect("create work run");
        simulate_crash_after_terminal_event(&mut handle, |handle| handle.finish("verified"));
        let run_id = handle.run_id().to_string();
        drop(handle);

        let reopened = RunHandle::open(temp.path(), &run_id).expect("reconcile work run");
        assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(reopened.manifest().outcome.as_deref(), Some("verified"));
        assert_eq!(
            reopened.work().expect("work state").stage,
            WorkStage::Completed,
            "work-only reconciliation must still complete the work stage"
        );
    }

    #[test]
    fn terminal_window_crash_reconciles_review_and_consult_runs() {
        for job in [RunJob::Review, RunJob::Consult] {
            let label = format!("terminal-window-{job:?}");
            let temp = TempDir::new(&label);
            let mut handle = RunHandle::create_at(temp.path(), job, new_run_request(), fixed_now())
                .expect("create run");
            simulate_crash_after_terminal_event(&mut handle, |handle| handle.finish("completed"));
            let run_id = handle.run_id().to_string();
            drop(handle);

            let reopened = RunHandle::open(temp.path(), &run_id)
                .unwrap_or_else(|error| panic!("reconcile {job:?} run: {error}"));
            assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
            assert_eq!(reopened.manifest().outcome.as_deref(), Some("completed"));
        }
    }

    #[test]
    fn terminal_window_crash_reconciles_plan_blocked_via_finish_plan_blocked() {
        let temp = TempDir::new("terminal-window-plan-blocked");
        let mut handle =
            RunHandle::create_plan(temp.path(), new_plan_run_request("run-plan-blocked"))
                .expect("create plan run");
        handle
            .start_plan_authoring(plan_execution("author", "anthropic"))
            .expect("start authoring");

        simulate_crash_after_terminal_event(&mut handle, RunHandle::finish_plan_blocked);
        let run_id = handle.run_id().to_string();
        drop(handle);

        let reopened = RunHandle::open(temp.path(), &run_id).expect("reconcile blocked plan");
        assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(reopened.manifest().outcome.as_deref(), Some("blocked"));
        assert!(matches!(
            reopened.plan().expect("plan").progress,
            PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::Blocked
            }
        ));
    }

    #[test]
    fn terminal_window_crash_reconciles_plan_blocked_via_cancel_prepared_plan() {
        let temp = TempDir::new("terminal-window-plan-cancel-prepared");
        let mut handle = RunHandle::create_plan(
            temp.path(),
            new_plan_run_request("run-plan-cancel-prepared"),
        )
        .expect("create plan run");
        // Progress is `Prepared` immediately after creation; no setup needed.

        simulate_crash_after_terminal_event(&mut handle, RunHandle::cancel_prepared_plan);
        let run_id = handle.run_id().to_string();
        drop(handle);

        let reopened =
            RunHandle::open(temp.path(), &run_id).expect("reconcile canceled-prepared plan");
        assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(reopened.manifest().outcome.as_deref(), Some("canceled"));
        assert!(
            matches!(
                reopened.plan().expect("plan").progress,
                PlanProgress::Terminal {
                    verdict: PlanTerminalVerdict::Blocked
                }
            ),
            "cancel_prepared_plan's \"canceled\" outcome must reconcile to the same \
             PlanTerminalVerdict::Blocked as finish_plan_blocked's \"blocked\" outcome -- \
             the outcome strings differ, the durable verdict must not"
        );
    }

    #[test]
    fn terminal_window_crash_reconciles_plan_blocked_via_cancel_failed_authoring_plan() {
        let temp = TempDir::new("terminal-window-plan-cancel-authoring");
        let mut handle = RunHandle::create_plan(
            temp.path(),
            new_plan_run_request("run-plan-cancel-authoring"),
        )
        .expect("create plan run");
        handle
            .start_plan_authoring(plan_execution("author", "anthropic"))
            .expect("start authoring");

        simulate_crash_after_terminal_event(&mut handle, RunHandle::cancel_failed_authoring_plan);
        let run_id = handle.run_id().to_string();
        drop(handle);

        let reopened =
            RunHandle::open(temp.path(), &run_id).expect("reconcile canceled-authoring plan");
        assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(reopened.manifest().outcome.as_deref(), Some("canceled"));
        assert!(matches!(
            reopened.plan().expect("plan").progress,
            PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::Blocked
            }
        ));
    }

    #[test]
    fn terminal_window_crash_reconciles_plan_needs_input() {
        let temp = TempDir::new("terminal-window-plan-needs-input");
        let mut handle =
            RunHandle::create_plan(temp.path(), new_plan_run_request("run-plan-needs-input"))
                .expect("create plan run");
        handle
            .start_plan_authoring(plan_execution("author", "anthropic"))
            .expect("start authoring");
        let artifact = handle
            .capture_plan_artifact(Path::new("needs-input.json"), b"{\"open_questions\":true}")
            .expect("capture needs-input artifact");

        simulate_crash_after_terminal_event(&mut handle, move |handle| {
            handle.finish_plan_needs_input(artifact)
        });
        let run_id = handle.run_id().to_string();
        drop(handle);

        let reopened = RunHandle::open(temp.path(), &run_id).expect("reconcile needs-input plan");
        assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(reopened.manifest().outcome.as_deref(), Some("needs_input"));
        assert!(matches!(
            reopened.plan().expect("plan").progress,
            PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::NeedsInput
            }
        ));
    }

    #[test]
    fn terminal_window_crash_reconciles_plan_accepted_via_record_plan_peer_verdict() {
        let temp = TempDir::new("terminal-window-plan-peer-accept");
        let mut handle =
            RunHandle::create_plan(temp.path(), new_plan_run_request("run-plan-peer-accept"))
                .expect("create plan run");
        handle
            .start_plan_authoring(plan_execution("author", "anthropic"))
            .expect("start authoring");
        let draft = handle
            .capture_plan_artifact(Path::new("draft.json"), b"{\"draft\":true}")
            .expect("capture draft artifact");
        handle.await_plan_peer(draft).expect("await peer review");
        let peer = plan_execution("peer", "openai");

        simulate_crash_after_terminal_event(&mut handle, move |handle| {
            handle.record_plan_peer_verdict(peer, PeerVerdict::Approve, false)
        });
        let run_id = handle.run_id().to_string();
        drop(handle);

        let reopened = RunHandle::open(temp.path(), &run_id).expect("reconcile accepted plan");
        assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(reopened.manifest().outcome.as_deref(), Some("accepted"));
        assert!(matches!(
            reopened.plan().expect("plan").progress,
            PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::Accepted
            }
        ));
    }

    #[test]
    fn terminal_window_crash_reconciles_plan_rejected_via_record_plan_second_opinion() {
        let temp = TempDir::new("terminal-window-plan-second-opinion");
        let mut handle = RunHandle::create_plan(
            temp.path(),
            new_plan_run_request("run-plan-second-opinion"),
        )
        .expect("create plan run");
        handle
            .start_plan_authoring(plan_execution("author", "anthropic"))
            .expect("start authoring");
        let draft = handle
            .capture_plan_artifact(Path::new("draft.json"), b"{\"draft\":true}")
            .expect("capture draft artifact");
        handle.await_plan_peer(draft).expect("await peer review");
        let peer = plan_execution("peer", "openai");
        handle
            .record_plan_peer_verdict(peer, PeerVerdict::Approve, true)
            .expect("peer approves; second opinion required");
        let second = plan_execution("second", "opencode-go");
        handle
            .bind_plan_second_opinion(second.clone(), "second-bind".to_string())
            .expect("persist second-opinion binding before its first invocation");

        simulate_crash_after_terminal_event(&mut handle, move |handle| {
            handle.record_plan_second_opinion(&second, SecondOpinionVerdict::Reject)
        });
        let run_id = handle.run_id().to_string();
        drop(handle);

        let reopened = RunHandle::open(temp.path(), &run_id).expect("reconcile rejected plan");
        assert_eq!(reopened.manifest().lifecycle, RunLifecycle::Finished);
        assert_eq!(reopened.manifest().outcome.as_deref(), Some("rejected"));
        assert!(matches!(
            reopened.plan().expect("plan").progress,
            PlanProgress::Terminal {
                verdict: PlanTerminalVerdict::Rejected
            }
        ));
    }
}
