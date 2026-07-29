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
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::bd::{self, Comment, Issue};
use crate::dispatch::{self, CommitAuthenticationRejection, DispatchFailure, DispatchResult};
use crate::job::MutationPosture;
use crate::musterroll::RuntimeLimitEvidence;
use crate::run::{
    self, ApprovedExecution, ArtifactRef, PlanProviderDiversity, StageAttemptLimit, TerminalVerdict,
};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// Injectable wall-clock source. `dispatch_cycle::ItemDeadline`
/// (`dispatch_cycle.rs:90-112`) reads `Instant::now()` directly; this
/// generalizes it behind a trait so [`ItemDeadline::remaining_at`] stays
/// deterministically testable without sleeping.
pub(crate) trait Clock {
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
pub(crate) trait AttemptExecutor {
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
pub(crate) trait JobPolicy {
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
}
