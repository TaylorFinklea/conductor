//! Closed native job policy and pinned selection evidence.
//!
//! This module intentionally owns no execution path. It accepts only the four
//! v2 [`RunJob`] variants, validates their immutable policy bindings, and
//! produces the evidence a loop kernel must persist before selecting a profile.
#![allow(
    dead_code,
    reason = "the loop kernel consumes this registry after its prerequisite lands"
)]

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::run::{RunJob, RunLimits, RunVerifier};

pub(crate) type Result<T, E = JobError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobError {
    message: String,
}

impl JobError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for JobError {}

/// The mutation authority granted to a job, never inferred from a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationPosture {
    ReadOnly,
    RepositoryWrite,
}

impl MutationPosture {
    const fn required_for(job: RunJob) -> Self {
        match job {
            RunJob::Work => Self::RepositoryWrite,
            RunJob::Review | RunJob::Consult | RunJob::Plan => Self::ReadOnly,
        }
    }
}

/// One static Undertake-owned binding to opaque Musterroll v2 profile identities.
/// Musterroll owns the profile facts and availability; this policy owns only the
/// legal order and execution posture for a named native job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobBinding {
    pub(crate) job: RunJob,
    pub(crate) profile_ids: Vec<String>,
    pub(crate) fallback_profile_ids: Vec<String>,
    pub(crate) mutation: MutationPosture,
    pub(crate) limits: RunLimits,
    pub(crate) verifier: RunVerifier,
    pub(crate) approval_required: bool,
    /// Opaque role-policy seam. It does not activate a scheduler or workflow.
    pub(crate) role_policy: Option<String>,
}

impl JobBinding {
    pub(crate) fn pinned_profile_ids(&self) -> impl Iterator<Item = &str> {
        self.profile_ids
            .iter()
            .chain(&self.fallback_profile_ids)
            .map(String::as_str)
    }
}

/// Immutable closed registry used by native loop jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobRegistry {
    bindings: Vec<JobBinding>,
}

impl JobRegistry {
    pub(crate) fn new(mut bindings: Vec<JobBinding>) -> Result<Self> {
        if bindings.len() != 4 {
            return Err(JobError::new(
                "job registry must bind exactly work, review, consult, and plan",
            ));
        }
        let mut seen = [false; 4];
        for binding in &bindings {
            let index = job_index(binding.job);
            if seen[index] {
                return Err(JobError::new(format!(
                    "job registry has duplicate {} binding",
                    job_name(binding.job)
                )));
            }
            seen[index] = true;
            validate_binding(binding)?;
        }
        if seen != [true; 4] {
            return Err(JobError::new(
                "job registry must bind exactly work, review, consult, and plan",
            ));
        }
        bindings.sort_by_key(|binding| job_index(binding.job));
        Ok(Self { bindings })
    }

    pub(crate) fn bindings(&self) -> &[JobBinding] {
        &self.bindings
    }

    pub(crate) fn binding(&self, job: RunJob) -> Option<&JobBinding> {
        self.bindings.iter().find(|binding| binding.job == job)
    }

    /// Confirms every configured opaque identity is present in the exact
    /// Musterroll v2 snapshot captured for this run. Availability is deliberately
    /// not inferred here: it belongs in selection evidence.
    pub(crate) fn validate_pinned_profiles(
        &self,
        snapshot: &crate::musterroll::RosterSnapshot,
    ) -> Result<()> {
        for binding in &self.bindings {
            for profile_id in binding.pinned_profile_ids() {
                if !snapshot
                    .profiles
                    .iter()
                    .any(|profile| profile.profile_id == profile_id)
                {
                    return Err(JobError::new(format!(
                        "{} job references profile absent from pinned Musterroll snapshot: {profile_id}",
                        job_name(binding.job)
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validates and explains one profile selected from the immutable binding.
    /// Every earlier pinned identity must be named unavailable; otherwise the
    /// caller attempted to bypass an eligible selection without evidence.
    pub(crate) fn explain(
        &self,
        job: RunJob,
        selected_profile_id: &str,
        unavailable_profile_ids: &[&str],
    ) -> Result<JobSelectionEvidence> {
        let binding = self
            .binding(job)
            .ok_or_else(|| JobError::new("job is not bound in closed registry"))?;
        let candidates = binding.pinned_profile_ids().collect::<Vec<_>>();
        let selected_index = candidates
            .iter()
            .position(|profile_id| *profile_id == selected_profile_id)
            .ok_or_else(|| {
                JobError::new("selected profile is absent from the pinned job binding")
            })?;
        if unavailable_profile_ids.contains(&selected_profile_id) {
            return Err(JobError::new("selected profile is recorded unavailable"));
        }
        let mut constraint_reasons = Vec::with_capacity(selected_index);
        for profile_id in &candidates[..selected_index] {
            if !unavailable_profile_ids.contains(profile_id) {
                return Err(JobError::new(format!(
                    "pinned profile {profile_id} was bypassed without an unavailable constraint"
                )));
            }
            constraint_reasons.push(format!("{profile_id}: unavailable"));
        }
        Ok(JobSelectionEvidence {
            job,
            selected_profile_id: selected_profile_id.to_string(),
            selected_via: if selected_index < binding.profile_ids.len() {
                SelectionSource::Primary
            } else {
                SelectionSource::Fallback
            },
            pinned_profile_ids: candidates.into_iter().map(str::to_string).collect(),
            constraint_reasons,
            mutation: binding.mutation,
            approval_required: binding.approval_required,
        })
    }
}

fn validate_binding(binding: &JobBinding) -> Result<()> {
    if binding.mutation != MutationPosture::required_for(binding.job) {
        return Err(JobError::new(format!(
            "{} job must use {:?} mutation posture",
            job_name(binding.job),
            MutationPosture::required_for(binding.job)
        )));
    }
    if binding.profile_ids.is_empty() {
        return Err(JobError::new(format!(
            "{} job must bind at least one Musterroll profile ID",
            job_name(binding.job)
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for profile_id in binding.pinned_profile_ids() {
        if profile_id.is_empty() || !seen.insert(profile_id) {
            return Err(JobError::new(format!(
                "{} job has an empty or duplicate Musterroll profile ID",
                job_name(binding.job)
            )));
        }
    }
    if binding.role_policy.as_deref().is_some_and(str::is_empty) {
        return Err(JobError::new(
            "job role-policy seam must be a nonempty identifier",
        ));
    }
    Ok(())
}

/// Parses only the canonical lower-case native job spelling. There is no
/// compatibility alias for Arena or historical workflow names.
pub(crate) fn parse_job(value: &str) -> Result<RunJob> {
    match value {
        "work" => Ok(RunJob::Work),
        "review" => Ok(RunJob::Review),
        "consult" => Ok(RunJob::Consult),
        "plan" => Ok(RunJob::Plan),
        _ => Err(JobError::new(format!("unknown native job: {value}"))),
    }
}

const fn job_index(job: RunJob) -> usize {
    match job {
        RunJob::Work => 0,
        RunJob::Review => 1,
        RunJob::Consult => 2,
        RunJob::Plan => 3,
    }
}

const fn job_name(job: RunJob) -> &'static str {
    match job {
        RunJob::Work => "work",
        RunJob::Review => "review",
        RunJob::Consult => "consult",
        RunJob::Plan => "plan",
    }
}

/// Why the chosen profile is legal under the captured registry binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelectionSource {
    Primary,
    Fallback,
}

/// Persistable `explain` evidence. It names every configured identity rather
/// than deriving a fallback from a live roster after a run has started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JobSelectionEvidence {
    pub(crate) job: RunJob,
    pub(crate) selected_profile_id: String,
    pub(crate) selected_via: SelectionSource,
    pub(crate) pinned_profile_ids: Vec<String>,
    pub(crate) constraint_reasons: Vec<String>,
    pub(crate) mutation: MutationPosture,
    pub(crate) approval_required: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{RunJob, RunLimits, RunVerifier};

    fn binding(job: RunJob, mutation: MutationPosture) -> JobBinding {
        JobBinding {
            job,
            profile_ids: vec!["profile-a".to_string()],
            fallback_profile_ids: vec!["profile-b".to_string()],
            mutation,
            limits: RunLimits {
                item_wall_clock_mins: Some(30),
                max_attempts: Some(2),
            },
            verifier: RunVerifier {
                mechanical: Some("cargo test".to_string()),
                qualitative: None,
            },
            approval_required: true,
            role_policy: Some("implementer".to_string()),
        }
    }

    #[test]
    fn registry_is_exactly_the_four_native_jobs() {
        let registry = JobRegistry::new(vec![
            binding(RunJob::Work, MutationPosture::RepositoryWrite),
            binding(RunJob::Review, MutationPosture::ReadOnly),
            binding(RunJob::Consult, MutationPosture::ReadOnly),
            binding(RunJob::Plan, MutationPosture::ReadOnly),
        ])
        .expect("closed registry");

        assert_eq!(registry.bindings().len(), 4);
        assert!(registry.binding(RunJob::Work).is_some());
        assert!(registry.binding(RunJob::Review).is_some());
        assert!(registry.binding(RunJob::Consult).is_some());
        assert!(registry.binding(RunJob::Plan).is_some());
    }

    #[test]
    fn parse_rejects_arena_and_unknown_jobs() {
        for job in ["arena", "fleet", "work ", "Work"] {
            assert!(parse_job(job).is_err(), "unexpected job: {job}");
        }
    }

    #[test]
    fn read_only_jobs_reject_write_capable_execution() {
        for job in [RunJob::Review, RunJob::Consult, RunJob::Plan] {
            assert!(
                JobRegistry::new(vec![
                    binding(RunJob::Work, MutationPosture::RepositoryWrite),
                    binding(job, MutationPosture::RepositoryWrite),
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn explain_pins_selected_profile_and_constraint_reasons() {
        let registry = JobRegistry::new(vec![
            binding(RunJob::Work, MutationPosture::RepositoryWrite),
            binding(RunJob::Review, MutationPosture::ReadOnly),
            binding(RunJob::Consult, MutationPosture::ReadOnly),
            binding(RunJob::Plan, MutationPosture::ReadOnly),
        ])
        .expect("registry");
        let evidence = registry
            .explain(RunJob::Work, "profile-b", &["profile-a"])
            .expect("fallback selection");

        assert_eq!(evidence.selected_profile_id, "profile-b");
        assert_eq!(evidence.selected_via, SelectionSource::Fallback);
        assert_eq!(evidence.pinned_profile_ids, ["profile-a", "profile-b"]);
        assert_eq!(evidence.constraint_reasons, ["profile-a: unavailable"]);
    }

    #[test]
    fn review_remains_read_only_and_keeps_its_existing_policy_seam() {
        let mut review = binding(RunJob::Review, MutationPosture::ReadOnly);
        review.role_policy = Some("reviewer-panel".to_string());
        let registry = JobRegistry::new(vec![
            binding(RunJob::Work, MutationPosture::RepositoryWrite),
            review,
            binding(RunJob::Consult, MutationPosture::ReadOnly),
            binding(RunJob::Plan, MutationPosture::ReadOnly),
        ])
        .expect("registry");

        let binding = registry.binding(RunJob::Review).expect("review binding");
        assert_eq!(binding.mutation, MutationPosture::ReadOnly);
        assert_eq!(binding.role_policy.as_deref(), Some("reviewer-panel"));
    }
    #[test]
    fn pinned_musterroll_snapshot_must_contain_every_bound_identity() {
        let registry = JobRegistry::new(vec![
            binding(RunJob::Work, MutationPosture::RepositoryWrite),
            binding(RunJob::Review, MutationPosture::ReadOnly),
            binding(RunJob::Consult, MutationPosture::ReadOnly),
            binding(RunJob::Plan, MutationPosture::ReadOnly),
        ])
        .expect("registry");
        let snapshot = crate::musterroll::parse_roster_snapshot(
            serde_json::json!({
                "schema": "musterroll/roster@2",
                "generated_at": "2026-07-17T12:00:00Z",
                "source_artifact": {
                    "path": "/fixture/roster.toml",
                    "sha256": "a".repeat(64)
                },
                "policy_sha256": "b".repeat(64),
                "providers": [{
                    "provider_id": "provider",
                    "availability_key": "provider",
                    "enabled": true,
                    "state": "healthy",
                    "availability": "healthy",
                    "checked_at": "2026-07-17T12:00:00Z",
                    "data_as_of": null,
                    "expires_at": null,
                    "reason": null,
                    "eligible": true,
                    "ineligibility_reason": null
                }],
                "profiles": [{
                    "profile_id": "profile-a",
                    "provider_id": "provider",
                    "model": "model",
                    "harness": "pi",
                    "dispatch_id": "model",
                    "reasoning_effort": null,
                    "tier": "lead",
                    "ceiling": "XL",
                    "efficiency": "lean",
                    "cost": 0.0,
                    "data_policy": "standard",
                    "enabled": true,
                    "roles": ["implementer"],
                    "state": "healthy",
                    "eligible": true,
                    "ineligibility_reason": null
                }]
            })
            .to_string()
            .as_bytes(),
        )
        .expect("snapshot");

        assert!(registry.validate_pinned_profiles(&snapshot).is_err());
    }
}
