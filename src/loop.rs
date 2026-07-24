#![allow(
    dead_code,
    reason = "the loop kernel is activated by the next job-specific CLI cutover"
)]

//! Native, explicit-target work loop.
//!
//! This module deliberately owns only the bounded worker/verifier state machine.
//! Legacy fleet dispatch remains in `dispatch_cycle`; callers supply an explicit
//! target and the already-approved harness and Bead mutation adapters.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::dispatch::{ChildProcess, CommitProbe, Exec, ProcessStatus, SpawnRequest};
use crate::quarantine::RepoLease;
use crate::run::{
    EventInput, EventKind, NewRun, RunHandle, RunJob, RunLimits, RunTarget, RunVerifier,
    WorkState,
};

const LOOP_STATE_PATH: &str = "loop.json";
const LOOP_SCHEMA: &str = "undertake/loop@1";
pub(crate) type Result<T, E = LoopError> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub(crate) struct LoopError {
    message: String,
}

impl LoopError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LoopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LoopError {}

/// The one immutable input accepted by the first native work loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopTarget {
    Bead(String),
    Artifact(String),
}

impl LoopTarget {
    fn validate(&self) -> Result<()> {
        let value = match self {
            Self::Bead(value) | Self::Artifact(value) => value,
        };
        if value.trim().is_empty() || value != value.trim() {
            return Err(LoopError::new("loop target must be non-empty and trimmed"));
        }
        Ok(())
    }

    fn bead(&self) -> Option<String> {
        match self {
            Self::Bead(value) => Some(value.clone()),
            Self::Artifact(_) => None,
        }
    }
}

/// Immutable invocation policy for one explicit repository target.
#[derive(Debug, Clone)]
pub(crate) struct LoopRequest {
    pub(crate) state_dir: PathBuf,
    pub(crate) repo: PathBuf,
    pub(crate) target: LoopTarget,
    pub(crate) profile_id: String,
    pub(crate) verifier_command: String,
    pub(crate) max_iterations: u64,
    pub(crate) iteration_timeout: Duration,
    pub(crate) resume_run_id: Option<String>,
}

impl LoopRequest {
    fn validate(&self) -> Result<()> {
        self.target.validate()?;
        if self.profile_id.trim().is_empty() || self.profile_id != self.profile_id.trim() {
            return Err(LoopError::new("loop profile_id must be non-empty and trimmed"));
        }
        if self.verifier_command.trim().is_empty() || self.verifier_command != self.verifier_command.trim() {
            return Err(LoopError::new("loop verifier command must be non-empty and trimmed"));
        }
        if self.max_iterations == 0 {
            return Err(LoopError::new("loop max_iterations must be nonzero"));
        }
        if self.iteration_timeout.is_zero() {
            return Err(LoopError::new("loop iteration timeout must be nonzero"));
        }
        Ok(())
    }

    fn canonical_repo(&self) -> String {
        self.repo.to_string_lossy().into_owned()
    }
}

/// Per-attempt input supplied to the selected harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoopIteration {
    pub(crate) number: u64,
    pub(crate) fresh_context_id: String,
    pub(crate) feedback: Vec<String>,
}

/// Supplies a fresh worker context and the matching mechanical verifier command.
pub(crate) trait LoopHarness {
    fn worker(&self, iteration: &LoopIteration) -> Result<SpawnRequest>;
    fn verifier(&self, iteration: &LoopIteration) -> Result<SpawnRequest>;
}

/// Applies the one Bead transition only after terminal run evidence is durable.
pub(crate) trait LoopClaim {
    fn release(&self, repo: &Path, target: &LoopTarget, reason: &str) -> Result<()>;
    fn close(&self, repo: &Path, target: &LoopTarget, reason: &str) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopTerminal {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LoopPhase {
    Ready,
    WorkerStarted,
    VerifierStarted,
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoopState {
    schema: String,
    target: String,
    attempts: u64,
    phase: LoopPhase,
    feedback: Vec<String>,
    terminal: Option<LoopTerminalState>,
    claim_transition_applied: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LoopTerminalState {
    Completed,
    Failed,
}

impl From<LoopTerminalState> for LoopTerminal {
    fn from(value: LoopTerminalState) -> Self {
        match value {
            LoopTerminalState::Completed => Self::Completed,
            LoopTerminalState::Failed => Self::Failed,
        }
    }
}

impl From<LoopTerminal> for LoopTerminalState {
    fn from(value: LoopTerminal) -> Self {
        match value {
            LoopTerminal::Completed => Self::Completed,
            LoopTerminal::Failed => Self::Failed,
        }
    }
}

impl LoopState {
    fn new(target: &LoopTarget) -> Self {
        Self {
            schema: LOOP_SCHEMA.to_string(),
            target: target_label(target),
            attempts: 0,
            phase: LoopPhase::Ready,
            feedback: Vec::new(),
            terminal: None,
            claim_transition_applied: false,
        }
    }
}

/// The native kernel. Each call either completes a terminal transition or
/// leaves a fully durable recovery point under `runs-v2/<run-id>/loop.json`.
pub(crate) struct LoopKernel;

impl LoopKernel {
    pub(crate) fn start<C: CommitProbe + ?Sized>(commits: &C, request: &LoopRequest) -> Result<String> {
        request.validate()?;
        let before_head = commits
            .head(&request.repo)
            .map_err(|error| LoopError::new(format!("read initial repository HEAD: {error}")))?;
        let authorization = authorization_hash(request);
        let work = WorkState {
            cycle_id: format!("loop-{}", &authorization[..16]),
            authorization_sha256: authorization,
            before_head,
            owner_pid: Some(std::process::id()),
            worker_pgid: None,
            worker_profile: None,
            worker_commit: None,
            mechanical: None,
            review_resume_budget_secs: None,
            stage: crate::run::WorkStage::Implementing,
        };
        let run = RunHandle::create(
            &request.state_dir,
            RunJob::Work,
            NewRun {
                target: RunTarget {
                    repo: request.canonical_repo(),
                    bead: request.target.bead(),
                },
                approved_profiles: vec![request.profile_id.clone()],
                limits: RunLimits {
                    item_wall_clock_mins: Some(request.iteration_timeout.as_secs() / 60),
                    max_attempts: Some(request.max_iterations),
                },
                verifier: RunVerifier {
                    mechanical: Some(request.verifier_command.clone()),
                    qualitative: None,
                },
                work: Some(work),
                ..NewRun::default()
            },
        )
        .map_err(run_error)?;
        let state = LoopState::new(&request.target);
        write_state(run.dir(), &state)?;
        Ok(run.run_id().to_string())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the durable checkpoint boundaries must remain visible in the loop state machine"
    )]
    pub(crate) fn run<E, C, B, H>(
        exec: &E,
        commits: &C,
        claim: &B,
        harness: &H,
        request: &LoopRequest,
    ) -> Result<LoopTerminal>
    where
        E: Exec + ?Sized,
        C: CommitProbe + ?Sized,
        B: LoopClaim + ?Sized,
        H: LoopHarness + ?Sized,
    {
        request.validate()?;
        let lease_holder = request.resume_run_id.clone().unwrap_or_else(|| {
            format!("loop-pending-{}", &authorization_hash(request)[..16])
        });
        let _lease = RepoLease::acquire(
            &request.state_dir,
            &request.canonical_repo(),
            &lease_holder,
        )
        .map_err(|error| LoopError::new(format!("acquire exclusive repo lease: {error}")))?;
        let mut run = if let Some(run_id) = request.resume_run_id.as_deref() {
            RunHandle::open(&request.state_dir, run_id).map_err(run_error)?
        } else {
            let run_id = Self::start(commits, request)?;
            RunHandle::open(&request.state_dir, &run_id).map_err(run_error)?
        };
        validate_run_target(&run, request)?;
        let mut state = read_state(run.dir(), &request.target)?;
        if let Some(terminal) = state.terminal {
            return Self::complete_transition(claim, request, &mut state, &mut run, terminal.into());
        }
        if !matches!(state.phase, LoopPhase::Ready) && run.worker_pgid().is_some() {
            return Err(LoopError::new(
                "resume refuses to reclaim a run with an unproven worker process group",
            ));
        }
        if !matches!(state.phase, LoopPhase::Ready) {
            state.phase = LoopPhase::Ready;
            state.feedback.push("interrupted attempt reclaimed before fresh context".to_string());
            write_state(run.dir(), &state)?;
            run.append_event(
                EventKind::AttemptFinished,
                EventInput {
                    outcome: Some("interrupted_reclaimed".to_string()),
                    ..EventInput::default()
                },
            )
            .map_err(run_error)?;
        }
        while state.attempts < request.max_iterations {
            let iteration = LoopIteration {
                number: state.attempts + 1,
                fresh_context_id: fresh_context_id(run.run_id(), state.attempts + 1),
                feedback: state.feedback.clone(),
            };
            state.phase = LoopPhase::WorkerStarted;
            write_state(run.dir(), &state)?;
            run.append_event(
                EventKind::AttemptStarted,
                EventInput {
                    profile_id: Some(request.profile_id.clone()),
                    outcome: Some(format!("fresh_context={}", iteration.fresh_context_id)),
                    ..EventInput::default()
                },
            )
            .map_err(run_error)?;
            run.invalidate_worker_group().map_err(run_error)?;

            let before = commits
                .head(&request.repo)
                .map_err(|error| LoopError::new(format!("read pre-worker HEAD: {error}")))?;
            let worker = match harness
                .worker(&iteration)
                .and_then(|spawn| run_worker_process(exec, &spawn, request.iteration_timeout, &mut run))
            {
                Ok(worker) => worker,
                Err(error) => {
                    fail_attempt(&mut state, &mut run, format!("worker could not start or finish: {error}"))?;
                    continue;
                }
            };
            // `run_worker_process` returned only after proving the worker group
            // quiescent; persist that proof before any verifier checkpoint.
            run.invalidate_worker_group().map_err(run_error)?;
            let after = commits
                .head(&request.repo)
                .map_err(|error| LoopError::new(format!("read post-worker HEAD: {error}")))?;
            let worker_commit = worker.authenticated_commit;
            let authenticated = worker.status.success()
                && worker_commit.as_ref().is_some_and(|commit| after.as_deref() == Some(commit))
                && worker_commit.as_ref().is_some_and(|commit| {
                    commits
                        .is_direct_child(&request.repo, before.as_deref(), commit)
                        .unwrap_or(false)
                });
            if !authenticated {
                fail_attempt(
                    &mut state,
                    &mut run,
                    "worker did not produce its authenticated direct-child commit".to_string(),
                )?;
                continue;
            }
            state.phase = LoopPhase::VerifierStarted;
            write_state(run.dir(), &state)?;
            let verifier = match harness.verifier(&iteration).and_then(|spawn| run_process(exec, &spawn, request.iteration_timeout)) {
                Ok(verifier) => verifier,
                Err(error) => {
                    fail_attempt(&mut state, &mut run, format!("verify_cmd failed: {error}"))?;
                    continue;
                }
            };
            if !verifier.status.success() {
                fail_attempt(&mut state, &mut run, "verify_cmd failed".to_string())?;
                continue;
            }
            run.append_event(
                EventKind::VerifyFinished,
                EventInput {
                    outcome: Some("passed".to_string()),
                    ..EventInput::default()
                },
            )
            .map_err(run_error)?;
            state.terminal = Some(LoopTerminalState::Completed);
            state.phase = LoopPhase::Terminal;
            write_state(run.dir(), &state)?;
            return Self::complete_transition(claim, request, &mut state, &mut run, LoopTerminal::Completed);
        }
        state.terminal = Some(LoopTerminalState::Failed);
        state.phase = LoopPhase::Terminal;
        state.feedback.push("max iterations exhausted".to_string());
        write_state(run.dir(), &state)?;
        Self::complete_transition(claim, request, &mut state, &mut run, LoopTerminal::Failed)
    }

    fn complete_transition<B: LoopClaim + ?Sized>(
        claim: &B,
        request: &LoopRequest,
        state: &mut LoopState,
        run: &mut RunHandle,
        terminal: LoopTerminal,
    ) -> Result<LoopTerminal> {
        if !state.claim_transition_applied {
            if !matches!(run.manifest().lifecycle, crate::run::RunLifecycle::Finished) {
                let outcome = match terminal {
                    LoopTerminal::Completed => "completed",
                    LoopTerminal::Failed => "failed",
                };
                run.finish(outcome).map_err(run_error)?;
            }
            match terminal {
                LoopTerminal::Completed => claim.close(&request.repo, &request.target, "verified loop completion")?,
                LoopTerminal::Failed => claim.release(&request.repo, &request.target, "loop terminal failure")?,
            }
            state.claim_transition_applied = true;
            write_state(run.dir(), state)?;
        }
        Ok(terminal)
    }
}

struct ProcessRun {
    status: ProcessStatus,
    authenticated_commit: Option<String>,
}

fn run_worker_process<E: Exec + ?Sized>(
    exec: &E,
    spawn: &SpawnRequest,
    timeout: Duration,
    run: &mut RunHandle,
) -> Result<ProcessRun> {
    let child = exec
        .spawn(spawn)
        .map_err(|error| LoopError::new(format!("spawn worker: {error}")))?;
    if let Some(pgid) = child.id() {
        run.record_worker_group(pgid).map_err(run_error)?;
    }
    finish_process(child, timeout)
}

fn run_process<E: Exec + ?Sized>(
    exec: &E,
    spawn: &SpawnRequest,
    timeout: Duration,
) -> Result<ProcessRun> {
    let child = exec
        .spawn(spawn)
        .map_err(|error| LoopError::new(format!("spawn subprocess: {error}")))?;
    finish_process(child, timeout)
}

fn finish_process(mut child: Box<dyn ChildProcess>, timeout: Duration) -> Result<ProcessRun> {
    let status = if let Some(status) = child
        .wait_for(timeout)
        .map_err(|error| LoopError::new(format!("wait for subprocess: {error}")))?
    {
        status
    } else {
        child
            .terminate()
            .map_err(|error| LoopError::new(format!("terminate timed-out subprocess: {error}")))?;
        child
            .wait()
            .map_err(|error| LoopError::new(format!("reap timed-out subprocess: {error}")))?
    };
    child
        .ensure_worker_quiescent()
        .map_err(|error| LoopError::new(format!("prove subprocess quiescent: {error}")))?;
    Ok(ProcessRun {
        status,
        authenticated_commit: child.authenticated_worker_commit(),
    })
}

fn fail_attempt(state: &mut LoopState, run: &mut RunHandle, feedback: String) -> Result<()> {
    state.attempts += 1;
    state.phase = LoopPhase::Ready;
    state.feedback.push(feedback.clone());
    write_state(run.dir(), state)?;
    run.append_event(
        EventKind::AttemptFinished,
        EventInput {
            outcome: Some(feedback),
            ..EventInput::default()
        },
    )
    .map_err(run_error)
}

fn target_label(target: &LoopTarget) -> String {
    match target {
        LoopTarget::Bead(id) => format!("bead:{id}"),
        LoopTarget::Artifact(path) => format!("artifact:{path}"),
    }
}

fn authorization_hash(request: &LoopRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(request.canonical_repo().as_bytes());
    hasher.update(target_label(&request.target).as_bytes());
    hasher.update(request.profile_id.as_bytes());
    hasher.update(request.verifier_command.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn fresh_context_id(run_id: &str, attempt: u64) -> String {
    format!("{run_id}-iteration-{attempt}")
}

fn validate_run_target(run: &RunHandle, request: &LoopRequest) -> Result<()> {
    let target = &run.manifest().target;
    if target.repo != request.canonical_repo() || target.bead != request.target.bead() {
        return Err(LoopError::new("resumed run target does not match explicit request"));
    }
    if run.manifest().verifier.mechanical.as_deref() != Some(request.verifier_command.as_str()) {
        return Err(LoopError::new("resumed run verifier does not match explicit request"));
    }
    Ok(())
}

fn read_state(run_dir: &Path, target: &LoopTarget) -> Result<LoopState> {
    let path = run_dir.join(LOOP_STATE_PATH);
    let bytes = fs::read(&path).map_err(|error| LoopError::new(format!("read loop state {}: {error}", path.display())))?;
    let state: LoopState = serde_json::from_slice(&bytes)
        .map_err(|error| LoopError::new(format!("parse loop state {}: {error}", path.display())))?;
    if state.schema != LOOP_SCHEMA || state.target != target_label(target) {
        return Err(LoopError::new("loop state identity does not match explicit target"));
    }
    Ok(state)
}

fn write_state(run_dir: &Path, state: &LoopState) -> Result<()> {
    let path = run_dir.join(LOOP_STATE_PATH);
    let temporary = run_dir.join(format!("{LOOP_STATE_PATH}.tmp-{}", std::process::id()));
    let mut bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| LoopError::new(format!("serialize loop state: {error}")))?;
    bytes.push(b'\n');
    fs::write(&temporary, bytes)
        .map_err(|error| LoopError::new(format!("write loop state {}: {error}", temporary.display())))?;
    fs::rename(&temporary, &path)
        .map_err(|error| LoopError::new(format!("commit loop state {}: {error}", path.display())))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "RunHandle conversions are passed directly to map_err"
)]
fn run_error(error: crate::run::RunError) -> LoopError {
    LoopError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    use crate::dispatch::{ChildProcess, DispatchError, ProcessStatus, StdinMode};

    #[derive(Clone)]
    struct TestHarness {
        inner: Rc<RefCell<TestState>>,
    }
    #[expect(
        clippy::struct_excessive_bools,
        reason = "the test double records independent observable loop outcomes"
    )]
    struct TestState {
        heads: VecDeque<Option<String>>,
        direct: bool,
        workers: VecDeque<TestProcess>,
        verifiers: VecDeque<TestProcess>,
        contexts: Vec<LoopIteration>,
        released: bool,
        panic_next_verifier: bool,
        closed: bool,
        finished_before_close: bool,
        verifier_saw_quiesced_worker: bool,
        state_dir: PathBuf,
    }

    enum TestProcess {
        SpawnError,
        Status(ProcessStatus, Option<String>),
        StatusWithPgid(ProcessStatus, Option<String>, u32),
    }

    impl TestHarness {
        fn new(workers: Vec<TestProcess>, verifiers: Vec<TestProcess>, direct: bool) -> Self {
            let state_dir = std::env::temp_dir().join(format!(
                "undertake-loop-test-{}-{}",
                std::process::id(),
                TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&state_dir);
            fs::create_dir_all(&state_dir).expect("state directory");
            Self {
                inner: Rc::new(RefCell::new(TestState {
                    heads: VecDeque::from([
                        Some("a".repeat(40)),
                        Some("a".repeat(40)),
                        Some("b".repeat(40)),
                        Some("b".repeat(40)),
                        Some("c".repeat(40)),
                    ]),
                    direct,
                    workers: VecDeque::from(workers),
                    verifiers: VecDeque::from(verifiers),
                    contexts: Vec::new(),
                    released: false,
                    closed: false,
                    finished_before_close: false,
                    panic_next_verifier: false,
                    verifier_saw_quiesced_worker: false,
                    state_dir,
                })),
            }
        }

        fn request(&self) -> LoopRequest {
            LoopRequest {
                state_dir: self.inner.borrow().state_dir.clone(),
                repo: self.inner.borrow().state_dir.join("synthetic-repo"),
                target: LoopTarget::Bead("undertake-loop-kernel".to_string()),
                profile_id: "lead".to_string(),
                verifier_command: "cargo test loop".to_string(),
                max_iterations: 3,
                iteration_timeout: Duration::from_secs(1),
                resume_run_id: None,
            }
        }

        fn claim_was_released(&self) -> bool { self.inner.borrow().released }
        fn worker_contexts(&self) -> usize { self.inner.borrow().contexts.len() }
        fn second_context_received(&self, value: &str) -> bool {
            self.inner.borrow().contexts.get(1).is_some_and(|context| context.feedback.iter().any(|feedback| feedback.contains(value)))
        }
        fn finished_run_before_close(&self) -> bool { self.inner.borrow().finished_before_close }
        fn verifier_saw_quiesced_worker(&self) -> bool {
            self.inner.borrow().verifier_saw_quiesced_worker
        }
        fn panic_during_next_verifier(&self) {
            self.inner.borrow_mut().panic_next_verifier = true;
        }
    }

    impl CommitProbe for TestHarness {
        fn head(&self, _repo: &Path) -> crate::dispatch::Result<Option<String>> {
            Ok(self.inner.borrow_mut().heads.pop_front().unwrap_or(Some("c".repeat(40))))
        }
        fn is_clean(&self, _repo: &Path) -> crate::dispatch::Result<bool> { Ok(true) }
        fn is_direct_child(&self, _repo: &Path, _before: Option<&str>, _commit: &str) -> crate::dispatch::Result<bool> { Ok(self.inner.borrow().direct) }
        fn committer_email(&self, _repo: &Path, _commit: &str) -> crate::dispatch::Result<Option<String>> { Ok(None) }
    }

    impl Exec for TestHarness {
        fn spawn(&self, request: &SpawnRequest) -> crate::dispatch::Result<Box<dyn ChildProcess>> {
            let mut state = self.inner.borrow_mut();
            let next = if request.argv.first().is_some_and(|arg| arg == "verify") {
                state.verifiers.pop_front()
            } else {
                state.workers.pop_front()
            };
            match next.unwrap_or(TestProcess::SpawnError) {
                TestProcess::SpawnError => Err(DispatchError::new("synthetic spawn failure")),
                TestProcess::Status(status, commit) => Ok(Box::new(TestChild {
                    status,
                    commit,
                    pgid: None,
                })),
                TestProcess::StatusWithPgid(status, commit, pgid) => Ok(Box::new(TestChild {
                    status,
                    commit,
                    pgid: Some(pgid),
                })),
            }
        }
    }

    impl LoopHarness for TestHarness {
        fn worker(&self, iteration: &LoopIteration) -> Result<SpawnRequest> {
            self.inner.borrow_mut().contexts.push(iteration.clone());
            Ok(spawn("worker"))
        }
        fn verifier(&self, _iteration: &LoopIteration) -> Result<SpawnRequest> {
            let state_dir = self.inner.borrow().state_dir.clone();
            let run_id = fs::read_dir(state_dir.join("runs-v2"))
                .expect("run directory")
                .next()
                .expect("run")
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8 run id");
            let pgid = RunHandle::open(&state_dir, &run_id)
                .expect("open run")
                .worker_pgid();
            let should_panic = {
                let mut state = self.inner.borrow_mut();
                state.verifier_saw_quiesced_worker = pgid.is_none();
                std::mem::take(&mut state.panic_next_verifier)
            };
            assert!(!should_panic, "synthetic verifier crash");
            Ok(spawn("verify"))
        }
    }

    impl LoopClaim for TestHarness {
        fn release(&self, _repo: &Path, _target: &LoopTarget, _reason: &str) -> Result<()> {
            self.inner.borrow_mut().released = true;
            Ok(())
        }
        fn close(&self, _repo: &Path, _target: &LoopTarget, _reason: &str) -> Result<()> {
            let mut state = self.inner.borrow_mut();
            let run = fs::read_dir(state.state_dir.join("runs-v2")).expect("run directory").next().expect("run").expect("entry").path();
            let manifest = fs::read_to_string(run.join("manifest.json")).expect("manifest");
            state.finished_before_close = manifest.contains("\"lifecycle\": \"finished\"");
            state.closed = true;
            Ok(())
        }
    }

    struct TestChild {
        status: ProcessStatus,
        commit: Option<String>,
        pgid: Option<u32>,
    }
    impl ChildProcess for TestChild {
        fn wait_for(&mut self, _timeout: Duration) -> crate::dispatch::Result<Option<ProcessStatus>> { Ok(Some(self.status)) }
        fn terminate(&mut self) -> crate::dispatch::Result<()> { Ok(()) }
        fn kill(&mut self) -> crate::dispatch::Result<()> { Ok(()) }
        fn wait(&mut self) -> crate::dispatch::Result<ProcessStatus> { Ok(self.status) }
        fn authenticated_worker_commit(&self) -> Option<String> { self.commit.clone() }
        fn id(&self) -> Option<u32> { self.pgid }
    }

    fn spawn(command: &str) -> SpawnRequest {
        SpawnRequest {
            argv: vec![command.to_string()], cwd: PathBuf::from("."), env: Vec::new(), stdin: StdinMode::Null,
            sandbox_profile: None, commit_receipt_socket: None, stdout_path: PathBuf::from("stdout"), stderr_path: PathBuf::from("stderr"),
        }
    }

    fn success_worker(commit: char) -> TestProcess { TestProcess::Status(ProcessStatus::code(0), Some(commit.to_string().repeat(40))) }
    fn success_verifier() -> TestProcess { TestProcess::Status(ProcessStatus::code(0), None) }

    #[test]
    fn loop_resumes_after_interrupted_worker_attempt() {
        let harness = TestHarness::new(vec![success_worker('b')], vec![success_verifier()], true);
        let mut request = harness.request();
        let run_id = LoopKernel::start(&harness, &request).expect("start");
        let run = RunHandle::open(&request.state_dir, &run_id).expect("open");
        let mut state = read_state(run.dir(), &request.target).expect("state");
        state.phase = LoopPhase::WorkerStarted;
        write_state(run.dir(), &state).expect("interrupted checkpoint");
        request.resume_run_id = Some(run_id);
        assert_eq!(LoopKernel::run(&harness, &harness, &harness, &harness, &request).expect("resume"), LoopTerminal::Completed);
    }

    #[test]
    fn loop_rejects_a_commit_not_authenticated_to_its_worker() {
        let harness = TestHarness::new(vec![success_worker('b')], vec![], false);
        assert_eq!(LoopKernel::run(&harness, &harness, &harness, &harness, &harness.request().with_max_iterations(1)).expect("terminal"), LoopTerminal::Failed);
    }

    #[test]
    fn loop_rejects_a_concurrent_repository_lease() {
        let harness = TestHarness::new(vec![], vec![], true);
        let request = harness.request();
        let lease = RepoLease::acquire(&request.state_dir, &request.canonical_repo(), "other")
            .expect("held lease");
        assert!(LoopKernel::run(&harness, &harness, &harness, &harness, &request).is_err());
        drop(lease);
        assert!(!request.state_dir.join("runs-v2").exists());
    }

    #[test]
    fn loop_clears_completed_worker_identity_before_verifier_subprocess() {
        let harness = TestHarness::new(
            vec![TestProcess::StatusWithPgid(
                ProcessStatus::code(0),
                Some("b".repeat(40)),
                42,
            )],
            vec![success_verifier()],
            true,
        );
        assert_eq!(
            LoopKernel::run(&harness, &harness, &harness, &harness, &harness.request())
                .expect("terminal"),
            LoopTerminal::Completed
        );
        assert!(harness.verifier_saw_quiesced_worker());
    }

    #[test]
    fn loop_resumes_after_crashing_at_verifier_checkpoint() {
        let harness = TestHarness::new(
            vec![
                TestProcess::StatusWithPgid(
                    ProcessStatus::code(0),
                    Some("b".repeat(40)),
                    42,
                ),
                success_worker('c'),
            ],
            vec![success_verifier()],
            true,
        );
        let mut request = harness.request();
        harness.panic_during_next_verifier();
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = LoopKernel::run(&harness, &harness, &harness, &harness, &request);
        }))
        .is_err());
        let run_id = fs::read_dir(request.state_dir.join("runs-v2"))
            .expect("run directory")
            .next()
            .expect("run")
            .expect("entry")
            .file_name()
            .into_string()
            .expect("utf-8 run id");
        request.resume_run_id = Some(run_id);
        assert_eq!(
            LoopKernel::run(&harness, &harness, &harness, &harness, &request)
                .expect("resume verifier checkpoint"),
            LoopTerminal::Completed
        );
    }

    #[test]
    fn loop_start_does_not_emit_a_spurious_coverage_gap() {
        let harness = TestHarness::new(vec![], vec![], true);
        let request = harness.request();
        let run_id = LoopKernel::start(&harness, &request).expect("start");
        let events = fs::read_to_string(request.state_dir.join("runs-v2").join(run_id).join("events.jsonl"))
            .expect("events");
        assert!(!events.contains("loop_prepared"));
    }

    #[test]
    fn loop_releases_claim_when_worker_cannot_start() {
        let harness = TestHarness::new(vec![TestProcess::SpawnError], vec![], true);
        assert_eq!(LoopKernel::run(&harness, &harness, &harness, &harness, &harness.request().with_max_iterations(1)).expect("terminal"), LoopTerminal::Failed);
        assert!(harness.claim_was_released());
    }

    #[test]
    fn loop_continues_after_a_failed_attempt() {
        let harness = TestHarness::new(vec![TestProcess::Status(ProcessStatus::code(1), None), success_worker('c')], vec![success_verifier()], true);
        assert_eq!(LoopKernel::run(&harness, &harness, &harness, &harness, &harness.request()).expect("terminal"), LoopTerminal::Completed);
        assert_eq!(harness.worker_contexts(), 2);
    }

    #[test]
    fn loop_stops_when_iteration_budget_is_exhausted() {
        let harness = TestHarness::new(vec![TestProcess::Status(ProcessStatus::code(1), None), TestProcess::Status(ProcessStatus::code(1), None)], vec![], true);
        assert_eq!(LoopKernel::run(&harness, &harness, &harness, &harness, &harness.request().with_max_iterations(2)).expect("terminal"), LoopTerminal::Failed);
        assert_eq!(harness.worker_contexts(), 2);
    }

    #[test]
    fn loop_passes_verifier_feedback_to_next_fresh_context() {
        let harness = TestHarness::new(vec![success_worker('b'), success_worker('c')], vec![TestProcess::Status(ProcessStatus::code(1), None), success_verifier()], true);
        assert_eq!(LoopKernel::run(&harness, &harness, &harness, &harness, &harness.request()).expect("terminal"), LoopTerminal::Completed);
        assert!(harness.second_context_received("verify_cmd failed"));
    }

    #[test]
    fn loop_finishes_run_before_closing_claim_after_verified_commit() {
        let harness = TestHarness::new(vec![success_worker('b')], vec![success_verifier()], true);
        assert_eq!(LoopKernel::run(&harness, &harness, &harness, &harness, &harness.request()).expect("terminal"), LoopTerminal::Completed);
        assert!(harness.finished_run_before_close());
    }

    #[test]
    fn loop_state_rejects_legacy_conductor_schema() {
        let harness = TestHarness::new(Vec::new(), Vec::new(), true);
        let run_dir = harness.inner.borrow().state_dir.join("legacy-loop");
        fs::create_dir_all(&run_dir).unwrap();
        let mut state = LoopState::new(&LoopTarget::Bead("loop-target".to_string()));
        state.schema = "conductor/loop@1".to_string();
        fs::write(
            run_dir.join(LOOP_STATE_PATH),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();

        let error = read_state(&run_dir, &LoopTarget::Bead("loop-target".to_string()))
            .expect_err("legacy loop schema must fail closed");

        assert!(error.to_string().contains("identity"));
    }

    trait RequestExt { fn with_max_iterations(self, max_iterations: u64) -> Self; }
    impl RequestExt for LoopRequest {
        fn with_max_iterations(mut self, max_iterations: u64) -> Self { self.max_iterations = max_iterations; self }
    }
}
