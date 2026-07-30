//! Musterroll `status --json` client and budget decision helpers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io;


use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

const SCHEMA: &str = "musterroll/status@2";
const ROSTER_SCHEMA: &str = "musterroll/roster@2";
const MAX_STATUS_AGE_MINS: i64 = 5;
const NEAR_EXHAUSTED_PERCENT: f64 = 90.0;
const PROVIDERS: [&str; 4] = ["anthropic", "codex", "opencode-go", "agy"];

pub(crate) type Result<T> = std::result::Result<T, MusterrollError>;

pub(crate) trait MusterrollClient {
    fn status(&self) -> Result<StatusReport>;

    /// Read the authoritative, resolved Musterroll execution-profile snapshot.
    /// Implementors without this newer seam remain read-only legacy clients;
    /// callers must fail closed rather than fabricating a roster.
    fn roster_snapshot(&self) -> Result<RosterSnapshot> {
        Err(MusterrollError::unavailable(
            "musterroll roster snapshot unavailable",
        ))
    }

    #[allow(dead_code)]
    fn observe(&self, _request: &ObservationRequest) -> Result<()> {
        Err(MusterrollError::unavailable("musterroll observation unavailable"))
    }

    /// Append a bounded, exact-scope runtime-success attestation via
    /// `musterroll success` (bead `conductor-bxb`). Unlike [`Self::observe`],
    /// this can only ever promote one exact (provider, model) profile — never
    /// a provider-wide health signal — and its evidence caps out at
    /// `RUNTIME_SUCCESS_MAX_TTL_SECONDS` regardless of the requested
    /// `expires_at`, both enforced by musterroll itself.
    #[allow(dead_code)]
    fn success(&self, _request: &SuccessObservationRequest) -> Result<()> {
        Err(MusterrollError::unavailable(
            "musterroll success observation unavailable",
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MusterrollErrorKind {
    Unavailable,
    Command,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MusterrollError {
    kind: MusterrollErrorKind,
    message: String,
}

impl MusterrollError {
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: MusterrollErrorKind::Unavailable,
            message: message.into(),
        }
    }

    pub(crate) fn command(message: impl Into<String>) -> Self {
        Self {
            kind: MusterrollErrorKind::Command,
            message: message.into(),
        }
    }
    fn json(message: impl Into<String>) -> Self {
        Self {
            kind: MusterrollErrorKind::Json,
            message: message.into(),
        }
    }

    pub(crate) const fn is_unavailable(&self) -> bool {
        matches!(self.kind, MusterrollErrorKind::Unavailable)
    }
}

impl fmt::Display for MusterrollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MusterrollError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Availability {
    Healthy,
    Caution,
    Exhausted,
    Unknown,
}

impl fmt::Display for Availability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => f.write_str("healthy"),
            Self::Caution => f.write_str("caution"),
            Self::Exhausted => f.write_str("exhausted"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub(crate) struct Window {
    pub(crate) label: String,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) percent: Option<f64>,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) reset_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderStatus {
    pub(crate) availability: Availability,
    pub(crate) source: String,
    pub(crate) checked_at: String,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) data_as_of: Option<String>,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) expires_at: Option<String>,
    pub(crate) windows: Vec<Window>,
    #[serde(deserialize_with = "deserialize_nullable")]
    pub(crate) reason: Option<String>,
    pub(crate) extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatusReport {
    pub(crate) schema: String,
    pub(crate) checked_at: String,
    pub(crate) providers: BTreeMap<String, ProviderStatus>,
}

/// Immutable source identity carried by a Musterroll v2 roster snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RosterSourceArtifact {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

/// Read-only `musterroll/roster@2` snapshot consumed by Undertake. Snapshot bytes
/// are retained outside its serialized shape so a run can copy exactly what
/// Musterroll emitted rather than reopening the mutable source roster.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RosterSnapshot {
    schema: String,
    generated_at: String,
    source_artifact: RosterSourceArtifact,
    policy_sha256: String,
    pub(crate) providers: Vec<RosterProvider>,
    pub(crate) profiles: Vec<RosterProfile>,
    #[serde(skip)]
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RosterProvider {
    pub(crate) provider_id: String,
    pub(crate) availability_key: String,
    pub(crate) enabled: bool,
    pub(crate) state: String,
    pub(crate) availability: Availability,
    pub(crate) checked_at: String,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub(crate) data_as_of: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub(crate) expires_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub(crate) reason: Option<String>,
    pub(crate) eligible: bool,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub(crate) ineligibility_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RosterProfile {
    pub(crate) profile_id: String,
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) harness: String,
    pub(crate) dispatch_id: String,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) tier: String,
    pub(crate) ceiling: String,
    pub(crate) efficiency: String,
    pub(crate) cost: f64,
    pub(crate) data_policy: String,
    pub(crate) enabled: bool,
    pub(crate) roles: Vec<String>,
    pub(crate) state: String,
    pub(crate) eligible: bool,
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub(crate) ineligibility_reason: Option<String>,
}

/// Parses, authenticates, and validates a `musterroll/roster@2` snapshot before
/// any profile becomes eligible for Undertake routing. The source artifact is
/// provenance only: authorization must use the captured bytes, never reopen it.
pub(crate) fn parse_roster_snapshot(bytes: &[u8]) -> Result<RosterSnapshot> {
    let mut snapshot: RosterSnapshot = serde_json::from_slice(bytes).map_err(|error| {
        MusterrollError::json(format!("failed to parse musterroll roster snapshot: {error}"))
    })?;
    snapshot.validate()?;
    snapshot.bytes = bytes.to_vec();
    Ok(snapshot)
}

impl RosterSnapshot {
    #[expect(
        clippy::too_many_lines,
        reason = "linear fail-closed validation keeps each evidence rejection explicit"
    )]
    fn validate(&self) -> Result<()> {
        if self.schema != ROSTER_SCHEMA {
            return Err(MusterrollError::json(format!(
                "unsupported musterroll roster schema {}",
                self.schema
            )));
        }
        parse_time("roster generated_at", &self.generated_at)
            .map_err(|error| MusterrollError::json(error.clone()))?;
        let source_path = std::path::Path::new(&self.source_artifact.path);
        if !source_path.is_absolute() {
            return Err(MusterrollError::json(
                "musterroll roster source artifact path must be absolute",
            ));
        }
        if !is_sha256(&self.source_artifact.sha256) {
            return Err(MusterrollError::json(
                "musterroll roster source artifact sha256 must be lowercase 64-hex",
            ));
        }
        if !is_sha256(&self.policy_sha256) {
            return Err(MusterrollError::json(
                "musterroll roster policy_sha256 must be lowercase 64-hex",
            ));
        }

        let mut providers = HashMap::new();
        for provider in &self.providers {
            if provider.provider_id.is_empty()
                || provider.availability_key.is_empty()
                || !matches!(
                    provider.state.as_str(),
                    "healthy" | "exhausted" | "unknown" | "stale" | "manually-disabled"
                )
            {
                return Err(MusterrollError::json("malformed musterroll roster provider"));
            }
            parse_time("roster provider checked_at", &provider.checked_at)
                .map_err(|error| MusterrollError::json(error.clone()))?;
            if let Some(data_as_of) = provider.data_as_of.as_deref() {
                parse_time("roster provider data_as_of", data_as_of)
                    .map_err(|error| MusterrollError::json(error.clone()))?;
            }
            if let Some(expires_at) = provider.expires_at.as_deref() {
                parse_time("roster provider expires_at", expires_at)
                    .map_err(|error| MusterrollError::json(error.clone()))?;
            }
            if provider.eligible
                && (!provider.enabled
                    || provider.state != "healthy"
                    || !matches!(
                        provider.availability,
                        Availability::Healthy | Availability::Caution
                    ))
            {
                return Err(MusterrollError::json(
                    "eligible musterroll roster provider is disabled or unavailable",
                ));
            }
            if providers
                .insert(provider.provider_id.as_str(), provider)
                .is_some()
            {
                return Err(MusterrollError::json("duplicate musterroll roster provider_id"));
            }
        }

        let mut profile_ids = HashSet::new();
        let mut execution_keys = HashSet::new();
        for profile in &self.profiles {
            if !is_identifier(&profile.profile_id)
                || !is_identifier(&profile.provider_id)
                || profile.model.trim().is_empty()
                || profile.harness.trim().is_empty()
                || profile.dispatch_id.trim().is_empty()
                || !providers.contains_key(profile.provider_id.as_str())
                || !profile.cost.is_finite()
                || profile.cost < 0.0
                || !matches!(
                    profile.state.as_str(),
                    "healthy" | "exhausted" | "unknown" | "stale" | "manually-disabled"
                )
                || !matches!(
                    profile.data_policy.as_str(),
                    "standard" | "zero-retention" | "local-only" | "trains-input"
                )
            {
                return Err(MusterrollError::json("malformed musterroll roster profile"));
            }
            profile
                .tier
                .parse::<crate::config::Tier>()
                .map_err(|error| {
                    MusterrollError::json(format!("malformed musterroll roster profile tier: {error}"))
                })?;
            profile
                .ceiling
                .parse::<crate::config::Ceiling>()
                .map_err(|error| {
                    MusterrollError::json(format!("malformed musterroll roster profile ceiling: {error}"))
                })?;
            profile
                .efficiency
                .parse::<crate::config::Efficiency>()
                .map_err(|error| {
                    MusterrollError::json(format!(
                        "malformed musterroll roster profile efficiency: {error}"
                    ))
                })?;
            backend_from_harness(&profile.harness)?;
            if let Some(reasoning_effort) = profile.reasoning_effort.as_deref() {
                reasoning_effort
                    .parse::<crate::config::ReasoningEffort>()
                    .map_err(|error| {
                        MusterrollError::json(format!(
                            "malformed musterroll roster profile reasoning effort: {error}"
                        ))
                    })?;
            }
            let provider = providers
                .get(profile.provider_id.as_str())
                .expect("profile provider was checked above");
            if profile.eligible
                && (!profile.enabled
                    || profile.state != "healthy"
                    || !provider.enabled
                    || !provider.eligible)
            {
                return Err(MusterrollError::json(
                    "eligible musterroll roster profile has a disabled or unavailable provider",
                ));
            }
            if !profile_ids.insert(profile.profile_id.as_str()) {
                return Err(MusterrollError::json("duplicate musterroll roster profile_id"));
            }
            let execution_key = (
                profile.provider_id.as_str(),
                profile.model.as_str(),
                profile.harness.as_str(),
                profile.dispatch_id.as_str(),
                profile.reasoning_effort.as_deref().unwrap_or_default(),
            );
            if !execution_keys.insert(execution_key) {
                return Err(MusterrollError::json(
                    "duplicate musterroll roster execution identity",
                ));
            }
            if profile.roles.is_empty()
                || profile.roles.windows(2).any(|pair| pair[0] >= pair[1])
                || profile.roles.iter().any(|role| !is_identifier(role))
            {
                return Err(MusterrollError::json(
                    "musterroll roster profile roles must be sorted, unique identifiers",
                ));
            }
        }
        Ok(())
    }

    /// Resolve Undertake-owned job fallback policy against this snapshot.
    /// Musterroll profiles supply identity/capability/availability, while policy
    /// only orders already-known profile IDs after a runtime retryable error.
    pub(crate) fn roster_entries_with_fallbacks(
        &self,
        job_fallbacks: &[crate::config::JobFallbackPolicy],
    ) -> Result<Vec<crate::config::RosterEntry>> {
        let known_profile_ids = self
            .profiles
            .iter()
            .map(|profile| profile.profile_id.as_str())
            .collect::<HashSet<_>>();
        for policy in job_fallbacks {
            if !known_profile_ids.contains(policy.profile_id.as_str()) {
                return Err(MusterrollError::json(format!(
                    "Undertake job fallback references missing Musterroll profile {}",
                    policy.profile_id
                )));
            }
            for fallback in &policy.fallback_profile_ids {
                if !known_profile_ids.contains(fallback.as_str()) {
                    return Err(MusterrollError::json(format!(
                        "Undertake job fallback for {} references missing Musterroll profile {fallback}",
                        policy.profile_id
                    )));
                }
            }
        }

        let eligible_profile_ids = self
            .profiles
            .iter()
            .filter(|profile| profile.enabled && profile.eligible)
            .map(|profile| profile.profile_id.as_str())
            .collect::<HashSet<_>>();
        let mut entries = self
            .profiles
            .iter()
            .filter(|profile| profile.enabled && profile.eligible)
            .map(|profile| {
                let backend = backend_from_harness(&profile.harness)?;
                let reasoning_effort = profile
                    .reasoning_effort
                    .as_deref()
                    .map(str::parse)
                    .transpose()
                    .map_err(|error| {
                        MusterrollError::json(format!(
                            "malformed musterroll roster profile reasoning effort: {error}"
                        ))
                    })?;
                let cost = if profile.data_policy == "trains-input" {
                    crate::config::Cost::FreeTrainsInput
                } else if profile.cost == 0.0 {
                    crate::config::Cost::Free
                } else {
                    crate::config::Cost::Paid
                };
                Ok(crate::config::RosterEntry {
                    name: profile.profile_id.clone(),
                    tier: profile.tier.parse().map_err(|error| {
                        MusterrollError::json(format!("malformed musterroll roster profile tier: {error}"))
                    })?,
                    ceiling: profile.ceiling.parse().map_err(|error| {
                        MusterrollError::json(format!(
                            "malformed musterroll roster profile ceiling: {error}"
                        ))
                    })?,
                    efficiency: profile.efficiency.parse().map_err(|error| {
                        MusterrollError::json(format!(
                            "malformed musterroll roster profile efficiency: {error}"
                        ))
                    })?,
                    backend,
                    dispatch_id: profile.dispatch_id.clone(),
                    reasoning_effort,
                    provider: profile.provider_id.clone(),
                    cost,
                    fallback: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        for policy in job_fallbacks {
            let Some(entry) = entries
                .iter_mut()
                .find(|entry| entry.name == policy.profile_id)
            else {
                continue;
            };
            entry.fallback = policy
                .fallback_profile_ids
                .iter()
                .filter(|profile_id| eligible_profile_ids.contains(profile_id.as_str()))
                .cloned()
                .collect();
        }
        Ok(entries)
    }

    pub(crate) fn policy_sha256(&self) -> &str {
        &self.policy_sha256
    }

    pub(crate) fn snapshot_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn source_artifact(&self) -> &RosterSourceArtifact {
        &self.source_artifact
    }
}
pub(crate) fn backend_from_harness(harness: &str) -> Result<crate::config::Backend> {
    match harness {
        "claude-code" => Ok(crate::config::Backend::Claude),
        "pi" => Ok(crate::config::Backend::Pi),
        "omp" => Ok(crate::config::Backend::Omp),
        "agy" => Ok(crate::config::Backend::Agy),
        "codex" => Ok(crate::config::Backend::Codex),
        _ => Err(MusterrollError::json(format!(
            "unsupported musterroll roster harness {harness:?}"
        ))),
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Runtime roster resolution. Production and compatibility surfaces consume a
/// validated Musterroll v2 snapshot; static `[[roster]]` entries cannot authorize
/// a run.
pub(crate) fn resolve_roster<C: MusterrollClient + ?Sized>(
    cfg: &crate::config::Config,
    client: &C,
) -> Result<ResolvedRoster> {
    let snapshot = client.roster_snapshot()?;
    let roster = snapshot.roster_entries_with_fallbacks(&cfg.job_fallbacks)?;
    Ok(ResolvedRoster {
        roster,
        source_artifact: snapshot.source_artifact.clone(),
        policy_sha256: snapshot.policy_sha256.clone(),
        snapshot_bytes: snapshot.bytes,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedRoster {
    pub(crate) roster: Vec<crate::config::RosterEntry>,
    pub(crate) source_artifact: RosterSourceArtifact,
    pub(crate) policy_sha256: String,
    pub(crate) snapshot_bytes: Vec<u8>,
}

fn deserialize_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetAction {
    Proceed,
    SpendCautiously,
    Defer,
    StaticCaps,
}

impl BudgetAction {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Proceed => "proceed",
            Self::SpendCautiously => "spend-cautiously",
            Self::Defer => "defer",
            Self::StaticCaps => "static-caps",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BudgetDecision {
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
    pub(crate) availability: Option<Availability>,
    pub(crate) source: Option<String>,
    pub(crate) checked_at: Option<String>,
    pub(crate) data_as_of: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) expiry_basis: Option<String>,
    pub(crate) action: BudgetAction,
    pub(crate) summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub(crate) enum ObservationExpiryBasis {
    ProviderReset,
    LocalCooldown,
}

impl ObservationExpiryBasis {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ProviderReset => "provider-reset",
            Self::LocalCooldown => "local-cooldown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub(crate) enum RuntimeLimitReason {
    Http429,
    QuotaExceeded,
    RateLimit,
    SessionLimit,
}

impl RuntimeLimitReason {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Http429 => "runtime HTTP 429",
            Self::QuotaExceeded => "runtime quota exceeded",
            Self::RateLimit => "runtime rate limit",
            Self::SessionLimit => "runtime session limit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservationRequest {
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
    pub(crate) expires_at: String,
    pub(crate) expiry_basis: ObservationExpiryBasis,
    pub(crate) reason: RuntimeLimitReason,
}

/// Durable, bounded evidence derived from a canonical runtime provider limit.
/// The owning run event supplies the run id and cycle work state; this record
/// binds the observed provider condition to the selected profile and model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeLimitEvidence {
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
    pub(crate) profile: String,
    pub(crate) expires_at: String,
    pub(crate) expiry_basis: ObservationExpiryBasis,
    pub(crate) reason: RuntimeLimitReason,
}

/// Request for a bounded, machine-generated `musterroll success` attestation
/// (bead `conductor-bxb`'s bootstrap probe). `provider` is musterroll's
/// `availability_key`, and `model` must be the exact model configured for
/// that provider in the roster — musterroll rejects an unrecognized
/// (provider, model) pair rather than silently widening scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SuccessObservationRequest {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) evidence_id: String,
    pub(crate) expires_at: String,
    pub(crate) source: String,
    pub(crate) reason: String,
}

impl ObservationRequest {
    pub(crate) fn runtime_limit(
        provider: impl Into<String>,
        model: Option<String>,
        expires_at: impl Into<String>,
        expiry_basis: ObservationExpiryBasis,
        reason: RuntimeLimitReason,
    ) -> Self {
        let provider = provider.into();
        Self {
            provider: canonical_provider_key(&provider).to_string(),
            model,
            expires_at: expires_at.into(),
            expiry_basis,
            reason,
        }
    }

    pub(crate) fn evidence(&self, profile: impl Into<String>) -> RuntimeLimitEvidence {
        RuntimeLimitEvidence {
            provider: self.provider.clone(),
            model: self.model.clone(),
            profile: profile.into(),
            expires_at: self.expires_at.clone(),
            expiry_basis: self.expiry_basis,
            reason: self.reason,
        }
    }
}

/// The 512-character ceiling on a formatted command failure's stderr/stdout
/// detail. `BoundedCommand` already caps raw stderr at 256 KiB; this is a
/// second, much tighter cap purely for what a human-readable error string
/// embeds, so one hostile or oversized line can never dominate an error
/// surfaced up through `Result<_, MusterrollError>`.
const FAILURE_DETAIL_CAP: usize = 512;

#[derive(Debug, Clone, Default)]
pub(crate) struct CommandMusterrollClient {
    /// A shared shutdown signal every command this client runs checks
    /// inside `BoundedCommand::run`'s wait loop, terminating promptly
    /// instead of running to its full timeout. `None` for every ordinary
    /// caller; set only by the dashboard's Musterroll worker thread (see
    /// `dashboard::runtime::spawn_musterroll_worker`), so `run_dashboard`
    /// can join that thread on shutdown without waiting out an in-flight
    /// command's up-to-60-second deadline.
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl CommandMusterrollClient {
    pub(crate) const fn new() -> Self {
        Self { cancel: None }
    }

    /// Attaches a shared cancellation flag; see the `cancel` field doc.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub(crate) fn with_cancel(cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            cancel: Some(cancel),
        }
    }

    fn attach_cancel(
        &self,
        command: crate::process::BoundedCommand,
    ) -> crate::process::BoundedCommand {
        match &self.cancel {
            Some(cancel) => command.cancel_flag(std::sync::Arc::clone(cancel)),
            None => command,
        }
    }
}

impl MusterrollClient for CommandMusterrollClient {
    fn status(&self) -> Result<StatusReport> {
        let outcome = self
            .attach_cancel(
                crate::process::BoundedCommand::new("musterroll")
                    .args(["status", "--json"])
                    .stdout_cap(4 * 1024 * 1024)
                    .stderr_cap(256 * 1024)
                    .timeout(std::time::Duration::from_secs(60)),
            )
            .run()
            .map_err(|error| spawn_error("musterroll status --json", &error))?;

        if outcome.timed_out() || outcome.stdout_truncated {
            return Err(MusterrollError::command(
                "musterroll status --json timed out or exceeded output bounds",
            ));
        }

        if outcome.exit_code != Some(0) {
            return Err(MusterrollError::command(command_failure_message(
                "musterroll status --json",
                &outcome,
            )));
        }

        serde_json::from_slice(&outcome.stdout).map_err(|error| {
            MusterrollError::json(format!("failed to parse musterroll status --json: {error}"))
        })
    }

    fn roster_snapshot(&self) -> Result<RosterSnapshot> {
        let outcome = self
            .attach_cancel(
                crate::process::BoundedCommand::new("musterroll")
                    .args(["roster", "snapshot", "--json"])
                    .stdout_cap(4 * 1024 * 1024)
                    .stderr_cap(256 * 1024)
                    .timeout(std::time::Duration::from_secs(60)),
            )
            .run()
            .map_err(|error| spawn_error("musterroll roster snapshot --json", &error))?;

        if outcome.timed_out() || outcome.stdout_truncated {
            return Err(MusterrollError::command(
                "musterroll roster snapshot --json timed out or exceeded output bounds",
            ));
        }

        if outcome.exit_code != Some(0) {
            return Err(MusterrollError::command(command_failure_message(
                "musterroll roster snapshot --json",
                &outcome,
            )));
        }

        parse_roster_snapshot(&outcome.stdout)
    }

    fn observe(&self, request: &ObservationRequest) -> Result<()> {
        let args = observation_args(request);
        let outcome = self
            .attach_cancel(
                crate::process::BoundedCommand::new("musterroll")
                    .args(&args)
                    .stdout_cap(4 * 1024 * 1024)
                    .stderr_cap(256 * 1024)
                    .timeout(std::time::Duration::from_secs(60)),
            )
            .run()
            .map_err(|error| spawn_error("musterroll observe", &error))?;

        if outcome.timed_out() || outcome.stdout_truncated {
            return Err(MusterrollError::command(
                "musterroll observe timed out or exceeded output bounds",
            ));
        }

        if outcome.exit_code != Some(0) {
            return Err(MusterrollError::command(command_failure_message(
                "musterroll observe",
                &outcome,
            )));
        }
        Ok(())
    }

    fn success(&self, request: &SuccessObservationRequest) -> Result<()> {
        let args = success_args(request);
        let outcome = self
            .attach_cancel(
                crate::process::BoundedCommand::new("musterroll")
                    .args(&args)
                    .stdout_cap(4 * 1024 * 1024)
                    .stderr_cap(256 * 1024)
                    .timeout(std::time::Duration::from_secs(60)),
            )
            .run()
            .map_err(|error| spawn_error("musterroll success", &error))?;

        if outcome.timed_out() || outcome.stdout_truncated {
            return Err(MusterrollError::command(
                "musterroll success timed out or exceeded output bounds",
            ));
        }

        if outcome.exit_code != Some(0) {
            return Err(MusterrollError::command(command_failure_message(
                "musterroll success",
                &outcome,
            )));
        }
        Ok(())
    }
}

/// Formats a command's exit condition as `exit <code>`, `cancelled` (its
/// `cancel` flag was observed before it exited — see
/// [`crate::process::CommandOutcome::cancelled`]), or, for a process that
/// never returned a code for any other reason — killed by signal, once
/// timeout, cancellation, and output-cap breaches are excluded by the
/// caller above — the literal word `signal`. Never the bare `Debug`
/// rendering of the `Option<i32>` (`Some(2)`), which is not what an
/// operator wants to read.
fn exit_label(outcome: &crate::process::CommandOutcome) -> String {
    if outcome.cancelled() {
        return "cancelled".to_string();
    }
    match outcome.exit_code {
        Some(code) => format!("exit {code}"),
        None => "signal".to_string(),
    }
}

/// Extracts a bounded, sanitized failure detail from a command's bounded
/// output: stderr when it has content, else stdout — matching the
/// pre-bounded-process `command_failure` helper (removed when
/// `CommandMusterrollClient` moved to `BoundedCommand`) this restores.
/// Control bytes are stripped and the result is capped at
/// [`FAILURE_DETAIL_CAP`] characters with a trailing `…` marker, the same
/// shape as the dashboard's own render-boundary length cap, applied here so
/// a musterroll failure is legible outside the TUI too (this module has no
/// `tui` dependency).
fn command_failure_detail(outcome: &crate::process::CommandOutcome) -> String {
    // Decode stderr first and only fall through to stdout when it has
    // nothing: stdout is capped at 4 MiB against stderr's 256 KiB, and
    // decoding a buffer that large only to discard it is pure waste on an
    // error path.
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    let detail = if stderr.trim().is_empty() {
        String::from_utf8_lossy(&outcome.stdout)
    } else {
        stderr
    };
    let raw = detail.trim();
    let sanitized = crate::sanitize::sanitize_single_line(raw);
    if sanitized.chars().count() <= FAILURE_DETAIL_CAP {
        return sanitized;
    }
    let mut capped: String = sanitized.chars().take(FAILURE_DETAIL_CAP).collect();
    capped.push('\u{2026}');
    capped
}

/// The full failure message for a command that spawned and ran to some
/// conclusion but did not exit 0: `<command> <exit label>[: <detail>]`. The
/// detail suffix is omitted entirely when both stdout and stderr are empty,
/// rather than rendering a dangling `: `.
fn command_failure_message(command: &str, outcome: &crate::process::CommandOutcome) -> String {
    let label = exit_label(outcome);
    let detail = command_failure_detail(outcome);
    if detail.is_empty() {
        format!("{command} {label}")
    } else {
        format!("{command} {label}: {detail}")
    }
}

fn spawn_error(command: &str, error: &io::Error) -> MusterrollError {
    match error.kind() {
        io::ErrorKind::NotFound => MusterrollError::unavailable("musterroll unavailable on PATH"),
        _ => MusterrollError::command(format!("failed to spawn {command}: {error}")),
    }
}

#[allow(dead_code)]
fn observation_args(request: &ObservationRequest) -> Vec<String> {
    let mut args = vec![
        "observe".to_string(),
        "--provider".to_string(),
        request.provider.clone(),
        "--availability".to_string(),
        "exhausted".to_string(),
        "--expires-at".to_string(),
        request.expires_at.clone(),
        "--expiry-basis".to_string(),
        request.expiry_basis.label().to_string(),
        "--source".to_string(),
        "undertake-runtime".to_string(),
        "--reason".to_string(),
        request.reason.label().to_string(),
    ];
    if let Some(model) = request.model.as_deref() {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    args
}

#[allow(dead_code)]
fn success_args(request: &SuccessObservationRequest) -> Vec<String> {
    vec![
        "success".to_string(),
        "--provider".to_string(),
        request.provider.clone(),
        "--model".to_string(),
        request.model.clone(),
        "--evidence-id".to_string(),
        request.evidence_id.clone(),
        "--expires-at".to_string(),
        request.expires_at.clone(),
        "--source".to_string(),
        request.source.clone(),
        "--reason".to_string(),
        request.reason.clone(),
    ]
}

pub(crate) fn canonical_provider_key(provider: &str) -> &str {
    match provider {
        "openai-codex" => "codex",
        other => other,
    }
}

pub(crate) fn normalize_provider_key(provider: &str) -> String {
    canonical_provider_key(provider.trim()).to_ascii_lowercase()
}

#[derive(Clone)]
#[allow(dead_code)]
struct SnapshotMusterrollClient {
    result: Result<StatusReport>,
}

impl MusterrollClient for SnapshotMusterrollClient {
    fn status(&self) -> Result<StatusReport> {
        self.result.clone()
    }
}

#[allow(dead_code)]
pub(crate) fn evaluate_provider_snapshot<C, I, S>(
    client: &C,
    providers: I,
    use_musterroll: bool,
) -> BTreeMap<String, BudgetDecision>
where
    C: MusterrollClient + ?Sized,
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let result = if use_musterroll {
        client.status()
    } else {
        Err(MusterrollError::unavailable(
            "musterroll intentionally bypassed by static caps",
        ))
    };
    let snapshot = SnapshotMusterrollClient { result };
    providers
        .into_iter()
        .map(|provider| normalize_provider_key(provider.as_ref()))
        .filter(|provider| !provider.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|provider| {
            let decision = evaluate_budget(&snapshot, &provider, use_musterroll);
            (provider, decision)
        })
        .collect()
}

pub(crate) fn evaluate_budget<C: MusterrollClient + ?Sized>(
    client: &C,
    provider: &str,
    use_musterroll: bool,
) -> BudgetDecision {
    let result = if use_musterroll {
        client.status()
    } else {
        Err(MusterrollError::unavailable(
            "musterroll intentionally bypassed by static caps",
        ))
    };
    let snapshot = SnapshotMusterrollClient { result };
    evaluate_budget_at(&snapshot, provider, use_musterroll, Utc::now())
}

#[expect(
    clippy::too_many_lines,
    reason = "linear fail-closed validation keeps each evidence rejection explicit"
)]
fn evaluate_budget_at<C: MusterrollClient + ?Sized>(
    client: &C,
    provider: &str,
    use_musterroll: bool,
    now: DateTime<Utc>,
) -> BudgetDecision {
    let provider = canonical_provider_key(provider);
    if !use_musterroll {
        return decision(
            provider,
            BudgetAction::StaticCaps,
            format!("{provider}: static-caps — budgets.use_musterroll is false"),
        );
    }

    let report = match client.status() {
        Ok(report) => report,
        Err(error) if error.is_unavailable() => {
            return decision(
                provider,
                BudgetAction::Defer,
                format!("{provider}: defer — musterroll unavailable ({error})"),
            );
        }
        Err(error) => {
            return decision(
                provider,
                BudgetAction::Defer,
                format!("{provider}: defer — musterroll status error: {error}"),
            );
        }
    };

    if report.schema != SCHEMA {
        return decision(
            provider,
            BudgetAction::Defer,
            format!(
                "{provider}: defer — unsupported musterroll schema {}",
                report.schema
            ),
        );
    }
    if !PROVIDERS
        .iter()
        .all(|provider| report.providers.contains_key(*provider))
    {
        return decision(
            provider,
            BudgetAction::Defer,
            format!("{provider}: defer — musterroll/status@2 missing baseline provider"),
        );
    }

    let report_checked_at = match parse_time("report checked_at", &report.checked_at) {
        Ok(value) => value,
        Err(error) => {
            return decision(
                provider,
                BudgetAction::Defer,
                format!("{provider}: defer — {error}"),
            );
        }
    };
    if report_checked_at > now {
        return decision(
            provider,
            BudgetAction::Defer,
            format!("{provider}: defer — musterroll report checked_at is in the future"),
        );
    }
    if now - report_checked_at > Duration::minutes(MAX_STATUS_AGE_MINS) {
        return decision(
            provider,
            BudgetAction::Defer,
            format!("{provider}: defer — musterroll report is stale"),
        );
    }

    let Some(status) = report.providers.get(provider) else {
        return decision(
            provider,
            BudgetAction::Defer,
            format!("{provider}: defer — provider absent from musterroll/status@2"),
        );
    };

    let status_checked_at = match parse_time("provider checked_at", &status.checked_at) {
        Ok(value) => value,
        Err(error) => {
            return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
        }
    };
    if status_checked_at != report_checked_at {
        return defer_with_status(
            provider,
            status,
            format!("{provider}: defer — provider checked_at does not match report checked_at"),
        );
    }

    if let Some(data_as_of) = status.data_as_of.as_deref() {
        let parsed = match parse_time("provider data_as_of", data_as_of) {
            Ok(value) => value,
            Err(error) => {
                return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
            }
        };
        if parsed > report_checked_at {
            return defer_with_status(
                provider,
                status,
                format!("{provider}: defer — provider data_as_of is in the future"),
            );
        }
    }

    if let Some(expires_at) = status.expires_at.as_deref() {
        let expiry = match parse_time("provider expires_at", expires_at) {
            Ok(value) => value,
            Err(error) => {
                return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
            }
        };
        if expiry <= now {
            return defer_with_status(
                provider,
                status,
                format!("{provider}: defer — musterroll evidence expired at {expires_at}"),
            );
        }
    }

    let model = match optional_extra_string(status, "observation_model") {
        Ok(value) => value,
        Err(error) => {
            return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
        }
    };
    let expiry_basis = match optional_expiry_basis(status) {
        Ok(value) => value,
        Err(error) => {
            return decision_with_status(
                provider,
                status,
                model,
                None,
                BudgetAction::Defer,
                format!("{provider}: defer — {error}"),
            );
        }
    };

    let max_window_percent = match validate_window_percents(status) {
        Ok(value) => value,
        Err(error) => {
            return defer_with_status(provider, status, format!("{provider}: defer — {error}"));
        }
    };
    if status.availability == Availability::Healthy {
        let Some(max_window_percent) = max_window_percent else {
            return decision_with_status(
                provider,
                status,
                model,
                expiry_basis,
                BudgetAction::SpendCautiously,
                format!(
                    "{provider}: spend-cautiously — musterroll healthy status has no percent windows"
                ),
            );
        };
        if max_window_percent >= NEAR_EXHAUSTED_PERCENT {
            return decision_with_status(
                provider,
                status,
                model,
                expiry_basis,
                BudgetAction::Defer,
                format!(
                    "{provider}: defer — musterroll window utilization {max_window_percent:.1}% is >= {NEAR_EXHAUSTED_PERCENT:.1}%"
                ),
            );
        }
    }

    let (action, label) = match status.availability {
        Availability::Healthy => (BudgetAction::Proceed, "proceed"),
        Availability::Caution => (BudgetAction::SpendCautiously, "spend-cautiously"),
        Availability::Exhausted | Availability::Unknown => (BudgetAction::Defer, "defer"),
    };
    decision_with_status(
        provider,
        status,
        model,
        expiry_basis,
        action,
        format!(
            "{provider}: {label} — musterroll availability {}{}",
            status.availability,
            reason_suffix(status.reason.as_deref())
        ),
    )
}

fn validate_window_percents(status: &ProviderStatus) -> std::result::Result<Option<f64>, String> {
    status
        .windows
        .iter()
        .try_fold(None::<f64>, |max_percent, window| {
            let percent = window
                .percent
                .ok_or_else(|| format!("musterroll window {} has no percent", window.label))?;
            if !percent.is_finite()
                || !(0.0..=100.0).contains(&percent)
                || (percent > 0.0 && percent <= 1.0)
            {
                return Err(format!(
                    "musterroll window {} has invalid percent {percent:?}; expected 0 or >1..=100",
                    window.label
                ));
            }
            Ok(Some(
                max_percent.map_or(percent, |current| current.max(percent)),
            ))
        })
}

fn parse_time(label: &str, value: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|error| format!("malformed {label} {value:?}: {error}"))
}

fn optional_extra_string(
    status: &ProviderStatus,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    match status.extra.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("malformed musterroll extra.{key}")),
    }
}

fn optional_expiry_basis(status: &ProviderStatus) -> std::result::Result<Option<String>, String> {
    let value = optional_extra_string(status, "observation_expiry_basis")?;
    match value.as_deref() {
        None | Some("provider-reset" | "local-cooldown" | "human-override") => Ok(value),
        Some(other) => Err(format!(
            "unsupported musterroll extra.observation_expiry_basis {other:?}"
        )),
    }
}

fn decision(provider: &str, action: BudgetAction, summary: String) -> BudgetDecision {
    BudgetDecision {
        provider: provider.to_string(),
        model: None,
        availability: None,
        source: None,
        checked_at: None,
        data_as_of: None,
        expires_at: None,
        expiry_basis: None,
        action,
        summary,
    }
}

fn defer_with_status(provider: &str, status: &ProviderStatus, summary: String) -> BudgetDecision {
    decision_with_status(provider, status, None, None, BudgetAction::Defer, summary)
}

fn decision_with_status(
    provider: &str,
    status: &ProviderStatus,
    model: Option<String>,
    expiry_basis: Option<String>,
    action: BudgetAction,
    summary: String,
) -> BudgetDecision {
    BudgetDecision {
        provider: provider.to_string(),
        model,
        availability: Some(status.availability),
        source: Some(status.source.clone()),
        checked_at: Some(status.checked_at.clone()),
        data_as_of: status.data_as_of.clone(),
        expires_at: status.expires_at.clone(),
        expiry_basis,
        action,
        summary,
    }
}

fn reason_suffix(reason: Option<&str>) -> String {
    reason.map_or_else(String::new, |reason| format!(" ({reason})"))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone)]
    pub(crate) struct FakeMusterrollClient {
        result: Rc<RefCell<Result<StatusReport>>>,
        snapshot: Result<RosterSnapshot>,
        observe_result: Result<()>,
        observations: Rc<RefCell<Vec<ObservationRequest>>>,
        apply_observations_to_status: bool,
    }

    impl FakeMusterrollClient {
        fn from_result(result: Result<StatusReport>) -> Self {
            let snapshot = fake_roster_snapshot(result.as_ref().ok());
            Self {
                result: Rc::new(RefCell::new(result)),
                snapshot,
                observe_result: Ok(()),
                observations: Rc::new(RefCell::new(Vec::new())),
                apply_observations_to_status: false,
            }
        }

        pub(crate) fn unavailable() -> Self {
            Self::from_result(Err(MusterrollError::unavailable("musterroll unavailable on PATH")))
        }

        pub(crate) fn with_roster_snapshot(mut self, snapshot: RosterSnapshot) -> Self {
            self.snapshot = Ok(snapshot);
            self
        }

        pub(crate) fn with_provider_availability(
            provider: &str,
            availability: Availability,
        ) -> Self {
            Self::with_provider_availabilities(&[(provider, availability)])
        }

        pub(crate) fn with_provider_availabilities(
            availability_by_provider: &[(&str, Availability)],
        ) -> Self {
            let checked_at = Utc::now().to_rfc3339();
            let mut providers: BTreeMap<String, ProviderStatus> = PROVIDERS
                .into_iter()
                .map(|name| {
                    (
                        name.to_string(),
                        ProviderStatus {
                            availability: Availability::Unknown,
                            source: "test".to_string(),
                            checked_at: checked_at.clone(),
                            data_as_of: None,
                            expires_at: None,
                            windows: Vec::new(),
                            reason: Some("test status".to_string()),
                            extra: Map::new(),
                        },
                    )
                })
                .collect();
            for (provider, availability) in availability_by_provider {
                if let Some(status) = providers.get_mut(canonical_provider_key(provider)) {
                    status.availability = *availability;
                    status.reason =
                        (*availability != Availability::Healthy).then(|| "test status".to_string());
                    if *availability == Availability::Healthy {
                        status.windows = vec![Window {
                            label: "primary".to_string(),
                            percent: Some(42.0),
                            reset_at: Some("2100-01-01T00:00:00Z".to_string()),
                        }];
                    }
                }
            }
            Self::from_result(Ok(StatusReport {
                schema: SCHEMA.to_string(),
                checked_at,
                providers,
            }))
        }

        pub(crate) fn without_provider() -> Self {
            Self::from_result(Ok(StatusReport {
                schema: SCHEMA.to_string(),
                checked_at: Utc::now().to_rfc3339(),
                providers: PROVIDERS
                    .into_iter()
                    .map(|provider| {
                        (
                            provider.to_string(),
                            ProviderStatus {
                                availability: Availability::Unknown,
                                source: "test".to_string(),
                                checked_at: Utc::now().to_rfc3339(),
                                data_as_of: None,
                                expires_at: None,
                                windows: Vec::new(),
                                reason: Some("test status".to_string()),
                                extra: Map::new(),
                            },
                        )
                    })
                    .collect(),
            }))
        }

        pub(crate) fn with_observe_failure(mut self) -> Self {
            self.observe_result = Err(MusterrollError::command("fixture observe failure"));
            self
        }

        pub(crate) fn with_observation_writeback(mut self) -> Self {
            self.apply_observations_to_status = true;
            self
        }

        pub(crate) fn observations(&self) -> Vec<ObservationRequest> {
            self.observations.borrow().clone()
        }
    }

    impl MusterrollClient for FakeMusterrollClient {
        fn status(&self) -> Result<StatusReport> {
            self.result.borrow().clone()
        }

        fn roster_snapshot(&self) -> Result<RosterSnapshot> {
            self.snapshot.clone()
        }

        fn observe(&self, request: &ObservationRequest) -> Result<()> {
            self.observations.borrow_mut().push(request.clone());
            self.observe_result.clone()?;
            if self.apply_observations_to_status {
                if let Ok(report) = &mut *self.result.borrow_mut() {
                    if let Some(status) = report.providers.get_mut(&request.provider) {
                        status.availability = Availability::Exhausted;
                        status.source = "undertake-runtime".to_string();
                        status.checked_at = Utc::now().to_rfc3339();
                        status.expires_at = Some(request.expires_at.clone());
                        status.windows.clear();
                        status.reason = Some(request.reason.label().to_string());
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
fn fake_roster_snapshot(status: Option<&StatusReport>) -> Result<RosterSnapshot> {
    let runtime_availability = |provider: &str| {
        status
            .and_then(|report| report.providers.get(provider))
            .map_or(Availability::Healthy, |provider| provider.availability)
    };
    // A roster snapshot is an authorization-time artifact. Runtime status
    // is intentionally separate so a just-exhausted provider reaches the
    // bounded defer path instead of erasing an approved route.
    let availability_for = |_| Availability::Healthy;
    let provider_json = PROVIDERS
        .into_iter()
        .map(|provider| {
            let availability = availability_for(provider);
            let eligible = matches!(availability, Availability::Healthy | Availability::Caution);
            let state = match availability {
                Availability::Healthy | Availability::Caution => "healthy",
                Availability::Exhausted => "exhausted",
                Availability::Unknown => "unknown",
            };
            serde_json::json!({
                "provider_id": provider,
                "availability_key": provider,
                "enabled": true,
                "state": state,
                "availability": availability.to_string(),
                "checked_at": "2026-07-17T12:00:00Z",
                "data_as_of": null,
                "expires_at": "2100-01-01T00:00:00Z",
                "reason": null,
                "eligible": eligible,
                "ineligibility_reason": null
            })
        })
        .collect::<Vec<_>>();
    let cautious_provider = ["anthropic", "codex", "opencode-go"]
        .into_iter()
        .find(|provider| runtime_availability(provider) == Availability::Caution)
        .or_else(|| {
            ["anthropic", "codex", "opencode-go"]
                .into_iter()
                .find(|provider| runtime_availability(provider) == Availability::Healthy)
        })
        .unwrap_or("anthropic");
    let profile_json = [
        ("fake-worker", "opencode-go", "fake-worker", "junior"),
        ("primary-worker", "opencode-go", "primary-worker", "junior"),
        ("fallback-worker", "codex", "fallback-worker", "junior"),
        (
            "cautious-peer",
            cautious_provider,
            "cautious-peer",
            "junior",
        ),
        (
            "senior-reviewer",
            "opencode-go",
            "senior-reviewer",
            "senior",
        ),
    ]
    .into_iter()
    .map(|(profile_id, provider_id, dispatch_id, tier)| {
        let eligible = matches!(
            availability_for(provider_id),
            Availability::Healthy | Availability::Caution
        );
        serde_json::json!({
            "profile_id": profile_id,
            "provider_id": provider_id,
            "model": format!("{profile_id}-model"),
            "harness": "pi",
            "dispatch_id": dispatch_id,
            "reasoning_effort": null,
            "tier": tier,
            "ceiling": "XL",
            "efficiency": "lean",
            "cost": 0.0,
            "data_policy": "standard",
            "enabled": true,
            "roles": ["default", "task"],
            "state": "healthy",
            "eligible": eligible,
            "ineligibility_reason": null
        })
    })
    .collect::<Vec<_>>();
    parse_roster_snapshot(
        serde_json::json!({
            "schema": "musterroll/roster@2",
            "generated_at": "2026-07-17T12:00:00Z",
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::cell::Cell;
    use test_support::FakeMusterrollClient;

    /// Finding 4 (final adversarial review): the failure message must
    /// never render the bare `Debug` form of `Option<i32>` (`Some(2)`),
    /// and dropped stderr detail must be restored, sanitized, and bounded.
    #[test]
    fn command_failure_message_shows_clean_exit_and_sanitized_capped_stderr() {
        let outcome = crate::process::CommandOutcome {
            stdout: Vec::new(),
            stderr: b"boom: connection refused\x1b[31m!!\x1b[0m".to_vec(),
            exit_code: Some(2),
            stop_condition: crate::process::StopCondition::None,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let message = command_failure_message("musterroll status --json", &outcome);
        assert!(
            !message.contains("Some("),
            "must never render the bare Option<i32> debug form, got: {message}"
        );
        assert!(message.contains("exit 2"), "got: {message}");
        assert!(
            message.contains("boom: connection refused"),
            "stderr detail must be restored, got: {message}"
        );
        assert!(
            !message.contains('\x1b'),
            "stderr must be sanitized, got: {message}"
        );
    }

    #[test]
    fn command_failure_message_caps_an_oversized_detail() {
        let outcome = crate::process::CommandOutcome {
            stdout: Vec::new(),
            stderr: "e".repeat(10_000).into_bytes(),
            exit_code: Some(1),
            stop_condition: crate::process::StopCondition::None,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let message = command_failure_message("musterroll observe", &outcome);
        assert!(
            message.chars().count() < 600,
            "detail must be capped well below the raw 10,000-char stderr, got {} chars",
            message.chars().count()
        );
        assert!(
            message.ends_with('\u{2026}'),
            "capped detail must be marked, got: {message}"
        );
    }

    #[test]
    fn command_failure_message_falls_back_to_stdout_when_stderr_is_empty() {
        let outcome = crate::process::CommandOutcome {
            stdout: b"stdout detail here".to_vec(),
            stderr: Vec::new(),
            exit_code: Some(3),
            stop_condition: crate::process::StopCondition::None,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let message = command_failure_message("musterroll roster snapshot --json", &outcome);
        assert!(message.contains("stdout detail here"), "got: {message}");
    }

    #[test]
    fn command_failure_message_omits_a_dangling_colon_when_output_is_empty() {
        let outcome = crate::process::CommandOutcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: Some(2),
            stop_condition: crate::process::StopCondition::None,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let message = command_failure_message("musterroll status --json", &outcome);
        assert_eq!(message, "musterroll status --json exit 2");
    }

    #[test]
    fn command_failure_message_reports_signal_death_not_none() {
        let outcome = crate::process::CommandOutcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: None,
            stop_condition: crate::process::StopCondition::None,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let message = command_failure_message("musterroll observe", &outcome);
        assert_eq!(message, "musterroll observe signal");
    }

    /// Cancellation must be reported distinctly from a bare signal death:
    /// "the operator quit while this was in flight" is a different fact
    /// from "the process died unexpectedly," even though both leave
    /// `exit_code` at `None`.
    #[test]
    fn command_failure_message_reports_cancelled_not_signal() {
        let outcome = crate::process::CommandOutcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: None,
            stop_condition: crate::process::StopCondition::Cancelled,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let message = command_failure_message("musterroll status --json", &outcome);
        assert_eq!(message, "musterroll status --json cancelled");
    }

    #[derive(Clone)]
    struct FakeClient {
        result: Result<StatusReport>,
    }

    impl MusterrollClient for FakeClient {
        fn status(&self) -> Result<StatusReport> {
            self.result.clone()
        }
    }

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    fn client_from_json(json: &str) -> FakeClient {
        FakeClient {
            result: serde_json::from_str(json)
                .map_err(|error| MusterrollError::json(error.to_string())),
        }
    }

    fn provider_roster_snapshot_fixture(
        provider_enabled: bool,
        provider_state: &str,
        availability: &str,
    ) -> (std::path::PathBuf, String) {
        let path = std::env::temp_dir().join(format!(
            "undertake-musterroll-roster-{provider_enabled}-{provider_state}-{availability}.toml"
        ));
        std::fs::write(&path, "fixture roster\n").expect("write fixture roster");
        let bytes = std::fs::read(&path).expect("read fixture roster");
        let sha256 = format!("{:x}", sha2::Sha256::digest(bytes));
        let json = format!(
            r#"{{
  "schema": "musterroll/roster@2",
  "generated_at": "2026-07-17T12:00:00Z",
  "source_artifact": {{"path": "{}", "sha256": "{}"}},
  "policy_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "providers": [{{
    "provider_id": "anthropic",
    "availability_key": "anthropic",
    "enabled": {provider_enabled},
    "state": "{provider_state}",
    "availability": "{availability}",
    "checked_at": "2026-07-17T12:00:00Z",
    "data_as_of": "2026-07-17T11:59:00Z",
    "expires_at": "2026-07-17T14:00:00Z",
    "reason": "bounded manual allow",
    "eligible": true,
    "ineligibility_reason": null
  }}],
  "profiles": [{{
    "profile_id": "anthropic--claude-code--claude-opus-4-8--none",
    "provider_id": "anthropic",
    "model": "claude-opus-4-8",
    "harness": "claude-code",
    "dispatch_id": "claude-opus-4-8",
    "reasoning_effort": null,
    "tier": "lead",
    "ceiling": "XL",
    "efficiency": "heavy",
    "cost": 1.0,
    "data_policy": "standard",
    "enabled": true,
    "roles": ["default"],
    "state": "healthy",
    "eligible": true,
    "ineligibility_reason": null
  }}]
}}"#,
            path.display(),
            sha256
        );
        (path, json)
    }

    #[test]
    fn strict_snapshot_parser_rejects_legacy_bursar_schema() {
        let (path, current) = provider_roster_snapshot_fixture(true, "healthy", "healthy");
        let legacy = current.replace("musterroll/roster@2", "bursar/roster@2");

        let error = parse_roster_snapshot(legacy.as_bytes())
            .expect_err("legacy product schema must fail closed");

        assert!(error.to_string().contains("unsupported musterroll roster schema"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn roster_snapshot_preserves_profile_dispatch_identity() {
        let path = std::env::temp_dir().join("undertake-musterroll-roster-snapshot.toml");
        std::fs::write(&path, "fixture roster\n").expect("write fixture roster");
        let bytes = std::fs::read(&path).expect("read fixture roster");
        let sha256 = format!("{:x}", sha2::Sha256::digest(bytes));
        let json = format!(
            r#"{{
  "schema": "musterroll/roster@2",
  "generated_at": "2026-07-16T12:00:00Z",
  "source_artifact": {{"path": "{}", "sha256": "{}"}},
  "policy_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "providers": [{{
    "provider_id": "openai-codex",
    "availability_key": "codex",
    "enabled": true,
    "state": "healthy",
    "availability": "healthy",
    "checked_at": "2026-07-16T12:00:00Z",
    "data_as_of": null,
    "expires_at": "2026-07-16T13:00:00Z",
    "reason": null,
    "eligible": true,
    "ineligibility_reason": null
  }}],
  "profiles": [{{
    "profile_id": "openai-codex--codex--gpt-5.6-luna--high",
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
    "roles": ["default"],
    "state": "healthy",
    "eligible": true,
    "ineligibility_reason": null
  }}]
}}"#,
            path.display(),
            sha256
        );

        let snapshot = parse_roster_snapshot(json.as_bytes()).expect("valid roster snapshot");
        let roster = snapshot
            .roster_entries_with_fallbacks(&[])
            .expect("convert snapshot profiles");

        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].name, "openai-codex--codex--gpt-5.6-luna--high");
        assert_eq!(roster[0].provider, "openai-codex");
        assert_eq!(roster[0].backend, crate::config::Backend::Codex);
        assert_eq!(roster[0].dispatch_id, "gpt-5.6-luna");
        assert_eq!(
            roster[0].reasoning_effort,
            Some(crate::config::ReasoningEffort::High)
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn roster_v2_accepts_opaque_identity_and_never_reads_live_source() {
        let snapshot = parse_roster_snapshot(
            br#"{
              "schema":"musterroll/roster@2",
              "generated_at":"2026-07-16T12:00:00Z",
              "source_artifact":{"path":"/intentionally/unreadable/roster.toml","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
              "policy_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
              "providers":[{
                "provider_id":"openai-codex",
                "availability_key":"codex",
                "enabled":true,
                "state":"healthy",
                "availability":"healthy",
                "checked_at":"2026-07-16T12:00:00Z",
                "data_as_of":null,
                "expires_at":"2026-07-16T13:00:00Z",
                "reason":null,
                "eligible":true,
                "ineligibility_reason":null
              }],
              "profiles":[{
                "profile_id":"openai-codex--omp--gpt-5.6-sol--xhigh",
                "provider_id":"openai-codex",
                "model":"gpt-5.6-sol",
                "harness":"omp",
                "dispatch_id":"openai-codex/gpt-5.6-sol",
                "reasoning_effort":"xhigh",
                "tier":"lead",
                "ceiling":"XL",
                "efficiency":"heavy",
                "cost":1.0,
                "data_policy":"standard",
                "enabled":true,
                "roles":["advisor","default","plan","task"],
                "state":"healthy",
                "eligible":true,
                "ineligibility_reason":null
              }]
            }"#,
        )
        .expect("strict v2 roster must not authenticate by rereading source_artifact");

        let roster = snapshot
            .roster_entries_with_fallbacks(&[])
            .expect("convert validated roster profile");
        assert_eq!(roster[0].name, "openai-codex--omp--gpt-5.6-sol--xhigh");
        assert_eq!(roster[0].provider, "openai-codex");
        assert_eq!(roster[0].dispatch_id, "openai-codex/gpt-5.6-sol");
    }

    #[test]
    fn backend_mapping_keeps_pi_and_omp_distinct() {
        assert_eq!(
            backend_from_harness("pi").expect("Pi maps"),
            crate::config::Backend::Pi
        );
        assert_eq!(
            backend_from_harness("omp").expect("OMP maps"),
            crate::config::Backend::Omp
        );
    }

    #[test]
    fn roster_snapshot_accepts_eligible_caution_provider() {
        let (path, json) = provider_roster_snapshot_fixture(true, "healthy", "caution");

        let snapshot = parse_roster_snapshot(json.as_bytes())
            .expect("eligible caution provider should be accepted");
        assert!(snapshot.providers[0].eligible);
        assert_eq!(snapshot.providers[0].availability, Availability::Caution);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn roster_snapshot_rejects_inconsistent_eligible_provider_states() {
        for (enabled, state, availability) in [
            (false, "healthy", "healthy"),
            (true, "exhausted", "exhausted"),
            (true, "unknown", "unknown"),
            (true, "stale", "healthy"),
            (true, "manually-disabled", "healthy"),
        ] {
            let (path, json) = provider_roster_snapshot_fixture(enabled, state, availability);
            let error = parse_roster_snapshot(json.as_bytes())
                .expect_err("inconsistent eligible provider must fail closed");
            assert_eq!(
                error.to_string(),
                "eligible musterroll roster provider is disabled or unavailable"
            );
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn provider_snapshot_normalizes_deduplicates_and_reads_musterroll_once() {
        struct CountingClient {
            report: StatusReport,
            calls: Cell<usize>,
        }

        impl MusterrollClient for CountingClient {
            fn status(&self) -> Result<StatusReport> {
                self.calls.set(self.calls.get() + 1);
                Ok(self.report.clone())
            }
        }

        let report = FakeMusterrollClient::with_provider_availabilities(&[
            ("codex", Availability::Healthy),
            ("anthropic", Availability::Caution),
        ])
        .status()
        .unwrap();
        let client = CountingClient {
            report,
            calls: Cell::new(0),
        };

        let snapshot =
            evaluate_provider_snapshot(&client, ["openai-codex", "codex", " Anthropic "], true);

        assert_eq!(client.calls.get(), 1);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot["codex"].action, BudgetAction::Proceed);
        assert_eq!(snapshot["anthropic"].action, BudgetAction::SpendCautiously);
    }

    #[test]
    fn budget_validation_clock_is_sampled_after_status_collection() {
        struct PostScanClient;

        impl MusterrollClient for PostScanClient {
            fn status(&self) -> Result<StatusReport> {
                std::thread::sleep(std::time::Duration::from_millis(2));
                FakeMusterrollClient::with_provider_availability("codex", Availability::Healthy)
                    .status()
            }
        }

        let decision = evaluate_budget(&PostScanClient, "codex", true);

        assert_eq!(decision.action, BudgetAction::Proceed);
        assert!(!decision.summary.contains("future"));
    }

    #[test]
    fn status_v2_fixture_maps_all_availability_values_and_evidence() {
        let client = client_from_json(include_str!("../tests/fixtures/musterroll-status-v2.json"));
        let now = at("2026-07-13T10:03:00Z");

        let anthropic = evaluate_budget_at(&client, "anthropic", true, now);
        assert_eq!(anthropic.action, BudgetAction::SpendCautiously);
        assert_eq!(anthropic.expiry_basis.as_deref(), Some("human-override"));
        assert_eq!(anthropic.model.as_deref(), Some("claude-opus-4-8"));

        let codex = evaluate_budget_at(&client, "openai-codex", true, now);
        assert_eq!(codex.provider, "codex");
        assert_eq!(codex.action, BudgetAction::Proceed);

        let opencode = evaluate_budget_at(&client, "opencode-go", true, now);
        assert_eq!(opencode.action, BudgetAction::Defer);
        assert_eq!(opencode.expiry_basis.as_deref(), Some("local-cooldown"));

        let agy = evaluate_budget_at(&client, "agy", true, now);
        assert_eq!(agy.action, BudgetAction::Defer);
        assert_eq!(agy.availability, Some(Availability::Unknown));
    }

    #[test]
    fn status_v2_fail_closed_cases_defer() {
        let now = at("2026-07-13T10:03:00Z");
        for json in [
            r#"{"schema":"musterroll/status@1","checked_at":"2026-07-13T10:02:00Z","providers":{}}"#,
            r#"{"schema":"musterroll/status@2","checked_at":"not-time","providers":{}}"#,
            r#"{"schema":"musterroll/status@2","checked_at":"2026-07-13T09:00:00Z","providers":{}}"#,
            r#"{"schema":"musterroll/status@2","checked_at":"2026-07-13T11:00:00Z","providers":{}}"#,
        ] {
            assert_eq!(
                evaluate_budget_at(&client_from_json(json), "codex", true, now).action,
                BudgetAction::Defer
            );
        }

        assert_eq!(
            evaluate_budget_at(&FakeMusterrollClient::unavailable(), "codex", true, now).action,
            BudgetAction::Defer
        );
        assert_eq!(
            evaluate_budget_at(
                &FakeMusterrollClient::without_provider(),
                "missing",
                true,
                Utc::now(),
            )
            .action,
            BudgetAction::Defer
        );
    }

    #[test]
    fn status_v2_requires_complete_fixed_provider_contract() {
        let now = at("2026-07-13T10:03:00Z");
        for field in ["data_as_of", "expires_at", "windows", "reason", "extra"] {
            let mut value: Value =
                serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                    .expect("fixture JSON");
            value["providers"]["codex"]
                .as_object_mut()
                .expect("provider object")
                .remove(field);
            assert_eq!(
                evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now,)
                    .action,
                BudgetAction::Defer,
                "missing {field}"
            );
        }

        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                .expect("fixture JSON");
        let unsupported = value["providers"]["agy"].take();
        let providers = value["providers"]
            .as_object_mut()
            .expect("providers object");
        providers.remove("agy");
        providers.insert("ollama-cloud".to_string(), unsupported);
        let decision = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "ollama-cloud",
            true,
            now,
        );
        assert_eq!(decision.action, BudgetAction::Defer);
        assert!(decision.summary.contains("baseline"));
    }

    #[test]
    fn status_v2_accepts_superset_with_new_providers() {
        // Regression for cycle-20260716-204555: Musterroll commit e588018 extended
        // status@2 to cover every roster provider, but Undertake's length check
        // still required the legacy four. Every cycle candidate deferred as
        // "malformed musterroll/status@2 provider set" and the fleet stopped.
        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                .expect("fixture JSON");
        let now = at("2026-07-13T10:03:00Z");
        for (name, availability) in [
            ("ollama-cloud", "healthy"),
            ("google-ai-studio", "caution"),
            ("neuralwatt", "exhausted"),
        ] {
            let mut provider = value["providers"]["codex"].clone();
            provider["availability"] = Value::String(availability.to_string());
            provider["reason"] = Value::String("test".to_string());
            value["providers"][name] = provider;
        }

        // Baseline providers keep their normal decisions on a superset report.
        let codex = evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now);
        assert_eq!(codex.action, BudgetAction::Proceed);

        // Added providers get their own Healthy/Caution/Exhausted decision.
        let ollama = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "ollama-cloud",
            true,
            now,
        );
        assert_eq!(ollama.action, BudgetAction::Proceed);
        assert_eq!(ollama.availability, Some(Availability::Healthy));

        let google = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "google-ai-studio",
            true,
            now,
        );
        assert_eq!(google.action, BudgetAction::SpendCautiously);
        assert_eq!(google.availability, Some(Availability::Caution));

        let neuralwatt = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "neuralwatt",
            true,
            now,
        );
        assert_eq!(neuralwatt.action, BudgetAction::Defer);
        assert_eq!(neuralwatt.availability, Some(Availability::Exhausted));

        // Requested provider absent from a superset still defers.
        let missing = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "missing-provider",
            true,
            now,
        );
        assert_eq!(missing.action, BudgetAction::Defer);
        assert!(missing.summary.contains("absent"));
    }

    #[test]
    fn status_v2_superset_missing_baseline_defers() {
        // Forward-compatible provider set must still require the legacy four.
        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                .expect("fixture JSON");
        let now = at("2026-07-13T10:03:00Z");
        let providers = value["providers"]
            .as_object_mut()
            .expect("providers object");
        let anthropic = providers.remove("anthropic").expect("anthropic present");
        providers.insert("ollama-cloud".to_string(), anthropic);
        providers.insert(
            "google-ai-studio".to_string(),
            providers.get("codex").expect("codex present").clone(),
        );
        providers.insert(
            "neuralwatt".to_string(),
            providers
                .get("opencode-go")
                .expect("opencode-go present")
                .clone(),
        );

        let decision =
            evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now);
        assert_eq!(decision.action, BudgetAction::Defer);
        assert!(decision.summary.contains("baseline"));
    }

    #[test]
    fn status_v2_rejects_even_near_future_checked_at() {
        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                .expect("fixture JSON");
        value["checked_at"] = Value::String("2026-07-13T10:03:01Z".to_string());
        for provider in PROVIDERS {
            value["providers"][provider]["checked_at"] =
                Value::String("2026-07-13T10:03:01Z".to_string());
        }
        let decision = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "codex",
            true,
            at("2026-07-13T10:03:00Z"),
        );
        assert_eq!(decision.action, BudgetAction::Defer);
        assert!(decision.summary.contains("future"));
    }

    #[test]
    fn provider_timestamps_and_expiry_fail_closed() {
        let now = at("2026-07-13T10:03:00Z");
        for (field, value) in [
            ("checked_at", "bad"),
            ("checked_at", "2026-07-13T10:01:00Z"),
            ("data_as_of", "2026-07-13T10:04:00Z"),
            ("data_as_of", "bad"),
            ("expires_at", "2026-07-13T10:03:00Z"),
            ("expires_at", "bad"),
        ] {
            let mut value_json: Value =
                serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                    .expect("fixture JSON");
            value_json["providers"]["codex"][field] = Value::String(value.to_string());
            let client = client_from_json(&value_json.to_string());
            assert_eq!(
                evaluate_budget_at(&client, "codex", true, now).action,
                BudgetAction::Defer,
                "{field}={value}"
            );
        }
    }

    #[test]
    fn malformed_observation_metadata_fails_closed() {
        let now = at("2026-07-13T10:03:00Z");
        for bad in [Value::Bool(true), Value::String("invented".to_string())] {
            let mut value: Value =
                serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                    .expect("fixture JSON");
            value["providers"]["anthropic"]["extra"]["observation_expiry_basis"] = bad;
            let decision = evaluate_budget_at(
                &client_from_json(&value.to_string()),
                "anthropic",
                true,
                now,
            );
            assert_eq!(decision.action, BudgetAction::Defer);
            assert!(decision.summary.contains("observation_expiry_basis"));
        }
    }

    #[test]
    fn healthy_provider_requires_bounded_non_fractional_window_percent() {
        let now = at("2026-07-13T10:03:00Z");
        for percent in [
            Value::Null,
            Value::from(0.42),
            Value::from(1.0),
            Value::from(-1.0),
            Value::from(100.1),
        ] {
            let mut value: Value =
                serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                    .expect("fixture JSON");
            value["providers"]["codex"]["windows"] = Value::Array(vec![serde_json::json!({
                "label": "primary",
                "percent": percent,
                "reset_at": "2100-01-01T00:00:00Z"
            })]);
            let decision =
                evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now);
            assert_eq!(decision.action, BudgetAction::Defer);
            assert!(decision.summary.contains("percent"));
        }

        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                .expect("fixture JSON");
        value["providers"]["codex"]["windows"] = Value::Array(Vec::new());
        let decision =
            evaluate_budget_at(&client_from_json(&value.to_string()), "codex", true, now);
        assert_eq!(decision.action, BudgetAction::SpendCautiously);
        assert!(decision.summary.contains("no percent windows"));
    }

    #[test]
    fn healthy_provider_defers_at_near_exhausted_window_percent() {
        let mut value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/musterroll-status-v2.json"))
                .expect("fixture JSON");
        value["providers"]["codex"]["windows"][0]["percent"] = Value::from(90.0);
        let decision = evaluate_budget_at(
            &client_from_json(&value.to_string()),
            "codex",
            true,
            at("2026-07-13T10:03:00Z"),
        );
        assert_eq!(decision.action, BudgetAction::Defer);
    }

    #[test]
    fn disabled_mode_is_the_only_static_caps_override() {
        let decision = evaluate_budget(&FakeMusterrollClient::unavailable(), "openai-codex", false);
        assert_eq!(decision.provider, "codex");
        assert_eq!(decision.action, BudgetAction::StaticCaps);
        assert!(decision.summary.contains("budgets.use_musterroll is false"));
    }

    #[test]
    fn observation_request_builds_exact_sanitized_musterroll_argv() {
        let request = ObservationRequest::runtime_limit(
            "openai-codex",
            Some("gpt-5.6-terra".to_string()),
            "2026-07-13T10:18:00Z",
            ObservationExpiryBasis::LocalCooldown,
            RuntimeLimitReason::Http429,
        );
        assert_eq!(
            observation_args(&request),
            [
                "observe",
                "--provider",
                "codex",
                "--availability",
                "exhausted",
                "--expires-at",
                "2026-07-13T10:18:00Z",
                "--expiry-basis",
                "local-cooldown",
                "--source",
                "undertake-runtime",
                "--reason",
                "runtime HTTP 429",
                "--model",
                "gpt-5.6-terra",
            ]
        );
    }

    #[test]
    fn observation_reason_and_basis_are_closed_enums() {
        assert_eq!(
            ObservationExpiryBasis::ProviderReset.label(),
            "provider-reset"
        );
        assert_eq!(
            RuntimeLimitReason::QuotaExceeded.label(),
            "runtime quota exceeded"
        );
        assert_eq!(RuntimeLimitReason::RateLimit.label(), "runtime rate limit");
    }
}
