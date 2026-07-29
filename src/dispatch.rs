//! backend runners (pi/agy/claude/codex) behind a trait (Exec) + timeout/kill

// Built ahead of the M4 integration path; unit tests exercise this module directly.
#![allow(dead_code)]

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{Backend, Budgets, ReasoningEffort};
use serde_json::Value;

const PI_THINKING: &str = "xhigh";
const ATTEMPT_IDENTITY_NAME: &str = "Undertake Worker Attempt";
const KILL_GRACE: Duration = Duration::from_secs(3);
const WAIT_POLL: Duration = Duration::from_millis(50);
const HELPER_COMMAND_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const HELPER_CAPTURE_LIMIT: usize = 8 * 1024 * 1024;
const HELPER_ERROR_EVIDENCE_LIMIT: usize = 4 * 1024;
/// Bound on the Claude backend auth-readiness probe (bd `conductor-5p8`).
/// The orchestrator measured `claude auth status --json` hanging past
/// 120-300s in a non-interactive shell on the affected machine; this timeout
/// sits far below that so a hang always classifies `Unreadable` well before
/// it could ever be mistaken for a live "still authenticating" wait, and
/// long before any cycle/item deadline would otherwise absorb the stall.
const CLAUDE_AUTH_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
static HELPER_TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

pub(crate) type Result<T> = std::result::Result<T, DispatchError>;

#[derive(Debug, Clone)]
pub(crate) struct DispatchError {
    message: String,
    worker_state_uncertain: bool,
}

impl DispatchError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            worker_state_uncertain: false,
        }
    }

    fn worker_state_uncertain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            worker_state_uncertain: true,
        }
    }

    pub(crate) const fn leaves_worker_state_uncertain(&self) -> bool {
        self.worker_state_uncertain
    }
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DispatchError {}

const MIB: u64 = 1024 * 1024;

/// Hard Unix limits installed by the trusted worker session wrapper before it
/// changes session or executes backend-controlled code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkerResourceLimits {
    cpu_seconds: u64,
    process_headroom: u64,
    address_space_headroom_bytes: u64,
    file_size_bytes: u64,
}

impl WorkerResourceLimits {
    pub(crate) fn new(
        cpu_seconds: u64,
        process_headroom: u64,
        address_space_headroom_bytes: u64,
        file_size_bytes: u64,
    ) -> Result<Self> {
        if [
            cpu_seconds,
            process_headroom,
            address_space_headroom_bytes,
            file_size_bytes,
        ]
        .contains(&0)
        {
            return Err(DispatchError::new(
                "worker resource limits must all be greater than zero",
            ));
        }
        Ok(Self {
            cpu_seconds,
            process_headroom,
            address_space_headroom_bytes,
            file_size_bytes,
        })
    }

    pub(crate) fn from_budgets(budgets: &Budgets) -> Self {
        Self {
            cpu_seconds: u64::from(budgets.worker_cpu_seconds),
            process_headroom: u64::from(budgets.worker_process_headroom),
            address_space_headroom_bytes:
                u64::from(budgets.worker_address_space_headroom_mib) * MIB,
            file_size_bytes: u64::from(budgets.worker_file_size_mib) * MIB,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchRequest {
    pub(crate) repo: PathBuf,
    pub(crate) before_head: Option<String>,
    pub(crate) attempt_id: String,
    pub(crate) cycle_id: String,
    pub(crate) bead_id: String,
    pub(crate) backend: Backend,
    pub(crate) dispatch_id: String,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) prompt: String,
    /// A per-attempt audit identity. Real worker authority comes from the
    /// kernel-authenticated commit receipt, not this observable value.
    pub(crate) attempt_identity: String,
    /// Parent-authored Seatbelt profile which confines this worker and all of
    /// its descendants to the current isolated checkout.
    pub(crate) sandbox_profile: Option<PathBuf>,
    /// Parent-created scratch root allowed by the profile for this attempt.
    pub(crate) worker_runtime_dir: Option<PathBuf>,
    pub(crate) worker_resource_limits: WorkerResourceLimits,
}

/// Mints a unique audit identity for a single worker attempt's Git metadata.
///
/// This value is intentionally not authority: another same-UID process can
/// observe or recreate environment and commit metadata. Real worker authority
/// is the kernel-authenticated socket receipt handled by [`CommandChild`].
pub(crate) fn attempt_commit_identity() -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use std::hash::BuildHasher;
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

    static MINTED: AtomicU64 = AtomicU64::new(0);

    let mut hasher = Sha256::new();
    let mut seed = [0u8; 32];
    if File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut seed))
        .is_ok()
    {
        hasher.update(seed);
    }
    // Mixed in unconditionally so a platform without `/dev/urandom` still
    // yields a value an already-running process cannot predict.
    let per_process = std::collections::hash_map::RandomState::new();
    hasher.update(
        per_process
            .hash_one(MINTED.fetch_add(1, Ordering::Relaxed))
            .to_le_bytes(),
    );
    hasher.update(std::process::id().to_le_bytes());
    if let Ok(since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH) {
        hasher.update(since_epoch.as_nanos().to_le_bytes());
    }
    let mut nonce = String::with_capacity(32);
    for byte in hasher.finalize().iter().take(16) {
        let _ = write!(nonce, "{byte:02x}");
    }
    format!("undertake-attempt-{nonce}@invalid")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchResult {
    pub(crate) status: DispatchStatus,
    pub(crate) worker_commit: Option<String>,
    pub(crate) authentication_rejection: Option<CommitAuthenticationRejection>,
    pub(crate) stdout_path: PathBuf,
    pub(crate) stderr_path: PathBuf,
    pub(crate) stdout_bytes: u64,
    pub(crate) stderr_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchStatus {
    Success,
    Failed(DispatchFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchFailure {
    TimedOut,
    ExitNonZero { code: Option<i32> },
    NoNewCommit,
    UnauthenticatedCommit,
    BackendFlakeZeroStdoutNoCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitAuthenticationRejection {
    CheckoutChangedBeforeSpawn,
    CheckoutHeadMissing,
    CheckoutHeadNotDirectChild,
    CheckoutDirty,
    ReceiptAbsent,
    ReceiptStale,
    ReceiptMismatched,
    ReceiptAmbiguous,
    AuditIdentityMismatched,
}

impl CommitAuthenticationRejection {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CheckoutChangedBeforeSpawn => "checkout_changed_before_spawn",
            Self::CheckoutHeadMissing => "checkout_head_missing",
            Self::CheckoutHeadNotDirectChild => "checkout_head_not_direct_child",
            Self::CheckoutDirty => "checkout_dirty",
            Self::ReceiptAbsent => "receipt_absent",
            Self::ReceiptStale => "receipt_stale",
            Self::ReceiptMismatched => "receipt_mismatched",
            Self::ReceiptAmbiguous => "receipt_ambiguous",
            Self::AuditIdentityMismatched => "audit_identity_mismatched",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequest {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) stdin: StdinMode,
    pub(crate) sandbox_profile: Option<PathBuf>,
    pub(crate) worker_resource_limits: Option<WorkerResourceLimits>,
    pub(crate) commit_receipt_socket: Option<PathBuf>,
    pub(crate) stdout_path: PathBuf,
    pub(crate) stderr_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StdinMode {
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessStatus {
    code: Option<i32>,
    success: bool,
}

impl ProcessStatus {
    pub(crate) const fn code(code: i32) -> Self {
        Self {
            code: Some(code),
            success: code == 0,
        }
    }

    pub(crate) const fn signal() -> Self {
        Self {
            code: None,
            success: false,
        }
    }

    pub(crate) const fn exit_code(self) -> Option<i32> {
        self.code
    }

    pub(crate) const fn success(self) -> bool {
        self.success
    }
}

impl From<ExitStatus> for ProcessStatus {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
        }
    }
}

/// Backend authentication readiness, classified before a Bead is claimed, an
/// attempt checkout is created, or a worker is spawned (bd `conductor-5p8`).
/// A probe that cannot prove readiness — a timeout, a spawn failure, or
/// unparseable evidence — classifies as [`AuthReadiness::Unreadable`] and
/// fails closed; it is never mistaken for [`AuthReadiness::Ready`]. Every
/// non-`Ready` variant carries an actionable, non-secret operator message
/// naming the supported unattended path; no variant is ever built from
/// token or credential bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthReadiness {
    /// The backend proved it can authenticate unattended.
    Ready,
    /// The backend responded but reports no valid session — logged out, or
    /// a subscription OAuth session that expired.
    NotAuthenticated { message: String },
    /// The readiness probe itself could not produce a trustworthy answer
    /// (timed out, failed to spawn, or returned unparseable evidence).
    Unreadable { message: String },
}

impl AuthReadiness {
    pub(crate) const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

pub(crate) trait Exec {
    fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>>;

    /// Classifies whether `backend` is ready to authenticate an unattended
    /// worker. Dispatch calls this before a Bead is claimed, before an
    /// attempt checkout is created, and before a worker is spawned (bd
    /// `conductor-5p8`) — never after. The default runs the production
    /// probe; in-memory test doubles override it so tests never depend on
    /// real credential state.
    fn auth_readiness(&self, backend: Backend) -> AuthReadiness {
        default_backend_auth_readiness(backend)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommitReceiptEvidence {
    accepted: Vec<String>,
    stale_lineage: usize,
    invalid: usize,
}

impl CommitReceiptEvidence {
    pub(crate) fn rejection_for(&self, expected: &str) -> Option<CommitAuthenticationRejection> {
        if self.accepted.iter().any(|commit| commit == expected) {
            return None;
        }
        let distinct = self
            .accepted
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        if distinct > 1 {
            Some(CommitAuthenticationRejection::ReceiptAmbiguous)
        } else if distinct == 1 || self.invalid > 0 {
            Some(CommitAuthenticationRejection::ReceiptMismatched)
        } else if self.stale_lineage > 0 {
            Some(CommitAuthenticationRejection::ReceiptStale)
        } else {
            Some(CommitAuthenticationRejection::ReceiptAbsent)
        }
    }
    #[cfg(test)]
    pub(crate) fn accepting(commit: String) -> Self {
        Self {
            accepted: vec![commit],
            ..Self::default()
        }
    }
}

pub(crate) trait ChildProcess {
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<ProcessStatus>>;
    fn terminate(&mut self) -> Result<()>;
    fn kill(&mut self) -> Result<()>;
    fn wait(&mut self) -> Result<ProcessStatus>;
    /// The child's OS pid, if it is a real process. Because workers are
    /// spawned as the leader of their own process group (see
    /// [`set_own_process_group`]), this pid also names that group — the durable
    /// identity stale-claim recovery binds to via
    /// [`WorkerHooks::on_spawn`]. In-memory test doubles return `None`, which
    /// recovery treats as an unprovable worker identity and fails closed on.
    fn id(&self) -> Option<u32> {
        None
    }
    /// Returns commit receipts whose connecting processes the kernel proved
    /// descend from this worker root, plus bounded rejection predicates.
    /// In-memory test doubles have no OS lineage and return empty evidence.
    fn commit_receipt_evidence(&self) -> CommitReceiptEvidence {
        CommitReceiptEvidence::default()
    }
    /// Proves that the worker's process group has no surviving descendants
    /// after the direct child exits. A real worker may fork background tools
    /// that outlive the harness process; those descendants must be terminated
    /// before an attempt checkout is removed or a fallback checkout is
    /// created. Test doubles without an OS pid have no recorded group to
    /// prove.
    fn ensure_process_group_quiescent(&mut self) -> Result<()> {
        self.id().map_or(Ok(()), ensure_process_group_quiescent)
    }
    /// Proves process-group quiescence. A descendant which changes session is
    /// contained by the irreversible per-attempt filesystem sandbox instead
    /// of an inherited descriptor.
    fn ensure_worker_quiescent(&mut self) -> Result<()> {
        self.ensure_process_group_quiescent()
    }
}

/// Durable ownership returned by a Work run before its hook is materialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerHookRegistration {
    run_id: String,
    superseded_hook: Option<String>,
}

impl WorkerHookRegistration {
    pub(crate) fn new(run_id: String, superseded_hook: Option<String>) -> Self {
        Self {
            run_id,
            superseded_hook,
        }
    }
}

/// Callbacks the worker runtime invokes around a dispatched worker's lifetime.
/// A single observer (rather than separate closures) so it can hold one
/// exclusive borrow of the run's durable state across both the one-shot
/// spawn hooks and the repeated heartbeat ticks.
pub(crate) trait WorkerHooks {
    /// Invoked exactly once, immediately before the worker's commit hook is
    /// materialized. Work runs durably bind `hook_name` and invalidate the
    /// prior worker group in this callback; the returned run identity lets
    /// bounded garbage collection distinguish current from stale hooks.
    /// Returning `Err` prevents hook creation and process spawn entirely.
    fn on_pre_spawn(
        &mut self,
        _hook_name: &str,
    ) -> Result<Option<WorkerHookRegistration>> {
        Ok(None)
    }
    /// Invoked exactly once, immediately after the worker is spawned and
    /// before it can meaningfully mutate the repository, with the worker's pid
    /// (which also names its process group). Returning an `Err` fails closed:
    /// the just-spawned worker is terminated and reaped before this error
    /// propagates, so a worker whose identity could not be durably recorded
    /// never runs unattended.
    fn on_spawn(&mut self, _pid: Option<u32>) -> Result<()> {
        Ok(())
    }
    /// Invoked only after dispatch proves the worker and all descendants
    /// quiescent, allowing the owning run to release its hook reference.
    fn on_worker_quiescent(&mut self, _hook_name: &str) -> Result<()> {
        Ok(())
    }
    /// Invoked on each heartbeat tick while the worker runs.
    fn on_heartbeat(&mut self, _elapsed: Duration) -> Result<()> {
        Ok(())
    }
}

/// No-op hooks for callers that dispatch a process needing neither durable
/// worker-group binding nor heartbeats (e.g. the plain [`run`] wrapper and
/// read-only reviewer probes).
impl WorkerHooks for () {}

pub(crate) trait CommitProbe {
    fn head(&self, repo: &Path) -> Result<Option<String>>;
    fn is_clean(&self, repo: &Path) -> Result<bool>;
    /// Proves `commit` is the single commit immediately after `before`.
    /// Dispatch only invokes this against a parent-created, attempt-specific
    /// checkout, so the observed checkout HEAD — never worker-controlled
    /// stdout — is what dispatch reads the commit from.
    fn is_direct_child(&self, repo: &Path, before: Option<&str>, commit: &str) -> Result<bool>;
    /// The committer email recorded on `commit`. Production workers never use
    /// this observable metadata as authority; it remains for audit evidence
    /// and in-memory tests which cannot model an OS process lineage.
    fn committer_email(&self, repo: &Path, commit: &str) -> Result<Option<String>>;
}

pub(crate) fn run<E: Exec, C: CommitProbe>(
    exec: &E,
    commits: &C,
    request: &DispatchRequest,
    state_dir: &Path,
    timeout: Duration,
) -> Result<DispatchResult> {
    run_with_heartbeat(exec, commits, request, state_dir, timeout, timeout, &mut ())
}

/// Runs a non-mutating attempt (`review`, `consult`, plan's model calls).
/// Mirrors [`run_with_heartbeat`]'s `on_pre_spawn` -> spawn -> `on_spawn`
/// sequencing, including its fail-closed behavior, so a read-only fan-out
/// slot durably records its spawn identity before it can act — without any
/// of the write path's commit-authentication hook-directory machinery,
/// which does not apply here (a read-only worker never commits). `hook_name`
/// identifies the calling slot to `hooks`; this function attaches no
/// meaning of its own to it.
///
/// Output is already captured to `request.stdout_path` / `stderr_path` by
/// the spawned process itself. What this returns is the *result* — status,
/// paths, and byte counts — via [`DispatchResult`], the same shape
/// `run_with_heartbeat` returns, so a caller can classify a read-only
/// attempt exactly as it would a mutating one. `worker_commit` and
/// `authentication_rejection` are always `None`: a read-only attempt never
/// commits, so those fields never apply.
pub(crate) fn run_readonly<E, K>(
    exec: &E,
    request: &SpawnRequest,
    timeout: Duration,
    hook_name: &str,
    hooks: &mut K,
) -> Result<DispatchResult>
where
    E: Exec + ?Sized,
    K: WorkerHooks + ?Sized,
{
    hooks.on_pre_spawn(hook_name)?;
    let mut child = exec.spawn(request)?;
    // Bind the run to this worker's identity before it can meaningfully act.
    // If that durable record fails, tear the worker (and any descendants)
    // down rather than let a worker whose identity we cannot prove keep
    // running unattended.
    if let Err(error) = hooks.on_spawn(child.id()) {
        if terminate_and_reap_best_effort(child.as_mut()) {
            return Err(error);
        }
        return Err(DispatchError::worker_state_uncertain(format!(
            "{error}; spawned worker process group could not be proven quiescent"
        )));
    }
    let process = wait_with_timeout_and_heartbeat(child.as_mut(), timeout, timeout, hooks)?;
    hooks.on_worker_quiescent(hook_name)?;
    let stdout_bytes = file_len(&request.stdout_path)?;
    let stderr_bytes = file_len(&request.stderr_path)?;
    let status = if process.timed_out {
        DispatchStatus::Failed(DispatchFailure::TimedOut)
    } else if process.status.success {
        DispatchStatus::Success
    } else {
        DispatchStatus::Failed(DispatchFailure::ExitNonZero {
            code: process.status.code,
        })
    };
    Ok(DispatchResult {
        status,
        worker_commit: None,
        authentication_rejection: None,
        stdout_path: request.stdout_path.clone(),
        stderr_path: request.stderr_path.clone(),
        stdout_bytes,
        stderr_bytes,
    })
}

pub(crate) fn run_with_heartbeat<E, C, K>(
    exec: &E,
    commits: &C,
    request: &DispatchRequest,
    state_dir: &Path,
    timeout: Duration,
    heartbeat_interval: Duration,
    hooks: &mut K,
) -> Result<DispatchResult>
where
    E: Exec + ?Sized,
    C: CommitProbe + ?Sized,
    K: WorkerHooks + ?Sized,
{
    let (stdout_path, stderr_path) = attempt_log_paths(request, state_dir);
    let attempt_head = commits.head(&request.repo)?;
    if attempt_head != request.before_head {
        return Ok(DispatchResult {
            status: DispatchStatus::Failed(DispatchFailure::UnauthenticatedCommit),
            worker_commit: None,
            authentication_rejection: Some(
                CommitAuthenticationRejection::CheckoutChangedBeforeSpawn,
            ),
            stdout_path,
            stderr_path,
            stdout_bytes: 0,
            stderr_bytes: 0,
        });
    }

    let hook_name = authenticated_commit_hook_name(&request.attempt_identity);
    let registration = hooks.on_pre_spawn(&hook_name)?;
    cleanup_stale_authenticated_commit_hooks(
        state_dir,
        registration
            .as_ref()
            .and_then(|registration| registration.superseded_hook.as_deref()),
    )?;
    let hook_dir = prepare_authenticated_commit_hook(
        state_dir,
        &hook_name,
        registration
            .as_ref()
            .map(|registration| registration.run_id.as_str()),
    )?;
    let spawn = spawn_request_with_hook(request, state_dir, &hook_dir)?;
    let mut child = exec.spawn(&spawn)?;
    #[cfg(test)]
    let requires_kernel_authentication = child.id().is_some();
    #[cfg(not(test))]
    let requires_kernel_authentication = true;
    // Bind the run to this worker's process group before it can meaningfully
    // mutate the repository. If that durable record fails, tear the worker
    // (and any descendants) down rather than let a worker whose identity we
    // cannot prove keep running unattended.
    if let Err(error) = hooks.on_spawn(child.id()) {
        if terminate_and_reap_best_effort(child.as_mut()) {
            return Err(error);
        }
        return Err(DispatchError::worker_state_uncertain(format!(
            "{error}; spawned worker process group could not be proven quiescent"
        )));
    }
    let process =
        wait_with_timeout_and_heartbeat(child.as_mut(), timeout, heartbeat_interval, hooks)?;
    hooks.on_worker_quiescent(&hook_name)?;
    let _ = fs::remove_dir_all(&hook_dir);
    let receipt_evidence = child.commit_receipt_evidence();
    let stdout_bytes = file_len(&spawn.stdout_path)?;
    let stderr_bytes = file_len(&spawn.stderr_path)?;
    let authentication = CommitAuthentication {
        attempt_identity: &request.attempt_identity,
        receipt_evidence: &receipt_evidence,
        requires_kernel_authentication,
    };
    let (status, worker_commit, authentication_rejection) = classify(
        process,
        stdout_bytes,
        request.before_head.as_deref(),
        commits,
        &request.repo,
        authentication,
    )?;

    Ok(DispatchResult {
        status,
        worker_commit,
        authentication_rejection,
        stdout_path: spawn.stdout_path,
        stderr_path: spawn.stderr_path,
        stdout_bytes,
        stderr_bytes,
    })
}

fn attempt_log_paths(request: &DispatchRequest, state_dir: &Path) -> (PathBuf, PathBuf) {
    let directory = state_dir
        .join("logs")
        .join(&request.cycle_id)
        .join(&request.bead_id);
    (
        directory.join(format!("{}.out", request.attempt_id)),
        directory.join(format!("{}.err", request.attempt_id)),
    )
}

fn spawn_request(request: &DispatchRequest, state_dir: &Path) -> Result<SpawnRequest> {
    let hook_name = authenticated_commit_hook_name(&request.attempt_identity);
    let hook_dir = prepare_authenticated_commit_hook(state_dir, &hook_name, None)?;
    spawn_request_with_hook(request, state_dir, &hook_dir)
}

#[expect(
    clippy::too_many_lines,
    reason = "spawn construction is a linear fail-closed assembly of argv, audit paths, and controls"
)]
fn spawn_request_with_hook(
    request: &DispatchRequest,
    state_dir: &Path,
    hook_dir: &Path,
) -> Result<SpawnRequest> {
    let (stdout_path, stderr_path) = attempt_log_paths(request, state_dir);
    let attempt_log_dir = stdout_path
        .parent()
        .ok_or_else(|| DispatchError::new("worker log path has no parent"))?;
    fs::create_dir_all(attempt_log_dir).map_err(|error| {
        DispatchError::new(format!(
            "failed to create attempt log dir {}: {error}",
            attempt_log_dir.display()
        ))
    })?;
    File::create(&stdout_path).map_err(|e| {
        DispatchError::new(format!(
            "failed to create stdout log {}: {e}",
            stdout_path.display()
        ))
    })?;
    File::create(&stderr_path).map_err(|e| {
        DispatchError::new(format!(
            "failed to create stderr log {}: {e}",
            stderr_path.display()
        ))
    })?;

    let mut argv = argv_for_backend(
        request.backend,
        &request.dispatch_id,
        request.reasoning_effort,
        &request.prompt,
        &request.repo,
    )?;
    // The outer Seatbelt profile is the single filesystem authority for a
    // worker. Disable a harness's nested sandbox only when that parent-owned
    // profile is actually present; a missing profile must fail safe.
    if request.sandbox_profile.is_some() {
        match request.backend {
            Backend::Codex => {
                argv.insert(2, "--dangerously-bypass-approvals-and-sandbox".to_string());
            }
            Backend::Claude => argv.push("--dangerously-skip-permissions".to_string()),
            Backend::Pi | Backend::Omp | Backend::Agy => {}
        }
    }
    let receipt_socket = commit_receipt_socket_path(&request.attempt_identity);
    if request.sandbox_profile.is_some() && request.worker_runtime_dir.is_none() {
        return Err(DispatchError::new(
            "worker sandbox mutation containment requires a per-attempt runtime directory",
        ));
    }
    // Git identity remains useful audit evidence. The socket receipt is the
    // authority: its peer pid is authenticated by the kernel.
    let mut env = vec![
        (
            "GIT_AUTHOR_NAME".to_string(),
            ATTEMPT_IDENTITY_NAME.to_string(),
        ),
        (
            "GIT_AUTHOR_EMAIL".to_string(),
            request.attempt_identity.clone(),
        ),
        (
            "GIT_COMMITTER_NAME".to_string(),
            ATTEMPT_IDENTITY_NAME.to_string(),
        ),
        (
            "GIT_COMMITTER_EMAIL".to_string(),
            request.attempt_identity.clone(),
        ),
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        ("GIT_CONFIG_KEY_0".to_string(), "core.hooksPath".to_string()),
        (
            "GIT_CONFIG_VALUE_0".to_string(),
            hook_dir.display().to_string(),
        ),
        (
            "UNDERTAKE_COMMIT_RECEIPT_SOCKET".to_string(),
            receipt_socket.display().to_string(),
        ),
    ];
    if let Some(runtime) = &request.worker_runtime_dir {
        let runtime = fs::canonicalize(runtime).map_err(|error| {
            DispatchError::new(format!(
                "canonicalize per-attempt worker runtime {}: {error}",
                runtime.display()
            ))
        })?;
        let tmp = canonical_worker_runtime_child(&runtime, "tmp")?;
        let cache = canonical_worker_runtime_child(&runtime, "cache")?;
        let config = canonical_worker_runtime_child(&runtime, "config")?;
        let data = canonical_worker_runtime_child(&runtime, "data")?;
        let state = canonical_worker_runtime_child(&runtime, "state")?;
        let tmp = tmp.display().to_string();
        env.extend([
            ("TMPDIR".to_string(), tmp.clone()),
            ("TMP".to_string(), tmp.clone()),
            ("TEMP".to_string(), tmp),
            ("XDG_CACHE_HOME".to_string(), cache.display().to_string()),
            ("XDG_CONFIG_HOME".to_string(), config.display().to_string()),
            ("XDG_DATA_HOME".to_string(), data.display().to_string()),
            ("XDG_STATE_HOME".to_string(), state.display().to_string()),
        ]);
    }

    Ok(SpawnRequest {
        argv,
        cwd: request.repo.clone(),
        env,
        stdin: StdinMode::Null,
        sandbox_profile: request.sandbox_profile.clone(),
        worker_resource_limits: Some(request.worker_resource_limits),
        commit_receipt_socket: Some(receipt_socket),
        stdout_path,
        stderr_path,
    })
}

fn canonical_worker_runtime_child(runtime: &Path, name: &str) -> Result<PathBuf> {
    let child = fs::canonicalize(runtime.join(name)).map_err(|error| {
        DispatchError::new(format!(
            "canonicalize worker runtime {name} under {}: {error}",
            runtime.display()
        ))
    })?;
    if !child.starts_with(runtime) {
        return Err(DispatchError::new(format!(
            "worker runtime {name} escapes {}",
            runtime.display()
        )));
    }
    Ok(child)
}

pub(crate) fn argv_for_backend(
    backend: Backend,
    dispatch_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    prompt: &str,
    repo: &Path,
) -> Result<Vec<String>> {
    Ok(match backend {
        Backend::Pi => strings([
            "pi",
            "--model",
            dispatch_id,
            "--thinking",
            PI_THINKING,
            "--approve",
            "-p",
            prompt,
        ]),
        Backend::Omp => {
            let effort = reasoning_effort.ok_or_else(|| {
                DispatchError::new("OMP dispatch requires an explicit reasoning_effort")
            })?;
            vec![
                "omp".to_string(),
                "--model".to_string(),
                dispatch_id.to_string(),
                "--thinking".to_string(),
                effort.as_str().to_string(),
                "--auto-approve".to_string(),
                "--no-session".to_string(),
                "-p".to_string(),
                prompt.to_string(),
            ]
        }
        Backend::Codex => {
            let effort = reasoning_effort.ok_or_else(|| {
                DispatchError::new("Codex dispatch requires an explicit reasoning_effort")
            })?;
            vec![
                "codex".to_string(),
                "exec".to_string(),
                "--model".to_string(),
                dispatch_id.to_string(),
                "--config".to_string(),
                format!("model_reasoning_effort=\"{}\"", effort.as_str()),
                prompt.to_string(),
            ]
        }
        Backend::Agy => vec![
            "agy".to_string(),
            "-p".to_string(),
            prompt.to_string(),
            "--add-dir".to_string(),
            repo.display().to_string(),
            "--model".to_string(),
            dispatch_id.to_string(),
            "--dangerously-skip-permissions".to_string(),
        ],
        Backend::Claude => strings(["claude", "-p", prompt, "--model", dispatch_id]),
    })
}

pub(crate) fn readonly_argv_for_backend(
    backend: Backend,
    dispatch_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    prompt: &str,
    state_dir: &Path,
) -> Result<Vec<String>> {
    Ok(match backend {
        Backend::Pi => strings([
            "pi",
            "--model",
            dispatch_id,
            "--thinking",
            PI_THINKING,
            "--no-tools",
            "-p",
            prompt,
        ]),
        Backend::Omp => {
            let effort = reasoning_effort.ok_or_else(|| {
                DispatchError::new("OMP dispatch requires an explicit reasoning_effort")
            })?;
            vec![
                "omp".to_string(),
                "--model".to_string(),
                dispatch_id.to_string(),
                "--thinking".to_string(),
                effort.as_str().to_string(),
                "--no-tools".to_string(),
                "--no-session".to_string(),
                "-p".to_string(),
                prompt.to_string(),
            ]
        }
        Backend::Codex => {
            let effort = reasoning_effort.ok_or_else(|| {
                DispatchError::new("Codex dispatch requires an explicit reasoning_effort")
            })?;
            vec![
                "codex".to_string(),
                "exec".to_string(),
                "--model".to_string(),
                dispatch_id.to_string(),
                "--config".to_string(),
                format!("model_reasoning_effort=\"{}\"", effort.as_str()),
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--skip-git-repo-check".to_string(),
                prompt.to_string(),
            ]
        }
        Backend::Agy => vec![
            "agy".to_string(),
            "-p".to_string(),
            prompt.to_string(),
            "--add-dir".to_string(),
            state_dir.display().to_string(),
            "--model".to_string(),
            dispatch_id.to_string(),
            "--mode".to_string(),
            "plan".to_string(),
            "--sandbox".to_string(),
        ],
        Backend::Claude => strings([
            "claude",
            "--safe-mode",
            "-p",
            prompt,
            "--model",
            dispatch_id,
            "--permission-mode",
            "plan",
            "--tools",
            "",
        ]),
    })
}

fn strings<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.into_iter().map(str::to_string).collect()
}

#[derive(Debug, Clone, Copy)]
struct ProcessRun {
    status: ProcessStatus,
    timed_out: bool,
}

fn wait_with_timeout_and_heartbeat<K>(
    child: &mut dyn ChildProcess,
    timeout: Duration,
    heartbeat_interval: Duration,
    hooks: &mut K,
) -> Result<ProcessRun>
where
    K: WorkerHooks + ?Sized,
{
    let mut elapsed = Duration::ZERO;
    let heartbeat_interval = if heartbeat_interval.is_zero() {
        WAIT_POLL
    } else {
        heartbeat_interval
    };

    loop {
        if elapsed >= timeout {
            break;
        }
        let wait = timeout.saturating_sub(elapsed).min(heartbeat_interval);
        let status = match child.wait_for(wait) {
            Ok(status) => status,
            Err(error) => {
                // A poll/wait error here (e.g. the OS call itself failing)
                // must never be mistaken for "the worker is done" — the
                // process, and any descendants in its group, could still be
                // running and writing to the repository. Terminate and reap
                // the whole group before propagating so no orphaned writer
                // can outlive the `dispatch_error` this returns.
                if terminate_and_reap_best_effort(child) {
                    return Err(error);
                }
                return Err(DispatchError::worker_state_uncertain(format!(
                    "{error}; worker process group could not be proven quiescent"
                )));
            }
        };
        if let Some(status) = status {
            ensure_child_worker_quiescent(child)?;
            return Ok(ProcessRun {
                status,
                timed_out: false,
            });
        }
        elapsed = elapsed.saturating_add(wait);
        if let Err(error) = hooks.on_heartbeat(elapsed) {
            // Same reasoning as above: a heartbeat failure (e.g. the live
            // report patch call erroring) must not leave the worker running
            // unattended after this function returns an error.
            if terminate_and_reap_best_effort(child) {
                return Err(error);
            }
            return Err(DispatchError::worker_state_uncertain(format!(
                "{error}; worker process group could not be proven quiescent"
            )));
        }
    }

    let _ = child.terminate();
    if let Ok(Some(status)) = child.wait_for(KILL_GRACE) {
        ensure_child_worker_quiescent(child)?;
        return Ok(ProcessRun {
            status,
            timed_out: true,
        });
    }

    let _ = child.kill();
    let status = child.wait()?;
    ensure_child_worker_quiescent(child)?;
    Ok(ProcessRun {
        status,
        timed_out: true,
    })
}

/// Escalates from a graceful signal to a hard kill and reaps the child,
/// swallowing every intermediate failure so a failure to signal (or to
/// observe the grace-period exit) never skips the harder escalation that
/// follows it. Used only on an already-erroring path, where the caller is
/// about to propagate a different error and cannot usefully report this
/// one too — an orphaned worker that keeps writing after Undertake has
/// moved on is worse than losing a diagnostic about noisy termination.
fn terminate_and_reap_best_effort(child: &mut dyn ChildProcess) -> bool {
    let _ = child.terminate();
    let _ = child.wait_for(KILL_GRACE);
    let _ = child.kill();
    let _ = child.wait();
    child.ensure_worker_quiescent().is_ok()
}

fn ensure_child_worker_quiescent(child: &mut dyn ChildProcess) -> Result<()> {
    child.ensure_worker_quiescent().map_err(|error| {
        DispatchError::worker_state_uncertain(format!(
            "worker process-group quiescence could not be proven: {error}"
        ))
    })
}

#[derive(Clone, Copy)]
struct CommitAuthentication<'a> {
    attempt_identity: &'a str,
    receipt_evidence: &'a CommitReceiptEvidence,
    requires_kernel_authentication: bool,
}

fn classify<C: CommitProbe + ?Sized>(
    process: ProcessRun,
    stdout_bytes: u64,
    before_head: Option<&str>,
    commits: &C,
    repo: &Path,
    authentication: CommitAuthentication<'_>,
) -> Result<(
    DispatchStatus,
    Option<String>,
    Option<CommitAuthenticationRejection>,
)> {
    if process.timed_out {
        return Ok((
            DispatchStatus::Failed(DispatchFailure::TimedOut),
            None,
            None,
        ));
    }
    if !process.status.success {
        return Ok((
            DispatchStatus::Failed(DispatchFailure::ExitNonZero {
                code: process.status.code,
            }),
            None,
            None,
        ));
    }

    let after_head = commits.head(repo)?;
    if after_head.as_deref() != before_head {
        // A new, clean, direct-child commit is necessary but not sufficient.
        // A real worker must have reported this exact hash through the
        // post-commit socket while the kernel still proved that hook client
        // was in the current worker's process lineage. Observable environment
        // identity is retained only for in-memory test doubles, which have no
        // OS pid and therefore cannot exercise the production boundary.
        let Some(commit) = after_head.as_deref() else {
            return Ok((
                DispatchStatus::Failed(DispatchFailure::UnauthenticatedCommit),
                None,
                Some(CommitAuthenticationRejection::CheckoutHeadMissing),
            ));
        };
        if !commits.is_direct_child(repo, before_head, commit)? {
            return Ok((
                DispatchStatus::Failed(DispatchFailure::UnauthenticatedCommit),
                None,
                Some(CommitAuthenticationRejection::CheckoutHeadNotDirectChild),
            ));
        }
        if !commits.is_clean(repo)? {
            return Ok((
                DispatchStatus::Failed(DispatchFailure::UnauthenticatedCommit),
                None,
                Some(CommitAuthenticationRejection::CheckoutDirty),
            ));
        }
        let rejection = if authentication.requires_kernel_authentication {
            authentication.receipt_evidence.rejection_for(commit)
        } else if commits.committer_email(repo, commit)?.as_deref()
            == Some(authentication.attempt_identity)
        {
            None
        } else {
            Some(CommitAuthenticationRejection::AuditIdentityMismatched)
        };
        if rejection.is_none() {
            return Ok((DispatchStatus::Success, after_head, None));
        }
        return Ok((
            DispatchStatus::Failed(DispatchFailure::UnauthenticatedCommit),
            None,
            rejection,
        ));
    }
    if stdout_bytes == 0 {
        Ok((
            DispatchStatus::Failed(DispatchFailure::BackendFlakeZeroStdoutNoCommit),
            None,
            None,
        ))
    } else {
        Ok((
            DispatchStatus::Failed(DispatchFailure::NoNewCommit),
            None,
            None,
        ))
    }
}

fn file_len(path: &Path) -> Result<u64> {
    fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| DispatchError::new(format!("failed to stat {}: {e}", path.display())))
}

fn commit_receipt_socket_path(attempt_identity: &str) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);
    let mut digest = Sha256::new();
    digest.update(attempt_identity.as_bytes());
    digest.update(NEXT_SOCKET.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let mut suffix = String::with_capacity(16);
    for byte in digest.finalize().iter().take(8) {
        let _ = write!(suffix, "{byte:02x}");
    }
    std::env::temp_dir().join(format!(
        "undertake-receipt-{}-{suffix}.sock",
        std::process::id()
    ))
}

const HOOK_OWNER_FILE: &str = ".run-id";
const HOOK_CLEANUP_SCAN_LIMIT: usize = 32;
const HOOK_CLEANUP_REMOVE_LIMIT: usize = 8;

fn authenticated_commit_hook_name(attempt_identity: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;

    let mut digest = Sha256::new();
    digest.update(attempt_identity.as_bytes());
    let mut name = String::with_capacity(32);
    for byte in digest.finalize().iter().take(16) {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

fn prepare_authenticated_commit_hook(
    state_dir: &Path,
    hook_name: &str,
    owner_run_id: Option<&str>,
) -> Result<PathBuf> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    if !valid_authenticated_commit_hook_name(hook_name) {
        return Err(DispatchError::new(format!(
            "invalid authenticated commit hook name {hook_name:?}"
        )));
    }
    let hook_dir = state_dir.join("worker-commit-hooks").join(hook_name);
    fs::create_dir_all(&hook_dir).map_err(|error| {
        DispatchError::new(format!(
            "create authenticated commit hook directory {}: {error}",
            hook_dir.display()
        ))
    })?;
    let hook = hook_dir.join("post-commit");
    fs::write(
        &hook,
        br#"#!/bin/sh
commit=$(/usr/bin/git rev-parse --verify HEAD) || exit 1
reply=$(printf '%s\n' "$commit" | /usr/bin/nc -w 3 -U "$UNDERTAKE_COMMIT_RECEIPT_SOCKET" 2>/dev/null) || exit 1
[ "$reply" = "ok" ]
"#,
    )
    .map_err(|error| {
        DispatchError::new(format!(
            "write authenticated commit hook {}: {error}",
            hook.display()
        ))
    })?;
    #[cfg(unix)]
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).map_err(|error| {
        DispatchError::new(format!(
            "make authenticated commit hook executable {}: {error}",
            hook.display()
        ))
    })?;
    if let Some(run_id) = owner_run_id {
        let mut owner = run_id.as_bytes().to_vec();
        owner.push(b'\n');
        crate::run::durable_atomic_replace(&hook_dir.join(HOOK_OWNER_FILE), &owner).map_err(
            |error| {
                DispatchError::new(format!(
                    "persist authenticated commit hook owner {}: {error}",
                    hook_dir.display()
                ))
            },
        )?;
    }
    Ok(hook_dir)
}

fn valid_authenticated_commit_hook_name(name: &str) -> bool {
    name.len() == 32
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct HookCleanupStats {
    scanned: usize,
    removed: usize,
}

fn cleanup_stale_authenticated_commit_hooks(
    state_dir: &Path,
    superseded_hook: Option<&str>,
) -> Result<HookCleanupStats> {
    let root = state_dir.join("worker-commit-hooks");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HookCleanupStats::default());
        }
        Err(error) => {
            return Err(DispatchError::new(format!(
                "read authenticated commit hook root {}: {error}",
                root.display()
            )));
        }
    };
    let mut stats = HookCleanupStats::default();
    if let Some(name) = superseded_hook {
        stats.scanned += 1;
        if remove_stale_authenticated_commit_hook(state_dir, &root.join(name), name) {
            stats.removed += 1;
        }
    }
    for entry in entries.take(HOOK_CLEANUP_SCAN_LIMIT.saturating_sub(stats.scanned)) {
        let Ok(entry) = entry else {
            stats.scanned += 1;
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            stats.scanned += 1;
            continue;
        };
        if superseded_hook == Some(name) {
            continue;
        }
        stats.scanned += 1;
        if stats.removed < HOOK_CLEANUP_REMOVE_LIMIT
            && remove_stale_authenticated_commit_hook(state_dir, &entry.path(), name)
        {
            stats.removed += 1;
        }
    }
    Ok(stats)
}

fn remove_stale_authenticated_commit_hook(
    state_dir: &Path,
    hook_dir: &Path,
    hook_name: &str,
) -> bool {
    if !valid_authenticated_commit_hook_name(hook_name)
        || !matches!(
            fs::symlink_metadata(hook_dir),
            Ok(metadata) if metadata.file_type().is_dir()
        )
    {
        return false;
    }
    let Ok(owner) = fs::read_to_string(hook_dir.join(HOOK_OWNER_FILE)) else {
        return false;
    };
    match crate::run::worker_commit_hook_is_current(state_dir, owner.trim(), hook_name) {
        Ok(true) | Err(_) => false,
        Ok(false) => fs::remove_dir_all(hook_dir).is_ok(),
    }
}

#[cfg(unix)]
struct CommitReceiptBroker {
    listener: UnixListener,
    socket_path: PathBuf,
    worker_root: u32,
    accepted: Vec<String>,
    stale_lineage: usize,
    invalid: usize,
}

#[cfg(unix)]
impl CommitReceiptBroker {
    fn bind(socket_path: &Path) -> Result<Self> {
        match fs::symlink_metadata(socket_path) {
            Ok(_) => {
                return Err(DispatchError::new(format!(
                    "commit receipt socket already exists: {}",
                    socket_path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DispatchError::new(format!(
                    "inspect commit receipt socket {}: {error}",
                    socket_path.display()
                )));
            }
        }
        let listener = UnixListener::bind(socket_path).map_err(|error| {
            DispatchError::new(format!(
                "bind commit receipt socket {}: {error}",
                socket_path.display()
            ))
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            DispatchError::new(format!(
                "make commit receipt socket nonblocking {}: {error}",
                socket_path.display()
            ))
        })?;
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
            worker_root: 0,
            accepted: Vec::new(),
            stale_lineage: 0,
            invalid: 0,
        })
    }

    fn set_worker_root(&mut self, worker_root: u32) {
        self.worker_root = worker_root;
    }

    fn poll(&mut self) -> Result<()> {
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => {
                    return Err(DispatchError::new(format!(
                        "accept authenticated commit receipt: {error}"
                    )));
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .map_err(|error| DispatchError::new(format!("set receipt timeout: {error}")))?;
            let peer = peer_pid(&stream)?;
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .map_err(|error| DispatchError::new(format!("read commit receipt: {error}")))?;
            let commit = line.trim();
            if !valid_git_oid(commit) {
                self.invalid += 1;
                let _ = stream.write_all(b"denied\n");
            } else if process_in_live_worker_lineage(peer, self.worker_root)? {
                self.accepted.push(commit.to_string());
                stream
                    .write_all(b"ok\n")
                    .map_err(|error| DispatchError::new(format!("ack commit receipt: {error}")))?;
            } else {
                self.stale_lineage += 1;
                let _ = stream.write_all(b"denied\n");
            }
        }
    }

    fn evidence(&self) -> CommitReceiptEvidence {
        CommitReceiptEvidence {
            accepted: self.accepted.clone(),
            stale_lineage: self.stale_lineage,
            invalid: self.invalid,
        }
    }
}

#[cfg(unix)]
impl Drop for CommitReceiptBroker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn valid_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> Result<u32> {
    let pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid)
        .map_err(|error| {
            DispatchError::worker_state_uncertain(format!(
                "authenticate commit receipt peer pid: {error}"
            ))
        })?;
    u32::try_from(pid)
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| {
            DispatchError::worker_state_uncertain("commit receipt peer returned an invalid pid")
        })
}

#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Result<u32> {
    let credentials =
        nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials).map_err(
            |error| {
                DispatchError::worker_state_uncertain(format!(
                    "authenticate commit receipt peer credentials: {error}"
                ))
            },
        )?;
    u32::try_from(credentials.pid())
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| {
            DispatchError::worker_state_uncertain("commit receipt peer returned an invalid pid")
        })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_pid(_stream: &UnixStream) -> Result<u32> {
    Err(DispatchError::worker_state_uncertain(
        "kernel peer-pid authentication is unsupported on this platform",
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct KernelProcessIdentity {
    pid: u32,
    parent: Option<u32>,
    start_time: u64,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, PartialEq, Eq)]
struct KernelLineageSnapshot {
    worker: KernelProcessIdentity,
    peer_lineage: Vec<KernelProcessIdentity>,
    reaches_worker: bool,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_in_live_worker_lineage(peer: u32, root: u32) -> Result<bool> {
    let root_pid = nix::unistd::Pid::from_raw(i32::try_from(root).map_err(|error| {
        DispatchError::worker_state_uncertain(format!("convert worker lineage pid: {error}"))
    })?);
    let worker_session = nix::unistd::getsid(Some(root_pid)).map_err(|error| {
        DispatchError::worker_state_uncertain(format!("authenticate live worker session: {error}"))
    })?;
    if worker_session != root_pid {
        // The broker can receive a forged connection in the short interval
        // before the worker wrapper completes `setsid`. Deny that peer; a
        // legitimate hook can authenticate only after the boundary exists.
        return Ok(false);
    }

    let first = kernel_lineage_snapshot(peer, root)?;
    let second = kernel_lineage_snapshot(peer, root)?;
    let worker_session_after = nix::unistd::getsid(Some(root_pid)).map_err(|error| {
        DispatchError::worker_state_uncertain(format!("recheck live worker session: {error}"))
    })?;
    if worker_session_after != root_pid || first != second {
        return Err(DispatchError::worker_state_uncertain(
            "commit receipt process ancestry changed while it was authenticated",
        ));
    }
    Ok(first.reaches_worker)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn kernel_lineage_snapshot(peer: u32, root: u32) -> Result<KernelLineageSnapshot> {
    use std::collections::HashSet;

    let worker = kernel_process_identity(root)?;
    let mut peer_lineage = Vec::new();
    let mut seen = HashSet::new();
    let mut current = peer;

    loop {
        if !seen.insert(current) {
            return Err(DispatchError::worker_state_uncertain(format!(
                "commit receipt ancestry contains a cycle at pid {current}"
            )));
        }
        let identity = if current == root {
            worker.clone()
        } else {
            kernel_process_identity(current)?
        };
        let parent = identity.parent;
        peer_lineage.push(identity);
        if current == root {
            return Ok(KernelLineageSnapshot {
                worker,
                peer_lineage,
                reaches_worker: true,
            });
        }
        if current == 1 || parent == Some(0) || (parent == Some(1) && root != 1) {
            return Ok(KernelLineageSnapshot {
                worker,
                peer_lineage,
                reaches_worker: false,
            });
        }
        current = parent.ok_or_else(|| {
            DispatchError::worker_state_uncertain(format!(
                "commit receipt ancestry pid {current} had no kernel-reported parent"
            ))
        })?;
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn kernel_process_identity(pid: u32) -> Result<KernelProcessIdentity> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

    let mut system = System::new();
    let target = [Pid::from_u32(pid)];
    let updated = system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&target),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    let process = system.process(target[0]).ok_or_else(|| {
        DispatchError::worker_state_uncertain(format!(
            "pid {pid} was absent from a targeted kernel process snapshot"
        ))
    })?;
    if updated != 1
        || matches!(
            process.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        )
    {
        return Err(DispatchError::worker_state_uncertain(format!(
            "pid {pid} was not unambiguously live while authenticating a commit receipt"
        )));
    }
    let start_time = process.start_time();
    if start_time == 0 {
        return Err(DispatchError::worker_state_uncertain(format!(
            "kernel process identity for pid {pid} had no start time"
        )));
    }
    Ok(KernelProcessIdentity {
        pid,
        parent: process.parent().map(sysinfo::Pid::as_u32),
        start_time,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_in_live_worker_lineage(_peer: u32, _root: u32) -> Result<bool> {
    Err(DispatchError::worker_state_uncertain(
        "kernel process ancestry authentication is unsupported on this platform",
    ))
}

const WORKER_LINEAGE_LEASE_FILE: &str = "worker-lineage.fifo";

/// The pre-isolation FIFO path retained only to recover runs created by older
/// Undertake versions. New workers never inherit this descriptor.
pub(crate) fn worker_lineage_lease_path(run_dir: &Path) -> PathBuf {
    run_dir.join(WORKER_LINEAGE_LEASE_FILE)
}

#[cfg(unix)]
pub(crate) fn prepare_worker_lineage_lease(path: &Path) -> Result<()> {
    use std::io::ErrorKind;

    match fs::symlink_metadata(path) {
        Ok(_) => {
            validate_worker_lineage_fifo(path)?;
            if worker_lineage_active(path)? {
                return Err(DispatchError::worker_state_uncertain(format!(
                    "earlier worker lineage still holds {}",
                    path.display()
                )));
            }
            fs::remove_file(path).map_err(|error| {
                DispatchError::new(format!(
                    "remove inactive worker-lineage lease {}: {error}",
                    path.display()
                ))
            })?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DispatchError::new(format!(
                "inspect worker-lineage lease {}: {error}",
                path.display()
            )));
        }
    }

    let parent = path.parent().ok_or_else(|| {
        DispatchError::new(format!(
            "worker-lineage lease has no parent: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        DispatchError::new(format!(
            "create worker-lineage lease directory {}: {error}",
            parent.display()
        ))
    })?;
    let mut command = Command::new("mkfifo");
    command.arg(path);
    let output = run_bounded_command(&mut command).map_err(|error| {
        DispatchError::new(format!(
            "run mkfifo for worker-lineage lease {}: {error}",
            path.display()
        ))
    })?;
    if !output.status.success() {
        return Err(DispatchError::new(format!(
            "mkfifo worker-lineage lease {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    validate_worker_lineage_fifo(path)
}

#[cfg(not(unix))]
pub(crate) fn prepare_worker_lineage_lease(_path: &Path) -> Result<()> {
    Err(DispatchError::new(
        "worker-lineage leases are only implemented on Unix",
    ))
}

#[cfg(unix)]
fn validate_worker_lineage_fifo(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt as _;

    let metadata = fs::symlink_metadata(path).map_err(|error| {
        DispatchError::new(format!(
            "inspect worker-lineage lease {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_fifo() {
        Ok(())
    } else {
        Err(DispatchError::worker_state_uncertain(format!(
            "worker-lineage lease is not a FIFO: {}",
            path.display()
        )))
    }
}

/// Returns whether any process still holds the inherited read end of a
/// worker's durable lineage FIFO. Opening the write end nonblocking succeeds
/// exactly while at least one reader survives; `ENXIO` proves there are none.
#[cfg(unix)]
pub(crate) fn worker_lineage_active(path: &Path) -> Result<bool> {
    use std::os::unix::fs::OpenOptionsExt as _;

    validate_worker_lineage_fifo(path)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(_) => Ok(true),
        Err(error) if error.raw_os_error() == Some(libc::ENXIO) => Ok(false),
        Err(error) => Err(DispatchError::worker_state_uncertain(format!(
            "probe worker-lineage lease {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(not(unix))]
pub(crate) fn worker_lineage_active(_path: &Path) -> Result<bool> {
    Err(DispatchError::worker_state_uncertain(
        "worker-lineage lease probes are only implemented on Unix",
    ))
}

fn stdin_for_mode(mode: &StdinMode) -> Stdio {
    match mode {
        StdinMode::Null => Stdio::null(),
    }
}

#[derive(Debug)]
enum BoundedCommandErrorKind {
    Spawn(std::io::Error),
    Setup {
        resource: &'static str,
        source: std::io::Error,
    },
    Poll(std::io::Error),
    TimedOut(Duration),
    OutputLimit {
        streams: &'static str,
        limit: usize,
    },
    StateUncertain(String),
}

/// Failure from a helper subprocess whose captured evidence remains bounded.
#[derive(Debug)]
pub(crate) struct BoundedCommandError {
    kind: BoundedCommandErrorKind,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl BoundedCommandError {
    fn new(kind: BoundedCommandErrorKind) -> Self {
        Self {
            kind,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    fn with_capture(kind: BoundedCommandErrorKind, capture: HelperCapture) -> Self {
        Self {
            kind,
            stdout: capture.stdout,
            stderr: capture.stderr,
            stdout_truncated: capture.stdout_truncated,
            stderr_truncated: capture.stderr_truncated,
        }
    }

    pub(crate) const fn is_timeout(&self) -> bool {
        matches!(&self.kind, BoundedCommandErrorKind::TimedOut(_))
    }

    pub(crate) const fn leaves_process_state_uncertain(&self) -> bool {
        matches!(&self.kind, BoundedCommandErrorKind::StateUncertain(_))
    }

    pub(crate) fn spawn_source(&self) -> Option<&std::io::Error> {
        match &self.kind {
            BoundedCommandErrorKind::Spawn(source) => Some(source),
            _ => None,
        }
    }

    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    fn write_evidence(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_stream_evidence(f, "stdout", &self.stdout, self.stdout_truncated)?;
        write_stream_evidence(f, "stderr", &self.stderr, self.stderr_truncated)
    }
}

impl fmt::Display for BoundedCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BoundedCommandErrorKind::Spawn(source) => write!(f, "spawn subprocess: {source}")?,
            BoundedCommandErrorKind::Setup { resource, source } => {
                write!(f, "prepare subprocess {resource}: {source}")?;
            }
            BoundedCommandErrorKind::Poll(source) => {
                write!(f, "poll subprocess: {source}")?;
            }
            BoundedCommandErrorKind::TimedOut(timeout) => {
                write!(
                    f,
                    "subprocess timed out after {} ms and was reaped after TERM/KILL escalation",
                    timeout.as_millis()
                )?;
            }
            BoundedCommandErrorKind::OutputLimit { streams, limit } => {
                write!(
                    f,
                    "subprocess {streams} exceeded the {limit}-byte capture limit"
                )?;
            }
            BoundedCommandErrorKind::StateUncertain(detail) => {
                write!(
                    f,
                    "subprocess state is uncertain after TERM/KILL escalation: {detail}"
                )?;
            }
        }
        self.write_evidence(f)
    }
}

impl std::error::Error for BoundedCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            BoundedCommandErrorKind::Spawn(source)
            | BoundedCommandErrorKind::Setup { source, .. }
            | BoundedCommandErrorKind::Poll(source) => Some(source),
            BoundedCommandErrorKind::TimedOut(_)
            | BoundedCommandErrorKind::OutputLimit { .. }
            | BoundedCommandErrorKind::StateUncertain(_) => None,
        }
    }
}

fn write_stream_evidence(
    f: &mut fmt::Formatter<'_>,
    label: &str,
    bytes: &[u8],
    truncated: bool,
) -> fmt::Result {
    if bytes.is_empty() {
        return Ok(());
    }
    let start = bytes.len().saturating_sub(HELPER_ERROR_EVIDENCE_LIMIT);
    write!(
        f,
        "; {label}: {}",
        String::from_utf8_lossy(&bytes[start..]).trim()
    )?;
    if truncated || start > 0 {
        f.write_str(" [truncated]")?;
    }
    Ok(())
}

#[derive(Debug)]
struct HelperCapture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

struct HelperTempFile {
    file: File,
    path: Option<PathBuf>,
}

impl HelperTempFile {
    fn create(label: &str) -> std::io::Result<Self> {
        for _ in 0..16 {
            let nonce = HELPER_TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "undertake-helper-{}-{timestamp}-{nonce}-{label}",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    #[cfg(unix)]
                    {
                        if let Err(error) = fs::remove_file(&path) {
                            let _ = fs::remove_file(&path);
                            return Err(error);
                        }
                        return Ok(Self { file, path: None });
                    }
                    #[cfg(not(unix))]
                    {
                        return Ok(Self {
                            file,
                            path: Some(path),
                        });
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique helper capture file",
        ))
    }

    fn stdio(&self) -> std::io::Result<Stdio> {
        self.file.try_clone().map(Stdio::from)
    }

    fn stage_input(&mut self, input: &[u8]) -> std::io::Result<()> {
        self.file.write_all(input)?;
        self.file.seek(SeekFrom::Start(0))?;
        Ok(())
    }

    fn read_bounded(&mut self) -> std::io::Result<(Vec<u8>, bool)> {
        self.file.seek(SeekFrom::Start(0))?;
        let read_limit = u64::try_from(HELPER_CAPTURE_LIMIT)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(
            usize::try_from(self.file.metadata()?.len())
                .unwrap_or(HELPER_CAPTURE_LIMIT)
                .min(HELPER_CAPTURE_LIMIT),
        );
        (&mut self.file)
            .take(read_limit)
            .read_to_end(&mut bytes)?;
        let truncated = bytes.len() > HELPER_CAPTURE_LIMIT;
        bytes.truncate(HELPER_CAPTURE_LIMIT);
        Ok((bytes, truncated))
    }
}

impl Drop for HelperTempFile {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

/// Runs a production helper command with null stdin, bounded capture, and a
/// process-group timeout.
pub(crate) fn run_bounded_command(
    command: &mut Command,
) -> std::result::Result<Output, BoundedCommandError> {
    run_bounded_command_with_limits(command, None, HELPER_COMMAND_TIMEOUT, KILL_GRACE)
}

/// Runs a helper with the caller's already-sampled remaining monotonic budget.
///
/// A zero budget refuses the spawn; a live child receives only this timeout
/// before TERM/KILL escalation.
pub(crate) fn run_bounded_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> std::result::Result<Output, BoundedCommandError> {
    if timeout.is_zero() {
        return Err(BoundedCommandError::new(
            BoundedCommandErrorKind::TimedOut(Duration::ZERO),
        ));
    }
    run_bounded_command_with_limits(command, None, timeout, KILL_GRACE)
}

/// Runs a production helper command with file-backed stdin so input and output
/// cannot deadlock on opposing pipe capacity.
pub(crate) fn run_bounded_command_with_input(
    command: &mut Command,
    input: &[u8],
) -> std::result::Result<Output, BoundedCommandError> {
    run_bounded_command_with_limits(command, Some(input), HELPER_COMMAND_TIMEOUT, KILL_GRACE)
}

/// Non-secret remediation guidance shown whenever a backend cannot prove it
/// is ready to authenticate unattended (bd `conductor-5p8`). Names the
/// supported unattended paths only; never echoes probe output, which this
/// classifier must never leak.
const CLAUDE_UNATTENDED_AUTH_GUIDANCE: &str = "unattended Claude dispatch requires either an \
    inference-only token from `claude setup-token` exported as CLAUDE_CODE_OAUTH_TOKEN, or an \
    apiKeyHelper-configured API key; interactive subscription login (`claude /login`) is not \
    usable from a detached background worker";

/// The only backend this classifier currently probes is Claude — the
/// subject of bd `conductor-5p8`'s live incident. The other backends have
/// no unattended-readiness contract defined yet, so they classify `Ready`
/// unconditionally; giving one of them a real probe is a separate,
/// deliberate change, not a side effect of this fix.
pub(crate) fn default_backend_auth_readiness(backend: Backend) -> AuthReadiness {
    match backend {
        Backend::Claude => claude_cli_auth_readiness(),
        Backend::Pi | Backend::Omp | Backend::Agy | Backend::Codex => AuthReadiness::Ready,
    }
}

/// Runs the production Claude auth-readiness probe: bounded, stdin-closed,
/// and process-group reaped via [`run_bounded_command_with_timeout`], so a
/// hang can never block dispatch — it times out and classifies
/// [`AuthReadiness::Unreadable`] instead.
fn claude_cli_auth_readiness() -> AuthReadiness {
    let mut command = Command::new("claude");
    command.args(["--safe-mode", "auth", "status", "--json"]);
    classify_claude_auth_probe(run_bounded_command_with_timeout(
        &mut command,
        CLAUDE_AUTH_PROBE_TIMEOUT,
    ))
}

/// Classifies a completed or failed bounded auth probe. Split from
/// [`claude_cli_auth_readiness`] so tests can feed a synthetic probe result
/// (built from a real, credential-free `sh` subprocess) without depending on
/// the actual `claude` CLI or any real credential state.
fn classify_claude_auth_probe(
    probe: std::result::Result<Output, BoundedCommandError>,
) -> AuthReadiness {
    match probe {
        Ok(output) => classify_claude_auth_output(&output),
        Err(error) => {
            // Never forward `error`'s `Display` verbatim: `BoundedCommandError`
            // embeds captured stdout/stderr evidence, and a credential probe's
            // captured streams are exactly the bytes this classifier must never
            // leak. A timeout classifies `Unreadable`, never `Ready`, so a hang
            // can never be mistaken for an authenticated backend.
            let cause = if error.is_timeout() {
                "the readiness probe did not respond before its bounded timeout"
            } else if error.spawn_source().is_some() {
                "the `claude` CLI could not be started (not installed or not on PATH)"
            } else if error.leaves_process_state_uncertain() {
                "the readiness probe left the helper process state uncertain"
            } else {
                "the readiness probe failed"
            };
            AuthReadiness::Unreadable {
                message: format!("{cause}; {CLAUDE_UNATTENDED_AUTH_GUIDANCE}"),
            }
        }
    }
}

/// Parses `claude auth status --json` evidence into a readiness
/// classification. Only two non-secret fields are ever read —
/// `loggedIn` (bool) and `authMethod` (a short method name, never a
/// credential) — and only `authMethod` is ever echoed back, bounded to a
/// short printable token so no unexpected payload can ride along in it.
fn classify_claude_auth_output(output: &Output) -> AuthReadiness {
    let Ok(parsed) = serde_json::from_slice::<Value>(&output.stdout) else {
        return AuthReadiness::Unreadable {
            message: format!(
                "`claude auth status --json` returned unparseable output; \
                 {CLAUDE_UNATTENDED_AUTH_GUIDANCE}"
            ),
        };
    };
    match parsed.get("loggedIn").and_then(Value::as_bool) {
        Some(true) => AuthReadiness::Ready,
        Some(false) => {
            let method = parsed.get("authMethod").and_then(Value::as_str).filter(|method| {
                method.len() <= 32 && method.chars().all(|c| c.is_ascii_graphic() || c == ' ')
            });
            let observed = method.map_or_else(String::new, |method| format!(" (authMethod={method})"));
            AuthReadiness::NotAuthenticated {
                message: format!(
                    "claude reports no active session{observed}; {CLAUDE_UNATTENDED_AUTH_GUIDANCE}"
                ),
            }
        }
        None => AuthReadiness::Unreadable {
            message: format!(
                "`claude auth status --json` omitted the expected loggedIn field; \
                 {CLAUDE_UNATTENDED_AUTH_GUIDANCE}"
            ),
        },
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the bounded helper lifecycle keeps setup, timeout escalation, reap, and capture \
              ordering explicit in one linear routine"
)]
fn run_bounded_command_with_limits(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    grace: Duration,
) -> std::result::Result<Output, BoundedCommandError> {
    let mut stdout_file = HelperTempFile::create("stdout").map_err(|source| {
        BoundedCommandError::new(BoundedCommandErrorKind::Setup {
            resource: "stdout capture",
            source,
        })
    })?;
    let mut stderr_file = HelperTempFile::create("stderr").map_err(|source| {
        BoundedCommandError::new(BoundedCommandErrorKind::Setup {
            resource: "stderr capture",
            source,
        })
    })?;
    let input_file = input
        .map(|bytes| {
            let mut file = HelperTempFile::create("stdin")?;
            file.stage_input(bytes)?;
            Ok::<_, std::io::Error>(file)
        })
        .transpose()
        .map_err(|source| {
            BoundedCommandError::new(BoundedCommandErrorKind::Setup {
                resource: "stdin",
                source,
            })
        })?;

    let stdin = input_file.as_ref().map_or_else(
        || Ok(Stdio::null()),
        HelperTempFile::stdio,
    );
    command
        .stdin(stdin.map_err(|source| {
            BoundedCommandError::new(BoundedCommandErrorKind::Setup {
                resource: "stdin",
                source,
            })
        })?)
        .stdout(stdout_file.stdio().map_err(|source| {
            BoundedCommandError::new(BoundedCommandErrorKind::Setup {
                resource: "stdout capture",
                source,
            })
        })?)
        .stderr(stderr_file.stdio().map_err(|source| {
            BoundedCommandError::new(BoundedCommandErrorKind::Setup {
                resource: "stderr capture",
                source,
            })
        })?);
    set_own_process_group(command);

    let started = Instant::now();
    let child = command.spawn();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = child
        .map_err(|source| BoundedCommandError::new(BoundedCommandErrorKind::Spawn(source)))?;
    let pgid = child.id();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let status = terminate_helper_group(&mut child, pgid, Some(status), grace)
                    .map_err(|detail| {
                        uncertain_error_with_capture(
                            detail,
                            &mut stdout_file,
                            &mut stderr_file,
                        )
                    })?;
                let capture = read_helper_capture(&mut stdout_file, &mut stderr_file)?;
                return helper_output(status, capture);
            }
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(
                    timeout
                        .saturating_sub(started.elapsed())
                        .min(WAIT_POLL),
                );
            }
            Ok(None) => {
                let outcome = terminate_helper_group(&mut child, pgid, None, grace);
                let capture = match read_helper_capture(&mut stdout_file, &mut stderr_file) {
                    Ok(capture) => capture,
                    Err(error) => {
                        if let Err(detail) = &outcome {
                            return Err(BoundedCommandError::new(
                                BoundedCommandErrorKind::StateUncertain(format!(
                                    "timed out after {} ms; {detail}; \
                                     capture unavailable: {error}",
                                    timeout.as_millis()
                                )),
                            ));
                        }
                        return Err(error);
                    }
                };
                return match outcome {
                    Ok(_) => Err(BoundedCommandError::with_capture(
                        BoundedCommandErrorKind::TimedOut(timeout),
                        capture,
                    )),
                    Err(detail) => Err(BoundedCommandError::with_capture(
                        BoundedCommandErrorKind::StateUncertain(format!(
                            "timed out after {} ms; {detail}",
                            timeout.as_millis()
                        )),
                        capture,
                    )),
                };
            }
            Err(source) => {
                let original = source.to_string();
                let outcome = terminate_helper_group(&mut child, pgid, None, grace);
                let capture = match read_helper_capture(&mut stdout_file, &mut stderr_file) {
                    Ok(capture) => capture,
                    Err(error) => {
                        if let Err(detail) = &outcome {
                            return Err(BoundedCommandError::new(
                                BoundedCommandErrorKind::StateUncertain(format!(
                                    "poll subprocess: {original}; {detail}; \
                                     capture unavailable: {error}"
                                )),
                            ));
                        }
                        return Err(error);
                    }
                };
                return match outcome {
                    Ok(_) => Err(BoundedCommandError::with_capture(
                        BoundedCommandErrorKind::Poll(source),
                        capture,
                    )),
                    Err(detail) => Err(BoundedCommandError::with_capture(
                        BoundedCommandErrorKind::StateUncertain(format!(
                            "poll subprocess: {original}; {detail}"
                        )),
                        capture,
                    )),
                };
            }
        }
    }
}

fn terminate_helper_group(
    child: &mut std::process::Child,
    pgid: u32,
    mut status: Option<ExitStatus>,
    grace: Duration,
) -> std::result::Result<ExitStatus, String> {
    use std::fmt::Write as _;

    if status.is_some() && !helper_group_alive(pgid) {
        return status.ok_or_else(|| "child exited without a reapable status".to_string());
    }

    let term_error = send_signal_to_group(pgid, "-TERM")
        .err()
        .map(|error| error.to_string());
    if helper_reaped_and_quiescent(child, pgid, &mut status, grace).is_ok_and(|done| done) {
        return status.ok_or_else(|| "child exited without a reapable status".to_string());
    }

    let kill_error = send_signal_to_group(pgid, "-KILL")
        .err()
        .map(|error| error.to_string());
    let direct_kill_error = if status.is_none() {
        child.kill().err().map(|error| error.to_string())
    } else {
        None
    };
    let final_wait = helper_reaped_and_quiescent(child, pgid, &mut status, grace);
    if matches!(&final_wait, Ok(true)) {
        return status.ok_or_else(|| "child exited without a reapable status".to_string());
    }

    let mut detail = format!(
        "child_reaped={}, process_group_alive={}",
        status.is_some(),
        helper_group_alive(pgid)
    );
    if let Err(error) = final_wait {
        let _ = write!(detail, ", final poll failed: {error}");
    }
    if let Some(error) = term_error {
        let _ = write!(detail, ", TERM failed: {error}");
    }
    if let Some(error) = kill_error {
        let _ = write!(detail, ", KILL failed: {error}");
    }
    if let Some(error) = direct_kill_error {
        let _ = write!(detail, ", direct KILL failed: {error}");
    }
    Err(detail)
}

fn helper_reaped_and_quiescent(
    child: &mut std::process::Child,
    pgid: u32,
    status: &mut Option<ExitStatus>,
    timeout: Duration,
) -> std::io::Result<bool> {
    let started = Instant::now();
    loop {
        if status.is_none() {
            *status = child.try_wait()?;
        }
        if status.is_some() && !helper_group_alive(pgid) {
            return Ok(true);
        }
        if started.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(
            timeout
                .saturating_sub(started.elapsed())
                .min(WAIT_POLL),
        );
    }
}

#[cfg(unix)]
fn helper_group_alive(pgid: u32) -> bool {
    crate::quarantine::process_group_alive(pgid)
}

#[cfg(not(unix))]
fn helper_group_alive(_pgid: u32) -> bool {
    false
}

fn read_helper_capture(
    stdout_file: &mut HelperTempFile,
    stderr_file: &mut HelperTempFile,
) -> std::result::Result<HelperCapture, BoundedCommandError> {
    let (stdout, stdout_truncated) = stdout_file.read_bounded().map_err(|source| {
        BoundedCommandError::new(BoundedCommandErrorKind::Setup {
            resource: "stdout evidence",
            source,
        })
    })?;
    let (stderr, stderr_truncated) = stderr_file.read_bounded().map_err(|source| {
        BoundedCommandError::new(BoundedCommandErrorKind::Setup {
            resource: "stderr evidence",
            source,
        })
    })?;
    Ok(HelperCapture {
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

fn uncertain_error_with_capture(
    detail: String,
    stdout_file: &mut HelperTempFile,
    stderr_file: &mut HelperTempFile,
) -> BoundedCommandError {
    match read_helper_capture(stdout_file, stderr_file) {
        Ok(capture) => BoundedCommandError::with_capture(
            BoundedCommandErrorKind::StateUncertain(detail),
            capture,
        ),
        Err(error) => BoundedCommandError::new(BoundedCommandErrorKind::StateUncertain(format!(
            "{detail}; capture unavailable: {error}"
        ))),
    }
}

fn helper_output(
    status: ExitStatus,
    capture: HelperCapture,
) -> std::result::Result<Output, BoundedCommandError> {
    let streams = match (capture.stdout_truncated, capture.stderr_truncated) {
        (true, true) => Some("stdout and stderr"),
        (true, false) => Some("stdout"),
        (false, true) => Some("stderr"),
        (false, false) => None,
    };
    if let Some(streams) = streams {
        return Err(BoundedCommandError::with_capture(
            BoundedCommandErrorKind::OutputLimit {
                streams,
                limit: HELPER_CAPTURE_LIMIT,
            },
            capture,
        ));
    }
    Ok(Output {
        status,
        stdout: capture.stdout,
        stderr: capture.stderr,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CommandExec;

const WORKER_SESSION_WRAPPER: &str = r"import os, resource, sys
limits = (
    (resource.RLIMIT_CPU, int(sys.argv[1])),
    (resource.RLIMIT_NPROC, int(sys.argv[2])),
    (resource.RLIMIT_AS, int(sys.argv[3])),
    (resource.RLIMIT_FSIZE, int(sys.argv[4])),
)
for resource_id, value in limits:
    resource.setrlimit(resource_id, (value, value))
os.setsid()
os.execvpe(sys.argv[5], sys.argv[5:], os.environ)
";

impl Exec for CommandExec {
    fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
        let Some((program, args)) = request.argv.split_first() else {
            return Err(DispatchError::new("cannot spawn empty argv"));
        };
        if request.sandbox_profile.is_some() {
            reject_multiply_linked_checkout_files(&request.cwd)?;
        }
        let stdout = File::create(&request.stdout_path).map_err(|e| {
            DispatchError::new(format!(
                "failed to open stdout log {}: {e}",
                request.stdout_path.display()
            ))
        })?;
        let stderr = File::create(&request.stderr_path).map_err(|e| {
            DispatchError::new(format!(
                "failed to open stderr log {}: {e}",
                request.stderr_path.display()
            ))
        })?;
        if (request.commit_receipt_socket.is_some() || request.sandbox_profile.is_some())
            && request.worker_resource_limits.is_none()
        {
            return Err(DispatchError::new(
                "worker isolation requires inherited resource limits before payload exec",
            ));
        }
        let session_isolation = request.worker_resource_limits.is_some();
        let (target_program, target_args) = request.worker_resource_limits.map_or_else(
            || Ok((program.clone(), args.to_vec())),
            |limits| worker_session_command(program, args, limits),
        )?;
        let mut command = if let Some(profile) = &request.sandbox_profile {
            let mut command = Command::new("/usr/bin/sandbox-exec");
            command
                .arg("-f")
                .arg(profile)
                .arg(&target_program)
                .args(&target_args);
            command
        } else {
            let mut command = Command::new(&target_program);
            command.args(&target_args);
            command
        };
        command
            .current_dir(&request.cwd)
            .envs(request.env.iter().map(|(key, value)| (key, value)))
            .stdin(stdin_for_mode(&request.stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if !session_isolation {
            // Non-worker subprocesses still get a dedicated process group.
            set_own_process_group(&mut command);
        }
        #[cfg(unix)]
        let mut receipt_broker = request
            .commit_receipt_socket
            .as_deref()
            .map(CommitReceiptBroker::bind)
            .transpose()?;
        let child = command.spawn().map_err(|e| {
            DispatchError::new(format!(
                "failed to spawn `{}` in {}: {e}",
                request.argv.join(" "),
                request.cwd.display()
            ))
        })?;
        #[cfg(unix)]
        if let Some(broker) = &mut receipt_broker {
            broker.set_worker_root(child.id());
        }
        Ok(Box::new(CommandChild {
            child,
            #[cfg(unix)]
            receipt_broker,
        }))
    }
}

#[cfg(unix)]
fn worker_session_command(
    program: &str,
    args: &[String],
    limits: WorkerResourceLimits,
) -> Result<(String, Vec<String>)> {
    let (same_user_processes, launcher_virtual_memory) = current_process_resource_baseline()?;
    let process_limit = same_user_processes
        .checked_add(limits.process_headroom)
        .ok_or_else(|| DispatchError::new("worker RLIMIT_NPROC overflow"))?;
    // Darwin maps a several-hundred-GiB shared region into every modern
    // process. RLIMIT_AS therefore has to cap growth above the trusted
    // launcher's existing mapping rather than use a small absolute value.
    let address_space_limit = launcher_virtual_memory
        .checked_add(limits.address_space_headroom_bytes)
        .ok_or_else(|| DispatchError::new("worker RLIMIT_AS headroom overflow"))?;
    let mut session_args = Vec::with_capacity(args.len() + 7);
    session_args.extend([
        "-c".to_string(),
        WORKER_SESSION_WRAPPER.to_string(),
        limits.cpu_seconds.to_string(),
        process_limit.to_string(),
        address_space_limit.to_string(),
        limits.file_size_bytes.to_string(),
        program.to_string(),
    ]);
    session_args.extend(args.iter().cloned());
    Ok(("/usr/bin/python3".to_string(), session_args))
}

#[cfg(unix)]
fn current_process_resource_baseline() -> Result<(u64, u64)> {
    use sysinfo::System;

    let system = System::new_all();
    let current_pid = sysinfo::get_current_pid()
        .map_err(|error| DispatchError::new(format!("inspect worker launcher pid: {error}")))?;
    let current_process = system
        .process(current_pid)
        .ok_or_else(|| DispatchError::new("inspect worker launcher process"))?;
    let current_user = current_process
        .user_id()
        .ok_or_else(|| DispatchError::new("inspect worker launcher user id"))?;
    let virtual_memory = current_process.virtual_memory();
    if virtual_memory == 0 {
        return Err(DispatchError::new(
            "inspect worker launcher virtual memory for RLIMIT_AS",
        ));
    }
    // Missing ownership metadata can only undercount, which lowers the hard
    // limit and refuses forks rather than granting extra authority.
    let count = system
        .processes()
        .values()
        .filter(|process| process.user_id() == Some(current_user))
        .count();
    let count = u64::try_from(count)
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| DispatchError::new("count same-user processes for RLIMIT_NPROC"))?;
    Ok((count, virtual_memory))
}

#[cfg(not(unix))]
fn worker_session_command(
    _program: &str,
    _args: &[String],
    _limits: WorkerResourceLimits,
) -> Result<(String, Vec<String>)> {
    Err(DispatchError::new(
        "worker resource and session controls are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn reject_multiply_linked_checkout_files(root: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            DispatchError::new(format!(
                "inspect sandbox checkout entry {}: {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_file() && metadata.nlink() > 1 {
            return Err(DispatchError::new(format!(
                "sandbox checkout contains a regular file with multiple hard links: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).map_err(|error| {
                DispatchError::new(format!(
                    "list sandbox checkout directory {}: {error}",
                    path.display()
                ))
            })? {
                pending.push(
                    entry
                        .map_err(|error| {
                            DispatchError::new(format!(
                                "read sandbox checkout entry in {}: {error}",
                                path.display()
                            ))
                        })?
                        .path(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_multiply_linked_checkout_files(_root: &Path) -> Result<()> {
    Err(DispatchError::new(
        "worker hard-link preflight is unsupported on this platform",
    ))
}

struct CommandChild {
    child: std::process::Child,
    #[cfg(unix)]
    receipt_broker: Option<CommitReceiptBroker>,
}

impl ChildProcess for CommandChild {
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<ProcessStatus>> {
        let start = Instant::now();
        loop {
            #[cfg(unix)]
            if let Some(broker) = &mut self.receipt_broker {
                broker.poll()?;
            }
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| DispatchError::new(format!("failed to poll child: {e}")))?
            {
                return Ok(Some(status.into()));
            }
            if start.elapsed() >= timeout {
                return Ok(None);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            std::thread::sleep(remaining.min(WAIT_POLL));
        }
    }

    fn terminate(&mut self) -> Result<()> {
        send_signal_to_group(self.child.id(), "-TERM")
    }

    fn kill(&mut self) -> Result<()> {
        let result = self
            .child
            .kill()
            .map_err(|e| DispatchError::new(format!("failed to kill child: {e}")));
        // Best-effort: the direct child is authoritative for this call's
        // result (matches prior behavior exactly), but any descendants that
        // outlived it in the same process group must die too.
        let _ = send_signal_to_group(self.child.id(), "-KILL");
        result
    }

    fn wait(&mut self) -> Result<ProcessStatus> {
        self.child
            .wait()
            .map(ProcessStatus::from)
            .map_err(|e| DispatchError::new(format!("failed to wait for child: {e}")))
    }

    fn id(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn commit_receipt_evidence(&self) -> CommitReceiptEvidence {
        #[cfg(unix)]
        {
            self.receipt_broker.as_ref().map_or_else(
                CommitReceiptEvidence::default,
                CommitReceiptBroker::evidence,
            )
        }
        #[cfg(not(unix))]
        {
            CommitReceiptEvidence::default()
        }
    }
}

/// Spawns the child as the leader of its own process group (`setpgid(0, 0)`
/// under the hood) so `-pid` addresses the whole group, not just this one
/// process. A safe, stable API — no `unsafe` `pre_exec` needed.
#[cfg(unix)]
fn set_own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn set_own_process_group(_command: &mut Command) {}

/// Sends `signal` (e.g. `"-TERM"`, `"-KILL"`) to the process *group* led by
/// `pid` — a negative pid in POSIX `kill(2)` targets the whole group — so
/// every descendant the worker spawned is reached, not just the direct
/// child. Requires the child to have been spawned via
/// [`set_own_process_group`]; harmless (targets an empty/nonexistent group)
/// otherwise.
#[cfg(unix)]
fn send_signal_to_group(pid: u32, signal: &str) -> Result<()> {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let raw_pid = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| DispatchError::new(format!("invalid process-group id {pid}")))?;
    let signal = match signal {
        "-TERM" => Signal::SIGTERM,
        "-KILL" => Signal::SIGKILL,
        _ => return Err(DispatchError::new(format!("unsupported signal {signal}"))),
    };
    match kill(Pid::from_raw(-raw_pid), Some(signal)) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(DispatchError::new(format!(
            "kill {signal:?} -{pid} failed: {error}"
        ))),
    }
}

#[cfg(not(unix))]
fn send_signal_to_group(_pid: u32, _signal: &str) -> Result<()> {
    Err(DispatchError::new(
        "process-group signal handling is only implemented on Unix",
    ))
}

#[cfg(unix)]
fn ensure_process_group_quiescent(pgid: u32) -> Result<()> {
    if !crate::quarantine::process_group_alive(pgid) {
        return Ok(());
    }

    let _ = send_signal_to_group(pgid, "-TERM");
    if wait_for_process_group_exit(pgid, KILL_GRACE) {
        return Ok(());
    }

    let _ = send_signal_to_group(pgid, "-KILL");
    if wait_for_process_group_exit(pgid, KILL_GRACE) {
        Ok(())
    } else {
        Err(DispatchError::worker_state_uncertain(format!(
            "worker process group {pgid} remained alive after TERM/KILL escalation"
        )))
    }
}

#[cfg(not(unix))]
fn ensure_process_group_quiescent(_pgid: u32) -> Result<()> {
    Err(DispatchError::worker_state_uncertain(
        "worker process-group quiescence is only implemented on Unix",
    ))
}

#[cfg(unix)]
fn wait_for_process_group_exit(pgid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !crate::quarantine::process_group_alive(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(WAIT_POLL);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GitCommitProbe;

impl CommitProbe for GitCommitProbe {
    fn head(&self, repo: &Path) -> Result<Option<String>> {
        let mut command = Command::new("git");
        command.arg("-C").arg(repo).args(["rev-parse", "HEAD"]);
        let output = run_bounded_command(&mut command).map_err(|error| {
            DispatchError::new(format!(
                "failed to run git rev-parse in {}: {error}",
                repo.display()
            ))
        })?;
        if !output.status.success() {
            return Ok(None);
        }
        let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if head.is_empty() {
            Ok(None)
        } else {
            Ok(Some(head))
        }
    }

    fn is_clean(&self, repo: &Path) -> Result<bool> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(["status", "--porcelain", "--untracked-files=normal"]);
        let output = run_bounded_command(&mut command).map_err(|error| {
            DispatchError::new(format!(
                "failed to run git status in {}: {error}",
                repo.display()
            ))
        })?;
        if !output.status.success() {
            return Err(DispatchError::new(format!(
                "git status failed in {}: {}",
                repo.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout.is_empty())
    }

    fn is_direct_child(&self, repo: &Path, before: Option<&str>, commit: &str) -> Result<bool> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(["rev-list", "--parents", "-n", "1", commit]);
        let output = run_bounded_command(&mut command).map_err(|error| {
            DispatchError::new(format!(
                "failed to inspect commit parents in {}: {error}",
                repo.display()
            ))
        })?;
        if !output.status.success() {
            return Ok(false);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut fields = stdout.split_whitespace();
        if fields.next() != Some(commit) {
            return Ok(false);
        }
        Ok(match before {
            Some(parent) => fields.next() == Some(parent) && fields.next().is_none(),
            None => fields.next().is_none(),
        })
    }

    fn committer_email(&self, repo: &Path, commit: &str) -> Result<Option<String>> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(["show", "--no-patch", "--format=%ce", commit]);
        let output = run_bounded_command(&mut command).map_err(|error| {
            DispatchError::new(format!(
                "failed to read committer identity in {}: {error}",
                repo.display()
            ))
        })?;
        if !output.status.success() {
            return Ok(None);
        }
        let email = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!email.is_empty()).then_some(email))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Backend;
    use crate::run::{
        NewRun, RunHandle, RunJob, RunTarget, WorkStage, WorkState,
    };
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const BEFORE_COMMIT: &str = "1111111111111111111111111111111111111111";
    const WORKER_COMMIT: &str = "2222222222222222222222222222222222222222";
    const WORKER_STDOUT: &str = "worker stdout\n";

    #[test]
    fn commit_receipt_diagnostics_discriminate_absent_stale_mismatched_and_ambiguous() {
        let absent = CommitReceiptEvidence::default();
        assert_eq!(
            absent.rejection_for(WORKER_COMMIT),
            Some(CommitAuthenticationRejection::ReceiptAbsent)
        );

        let stale = CommitReceiptEvidence {
            stale_lineage: 1,
            ..CommitReceiptEvidence::default()
        };
        assert_eq!(
            stale.rejection_for(WORKER_COMMIT),
            Some(CommitAuthenticationRejection::ReceiptStale)
        );

        let mismatched = CommitReceiptEvidence {
            accepted: vec![BEFORE_COMMIT.to_string()],
            ..CommitReceiptEvidence::default()
        };
        assert_eq!(
            mismatched.rejection_for(WORKER_COMMIT),
            Some(CommitAuthenticationRejection::ReceiptMismatched)
        );

        let ambiguous = CommitReceiptEvidence {
            accepted: vec![BEFORE_COMMIT.to_string(), "3".repeat(40)],
            ..CommitReceiptEvidence::default()
        };
        assert_eq!(
            ambiguous.rejection_for(WORKER_COMMIT),
            Some(CommitAuthenticationRejection::ReceiptAmbiguous)
        );

        let rotated_current_worker = CommitReceiptEvidence {
            accepted: vec![BEFORE_COMMIT.to_string(), WORKER_COMMIT.to_string()],
            ..CommitReceiptEvidence::default()
        };
        assert_eq!(rotated_current_worker.rejection_for(WORKER_COMMIT), None);
    }

    /// Adapts a bare heartbeat closure to the [`WorkerHooks`] trait so the
    /// wait-loop tests can drive `on_heartbeat` without a full observer.
    struct HeartbeatFn<F>(F);

    impl<F: FnMut(Duration) -> Result<()>> WorkerHooks for HeartbeatFn<F> {
        fn on_heartbeat(&mut self, elapsed: Duration) -> Result<()> {
            (self.0)(elapsed)
        }
    }

    #[test]
    fn pi_backend_uses_pinned_argv_repo_cwd_and_receipt_socket() {
        let temp = TempDir::new("pi-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(BEFORE_COMMIT),
        );

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(result.status, DispatchStatus::Success);
        assert_eq!(result.worker_commit.as_deref(), Some(WORKER_COMMIT));
        let spawn = exec.spawned();
        assert_eq!(
            spawn.argv,
            vec![
                "pi",
                "--model",
                "opencode-go/glm-5.2",
                "--thinking",
                "xhigh",
                "--approve",
                "-p",
                PROMPT,
            ]
        );
        assert_eq!(spawn.cwd, repo);
        assert_eq!(spawn.stdin, StdinMode::Null);
        assert_eq!(spawn.sandbox_profile, None);
        assert!(spawn.commit_receipt_socket.is_some());
        assert_eq!(
            spawn.stdout_path,
            temp.path().join("logs/cycle-1/bead-1/001-worker.out")
        );
        assert_eq!(
            spawn.stderr_path,
            temp.path().join("logs/cycle-1/bead-1/001-worker.err")
        );
        let receipt_socket = spawn
            .commit_receipt_socket
            .as_ref()
            .expect("receipt socket")
            .display()
            .to_string();
        let env = spawn
            .env
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(env["GIT_AUTHOR_NAME"], ATTEMPT_IDENTITY_NAME);
        assert_eq!(env["GIT_AUTHOR_EMAIL"], TEST_ATTEMPT_IDENTITY);
        assert_eq!(env["GIT_COMMITTER_NAME"], ATTEMPT_IDENTITY_NAME);
        assert_eq!(env["GIT_COMMITTER_EMAIL"], TEST_ATTEMPT_IDENTITY);
        assert_eq!(env["GIT_CONFIG_COUNT"], "1");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "core.hooksPath");
        assert!(
            exec.hook_existed_at_spawn(),
            "authenticated post-commit hook must exist when the worker is spawned"
        );
        assert_eq!(env["UNDERTAKE_COMMIT_RECEIPT_SOCKET"], receipt_socket);
    }

    #[test]
    fn spawn_request_routes_worker_mutable_runtime_to_per_attempt_directory() {
        let temp = TempDir::new("worker-runtime-env");
        let repo = temp.path().join("repo");
        let runtime = temp.path().join("runtime");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        std::fs::create_dir_all(runtime.join("tmp")).expect("mkdir worker tmp");
        std::fs::create_dir_all(runtime.join("cache")).expect("mkdir worker cache");
        std::fs::create_dir_all(runtime.join("config")).expect("mkdir worker config");
        std::fs::create_dir_all(runtime.join("data")).expect("mkdir worker data");
        std::fs::create_dir_all(runtime.join("state")).expect("mkdir worker state");
        let runtime = std::fs::canonicalize(runtime).expect("canonical worker runtime");
        let mut request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(BEFORE_COMMIT),
        );
        request.worker_runtime_dir = Some(runtime.clone());

        let spawn = spawn_request(&request, temp.path()).expect("build worker spawn");
        let env = spawn
            .env
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(env["TMPDIR"], runtime.join("tmp").display().to_string());
        assert_eq!(
            env["XDG_CACHE_HOME"],
            runtime.join("cache").display().to_string()
        );
        assert_eq!(
            env["XDG_CONFIG_HOME"],
            runtime.join("config").display().to_string()
        );
        assert_eq!(
            env["XDG_DATA_HOME"],
            runtime.join("data").display().to_string()
        );
        assert_eq!(
            env["XDG_STATE_HOME"],
            runtime.join("state").display().to_string()
        );
        assert_eq!(env["TMP"], env["TMPDIR"]);
        assert_eq!(env["TEMP"], env["TMPDIR"]);
    }

    #[test]
    fn codex_backend_uses_per_run_reasoning_override() {
        let temp = TempDir::new("codex-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let mut request = request(&repo, Backend::Codex, "gpt-5.6-sol", Some(BEFORE_COMMIT));
        request.reasoning_effort = Some(ReasoningEffort::Max);
        request.sandbox_profile = Some(repo.join("worker.sb"));
        request.worker_runtime_dir = Some(test_worker_runtime(temp.path()));

        run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(
            exec.spawned().argv,
            vec![
                "codex",
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "gpt-5.6-sol",
                "--config",
                "model_reasoning_effort=\"max\"",
                PROMPT,
            ]
        );
    }

    #[test]
    fn missing_outer_sandbox_does_not_disable_harness_safety() {
        let temp = TempDir::new("missing-outer-sandbox");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut codex_request = request(&repo, Backend::Codex, "gpt-5.6-sol", Some(BEFORE_COMMIT));
        codex_request.reasoning_effort = Some(ReasoningEffort::Max);
        let codex = spawn_request(&codex_request, temp.path())
            .expect("build Codex spawn without outer sandbox");
        assert!(
            !codex
                .argv
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"),
            "Codex's sandbox must remain enabled when no outer profile exists"
        );

        let claude = spawn_request(
            &request(
                &repo,
                Backend::Claude,
                "claude-sonnet-5",
                Some(BEFORE_COMMIT),
            ),
            temp.path(),
        )
        .expect("build Claude spawn without outer sandbox");
        assert!(
            !claude
                .argv
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions"),
            "Claude's permission checks must remain enabled when no outer profile exists"
        );
    }

    #[test]
    fn agy_backend_uses_pinned_argv_with_load_bearing_add_dir() {
        let temp = TempDir::new("agy-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let request = request(
            &repo,
            Backend::Agy,
            "Gemini 3.5 Flash (High)",
            Some(BEFORE_COMMIT),
        );

        run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(
            exec.spawned().argv,
            vec![
                "agy",
                "-p",
                PROMPT,
                "--add-dir",
                repo.to_str().expect("utf8 repo"),
                "--model",
                "Gemini 3.5 Flash (High)",
                "--dangerously-skip-permissions",
            ]
        );
    }

    #[test]
    fn claude_backend_uses_pinned_argv() {
        let temp = TempDir::new("claude-argv");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let mut request = request(
            &repo,
            Backend::Claude,
            "claude-sonnet-5",
            Some(BEFORE_COMMIT),
        );
        request.sandbox_profile = Some(repo.join("worker.sb"));
        request.worker_runtime_dir = Some(test_worker_runtime(temp.path()));

        run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(
            exec.spawned().argv,
            vec![
                "claude",
                "-p",
                PROMPT,
                "--model",
                "claude-sonnet-5",
                "--dangerously-skip-permissions"
            ]
        );
    }

    #[test]
    fn adversarial_readonly_argv_disables_tools_for_every_backend() {
        let repo = Path::new("/tmp/review-state");

        assert_eq!(
            readonly_argv_for_backend(Backend::Pi, "opencode-go/glm-5.2", None, PROMPT, repo,)
                .expect("pi readonly argv"),
            vec![
                "pi",
                "--model",
                "opencode-go/glm-5.2",
                "--thinking",
                "xhigh",
                "--no-tools",
                "-p",
                PROMPT,
            ]
        );
        assert_eq!(
            readonly_argv_for_backend(
                Backend::Omp,
                "openai-codex/gpt-5.6-terra",
                Some(ReasoningEffort::Xhigh),
                PROMPT,
                repo,
            )
            .expect("OMP readonly argv"),
            vec![
                "omp",
                "--model",
                "openai-codex/gpt-5.6-terra",
                "--thinking",
                "xhigh",
                "--no-tools",
                "--no-session",
                "-p",
                PROMPT,
            ]
        );
        assert_eq!(
            readonly_argv_for_backend(
                Backend::Codex,
                "gpt-5.6-terra",
                Some(ReasoningEffort::Xhigh),
                PROMPT,
                repo,
            )
            .expect("codex readonly argv"),
            vec![
                "codex",
                "exec",
                "--model",
                "gpt-5.6-terra",
                "--config",
                "model_reasoning_effort=\"xhigh\"",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                PROMPT,
            ]
        );
        assert_eq!(
            readonly_argv_for_backend(Backend::Agy, "Gemini 3.5 Flash (High)", None, PROMPT, repo,)
                .expect("agy readonly argv"),
            vec![
                "agy",
                "-p",
                PROMPT,
                "--add-dir",
                "/tmp/review-state",
                "--model",
                "Gemini 3.5 Flash (High)",
                "--mode",
                "plan",
                "--sandbox",
            ]
        );
        assert_eq!(
            readonly_argv_for_backend(Backend::Claude, "claude-sonnet-5", None, PROMPT, repo,)
                .expect("claude readonly argv"),
            vec![
                "claude",
                "--safe-mode",
                "-p",
                PROMPT,
                "--model",
                "claude-sonnet-5",
                "--permission-mode",
                "plan",
                "--tools",
                "",
            ]
        );
    }

    #[test]
    fn omp_backend_uses_explicit_ephemeral_unattended_argv() {
        assert_eq!(
            argv_for_backend(
                Backend::Omp,
                "openai-codex/gpt-5.6-luna",
                Some(ReasoningEffort::Medium),
                PROMPT,
                Path::new("/tmp/repo"),
            )
            .expect("OMP argv"),
            vec![
                "omp",
                "--model",
                "openai-codex/gpt-5.6-luna",
                "--thinking",
                "medium",
                "--auto-approve",
                "--no-session",
                "-p",
                PROMPT,
            ]
        );
        assert!(
            argv_for_backend(
                Backend::Omp,
                "openai-codex/gpt-5.6-luna",
                None,
                PROMPT,
                Path::new("/tmp/repo"),
            )
            .is_err(),
            "OMP dispatch must not inherit a global thinking level"
        );
    }

    #[test]
    fn omp_backend_preserves_exact_ollama_cloud_model_aliases() {
        for dispatch_id in ["ollama-cloud/glm-5.2", "ollama-cloud/minimax-m3"] {
            assert_eq!(
                argv_for_backend(
                    Backend::Omp,
                    dispatch_id,
                    Some(ReasoningEffort::Max),
                    PROMPT,
                    Path::new("/tmp/repo"),
                )
                .expect("OMP argv"),
                vec![
                    "omp",
                    "--model",
                    dispatch_id,
                    "--thinking",
                    "max",
                    "--auto-approve",
                    "--no-session",
                    "-p",
                    PROMPT,
                ]
            );
        }
    }

    #[test]
    fn timeout_path_sends_term_then_waits_grace_then_kills() {
        let temp = TempDir::new("timeout");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::timeout_then_kill();
        let commits = FakeCommits::new([Some("before")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2", Some("before"));

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("timeout is reported as dispatch result");

        assert_eq!(
            result.status,
            DispatchStatus::Failed(DispatchFailure::TimedOut)
        );
        assert_eq!(
            exec.events(),
            vec![
                ExecEvent::WaitFor(Duration::from_secs(45)),
                ExecEvent::Terminate,
                ExecEvent::WaitFor(Duration::from_secs(3)),
                ExecEvent::Kill,
                ExecEvent::Wait,
            ]
        );
    }

    #[test]
    fn wait_for_error_terminates_and_reaps_the_process_group_before_propagating() {
        // A `wait_for` failure (e.g. the OS poll call itself erroring) must
        // never be mistaken for "the worker finished" — it, and any
        // descendants in its process group, could still be running. The
        // group must be terminated and reaped before the error propagates,
        // not after.
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut child = FakeChild::wait_for_error(Rc::clone(&events));

        let error = wait_with_timeout_and_heartbeat(
            &mut child,
            Duration::from_secs(45),
            Duration::from_secs(45),
            &mut (),
        )
        .expect_err("a wait_for error must propagate, not be swallowed");

        assert_eq!(error.to_string(), "simulated wait_for failure");
        assert_eq!(
            events.borrow().as_slice(),
            [
                ExecEvent::WaitFor(Duration::from_secs(45)),
                ExecEvent::Terminate,
                ExecEvent::WaitFor(KILL_GRACE),
                ExecEvent::Kill,
                ExecEvent::Wait,
            ]
        );
    }

    #[test]
    fn heartbeat_error_terminates_and_reaps_the_process_group_before_propagating() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut child = FakeChild::pending(Rc::clone(&events));

        let error = wait_with_timeout_and_heartbeat(
            &mut child,
            Duration::from_secs(45),
            Duration::from_secs(45),
            &mut HeartbeatFn(|_elapsed: Duration| {
                Err(DispatchError::new("simulated heartbeat failure"))
            }),
        )
        .expect_err("a heartbeat error must propagate, not be swallowed");

        assert_eq!(error.to_string(), "simulated heartbeat failure");
        assert_eq!(
            events.borrow().as_slice(),
            [
                ExecEvent::WaitFor(Duration::from_secs(45)),
                ExecEvent::Terminate,
                ExecEvent::WaitFor(KILL_GRACE),
                ExecEvent::Kill,
                ExecEvent::Wait,
            ]
        );
    }

    /// Records call order across [`WorkerHooks::on_pre_spawn`] and
    /// [`Exec::spawn`] into a single shared log, proving the invalidate step
    /// truly happens before the worker process exists rather than merely
    /// before `on_spawn`.
    struct OrderingHooks {
        log: Rc<RefCell<Vec<&'static str>>>,
        pre_spawn_error: Option<&'static str>,
    }

    impl WorkerHooks for OrderingHooks {
        fn on_pre_spawn(
            &mut self,
            _hook_name: &str,
        ) -> Result<Option<WorkerHookRegistration>> {
            self.log.borrow_mut().push("pre_spawn");
            match self.pre_spawn_error {
                None => Ok(None),
                Some(message) => Err(DispatchError::new(message)),
            }
        }
    }

    struct OrderingExec {
        log: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Exec for OrderingExec {
        fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
            self.log.borrow_mut().push("spawn");
            std::fs::write(&request.stdout_path, b"").expect("write fake stdout");
            std::fs::write(&request.stderr_path, b"").expect("write fake stderr");
            Ok(Box::new(FakeChild::success(Rc::new(RefCell::new(
                Vec::new(),
            )))))
        }
    }

    #[test]
    fn on_pre_spawn_runs_before_the_worker_is_spawned() {
        let temp = TempDir::new("pre-spawn-order");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let log = Rc::new(RefCell::new(Vec::new()));
        let exec = OrderingExec {
            log: Rc::clone(&log),
        };
        let commits = FakeCommits::new([Some("before"), Some("before")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2", Some("before"));
        let mut hooks = OrderingHooks {
            log: Rc::clone(&log),
            pre_spawn_error: None,
        };

        run_with_heartbeat(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
            Duration::from_secs(45),
            &mut hooks,
        )
        .expect("dispatch succeeds");

        assert_eq!(
            log.borrow().as_slice(),
            ["pre_spawn", "spawn"],
            "the prior attempt's identity must be invalidated before the new worker exists"
        );
    }

    #[test]
    fn on_pre_spawn_failure_prevents_the_spawn_entirely() {
        let temp = TempDir::new("pre-spawn-failure");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let log = Rc::new(RefCell::new(Vec::new()));
        let exec = OrderingExec {
            log: Rc::clone(&log),
        };
        let commits = FakeCommits::new([Some("before")]);
        let request = request(&repo, Backend::Pi, "opencode-go/glm-5.2", Some("before"));
        let mut hooks = OrderingHooks {
            log: Rc::clone(&log),
            pre_spawn_error: Some("simulated invalidate failure"),
        };

        let error = run_with_heartbeat(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
            Duration::from_secs(45),
            &mut hooks,
        )
        .expect_err("a failed invalidation must prevent the worker from ever running");

        assert_eq!(error.to_string(), "simulated invalidate failure");
        assert_eq!(
            log.borrow().as_slice(),
            ["pre_spawn"],
            "the worker must never spawn once identity invalidation has failed"
        );
    }

    #[test]
    fn stdout_and_stderr_logs_are_written_under_cycle_and_bead() {
        let temp = TempDir::new("logs");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(WORKER_STDOUT, "worker stderr\n");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(BEFORE_COMMIT),
        );

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch succeeds");

        assert_eq!(
            result.stdout_path,
            temp.path().join("logs/cycle-1/bead-1/001-worker.out")
        );
        assert_eq!(
            result.stderr_path,
            temp.path().join("logs/cycle-1/bead-1/001-worker.err")
        );
        assert_eq!(
            std::fs::read_to_string(&result.stdout_path).unwrap(),
            WORKER_STDOUT
        );
        assert_eq!(
            std::fs::read_to_string(&result.stderr_path).unwrap(),
            "worker stderr\n"
        );
        assert_eq!(result.stdout_bytes, WORKER_STDOUT.len() as u64);
        assert_eq!(result.stderr_bytes, 14);
    }

    #[test]
    fn exit_zero_with_no_new_commit_and_zero_stdout_is_backend_flake_failure() {
        let temp = TempDir::new("zero-stdout-no-commit");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("", "");
        let commits = FakeCommits::new([Some("same"), Some("same")]);
        let request = request(&repo, Backend::Agy, "Gemini 3.5 Flash (High)", Some("same"));

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert_eq!(
            result.status,
            DispatchStatus::Failed(DispatchFailure::BackendFlakeZeroStdoutNoCommit)
        );
        assert_eq!(result.stdout_bytes, 0);
    }

    #[test]
    fn exit_zero_with_no_new_commit_and_nonzero_stdout_is_no_new_commit_failure() {
        let temp = TempDir::new("nonzero-stdout-no-commit");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success("worker tried\n", "");
        let commits = FakeCommits::new([Some("same"), Some("same")]);
        let request = request(&repo, Backend::Claude, "claude-sonnet-5", Some("same"));

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert_eq!(
            result.status,
            DispatchStatus::Failed(DispatchFailure::NoNewCommit)
        );
        assert_eq!(result.stdout_bytes, 13);
    }

    #[test]
    fn exit_zero_with_foreign_head_change_is_not_worker_success() {
        let temp = TempDir::new("foreign-head");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let exec = FakeExec::success(
            "UNDERTAKE_WORKER_COMMIT: 2222222222222222222222222222222222222222\n",
            "",
        );
        let commits = FakeCommits::new([
            Some("1111111111111111111111111111111111111111"),
            Some("3333333333333333333333333333333333333333"),
        ]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some("1111111111111111111111111111111111111111"),
        );

        let result = run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert!(
            !matches!(result.status, DispatchStatus::Success),
            "a foreign HEAD change must not authenticate worker success"
        );
    }

    #[test]
    fn parent_observes_a_clean_direct_child_in_the_attempt_checkout() {
        let temp = TempDir::new("observed-direct-child");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Undertake Test"]);
        git(
            &repo,
            &["config", "user.email", "undertake-test@example.invalid"],
        );
        std::fs::write(repo.join("README.md"), b"base\n").expect("write base");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "initial"]);
        let before_head = git(&repo, &["rev-parse", "HEAD"]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(&before_head),
        );

        let result = run(
            &DirectChildExec,
            &GitCommitProbe,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert_eq!(result.status, DispatchStatus::Success);
        assert_eq!(
            result.worker_commit.as_deref(),
            Some(git(&repo, &["rev-parse", "HEAD"]).as_str())
        );
    }

    #[test]
    fn foreign_commit_inserted_before_worker_commit_is_not_worker_success() {
        let temp = TempDir::new("foreign-parent");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Undertake Test"]);
        git(
            &repo,
            &["config", "user.email", "undertake-test@example.invalid"],
        );
        std::fs::write(repo.join("README.md"), b"base\n").expect("write base");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "initial"]);
        let before_head = git(&repo, &["rev-parse", "HEAD"]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(&before_head),
        );

        let result = run(
            &ForeignThenWorkerExec,
            &GitCommitProbe,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("dispatch result");

        assert!(
            !matches!(result.status, DispatchStatus::Success),
            "a foreign commit inserted between the base and worker commit must not authenticate success"
        );
    }

    const PROMPT: &str = "work on the bead";
    const TEST_ATTEMPT_IDENTITY: &str = "undertake-attempt-test@invalid";

    fn test_worker_resource_limits() -> WorkerResourceLimits {
        WorkerResourceLimits::new(
            900,
            64,
            32 * 1024 * 1024 * 1024,
            1024 * 1024 * 1024,
        )
        .expect("valid test worker limits")
    }

    fn test_worker_runtime(root: &Path) -> PathBuf {
        let runtime = root.join("worker-runtime");
        for child in ["tmp", "cache", "config", "data", "state"] {
            std::fs::create_dir_all(runtime.join(child))
                .expect("create worker runtime child");
        }
        std::fs::canonicalize(runtime).expect("canonical worker runtime")
    }

    fn request(
        repo: &Path,
        backend: Backend,
        dispatch_id: &str,
        before_head: Option<&str>,
    ) -> DispatchRequest {
        DispatchRequest {
            repo: repo.to_path_buf(),
            before_head: before_head.map(str::to_string),
            attempt_id: "001-worker".to_string(),
            cycle_id: "cycle-1".to_string(),
            bead_id: "bead-1".to_string(),
            backend,
            dispatch_id: dispatch_id.to_string(),
            reasoning_effort: None,
            prompt: PROMPT.to_string(),
            attempt_identity: TEST_ATTEMPT_IDENTITY.to_string(),
            sandbox_profile: None,
            worker_runtime_dir: None,
            worker_resource_limits: test_worker_resource_limits(),
        }
    }

    #[derive(Clone)]
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("undertake-dispatch-{label}-{nanos}"));
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

    fn implementing_run(state_dir: &Path) -> RunHandle {
        RunHandle::create(
            state_dir,
            RunJob::Work,
            NewRun {
                target: RunTarget {
                    repo: "/repo/undertake".to_string(),
                    bead: Some("conductor-u9t".to_string()),
                },
                work: Some(WorkState {
                    cycle_id: "cycle-hook-gc".to_string(),
                    authorization_sha256: "a".repeat(64),
                    before_head: Some("b".repeat(40)),
                    owner_pid: Some(std::process::id()),
                    owner_pid_generation: None,
                    worker_pgid: None,
                    worker_pgid_generation: None,
                    worker_slots: Vec::new(),
                    worker_profile: None,
                    worker_commit: None,
                    mechanical: None,
                    review_resume_budget_secs: None,
                    stage: WorkStage::Implementing,
                }),
                ..NewRun::default()
            },
        )
        .expect("create implementing run")
    }

    #[test]
    fn worker_commit_hook_cleanup_removes_a_finished_runs_stale_hook() {
        let temp = TempDir::new("hook-gc-stale");
        let mut run = implementing_run(temp.path());
        let hook_name = "a".repeat(32);
        run.prepare_worker_commit_hook(&hook_name)
            .expect("record current hook");
        prepare_authenticated_commit_hook(temp.path(), &hook_name, Some(run.run_id()))
            .expect("create owned hook");
        run.finish("failed").expect("finish run");

        let stats = cleanup_stale_authenticated_commit_hooks(temp.path(), None)
            .expect("clean stale hooks");

        assert_eq!(stats.removed, 1);
        assert!(!temp.path().join("worker-commit-hooks").join(hook_name).exists());
    }

    #[test]
    fn worker_commit_hook_cleanup_preserves_current_unfinished_attempt_hook() {
        let temp = TempDir::new("hook-gc-active");
        let mut run = implementing_run(temp.path());
        let hook_name = "b".repeat(32);
        run.prepare_worker_commit_hook(&hook_name)
            .expect("record current hook");
        let hook_dir =
            prepare_authenticated_commit_hook(temp.path(), &hook_name, Some(run.run_id()))
                .expect("create active hook");

        let stats = cleanup_stale_authenticated_commit_hooks(temp.path(), None)
            .expect("scan hooks");

        assert_eq!(stats.removed, 0);
        assert!(hook_dir.join("post-commit").is_file());
    }

    #[test]
    fn worker_commit_hook_cleanup_bounds_scans_and_removals_per_invocation() {
        let temp = TempDir::new("hook-gc-bounded");
        let mut run = implementing_run(temp.path());
        let run_id = run.run_id().to_string();
        run.finish("failed").expect("finish run");
        for index in 0..(HOOK_CLEANUP_SCAN_LIMIT + 5) {
            let hook_name = format!("{index:032x}");
            prepare_authenticated_commit_hook(temp.path(), &hook_name, Some(&run_id))
                .expect("create stale hook");
        }

        let stats = cleanup_stale_authenticated_commit_hooks(temp.path(), None)
            .expect("bounded cleanup");

        assert!(stats.scanned <= HOOK_CLEANUP_SCAN_LIMIT);
        assert!(stats.removed <= HOOK_CLEANUP_REMOVE_LIMIT);
        assert!(
            std::fs::read_dir(temp.path().join("worker-commit-hooks"))
                .expect("read hook root")
                .count()
                > 0,
            "one invocation must not sweep unbounded history"
        );
    }

    #[test]
    fn completed_worker_removes_its_commit_hook_after_quiescence() {
        let temp = TempDir::new("hook-clean-after-worker");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        let exec = FakeExec::success(WORKER_STDOUT, "");
        let commits = FakeCommits::new([Some(BEFORE_COMMIT), Some(WORKER_COMMIT)]);
        let request = request(
            &repo,
            Backend::Pi,
            "opencode-go/glm-5.2",
            Some(BEFORE_COMMIT),
        );

        run(
            &exec,
            &commits,
            &request,
            temp.path(),
            Duration::from_secs(45),
        )
        .expect("worker completes");

        assert_eq!(
            std::fs::read_dir(temp.path().join("worker-commit-hooks"))
                .expect("read hook root")
                .count(),
            0
        );
    }

    struct FakeExec {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        child: RefCell<Option<FakeChild>>,
        spawned: RefCell<Option<SpawnRequest>>,
        hook_existed_at_spawn: RefCell<Option<bool>>,
        events: Rc<RefCell<Vec<ExecEvent>>>,
    }

    impl FakeExec {
        fn success(stdout: &str, stderr: &str) -> Self {
            let events = Rc::new(RefCell::new(Vec::new()));
            Self {
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                child: RefCell::new(Some(FakeChild::success(Rc::clone(&events)))),
                spawned: RefCell::new(None),
                hook_existed_at_spawn: RefCell::new(None),
                events,
            }
        }

        fn timeout_then_kill() -> Self {
            let events = Rc::new(RefCell::new(Vec::new()));
            Self {
                stdout: Vec::new(),
                stderr: Vec::new(),
                child: RefCell::new(Some(FakeChild::timeout_then_kill(Rc::clone(&events)))),
                spawned: RefCell::new(None),
                hook_existed_at_spawn: RefCell::new(None),
                events,
            }
        }

        fn spawned(&self) -> SpawnRequest {
            self.spawned.borrow().as_ref().expect("spawned").clone()
        }

        fn hook_existed_at_spawn(&self) -> bool {
            self.hook_existed_at_spawn
                .borrow()
                .expect("hook existence recorded at spawn")
        }

        fn events(&self) -> Vec<ExecEvent> {
            self.events.borrow().clone()
        }
    }

    struct ForeignThenWorkerExec;

    struct DirectChildExec;

    impl Exec for DirectChildExec {
        fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
            std::fs::write(request.cwd.join("worker.txt"), b"worker\n")
                .expect("write worker change");
            git_as_worker(request, &["add", "worker.txt"]);
            git_as_worker(request, &["commit", "-m", "worker: clean direct child"]);
            std::fs::write(&request.stdout_path, b"worker complete\n")
                .expect("write worker stdout");
            std::fs::write(&request.stderr_path, b"").expect("write worker stderr");
            Ok(Box::new(FakeChild::success(Rc::new(RefCell::new(
                Vec::new(),
            )))))
        }
    }

    impl Exec for ForeignThenWorkerExec {
        fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
            std::fs::write(request.cwd.join("foreign.txt"), b"foreign\n")
                .expect("write foreign change");
            git(&request.cwd, &["add", "foreign.txt"]);
            git(
                &request.cwd,
                &["commit", "-m", "foreign: concurrent change"],
            );

            std::fs::write(request.cwd.join("worker.txt"), b"worker\n")
                .expect("write worker change");
            git(&request.cwd, &["add", "worker.txt"]);
            git(&request.cwd, &["commit", "-m", "worker: intended change"]);
            let worker_commit = git(&request.cwd, &["rev-parse", "HEAD"]);

            std::fs::write(
                &request.stdout_path,
                format!("UNDERTAKE_WORKER_COMMIT: {worker_commit}\n"),
            )
            .expect("write worker stdout");
            std::fs::write(&request.stderr_path, b"").expect("write worker stderr");
            Ok(Box::new(FakeChild::success(Rc::new(RefCell::new(
                Vec::new(),
            )))))
        }
    }

    /// Runs git under the spawn environment so in-memory test doubles carry
    /// the audit identity used by their non-OS authentication fallback.
    fn git_as_worker(request: &SpawnRequest, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&request.cwd)
            .args(args)
            .envs(request.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn git as worker");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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

    impl Exec for FakeExec {
        fn spawn(&self, request: &SpawnRequest) -> Result<Box<dyn ChildProcess>> {
            std::fs::write(&request.stdout_path, &self.stdout).expect("write fake stdout");
            std::fs::write(&request.stderr_path, &self.stderr).expect("write fake stderr");
            let hook_existed = request
                .env
                .iter()
                .find(|(key, _)| key == "GIT_CONFIG_VALUE_0")
                .is_some_and(|(_, value)| Path::new(value).join("post-commit").exists());
            *self.hook_existed_at_spawn.borrow_mut() = Some(hook_existed);
            *self.spawned.borrow_mut() = Some(request.clone());
            let child = self.child.borrow_mut().take().expect("one spawn");
            Ok(Box::new(child))
        }
    }

    struct FakeChild {
        events: Rc<RefCell<Vec<ExecEvent>>>,
        wait_for_results: RefCell<Vec<Option<ProcessStatus>>>,
        /// 0-indexed `wait_for` call number that should return `Err` instead
        /// of popping `wait_for_results` — used to prove the caller reaps
        /// the process group rather than leaving it running on error.
        wait_for_error_at_call: Option<usize>,
        wait_for_calls: usize,
        wait_result: ProcessStatus,
    }

    impl FakeChild {
        fn success(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![Some(ProcessStatus::code(0))]),
                wait_for_error_at_call: None,
                wait_for_calls: 0,
                wait_result: ProcessStatus::code(0),
            }
        }

        fn timeout_then_kill(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![None, None]),
                wait_for_error_at_call: None,
                wait_for_calls: 0,
                wait_result: ProcessStatus::signal(),
            }
        }

        /// The very first `wait_for` call fails outright — simulates an OS
        /// poll error while the worker may still be running.
        fn wait_for_error(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![Some(ProcessStatus::code(0))]),
                wait_for_error_at_call: Some(0),
                wait_for_calls: 0,
                wait_result: ProcessStatus::signal(),
            }
        }

        /// The first `wait_for` call reports "still running" (`None`) so a
        /// caller-supplied heartbeat closure gets invoked next.
        fn pending(events: Rc<RefCell<Vec<ExecEvent>>>) -> Self {
            Self {
                events,
                wait_for_results: RefCell::new(vec![None, Some(ProcessStatus::code(0))]),
                wait_for_error_at_call: None,
                wait_for_calls: 0,
                wait_result: ProcessStatus::signal(),
            }
        }
    }

    impl ChildProcess for FakeChild {
        fn wait_for(&mut self, timeout: Duration) -> Result<Option<ProcessStatus>> {
            self.events.borrow_mut().push(ExecEvent::WaitFor(timeout));
            let call = self.wait_for_calls;
            self.wait_for_calls += 1;
            if self.wait_for_error_at_call == Some(call) {
                return Err(DispatchError::new("simulated wait_for failure"));
            }
            Ok(self.wait_for_results.borrow_mut().remove(0))
        }

        fn terminate(&mut self) -> Result<()> {
            self.events.borrow_mut().push(ExecEvent::Terminate);
            Ok(())
        }

        fn kill(&mut self) -> Result<()> {
            self.events.borrow_mut().push(ExecEvent::Kill);
            Ok(())
        }

        fn wait(&mut self) -> Result<ProcessStatus> {
            self.events.borrow_mut().push(ExecEvent::Wait);
            Ok(self.wait_result)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ExecEvent {
        WaitFor(Duration),
        Terminate,
        Kill,
        Wait,
    }

    struct FakeCommits {
        heads: RefCell<Vec<Option<String>>>,
    }

    impl FakeCommits {
        fn new<const N: usize>(heads: [Option<&str>; N]) -> Self {
            Self {
                heads: RefCell::new(heads.into_iter().map(|h| h.map(str::to_string)).collect()),
            }
        }
    }

    impl CommitProbe for FakeCommits {
        fn head(&self, _repo: &Path) -> Result<Option<String>> {
            Ok(self.heads.borrow_mut().remove(0))
        }

        fn is_clean(&self, _repo: &Path) -> Result<bool> {
            Ok(true)
        }

        fn is_direct_child(
            &self,
            _repo: &Path,
            _before: Option<&str>,
            commit: &str,
        ) -> Result<bool> {
            Ok(matches!(commit, WORKER_COMMIT | "after"))
        }

        fn committer_email(&self, _repo: &Path, _commit: &str) -> Result<Option<String>> {
            Ok(Some(TEST_ATTEMPT_IDENTITY.to_string()))
        }
    }

    #[test]
    #[cfg(unix)]
    fn bounded_command_captures_normal_output() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf normal; printf diagnostic >&2");

        let output = run_bounded_command_with_limits(
            &mut command,
            None,
            Duration::from_secs(1),
            Duration::from_millis(100),
        )
        .expect("bounded command succeeds");

        assert_eq!(
            (
                output.status.code(),
                output.stdout.as_slice(),
                output.stderr.as_slice()
            ),
            (Some(0), b"normal".as_slice(), b"diagnostic".as_slice())
        );
    }

    #[test]
    #[cfg(unix)]
    fn bounded_command_file_backed_stdin_cannot_deadlock_against_output() {
        let input = vec![b'i'; 256 * 1024];
        let mut command = Command::new("/usr/bin/python3");
        command.args([
            "-c",
            "import sys; sys.stdout.buffer.write(b'o' * 131072); sys.stdout.flush(); \
             data = sys.stdin.buffer.read(); sys.stdout.write(str(len(data)))",
        ]);

        let output = run_bounded_command_with_limits(
            &mut command,
            Some(&input),
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
        .expect("opposing large stdin/stdout completes without pipe deadlock");

        assert!(
            output.status.success()
                && output.stdout.len() == 131_072 + 6
                && output.stdout.ends_with(b"262144"),
            "unexpected staged-stdin output: status={}, bytes={}",
            output.status,
            output.stdout.len()
        );
    }

    #[test]
    #[cfg(unix)]
    fn bounded_command_returns_nonzero_output_without_hiding_evidence() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf partial; printf rejected >&2; exit 7");

        let output = run_bounded_command_with_limits(
            &mut command,
            None,
            Duration::from_secs(1),
            Duration::from_millis(100),
        )
        .expect("nonzero status is observable output, not a runner error");

        assert_eq!(
            (
                output.status.code(),
                output.stdout.as_slice(),
                output.stderr.as_slice()
            ),
            (Some(7), b"partial".as_slice(), b"rejected".as_slice())
        );
    }

    #[test]
    #[cfg(unix)]
    fn bounded_command_timeout_preserves_captured_output_after_reap() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf before-timeout; printf timeout-detail >&2; sleep 30");

        let error = run_bounded_command_with_limits(
            &mut command,
            None,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .expect_err("sleep must time out");

        assert_eq!(
            (
                error.is_timeout(),
                error.leaves_process_state_uncertain(),
                error.stdout(),
                error.stderr()
            ),
            (
                true,
                false,
                b"before-timeout".as_slice(),
                b"timeout-detail".as_slice()
            )
        );
    }

    #[test]
    fn claude_auth_output_classifies_ready_from_logged_in_true() {
        let output = Output {
            status: successful_exit_status(),
            stdout: br#"{"loggedIn":true,"authMethod":"claude.ai"}"#.to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(classify_claude_auth_output(&output), AuthReadiness::Ready);
    }

    #[test]
    fn claude_auth_output_classifies_not_authenticated_from_logged_in_false() {
        let output = Output {
            status: successful_exit_status(),
            stdout: br#"{"loggedIn":false,"authMethod":"none"}"#.to_vec(),
            stderr: Vec::new(),
        };
        let AuthReadiness::NotAuthenticated { message } = classify_claude_auth_output(&output)
        else {
            panic!("logged-out output must classify NotAuthenticated");
        };
        assert!(message.contains("authMethod=none"));
        assert!(message.contains("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(message.contains("apiKeyHelper"));
    }

    #[test]
    fn claude_auth_output_classifies_unreadable_on_missing_logged_in_field() {
        let output = Output {
            status: successful_exit_status(),
            stdout: br#"{"authMethod":"none"}"#.to_vec(),
            stderr: Vec::new(),
        };
        let AuthReadiness::Unreadable { message } = classify_claude_auth_output(&output) else {
            panic!("a missing loggedIn field must classify Unreadable, never Ready");
        };
        assert!(message.contains("loggedIn"));
    }

    /// Never forwards raw probe bytes: unparseable output containing an
    /// obviously credential-shaped string must classify `Unreadable`
    /// without that string ever appearing in the produced message (bd
    /// `conductor-5p8`).
    #[test]
    fn claude_auth_output_never_echoes_raw_probe_bytes_on_parse_failure() {
        let secret_marker = "sk-ant-definitely-not-a-real-secret-marker-12345";
        let output = Output {
            status: successful_exit_status(),
            stdout: format!("not json at all: {secret_marker}").into_bytes(),
            stderr: Vec::new(),
        };
        let AuthReadiness::Unreadable { message } = classify_claude_auth_output(&output) else {
            panic!("unparseable output must classify Unreadable, never Ready");
        };
        assert!(!message.contains(secret_marker), "classifier must never echo raw probe bytes");
        assert!(message.contains("unparseable"));
    }

    /// A field longer than the short printable bound this classifier
    /// enforces must never ride along into the operator message either.
    #[test]
    fn claude_auth_output_bounds_and_never_echoes_an_oversized_auth_method() {
        let secret_marker = "sk-ant-oversized-payload-riding-in-auth-method-field-xyz";
        let output = Output {
            status: successful_exit_status(),
            stdout: format!(r#"{{"loggedIn":false,"authMethod":"{secret_marker}"}}"#).into_bytes(),
            stderr: Vec::new(),
        };
        let AuthReadiness::NotAuthenticated { message } = classify_claude_auth_output(&output)
        else {
            panic!("logged-out output must classify NotAuthenticated");
        };
        assert!(!message.contains(secret_marker), "an oversized authMethod must never be echoed");
    }

    /// A probe hang must classify `Unreadable`, never `Ready` — proving a
    /// stall (the orchestrator measured 120-300s real hangs) cannot block
    /// dispatch indefinitely (bd `conductor-5p8`). Uses a real, credential-free
    /// `sh` subprocess; never touches the actual `claude` CLI.
    #[test]
    #[cfg(unix)]
    fn claude_auth_probe_timeout_classifies_unreadable_never_ready() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 30");
        let probe = run_bounded_command_with_timeout(&mut command, Duration::from_millis(100));
        let readiness = classify_claude_auth_probe(probe);
        let AuthReadiness::Unreadable { message } = readiness else {
            panic!("a probe hang must classify Unreadable, never Ready");
        };
        assert!(message.contains("bounded timeout"));
        assert!(message.contains("CLAUDE_CODE_OAUTH_TOKEN"));
    }

    /// A probe that fails to spawn (e.g. `claude` missing from PATH) must
    /// also fail closed to `Unreadable`, never `Ready`.
    #[test]
    fn claude_auth_probe_spawn_failure_classifies_unreadable_never_ready() {
        let mut command = Command::new("/definitely/not/a/real/undertake-test-binary-xyz");
        let probe = run_bounded_command_with_timeout(&mut command, Duration::from_secs(5));
        let readiness = classify_claude_auth_probe(probe);
        let AuthReadiness::Unreadable { message } = readiness else {
            panic!("a spawn failure must classify Unreadable, never Ready");
        };
        assert!(message.contains("could not be started"));
    }

    #[test]
    fn default_backend_auth_readiness_is_ready_for_backends_without_a_defined_probe() {
        for backend in [Backend::Pi, Backend::Omp, Backend::Agy, Backend::Codex] {
            assert_eq!(default_backend_auth_readiness(backend), AuthReadiness::Ready);
        }
    }

    #[cfg(unix)]
    fn successful_exit_status() -> ExitStatus {
        std::process::Command::new("true")
            .status()
            .expect("run true(1) for a real zero exit status")
    }

    #[test]
    #[cfg(unix)]
    fn bounded_command_kills_term_resistant_descendant_before_timeout_error() {
        let temp = TempDir::new("bounded-command-descendant");
        let marker = temp.path().join("descendant.pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(
                "sh -c 'trap \"\" TERM; echo $$ > \"$1\"; while :; do sleep 1; done' \
                 descendant \"$1\" & wait",
            )
            .arg("parent")
            .arg(&marker);

        let error = run_bounded_command_with_limits(
            &mut command,
            None,
            Duration::from_secs(1),
            Duration::from_millis(500),
        )
        .expect_err("TERM-resistant descendant must force KILL escalation");
        let descendant_pid = std::fs::read_to_string(&marker)
            .expect("descendant wrote pid")
            .trim()
            .parse::<u32>()
            .expect("descendant pid");

        assert!(
            error.is_timeout()
                && !error.leaves_process_state_uncertain()
                && !crate::quarantine::process_alive(descendant_pid),
            "timeout must be reported only after the TERM-resistant descendant is gone: {error}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn authenticated_worker_without_resource_limits_is_rejected_before_payload_exec() {
        let temp = TempDir::new("missing-worker-resource-limits");
        let marker = temp.path().join("payload-ran");
        let request = SpawnRequest {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("printf ran > {}", marker.display()),
            ],
            cwd: temp.path().to_path_buf(),
            env: Vec::new(),
            stdin: StdinMode::Null,
            sandbox_profile: None,
            worker_resource_limits: None,
            commit_receipt_socket: Some(commit_receipt_socket_path(&attempt_commit_identity())),
            stdout_path: temp.path().join("out.log"),
            stderr_path: temp.path().join("err.log"),
        };

        let Err(error) = CommandExec.spawn(&request) else {
            panic!("authenticated worker must not start without resource controls");
        };

        assert!(error.to_string().contains("resource limits"));
        assert!(!marker.exists(), "rejected worker payload must not execute");
    }

    #[test]
    #[cfg(unix)]
    fn worker_session_installs_hard_resource_limits_before_exec() {
        let temp = TempDir::new("worker-resource-limits");
        let stdout = temp.path().join("out.log");
        let stderr = temp.path().join("err.log");
        let limits = WorkerResourceLimits::new(
            7,
            256,
            32 * 1024 * 1024 * 1024,
            16 * 1024 * 1024,
        )
        .expect("valid test limits");
        let script = r#"
import errno, json, mmap, os, resource, subprocess
address_limit = resource.getrlimit(resource.RLIMIT_AS)
virtual_bytes = int(subprocess.check_output(
    ["/bin/ps", "-o", "vsz=", "-p", str(os.getpid())],
    text=True,
).strip()) * 1024
probe_bytes = address_limit[0] - virtual_bytes + 64 * 1024 * 1024
allocation_errno = None
try:
    mmap.mmap(-1, probe_bytes)
except (OSError, MemoryError) as error:
    allocation_errno = getattr(error, "errno", errno.ENOMEM)
print(json.dumps({
    "cpu": resource.getrlimit(resource.RLIMIT_CPU),
    "nproc": resource.getrlimit(resource.RLIMIT_NPROC),
    "as": address_limit,
    "fsize": resource.getrlimit(resource.RLIMIT_FSIZE),
    "allocation_errno": allocation_errno,
}))
"#;
        let request = SpawnRequest {
            argv: vec![
                "/usr/bin/python3".to_string(),
                "-c".to_string(),
                script.to_string(),
            ],
            cwd: temp.path().to_path_buf(),
            env: Vec::new(),
            stdin: StdinMode::Null,
            sandbox_profile: None,
            worker_resource_limits: Some(limits),
            commit_receipt_socket: Some(commit_receipt_socket_path(&attempt_commit_identity())),
            stdout_path: stdout.clone(),
            stderr_path: stderr.clone(),
        };

        let mut child = CommandExec.spawn(&request).expect("spawn limited worker");
        let status = child.wait().expect("wait limited worker");
        assert!(
            status.success(),
            "limited worker failed before payload exec: {}",
            std::fs::read_to_string(stderr).expect("read worker stderr")
        );
        let observed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(stdout).expect("read limits"))
                .expect("parse limits");

        assert_eq!(observed["cpu"], serde_json::json!([7, 7]));
        assert_eq!(observed["as"][0], observed["as"][1]);
        let address_limit = observed["as"][0].as_u64().expect("finite RLIMIT_AS");
        assert_ne!(address_limit, i64::MAX as u64);
        assert!(address_limit > 32_u64 * 1024 * 1024 * 1024);
        assert_eq!(observed["allocation_errno"], serde_json::json!(libc::ENOMEM));
        assert_eq!(
            observed["fsize"],
            serde_json::json!([16_777_216_u64, 16_777_216_u64])
        );
        assert_eq!(observed["nproc"][0], observed["nproc"][1]);
        assert!(
            observed["nproc"][0].as_u64().is_some_and(|value| value > 256),
            "RLIMIT_NPROC must include the sampled same-UID baseline plus concurrency headroom: {observed}"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn worker_process_limit_bounds_setsid_fork_exhaustion() {
        let temp = TempDir::new("worker-fork-limit");
        let stdout = temp.path().join("out.log");
        let limits = WorkerResourceLimits::new(
            30,
            8,
            32 * 1024 * 1024 * 1024,
            16 * 1024 * 1024,
        )
        .expect("valid test limits");
        let script = r#"
import errno, json, os, signal, time
children = []
failure = None
try:
    for _ in range(512):
        pid = os.fork()
        if pid == 0:
            os.setsid()
            time.sleep(30)
            os._exit(0)
        children.append(pid)
except OSError as error:
    failure = error.errno
finally:
    for pid in children:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    for pid in children:
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
print(json.dumps({"children": len(children), "errno": failure}))
raise SystemExit(0 if failure == errno.EAGAIN and len(children) < 512 else 1)
"#;
        let request = SpawnRequest {
            argv: vec![
                "/usr/bin/python3".to_string(),
                "-c".to_string(),
                script.to_string(),
            ],
            cwd: temp.path().to_path_buf(),
            env: Vec::new(),
            stdin: StdinMode::Null,
            sandbox_profile: None,
            worker_resource_limits: Some(limits),
            commit_receipt_socket: Some(commit_receipt_socket_path(&attempt_commit_identity())),
            stdout_path: stdout.clone(),
            stderr_path: temp.path().join("err.log"),
        };

        let mut child = CommandExec.spawn(&request).expect("spawn fork probe");
        let status = child.wait().expect("wait fork probe");
        let observed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(stdout).expect("read fork result"))
                .expect("parse fork result");

        assert!(
            status.success(),
            "setsid descendants must retain the inherited process ceiling: {observed}"
        );
        assert_eq!(
            observed["errno"],
            serde_json::json!(libc::EAGAIN),
            "fork must stop at RLIMIT_NPROC: {observed}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn command_exec_kill_terminates_descendant_processes_in_the_group() {
        // A worker CLI can fork children of its own (subshells, tool
        // invocations); if the timeout path only kills the direct child, a
        // grandchild can outlive it and keep writing to the repository
        // after Undertake has already declared the tree state. Spawning the
        // worker as the leader of its own process group and signaling
        // `-pid` on timeout must reach every descendant, not just the one
        // process std::process::Child knows about directly.
        let temp = TempDir::new("process-group-kill");
        let marker = temp.path().join("grandchild.pid");
        let request = SpawnRequest {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("sleep 30 & echo $! > {}; wait", marker.display()),
            ],
            cwd: temp.path().to_path_buf(),
            env: Vec::new(),
            stdin: StdinMode::Null,
            sandbox_profile: None,
            worker_resource_limits: None,
            commit_receipt_socket: None,
            stdout_path: temp.path().join("out.log"),
            stderr_path: temp.path().join("err.log"),
        };

        let exec = CommandExec;
        let mut child = exec.spawn(&request).expect("spawn worker shell");
        let grandchild_pid = wait_for_pid_marker(&marker);
        assert!(
            process_alive(grandchild_pid),
            "precondition: grandchild must actually be running before we try to kill it"
        );

        child.kill().expect("kill direct child");
        let _ = child.wait();

        assert!(
            !process_alive(grandchild_pid),
            "grandchild process must not survive killing the process group"
        );
    }

    #[cfg(unix)]
    fn wait_for_pid_marker(marker: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(marker) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(Instant::now() < deadline, "grandchild never wrote its pid");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Polls briefly since signal delivery/reaping is not synchronous with
    /// the `kill` call returning.
    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("spawn kill -0 probe");
            if !status.success() {
                return false;
            }
            if Instant::now() >= deadline {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
