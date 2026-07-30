//! The `work` job policy and production executor — the first job migrated
//! onto [`crate::runner::AttemptRunner`] (bead `conductor-vd3y`).
//!
//! [`WorkPolicy`] is pure per the runner contract
//! (`.docs/ai/phases/undertake-runner-contract.md`): it only renders prompts
//! and computes ledger/terminal transitions, and leaves attempt
//! classification to the runner's default (`JobPolicy::classify_attempt`
//! always returns `None` here — the runner's own `dispatch_default_outcome`
//! reading of a [`dispatch::DispatchResult`] already matches this policy's
//! declared outcome-to-action mapping, so no policy-level override is
//! needed). [`ProductionAttemptExecutor`] is where the impure work happens:
//! resolving a candidate's dispatch facts, spawning the worker or the
//! mechanical verifier, and quarantining a failed attempt's uncommitted
//! changes before the next candidate runs.
//!
//! `work` runs as two runner stages: `"work"` (the LLM worker chain, one
//! slot, candidates = the job binding's pinned pool plus fallbacks in
//! order) and `"verify"` (the bead's mechanical `verify_cmd`, one
//! synthetic candidate, run directly as a shell command rather than an LLM
//! dispatch). `"verify"` only runs when `"work"` produced an accepted
//! commit; a rejected/exhausted `"work"` stage ends the run without ever
//! reaching `"verify"`.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::bd::Issue;
use crate::config::{Backend, ReasoningEffort};
use crate::dispatch::{
    self, CommitProbe, DispatchRequest, DispatchResult, DispatchStatus, Exec, SpawnRequest,
    StdinMode, WorkerResourceLimits,
};
use crate::job::{JobBinding, MutationPosture};
use crate::musterroll::RosterSnapshot;
use crate::quarantine::RepoRecovery;
use crate::run::{self, ApprovedExecution, ArtifactRef};
use crate::runner::{
    AttemptAction, AttemptContext, AttemptExecutor, AttemptOutcome, AttemptOutcomeCategory,
    AttemptOutput, DigestKind, DigestSource, JobPolicy, PromptMaterial, Slot, SlotOutcome,
    SlotResult, Stage, StageConstraints, StageId, StageLedger, StageOutcome, TargetKind, Terminal,
    Transition,
};

pub(crate) const WORK_STAGE: &str = "work";
pub(crate) const VERIFY_STAGE: &str = "verify";

/// Dispatch facts a [`run::ApprovedExecution`]'s opaque `profile_id` resolves
/// to against a live Musterroll snapshot — everything [`DispatchRequest`]
/// needs beyond identity. `ApprovedExecution` deliberately carries no
/// dispatch mechanics of its own (it is a pinned identity, per
/// `run.rs`'s own doc comment), so the executor keeps this side table.
#[derive(Debug, Clone)]
pub(crate) struct DispatchFacts {
    pub(crate) backend: Backend,
    pub(crate) dispatch_id: String,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

/// Resolves a `work` [`JobBinding`]'s pinned pool (primary then fallbacks,
/// in order) against a live [`RosterSnapshot`], skipping any profile whose
/// profile or provider is not currently `enabled && eligible`. Fails
/// closed if a *pinned* profile id is absent from the snapshot entirely
/// (mirrors [`crate::job::JobRegistry::validate_pinned_profiles`]'s
/// stricter presence check, run separately by the caller before this).
pub(crate) fn resolve_candidates(
    binding: &JobBinding,
    snapshot: &RosterSnapshot,
) -> std::result::Result<(Vec<ApprovedExecution>, BTreeMap<String, DispatchFacts>), String> {
    let mut candidates = Vec::new();
    let mut facts = BTreeMap::new();
    for profile_id in binding.pinned_profile_ids() {
        let Some(profile) = snapshot
            .profiles
            .iter()
            .find(|p| p.profile_id == profile_id)
        else {
            return Err(format!(
                "pinned profile {profile_id} is absent from the Musterroll roster snapshot"
            ));
        };
        if !(profile.enabled && profile.eligible) {
            continue;
        }
        let Some(provider) = snapshot
            .providers
            .iter()
            .find(|p| p.provider_id == profile.provider_id)
        else {
            return Err(format!(
                "profile {profile_id} references unknown provider {}",
                profile.provider_id
            ));
        };
        if !(provider.enabled && provider.eligible) {
            continue;
        }
        let backend = crate::musterroll::backend_from_harness(&profile.harness)
            .map_err(|error| error.to_string())?;
        let reasoning_effort = profile
            .reasoning_effort
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|error: crate::config::ConfigError| error.to_string())?;
        candidates.push(crate::role_routing::approved_execution(profile, provider));
        facts.insert(
            profile_id.to_string(),
            DispatchFacts {
                backend,
                dispatch_id: profile.dispatch_id.clone(),
                reasoning_effort,
            },
        );
    }
    Ok((candidates, facts))
}

fn verify_candidate() -> ApprovedExecution {
    ApprovedExecution {
        profile_id: "mechanical-verify".to_string(),
        provider_id: "local".to_string(),
        availability_key: "local".to_string(),
        execution_key: "mechanical-verify".to_string(),
    }
}

/// The pure `work` [`JobPolicy`]. Holds a pre-fetched [`Issue`] snapshot
/// (fetched by the caller via `BeadGateway::show` before the run starts,
/// since the runner itself never calls `show` and a policy may never touch
/// bd) so prompt rendering never needs live bd access.
pub(crate) struct WorkPolicy {
    issue: Issue,
    verify_cmd: String,
    repo: PathBuf,
    candidates: Vec<ApprovedExecution>,
    /// The `work` [`JobBinding`]'s `limits.max_attempts`, read into the
    /// `"work"` stage's per-candidate [`Stage::attempt_budget`] (contract:
    /// "the same-candidate retry budget the binding allows"). No outcome
    /// category this policy declares currently maps to `RetrySameCandidate`
    /// (decision 5: work's failures all map to `AdvanceCandidate` or
    /// `Fatal`), so a value above 1 has no behavioral effect yet — it is
    /// still read and wired rather than left dead, ready for a future
    /// retry-mapping change.
    attempt_budget: run::StageAttemptLimit,
}

impl WorkPolicy {
    pub(crate) fn new(
        issue: Issue,
        verify_cmd: String,
        repo: PathBuf,
        candidates: Vec<ApprovedExecution>,
        attempt_budget: run::StageAttemptLimit,
    ) -> Self {
        Self {
            issue,
            verify_cmd,
            repo,
            candidates,
            attempt_budget,
        }
    }

    fn work_stage(&self) -> Stage {
        let mut outcome_actions = BTreeMap::new();
        outcome_actions.insert(
            AttemptOutcomeCategory::ProcessFailure,
            AttemptAction::AdvanceCandidate,
        );
        outcome_actions.insert(
            AttemptOutcomeCategory::CommitAuthenticationFailure,
            AttemptAction::AdvanceCandidate,
        );
        outcome_actions.insert(
            AttemptOutcomeCategory::RuntimeLimit,
            AttemptAction::AdvanceCandidate,
        );
        outcome_actions.insert(
            AttemptOutcomeCategory::BudgetExhausted,
            AttemptAction::Fatal,
        );
        Stage {
            id: StageId::new(WORK_STAGE).expect("WORK_STAGE is valid snake_case"),
            slots: vec![Slot {
                index: 0,
                candidates: self.candidates.clone(),
            }],
            concurrency: NonZeroUsize::new(1).expect("nonzero"),
            target_kind: TargetKind::GitWorkingTree,
            constraints: StageConstraints::unconstrained(),
            attempt_budget: self.attempt_budget,
            outcome_actions,
        }
    }

    fn verify_stage() -> Stage {
        let mut outcome_actions = BTreeMap::new();
        outcome_actions.insert(AttemptOutcomeCategory::ProcessFailure, AttemptAction::Fatal);
        outcome_actions.insert(
            AttemptOutcomeCategory::CommitAuthenticationFailure,
            AttemptAction::Fatal,
        );
        outcome_actions.insert(AttemptOutcomeCategory::RuntimeLimit, AttemptAction::Fatal);
        outcome_actions.insert(
            AttemptOutcomeCategory::BudgetExhausted,
            AttemptAction::Fatal,
        );
        Stage {
            id: StageId::new(VERIFY_STAGE).expect("VERIFY_STAGE is valid snake_case"),
            slots: vec![Slot {
                index: 0,
                candidates: vec![verify_candidate()],
            }],
            concurrency: NonZeroUsize::new(1).expect("nonzero"),
            target_kind: TargetKind::GitWorkingTree,
            constraints: StageConstraints::unconstrained(),
            attempt_budget: run::StageAttemptLimit::new(1).expect("nonzero"),
            outcome_actions,
        }
    }

    /// The stage plan this policy will run, in order — used by the caller
    /// to size the run-wide call budget ([`crate::runner::CallBudget::worst_case`])
    /// before the run is created, since [`crate::runner::AttemptRunner::run`]
    /// reads that ceiling from the manifest rather than calling
    /// `JobPolicy::call_budget` itself.
    pub(crate) fn stage_plan(&self) -> Vec<Stage> {
        vec![self.work_stage(), Self::verify_stage()]
    }

    fn work_outcome_ok(ledger: &StageLedger) -> bool {
        let work_stage = StageId::new(WORK_STAGE).expect("valid");
        ledger
            .outcome(&work_stage)
            .is_some_and(|outcome| !outcome.outputs.is_empty())
    }

    fn verify_outcome_ok(ledger: &StageLedger) -> bool {
        let verify_stage = StageId::new(VERIFY_STAGE).expect("valid");
        ledger
            .outcome(&verify_stage)
            .is_some_and(|outcome| !outcome.outputs.is_empty())
    }
}

impl JobPolicy for WorkPolicy {
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
        // Deliberately empty, correcting the contract's draft table (which
        // named `target_head`, `bead status/claim ownership`, and
        // `roster_policy_sha256`) against how the generic per-stage-boundary
        // mechanism actually behaves for a *mutating* job:
        //
        // - `target_head`: `revalidate_digests` runs at every stage
        //   boundary against one fixed pinned value supplied before the run
        //   starts. `work`'s own successful worker commit legitimately
        //   advances HEAD between the `"work"` and `"verify"` stages, and
        //   the mechanism has no way to express "matches the original
        //   baseline OR our own attempt's accepted commit" — it would flag
        //   the run's own intended progress as drift on every successful
        //   run. D1's actual HEAD-drift detection is already covered by
        //   narrower, correct mechanisms: `AttemptRunner::run`'s one-time
        //   `is_clean` preflight, `dispatch::run_with_heartbeat`'s
        //   per-attempt pre-spawn HEAD check against
        //   `DispatchRequest::before_head`
        //   (`ProductionAttemptExecutor::execute_work`), and
        //   `quarantine_if_dirty`'s own pre/post HEAD check around a
        //   restore. Wiring the same check twice through the generic digest
        //   path would only reintroduce the false-positive above.
        // - `bead status/claim ownership`: the bead is claimed by the
        //   runner itself, after the first (pre-claim) revalidation and
        //   before the loop's first (post-claim) one — no single pinned
        //   value can pass both without a runner-side ordering change this
        //   pass does not make.
        // - `roster_policy_sha256`: no live Musterroll roster snapshot is
        //   pinned into this run's manifest by this pass.
        &[]
    }

    fn next_stage(&self, ledger: &StageLedger) -> Option<Stage> {
        match ledger.completed_stages().count() {
            0 => Some(self.work_stage()),
            1 if Self::work_outcome_ok(ledger) => Some(Self::verify_stage()),
            _ => None,
        }
    }

    fn prompt(&self, ctx: AttemptContext<'_>) -> PromptMaterial {
        if ctx.stage.id.as_str() == VERIFY_STAGE {
            PromptMaterial {
                prompt: self.verify_cmd.clone(),
                response_schema: None,
            }
        } else {
            PromptMaterial {
                prompt: crate::dispatch_cycle::render_worker_prompt(
                    &self.issue,
                    &self.repo,
                    &self.verify_cmd,
                ),
                response_schema: None,
            }
        }
    }

    fn classify_attempt(
        &self,
        _ctx: AttemptContext<'_>,
        _output: &AttemptOutput,
    ) -> Option<AttemptOutcome> {
        // The runner's default reading of a `DispatchResult` already matches
        // this policy's declared outcome-to-action mapping for both stages
        // (see `work_stage`/`verify_stage`), so no override is needed.
        None
    }

    fn aggregate_stage(&self, stage: &Stage, slot_results: &[SlotResult]) -> StageOutcome {
        let outputs = slot_results
            .first()
            .map(|result| match &result.outcome {
                SlotOutcome::Accepted(output) => vec![output.clone()],
                SlotOutcome::Unaccepted => Vec::new(),
            })
            .unwrap_or_default();
        StageOutcome {
            stage: stage.id.clone(),
            outputs,
        }
    }

    fn transition(&self, _ledger: &StageLedger, stage_outcome: StageOutcome) -> Transition {
        if stage_outcome.stage.as_str() == WORK_STAGE && !stage_outcome.outputs.is_empty() {
            Transition::Continue(stage_outcome)
        } else {
            Transition::Terminal(stage_outcome)
        }
    }

    fn terminal(&self, ledger: &StageLedger) -> Terminal {
        if !Self::work_outcome_ok(ledger) {
            return if self.candidates.is_empty() {
                Terminal::blocked("no eligible profile in the work job's pinned pool")
            } else {
                Terminal {
                    verdict: run::TerminalVerdict::Failed,
                    reason: Some("worker chain exhausted without an accepted commit".to_string()),
                }
            };
        }
        if Self::verify_outcome_ok(ledger) {
            Terminal::completed()
        } else {
            Terminal {
                verdict: run::TerminalVerdict::Failed,
                reason: Some(format!("verify_cmd failed: {}", self.verify_cmd)),
            }
        }
    }
}

/// Digest source backing [`WorkPolicy::revalidation_digests`]'s single
/// declared digest.
pub(crate) struct HeadDigestSource<'a, C: CommitProbe> {
    commits: &'a C,
    repo: PathBuf,
}

impl<'a, C: CommitProbe> HeadDigestSource<'a, C> {
    pub(crate) fn new(commits: &'a C, repo: PathBuf) -> Self {
        Self { commits, repo }
    }
}

impl<C: CommitProbe> DigestSource for HeadDigestSource<'_, C> {
    fn current(&self, kind: DigestKind) -> crate::runner::Result<String> {
        match kind {
            DigestKind::TargetHead => self
                .commits
                .head(&self.repo)
                .map(Option::unwrap_or_default)
                .map_err(|error| crate::runner::RunnerError::new(error.to_string())),
            other => Err(crate::runner::RunnerError::new(format!(
                "WorkPolicy does not declare revalidation digest {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct QuarantineNote {
    path: String,
    sha256: String,
    changed_paths: usize,
}

fn append_quarantine_note(prompt: &str, note: Option<&QuarantineNote>) -> String {
    let Some(note) = note else {
        return prompt.to_string();
    };
    format!(
        "{prompt}\n\n---\nA previous attempt on this item left uncommitted changes; they were \
         captured and the working tree was restored to a clean state before this attempt. \
         Reference only (not applied): {} (sha256 {}, {} path(s) touched).\n",
        note.path, note.sha256, note.changed_paths,
    )
}

fn write_quarantine_patch(
    run_dir: &Path,
    label: &str,
    patch: &[u8],
) -> std::result::Result<ArtifactRef, String> {
    let sha256 = format!("{:x}", Sha256::digest(patch));
    let relative = format!("artifacts/{label}.patch");
    let destination = run_dir.join(&relative);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
    }
    std::fs::write(&destination, patch)
        .map_err(|error| format!("write {}: {error}", destination.display()))?;
    Ok(ArtifactRef {
        path: relative,
        sha256,
    })
}

/// Production [`AttemptExecutor`] for `work`. Generic over the concrete
/// production port types (never `dyn`): [`AttemptExecutor: Sync`] requires
/// every field to be `Sync`, and a bare `&dyn Exec`/`&dyn CommitProbe` is
/// not `Sync` unless the trait itself names `Sync` as a supertrait (neither
/// does) — the existing production dispatch helpers
/// (`dispatch::run_with_heartbeat`, `dispatch_cycle::run_worker_chain`) are
/// generic for the same reason, so this mirrors that convention rather than
/// widening either trait.
pub(crate) struct ProductionAttemptExecutor<
    'a,
    E: Exec + Sync,
    C: CommitProbe + Sync,
    R: RepoRecovery + Sync,
> {
    exec: &'a E,
    commits: &'a C,
    recovery: &'a R,
    repo: PathBuf,
    run_id: String,
    bead_id: String,
    state_dir: PathBuf,
    run_dir: PathBuf,
    before_head: Option<String>,
    dispatch_facts: BTreeMap<String, DispatchFacts>,
    worker_resource_limits: WorkerResourceLimits,
    timeout: Duration,
    heartbeat_interval: Duration,
    sequence: AtomicU64,
    quarantine_sequence: AtomicU64,
    last_quarantine: Mutex<Option<QuarantineNote>>,
}

impl<'a, E: Exec + Sync, C: CommitProbe + Sync, R: RepoRecovery + Sync>
    ProductionAttemptExecutor<'a, E, C, R>
{
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the ports a production worker dispatch genuinely needs"
    )]
    pub(crate) fn new(
        exec: &'a E,
        commits: &'a C,
        recovery: &'a R,
        repo: PathBuf,
        run_id: String,
        bead_id: String,
        state_dir: PathBuf,
        run_dir: PathBuf,
        before_head: Option<String>,
        dispatch_facts: BTreeMap<String, DispatchFacts>,
        worker_resource_limits: WorkerResourceLimits,
        timeout: Duration,
        heartbeat_interval: Duration,
    ) -> Self {
        Self {
            exec,
            commits,
            recovery,
            repo,
            run_id,
            bead_id,
            state_dir,
            run_dir,
            before_head,
            dispatch_facts,
            worker_resource_limits,
            timeout,
            heartbeat_interval,
            sequence: AtomicU64::new(0),
            quarantine_sequence: AtomicU64::new(0),
            last_quarantine: Mutex::new(None),
        }
    }

    fn quarantine_if_dirty(&self) -> dispatch::Result<()> {
        let clean = self.commits.is_clean(&self.repo)?;
        if clean {
            return Ok(());
        }
        let current_head = self.commits.head(&self.repo)?;
        if current_head.as_deref() != self.before_head.as_deref() {
            return Err(dispatch::DispatchError::new(format!(
                "refusing to quarantine: repository HEAD moved from {:?} to {current_head:?} \
                 during a failed attempt",
                self.before_head,
            )));
        }
        let changed_paths = self
            .recovery
            .changed_paths(&self.repo)
            .map_err(dispatch::DispatchError::new)?;
        let patch = self
            .recovery
            .capture_patch(&self.repo)
            .map_err(dispatch::DispatchError::new)?;
        if patch.is_empty() {
            return Err(dispatch::DispatchError::new(
                "repository is dirty but no patch content could be captured",
            ));
        }
        let n = self.quarantine_sequence.fetch_add(1, Ordering::SeqCst);
        let artifact = write_quarantine_patch(&self.run_dir, &format!("quarantine-{n:06}"), &patch)
            .map_err(dispatch::DispatchError::new)?;
        self.recovery
            .restore_clean(&self.repo)
            .map_err(dispatch::DispatchError::new)?;
        let post_head = self.commits.head(&self.repo)?;
        let post_clean = self.commits.is_clean(&self.repo)?;
        if post_head.as_deref() != self.before_head.as_deref() || !post_clean {
            return Err(dispatch::DispatchError::new(format!(
                "quarantine restore left the tree at head={post_head:?} clean={post_clean}, \
                 expected head={:?} clean=true",
                self.before_head,
            )));
        }
        let mut note = self.last_quarantine.lock().expect("quarantine note lock");
        *note = Some(QuarantineNote {
            path: artifact.path,
            sha256: artifact.sha256,
            changed_paths: changed_paths.len(),
        });
        Ok(())
    }

    fn execute_work(
        &self,
        candidate: &ApprovedExecution,
        prompt: &PromptMaterial,
    ) -> dispatch::Result<DispatchResult> {
        let facts = self
            .dispatch_facts
            .get(&candidate.profile_id)
            .ok_or_else(|| {
                dispatch::DispatchError::new(format!(
                    "no dispatch facts resolved for candidate {}",
                    candidate.profile_id
                ))
            })?;
        let attempt_id = format!("work-{:06}", self.sequence.fetch_add(1, Ordering::SeqCst));
        let prompt_text = {
            let note = self.last_quarantine.lock().expect("quarantine note lock");
            append_quarantine_note(&prompt.prompt, note.as_ref())
        };
        let request = DispatchRequest {
            repo: self.repo.clone(),
            before_head: self.before_head.clone(),
            attempt_id,
            cycle_id: self.run_id.clone(),
            bead_id: self.bead_id.clone(),
            backend: facts.backend,
            dispatch_id: facts.dispatch_id.clone(),
            reasoning_effort: facts.reasoning_effort,
            prompt: prompt_text,
            attempt_identity: dispatch::attempt_commit_identity(),
            sandbox_profile: None,
            worker_runtime_dir: None,
            worker_resource_limits: self.worker_resource_limits,
        };
        let mut hooks = ();
        let result = dispatch::run_with_heartbeat(
            self.exec,
            self.commits,
            &request,
            &self.state_dir,
            self.timeout,
            self.heartbeat_interval,
            &mut hooks,
        )?;
        if !matches!(result.status, DispatchStatus::Success) {
            self.quarantine_if_dirty()?;
        }
        Ok(result)
    }

    fn execute_verify(&self, prompt: &PromptMaterial) -> dispatch::Result<DispatchResult> {
        let n = self.sequence.fetch_add(1, Ordering::SeqCst);
        let attempts_dir = self.run_dir.join("attempts");
        let request = SpawnRequest {
            argv: vec!["sh".to_string(), "-c".to_string(), prompt.prompt.clone()],
            cwd: self.repo.clone(),
            env: Vec::new(),
            stdin: StdinMode::Null,
            sandbox_profile: None,
            worker_resource_limits: Some(self.worker_resource_limits),
            commit_receipt_socket: None,
            stdout_path: attempts_dir.join(format!("verify-{n:06}.out")),
            stderr_path: attempts_dir.join(format!("verify-{n:06}.err")),
        };
        let mut hooks = ();
        dispatch::run_readonly(self.exec, &request, self.timeout, "work-verify", &mut hooks)
    }
}

impl<E: Exec + Sync, C: CommitProbe + Sync, R: RepoRecovery + Sync> AttemptExecutor
    for ProductionAttemptExecutor<'_, E, C, R>
{
    fn execute(
        &self,
        _posture: MutationPosture,
        stage: &Stage,
        candidate: &ApprovedExecution,
        prompt: &PromptMaterial,
    ) -> dispatch::Result<DispatchResult> {
        if stage.id.as_str() == VERIFY_STAGE {
            self.execute_verify(prompt)
        } else {
            self.execute_work(candidate, prompt)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicU64 as StdAtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::dispatch::{CommandExec, DispatchFailure, GitCommitProbe};
    use crate::runner::{AttemptRunner, CallBudget, RunRequest, RunnerPorts, SystemClock};

    struct TempDir(PathBuf);

    static NONCE: StdAtomicU64 = StdAtomicU64::new(0);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let n = NONCE.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("undertake-work-policy-{label}-{nanos}-{n}"));
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

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_git_repo(repo: &Path) {
        std::fs::create_dir_all(repo).expect("mkdir repo");
        git(repo, &["init"]);
        git(repo, &["config", "user.name", "Undertake Test"]);
        git(
            repo,
            &["config", "user.email", "undertake-test@example.invalid"],
        );
        std::fs::write(repo.join("README.md"), b"seed\n").expect("write seed");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-m", "init"]);
    }

    fn bd_on_path() -> bool {
        Command::new("which")
            .arg("bd")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn init_bd_repo(repo: &Path) {
        let output = Command::new("bd")
            .current_dir(repo)
            .args(["init", "--non-interactive", "-p", "fixture"])
            .stdin(Stdio::null())
            .output()
            .expect("spawn bd init");
        assert!(
            output.status.success(),
            "bd init failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn bd_create(repo: &Path, args: &[&str]) {
        let output = Command::new("bd")
            .arg("-C")
            .arg(repo)
            .args(args)
            .arg("--json")
            .stdin(Stdio::null())
            .output()
            .expect("spawn bd create");
        assert!(
            output.status.success(),
            "bd create failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn bd_set_metadata(
        client: crate::bd::CommandBdClient,
        repo: &Path,
        id: &str,
        key: &str,
        value: &str,
    ) {
        crate::bd::BdClient::set_metadata(&client, repo, id, key, value)
            .unwrap_or_else(|error| panic!("bd set-metadata {key}={value}: {error}"));
    }

    /// Writes a scripted worker backend: a shell script the `SpawnRequest`
    /// argv invokes (by absolute path — see [`ScriptedTestExecutor`]), which
    /// writes `marker` and commits it. The script ignores all arguments
    /// passed to it and just performs the fixed commit.
    fn write_worker_script(path: &Path, exit_code: i32, make_commit: bool) {
        let commit = if make_commit {
            "git add -A && GIT_AUTHOR_NAME=Worker GIT_AUTHOR_EMAIL=worker@example.invalid \
             GIT_COMMITTER_NAME=Worker GIT_COMMITTER_EMAIL=worker@example.invalid \
             git commit -m worked >/dev/null"
        } else {
            "true"
        };
        let script = format!("#!/bin/sh\necho worked > marker\n{commit}\nexit {exit_code}\n");
        std::fs::write(path, script).expect("write worker script");
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            perms.set_mode(0o755);
        }
        std::fs::set_permissions(path, perms).expect("chmod");
    }

    #[test]
    fn quarantine_note_wraps_prompt_when_present() {
        let unwrapped = append_quarantine_note("base", None);
        assert_eq!(unwrapped, "base");
        let note = QuarantineNote {
            path: "artifacts/quarantine-000000.patch".to_string(),
            sha256: "deadbeef".to_string(),
            changed_paths: 2,
        };
        let wrapped = append_quarantine_note("base", Some(&note));
        assert!(wrapped.starts_with("base"));
        assert!(wrapped.contains("artifacts/quarantine-000000.patch"));
        assert!(wrapped.contains("deadbeef"));
        assert!(wrapped.contains("2 path(s)"));
    }

    #[test]
    fn work_policy_terminal_is_blocked_with_no_eligible_candidate() {
        let policy = WorkPolicy::new(
            fixture_issue(),
            "true".to_string(),
            PathBuf::from("/tmp/example"),
            Vec::new(),
            run::StageAttemptLimit::new(1).expect("nonzero"),
        );
        let ledger = StageLedger::new();
        assert_eq!(
            policy.terminal(&ledger).verdict,
            run::TerminalVerdict::Blocked
        );
    }

    fn fixture_issue() -> Issue {
        Issue {
            id: "fixture-work-1".to_string(),
            title: "fixture".to_string(),
            description: "fixture description".to_string(),
            acceptance_criteria: "fixture acceptance".to_string(),
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

    /// Test-scoped [`AttemptExecutor`]. Uses real [`CommandExec`] and real
    /// [`GitCommitProbe`] for every process/git operation it performs, but
    /// invokes the scripted worker backend by its absolute path via
    /// [`dispatch::run_readonly`] rather than through
    /// [`ProductionAttemptExecutor::execute_work`]'s
    /// `dispatch::run_with_heartbeat`. The latter builds its argv through
    /// `dispatch::argv_for_backend`, which always spawns one of five fixed
    /// program names (`pi`/`claude`/`codex`/`omp`/`agy`) resolved via
    /// `PATH` — redirecting that to a test script would require mutating
    /// the test process's `PATH`, which needs `std::env::set_var` and this
    /// crate sets `unsafe_code = "forbid"` (`Cargo.toml`). Since the
    /// backend-argv/commit-receipt-authentication machinery
    /// `run_with_heartbeat` wraps is pre-existing, unmodified `dispatch.rs`
    /// infrastructure with its own extensive test coverage (see e.g.
    /// `dispatch.rs`'s `worker_process_limit_bounds_setsid_fork_exhaustion`
    /// and neighbors, which already exercise real `CommandExec` dispatch),
    /// this executor instead classifies "did a new authenticated-enough
    /// commit land" itself via real `CommitProbe::head`/`is_direct_child`
    /// after the scripted process exits — proving the same observable
    /// outcome (`WorkPolicy`/`AttemptRunner` correctly drive a real worker
    /// spawn, a real commit, and a real mechanical verify) without
    /// reimplementing or bypassing that authentication logic.
    struct ScriptedTestExecutor<'a, E: Exec + Sync, C: CommitProbe + Sync> {
        exec: &'a E,
        commits: &'a C,
        repo: PathBuf,
        run_dir: PathBuf,
        script: PathBuf,
        before_head: Option<String>,
        sequence: AtomicU64,
    }

    impl<E: Exec + Sync, C: CommitProbe + Sync> AttemptExecutor for ScriptedTestExecutor<'_, E, C> {
        fn execute(
            &self,
            _posture: MutationPosture,
            stage: &Stage,
            _candidate: &ApprovedExecution,
            prompt: &PromptMaterial,
        ) -> dispatch::Result<DispatchResult> {
            let n = self.sequence.fetch_add(1, Ordering::SeqCst);
            let attempts_dir = self.run_dir.join("attempts");
            let request = SpawnRequest {
                argv: if stage.id.as_str() == VERIFY_STAGE {
                    vec!["sh".to_string(), "-c".to_string(), prompt.prompt.clone()]
                } else {
                    vec![self.script.display().to_string()]
                },
                cwd: self.repo.clone(),
                env: Vec::new(),
                stdin: StdinMode::Null,
                sandbox_profile: None,
                worker_resource_limits: None,
                commit_receipt_socket: None,
                stdout_path: attempts_dir.join(format!("test-{n:06}.out")),
                stderr_path: attempts_dir.join(format!("test-{n:06}.err")),
            };
            let mut hooks = ();
            let result = dispatch::run_readonly(
                self.exec,
                &request,
                Duration::from_secs(20),
                "test-attempt",
                &mut hooks,
            )?;
            if stage.id.as_str() == VERIFY_STAGE || result.status != DispatchStatus::Success {
                return Ok(result);
            }
            let head = self.commits.head(&self.repo)?;
            let is_new_commit = head.as_deref() != self.before_head.as_deref()
                && self.commits.is_direct_child(
                    &self.repo,
                    self.before_head.as_deref(),
                    head.as_deref().unwrap_or_default(),
                )?;
            if is_new_commit {
                Ok(DispatchResult {
                    worker_commit: head,
                    ..result
                })
            } else {
                Ok(DispatchResult {
                    status: DispatchStatus::Failed(DispatchFailure::NoNewCommit),
                    ..result
                })
            }
        }
    }

    /// Shared end-to-end scaffolding for both acceptance scenarios: a real
    /// temp git repo, a real temp `bd` repo (the whole test is skipped if
    /// `bd` is not on `PATH`, mirroring `bd.rs`'s own release-race test), a
    /// claimed-and-triaged bead with `verify_cmd`, and a production
    /// `WorkPolicy` plus `AttemptRunner` plus real `BeadGateway`
    /// (`CommandBdClient`), `CommandExec`, and `GitCommitProbe`, run driven
    /// in-process rather than through `cli::run`.
    ///
    /// The bead's stated alternative is that driving the same production
    /// `WorkPolicy` plus `AttemptRunner` plus production `BeadGateway` and
    /// `CommandExec` path in-process is acceptable. Exercising `cli::run`'s
    /// argument parsing and live Musterroll roster resolution here would
    /// only add process-spawning flakiness without covering any additional
    /// runner or policy code this harness doesn't already drive directly.
    #[expect(
        clippy::too_many_lines,
        reason = "one linear scaffold: repo/bd fixtures, roster resolution stand-in, run \
                  creation, and the AttemptRunner call, in the order a reader needs them"
    )]
    fn run_work_scenario(
        label: &str,
        verify_cmd: &str,
    ) -> Option<(
        TempDir,
        crate::bd::CommandBdClient,
        PathBuf,
        String,
        Terminal,
    )> {
        if !bd_on_path() {
            return None;
        }
        let temp = TempDir::new(label);
        let repo = temp.path().join("repo");
        init_git_repo(&repo);
        init_bd_repo(&repo);
        let bead_id = "fixture-work-e2e";
        bd_create(
            &repo,
            &[
                "create",
                "fixture work item",
                "--id",
                bead_id,
                "--description",
                "fixture description",
                "--acceptance",
                "fixture acceptance",
                "-t",
                "task",
                "-p",
                "1",
            ],
        );
        let bd_client = crate::bd::CommandBdClient::new();
        bd_set_metadata(bd_client, &repo, bead_id, "tier_floor", "junior");
        bd_set_metadata(bd_client, &repo, bead_id, "complexity", "S");
        bd_set_metadata(bd_client, &repo, bead_id, "verify_cmd", verify_cmd);

        let script = temp.path().join("worker.sh");
        write_worker_script(&script, 0, true);

        let issue = crate::bd::BdClient::show(&bd_client, &repo, bead_id).expect("show issue");
        let triage = crate::fields::extract(&issue);
        let extracted_verify_cmd = match triage {
            crate::fields::Triage::Triaged(fields) => fields
                .verify_cmd
                .expect("verify_cmd present on a triaged issue"),
            crate::fields::Triage::Untriaged { missing } => {
                panic!("expected a triaged issue, missing {missing:?}")
            }
        };
        assert_eq!(extracted_verify_cmd, verify_cmd);

        let candidate = ApprovedExecution {
            profile_id: "test-worker".to_string(),
            provider_id: "test-provider".to_string(),
            availability_key: "test-worker-avail".to_string(),
            execution_key: "test-worker-exec".to_string(),
        };
        let policy = WorkPolicy::new(
            issue,
            extracted_verify_cmd,
            repo.clone(),
            vec![candidate],
            run::StageAttemptLimit::new(1).expect("nonzero"),
        );
        let max_attempts = u64::from(CallBudget::worst_case(&policy.stage_plan()).ceiling());

        let state_dir = temp.path().join("state");
        let commits = GitCommitProbe;
        let before_head = crate::dispatch::CommitProbe::head(&commits, &repo)
            .expect("head")
            .expect("head present after init commit");

        let mut handle = run::RunHandle::create(
            &state_dir,
            run::RunJob::Work,
            run::NewRun {
                target: run::RunTarget {
                    repo: repo.display().to_string(),
                    bead: Some(bead_id.to_string()),
                },
                approved_profiles: vec!["test-worker".to_string()],
                musterroll_roster_artifact: None,
                roster_snapshot: None,
                limits: run::RunLimits {
                    item_wall_clock_mins: Some(5),
                    max_attempts: Some(max_attempts),
                },
                verifier: run::RunVerifier {
                    mechanical: Some(verify_cmd.to_string()),
                    qualitative: None,
                },
                work: Some(run::WorkState {
                    cycle_id: format!("work-{bead_id}"),
                    authorization_sha256: "0".repeat(64),
                    before_head: Some(before_head.clone()),
                    owner_pid: Some(std::process::id()),
                    owner_pid_generation: crate::quarantine::process_generation(std::process::id()),
                    worker_pgid: None,
                    worker_pgid_generation: None,
                    worker_slots: Vec::new(),
                    worker_profile: None,
                    worker_commit: None,
                    mechanical: None,
                    review_resume_budget_secs: None,
                    stage: run::WorkStage::Implementing,
                }),
                approval: None,
            },
        )
        .expect("create run");

        let run_dir = handle.dir().to_path_buf();
        let exec = CommandExec;
        let executor = ScriptedTestExecutor {
            exec: &exec,
            commits: &commits,
            repo: repo.clone(),
            run_dir,
            script,
            before_head: Some(before_head.clone()),
            sequence: AtomicU64::new(0),
        };

        let digests = HeadDigestSource::new(&commits, repo.clone());
        let request = RunRequest {
            state_dir: state_dir.clone(),
            backend: Backend::Pi,
            owner: "undertake".to_string(),
            pinned_digests: BTreeMap::new(),
        };
        let ports = RunnerPorts {
            exec: &exec,
            commits: &commits,
            bd: &bd_client,
            executor: &executor,
            clock: &SystemClock,
            digests: &digests,
        };

        let terminal =
            AttemptRunner::run(&policy, &ports, &mut handle, &request).expect("runner completes");

        // Mirrors what `undertake work`'s CLI does after `AttemptRunner::run`
        // returns: a best-effort diagnostic comment on any non-`Completed`
        // terminal, posted outside the runner's own single-durable-mutation
        // boundary (`finalize` already performed the one required close/
        // release; this is a follow-up, not a second required mutation).
        if terminal.verdict != run::TerminalVerdict::Completed {
            if let Some(reason) = &terminal.reason {
                crate::bd::BdClient::comment(
                    &bd_client,
                    &repo,
                    bead_id,
                    &format!("undertake work: {reason}"),
                )
                .expect("post diagnostic comment");
            }
        }

        Some((temp, bd_client, repo, bead_id.to_string(), terminal))
    }

    #[test]
    fn work_policy_end_to_end_commits_verifies_and_closes_the_bead() {
        let Some((_temp, bd_client, repo, bead_id, terminal)) =
            run_work_scenario("e2e-success", "test -f marker")
        else {
            return;
        };
        assert_eq!(
            terminal.verdict,
            run::TerminalVerdict::Completed,
            "terminal: {terminal:?}"
        );

        let closed = crate::bd::BdClient::show(&bd_client, &repo, &bead_id).expect("show closed");
        assert_eq!(closed.status, "closed");

        let head = git(&repo, &["log", "--oneline", "-1"]);
        assert!(head.contains("worked"), "unexpected head: {head}");
        assert!(repo.join("marker").exists());
    }

    #[test]
    fn work_policy_end_to_end_failing_verify_releases_with_diagnostic_comment() {
        let Some((_temp, bd_client, repo, bead_id, terminal)) =
            run_work_scenario("e2e-verify-fail", "test -f nonexistent-marker")
        else {
            return;
        };
        assert_eq!(
            terminal.verdict,
            run::TerminalVerdict::Failed,
            "terminal: {terminal:?}"
        );

        let released =
            crate::bd::BdClient::show(&bd_client, &repo, &bead_id).expect("show released");
        assert_eq!(released.status, "open");
        assert!(released.assignee.is_none());

        let comments = Command::new("bd")
            .arg("-C")
            .arg(&repo)
            .args(["comments", &bead_id])
            .arg("--json")
            .stdin(Stdio::null())
            .output()
            .expect("spawn bd comments");
        assert!(comments.status.success());
        let comments_text = String::from_utf8_lossy(&comments.stdout);
        assert!(
            comments_text.contains("verify_cmd failed"),
            "expected a diagnostic comment mentioning the verify_cmd failure, got: {comments_text}"
        );
    }
}
