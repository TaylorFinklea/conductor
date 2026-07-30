//! The generic attempt-runner contract's vocabulary and ports.
//!
//! This module implements pass (a) of bead `conductor-mkct`: the types and
//! traits from `.docs/ai/phases/undertake-runner-contract.md`, and nothing
//! else. It defines **no attempt loop** — `AttemptRunner::run` from the
//! contract's `## Shape` section does not exist here yet, no job is migrated
//! onto these types, and no other module references this one. That is pass
//! (b)'s job (the extraction described in the contract's `## Phase 1b-prep`
//! and beyond).
//!
//! Every type below either reuses an existing type verbatim, generalizes an
//! existing job-specific type, or is new because the contract names a seam
//! with no prior implementation. Each is called out at its definition.

#![allow(
    dead_code,
    reason = "pass (a) defines the runner contract's types and ports only; \
              pass (b) wires them into an attempt loop and migrates jobs \
              onto it, per .docs/ai/phases/undertake-runner-contract.md"
)]

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::bd::{self, BdClient, Comment, Issue};
use crate::config::Backend;
use crate::dispatch::{self, CommitAuthenticationRejection, DispatchFailure, DispatchResult};
use crate::job::MutationPosture;
use crate::musterroll::RuntimeLimitEvidence;
use crate::quarantine;
use crate::run::{
    self, ApprovedExecution, ArtifactRef, PlanProviderDiversity, StageAttemptLimit, TerminalVerdict,
};
use sha2::{Digest, Sha256};

pub(crate) type Result<T> = std::result::Result<T, RunnerError>;

/// Error returned by the runner's own fallible constructors (`StageId`
/// validation, `CallBudget` exhaustion, `WorktreePort` failures). Mirrors
/// the `RunError`/`JobError`/`BdError` shape already used by every other
/// module's own error type (`run.rs:58-81`, `job.rs:19-38`, `bd.rs:32-`).
#[derive(Debug, Clone)]
pub(crate) struct RunnerError {
    message: String,
}

impl RunnerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RunnerError {}

/// A validated `snake_case` stage identifier. Non-empty, lowercase ASCII
/// letters/digits/underscores only, no leading/trailing underscore, no
/// double underscore. `run::PlanStage` (`run.rs:369-373`) is a *closed*
/// three-variant enum specific to `plan`; the generic runner instead needs
/// an open identifier space so `work`, `review`, and `consult` can each name
/// their own stages, hence a validated newtype rather than reusing or
/// widening `PlanStage`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct StageId(String);

impl StageId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if !is_snake_case(&value) {
            return Err(RunnerError::new(format!(
                "invalid stage id {value:?}: must be non-empty snake_case \
                 (lowercase ascii letters/digits/underscores, no leading, \
                 trailing, or doubled underscore)"
            )));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_snake_case(value: &str) -> bool {
    let Some(first) = value.chars().next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    if value.ends_with('_') {
        return false;
    }
    let mut prev_underscore = false;
    for c in value.chars() {
        if c == '_' {
            if prev_underscore {
                return false;
            }
            prev_underscore = true;
        } else if c.is_ascii_lowercase() || c.is_ascii_digit() {
            prev_underscore = false;
        } else {
            return false;
        }
    }
    true
}

/// What conditions repo lease acquisition, the `is_clean` preflight, and the
/// post-attempt git postcheck for one stage. See the contract's `## Target
/// kinds` table: `work` is `GitWorkingTree`, `plan` is `GitWorktreeIsolated`,
/// `review`/`consult` are `ArtifactOnly` (`cli.rs` puts an artifact path into
/// `RunTarget.repo` for those two — verified, not every `RunTarget.repo` is
/// a git working tree).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetKind {
    GitWorkingTree,
    GitWorktreeIsolated,
    ArtifactOnly,
}

/// One slot's own ordered candidate chain within a stage. `review`'s
/// concurrent reviewer panel is `N` slots, each walking its own approved
/// fallback chain; every other job today declares exactly one slot. This is
/// the two-level shape the contract's `## Shape` section calls out as the
/// reason a single-level candidate walk cannot host `review`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) index: u32,
    pub(crate) candidates: Vec<ApprovedExecution>,
}

/// Immutable relationship constraints for a stage, generalized off
/// `run::PlanStageConstraints` (`run.rs:391-405`) by replacing `PlanStage`
/// references with the generic `StageId`. Field shape and semantics —
/// including `provider_diversity`'s two non-`None` policies — are otherwise
/// unchanged; renaming that field to a model-family-based rule is
/// `conductor-ao8`'s job (`decisions.md [2026-07-28]`), not this one's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageConstraints {
    pub(crate) distinct_execution_from: Vec<StageId>,
    pub(crate) tier_at_least: Vec<StageId>,
    pub(crate) provider_diversity: PlanProviderDiversity,
}

impl StageConstraints {
    pub(crate) fn unconstrained() -> Self {
        Self {
            distinct_execution_from: Vec::new(),
            tier_at_least: Vec::new(),
            provider_diversity: PlanProviderDiversity::None,
        }
    }
}

/// Coarse discriminant of [`AttemptOutcome`], used only as a mapping key: a
/// stage declares an [`AttemptAction`] per category, never per exact
/// payload (a `DispatchFailure::ExitNonZero { code }`'s exact code does not
/// change which action applies). See [`Stage::action_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AttemptOutcomeCategory {
    ProcessFailure,
    CommitAuthenticationFailure,
    RuntimeLimit,
    SchemaInvalid,
    Repaired,
    EligibilityLost,
    BudgetExhausted,
    ApprovalDrift,
}

/// The classification union one attempt reduces to before `AttemptAction`
/// mapping. Reuses the existing execution-failure taxonomies rather than
/// restating them, per the contract's `## AttemptOutcome` section:
///
/// - `Dispatch` / `CommitAuthentication` fold in `dispatch::DispatchFailure`
///   and `dispatch::CommitAuthenticationRejection` (`dispatch.rs:202-221`)
///   unchanged.
/// - `RuntimeLimit` carries `musterroll::RuntimeLimitEvidence`, the same
///   evidence type `RunEvent::provider_limit` already stores
///   (`run.rs:1039-1040`). The classification logic that *produces* one
///   (`classify_retryable_failure` / `contains_contextual_429` /
///   `classify_canonical_harness_session_limit` / `extract_provider_reset`,
///   `dispatch_cycle.rs:7719,7893,7744,7763`) is not moved by this pass —
///   only its result's home is defined here.
/// - `SchemaInvalid`, `SchemaRepaired`, `EligibilityLost`, `BudgetExhausted`,
///   and `ApprovalDrift` are policy-contributed readings of an otherwise
///   successful process, covering the contract's stated "union of abandon
///   reasons": process/spawn failure (`Dispatch`/`CommitAuthentication`),
///   schema or parse failure (`SchemaInvalid`), eligibility loss mid-run
///   (`EligibilityLost`), budget/revision/attempt-cap exhaustion
///   (`BudgetExhausted`), and external-state drift since approval
///   (`ApprovalDrift`).
///
/// Deliberately **absent**: `VerdictRejected`. An earlier contract draft
/// listed it here and was corrected — a domain verdict (`ReviewerVerdict::
/// NoGo`, a rejected plan document) is not an execution failure; `review`
/// stays `Complete` when synthesis itself succeeds (`adversarial.rs:
/// 216-263`). Verdicts live in `AttemptOutput`'s payload and reach
/// `Terminal` through `JobPolicy::transition`, never through this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    Dispatch(DispatchFailure),
    CommitAuthentication(CommitAuthenticationRejection),
    RuntimeLimit(RuntimeLimitEvidence),
    SchemaInvalid { detail: String },
    SchemaRepaired,
    EligibilityLost { detail: String },
    BudgetExhausted { detail: String },
    ApprovalDrift { digest: DigestKind, detail: String },
}

impl AttemptOutcome {
    pub(crate) const fn category(&self) -> AttemptOutcomeCategory {
        match self {
            Self::Dispatch(_) => AttemptOutcomeCategory::ProcessFailure,
            Self::CommitAuthentication(_) => AttemptOutcomeCategory::CommitAuthenticationFailure,
            Self::RuntimeLimit(_) => AttemptOutcomeCategory::RuntimeLimit,
            Self::SchemaInvalid { .. } => AttemptOutcomeCategory::SchemaInvalid,
            Self::SchemaRepaired => AttemptOutcomeCategory::Repaired,
            Self::EligibilityLost { .. } => AttemptOutcomeCategory::EligibilityLost,
            Self::BudgetExhausted { .. } => AttemptOutcomeCategory::BudgetExhausted,
            Self::ApprovalDrift { .. } => AttemptOutcomeCategory::ApprovalDrift,
        }
    }
}

/// The four actions the runner supplies. Which `AttemptOutcomeCategory` maps
/// to which is declared per stage (see [`Stage::action_for`]) — never
/// globally. Quota/rate-limit/session-limit outcomes advance the candidate
/// in some stages and must not in others (`review` must stay inside its
/// slot's approved provider envelope; `plan` blocks rather than falling back
/// when a bound peer loses eligibility, `plan_job.rs:1915-1922`); a global
/// `AdvanceCandidate ≙ process/eligibility failure` rule was an earlier
/// draft's verified error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptAction {
    Accept,
    /// Schema repair: same profile, prompt sees the failed attempt's output.
    /// The one mapping every stage shares (contract: "the one mapping both
    /// share").
    RetrySameCandidate,
    /// Next entry in this slot's own candidate chain.
    AdvanceCandidate,
    Fatal,
}

/// A snake-case stage id plus its own pinned candidate pool, attempt budget,
/// concurrency, isolation, and outcome-to-action mapping. `plan`'s `PlanStage`
/// (`run.rs:369-373`) already models exactly three closed stages; this
/// generalizes the same responsibilities onto an open `StageId` space so
/// `work`, `review`, and `consult` can each declare their own.
///
/// `attempt_budget` reuses `run::StageAttemptLimit` (`run.rs:505-520`,
/// "a stage can never be attempted zero times") rather than a bare integer —
/// the contract does not name this field's type explicitly, but that
/// existing newtype is exactly the invariant a per-candidate attempt cap
/// needs, and reusing it keeps one nonzero-attempt-cap vocabulary instead of
/// two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stage {
    pub(crate) id: StageId,
    pub(crate) slots: Vec<Slot>,
    pub(crate) concurrency: NonZeroUsize,
    pub(crate) target_kind: TargetKind,
    pub(crate) constraints: StageConstraints,
    pub(crate) attempt_budget: StageAttemptLimit,
    /// The stage-declared `AttemptOutcomeCategory -> AttemptAction` mapping.
    /// A category absent from this map fails closed to `AttemptAction::
    /// Fatal` (see [`Stage::action_for`]) rather than falling back to any
    /// shared default — a missing entry must never silently reintroduce the
    /// global mapping the contract rejects.
    pub(crate) outcome_actions: BTreeMap<AttemptOutcomeCategory, AttemptAction>,
}

impl Stage {
    /// The `AttemptAction` this stage declares for `outcome`. Fails closed
    /// to `Fatal` when the stage did not declare a mapping for `outcome`'s
    /// category.
    pub(crate) fn action_for(&self, outcome: &AttemptOutcome) -> AttemptAction {
        self.outcome_actions
            .get(&outcome.category())
            .copied()
            .unwrap_or(AttemptAction::Fatal)
    }
}

/// One successful attempt's payload: canonical bytes plus their pinned
/// identity. An `AttemptOutcome` alone cannot express what a successful
/// attempt *produced* — a plan document, a typed reviewer response — so the
/// output channel carries both; the runner hashes and captures before a
/// policy ever sees them (contract: "`AttemptOutput` must carry a payload").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) artifact: ArtifactRef,
}

/// One stage's aggregated result, as `JobPolicy::aggregate_stage` reduces it
/// from that stage's `SlotResult`s. Deliberately opaque to the runner beyond
/// its retained outputs: `review`'s panel-completeness read and `plan`'s
/// peer-review verdict are both job-specific interpretations of the same
/// slot results, and `StageLedger` only needs to carry the artifacts
/// forward, never understand them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageOutcome {
    pub(crate) stage: StageId,
    pub(crate) outputs: Vec<AttemptOutput>,
}

/// One slot's terminal result after walking its ordered candidate chain to
/// `Accept` or exhaustion/`Fatal`. Not named explicitly in the contract —
/// it exists because `JobPolicy::aggregate_stage(stage, slot_results)`
/// needs a concrete element type for `slot_results`, and `review`'s
/// panel-completeness/minority-preservation reads (`adversarial.rs:
/// 1372-1420`) are exactly a reduction over one of these per reviewer slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlotOutcome {
    Accepted(AttemptOutput),
    /// The slot's chain was exhausted, or ended `Fatal`, without producing
    /// an accepted output.
    Unaccepted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotResult {
    pub(crate) slot: u32,
    pub(crate) outcome: SlotOutcome,
}

/// Completed stages, their `StageOutcome`s, and their hash-pinned artifacts —
/// what `JobPolicy::next_stage` and `JobPolicy::terminal` read. Plan's
/// `PlanProgress` (`run.rs:487-524`) becomes a projection a plan policy
/// computes over this generic, append-only ledger rather than a parallel
/// state machine (contract: "`next_stage(ledger)` replaces `stages
/// (progress)`").
///
/// `outcome`/`artifacts_for` return the **most recently recorded** entry for
/// a `StageId` — a revision loop (`plan`'s `Revising ⇄ AwaitingPeer`)
/// legitimately records the same `StageId` more than once, and a policy
/// reading "what did this stage last produce" wants the latest, not the
/// first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StageLedger {
    completed: Vec<StageOutcome>,
}

impl StageLedger {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record(&mut self, outcome: StageOutcome) {
        self.completed.push(outcome);
    }

    pub(crate) fn outcome(&self, stage: &StageId) -> Option<&StageOutcome> {
        self.completed
            .iter()
            .rev()
            .find(|outcome| &outcome.stage == stage)
    }

    pub(crate) fn completed_stages(&self) -> impl Iterator<Item = &StageId> {
        self.completed.iter().map(|outcome| &outcome.stage)
    }

    pub(crate) fn artifacts_for<'a>(
        &'a self,
        stage: &StageId,
    ) -> impl Iterator<Item = &'a ArtifactRef> {
        self.outcome(stage)
            .into_iter()
            .flat_map(|outcome| outcome.outputs.iter().map(|output| &output.artifact))
    }
}

/// The durable step `JobPolicy::transition` computes from a stage's outcome —
/// the contract's "missing reducer": `classify_attempt` decides one
/// attempt's outcome and `terminal` decides the run's final verdict, but
/// nothing between those turned `(ledger, stage outcome)` into what the
/// ledger holds next. The runner persists exactly what this returns before
/// it calls `next_stage` again; it never derives ledger progress on its own
/// initiative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transition {
    /// Record `StageOutcome` and continue — `next_stage` decides what (if
    /// anything) runs next, including re-yielding this same `StageId` for a
    /// revision loop (`plan`'s `Revising ⇄ AwaitingPeer`, `plan_job.rs:
    /// 2104-2155`).
    Continue(StageOutcome),
    /// Record `StageOutcome` and end the sequence now, without another
    /// `next_stage` call. `JobPolicy::terminal` still supplies the actual
    /// `Terminal` verdict — this only stops the loop.
    Terminal(StageOutcome),
}

/// One attempt's full addressable context. A first contract draft had
/// `prompt(stage, attempt)`, which cannot build the prompts that matter most:
/// a judge prompt assembled from reviewers' outputs, a peer-review prompt
/// needing the author's plan document, or a schema-repair prompt embedding
/// the failed attempt's own stdout — none of those are functions of the
/// stage and candidate alone. `prior_attempt_output` is `Some` exactly on a
/// `RetrySameCandidate` action; every other action starts a fresh candidate
/// with no prior output to embed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AttemptContext<'a> {
    pub(crate) stage: &'a Stage,
    pub(crate) slot: &'a Slot,
    pub(crate) attempt_index: u32,
    pub(crate) candidate: &'a ApprovedExecution,
    pub(crate) prior_stages: &'a StageLedger,
    pub(crate) prior_attempt_output: Option<&'a ArtifactRef>,
}

/// What a policy returns from `JobPolicy::prompt` — prompt and schema
/// material only. **Not** `dispatch::SpawnRequest`: that struct carries
/// `cwd`, `env`, `stdout_path`/`stderr_path`, `sandbox_profile`,
/// `worker_resource_limits`, and `commit_receipt_socket` (`dispatch.rs:
/// 240-250`), any of which would hand a policy process authority and let it
/// bypass runner-enforced posture — directly contradicting the purity rule
/// ("a policy never touches `RunHandle`, bd, git, or a process"). The
/// `AttemptExecutor` builds the trusted spawn envelope from this plus the
/// candidate and stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptMaterial {
    pub(crate) prompt: String,
    /// Opaque schema identity or inline schema text a policy's
    /// `classify_attempt` uses to validate the attempt's output. `None` for
    /// a job stage with no structured-output requirement.
    pub(crate) response_schema: Option<String>,
}

/// The generic run-ending verdict. Reuses [`run::TerminalVerdict`] verbatim
/// (`run.rs:1013-1019`, added by `1078a1f`) rather than a parallel enum —
/// see that type's own doc comment for the `plan` mapping (`Accepted ->
/// Completed`, `Rejected -> Failed`) this contract adopts wholesale.
///
/// `TerminalVerdict` itself stays fieldless because it doubles as the
/// `RunEvent::terminal_verdict` wire discriminant, which is
/// `#[serde(deny_unknown_fields)]` (`run.rs:1010-1019`) — giving it a
/// payload there would change that schema. `Blocked`, `NeedsInput`, and
/// often `Failed` need an operator-legible reason alongside the verdict
/// (`plan`'s existing `Blocked`/`NeedsInput` finish paths already pass one
/// as a plain `&str`, e.g. `run.rs:1948-1950`), so this wraps rather than
/// duplicates: the reused enum stays the wire type, and `reason` is carried
/// alongside it instead of folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Terminal {
    pub(crate) verdict: TerminalVerdict,
    pub(crate) reason: Option<String>,
}

impl Terminal {
    pub(crate) const fn completed() -> Self {
        Self {
            verdict: TerminalVerdict::Completed,
            reason: None,
        }
    }

    pub(crate) fn blocked(reason: impl Into<String>) -> Self {
        Self {
            verdict: TerminalVerdict::Blocked,
            reason: Some(reason.into()),
        }
    }
}

/// Which digest a job re-checks at a stage boundary before any model call —
/// drift since approval is a fail-closed terminal, never a retry. Per-job
/// sets genuinely differ (contract table under "Approval must be
/// re-validated per stage"); extend this enum rather than adding a
/// job-specific escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DigestKind {
    TargetHead,
    TargetStatus,
    TargetSha256,
    RosterPolicySha256,
    SchedulerPolicySha256,
    DeckResponseAfterApprovalWatermark,
    PlanSha256,
    RosterSha256,
    BeadClaimOwnership,
}

/// Per-run model-call ceiling, reserve-never-refund. Generalizes
/// `adversarial::ReviewerCallBudget` (`adversarial.rs:158-189`) — same
/// atomic reserve-or-fail-closed shape, off `review` specifically. There is
/// deliberately no release/refund method: after a crash the caller cannot
/// know whether the spawn it was about to make actually happened, so
/// over-counting is the only safe direction (contract "Durable call
/// budget", open question 2).
#[derive(Debug)]
pub(crate) struct CallBudget {
    ceiling: u32,
    reserved: AtomicU32,
}

impl CallBudget {
    pub(crate) const fn new(ceiling: u32) -> Self {
        Self {
            ceiling,
            reserved: AtomicU32::new(0),
        }
    }

    /// Default worst-case ceiling: `sum over stages of (slots' chain_length
    /// x attempts_per_candidate)` — the contract's stated default for
    /// `JobPolicy::call_budget` when a job has no formula of its own (only
    /// `review`'s exists today, `adversarial.rs:868-871`).
    pub(crate) fn worst_case(stage_plan: &[Stage]) -> Self {
        let ceiling = stage_plan.iter().fold(0u32, |total, stage| {
            let attempts_per_candidate = u32::from(stage.attempt_budget.value());
            let per_stage = stage.slots.iter().fold(0u32, |sum, slot| {
                let chain_length = u32::try_from(slot.candidates.len()).unwrap_or(u32::MAX);
                sum.saturating_add(chain_length.saturating_mul(attempts_per_candidate))
            });
            total.saturating_add(per_stage)
        });
        Self::new(ceiling)
    }

    /// Reserves one call against the ceiling, atomically. Fails closed once
    /// the ceiling is reached; the returned count on success is the new
    /// reserved total (mirrors `ReviewerCallBudget::reserve`,
    /// `adversarial.rs:172-184`).
    pub(crate) fn reserve(&self) -> Result<u32> {
        self.reserved
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |reserved| {
                (reserved < self.ceiling).then_some(reserved + 1)
            })
            .map(|reserved| reserved + 1)
            .map_err(|reserved| {
                RunnerError::new(format!(
                    "call budget exhausted: {reserved}/{}",
                    self.ceiling
                ))
            })
    }

    pub(crate) fn reserved(&self) -> u32 {
        self.reserved.load(Ordering::SeqCst)
    }

    pub(crate) const fn ceiling(&self) -> u32 {
        self.ceiling
    }

    /// Reconstructs a budget after a crash or resume, per the contract's
    /// "Durable call budget" resolution: `ceiling` is the run's pinned
    /// `run::RunLimits.max_attempts` (a missing ceiling is the caller's
    /// fail-closed refusal, not this constructor's — see
    /// [`AttemptRunner::run`]), and `consumed` is the count of durable
    /// `AttemptStarted` events already carrying `run::InvocationEvidence`.
    /// Seeding `reserved` with `consumed` — rather than starting fresh at
    /// zero — is what makes reserve-never-refund span a crash: a resumed
    /// run can never spend more than `ceiling` calls across its whole
    /// lifetime, not just its current process.
    pub(crate) const fn reconstructed(ceiling: u32, consumed: u32) -> Self {
        Self {
            ceiling,
            reserved: AtomicU32::new(consumed),
        }
    }
}

/// Injectable wall-clock source. `dispatch_cycle::ItemDeadline`
/// (`dispatch_cycle.rs:90-112`) reads `Instant::now()` directly; this
/// generalizes it behind a trait so [`ItemDeadline::remaining_at`] stays
/// deterministically testable without sleeping.
/// `Sync` so a `&dyn Clock` can be shared into concurrent slot threads (see
/// `## Concurrency` / [`AttemptRunner::run`]) — every attempt on every slot
/// reads it to time its own execution.
pub(crate) trait Clock: Sync {
    fn now(&self) -> Instant;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// One caller-owned monotonic ceiling shared by worker, mechanical verify,
/// and qualitative review — not a per-phase timeout. Mirrors
/// `dispatch_cycle::ItemDeadline` (`dispatch_cycle.rs:90-112`) field-for-field
/// and method-for-method, generalized out of that module so all four jobs
/// share one deadline type.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ItemDeadline {
    instant: Instant,
}

impl ItemDeadline {
    pub(crate) fn start_at(start: Instant, timeout: Duration) -> Self {
        Self {
            instant: start + timeout,
        }
    }

    pub(crate) fn capped_from(self, start: Instant, timeout: Duration) -> Self {
        Self {
            instant: self.instant.min(start + timeout),
        }
    }

    pub(crate) fn remaining_at(self, now: Instant) -> Option<Duration> {
        self.instant
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
    }
}

/// A disposable checkout a stage declaring `TargetKind::GitWorktreeIsolated`
/// runs its attempt inside. `Drop` removes it unconditionally, even on an
/// early return or panic — the shape is lifted from `plan_job::
/// with_isolated_worktree`'s unconditional `--force` removal
/// (`plan_job.rs:3078-3116`), but that function's *code* is not moved by
/// this pass; this is an independent implementation of the same removal
/// contract so the type is usable once `WorktreePort::create` is
/// implemented in pass (b).
pub(crate) struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

impl Worktree {
    pub(crate) fn new(repo: PathBuf, path: PathBuf) -> Self {
        Self { repo, path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .stdin(Stdio::null())
            .output();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Bead access, routed through the runner's durable boundary. `loop::
/// LoopClaim` (`loop.rs:132`) has only `release`/`close` and cannot claim;
/// both adversarial reviewers of the contract flagged that a claim outside
/// the kernel places a mutation before the durable boundary. Jobs never
/// touch bd directly — only the runner calls this, and only when
/// `JobPolicy::claims_bead()` is true.
///
/// `release` intentionally does not match the contract's original
/// `release(repo, bead, reason)` pseudocode: `BdClient::release_owned`
/// (`bd.rs:193-209`, added `b88da79`) — the primitive this method **must**
/// route through, never the raw unconditional `BdClient::release` — takes
/// `expected_assignee`, not a reason, so the port's signature is corrected
/// to match the actual primitive rather than the stale sketch.
pub(crate) trait BeadGateway {
    fn show(&self, repo: &Path, id: &str) -> bd::Result<Issue>;
    fn claim(&self, repo: &Path, id: &str, owner: &str) -> bd::Result<Issue>;
    fn release(&self, repo: &Path, id: &str, expected_assignee: &str) -> bd::Result<Issue>;
    fn close(&self, repo: &Path, id: &str, reason: &str) -> bd::Result<Issue>;
    fn comment(&self, repo: &Path, id: &str, text: &str) -> bd::Result<Comment>;
}

/// Runs one attempt, posture-selected: `MutationPosture::RepositoryWrite`
/// dispatches through `dispatch::run_with_heartbeat` (`dispatch.rs:513`);
/// `MutationPosture::ReadOnly` dispatches through `dispatch::run_readonly`
/// (`dispatch.rs:506`, widened to take `WorkerHooks` and return
/// `DispatchResult` by prep `48a21b9`). The executor — not the policy —
/// builds the trusted spawn envelope from `prompt`, `candidate`, and
/// `stage`; see [`PromptMaterial`] for why the policy never sees a
/// `dispatch::SpawnRequest`.
/// `Sync` so a `&dyn AttemptExecutor` can be shared into concurrent slot
/// threads (see `## Concurrency` / [`AttemptRunner::run`]) — one stage's
/// slots each call `execute` from their own thread against the same
/// injected executor.
pub(crate) trait AttemptExecutor: Sync {
    fn execute(
        &self,
        posture: MutationPosture,
        stage: &Stage,
        candidate: &ApprovedExecution,
        prompt: &PromptMaterial,
    ) -> dispatch::Result<DispatchResult>;
}

/// Creates the disposable isolated worktree a `GitWorktreeIsolated` stage
/// runs inside. Not a `dispatch.rs` primitive — `plan_job` hand-rolls it
/// today (`with_isolated_worktree`, `plan_job.rs:3078`, 8 call sites); this
/// lifts the *shape* only; the creation logic itself is not moved by this
/// pass.
pub(crate) trait WorktreePort {
    fn create(&self, repo: &Path, head: &str) -> Result<Worktree>;
}

/// The only per-job seam. A policy is **pure**: it builds prompts,
/// classifies output, and computes durable progress and a terminal verdict.
/// It never touches `RunHandle`, bd, git, or a process — the runner owns
/// all of those. This is what lets `adversarial.rs` keep its self-enforced
/// isolation invariant (the eleven-string production-code scan,
/// `adversarial.rs:3172-3193`) after it becomes a `JobPolicy` implementor.
///
/// **Correction to the contract's original method list**:
/// `requires_pinned_roster() -> bool` is **not** a method here. `RunJob` has
/// exactly four variants (`run.rs:81-86`), the v1 spec states the bootstrap
/// probe is a preflight and never a fifth job, and the probe must append
/// Musterroll evidence and re-snapshot the roster — mutations a pure policy
/// may not perform. `requires_pinned_roster` is instead a property of a
/// runner preflight phase, not of `JobPolicy`.
/// `Sync` so a `&dyn JobPolicy` can be shared into concurrent slot threads
/// (see `## Concurrency` / [`AttemptRunner::run`]) — `prompt` and
/// `classify_attempt` are called from each slot's own thread against the
/// same injected policy.
pub(crate) trait JobPolicy: Sync {
    fn job(&self) -> run::RunJob;
    fn posture(&self) -> MutationPosture;
    /// Declares bd participation; only `work` returns `true` (verified:
    /// neither `plan_job` nor `adversarial` touches bd).
    fn claims_bead(&self) -> bool;
    fn revalidation_digests(&self) -> &[DigestKind];

    /// Worst-case model calls for the whole run. Defaults to
    /// [`CallBudget::worst_case`]; a policy overrides this only when that
    /// default undercounts its real worst case.
    fn call_budget(&self, stage_plan: &[Stage]) -> CallBudget {
        CallBudget::worst_case(stage_plan)
    }

    /// The next stage to run, computed purely from completed-stage evidence.
    /// `None` ends the sequence. A revision loop (`plan`'s `Revising ⇄
    /// AwaitingPeer`) is simply this yielding the same `StageId` again after
    /// reading the prior peer-review outcome off `ledger`.
    fn next_stage(&self, ledger: &StageLedger) -> Option<Stage>;

    fn prompt(&self, ctx: AttemptContext<'_>) -> PromptMaterial;

    /// `None` means "runner default": classify purely from the process-level
    /// `DispatchResult` (success -> accept, a `DispatchFailure` ->
    /// `AttemptOutcome::Dispatch`, and so on). `Some` lets the policy refine
    /// or override that default reading of a successful process (a schema
    /// that failed to parse, a repair that succeeded, eligibility lost mid
    /// run, drift since approval).
    fn classify_attempt(
        &self,
        ctx: AttemptContext<'_>,
        output: &AttemptOutput,
    ) -> Option<AttemptOutcome>;

    fn aggregate_stage(&self, stage: &Stage, slot_results: &[SlotResult]) -> StageOutcome;

    fn transition(&self, ledger: &StageLedger, stage_outcome: StageOutcome) -> Transition;

    fn terminal(&self, ledger: &StageLedger) -> Terminal;
}

/// Live provider of the *currently observed* value for one [`DigestKind`],
/// so the runner can revalidate approval/digest drift at every stage
/// boundary (`## Approval must be re-validated per stage`) without a
/// [`JobPolicy`] ever touching git, bd, or a process itself — the purity
/// rule the contract states for policies. Pass (a) named exactly the four
/// ports plus [`Clock`]; this fifth exists because the loop genuinely
/// cannot implement "revalidate approval + the job's declared digests"
/// without *some* way to ask "what does this digest currently read as" —
/// none of the four named ports answers that for a job-declared
/// [`DigestKind`] like `RosterPolicySha256` or `PlanSha256`. The pinned
/// (approval-time) value to compare against is supplied by the caller via
/// [`RunRequest::pinned_digests`]; this port supplies the live one.
pub(crate) trait DigestSource {
    fn current(&self, kind: DigestKind) -> Result<String>;
}

/// The concrete ports one [`AttemptRunner::run`] call is injected with.
/// `executor`, `policy`, and `clock` are read from concurrent slot threads
/// (`## Concurrency`) and so require `Sync` — enforced as a supertrait on
/// [`JobPolicy`], [`AttemptExecutor`], and [`Clock`] themselves, so a
/// `&dyn` of any of them is usable across a `std::thread::scope` spawn
/// without a redundant bound here. `exec`, `commits`, `bd`, and `digests`
/// are touched only by the single runner thread (preflight and terminal
/// handling), so they carry no such requirement.
pub(crate) struct RunnerPorts<'a> {
    pub(crate) exec: &'a dyn dispatch::Exec,
    pub(crate) commits: &'a dyn dispatch::CommitProbe,
    pub(crate) bd: &'a dyn BeadGateway,
    pub(crate) executor: &'a dyn AttemptExecutor,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) digests: &'a dyn DigestSource,
}

/// Caller-supplied context for one [`AttemptRunner::run`] invocation.
/// `state_dir` and `backend` exist only because [`quarantine::RepoLease`]
/// and [`dispatch::Exec::auth_readiness`] need them and no pass (a) port
/// carries them; `owner` is the bd actor identity both `claim` and
/// `release`'s `expected_assignee` use (`BeadGateway::release` routes
/// through `BdClient::release_owned`, which compares against exactly this
/// string). `pinned_digests` is the approval-time snapshot
/// [`DigestSource::current`] is checked against at every stage boundary;
/// populating it from a run's pinned manifest/approval envelope is a
/// migration-time (`conductor-vd3y`) concern the generic loop does not own.
#[derive(Debug, Clone)]
pub(crate) struct RunRequest {
    pub(crate) state_dir: PathBuf,
    pub(crate) backend: Backend,
    pub(crate) owner: String,
    pub(crate) pinned_digests: BTreeMap<DigestKind, String>,
}

/// Implements every [`BeadGateway`] method over the existing [`BdClient`],
/// per the contract's `### BeadGateway` section ("Implement over the
/// existing `BdClient` — do not write a second bd client"). `release`
/// deliberately routes through `BdClient::release_owned`
/// (`conductor-moe`, `b88da79`), never the raw unconditional
/// `BdClient::release` — see that method's own doc comment for why a raw
/// release is unsafe.
impl<T: BdClient> BeadGateway for T {
    fn show(&self, repo: &Path, id: &str) -> bd::Result<Issue> {
        BdClient::show(self, repo, id)
    }

    fn claim(&self, repo: &Path, id: &str, owner: &str) -> bd::Result<Issue> {
        BdClient::claim(self, repo, id, owner)
    }

    fn release(&self, repo: &Path, id: &str, expected_assignee: &str) -> bd::Result<Issue> {
        BdClient::release_owned(self, repo, id, expected_assignee)
    }

    fn close(&self, repo: &Path, id: &str, reason: &str) -> bd::Result<Issue> {
        BdClient::close(self, repo, id, reason)
    }

    fn comment(&self, repo: &Path, id: &str, text: &str) -> bd::Result<Comment> {
        BdClient::comment(self, repo, id, text)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The runner's default reading of a completed process — used only when
/// `JobPolicy::classify_attempt` returns `None` ("runner default": success
/// -> accept, a `DispatchFailure` -> `AttemptOutcome::Dispatch`, and so
/// on). `DispatchStatus::Success` with no policy override is an implicit
/// accept and has no `AttemptOutcome` representation at all — callers
/// branch on that before reaching for this function.
fn dispatch_default_outcome(result: &DispatchResult) -> Option<AttemptOutcome> {
    match &result.status {
        dispatch::DispatchStatus::Success => None,
        dispatch::DispatchStatus::Failed(DispatchFailure::UnauthenticatedCommit) => {
            Some(AttemptOutcome::CommitAuthentication(
                result
                    .authentication_rejection
                    .unwrap_or(CommitAuthenticationRejection::ReceiptAbsent),
            ))
        }
        dispatch::DispatchStatus::Failed(failure) => {
            Some(AttemptOutcome::Dispatch(failure.clone()))
        }
    }
}

/// One real attempt's full trace, collected inside a slot's own thread and
/// handed back to the single-writer runner to journal only after the join
/// (`## Concurrency`: "Concurrent slots return results; they never touch
/// `RunHandle`, the state file, or the journal").
#[derive(Debug, Clone)]
struct AttemptRecord {
    candidate: ApprovedExecution,
    attempt_index: u32,
    /// `true` exactly when this record is a `RetrySameCandidate` chained
    /// continuation of the record immediately before it in this slot's
    /// trace — how the post-join writer links
    /// `InvocationEvidence::retry_of` to the prior attempt's `event_id`.
    continues_prior: bool,
    input_sha256: String,
    duration_ms: Option<u64>,
    /// The attempt's captured output, present whenever
    /// [`AttemptExecutor::execute`] returned `Ok(..)` at all (regardless of
    /// accept/fail — a failed dispatch still produced stdout bytes). Its
    /// `artifact.path` is pre-assigned by the walking thread so
    /// `classify_attempt` sees a stable identity; the post-join writer
    /// re-derives the same bytes' hash via `RunHandle::capture_artifact`
    /// rather than trusting this copy, so the journal's evidence is always
    /// what was actually persisted.
    output: Option<AttemptOutput>,
    /// The real file `execute` wrote the attempt's stdout to, so the
    /// post-join single writer can `capture_artifact` from it. `None`
    /// exactly when `output` is `None`.
    stdout_path: Option<PathBuf>,
    /// Set only when `execute` itself returned `Err` — an infrastructure
    /// failure below the level any `AttemptOutcome` classifies (no output
    /// exists to classify), so it always ends the slot.
    executor_error: Option<String>,
    outcome_label: String,
    action: AttemptAction,
}

struct SlotTrace {
    slot: u32,
    records: Vec<AttemptRecord>,
    outcome: SlotOutcome,
}

/// Walks one slot's own ordered candidate chain to `Accept`, `Fatal`, or
/// exhaustion — the inner half of the contract's two-level shape (`##
/// Shape`: "per slot, walk its OWN ordered candidate chain"). Runs entirely
/// inside one thread with no access to `RunHandle`; see [`AttemptRecord`]
/// and `## Concurrency`.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the ports a slot's walk genuinely needs; bundling them would just move the same count into a struct"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one candidate's attempt loop is a single linear state machine (reserve -> execute -> classify -> act); splitting it would scatter, not clarify, the retry/advance/fatal decision it makes in one place"
)]
fn walk_slot(
    policy: &dyn JobPolicy,
    executor: &dyn AttemptExecutor,
    stage: &Stage,
    slot: &Slot,
    ledger: &StageLedger,
    budget: &CallBudget,
    clock: &dyn Clock,
    sequencer: &AtomicU64,
) -> SlotTrace {
    let mut records: Vec<AttemptRecord> = Vec::new();
    for candidate in &slot.candidates {
        let mut prior_output: Option<AttemptOutput> = None;
        let mut accepted: Option<AttemptOutput> = None;
        let mut fatal = false;
        for attempt_index in 1..=stage.attempt_budget.value() {
            let attempt_index = u32::from(attempt_index);
            let ctx = AttemptContext {
                stage,
                slot,
                attempt_index,
                candidate,
                prior_stages: ledger,
                prior_attempt_output: prior_output.as_ref().map(|output| &output.artifact),
            };
            let prompt = policy.prompt(ctx);
            let input_sha256 = sha256_hex(prompt.prompt.as_bytes());
            let continues_prior = prior_output.is_some();

            // Reserve BEFORE spawn (reserve-never-refund): after a crash we
            // cannot know whether the spawn happened, so over-counting is
            // the only safe direction.
            if budget.reserve().is_err() {
                let outcome = AttemptOutcome::BudgetExhausted {
                    detail: "run-wide call budget exhausted before this attempt could be reserved"
                        .to_string(),
                };
                match stage.action_for(&outcome) {
                    AttemptAction::AdvanceCandidate => break,
                    AttemptAction::Accept
                    | AttemptAction::RetrySameCandidate
                    | AttemptAction::Fatal => {
                        fatal = true;
                        break;
                    }
                }
            }

            let start = clock.now();
            let executed = executor.execute(policy.posture(), stage, candidate, &prompt);
            let duration_ms = u64::try_from(clock.now().duration_since(start).as_millis()).ok();

            let dispatch_result = match executed {
                Ok(result) => result,
                Err(error) => {
                    records.push(AttemptRecord {
                        candidate: candidate.clone(),
                        attempt_index,
                        continues_prior,
                        input_sha256,
                        duration_ms,
                        output: None,
                        stdout_path: None,
                        executor_error: Some(error.to_string()),
                        outcome_label: format!("executor_error: {error}"),
                        action: AttemptAction::Fatal,
                    });
                    fatal = true;
                    break;
                }
            };

            let output_bytes = std::fs::read(&dispatch_result.stdout_path).unwrap_or_default();
            let artifact = ArtifactRef {
                path: format!(
                    "attempts/attempt-{:010}.out",
                    sequencer.fetch_add(1, Ordering::SeqCst)
                ),
                sha256: sha256_hex(&output_bytes),
            };
            let output = AttemptOutput {
                bytes: output_bytes,
                artifact,
            };

            let outcome = policy
                .classify_attempt(ctx, &output)
                .or_else(|| dispatch_default_outcome(&dispatch_result));

            let Some(outcome) = outcome else {
                records.push(AttemptRecord {
                    candidate: candidate.clone(),
                    attempt_index,
                    continues_prior,
                    input_sha256,
                    duration_ms,
                    output: Some(output.clone()),
                    stdout_path: Some(dispatch_result.stdout_path.clone()),
                    executor_error: None,
                    outcome_label: "accepted".to_string(),
                    action: AttemptAction::Accept,
                });
                accepted = Some(output);
                break;
            };

            let action = stage.action_for(&outcome);
            records.push(AttemptRecord {
                candidate: candidate.clone(),
                attempt_index,
                continues_prior,
                input_sha256,
                duration_ms,
                output: Some(output.clone()),
                stdout_path: Some(dispatch_result.stdout_path.clone()),
                executor_error: None,
                outcome_label: format!("{outcome:?}"),
                action,
            });
            match action {
                AttemptAction::Accept => {
                    accepted = Some(output);
                    break;
                }
                AttemptAction::RetrySameCandidate => {
                    if attempt_index >= u32::from(stage.attempt_budget.value()) {
                        // The per-candidate attempt cap (`Stage::
                        // attempt_budget`) is exhausted: reclassify as a
                        // budget exhaustion and let the stage's own
                        // declared mapping (fail-closed to `Fatal` when
                        // unmapped, per `Stage::action_for`) decide whether
                        // to fall back to the next candidate.
                        let exhausted = AttemptOutcome::BudgetExhausted {
                            detail: format!(
                                "per-candidate attempt cap ({}) reached after a retry request",
                                stage.attempt_budget.value()
                            ),
                        };
                        if !matches!(
                            stage.action_for(&exhausted),
                            AttemptAction::AdvanceCandidate
                        ) {
                            fatal = true;
                        }
                        break;
                    }
                    prior_output = Some(output);
                }
                AttemptAction::AdvanceCandidate => break,
                AttemptAction::Fatal => {
                    fatal = true;
                    break;
                }
            }
        }

        if let Some(output) = accepted {
            return SlotTrace {
                slot: slot.index,
                records,
                outcome: SlotOutcome::Accepted(output),
            };
        }
        if fatal {
            return SlotTrace {
                slot: slot.index,
                records,
                outcome: SlotOutcome::Unaccepted,
            };
        }
    }
    SlotTrace {
        slot: slot.index,
        records,
        outcome: SlotOutcome::Unaccepted,
    }
}

/// Runs one stage's slots at `stage.concurrency`, batched exactly like
/// `adversarial::run_reviewers`'s reviewer panel (`adversarial.rs:998`:
/// `chunks(parallel)` + `std::thread::scope`, joined and collected before
/// the next batch) — the same idiom, generalized off a fixed reviewer
/// panel onto any stage's slots.
fn dispatch_stage_slots(
    policy: &dyn JobPolicy,
    executor: &dyn AttemptExecutor,
    stage: &Stage,
    ledger: &StageLedger,
    budget: &CallBudget,
    clock: &dyn Clock,
    sequencer: &AtomicU64,
) -> Result<Vec<SlotTrace>> {
    let mut traces = Vec::with_capacity(stage.slots.len());
    for batch in stage.slots.chunks(stage.concurrency.get()) {
        let batch_traces = std::thread::scope(|scope| {
            let handles = batch
                .iter()
                .map(|slot| {
                    scope.spawn(|| {
                        walk_slot(
                            policy, executor, stage, slot, ledger, budget, clock, sequencer,
                        )
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| RunnerError::new("attempt slot worker thread panicked"))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        traces.extend(batch_traces);
    }
    Ok(traces)
}

/// The single-writer phase: journals every attempt collected by
/// [`dispatch_stage_slots`] in deterministic slot order, exactly once, only
/// after the join — this function, and only this function, ever calls
/// `RunHandle::append_event`/`capture_artifact` for attempt evidence.
fn write_attempt_events(
    handle: &mut run::RunHandle,
    stage: &Stage,
    mut traces: Vec<SlotTrace>,
) -> Result<Vec<SlotResult>> {
    traces.sort_by_key(|trace| trace.slot);
    let mut next_seq = run::read_events(&handle.events_path())
        .map_err(|error| RunnerError::new(error.to_string()))?
        .len() as u64
        + 1;
    let mut results = Vec::with_capacity(traces.len());
    for trace in &traces {
        let mut last_started_id: Option<String> = None;
        for record in &trace.records {
            let retry_of = record
                .continues_prior
                .then(|| last_started_id.clone())
                .flatten();
            let started_event_id = format!("{}-{next_seq:06}", handle.run_id());
            handle
                .append_event(
                    run::EventKind::AttemptStarted,
                    run::EventInput {
                        profile_id: Some(record.candidate.profile_id.clone()),
                        invocation: Some(run::InvocationEvidence {
                            stage: stage.id.as_str().to_string(),
                            slot: trace.slot,
                            attempt: record.attempt_index,
                            execution: record.candidate.clone(),
                            input_sha256: record.input_sha256.clone(),
                            output_sha256: None,
                            duration_ms: None,
                            tokens: None,
                            retry_of: retry_of.clone(),
                        }),
                        ..run::EventInput::default()
                    },
                )
                .map_err(|error| RunnerError::new(error.to_string()))?;
            next_seq += 1;
            last_started_id = Some(started_event_id);

            let (artifact_refs, output_sha256) = if let Some(output) = &record.output {
                let source = record
                    .stdout_path
                    .as_deref()
                    .ok_or_else(|| RunnerError::new("attempt output has no source path"))?;
                let captured = handle
                    .capture_artifact(source, Path::new(&output.artifact.path))
                    .map_err(|error| RunnerError::new(error.to_string()))?;
                (vec![captured.clone()], Some(captured.sha256))
            } else {
                (Vec::new(), None)
            };

            handle
                .append_event(
                    run::EventKind::AttemptFinished,
                    run::EventInput {
                        profile_id: Some(record.candidate.profile_id.clone()),
                        artifact_refs,
                        outcome: Some(record.outcome_label.clone()),
                        invocation: Some(run::InvocationEvidence {
                            stage: stage.id.as_str().to_string(),
                            slot: trace.slot,
                            attempt: record.attempt_index,
                            execution: record.candidate.clone(),
                            input_sha256: record.input_sha256.clone(),
                            output_sha256,
                            duration_ms: record.duration_ms,
                            tokens: None,
                            retry_of,
                        }),
                        ..run::EventInput::default()
                    },
                )
                .map_err(|error| RunnerError::new(error.to_string()))?;
            next_seq += 1;
        }
        results.push(SlotResult {
            slot: trace.slot,
            outcome: trace.outcome.clone(),
        });
    }
    Ok(results)
}

/// Revalidates `policy.revalidation_digests()` against `pinned`, using
/// `digests` for the live reading. `Ok(Some(reason))` reports drift for the
/// caller to turn into a fail-closed `Terminal::blocked` (contract:
/// "drift since approval = fail closed terminal, never a retry"); `Err`
/// is reserved for a genuine wiring problem (a declared digest with no
/// pinned value, or the digest source itself failing) rather than drift.
fn revalidate_digests(
    policy: &dyn JobPolicy,
    digests: &dyn DigestSource,
    pinned: &BTreeMap<DigestKind, String>,
) -> Result<Option<String>> {
    for kind in policy.revalidation_digests() {
        let expected = pinned.get(kind).ok_or_else(|| {
            RunnerError::new(format!(
                "policy declares revalidation digest {kind:?} but no pinned value was supplied"
            ))
        })?;
        let observed = digests.current(*kind)?;
        if &observed != expected {
            return Ok(Some(format!(
                "approval/digest drift on {kind:?}: expected {expected:?}, observed {observed:?}"
            )));
        }
    }
    Ok(None)
}

/// Durably records one stage's outcome as a `stage_finished` event, before
/// the ledger is used to pick another stage — the contract's "missing
/// reducer" durability requirement ("the runner persists exactly what
/// \[`Transition`\] returns before it calls `next_stage` again"). This is
/// what [`reconstruct_stage_ledger`] later replays on resume (bead
/// `conductor-v37z`). `artifact_refs` re-assert the exact `ArtifactRef`s
/// [`write_attempt_events`] already captured for this stage's accepted
/// attempts — no new bytes are written here, only their identity is
/// re-declared as this stage's durable evidence.
fn write_stage_finished_event(
    handle: &mut run::RunHandle,
    outcome: &StageOutcome,
    transition: run::StageTransitionKind,
) -> Result<()> {
    handle
        .append_event(
            run::EventKind::StageFinished,
            run::EventInput {
                artifact_refs: outcome
                    .outputs
                    .iter()
                    .map(|output| output.artifact.clone())
                    .collect(),
                stage_progress: Some(run::StageProgress {
                    stage: outcome.stage.as_str().to_string(),
                    transition,
                }),
                ..run::EventInput::default()
            },
        )
        .map_err(|error| RunnerError::new(error.to_string()))
}

/// Reconstructs the durable [`StageLedger`] a resumed run must continue from,
/// rather than restarting it empty and replaying every already-completed
/// stage — bead `conductor-v37z`'s defect: budget is reserve-never-refund by
/// design, so a replayed stage permanently consumes approved ceiling a
/// crash-then-resume should never have spent.
///
/// Replays every `stage_finished` event in `events` (already
/// sequence-validated by [`run::read_events`]) in journal order. For each,
/// its artifact bytes are re-read from `handle`'s run directory and
/// re-hashed against the pinned `sha256` — the ledger is only ever built
/// from evidence that is still exactly what the journal says it is, never
/// guessed. `@2` journals, and any `@3` journal written before this bead,
/// carry no `stage_finished` events at all and correctly reconstruct to an
/// empty ledger — identical to the pre-fix behavior; this only changes
/// journals this binary writes going forward.
///
/// Fails closed — `Err`, never a partial or guessed ledger — on: a
/// `stage_finished` event with no `stage_progress` (an uninterpretable
/// record — some writer emitted the kind without the payload reconstruction
/// depends on); an invalid stage id; or a referenced artifact missing from
/// disk or no longer matching its pinned hash.
///
/// The returned `bool` is `true` exactly when the *last* `stage_finished`
/// event recorded a [`run::StageTransitionKind::Terminal`] transition,
/// meaning a prior process had already decided to end the run without
/// consulting `next_stage` again (`## Shape`: "`Transition::Terminal`:
/// ... end the sequence now, without another `next_stage` call"). Resume
/// must honor that same skip: `next_stage` is only a pure function of
/// completed-stage evidence, and a policy is free to have it disagree with a
/// `Terminal` transition already returned for that exact ledger state —
/// calling it anyway on resume could resurrect a stage the policy had
/// already decided was done.
fn reconstruct_stage_ledger(
    handle: &run::RunHandle,
    events: &[run::RunEvent],
) -> Result<(StageLedger, bool)> {
    let mut ledger = StageLedger::new();
    let mut last_was_terminal = false;
    for event in events {
        if event.kind != run::EventKind::StageFinished {
            continue;
        }
        let progress = event.stage_progress.as_ref().ok_or_else(|| {
            RunnerError::new(format!(
                "stage_finished event {} carries no stage_progress; refusing to guess the ledger",
                event.event_id
            ))
        })?;
        let stage = StageId::new(progress.stage.clone())?;
        let mut outputs = Vec::with_capacity(event.artifact_refs.len());
        for artifact in &event.artifact_refs {
            let path = handle.dir().join(&artifact.path);
            let bytes = std::fs::read(&path).map_err(|error| {
                RunnerError::new(format!(
                    "stage_finished artifact {} unreadable: {error}",
                    artifact.path
                ))
            })?;
            let actual = sha256_hex(&bytes);
            if actual != artifact.sha256 {
                return Err(RunnerError::new(format!(
                    "stage_finished artifact {} hash mismatch: journal pins {}, disk has {actual}",
                    artifact.path, artifact.sha256
                )));
            }
            outputs.push(AttemptOutput {
                bytes,
                artifact: artifact.clone(),
            });
        }
        ledger.record(StageOutcome { stage, outputs });
        last_was_terminal = matches!(progress.transition, run::StageTransitionKind::Terminal);
    }
    Ok((ledger, last_was_terminal))
}

/// Terminal handling: durable evidence first (`RunHandle::finish_with_verdict`,
/// itself event-then-manifest per `1078a1f`), then the single Bead mutation
/// last — `## Shape`: "terminal: policy.terminal(ledger) -> durable
/// evidence FIRST, then the one Bead mutation". Only `Completed` closes;
/// every other verdict releases (`## Shape`: "Only `Completed` may close a
/// Bead. `Blocked` and `NeedsInput` are not degraded success").
fn finalize(
    handle: &mut run::RunHandle,
    ports: &RunnerPorts<'_>,
    request: &RunRequest,
    terminal: Terminal,
    bead_claimed: bool,
) -> Result<Terminal> {
    let outcome = terminal
        .reason
        .clone()
        .unwrap_or_else(|| format!("{:?}", terminal.verdict).to_lowercase());
    handle
        .finish_with_verdict(outcome.clone(), terminal.verdict, Vec::new())
        .map_err(|error| RunnerError::new(error.to_string()))?;
    if bead_claimed {
        let repo = handle.manifest().target.repo.clone();
        let bead_id = handle.manifest().target.bead.clone().ok_or_else(|| {
            RunnerError::new("job claimed a bead but the manifest target has no bead id")
        })?;
        if matches!(terminal.verdict, TerminalVerdict::Completed) {
            ports
                .bd
                .close(Path::new(&repo), &bead_id, &outcome)
                .map_err(|error| RunnerError::new(error.to_string()))?;
        } else {
            ports
                .bd
                .release(Path::new(&repo), &bead_id, &request.owner)
                .map_err(|error| RunnerError::new(error.to_string()))?;
        }
    }
    Ok(terminal)
}

/// The generic attempt sequencer: `.docs/ai/phases/undertake-runner-contract.md`
/// `## Shape`, implemented over the ports and types pass (a) defined. No
/// job is migrated onto this — see that file's `## Migration hazard`
/// section for why `adversarial.rs` becomes a [`JobPolicy`] implementor
/// only in a later pass, not this one.
pub(crate) struct AttemptRunner;

impl AttemptRunner {
    /// Runs `policy` to a [`Terminal`] over `ports`, durably journaling
    /// every attempt into `handle` as the sole writer. `Err` means the
    /// runner refused to touch (or continue touching) `handle` at all —
    /// resume-liveness refusal, a missing call-budget ceiling, a busy repo
    /// lease, an unready backend, or a dirty preflight tree — and the run's
    /// durable state is left exactly as it was found, still resumable by a
    /// later attempt. `Ok(Terminal)` means a verdict was reached *and*
    /// durably written (journal, then the one Bead mutation) before
    /// returning.
    #[expect(
        clippy::too_many_lines,
        reason = "the preflight -> stage loop -> terminal sequence is the contract's one linear control flow; splitting it further would scatter, not clarify, the ordering it depends on"
    )]
    pub(crate) fn run(
        policy: &dyn JobPolicy,
        ports: &RunnerPorts<'_>,
        handle: &mut run::RunHandle,
        request: &RunRequest,
    ) -> Result<Terminal> {
        if handle.manifest().job != policy.job() {
            return Err(RunnerError::new(format!(
                "policy job {:?} does not match the run's pinned job {:?}",
                policy.job(),
                handle.manifest().job
            )));
        }

        // Resume: reclaim requires EVERY recorded slot provably dead: alive
        // or inconclusive refuses outright (`## Resume` #2). Empty on a
        // fresh run, where there is nothing yet to reclaim against. Only
        // `work` runs carry `WorkState` today (`RunHandle::work()` is
        // generic but returns `None` for the other three jobs); see the
        // report for why per-job worker-slot recording does not yet exist
        // for `review`/`consult`/`plan`.
        if let Some(work) = handle.work() {
            let slots = work.effective_worker_slots();
            if !slots.is_empty() && quarantine::worker_slots_authenticated_live(&slots) {
                return Err(RunnerError::new(
                    "refusing to resume: a previously recorded worker slot is still alive or its liveness is inconclusive",
                ));
            }
        }

        // Durable call-budget reconstruction (`## Open questions` #2):
        // `consumed` is the count of `AttemptStarted` events already
        // carrying `InvocationEvidence`; the ceiling is `RunLimits.
        // max_attempts`, and a missing ceiling is this runner's fail-closed
        // refusal (never `RunHandle::create`'s).
        let events = run::read_events(&handle.events_path())
            .map_err(|error| RunnerError::new(error.to_string()))?;
        let consumed = events
            .iter()
            .filter(|event| {
                event.kind == run::EventKind::AttemptStarted && event.invocation.is_some()
            })
            .count();
        let ceiling = handle.manifest().limits.max_attempts.ok_or_else(|| {
            RunnerError::new(
                "run limits are missing max_attempts; refusing to run without a durable call-budget ceiling",
            )
        })?;
        let ceiling = u32::try_from(ceiling)
            .map_err(|_| RunnerError::new("max_attempts does not fit u32"))?;
        let consumed = u32::try_from(consumed)
            .map_err(|_| RunnerError::new("consumed attempt count does not fit u32"))?;
        let budget = CallBudget::reconstructed(ceiling, consumed);
        let sequencer = AtomicU64::new(0);

        // Reconstruct completed stages from the journal instead of
        // restarting the ledger empty (`conductor-v37z`): a run resumed
        // after completing stage N must continue at N+1, not replay it and
        // permanently burn more of its reserve-never-refund budget.
        let (mut ledger, resumed_terminal) = reconstruct_stage_ledger(handle, &events)?;

        // A prior process's `Transition::Terminal` already decided this run
        // was over without ever consulting `next_stage` again (`## Shape`:
        // "`Transition::Terminal`: ... end the sequence now, without
        // another `next_stage` call") -- honor that same skip on resume
        // instead of asking the policy for a stage it never intended to
        // run again. Otherwise this is exactly the fresh-run selection:
        // `next_stage` on an empty ledger picks the first stage; on a
        // reconstructed non-empty ledger it picks up after the last
        // completed one.
        let mut stage = if resumed_terminal {
            None
        } else {
            policy.next_stage(&ledger)
        };
        if stage.is_none() && ledger.completed_stages().next().is_none() {
            // Nothing was ever durably recorded and the policy names no
            // stage at all -- the original zero-stage early return,
            // unchanged: no lease was ever needed and no bead was ever
            // claimed.
            let terminal = policy.terminal(&ledger);
            return finalize(handle, ports, request, terminal, false);
        }

        // Repo lease: taken once, held for the whole run, only when the
        // job's target is git-backed (`## Target kinds`: `ArtifactOnly`
        // takes no lease). Every job in practice declares one uniform
        // `TargetKind` across all its stages, so this reads it from
        // whichever stage is about to dispatch, falling back to the job's
        // first-ever stage when resume has nothing left to dispatch at all
        // (`stage` is `None` here only when a prior process already
        // recorded a terminal transition, or ran every stage to
        // exhaustion).
        let target_kind = match &stage {
            Some(next) => next.target_kind,
            None => {
                policy
                    .next_stage(&StageLedger::new())
                    .ok_or_else(|| {
                        RunnerError::new(
                            "resumed run has completed stages but the policy names no first \
                             stage to determine its target kind from",
                        )
                    })?
                    .target_kind
            }
        };
        let repo = handle.manifest().target.repo.clone();
        let _lease = if target_kind == TargetKind::ArtifactOnly {
            None
        } else {
            Some(
                quarantine::RepoLease::acquire(&request.state_dir, &repo, handle.run_id())
                    .map_err(|error| RunnerError::new(error.to_string()))?,
            )
        };

        match ports.exec.auth_readiness(request.backend) {
            dispatch::AuthReadiness::Ready => {}
            dispatch::AuthReadiness::NotAuthenticated { message }
            | dispatch::AuthReadiness::Unreadable { message } => {
                return Err(RunnerError::new(format!(
                    "backend not ready to dispatch: {message}"
                )));
            }
        }

        if target_kind == TargetKind::GitWorkingTree {
            let clean = ports
                .commits
                .is_clean(Path::new(&repo))
                .map_err(|error| RunnerError::new(error.to_string()))?;
            if !clean {
                return Err(RunnerError::new(format!(
                    "refusing to dispatch: target working tree {repo} is dirty"
                )));
            }
        }

        if let Some(reason) = revalidate_digests(policy, ports.digests, &request.pinned_digests)? {
            return finalize(handle, ports, request, Terminal::blocked(reason), false);
        }

        let mut bead_claimed = false;
        if policy.claims_bead() {
            let bead_id = handle.manifest().target.bead.clone().ok_or_else(|| {
                RunnerError::new("job claims a bead but the manifest target has no bead id")
            })?;
            ports
                .bd
                .claim(Path::new(&repo), &bead_id, &request.owner)
                .map_err(|error| RunnerError::new(error.to_string()))?;
            bead_claimed = true;
        }

        while let Some(current_stage) = stage.take() {
            // Revalidated at every stage boundary, including this first
            // one again: drift since the preflight check above is exactly
            // as fail-closed as drift caught there (`## Approval must be
            // re-validated per stage`).
            if let Some(reason) =
                revalidate_digests(policy, ports.digests, &request.pinned_digests)?
            {
                return finalize(
                    handle,
                    ports,
                    request,
                    Terminal::blocked(reason),
                    bead_claimed,
                );
            }

            let traces = dispatch_stage_slots(
                policy,
                ports.executor,
                &current_stage,
                &ledger,
                &budget,
                ports.clock,
                &sequencer,
            )?;
            let slot_results = write_attempt_events(handle, &current_stage, traces)?;
            let stage_outcome = policy.aggregate_stage(&current_stage, &slot_results);
            match policy.transition(&ledger, stage_outcome) {
                Transition::Continue(outcome) => {
                    write_stage_finished_event(handle, &outcome, run::StageTransitionKind::Continue)?;
                    ledger.record(outcome);
                    stage = policy.next_stage(&ledger);
                }
                Transition::Terminal(outcome) => {
                    write_stage_finished_event(handle, &outcome, run::StageTransitionKind::Terminal)?;
                    ledger.record(outcome);
                    break;
                }
            }
        }

        // D1's post-verify recheck before the Bead mutation: one more
        // revalidation immediately before the terminal write, so drift
        // introduced during the final stage cannot slip through under an
        // already-decided verdict.
        if bead_claimed {
            if let Some(reason) =
                revalidate_digests(policy, ports.digests, &request.pinned_digests)?
            {
                return finalize(
                    handle,
                    ports,
                    request,
                    Terminal::blocked(reason),
                    bead_claimed,
                );
            }
        }

        let terminal = policy.terminal(&ledger);
        finalize(handle, ports, request, terminal, bead_claimed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // ---- StageId validation ----------------------------------------------

    #[test]
    fn stage_id_accepts_valid_snake_case() {
        for value in ["planner", "peer_review", "second_opinion", "a", "a1_b2"] {
            assert!(
                StageId::new(value).is_ok(),
                "expected {value:?} to be valid"
            );
        }
    }

    #[test]
    fn stage_id_rejects_invalid_forms() {
        for value in [
            "",
            "PeerReview",
            "_leading",
            "trailing_",
            "double__underscore",
            "123abc",
            "has space",
            "has-dash",
        ] {
            assert!(
                StageId::new(value).is_err(),
                "expected {value:?} to be rejected"
            );
        }
    }

    // ---- Constraint generalization off StageId ----------------------------

    #[test]
    fn stage_constraints_generalize_off_stage_id_not_plan_stage() {
        let planner = StageId::new("planner").expect("valid");
        let peer_review = StageId::new("peer_review").expect("valid");
        let constraints = StageConstraints {
            distinct_execution_from: vec![planner.clone()],
            tier_at_least: vec![planner],
            provider_diversity: PlanProviderDiversity::PairwiseDistinct,
        };
        assert_eq!(
            constraints.distinct_execution_from,
            vec![StageId::new("planner").unwrap()]
        );
        assert_ne!(constraints.distinct_execution_from[0], peer_review);
    }

    #[test]
    fn stage_constraints_unconstrained_has_no_relationships() {
        let constraints = StageConstraints::unconstrained();
        assert!(constraints.distinct_execution_from.is_empty());
        assert!(constraints.tier_at_least.is_empty());
        assert_eq!(constraints.provider_diversity, PlanProviderDiversity::None);
    }

    // ---- AttemptOutcome/AttemptAction mapping is stage-declared -----------

    fn stage_with_mapping(
        id: &str,
        outcome_actions: BTreeMap<AttemptOutcomeCategory, AttemptAction>,
    ) -> Stage {
        Stage {
            id: StageId::new(id).expect("valid"),
            slots: Vec::new(),
            concurrency: NonZeroUsize::new(1).expect("nonzero"),
            target_kind: TargetKind::ArtifactOnly,
            constraints: StageConstraints::unconstrained(),
            attempt_budget: StageAttemptLimit::new(1).expect("nonzero"),
            outcome_actions,
        }
    }

    #[test]
    fn outcome_action_mapping_is_declared_per_stage_not_global() {
        let outcome = AttemptOutcome::RuntimeLimit(RuntimeLimitEvidence {
            provider: "opencode-go".to_string(),
            model: None,
            profile: "glm-5.2".to_string(),
            expires_at: "2026-07-28T00:00:00Z".to_string(),
            expiry_basis: crate::musterroll::ObservationExpiryBasis::ProviderReset,
            reason: crate::musterroll::RuntimeLimitReason::QuotaExceeded,
        });

        // "review"-shaped stage: a provider-wide limit advances the
        // candidate (stays inside the slot's approved chain).
        let review_stage = stage_with_mapping(
            "review",
            BTreeMap::from([(
                AttemptOutcomeCategory::RuntimeLimit,
                AttemptAction::AdvanceCandidate,
            )]),
        );
        // "plan"-shaped stage: the same outcome category blocks instead of
        // advancing, mirroring plan's refusal to fall back on a bound peer's
        // eligibility loss (plan_job.rs:1915-1922).
        let plan_stage = stage_with_mapping(
            "planner",
            BTreeMap::from([(AttemptOutcomeCategory::RuntimeLimit, AttemptAction::Fatal)]),
        );

        assert_eq!(
            review_stage.action_for(&outcome),
            AttemptAction::AdvanceCandidate
        );
        assert_eq!(plan_stage.action_for(&outcome), AttemptAction::Fatal);
    }

    #[test]
    fn schema_invalid_maps_to_retry_same_candidate_by_convention_not_enforcement() {
        let stage = stage_with_mapping(
            "planner",
            BTreeMap::from([(
                AttemptOutcomeCategory::SchemaInvalid,
                AttemptAction::RetrySameCandidate,
            )]),
        );
        let outcome = AttemptOutcome::SchemaInvalid {
            detail: "missing required field".to_string(),
        };
        assert_eq!(
            stage.action_for(&outcome),
            AttemptAction::RetrySameCandidate
        );
    }

    #[test]
    fn unmapped_outcome_category_fails_closed_to_fatal() {
        let stage = stage_with_mapping("planner", BTreeMap::new());
        let outcome = AttemptOutcome::Dispatch(DispatchFailure::TimedOut);
        assert_eq!(stage.action_for(&outcome), AttemptAction::Fatal);
    }

    // ---- CallBudget reserve-never-refund -----------------------------------

    #[test]
    fn call_budget_reserves_up_to_ceiling_then_fails_closed() {
        let budget = CallBudget::new(2);
        assert_eq!(budget.reserve().expect("first reserve"), 1);
        assert_eq!(budget.reserve().expect("second reserve"), 2);
        let error = budget.reserve().expect_err("third reserve must fail");
        assert!(error.to_string().contains("exhausted"));
        // Reservation is never released: the failed attempt above did not
        // decrement `reserved`.
        assert_eq!(budget.reserved(), 2);
        assert_eq!(budget.ceiling(), 2);
    }

    #[test]
    fn call_budget_zero_ceiling_rejects_every_reservation() {
        let budget = CallBudget::new(0);
        assert!(budget.reserve().is_err());
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn call_budget_worst_case_sums_slots_times_chain_length_times_attempts() {
        let candidate = ApprovedExecution {
            profile_id: "p".to_string(),
            provider_id: "prov".to_string(),
            availability_key: "avail".to_string(),
            execution_key: "exec".to_string(),
        };
        let stage = Stage {
            id: StageId::new("review").expect("valid"),
            slots: vec![
                Slot {
                    index: 0,
                    candidates: vec![candidate.clone(), candidate.clone()],
                },
                Slot {
                    index: 1,
                    candidates: vec![candidate.clone()],
                },
            ],
            concurrency: NonZeroUsize::new(2).expect("nonzero"),
            target_kind: TargetKind::ArtifactOnly,
            constraints: StageConstraints::unconstrained(),
            attempt_budget: StageAttemptLimit::new(3).expect("nonzero"),
            outcome_actions: BTreeMap::new(),
        };
        // slot 0: 2 candidates * 3 attempts = 6; slot 1: 1 candidate * 3 = 3.
        let budget = CallBudget::worst_case(std::slice::from_ref(&stage));
        assert_eq!(budget.ceiling(), 9);
    }

    // ---- StageLedger accessors ---------------------------------------------

    fn output(path: &str) -> AttemptOutput {
        AttemptOutput {
            bytes: path.as_bytes().to_vec(),
            artifact: ArtifactRef {
                path: path.to_string(),
                sha256: "deadbeef".to_string(),
            },
        }
    }

    #[test]
    fn stage_ledger_records_and_reads_back_completed_stages() {
        let mut ledger = StageLedger::new();
        let planner = StageId::new("planner").expect("valid");
        ledger.record(StageOutcome {
            stage: planner.clone(),
            outputs: vec![output("artifacts/plan-v1.json")],
        });
        assert_eq!(
            ledger.completed_stages().collect::<Vec<_>>(),
            vec![&planner]
        );
        let recorded = ledger.outcome(&planner).expect("outcome present");
        assert_eq!(recorded.outputs.len(), 1);
        let artifacts: Vec<_> = ledger.artifacts_for(&planner).collect();
        assert_eq!(artifacts, vec![&output("artifacts/plan-v1.json").artifact]);
    }

    #[test]
    fn stage_ledger_outcome_prefers_most_recent_entry_across_a_revision_loop() {
        let mut ledger = StageLedger::new();
        let planner = StageId::new("planner").expect("valid");
        ledger.record(StageOutcome {
            stage: planner.clone(),
            outputs: vec![output("artifacts/plan-v1.json")],
        });
        ledger.record(StageOutcome {
            stage: planner.clone(),
            outputs: vec![output("artifacts/plan-v2.json")],
        });
        let latest = ledger.outcome(&planner).expect("outcome present");
        assert_eq!(latest.outputs[0].artifact.path, "artifacts/plan-v2.json");
    }

    #[test]
    fn stage_ledger_unrecorded_stage_reads_empty() {
        let ledger = StageLedger::new();
        let planner = StageId::new("planner").expect("valid");
        assert!(ledger.outcome(&planner).is_none());
        assert_eq!(ledger.artifacts_for(&planner).count(), 0);
    }

    // ---- ItemDeadline -------------------------------------------------------

    #[test]
    fn item_deadline_reports_none_once_elapsed() {
        let start = Instant::now();
        let deadline = ItemDeadline::start_at(start, Duration::from_millis(10));
        assert!(deadline.remaining_at(start).is_some());
        let past_deadline = start + Duration::from_secs(1);
        assert!(deadline.remaining_at(past_deadline).is_none());
    }

    #[test]
    fn item_deadline_capped_from_never_extends_the_ceiling() {
        let start = Instant::now();
        let deadline = ItemDeadline::start_at(start, Duration::from_secs(10));
        let capped = deadline.capped_from(start, Duration::from_secs(1));
        let one_and_a_half_secs_later = start + Duration::from_millis(1500);
        assert!(capped.remaining_at(one_and_a_half_secs_later).is_none());
        assert!(deadline.remaining_at(one_and_a_half_secs_later).is_some());
    }

    #[test]
    fn system_clock_now_is_monotonic_with_instant_now() {
        let clock = SystemClock;
        let before = Instant::now();
        let observed = clock.now();
        let after = Instant::now();
        assert!(observed >= before && observed <= after);
    }

    // ---- Worktree Drop ------------------------------------------------------

    #[test]
    fn worktree_drop_removes_its_directory_unconditionally() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let scratch = std::env::temp_dir().join(format!("undertake-runner-worktree-test-{nanos}"));
        std::fs::create_dir_all(&scratch).expect("mkdir scratch");
        std::fs::write(scratch.join("marker"), b"x").expect("write marker");
        assert!(scratch.exists());
        {
            // `repo` need not be a real git repository: `git worktree
            // remove` failing against it is expected and ignored, proving
            // the unconditional `remove_dir_all` fallback (not a
            // successful `git worktree remove`) is what the guarantee rests
            // on.
            let _worktree = Worktree::new(PathBuf::from("/nonexistent-repo"), scratch.clone());
        }
        assert!(
            !scratch.exists(),
            "Worktree::drop must remove its path unconditionally"
        );
    }

    // ---- JobPolicy is implementable ----------------------------------------

    struct NullPolicy;

    impl JobPolicy for NullPolicy {
        fn job(&self) -> run::RunJob {
            run::RunJob::Work
        }

        fn posture(&self) -> MutationPosture {
            MutationPosture::RepositoryWrite
        }

        fn claims_bead(&self) -> bool {
            true
        }

        fn revalidation_digests(&self) -> &[DigestKind] {
            &[DigestKind::TargetHead]
        }

        fn next_stage(&self, _ledger: &StageLedger) -> Option<Stage> {
            None
        }

        fn prompt(&self, _ctx: AttemptContext<'_>) -> PromptMaterial {
            PromptMaterial {
                prompt: String::new(),
                response_schema: None,
            }
        }

        fn classify_attempt(
            &self,
            _ctx: AttemptContext<'_>,
            _output: &AttemptOutput,
        ) -> Option<AttemptOutcome> {
            None
        }

        fn aggregate_stage(&self, stage: &Stage, _slot_results: &[SlotResult]) -> StageOutcome {
            StageOutcome {
                stage: stage.id.clone(),
                outputs: Vec::new(),
            }
        }

        fn transition(&self, _ledger: &StageLedger, stage_outcome: StageOutcome) -> Transition {
            Transition::Terminal(stage_outcome)
        }

        fn terminal(&self, _ledger: &StageLedger) -> Terminal {
            Terminal::completed()
        }
    }

    #[test]
    fn job_policy_trait_is_implementable_end_to_end() {
        let policy = NullPolicy;
        assert_eq!(policy.job(), run::RunJob::Work);
        assert!(policy.claims_bead());
        let ledger = StageLedger::new();
        assert!(policy.next_stage(&ledger).is_none());
        let stage = stage_with_mapping("work", BTreeMap::new());
        let outcome = policy.aggregate_stage(&stage, &[]);
        let transition = policy.transition(&ledger, outcome);
        assert!(matches!(transition, Transition::Terminal(_)));
        assert_eq!(policy.terminal(&ledger).verdict, TerminalVerdict::Completed);
        let budget = policy.call_budget(&[stage]);
        assert_eq!(budget.ceiling(), 0);
    }

    /// Pass (b): `AttemptRunner::run` itself, driven entirely by fakes.
    /// Mirrors the crate's existing `Fake*` test-double convention
    /// (`FakeBdClient`, `Exec`/`CommitProbe` fakes throughout `dispatch.rs`,
    /// `quarantine.rs`, `verify.rs`) rather than inventing a different
    /// style for this module.
    mod attempt_runner_tests {
        use super::*;
        use std::collections::{HashMap, VecDeque};
        use std::sync::Mutex;

        struct TempDir(PathBuf);

        impl TempDir {
            fn new(label: &str) -> Self {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos();
                let path =
                    std::env::temp_dir().join(format!("undertake-runner-attempt-{label}-{nanos}"));
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

        fn candidate(profile_id: &str) -> ApprovedExecution {
            ApprovedExecution {
                profile_id: profile_id.to_string(),
                provider_id: "test-provider".to_string(),
                availability_key: format!("{profile_id}-avail"),
                execution_key: format!("{profile_id}-exec"),
            }
        }

        fn one_slot_stage(
            id: &str,
            candidates: Vec<ApprovedExecution>,
            attempt_budget: u8,
            target_kind: TargetKind,
            outcome_actions: BTreeMap<AttemptOutcomeCategory, AttemptAction>,
        ) -> Stage {
            Stage {
                id: StageId::new(id).expect("valid stage id"),
                slots: vec![Slot {
                    index: 0,
                    candidates,
                }],
                concurrency: NonZeroUsize::new(1).expect("nonzero"),
                target_kind,
                constraints: StageConstraints::unconstrained(),
                attempt_budget: StageAttemptLimit::new(attempt_budget).expect("nonzero"),
                outcome_actions,
            }
        }

        // ---- fakes ----------------------------------------------------------

        #[derive(Debug, Clone)]
        enum ScriptedAttempt {
            Success(&'static str),
            Failed(DispatchFailure, &'static str),
        }

        struct FakeAttemptExecutor {
            stdout_dir: PathBuf,
            scripts: Mutex<HashMap<String, VecDeque<ScriptedAttempt>>>,
            calls: Mutex<Vec<(String, String)>>,
            sequence: AtomicU64,
        }

        impl FakeAttemptExecutor {
            fn new(stdout_dir: PathBuf) -> Self {
                std::fs::create_dir_all(&stdout_dir).expect("mkdir stdout dir");
                Self {
                    stdout_dir,
                    scripts: Mutex::new(HashMap::new()),
                    calls: Mutex::new(Vec::new()),
                    sequence: AtomicU64::new(0),
                }
            }

            fn script(&self, profile_id: &str, attempts: Vec<ScriptedAttempt>) {
                self.scripts
                    .lock()
                    .expect("lock")
                    .insert(profile_id.to_string(), attempts.into_iter().collect());
            }

            fn calls(&self) -> Vec<(String, String)> {
                self.calls.lock().expect("lock").clone()
            }

            fn call_count(&self) -> usize {
                self.calls.lock().expect("lock").len()
            }
        }

        impl AttemptExecutor for FakeAttemptExecutor {
            fn execute(
                &self,
                _posture: MutationPosture,
                _stage: &Stage,
                candidate: &ApprovedExecution,
                prompt: &PromptMaterial,
            ) -> dispatch::Result<DispatchResult> {
                self.calls
                    .lock()
                    .expect("lock")
                    .push((candidate.profile_id.clone(), prompt.prompt.clone()));
                let next = self
                    .scripts
                    .lock()
                    .expect("lock")
                    .get_mut(&candidate.profile_id)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or_else(|| {
                        panic!(
                            "no scripted attempt left for candidate {}",
                            candidate.profile_id
                        )
                    });
                let n = self.sequence.fetch_add(1, Ordering::SeqCst);
                let stdout_path = self.stdout_dir.join(format!("stdout-{n:06}.txt"));
                let stderr_path = self.stdout_dir.join(format!("stderr-{n:06}.txt"));
                let (status, body) = match next {
                    ScriptedAttempt::Success(body) => (dispatch::DispatchStatus::Success, body),
                    ScriptedAttempt::Failed(failure, body) => {
                        (dispatch::DispatchStatus::Failed(failure), body)
                    }
                };
                std::fs::write(&stdout_path, body).expect("write fake stdout");
                std::fs::write(&stderr_path, b"").expect("write fake stderr");
                Ok(DispatchResult {
                    status,
                    worker_commit: None,
                    authentication_rejection: None,
                    stdout_bytes: body.len() as u64,
                    stderr_bytes: 0,
                    stdout_path,
                    stderr_path,
                })
            }
        }

        struct FakeExec {
            readiness: dispatch::AuthReadiness,
        }

        impl dispatch::Exec for FakeExec {
            fn spawn(
                &self,
                _request: &dispatch::SpawnRequest,
            ) -> dispatch::Result<Box<dyn dispatch::ChildProcess>> {
                unreachable!("AttemptRunner::run never calls Exec::spawn directly")
            }

            fn auth_readiness(&self, _backend: Backend) -> dispatch::AuthReadiness {
                self.readiness.clone()
            }
        }

        struct FakeCommitProbe {
            clean: bool,
        }

        impl dispatch::CommitProbe for FakeCommitProbe {
            fn head(&self, _repo: &Path) -> dispatch::Result<Option<String>> {
                Ok(Some("a".repeat(40)))
            }

            fn is_clean(&self, _repo: &Path) -> dispatch::Result<bool> {
                Ok(self.clean)
            }

            fn is_direct_child(
                &self,
                _repo: &Path,
                _before: Option<&str>,
                _commit: &str,
            ) -> dispatch::Result<bool> {
                Ok(true)
            }

            fn committer_email(
                &self,
                _repo: &Path,
                _commit: &str,
            ) -> dispatch::Result<Option<String>> {
                Ok(None)
            }
        }

        fn fake_issue(id: &str) -> Issue {
            Issue {
                id: id.to_string(),
                title: "test issue".to_string(),
                description: String::new(),
                acceptance_criteria: String::new(),
                notes: String::new(),
                status: "in_progress".to_string(),
                priority: 2,
                issue_type: "task".to_string(),
                assignee: Some("undertake".to_string()),
                owner: None,
                created_at: "2026-07-16T00:00:00Z".to_string(),
                created_by: "test".to_string(),
                updated_at: "2026-07-16T00:00:00Z".to_string(),
                started_at: None,
                labels: None,
                estimated_minutes: None,
                metadata: None,
                parent: None,
                dependencies: None,
                dependency_count: None,
                dependent_count: None,
                comment_count: None,
            }
        }

        #[derive(Default)]
        struct FakeBeadGateway {
            calls: Mutex<Vec<String>>,
        }

        impl FakeBeadGateway {
            fn calls(&self) -> Vec<String> {
                self.calls.lock().expect("lock").clone()
            }
        }

        impl BeadGateway for FakeBeadGateway {
            fn show(&self, _repo: &Path, id: &str) -> bd::Result<Issue> {
                self.calls.lock().expect("lock").push(format!("show:{id}"));
                Ok(fake_issue(id))
            }

            fn claim(&self, _repo: &Path, id: &str, owner: &str) -> bd::Result<Issue> {
                self.calls
                    .lock()
                    .expect("lock")
                    .push(format!("claim:{id}:{owner}"));
                Ok(fake_issue(id))
            }

            fn release(
                &self,
                _repo: &Path,
                id: &str,
                expected_assignee: &str,
            ) -> bd::Result<Issue> {
                self.calls
                    .lock()
                    .expect("lock")
                    .push(format!("release:{id}:{expected_assignee}"));
                Ok(fake_issue(id))
            }

            fn close(&self, _repo: &Path, id: &str, reason: &str) -> bd::Result<Issue> {
                self.calls
                    .lock()
                    .expect("lock")
                    .push(format!("close:{id}:{reason}"));
                Ok(fake_issue(id))
            }

            fn comment(&self, _repo: &Path, id: &str, text: &str) -> bd::Result<Comment> {
                self.calls
                    .lock()
                    .expect("lock")
                    .push(format!("comment:{id}:{text}"));
                Ok(Comment {
                    id: "c1".to_string(),
                    issue_id: id.to_string(),
                    text: text.to_string(),
                    author: "test".to_string(),
                    created_at: "2026-07-16T00:00:00Z".to_string(),
                    schema_version: None,
                })
            }
        }

        #[derive(Default)]
        struct FakeDigestSource {
            values: Mutex<BTreeMap<DigestKind, VecDeque<String>>>,
        }

        impl FakeDigestSource {
            fn script(&self, kind: DigestKind, values: &[&str]) {
                self.values.lock().expect("lock").insert(
                    kind,
                    values.iter().map(|value| (*value).to_string()).collect(),
                );
            }
        }

        impl DigestSource for FakeDigestSource {
            fn current(&self, kind: DigestKind) -> Result<String> {
                let mut values = self.values.lock().expect("lock");
                let queue = values
                    .get_mut(&kind)
                    .ok_or_else(|| RunnerError::new(format!("no scripted digest for {kind:?}")))?;
                if queue.len() > 1 {
                    Ok(queue.pop_front().expect("nonempty"))
                } else {
                    Ok(queue.front().cloned().expect("at least one scripted value"))
                }
            }
        }

        /// A `JobPolicy` whose entire behavior is data: a fixed stage
        /// sequence, a stage index after which `transition` returns
        /// `Terminal` instead of `Continue`, and a uniform prompt/aggregate/
        /// terminal shape reused by every test in this module.
        struct ScriptedPolicy {
            job: run::RunJob,
            stages: Vec<Stage>,
            claims_bead: bool,
            revalidation_digests: Vec<DigestKind>,
            terminal_transition_after: Option<usize>,
        }

        impl ScriptedPolicy {
            fn new(job: run::RunJob, stages: Vec<Stage>) -> Self {
                Self {
                    job,
                    stages,
                    claims_bead: false,
                    revalidation_digests: Vec::new(),
                    terminal_transition_after: None,
                }
            }
        }

        impl JobPolicy for ScriptedPolicy {
            fn job(&self) -> run::RunJob {
                self.job
            }

            fn posture(&self) -> MutationPosture {
                MutationPosture::ReadOnly
            }

            fn claims_bead(&self) -> bool {
                self.claims_bead
            }

            fn revalidation_digests(&self) -> &[DigestKind] {
                &self.revalidation_digests
            }

            fn next_stage(&self, ledger: &StageLedger) -> Option<Stage> {
                self.stages.get(ledger.completed_stages().count()).cloned()
            }

            fn prompt(&self, ctx: AttemptContext<'_>) -> PromptMaterial {
                // `ledger_artifacts` proves the ledger (whether built fresh
                // or reconstructed on resume) is what every later stage's
                // prompt actually reads from -- not just what `terminal`
                // sees at the very end.
                let ledger_artifacts: Vec<String> = ctx
                    .prior_stages
                    .completed_stages()
                    .flat_map(|stage| ctx.prior_stages.artifacts_for(stage))
                    .map(|artifact| artifact.sha256.clone())
                    .collect();
                PromptMaterial {
                    prompt: format!(
                        "stage={} slot={} attempt={} candidate={} prior={:?} \
                         ledger_stages={} ledger_artifacts={ledger_artifacts:?}",
                        ctx.stage.id.as_str(),
                        ctx.slot.index,
                        ctx.attempt_index,
                        ctx.candidate.profile_id,
                        ctx.prior_attempt_output
                            .map(|artifact| artifact.sha256.clone()),
                        ctx.prior_stages.completed_stages().count(),
                    ),
                    response_schema: None,
                }
            }

            fn classify_attempt(
                &self,
                _ctx: AttemptContext<'_>,
                _output: &AttemptOutput,
            ) -> Option<AttemptOutcome> {
                None
            }

            fn aggregate_stage(&self, stage: &Stage, slot_results: &[SlotResult]) -> StageOutcome {
                StageOutcome {
                    stage: stage.id.clone(),
                    outputs: slot_results
                        .iter()
                        .filter_map(|result| match &result.outcome {
                            SlotOutcome::Accepted(output) => Some(output.clone()),
                            SlotOutcome::Unaccepted => None,
                        })
                        .collect(),
                }
            }

            fn transition(&self, ledger: &StageLedger, stage_outcome: StageOutcome) -> Transition {
                let stage_index = ledger.completed_stages().count();
                if self.terminal_transition_after == Some(stage_index) {
                    Transition::Terminal(stage_outcome)
                } else {
                    Transition::Continue(stage_outcome)
                }
            }

            fn terminal(&self, ledger: &StageLedger) -> Terminal {
                let accepted = ledger
                    .completed_stages()
                    .last()
                    .and_then(|stage| ledger.outcome(stage))
                    .is_some_and(|outcome| !outcome.outputs.is_empty());
                if accepted {
                    Terminal::completed()
                } else {
                    Terminal::blocked("no accepted output")
                }
            }
        }

        fn new_run_request(
            repo: &str,
            bead: Option<&str>,
            max_attempts: Option<u64>,
        ) -> run::NewRun {
            run::NewRun {
                target: run::RunTarget {
                    repo: repo.to_string(),
                    bead: bead.map(str::to_string),
                },
                approved_profiles: vec!["worker-1".to_string(), "worker-2".to_string()],
                musterroll_roster_artifact: None,
                roster_snapshot: None,
                limits: run::RunLimits {
                    item_wall_clock_mins: Some(45),
                    max_attempts,
                },
                verifier: run::RunVerifier::default(),
                work: None,
                approval: None,
            }
        }

        fn create_run(
            temp: &TempDir,
            repo: &str,
            bead: Option<&str>,
            max_attempts: Option<u64>,
        ) -> run::RunHandle {
            run::RunHandle::create(
                temp.path(),
                run::RunJob::Work,
                new_run_request(repo, bead, max_attempts),
            )
            .expect("create run")
        }

        fn attempt_started_events(handle: &run::RunHandle) -> Vec<run::RunEvent> {
            run::read_events(&handle.events_path())
                .expect("read events")
                .into_iter()
                .filter(|event| event.kind == run::EventKind::AttemptStarted)
                .collect()
        }

        // ---- tests ------------------------------------------------------------

        #[test]
        fn single_slot_happy_path_completes_and_closes_the_bead() {
            let temp = TempDir::new("happy-path");
            let mut handle = create_run(&temp, "/nonexistent/repo-a", Some("bead-1"), Some(5));

            let stage = one_slot_stage(
                "work",
                vec![candidate("worker-1")],
                1,
                TargetKind::GitWorkingTree,
                BTreeMap::new(),
            );
            let mut policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);
            policy.claims_bead = true;

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            executor.script("worker-1", vec![ScriptedAttempt::Success("plan output")]);
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal =
                AttemptRunner::run(&policy, &ports, &mut handle, &request).expect("run completes");
            assert_eq!(terminal.verdict, TerminalVerdict::Completed);
            assert_eq!(executor.call_count(), 1);
            assert_eq!(
                bd.calls(),
                vec![
                    "claim:bead-1:undertake".to_string(),
                    "close:bead-1:completed".to_string(),
                ]
            );
            assert_eq!(handle.manifest().lifecycle, run::RunLifecycle::Finished);

            let events = run::read_events(&handle.events_path()).expect("read events");
            let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
            assert_eq!(
                kinds,
                vec![
                    run::EventKind::RunStarted,
                    run::EventKind::CoverageGap,
                    run::EventKind::AttemptStarted,
                    run::EventKind::AttemptFinished,
                    // Durable stage-boundary evidence (`conductor-v37z`),
                    // written before the ledger is used to pick the next
                    // stage -- here, immediately before the single stage's
                    // `Transition::Continue` ends the run with no further
                    // stage to run.
                    run::EventKind::StageFinished,
                    run::EventKind::RunFinished,
                ]
            );
        }

        #[test]
        fn dirty_working_tree_refuses_before_claiming_the_bead() {
            let temp = TempDir::new("dirty-tree");
            let mut handle = create_run(&temp, "/nonexistent/repo-b", Some("bead-2"), Some(5));

            let stage = one_slot_stage(
                "work",
                vec![candidate("worker-1")],
                1,
                TargetKind::GitWorkingTree,
                BTreeMap::new(),
            );
            let mut policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);
            policy.claims_bead = true;

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: false };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let error = AttemptRunner::run(&policy, &ports, &mut handle, &request)
                .expect_err("dirty tree must refuse");
            assert!(error.to_string().contains("dirty"), "{error}");
            assert!(bd.calls().is_empty(), "bead must never be claimed");
            assert_eq!(executor.call_count(), 0);
        }

        #[test]
        fn stage_declared_advance_candidate_walks_the_slot_chain() {
            let temp = TempDir::new("advance-candidate");
            let mut handle = create_run(&temp, "/artifact/review-target", None, Some(5));

            let mut outcome_actions = BTreeMap::new();
            outcome_actions.insert(
                AttemptOutcomeCategory::ProcessFailure,
                AttemptAction::AdvanceCandidate,
            );
            let stage = one_slot_stage(
                "review",
                vec![candidate("worker-1"), candidate("worker-2")],
                1,
                TargetKind::ArtifactOnly,
                outcome_actions,
            );
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            executor.script(
                "worker-1",
                vec![ScriptedAttempt::Failed(
                    DispatchFailure::ExitNonZero { code: Some(1) },
                    "boom",
                )],
            );
            executor.script("worker-2", vec![ScriptedAttempt::Success("recovered")]);
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal =
                AttemptRunner::run(&policy, &ports, &mut handle, &request).expect("run completes");
            assert_eq!(terminal.verdict, TerminalVerdict::Completed);
            let calls = executor.calls();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].0, "worker-1");
            assert_eq!(calls[1].0, "worker-2");

            let started = attempt_started_events(&handle);
            assert_eq!(started.len(), 2);
            assert_eq!(
                started[1]
                    .invocation
                    .as_ref()
                    .expect("invocation")
                    .execution
                    .profile_id,
                "worker-2"
            );
        }

        #[test]
        fn retry_same_candidate_passes_the_failed_attempts_output_into_the_next_prompt() {
            let temp = TempDir::new("retry-same-candidate");
            let mut handle = create_run(&temp, "/artifact/review-target", None, Some(5));

            let mut outcome_actions = BTreeMap::new();
            outcome_actions.insert(
                AttemptOutcomeCategory::ProcessFailure,
                AttemptAction::RetrySameCandidate,
            );
            let stage = one_slot_stage(
                "review",
                vec![candidate("worker-1")],
                2,
                TargetKind::ArtifactOnly,
                outcome_actions,
            );
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            executor.script(
                "worker-1",
                vec![
                    ScriptedAttempt::Failed(
                        DispatchFailure::ExitNonZero { code: Some(1) },
                        "first-output",
                    ),
                    ScriptedAttempt::Success("second-output"),
                ],
            );
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal =
                AttemptRunner::run(&policy, &ports, &mut handle, &request).expect("run completes");
            assert_eq!(terminal.verdict, TerminalVerdict::Completed);

            let calls = executor.calls();
            assert_eq!(calls.len(), 2);
            assert!(
                calls[0].1.contains("prior=None"),
                "first attempt must carry no prior output: {}",
                calls[0].1
            );
            assert!(
                !calls[1].1.contains("prior=None"),
                "retry attempt must carry the failed attempt's output: {}",
                calls[1].1
            );

            let started = attempt_started_events(&handle);
            assert_eq!(started.len(), 2);
            let first_id = started[0].event_id.clone();
            assert_eq!(
                started[1].invocation.as_ref().expect("invocation").retry_of,
                Some(first_id),
                "the retry's InvocationEvidence must link back to the first attempt's event id"
            );
        }

        #[test]
        fn concurrent_multi_slot_fan_out_is_written_by_the_runner_in_deterministic_slot_order() {
            let temp = TempDir::new("concurrent-fan-out");
            let mut handle = create_run(&temp, "/artifact/review-target", None, Some(10));

            let stage = Stage {
                id: StageId::new("review").expect("valid"),
                slots: vec![
                    Slot {
                        index: 0,
                        candidates: vec![candidate("reviewer-0")],
                    },
                    Slot {
                        index: 1,
                        candidates: vec![candidate("reviewer-1")],
                    },
                    Slot {
                        index: 2,
                        candidates: vec![candidate("reviewer-2")],
                    },
                ],
                concurrency: NonZeroUsize::new(3).expect("nonzero"),
                target_kind: TargetKind::ArtifactOnly,
                constraints: StageConstraints::unconstrained(),
                attempt_budget: StageAttemptLimit::new(1).expect("nonzero"),
                outcome_actions: BTreeMap::new(),
            };
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            // Slot 0's candidate finishes last despite starting first, so a
            // pass that wrote events in completion order rather than slot
            // order would fail this test's ordering assertion below.
            executor.script("reviewer-0", vec![ScriptedAttempt::Success("r0")]);
            executor.script("reviewer-1", vec![ScriptedAttempt::Success("r1")]);
            executor.script("reviewer-2", vec![ScriptedAttempt::Success("r2")]);
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal =
                AttemptRunner::run(&policy, &ports, &mut handle, &request).expect("run completes");
            assert_eq!(terminal.verdict, TerminalVerdict::Completed);
            assert_eq!(executor.call_count(), 3);

            let started = attempt_started_events(&handle);
            assert_eq!(started.len(), 3);
            let slots: Vec<u32> = started
                .iter()
                .map(|event| event.invocation.as_ref().expect("invocation").slot)
                .collect();
            assert_eq!(
                slots,
                vec![0, 1, 2],
                "the runner must write every slot's events in deterministic slot order, \
                 never thread-completion order"
            );
            // Sequence numbers strictly increase with no interleaving between
            // slots -- proof events were written by one sole writer after
            // the join, not natively from each slot's own thread.
            let seqs: Vec<u64> = started.iter().map(|event| event.seq).collect();
            assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]));
        }

        #[test]
        fn run_wide_call_budget_exhaustion_fails_closed_without_spawning_past_the_ceiling() {
            let temp = TempDir::new("budget-exhaustion");
            // Ceiling of 1: the second candidate's reservation must fail
            // before its executor is ever invoked.
            let mut handle = create_run(&temp, "/artifact/review-target", None, Some(1));

            let mut outcome_actions = BTreeMap::new();
            outcome_actions.insert(
                AttemptOutcomeCategory::ProcessFailure,
                AttemptAction::AdvanceCandidate,
            );
            let stage = one_slot_stage(
                "review",
                vec![candidate("worker-1"), candidate("worker-2")],
                1,
                TargetKind::ArtifactOnly,
                outcome_actions,
            );
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            executor.script(
                "worker-1",
                vec![ScriptedAttempt::Failed(
                    DispatchFailure::ExitNonZero { code: Some(1) },
                    "boom",
                )],
            );
            // worker-2 is intentionally never scripted: if the runner spent
            // past its reconstructed ceiling and called it anyway, the fake
            // panics on an unscripted attempt rather than silently passing.
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal = AttemptRunner::run(&policy, &ports, &mut handle, &request)
                .expect("run reaches a terminal, not an error");
            assert_ne!(terminal.verdict, TerminalVerdict::Completed);
            assert_eq!(
                executor.call_count(),
                1,
                "the exhausted second candidate must never reach the executor"
            );
        }

        #[test]
        fn approval_digest_drift_mid_run_fails_closed_and_releases_the_bead() {
            let temp = TempDir::new("digest-drift");
            let mut handle = create_run(&temp, "/nonexistent/repo-c", Some("bead-3"), Some(5));

            let stage_a = one_slot_stage(
                "stage_a",
                vec![candidate("worker-1")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let stage_b = one_slot_stage(
                "stage_b",
                vec![candidate("worker-2")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let mut policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage_a, stage_b]);
            policy.claims_bead = true;
            policy.revalidation_digests = vec![DigestKind::TargetHead];

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            executor.script("worker-1", vec![ScriptedAttempt::Success("stage-a-output")]);
            // worker-2 is never scripted: drift must be caught before
            // stage_b's slots ever dispatch.
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            // Preflight check + stage_a's loop-top check both read "head-a"
            // (matches pinned); stage_b's loop-top check reads "head-b"
            // (drift).
            digests.script(DigestKind::TargetHead, &["head-a", "head-a", "head-b"]);
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let mut pinned_digests = BTreeMap::new();
            pinned_digests.insert(DigestKind::TargetHead, "head-a".to_string());
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests,
            };

            let terminal = AttemptRunner::run(&policy, &ports, &mut handle, &request)
                .expect("drift ends the run in a written terminal, not an error");
            assert_eq!(terminal.verdict, TerminalVerdict::Blocked);
            assert!(
                terminal
                    .reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("TargetHead"),
                "{:?}",
                terminal.reason
            );
            assert_eq!(
                executor.call_count(),
                1,
                "stage_b must never dispatch once drift is detected"
            );
            assert_eq!(
                bd.calls(),
                vec![
                    "claim:bead-3:undertake".to_string(),
                    "release:bead-3:undertake".to_string(),
                ],
                "a blocked (not completed) terminal must release, never close, the bead"
            );
        }

        #[test]
        fn transition_terminal_ends_the_run_without_visiting_a_later_stage() {
            let temp = TempDir::new("transition-terminal");
            let mut handle = create_run(&temp, "/artifact/review-target", None, Some(5));

            let stage_a = one_slot_stage(
                "stage_a",
                vec![candidate("worker-1")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let stage_b = one_slot_stage(
                "stage_b",
                vec![candidate("worker-2")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let mut policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage_a, stage_b]);
            policy.terminal_transition_after = Some(0);

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            executor.script("worker-1", vec![ScriptedAttempt::Success("stage-a-output")]);
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal =
                AttemptRunner::run(&policy, &ports, &mut handle, &request).expect("run completes");
            assert_eq!(terminal.verdict, TerminalVerdict::Completed);
            assert_eq!(
                executor.call_count(),
                1,
                "a Transition::Terminal after stage_a must never dispatch stage_b"
            );
        }

        #[test]
        fn missing_max_attempts_ceiling_is_a_fail_closed_refusal() {
            let temp = TempDir::new("missing-ceiling");
            let mut handle = create_run(&temp, "/artifact/review-target", None, None);

            let stage = one_slot_stage(
                "review",
                vec![candidate("worker-1")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let error = AttemptRunner::run(&policy, &ports, &mut handle, &request)
                .expect_err("a missing ceiling must refuse, not default to unlimited");
            assert!(error.to_string().contains("max_attempts"), "{error}");
            assert_eq!(executor.call_count(), 0);
        }

        #[test]
        fn resume_refuses_while_a_recorded_worker_slot_is_alive() {
            use std::os::unix::process::CommandExt as _;
            use std::process::{Command, Stdio};

            let temp = TempDir::new("resume-liveness");
            let mut request = new_run_request("/nonexistent/repo-d", Some("bead-4"), Some(5));

            let mut worker = Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("spawn a live worker to stand in for a recorded slot");
            let pgid = worker.id();

            request.work = Some(run::WorkState {
                cycle_id: "cycle-1".to_string(),
                authorization_sha256: "b".repeat(64),
                before_head: None,
                owner_pid: None,
                owner_pid_generation: None,
                worker_pgid: Some(pgid),
                worker_pgid_generation: crate::quarantine::process_generation(pgid),
                worker_slots: Vec::new(),
                worker_profile: None,
                worker_commit: None,
                mechanical: None,
                stage: run::WorkStage::Implementing,
                review_resume_budget_secs: None,
            });
            let mut handle = run::RunHandle::create(temp.path(), run::RunJob::Work, request)
                .expect("create run");

            let stage = one_slot_stage(
                "work",
                vec![candidate("worker-1")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);
            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let run_request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let error = AttemptRunner::run(&policy, &ports, &mut handle, &run_request)
                .expect_err("a live recorded slot must refuse resume");
            assert!(error.to_string().contains("alive"), "{error}");
            assert_eq!(executor.call_count(), 0);
            assert!(bd.calls().is_empty());

            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{pgid}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            worker.wait().expect("reap worker");
        }

        // ---- resume: reconstruct the StageLedger instead of restarting it (`conductor-v37z`) ----

        /// Seeds `handle`'s journal with the durable evidence a prior
        /// process would have written for one already-completed stage --
        /// one accepted attempt (`AttemptStarted`/`AttemptFinished`, both
        /// carrying `InvocationEvidence`) plus the `StageFinished` event
        /// [`reconstruct_stage_ledger`] replays. A single test process
        /// cannot literally run `AttemptRunner::run` twice against the same
        /// handle to produce this state -- `finalize` marks a run
        /// permanently `Finished`, and `append_event` refuses to append to
        /// one -- so seeding the journal directly is the same technique
        /// `resume_refuses_while_a_recorded_worker_slot_is_alive` above
        /// already uses to simulate "a prior process's state" for the
        /// worker-slot liveness gate.
        fn seed_completed_stage(
            temp: &TempDir,
            handle: &mut run::RunHandle,
            stage_id: &str,
            profile_id: &str,
            body: &str,
            transition: run::StageTransitionKind,
        ) -> ArtifactRef {
            let source = temp.path().join(format!("{stage_id}-{profile_id}-seed.out"));
            std::fs::write(&source, body).expect("write seed artifact source");
            let artifact = handle
                .capture_artifact(
                    &source,
                    Path::new(&format!("attempts/{stage_id}-{profile_id}-seed.out")),
                )
                .expect("capture seed artifact");
            let execution = candidate(profile_id);
            let input_sha256 = sha256_hex(b"seed-input");
            handle
                .append_event(
                    run::EventKind::AttemptStarted,
                    run::EventInput {
                        profile_id: Some(profile_id.to_string()),
                        invocation: Some(run::InvocationEvidence {
                            stage: stage_id.to_string(),
                            slot: 0,
                            attempt: 1,
                            execution: execution.clone(),
                            input_sha256: input_sha256.clone(),
                            output_sha256: None,
                            duration_ms: None,
                            tokens: None,
                            retry_of: None,
                        }),
                        ..run::EventInput::default()
                    },
                )
                .expect("seed attempt_started");
            handle
                .append_event(
                    run::EventKind::AttemptFinished,
                    run::EventInput {
                        profile_id: Some(profile_id.to_string()),
                        artifact_refs: vec![artifact.clone()],
                        outcome: Some("accepted".to_string()),
                        invocation: Some(run::InvocationEvidence {
                            stage: stage_id.to_string(),
                            slot: 0,
                            attempt: 1,
                            execution,
                            input_sha256,
                            output_sha256: Some(artifact.sha256.clone()),
                            duration_ms: Some(1),
                            tokens: None,
                            retry_of: None,
                        }),
                        ..run::EventInput::default()
                    },
                )
                .expect("seed attempt_finished");
            handle
                .append_event(
                    run::EventKind::StageFinished,
                    run::EventInput {
                        artifact_refs: vec![artifact.clone()],
                        stage_progress: Some(run::StageProgress {
                            stage: stage_id.to_string(),
                            transition,
                        }),
                        ..run::EventInput::default()
                    },
                )
                .expect("seed stage_finished");
            artifact
        }

        #[test]
        fn resume_after_a_completed_stage_continues_at_the_next_stage_without_re_executing_it() {
            let temp = TempDir::new("resume-continue");
            let mut handle = create_run(&temp, "/artifact/review-target", None, Some(5));
            let seeded = seed_completed_stage(
                &temp,
                &mut handle,
                "stage_a",
                "worker-1",
                "stage-a-output",
                run::StageTransitionKind::Continue,
            );

            let stage_a = one_slot_stage(
                "stage_a",
                vec![candidate("worker-1")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let stage_b = one_slot_stage(
                "stage_b",
                vec![candidate("worker-2")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage_a, stage_b]);

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            // worker-1 (stage_a's candidate) is deliberately never scripted:
            // if resume incorrectly replayed stage_a, the fake would panic
            // on an unscripted call instead of silently re-running it.
            executor.script("worker-2", vec![ScriptedAttempt::Success("stage-b-output")]);
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal =
                AttemptRunner::run(&policy, &ports, &mut handle, &request).expect("run completes");
            assert_eq!(terminal.verdict, TerminalVerdict::Completed);
            let calls = executor.calls();
            assert_eq!(calls.len(), 1, "stage_a must not be re-executed on resume");
            assert_eq!(calls[0].0, "worker-2");
            assert!(
                calls[0].1.contains("ledger_stages=1"),
                "stage_b's prompt must see stage_a as already completed via the ledger: {}",
                calls[0].1
            );
            assert!(
                calls[0].1.contains(seeded.sha256.as_str()),
                "stage_a's artifact must be visible to stage_b's prompt via the ledger: {}",
                calls[0].1
            );

            // Only stage_b's attempt is newly journaled; stage_a's seeded
            // events are untouched and not duplicated.
            let started = attempt_started_events(&handle);
            assert_eq!(
                started.len(),
                2,
                "seeded stage_a attempt + new stage_b attempt"
            );
            assert_eq!(
                started[1]
                    .invocation
                    .as_ref()
                    .expect("invocation")
                    .execution
                    .profile_id,
                "worker-2"
            );
        }

        #[test]
        fn resume_mid_stage_without_a_stage_finished_event_reruns_the_stage_from_attempt_one() {
            let temp = TempDir::new("resume-mid-stage");
            let mut handle = create_run(&temp, "/artifact/review-target", None, Some(2));

            // Simulate a crash: stage_a's one candidate has durable
            // attempt-level evidence from a prior process, but no
            // `StageFinished` event followed it -- the process crashed
            // before `aggregate_stage`/`transition` ever ran for stage_a.
            handle
                .append_event(
                    run::EventKind::AttemptStarted,
                    run::EventInput {
                        profile_id: Some("worker-1".to_string()),
                        invocation: Some(run::InvocationEvidence {
                            stage: "stage_a".to_string(),
                            slot: 0,
                            attempt: 1,
                            execution: candidate("worker-1"),
                            input_sha256: sha256_hex(b"crashed-input"),
                            output_sha256: None,
                            duration_ms: None,
                            tokens: None,
                            retry_of: None,
                        }),
                        ..run::EventInput::default()
                    },
                )
                .expect("seed crashed attempt_started");

            let stage = one_slot_stage(
                "stage_a",
                vec![candidate("worker-1")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            executor.script("worker-1", vec![ScriptedAttempt::Success("replay-output")]);
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal =
                AttemptRunner::run(&policy, &ports, &mut handle, &request).expect("run completes");
            assert_eq!(terminal.verdict, TerminalVerdict::Completed);
            assert_eq!(
                executor.call_count(),
                1,
                "the incomplete stage must be re-dispatched, not skipped"
            );
            let started = attempt_started_events(&handle);
            assert_eq!(
                started.len(),
                2,
                "the crashed attempt plus exactly one fresh replay attempt"
            );
            assert_eq!(
                started[1].invocation.as_ref().expect("invocation").attempt,
                1,
                "the replay must start the candidate over at attempt 1, not continue a phantom retry chain"
            );
        }

        #[test]
        fn resume_mid_stage_budget_reservation_carries_the_crashed_attempt_forward() {
            let temp = TempDir::new("resume-mid-stage-budget");
            // Ceiling of 1, already fully consumed by the pre-seeded
            // crashed attempt below: the replay's reservation must fail
            // closed before the (deliberately unscripted) executor is ever
            // called.
            let mut handle = create_run(&temp, "/artifact/review-target", None, Some(1));

            handle
                .append_event(
                    run::EventKind::AttemptStarted,
                    run::EventInput {
                        profile_id: Some("worker-1".to_string()),
                        invocation: Some(run::InvocationEvidence {
                            stage: "stage_a".to_string(),
                            slot: 0,
                            attempt: 1,
                            execution: candidate("worker-1"),
                            input_sha256: sha256_hex(b"crashed-input"),
                            output_sha256: None,
                            duration_ms: None,
                            tokens: None,
                            retry_of: None,
                        }),
                        ..run::EventInput::default()
                    },
                )
                .expect("seed crashed attempt_started");

            let stage = one_slot_stage(
                "stage_a",
                vec![candidate("worker-1")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);

            // worker-1 is intentionally never scripted: if reserve-never-refund
            // did not carry the crashed attempt's cost forward, the replay's
            // reservation would wrongly succeed and the fake would panic on
            // an unscripted call.
            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let terminal = AttemptRunner::run(&policy, &ports, &mut handle, &request)
                .expect("run reaches a terminal, not an error");
            assert_ne!(terminal.verdict, TerminalVerdict::Completed);
            assert_eq!(
                executor.call_count(),
                0,
                "the crashed attempt's reservation must count against the ceiling on resume"
            );
        }

        #[test]
        fn resume_with_an_uninterpretable_stage_finished_event_fails_closed_without_guessing() {
            let temp = TempDir::new("resume-uninterpretable");
            let mut handle = create_run(&temp, "/nonexistent/repo-e", Some("bead-5"), Some(5));

            // A `stage_finished` event with no `stage_progress` is legal to
            // write (mirrors `run_finished` without a `terminal_verdict`)
            // but carries none of the payload reconstruction depends on --
            // exactly the "uninterpretable record" the contract requires
            // reconstruction to refuse rather than guess past.
            handle
                .append_event(run::EventKind::StageFinished, run::EventInput::default())
                .expect("write an uninterpretable stage_finished event");

            let stage = one_slot_stage(
                "stage_a",
                vec![candidate("worker-1")],
                1,
                TargetKind::ArtifactOnly,
                BTreeMap::new(),
            );
            let mut policy = ScriptedPolicy::new(run::RunJob::Work, vec![stage]);
            policy.claims_bead = true;

            let executor = FakeAttemptExecutor::new(temp.path().join("stdout"));
            let exec = FakeExec {
                readiness: dispatch::AuthReadiness::Ready,
            };
            let commits = FakeCommitProbe { clean: true };
            let bd = FakeBeadGateway::default();
            let digests = FakeDigestSource::default();
            let ports = RunnerPorts {
                exec: &exec,
                commits: &commits,
                bd: &bd,
                executor: &executor,
                clock: &SystemClock,
                digests: &digests,
            };
            let request = RunRequest {
                state_dir: temp.path().join("state"),
                backend: Backend::Claude,
                owner: "undertake".to_string(),
                pinned_digests: BTreeMap::new(),
            };

            let error = AttemptRunner::run(&policy, &ports, &mut handle, &request)
                .expect_err("an uninterpretable stage_finished record must refuse, not guess");
            assert!(error.to_string().contains("stage_progress"), "{error}");
            assert_eq!(executor.call_count(), 0);
            assert!(
                bd.calls().is_empty(),
                "bead must never be claimed on a fail-closed refusal"
            );
        }

        #[test]
        fn reconstruction_over_the_v2_fixture_journal_yields_an_empty_ledger_without_erroring() {
            // The @2 fixture predates `StageFinished` entirely, so
            // reconstruction must open and read it cleanly -- exactly
            // today's (pre-fix) always-empty ledger, not an error. This is
            // the resume path's own round-trip over the same fixture
            // `run::tests::v2_fixture_journal_opens_reads_and_resumes_under_the_new_binary`
            // already proves `read_events` handles.
            const CAPTURED_EVENTS_V2: &str = include_str!("../tests/fixtures/run-events-v2.jsonl");
            // `read_events` fails closed if a referenced local artifact is
            // missing (`run::validate_local_artifact`); the fixture's first
            // event pins `approval.json` by path and sha256, so the fixture
            // directory needs it too, exactly like
            // `run::tests::v2_fixture_journal_opens_reads_and_resumes_under_the_new_binary`.
            const CAPTURED_APPROVAL_V2: &str = include_str!("../tests/fixtures/run-approval-v2.json");

            let temp = TempDir::new("v2-fixture-reconstruction");
            let events_path = temp.path().join("events.jsonl");
            std::fs::write(&events_path, CAPTURED_EVENTS_V2).expect("write v2 fixture events");
            std::fs::write(temp.path().join("approval.json"), CAPTURED_APPROVAL_V2)
                .expect("write v2 fixture approval");
            let events = run::read_events(&events_path).expect("read v2 fixture events");
            assert!(
                !events.is_empty(),
                "the v2 fixture must contain at least one event to be a meaningful round trip"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event.kind != run::EventKind::StageFinished),
                "the v2 fixture predates StageFinished by construction"
            );

            let handle = create_run(&temp, "/artifact/review-target", None, Some(5));
            let (ledger, resumed_terminal) = reconstruct_stage_ledger(&handle, &events)
                .expect("the v2 journal must reconstruct cleanly, not fail closed");
            assert_eq!(ledger.completed_stages().count(), 0);
            assert!(!resumed_terminal);
        }
    }
}
