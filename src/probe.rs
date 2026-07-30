//! The bootstrap provider probe (bead `conductor-bxb`).
//!
//! When every profile a `work` job binding pins is Musterroll `Unknown`,
//! [`crate::work_policy::resolve_candidates`] returns an empty pool and no
//! ordinary dispatch can ever generate the evidence that would change that —
//! a permanent wedge. This module breaks the deadlock as a preflight the CLI
//! layer runs *before* a bead is claimed or the repository is touched (see
//! `.docs/ai/phases/undertake-runner-contract.md`'s "the probe cannot be a
//! policy" section — `RunJob` has exactly four variants, so this is
//! deliberately not a fifth job kind or a [`crate::runner::JobPolicy`]).
//!
//! A probe is a bounded, tools-disabled, read-only invocation reusing the
//! existing [`dispatch::run_readonly`] machinery and
//! [`dispatch::readonly_argv_for_backend`] argv builders — the same pattern
//! `adversarial.rs` uses for its own read-only reviewer/judge probes
//! (`run_reviewer_attempt`, `adversarial.rs:1999`). Its stdout must exactly
//! match a fixed, non-secret challenge token; failure, timeout, or an
//! unparseable answer leaves the profile `Unknown` — a probe never marks a
//! provider bad, since absence of proof is not proof of absence. A validated
//! probe appends exact-scope runtime-success evidence through musterroll's
//! own bounded `success` observation (never `roster.toml`), each probe
//! recorded as one canonical `AttemptStarted`/`AttemptFinished` pair with
//! stage id `"provider_probe"` — distinct from `"work"`/`"verify"` stage
//! evidence — in a dedicated run created only for the probe set, so probe
//! evidence is never confused with task work.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::config::{Backend, ReasoningEffort};
use crate::dispatch;
use crate::job::JobBinding;
use crate::musterroll::{MusterrollClient, RosterSnapshot, SuccessObservationRequest};
use crate::run::{self, ApprovedExecution, ArtifactRef};
use crate::work_policy::DispatchFacts;

/// Stage id every probe attempt is recorded under, distinguishing it from a
/// job's own `"work"`/`"verify"` (or future `"review"`/`"plan"`) stages in
/// the same evidence vocabulary.
pub(crate) const PROBE_STAGE: &str = "provider_probe";
const PROBE_VALIDATED_OUTCOME: &str = "validated";
const PROBE_TOKEN: &str = "UNDERTAKE_PROBE_OK";
/// Hard cap on how long a bootstrap probe's success evidence is trusted for.
/// Well under musterroll's own `RUNTIME_SUCCESS_MAX_TTL_SECONDS` (1800s)
/// server-side cap; a probe proves only that the provider accepted a
/// trivial call recently, not that quota headroom exists.
const PROBE_EVIDENCE_TTL_SECS: i64 = 300;

fn challenge_prompt() -> String {
    format!(
        "This is an automated readiness probe. Respond with exactly this \
         token and nothing else — no punctuation, no explanation: {PROBE_TOKEN}"
    )
}

fn validate_probe_output(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).trim() == PROBE_TOKEN
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// One Unknown+enabled pinned profile eligible to be probed, resolved
/// against a live [`RosterSnapshot`] the same way
/// [`crate::work_policy::resolve_candidates`] resolves a dispatchable
/// candidate.
#[derive(Debug, Clone)]
pub(crate) struct ProbeCandidate {
    pub(crate) execution: ApprovedExecution,
    /// Musterroll's `availability_key` for the candidate's provider — the
    /// scope `musterroll success --provider` names.
    pub(crate) provider_key: String,
    pub(crate) model: String,
    pub(crate) backend: Backend,
    pub(crate) dispatch_id: String,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

/// Resolves `binding`'s pinned pool (primary then fallbacks) against
/// `snapshot`, keeping only a profile that is enabled and whose provider is
/// enabled and currently `Unknown` — never exhausted, manually deferred,
/// disabled, or stale-config, which all resolve to a provider state other
/// than `"unknown"` and stay ineligible and unprobed.
pub(crate) fn unknown_enabled_candidates(
    binding: &JobBinding,
    snapshot: &RosterSnapshot,
) -> Vec<ProbeCandidate> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    for profile_id in binding.pinned_profile_ids() {
        if !seen.insert(profile_id.to_string()) {
            continue;
        }
        let Some(profile) = snapshot
            .profiles
            .iter()
            .find(|profile| profile.profile_id == profile_id)
        else {
            continue;
        };
        if !profile.enabled {
            continue;
        }
        let Some(provider) = snapshot
            .providers
            .iter()
            .find(|provider| provider.provider_id == profile.provider_id)
        else {
            continue;
        };
        if !provider.enabled || provider.state != "unknown" {
            continue;
        }
        let Ok(backend) = crate::musterroll::backend_from_harness(&profile.harness) else {
            continue;
        };
        let reasoning_effort: Option<ReasoningEffort> = match profile
            .reasoning_effort
            .as_deref()
            .map(str::parse)
            .transpose()
        {
            Ok(value) => value,
            Err(_) => continue,
        };
        candidates.push(ProbeCandidate {
            execution: crate::role_routing::approved_execution(profile, provider),
            provider_key: provider.availability_key.clone(),
            model: profile.model.clone(),
            backend,
            dispatch_id: profile.dispatch_id.clone(),
            reasoning_effort,
        });
    }
    candidates
}

/// One probed candidate's classification. `Unvalidated` never implies the
/// provider is bad — only that this probe did not prove it usable; the
/// profile simply stays `Unknown`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeVerdict {
    Validated,
    Unvalidated { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeReport {
    pub(crate) profile_id: String,
    pub(crate) verdict: ProbeVerdict,
}

/// Runs one bounded, tools-disabled, read-only probe per `candidate`,
/// journaling each as a canonical `AttemptStarted`/`AttemptFinished` pair
/// (stage [`PROBE_STAGE`]) into `handle` — the same durable evidence
/// mechanism `crate::runner::AttemptRunner` uses for task attempts, so
/// scorecards can tell probe evidence from task work by stage id alone.
///
/// Resume-safe: a candidate already recorded with outcome
/// [`PROBE_VALIDATED_OUTCOME`] under [`PROBE_STAGE`] in `handle`'s existing
/// journal is reported `Validated` without spawning a second process or
/// appending a second evidence record — re-invoking this on the same handle
/// (e.g. after a crash) never re-approves or double-appends.
pub(crate) fn run_bootstrap_probes(
    handle: &mut run::RunHandle,
    exec: &dyn dispatch::Exec,
    musterroll: &dyn MusterrollClient,
    candidates: &[ProbeCandidate],
    timeout: Duration,
) -> run::Result<Vec<ProbeReport>> {
    let probe_dir = handle.dir().to_path_buf();
    let events = run::read_events(&handle.events_path())?;
    let already_validated: BTreeSet<String> = events
        .iter()
        .filter(|event| {
            event.kind == run::EventKind::AttemptFinished
                && event.outcome.as_deref() == Some(PROBE_VALIDATED_OUTCOME)
                && event
                    .invocation
                    .as_ref()
                    .is_some_and(|invocation| invocation.stage == PROBE_STAGE)
        })
        .filter_map(|event| event.profile_id.clone())
        .collect();
    let mut sequence = events.len() as u64;

    let mut reports = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if already_validated.contains(&candidate.execution.profile_id) {
            reports.push(ProbeReport {
                profile_id: candidate.execution.profile_id.clone(),
                verdict: ProbeVerdict::Validated,
            });
            continue;
        }
        let verdict = probe_one(
            handle,
            exec,
            musterroll,
            candidate,
            &probe_dir,
            timeout,
            &mut sequence,
        )?;
        reports.push(ProbeReport {
            profile_id: candidate.execution.profile_id.clone(),
            verdict,
        });
    }
    Ok(reports)
}

#[expect(
    clippy::too_many_lines,
    reason = "one linear probe attempt: record start, spawn read-only, classify, capture \
              evidence, record finish — splitting it would scatter the ordering it depends on"
)]
fn probe_one(
    handle: &mut run::RunHandle,
    exec: &dyn dispatch::Exec,
    musterroll: &dyn MusterrollClient,
    candidate: &ProbeCandidate,
    probe_dir: &Path,
    timeout: Duration,
    sequence: &mut u64,
) -> run::Result<ProbeVerdict> {
    let n = *sequence;
    *sequence += 1;
    let prompt = challenge_prompt();
    let input_sha256 = sha256_hex(prompt.as_bytes());

    handle.append_event(
        run::EventKind::AttemptStarted,
        run::EventInput {
            profile_id: Some(candidate.execution.profile_id.clone()),
            invocation: Some(run::InvocationEvidence {
                stage: PROBE_STAGE.to_string(),
                slot: 0,
                attempt: 1,
                execution: candidate.execution.clone(),
                input_sha256: input_sha256.clone(),
                output_sha256: None,
                duration_ms: None,
                tokens: None,
                retry_of: None,
            }),
            ..run::EventInput::default()
        },
    )?;

    let attempts_dir = handle.dir().join("attempts");
    let stdout_path = attempts_dir.join(format!("provider-probe-raw-{n:06}.out"));
    let stderr_path = attempts_dir.join(format!("provider-probe-raw-{n:06}.err"));

    let started = Instant::now();
    let (label, validated) = match dispatch::readonly_argv_for_backend(
        candidate.backend,
        &candidate.dispatch_id,
        candidate.reasoning_effort,
        &prompt,
        probe_dir,
    ) {
        Err(error) => (format!("probe argv construction failed: {error}"), false),
        Ok(argv) => {
            let spawn = dispatch::SpawnRequest {
                argv,
                cwd: probe_dir.to_path_buf(),
                env: Vec::new(),
                stdin: dispatch::StdinMode::Null,
                sandbox_profile: None,
                worker_resource_limits: None,
                commit_receipt_socket: None,
                stdout_path: stdout_path.clone(),
                stderr_path,
            };
            match dispatch::run_readonly(exec, &spawn, timeout, "provider-probe", &mut ()) {
                Err(error) => (format!("probe process error: {error}"), false),
                Ok(result) if result.status != dispatch::DispatchStatus::Success => (
                    format!("probe process did not succeed: {:?}", result.status),
                    false,
                ),
                Ok(_) => {
                    let stdout = std::fs::read(&stdout_path).unwrap_or_default();
                    if validate_probe_output(&stdout) {
                        ("probe token matched".to_string(), true)
                    } else {
                        (
                            "probe output did not match the expected token".to_string(),
                            false,
                        )
                    }
                }
            }
        }
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).ok();

    let artifact = if stdout_path.exists() {
        Some(handle.capture_artifact(
            &stdout_path,
            Path::new(&format!("attempts/provider-probe-{n:06}.out")),
        )?)
    } else {
        None
    };

    if !validated {
        finish_probe_attempt(
            handle,
            candidate,
            &input_sha256,
            artifact,
            label.clone(),
            duration_ms,
        )?;
        return Ok(ProbeVerdict::Unvalidated { detail: label });
    }

    let observed_at = chrono::Utc::now();
    let expires_at =
        (observed_at + chrono::Duration::seconds(PROBE_EVIDENCE_TTL_SECS)).to_rfc3339();
    let evidence = SuccessObservationRequest {
        provider: candidate.provider_key.clone(),
        model: candidate.model.clone(),
        evidence_id: format!("{}-probe-{n:06}", handle.run_id()),
        expires_at,
        source: "undertake-probe".to_string(),
        reason: "bootstrap probe validated a read-only invocation".to_string(),
    };
    match musterroll.success(&evidence) {
        Ok(()) => {
            finish_probe_attempt(
                handle,
                candidate,
                &input_sha256,
                artifact,
                PROBE_VALIDATED_OUTCOME.to_string(),
                duration_ms,
            )?;
            Ok(ProbeVerdict::Validated)
        }
        Err(error) => {
            let detail = format!("probe validated but evidence append failed: {error}");
            finish_probe_attempt(
                handle,
                candidate,
                &input_sha256,
                artifact,
                detail.clone(),
                duration_ms,
            )?;
            Ok(ProbeVerdict::Unvalidated { detail })
        }
    }
}

fn finish_probe_attempt(
    handle: &mut run::RunHandle,
    candidate: &ProbeCandidate,
    input_sha256: &str,
    artifact: Option<ArtifactRef>,
    outcome_label: String,
    duration_ms: Option<u64>,
) -> run::Result<()> {
    let output_sha256 = artifact.as_ref().map(|artifact| artifact.sha256.clone());
    handle.append_event(
        run::EventKind::AttemptFinished,
        run::EventInput {
            profile_id: Some(candidate.execution.profile_id.clone()),
            artifact_refs: artifact.into_iter().collect(),
            outcome: Some(outcome_label),
            invocation: Some(run::InvocationEvidence {
                stage: PROBE_STAGE.to_string(),
                slot: 0,
                attempt: 1,
                execution: candidate.execution.clone(),
                input_sha256: input_sha256.to_string(),
                output_sha256,
                duration_ms,
                tokens: None,
                retry_of: None,
            }),
            ..run::EventInput::default()
        },
    )
}

fn create_probe_run(
    state_dir: &Path,
    repo: &str,
    bead: &str,
    probe_candidates: &[ProbeCandidate],
) -> run::Result<run::RunHandle> {
    run::RunHandle::create(
        state_dir,
        run::RunJob::Work,
        run::NewRun {
            target: run::RunTarget {
                repo: repo.to_string(),
                bead: Some(bead.to_string()),
            },
            approved_profiles: probe_candidates
                .iter()
                .map(|candidate| candidate.execution.profile_id.clone())
                .collect(),
            musterroll_roster_artifact: None,
            roster_snapshot: None,
            limits: run::RunLimits {
                item_wall_clock_mins: None,
                max_attempts: Some(probe_candidates.len() as u64),
            },
            verifier: run::RunVerifier::default(),
            work: None,
            approval: None,
        },
    )
}

/// Outcome of resolving a `work` binding's candidate pool, with the
/// bootstrap probe run only when it was needed. `probed` is empty whenever
/// the pool was non-empty already, or nothing pinned was Unknown+enabled.
pub(crate) struct BootstrapOutcome {
    pub(crate) candidates: Vec<ApprovedExecution>,
    pub(crate) dispatch_facts: BTreeMap<String, DispatchFacts>,
    pub(crate) probed: Vec<ProbeReport>,
}

/// The CLI-layer preflight `undertake work` runs before job dispatch when
/// `binding`'s pool resolves empty against `initial_snapshot`
/// (bead `conductor-bxb`, pinned design point 8): probe any Unknown+enabled
/// pinned profile, append validated evidence, then re-resolve against a
/// fresh snapshot. Everything here runs before any bead claim or repo
/// mutation — the probe run this creates never touches `bd` and the target
/// repository is never checked out or written to.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the ports the bootstrap orchestration genuinely needs: where to persist \
              the probe run, what to probe, and how to execute and record it"
)]
pub(crate) fn resolve_with_bootstrap_probe(
    state_dir: &Path,
    repo: &str,
    bead: &str,
    binding: &JobBinding,
    initial_snapshot: &RosterSnapshot,
    exec: &dyn dispatch::Exec,
    musterroll: &dyn MusterrollClient,
    timeout: Duration,
) -> Result<BootstrapOutcome, String> {
    let (candidates, dispatch_facts) =
        crate::work_policy::resolve_candidates(binding, initial_snapshot)?;
    if !candidates.is_empty() {
        return Ok(BootstrapOutcome {
            candidates,
            dispatch_facts,
            probed: Vec::new(),
        });
    }

    let probe_candidates = unknown_enabled_candidates(binding, initial_snapshot);
    if probe_candidates.is_empty() {
        return Ok(BootstrapOutcome {
            candidates,
            dispatch_facts,
            probed: Vec::new(),
        });
    }

    let mut handle = create_probe_run(state_dir, repo, bead, &probe_candidates)
        .map_err(|error| format!("failed to create bootstrap probe run: {error}"))?;
    let probed = run_bootstrap_probes(&mut handle, exec, musterroll, &probe_candidates, timeout)
        .map_err(|error| format!("bootstrap probe failed: {error}"))?;
    handle
        .finish_with_verdict(
            "provider bootstrap probe completed",
            run::TerminalVerdict::Completed,
            Vec::new(),
        )
        .map_err(|error| format!("failed to finalize bootstrap probe run: {error}"))?;

    let refreshed = musterroll.roster_snapshot().map_err(|error| {
        format!("musterroll roster snapshot unavailable after bootstrap probing: {error}")
    })?;
    let (candidates, dispatch_facts) = crate::work_policy::resolve_candidates(binding, &refreshed)?;
    Ok(BootstrapOutcome {
        candidates,
        dispatch_facts,
        probed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::musterroll::{MusterrollError, RosterSnapshot, StatusReport};
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::time::SystemTime;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("undertake-probe-test-{label}-{nanos}"));
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

    fn binding(profile_ids: &[&str], fallback_ids: &[&str]) -> JobBinding {
        JobBinding {
            job: run::RunJob::Work,
            profile_ids: profile_ids.iter().copied().map(str::to_string).collect(),
            fallback_profile_ids: fallback_ids.iter().copied().map(str::to_string).collect(),
            mutation: crate::job::MutationPosture::RepositoryWrite,
            limits: run::RunLimits::default(),
            verifier: run::RunVerifier::default(),
            approval_required: false,
            role_policy: None,
        }
    }

    #[derive(Clone, Copy)]
    struct ProfileFixture {
        profile_id: &'static str,
        provider_id: &'static str,
        provider_state: &'static str,
        provider_enabled: bool,
        profile_enabled: bool,
    }

    fn snapshot_with(profiles: &[ProfileFixture]) -> RosterSnapshot {
        let mut providers_by_id: BTreeMap<&str, &ProfileFixture> = BTreeMap::new();
        for profile in profiles {
            providers_by_id
                .entry(profile.provider_id)
                .or_insert(profile);
        }
        let provider_json: Vec<_> = providers_by_id
            .values()
            .map(|profile| {
                let eligible = profile.provider_state == "healthy";
                serde_json::json!({
                    "provider_id": profile.provider_id,
                    "availability_key": profile.provider_id,
                    "enabled": profile.provider_enabled,
                    "state": profile.provider_state,
                    "availability": if profile.provider_state == "healthy" { "healthy" } else { "unknown" },
                    "checked_at": "2026-07-28T00:00:00Z",
                    "data_as_of": null,
                    "expires_at": null,
                    "reason": null,
                    "eligible": eligible && profile.provider_enabled,
                    "ineligibility_reason": null
                })
            })
            .collect();
        let profile_json: Vec<_> = profiles
            .iter()
            .map(|profile| {
                let provider_eligible =
                    profile.provider_state == "healthy" && profile.provider_enabled;
                let eligible = provider_eligible && profile.profile_enabled;
                serde_json::json!({
                    "profile_id": profile.profile_id,
                    "provider_id": profile.provider_id,
                    "model": format!("{}-model", profile.profile_id),
                    "harness": "pi",
                    "dispatch_id": profile.profile_id,
                    "reasoning_effort": null,
                    "tier": "junior",
                    "ceiling": "XL",
                    "efficiency": "lean",
                    "cost": 0.0,
                    "data_policy": "standard",
                    "enabled": profile.profile_enabled,
                    "roles": ["default", "task"],
                    "state": if eligible { "healthy" } else { "unknown" },
                    "eligible": eligible,
                    "ineligibility_reason": null
                })
            })
            .collect();
        crate::musterroll::parse_roster_snapshot(
            serde_json::json!({
                "schema": "musterroll/roster@2",
                "generated_at": "2026-07-28T00:00:00Z",
                "source_artifact": {
                    "path": "/fixture/musterroll-roster.toml",
                    "sha256": "a".repeat(64)
                },
                "policy_sha256": "b".repeat(64),
                "providers": provider_json,
                "profiles": profile_json
            })
            .to_string()
            .as_bytes(),
        )
        .expect("valid fixture snapshot")
    }

    /// A `MusterrollClient` whose `success` call flips the matching
    /// (provider, model) profile eligible on the snapshot the *next*
    /// `roster_snapshot()` call returns — mirroring what real musterroll
    /// evidence does, without depending on the `musterroll` binary.
    struct BootstrapMusterroll {
        snapshot: RefCell<RosterSnapshot>,
        success_calls: RefCell<Vec<SuccessObservationRequest>>,
        success_result: crate::musterroll::Result<()>,
        promote_on_success: bool,
    }

    impl BootstrapMusterroll {
        fn new(snapshot: RosterSnapshot) -> Self {
            Self {
                snapshot: RefCell::new(snapshot),
                success_calls: RefCell::new(Vec::new()),
                success_result: Ok(()),
                promote_on_success: true,
            }
        }

        fn with_success_failure(mut self) -> Self {
            self.success_result = Err(MusterrollError::command("fixture success failure"));
            self
        }

        fn success_calls(&self) -> Vec<SuccessObservationRequest> {
            self.success_calls.borrow().clone()
        }
    }

    impl MusterrollClient for BootstrapMusterroll {
        fn status(&self) -> crate::musterroll::Result<StatusReport> {
            Err(MusterrollError::unavailable("not used by the probe flow"))
        }

        fn roster_snapshot(&self) -> crate::musterroll::Result<RosterSnapshot> {
            Ok(self.snapshot.borrow().clone())
        }

        fn success(&self, request: &SuccessObservationRequest) -> crate::musterroll::Result<()> {
            self.success_calls.borrow_mut().push(request.clone());
            self.success_result.clone()?;
            if self.promote_on_success {
                let mut snapshot = self.snapshot.borrow_mut();
                for profile in &mut snapshot.profiles {
                    if profile.provider_id == request.provider && profile.model == request.model {
                        profile.eligible = true;
                    }
                }
                for provider in &mut snapshot.providers {
                    if provider.availability_key == request.provider {
                        provider.eligible = true;
                    }
                }
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    enum ScriptedAttempt {
        Success(&'static str),
        Failed,
        Garbage,
    }

    struct ScriptedExec {
        scripts: Mutex<HashMap<String, VecDeque<ScriptedAttempt>>>,
        spawned: Mutex<Vec<String>>,
    }

    impl ScriptedExec {
        fn new(scripts: HashMap<String, VecDeque<ScriptedAttempt>>) -> Self {
            Self {
                scripts: Mutex::new(scripts),
                spawned: Mutex::new(Vec::new()),
            }
        }

        fn spawned(&self) -> Vec<String> {
            self.spawned.lock().expect("lock").clone()
        }
    }

    struct ScriptedChild {
        status: dispatch::ProcessStatus,
    }

    impl dispatch::ChildProcess for ScriptedChild {
        fn wait_for(
            &mut self,
            _timeout: Duration,
        ) -> dispatch::Result<Option<dispatch::ProcessStatus>> {
            Ok(Some(self.status))
        }

        fn terminate(&mut self) -> dispatch::Result<()> {
            Ok(())
        }

        fn kill(&mut self) -> dispatch::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> dispatch::Result<dispatch::ProcessStatus> {
            Ok(self.status)
        }
    }

    impl dispatch::Exec for ScriptedExec {
        fn spawn(
            &self,
            request: &dispatch::SpawnRequest,
        ) -> dispatch::Result<Box<dyn dispatch::ChildProcess>> {
            // `--model <dispatch_id>` names the profile being probed; every
            // backend's readonly argv carries it.
            let model_index = request
                .argv
                .iter()
                .position(|arg| arg == "--model")
                .expect("probe argv always names --model");
            let dispatch_id = request.argv[model_index + 1].clone();
            self.spawned.lock().expect("lock").push(dispatch_id.clone());
            let attempt = self
                .scripts
                .lock()
                .expect("lock")
                .get_mut(&dispatch_id)
                .and_then(VecDeque::pop_front)
                .unwrap_or(ScriptedAttempt::Failed);
            std::fs::create_dir_all(request.stdout_path.parent().expect("parent")).expect("mkdir");
            let (stdout, status) = match attempt {
                ScriptedAttempt::Success(token) => {
                    (token.as_bytes().to_vec(), dispatch::ProcessStatus::code(0))
                }
                ScriptedAttempt::Garbage => {
                    (b"not the token".to_vec(), dispatch::ProcessStatus::code(0))
                }
                ScriptedAttempt::Failed => (Vec::new(), dispatch::ProcessStatus::code(1)),
            };
            std::fs::write(&request.stdout_path, stdout).expect("write stdout");
            std::fs::write(&request.stderr_path, b"").expect("write stderr");
            Ok(Box::new(ScriptedChild { status }))
        }
    }

    fn fixture(
        profile_id: &'static str,
        provider_id: &'static str,
        state: &'static str,
    ) -> ProfileFixture {
        ProfileFixture {
            profile_id,
            provider_id,
            provider_state: state,
            provider_enabled: true,
            profile_enabled: true,
        }
    }

    #[test]
    fn unknown_enabled_candidates_selects_only_unknown_and_enabled() {
        let binding = binding(
            &["healthy-worker"],
            &["unknown-worker", "exhausted-worker", "disabled-worker"],
        );
        let mut disabled = fixture("disabled-worker", "disabled-provider", "unknown");
        disabled.profile_enabled = false;
        let snapshot = snapshot_with(&[
            fixture("healthy-worker", "healthy-provider", "healthy"),
            fixture("unknown-worker", "unknown-provider", "unknown"),
            fixture("exhausted-worker", "exhausted-provider", "exhausted"),
            disabled,
        ]);

        let candidates = unknown_enabled_candidates(&binding, &snapshot);
        let ids: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.execution.profile_id.clone())
            .collect();
        assert_eq!(ids, vec!["unknown-worker".to_string()]);
    }

    #[test]
    fn all_unknown_bootstrap_probe_succeeds_and_dispatch_proceeds() {
        let temp = TempDir::new("all-unknown");
        let binding = binding(&["only-worker"], &[]);
        let snapshot = snapshot_with(&[fixture("only-worker", "only-provider", "unknown")]);
        let musterroll = BootstrapMusterroll::new(snapshot.clone());
        let mut scripts = HashMap::new();
        scripts.insert(
            "only-worker".to_string(),
            VecDeque::from([ScriptedAttempt::Success(PROBE_TOKEN)]),
        );
        let exec = ScriptedExec::new(scripts);

        let outcome = resolve_with_bootstrap_probe(
            temp.path(),
            "/fixture/repo",
            "bead-1",
            &binding,
            &snapshot,
            &exec,
            &musterroll,
            Duration::from_secs(5),
        )
        .expect("bootstrap probe resolves");

        assert_eq!(
            outcome.candidates.len(),
            1,
            "eligibility passes after probing"
        );
        assert_eq!(outcome.probed.len(), 1);
        assert_eq!(outcome.probed[0].verdict, ProbeVerdict::Validated);
        assert_eq!(musterroll.success_calls().len(), 1);
        assert_eq!(musterroll.success_calls()[0].provider, "only-provider");
    }

    #[test]
    fn mixed_providers_probes_only_unknown_enabled() {
        let temp = TempDir::new("mixed");
        let binding = binding(
            &["disabled-worker"],
            &["unknown-worker", "exhausted-worker"],
        );
        // `disabled-worker`'s profile is pinned but disabled, so the initial
        // pool is empty only because of that plus the exhausted fallback —
        // a realistic mixed pool, none of which is Unknown+enabled except
        // `unknown-worker`.
        let mut disabled = fixture("disabled-worker", "disabled-provider", "healthy");
        disabled.profile_enabled = false;
        let snapshot = snapshot_with(&[
            disabled,
            fixture("unknown-worker", "unknown-provider", "unknown"),
            fixture("exhausted-worker", "exhausted-provider", "exhausted"),
        ]);
        let musterroll = BootstrapMusterroll::new(snapshot.clone());
        let mut scripts = HashMap::new();
        scripts.insert(
            "unknown-worker".to_string(),
            VecDeque::from([ScriptedAttempt::Success(PROBE_TOKEN)]),
        );
        let exec = ScriptedExec::new(scripts);

        let outcome = resolve_with_bootstrap_probe(
            temp.path(),
            "/fixture/repo",
            "bead-2",
            &binding,
            &snapshot,
            &exec,
            &musterroll,
            Duration::from_secs(5),
        )
        .expect("bootstrap probe resolves");

        assert_eq!(
            exec.spawned(),
            vec!["unknown-worker".to_string()],
            "exhausted and disabled profiles must never be probed"
        );
        assert_eq!(outcome.probed.len(), 1);
        assert_eq!(outcome.probed[0].profile_id, "unknown-worker");
    }

    #[test]
    fn probe_failure_leaves_profile_unknown_and_blocks_without_claiming() {
        let temp = TempDir::new("failure");
        let binding = binding(&["only-worker"], &[]);
        let snapshot = snapshot_with(&[fixture("only-worker", "only-provider", "unknown")]);
        let musterroll = BootstrapMusterroll::new(snapshot.clone());
        let mut scripts = HashMap::new();
        scripts.insert(
            "only-worker".to_string(),
            VecDeque::from([ScriptedAttempt::Failed]),
        );
        let exec = ScriptedExec::new(scripts);

        let outcome = resolve_with_bootstrap_probe(
            temp.path(),
            "/fixture/repo",
            "bead-3",
            &binding,
            &snapshot,
            &exec,
            &musterroll,
            Duration::from_secs(5),
        )
        .expect("bootstrap probe resolves");

        assert!(outcome.candidates.is_empty(), "profile stays Unknown");
        assert!(
            musterroll.success_calls().is_empty(),
            "no evidence for a failed probe"
        );
        assert!(matches!(
            outcome.probed[0].verdict,
            ProbeVerdict::Unvalidated { .. }
        ));
    }

    #[test]
    fn probe_validated_but_evidence_append_failure_leaves_profile_unknown() {
        let temp = TempDir::new("evidence-append-failure");
        let binding = binding(&["only-worker"], &[]);
        let snapshot = snapshot_with(&[fixture("only-worker", "only-provider", "unknown")]);
        let musterroll = BootstrapMusterroll::new(snapshot.clone()).with_success_failure();
        let mut scripts = HashMap::new();
        scripts.insert(
            "only-worker".to_string(),
            VecDeque::from([ScriptedAttempt::Success(PROBE_TOKEN)]),
        );
        let exec = ScriptedExec::new(scripts);

        let outcome = resolve_with_bootstrap_probe(
            temp.path(),
            "/fixture/repo",
            "bead-3b",
            &binding,
            &snapshot,
            &exec,
            &musterroll,
            Duration::from_secs(5),
        )
        .expect("bootstrap probe resolves");

        assert_eq!(
            musterroll.success_calls().len(),
            1,
            "evidence append was attempted"
        );
        assert!(
            outcome.candidates.is_empty(),
            "a failed evidence append must not be treated as eligibility"
        );
        assert!(matches!(
            outcome.probed[0].verdict,
            ProbeVerdict::Unvalidated { .. }
        ));
    }

    #[test]
    fn probe_garbage_output_leaves_profile_unknown() {
        let temp = TempDir::new("garbage");
        let binding = binding(&["only-worker"], &[]);
        let snapshot = snapshot_with(&[fixture("only-worker", "only-provider", "unknown")]);
        let musterroll = BootstrapMusterroll::new(snapshot.clone());
        let mut scripts = HashMap::new();
        scripts.insert(
            "only-worker".to_string(),
            VecDeque::from([ScriptedAttempt::Garbage]),
        );
        let exec = ScriptedExec::new(scripts);

        let outcome = resolve_with_bootstrap_probe(
            temp.path(),
            "/fixture/repo",
            "bead-4",
            &binding,
            &snapshot,
            &exec,
            &musterroll,
            Duration::from_secs(5),
        )
        .expect("bootstrap probe resolves");

        assert!(outcome.candidates.is_empty());
        assert!(musterroll.success_calls().is_empty());
    }

    #[test]
    fn probe_evidence_lands_as_canonical_attempt_records_distinct_from_task_work() {
        let temp = TempDir::new("evidence");
        let candidate = ProbeCandidate {
            execution: ApprovedExecution {
                profile_id: "only-worker".to_string(),
                provider_id: "only-provider".to_string(),
                availability_key: "only-provider".to_string(),
                execution_key: "only-worker-exec".to_string(),
            },
            provider_key: "only-provider".to_string(),
            model: "only-worker-model".to_string(),
            backend: Backend::Pi,
            dispatch_id: "only-worker".to_string(),
            reasoning_effort: None,
        };
        let mut handle = create_probe_run(
            temp.path(),
            "/fixture/repo",
            "bead-5",
            std::slice::from_ref(&candidate),
        )
        .expect("create probe run");
        let musterroll = BootstrapMusterroll::new(snapshot_with(&[fixture(
            "only-worker",
            "only-provider",
            "unknown",
        )]));
        let mut scripts = HashMap::new();
        scripts.insert(
            "only-worker".to_string(),
            VecDeque::from([ScriptedAttempt::Success(PROBE_TOKEN)]),
        );
        let exec = ScriptedExec::new(scripts);

        run_bootstrap_probes(
            &mut handle,
            &exec,
            &musterroll,
            &[candidate],
            Duration::from_secs(5),
        )
        .expect("probe runs");

        let events = run::read_events(&handle.events_path()).expect("read events");
        let probe_events: Vec<_> = events
            .iter()
            .filter(|event| {
                event
                    .invocation
                    .as_ref()
                    .is_some_and(|invocation| invocation.stage == PROBE_STAGE)
            })
            .collect();
        assert_eq!(
            probe_events.len(),
            2,
            "one AttemptStarted + one AttemptFinished"
        );
        assert!(probe_events
            .iter()
            .all(|event| event.invocation.as_ref().unwrap().stage == PROBE_STAGE));
        assert!(events.iter().all(|event| event
            .invocation
            .as_ref()
            .is_none_or(|invocation| invocation.stage != "work")));
    }

    #[test]
    fn resume_does_not_reapprove_or_double_append_evidence() {
        let temp = TempDir::new("resume");
        let candidate = ProbeCandidate {
            execution: ApprovedExecution {
                profile_id: "only-worker".to_string(),
                provider_id: "only-provider".to_string(),
                availability_key: "only-provider".to_string(),
                execution_key: "only-worker-exec".to_string(),
            },
            provider_key: "only-provider".to_string(),
            model: "only-worker-model".to_string(),
            backend: Backend::Pi,
            dispatch_id: "only-worker".to_string(),
            reasoning_effort: None,
        };
        let mut handle = create_probe_run(
            temp.path(),
            "/fixture/repo",
            "bead-6",
            std::slice::from_ref(&candidate),
        )
        .expect("create probe run");
        let musterroll = BootstrapMusterroll::new(snapshot_with(&[fixture(
            "only-worker",
            "only-provider",
            "unknown",
        )]));
        let mut scripts = HashMap::new();
        scripts.insert(
            "only-worker".to_string(),
            VecDeque::from([ScriptedAttempt::Success(PROBE_TOKEN)]),
        );
        let exec = ScriptedExec::new(scripts);

        run_bootstrap_probes(
            &mut handle,
            &exec,
            &musterroll,
            std::slice::from_ref(&candidate),
            Duration::from_secs(5),
        )
        .expect("first probe run");
        assert_eq!(musterroll.success_calls().len(), 1);

        // Simulate a crash-then-resume: re-open the same run directory and
        // re-invoke the probe phase on the same candidate set. No script is
        // queued for a second spawn, so a re-probe would fail loudly if one
        // were attempted.
        let mut reopened =
            run::RunHandle::open(temp.path(), handle.run_id()).expect("reopen probe run");
        let reports = run_bootstrap_probes(
            &mut reopened,
            &exec,
            &musterroll,
            &[candidate],
            Duration::from_secs(5),
        )
        .expect("resumed probe run does not re-probe");

        assert_eq!(reports[0].verdict, ProbeVerdict::Validated);
        assert_eq!(
            musterroll.success_calls().len(),
            1,
            "resume must not double-append evidence"
        );
        assert_eq!(
            exec.spawned().len(),
            1,
            "resume must not re-approve a second spawn"
        );
    }
}
