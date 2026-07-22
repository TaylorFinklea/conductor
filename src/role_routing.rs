//! Generic, durable role-policy routing.
#![allow(
    dead_code,
    reason = "role routing is deliberately not connected to model invocation until the plan job activates"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) use crate::run::PlanStage;

const LANE_SCHEMA: &str = "conductor/role-lane@1";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) type Result<T, E = RoleRoutingError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleRoutingError {
    message: String,
}

impl RoleRoutingError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RoleRoutingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RoleRoutingError {}

macro_rules! opaque_identifier {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if !is_identifier(&value) {
                    return Err(RoleRoutingError::new(format!(
                        "invalid {}",
                        stringify!($name)
                    )));
                }
                Ok(Self(value))
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

opaque_identifier!(RoleId);
opaque_identifier!(ProfileId);
opaque_identifier!(RunId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleBinding {
    role_id: RoleId,
    profile_id: ProfileId,
    weight: NonZeroU32,
}

impl RoleBinding {
    pub(crate) const fn new(role_id: RoleId, profile_id: ProfileId, weight: NonZeroU32) -> Self {
        Self {
            role_id,
            profile_id,
            weight,
        }
    }

    pub(crate) const fn role_id(&self) -> &RoleId {
        &self.role_id
    }

    pub(crate) const fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }

    pub(crate) const fn weight(&self) -> NonZeroU32 {
        self.weight
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RoutingPolicy {
    roster_policy_digest: String,
    digest: String,
    bindings: Vec<RoleBinding>,
}

impl RoutingPolicy {
    pub(crate) fn new(
        roster_policy_digest: String,
        mut bindings: Vec<RoleBinding>,
    ) -> Result<Self> {
        if !is_sha256(&roster_policy_digest) {
            return Err(RoleRoutingError::new(
                "pinned Bursar roster policy digest must be lowercase 64-hex",
            ));
        }
        if bindings.is_empty() {
            return Err(RoleRoutingError::new("role policy has no enabled bindings"));
        }
        bindings.sort_by(|left, right| {
            left.role_id
                .cmp(&right.role_id)
                .then_with(|| left.profile_id.cmp(&right.profile_id))
        });
        let mut unique = BTreeSet::new();
        for binding in &bindings {
            if !unique.insert((&binding.role_id, &binding.profile_id)) {
                return Err(RoleRoutingError::new(
                    "duplicate enabled role/profile policy binding",
                ));
            }
        }
        let digest = policy_digest(&roster_policy_digest, &bindings);
        Ok(Self {
            roster_policy_digest,
            digest,
            bindings,
        })
    }

    pub(crate) fn from_config(
        config: &crate::config::Config,
        snapshot: &crate::bursar::RosterSnapshot,
    ) -> Result<Self> {
        let mut bindings = Vec::with_capacity(config.role_bindings.len());
        for binding in &config.role_bindings {
            let role_id = RoleId::new(binding.role.clone())?;
            let profile_id = ProfileId::new(binding.profile_id.clone())?;
            let profile = snapshot
                .profiles
                .iter()
                .find(|profile| profile.profile_id == binding.profile_id)
                .ok_or_else(|| {
                    RoleRoutingError::new(format!(
                        "role policy references profile absent from pinned Bursar snapshot: {}",
                        binding.profile_id
                    ))
                })?;
            if !profile.roles.iter().any(|role| role == role_id.as_str()) {
                return Err(RoleRoutingError::new(format!(
                    "role policy profile {} lacks pinned role {}",
                    binding.profile_id,
                    role_id.as_str()
                )));
            }
            let provider = snapshot
                .providers
                .iter()
                .find(|provider| provider.provider_id == profile.provider_id)
                .ok_or_else(|| {
                    RoleRoutingError::new(format!(
                        "role policy profile {} has no pinned provider",
                        binding.profile_id
                    ))
                })?;
            if provider.provider_id != profile.provider_id {
                return Err(RoleRoutingError::new(
                    "role policy execution coordinate is not pinned exactly",
                ));
            }
            bindings.push(RoleBinding::new(role_id, profile_id, binding.weight));
        }
        Self::new(snapshot.policy_sha256().to_string(), bindings)
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }

    pub(crate) fn roster_policy_digest(&self) -> &str {
        &self.roster_policy_digest
    }

    pub(crate) fn bindings_for(&self, role_id: &RoleId) -> impl Iterator<Item = &RoleBinding> {
        self.bindings
            .iter()
            .filter(move |binding| binding.role_id == *role_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReservationState {
    PendingApproval,
    Committed,
    Canceled,
}

/// Durable evidence for the exact score transition that created a reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScoreEvidence {
    pub(crate) policy_digest: String,
    pub(crate) eligible_weight_total: i64,
    pub(crate) scores_before: BTreeMap<String, i64>,
    pub(crate) scores_after: BTreeMap<String, i64>,
}

/// All hard gates a caller must pin before a profile can enter a reservation.
/// Empty allow-lists fail closed rather than widening to the live roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HardEligibility {
    pub(crate) allowed_profile_ids: BTreeSet<ProfileId>,
    pub(crate) allowed_provider_ids: BTreeSet<String>,
    pub(crate) approved_execution_keys: BTreeSet<String>,
    pub(crate) required_roles: BTreeSet<RoleId>,
    pub(crate) allowed_data_policies: BTreeSet<String>,
    pub(crate) minimum_tier: crate::config::Tier,
    pub(crate) minimum_ceiling: crate::config::Ceiling,
    pub(crate) budget_available: bool,
    pub(crate) max_in_flight_per_profile: NonZeroU32,
    pub(crate) provider_distinct_from: BTreeSet<String>,
}

/// One pinned candidate together with every hard-gate result used at approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuditedCandidate {
    pub(crate) execution: crate::run::ApprovedExecution,
    pub(crate) weight: NonZeroU32,
    pub(crate) eligible: bool,
    pub(crate) rejection_reasons: Vec<String>,
}

/// The complete immutable result of preparing the planner stage. It has no
/// model side effect; dispatch remains a later, separately guarded action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedPlanner {
    pub(crate) policy_digest: String,
    pub(crate) roster_policy_digest: String,
    pub(crate) role_id: RoleId,
    pub(crate) constraints: HardEligibility,
    pub(crate) audited_pool: Vec<AuditedCandidate>,
    pub(crate) weights: BTreeMap<ProfileId, NonZeroU32>,
    pub(crate) reservation: Reservation,
    pub(crate) selected: crate::run::ApprovedExecution,
    pub(crate) fallbacks: Vec<crate::run::ApprovedExecution>,
}

/// A delayed peer-review or second-opinion binding. Unlike planner prepare it
/// is legal only after the concrete earlier identities are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedReviewer {
    pub(crate) stage: PlanStage,
    pub(crate) author: crate::run::ApprovedExecution,
    pub(crate) peer: Option<crate::run::ApprovedExecution>,
    pub(crate) route: PreparedPlanner,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reservation {
    pub(crate) run_id: RunId,
    pub(crate) role_id: RoleId,
    pub(crate) stage: PlanStage,
    pub(crate) selected_profile_id: ProfileId,
    pub(crate) state: ReservationState,
    pub(crate) sequence: u64,
    pub(crate) score_evidence: ScoreEvidence,
}

impl Reservation {
    pub(crate) const fn selected_profile_id(&self) -> &ProfileId {
        &self.selected_profile_id
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RoleRouter {
    root: PathBuf,
    policy: RoutingPolicy,
    snapshot: Option<crate::bursar::RosterSnapshot>,
}

/// Stable per-run filesystem guard for every dispatch, resume, or cancel
/// transition. The kernel lock, not a process-local map, is the exclusion.
#[derive(Debug)]
pub(crate) struct RunTransitionGuard {
    guard: File,
}

impl Drop for RunTransitionGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.guard);
    }
}

impl RoleRouter {
    pub(crate) fn new(root: &Path, policy: RoutingPolicy) -> Result<Self> {
        std::fs::create_dir_all(root.join("role-routing")).map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to create role-routing state root {}: {error}",
                root.display()
            ))
        })?;
        Ok(Self {
            root: root.to_path_buf(),
            policy,
            snapshot: None,
        })
    }

    pub(crate) fn with_pinned_snapshot(
        root: &Path,
        policy: RoutingPolicy,
        snapshot: crate::bursar::RosterSnapshot,
    ) -> Result<Self> {
        if policy.roster_policy_digest() != snapshot.policy_sha256() {
            return Err(RoleRoutingError::new(
                "role policy digest does not match the pinned Bursar roster snapshot",
            ));
        }
        let mut router = Self::new(root, policy)?;
        router.snapshot = Some(snapshot);
        Ok(router)
    }

    pub(crate) fn acquire_run_transition(&self, run_id: &RunId) -> Result<RunTransitionGuard> {
        let directory = self.root.join("role-routing").join("run-locks");
        std::fs::create_dir_all(&directory).map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to create per-run role-routing lock directory {}: {error}",
                directory.display()
            ))
        })?;
        let path = directory.join(format!("{}.lock", hex_digest(run_id.as_str().as_bytes())));
        let guard = open_lock(&path)?;
        match guard.try_lock_exclusive() {
            Ok(()) => Ok(RunTransitionGuard { guard }),
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => {
                Err(RoleRoutingError::new(format!(
                    "role-routing transition for run {} is already in progress",
                    run_id.as_str()
                )))
            }
            Err(error) => Err(RoleRoutingError::new(format!(
                "failed to lock role-routing transition for run {}: {error}",
                run_id.as_str()
            ))),
        }
    }

    pub(crate) fn reserve(
        &self,
        run_id: RunId,
        role_id: RoleId,
        stage: PlanStage,
        ineligible_profiles: &[ProfileId],
    ) -> Result<Reservation> {
        let ineligible = ineligible_profiles
            .iter()
            .map(ProfileId::as_str)
            .collect::<BTreeSet<_>>();
        let allowed = self
            .policy
            .bindings_for(&role_id)
            .filter(|binding| !ineligible.contains(binding.profile_id.as_str()))
            .map(|binding| binding.profile_id.clone())
            .collect::<BTreeSet<_>>();
        self.reserve_eligible(run_id, role_id, stage, &allowed, u32::MAX)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the lane lock owns one crash-atomic admission, score, reservation, and reset transaction"
    )]
    fn reserve_eligible(
        &self,
        run_id: RunId,
        role_id: RoleId,
        plan_stage: PlanStage,
        allowed_profiles: &BTreeSet<ProfileId>,
        max_in_flight_per_profile: u32,
    ) -> Result<Reservation> {
        let lane = LaneKey::new(self.policy.digest(), role_id, plan_stage);
        let paths = LanePaths::new(&self.root, &lane);
        std::fs::create_dir_all(&paths.dir).map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to create role-routing lane {}: {error}",
                paths.dir.display()
            ))
        })?;
        let guard = open_lock(&paths.lock_path)?;
        guard.lock_exclusive().map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to lock role-routing lane {}: {error}",
                paths.lock_path.display()
            ))
        })?;
        let outcome = (|| {
            let mut state = load_lane(&paths.state_path, &lane)?;
            if let Some(existing) = state.reservations.get(run_id.as_str()) {
                store_current_policy(&paths.family_path, &lane.policy_digest)?;
                return Ok(existing.reservation.clone());
            }
            if state.sequence == 0
                && state.reservations.is_empty()
                && state.reset_evidence.is_none()
            {
                state.reset_evidence = load_previous_policy(&paths.family_path)?
                    .filter(|previous| previous != &lane.policy_digest)
                    .map(|previous_policy_digest| PolicyResetEvidence {
                        previous_policy_digest,
                    });
            }
            let mut in_flight = BTreeMap::<&str, u32>::new();
            for stored in state.reservations.values() {
                if matches!(
                    stored.reservation.state,
                    ReservationState::PendingApproval | ReservationState::Committed
                ) {
                    let count = in_flight
                        .entry(stored.reservation.selected_profile_id.as_str())
                        .or_default();
                    *count = count.checked_add(1).ok_or_else(|| {
                        RoleRoutingError::new("role-routing in-flight capacity overflow")
                    })?;
                }
            }
            let eligible = self
                .policy
                .bindings_for(&lane.role_id)
                .filter(|binding| {
                    allowed_profiles.contains(&binding.profile_id)
                        && in_flight
                            .get(binding.profile_id.as_str())
                            .copied()
                            .unwrap_or(0)
                            < max_in_flight_per_profile
                })
                .collect::<Vec<_>>();
            if eligible.is_empty() {
                return Err(RoleRoutingError::new(
                    "no hard-eligible profile remains for role-routing reservation",
                ));
            }
            let scores_before = eligible
                .iter()
                .map(|binding| {
                    (
                        binding.profile_id.as_str().to_string(),
                        state
                            .scores
                            .get(binding.profile_id.as_str())
                            .copied()
                            .unwrap_or(0),
                    )
                })
                .collect();
            let (selected, eligible_weight_total) =
                apply_smooth_weighted_round_robin(&mut state, &eligible)?;
            let scores_after = eligible
                .iter()
                .map(|binding| {
                    (
                        binding.profile_id.as_str().to_string(),
                        state.scores[binding.profile_id.as_str()],
                    )
                })
                .collect();
            state.sequence = state
                .sequence
                .checked_add(1)
                .ok_or_else(|| RoleRoutingError::new("role-routing sequence overflow"))?;
            let reservation = Reservation {
                run_id,
                role_id: lane.role_id.clone(),
                stage: plan_stage,
                selected_profile_id: selected,
                state: ReservationState::PendingApproval,
                sequence: state.sequence,
                score_evidence: ScoreEvidence {
                    policy_digest: lane.policy_digest.clone(),
                    eligible_weight_total,
                    scores_before,
                    scores_after,
                },
            };
            state.reservations.insert(
                reservation.run_id.as_str().to_string(),
                PersistedReservation {
                    reservation: reservation.clone(),
                },
            );
            store_lane(&paths.state_path, &state)?;
            store_current_policy(&paths.family_path, &lane.policy_digest)?;
            Ok(reservation)
        })();
        let _ = fs2::FileExt::unlock(&guard);
        outcome
    }

    /// Performs a no-model preparation transaction against the exact snapshot
    /// captured by `with_pinned_snapshot`.
    fn prepare_stage(
        &self,
        run_id: RunId,
        role_id: RoleId,
        stage: PlanStage,
        constraints: HardEligibility,
    ) -> Result<PreparedPlanner> {
        let snapshot = self.snapshot.as_ref().ok_or_else(|| {
            RoleRoutingError::new("planner preparation requires a pinned Bursar roster snapshot")
        })?;
        let mut audited_pool = Vec::new();
        let mut allowed = BTreeSet::new();
        let mut weights = BTreeMap::new();

        for binding in self.policy.bindings_for(&role_id) {
            let profile = snapshot
                .profiles
                .iter()
                .find(|profile| profile.profile_id == binding.profile_id.as_str())
                .ok_or_else(|| {
                    RoleRoutingError::new(format!(
                        "policy-bound profile {} is absent from pinned snapshot",
                        binding.profile_id.as_str()
                    ))
                })?;
            let provider = snapshot
                .providers
                .iter()
                .find(|provider| provider.provider_id == profile.provider_id)
                .ok_or_else(|| {
                    RoleRoutingError::new(format!(
                        "policy-bound profile {} lacks a pinned provider",
                        binding.profile_id.as_str()
                    ))
                })?;
            let execution = approved_execution(profile, provider);
            let rejection_reasons =
                hard_rejection_reasons(profile, provider, &role_id, &constraints, &execution);
            let eligible = rejection_reasons.is_empty();
            if eligible {
                allowed.insert(binding.profile_id.clone());
            }
            weights.insert(binding.profile_id.clone(), binding.weight);
            audited_pool.push(AuditedCandidate {
                execution,
                weight: binding.weight,
                eligible,
                rejection_reasons,
            });
        }
        if audited_pool.is_empty() {
            return Err(RoleRoutingError::new(
                "role policy has no bindings for requested planner role",
            ));
        }
        let reservation = self.reserve_eligible(
            run_id,
            role_id.clone(),
            stage,
            &allowed,
            constraints.max_in_flight_per_profile.get(),
        )?;
        let selected = audited_pool
            .iter()
            .find(|candidate| {
                candidate.execution.profile_id == reservation.selected_profile_id.as_str()
            })
            .map(|candidate| candidate.execution.clone())
            .ok_or_else(|| {
                RoleRoutingError::new(
                    "selected scheduler profile has no audited pinned execution coordinate",
                )
            })?;
        let mut fallback_candidates = audited_pool
            .iter()
            .filter(|candidate| {
                candidate.eligible && candidate.execution.profile_id != selected.profile_id
            })
            .collect::<Vec<_>>();
        fallback_candidates.sort_by(|left, right| {
            right
                .weight
                .cmp(&left.weight)
                .then_with(|| left.execution.profile_id.cmp(&right.execution.profile_id))
        });
        let fallbacks = fallback_candidates
            .into_iter()
            .map(|candidate| candidate.execution.clone())
            .collect();
        Ok(PreparedPlanner {
            policy_digest: self.policy.digest().to_string(),
            roster_policy_digest: self.policy.roster_policy_digest().to_string(),
            role_id,
            constraints,
            audited_pool,
            weights,
            reservation,
            selected,
            fallbacks,
        })
    }

    /// Reserves only the planner stage; peer identities remain deliberately
    /// unbound until an author has produced a valid artifact.
    pub(crate) fn prepare_planner(
        &self,
        run_id: RunId,
        role_id: RoleId,
        constraints: HardEligibility,
    ) -> Result<PreparedPlanner> {
        self.prepare_stage(run_id, role_id, PlanStage::Planner, constraints)
    }

    /// Verifies preapproval contingencies from the immutable pool. This never
    /// reserves a reviewer: actual reviewer identities remain delayed.
    pub(crate) fn validate_preapproval_contingencies(
        &self,
        role_id: &RoleId,
        constraints: &HardEligibility,
        require_three_way_team: bool,
    ) -> Result<()> {
        let snapshot = self.snapshot.as_ref().ok_or_else(|| {
            RoleRoutingError::new("contingency validation requires a pinned Bursar roster snapshot")
        })?;
        let mut candidates = Vec::<String>::new();
        for binding in self.policy.bindings_for(role_id) {
            let profile = snapshot
                .profiles
                .iter()
                .find(|profile| profile.profile_id == binding.profile_id.as_str())
                .ok_or_else(|| {
                    RoleRoutingError::new(
                        "contingency validation found a policy profile absent from pinned snapshot",
                    )
                })?;
            let provider = snapshot
                .providers
                .iter()
                .find(|provider| provider.provider_id == profile.provider_id)
                .ok_or_else(|| {
                    RoleRoutingError::new(
                        "contingency validation found a profile without pinned provider",
                    )
                })?;
            let execution = approved_execution(profile, provider);
            if hard_rejection_reasons(profile, provider, role_id, constraints, &execution)
                .is_empty()
            {
                candidates.push(profile.provider_id.clone());
            }
        }
        if candidates.is_empty() {
            return Err(RoleRoutingError::new(
                "preapproval contingency has no hard-eligible planner candidate",
            ));
        }
        for author_provider in &candidates {
            let distinct = candidates
                .iter()
                .filter(|provider| *provider != author_provider)
                .collect::<BTreeSet<_>>();
            if distinct.is_empty() {
                return Err(RoleRoutingError::new(
                    "planner candidate lacks a legal provider-distinct peer contingency",
                ));
            }
            if require_three_way_team && distinct.len() < 2 {
                return Err(RoleRoutingError::new(
                    "spec planner candidate lacks a legal provider-distinct three-way team",
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn commit(&self, reservation: &Reservation) -> Result<Reservation> {
        self.transition(reservation, ReservationState::Committed)
    }

    pub(crate) fn cancel(&self, reservation: &Reservation) -> Result<Reservation> {
        self.transition(reservation, ReservationState::Canceled)
    }

    /// Binds a reviewer only after the actual author (and for second opinion,
    /// actual peer) identity is immutable. It always consumes its own stage
    /// lane and never consults live Bursar or config state.
    pub(crate) fn bind_reviewer(
        &self,
        run_id: RunId,
        role_id: RoleId,
        stage: PlanStage,
        author: crate::run::ApprovedExecution,
        peer: Option<crate::run::ApprovedExecution>,
        mut constraints: HardEligibility,
    ) -> Result<PreparedReviewer> {
        if !matches!(stage, PlanStage::PeerReview | PlanStage::SecondOpinion) {
            return Err(RoleRoutingError::new(
                "delayed reviewer binding requires peer_review or second_opinion stage",
            ));
        }
        self.validate_pinned_execution(&author)?;
        constraints
            .provider_distinct_from
            .insert(author.provider_id.clone());
        if matches!(stage, PlanStage::SecondOpinion) {
            let known_peer = peer.as_ref().ok_or_else(|| {
                RoleRoutingError::new(
                    "second-opinion binding requires the actual immutable peer identity",
                )
            })?;
            self.validate_pinned_execution(known_peer)?;
            constraints
                .provider_distinct_from
                .insert(known_peer.provider_id.clone());
        } else if peer.is_some() {
            return Err(RoleRoutingError::new(
                "peer-review binding cannot receive a future second-opinion identity",
            ));
        }
        let route = self.prepare_stage(run_id, role_id, stage, constraints)?;
        Ok(PreparedReviewer {
            stage,
            author,
            peer,
            route,
        })
    }

    fn validate_pinned_execution(&self, execution: &crate::run::ApprovedExecution) -> Result<()> {
        let snapshot = self.snapshot.as_ref().ok_or_else(|| {
            RoleRoutingError::new("reviewer binding requires a pinned Bursar roster snapshot")
        })?;
        let profile = snapshot
            .profiles
            .iter()
            .find(|profile| profile.profile_id == execution.profile_id)
            .ok_or_else(|| {
                RoleRoutingError::new("reviewer identity is absent from pinned snapshot")
            })?;
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.provider_id == profile.provider_id)
            .ok_or_else(|| RoleRoutingError::new("reviewer identity has no pinned provider"))?;
        if approved_execution(profile, provider) != *execution {
            return Err(RoleRoutingError::new(
                "reviewer identity does not match its pinned exact execution coordinate",
            ));
        }
        Ok(())
    }

    pub(crate) fn reservation(
        &self,
        role_id: &RoleId,
        stage: PlanStage,
        run_id: &RunId,
    ) -> Result<Option<Reservation>> {
        let lane = LaneKey::new(self.policy.digest(), role_id.clone(), stage);
        let paths = LanePaths::new(&self.root, &lane);
        let guard = open_lock(&paths.lock_path)?;
        guard.lock_exclusive().map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to lock role-routing lane {}: {error}",
                paths.lock_path.display()
            ))
        })?;
        let outcome = load_lane(&paths.state_path, &lane).map(|state| {
            state
                .reservations
                .get(run_id.as_str())
                .map(|stored| stored.reservation.clone())
        });
        let _ = fs2::FileExt::unlock(&guard);
        outcome
    }

    pub(crate) fn policy_reset_from(
        &self,
        role_id: &RoleId,
        stage: PlanStage,
    ) -> Result<Option<String>> {
        let lane = LaneKey::new(self.policy.digest(), role_id.clone(), stage);
        let paths = LanePaths::new(&self.root, &lane);
        let guard = open_lock(&paths.lock_path)?;
        guard.lock_exclusive().map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to lock role-routing lane {}: {error}",
                paths.lock_path.display()
            ))
        })?;
        let outcome = load_lane(&paths.state_path, &lane).map(|state| {
            state
                .reset_evidence
                .map(|evidence| evidence.previous_policy_digest)
        });
        let _ = fs2::FileExt::unlock(&guard);
        outcome
    }

    /// Repairs a crash between reservation persistence and durable run
    /// creation. It only cancels unlinked pending capacity; score history and
    /// sequence remain untouched, and committed reservations are fail-closed.
    pub(crate) fn reconcile_orphans(
        &self,
        role_id: &RoleId,
        plan_stage: PlanStage,
        linked_run_ids: &[RunId],
    ) -> Result<Vec<RunId>> {
        let lane = LaneKey::new(self.policy.digest(), role_id.clone(), plan_stage);
        let paths = LanePaths::new(&self.root, &lane);
        let guard = open_lock(&paths.lock_path)?;
        guard.lock_exclusive().map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to lock role-routing lane {}: {error}",
                paths.lock_path.display()
            ))
        })?;
        let outcome = (|| {
            let mut state = load_lane(&paths.state_path, &lane)?;
            let linked = linked_run_ids
                .iter()
                .map(RunId::as_str)
                .collect::<BTreeSet<_>>();
            let orphan_keys = state
                .reservations
                .iter()
                .filter(|(_, stored)| {
                    matches!(stored.reservation.state, ReservationState::PendingApproval)
                        && !linked.contains(stored.reservation.run_id.as_str())
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            let mut canceled = Vec::with_capacity(orphan_keys.len());
            for key in orphan_keys {
                let stored = state
                    .reservations
                    .get_mut(&key)
                    .expect("orphan key came from durable reservations");
                stored.reservation.state = ReservationState::Canceled;
                canceled.push(stored.reservation.run_id.clone());
            }
            if !canceled.is_empty() {
                store_lane(&paths.state_path, &state)?;
            }
            Ok(canceled)
        })();
        let _ = fs2::FileExt::unlock(&guard);
        outcome
    }

    fn transition(
        &self,
        reservation: &Reservation,
        requested: ReservationState,
    ) -> Result<Reservation> {
        let lane = LaneKey::new(
            self.policy.digest(),
            reservation.role_id.clone(),
            reservation.stage,
        );
        let paths = LanePaths::new(&self.root, &lane);
        let guard = open_lock(&paths.lock_path)?;
        guard.lock_exclusive().map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to lock role-routing lane {}: {error}",
                paths.lock_path.display()
            ))
        })?;
        let outcome = (|| {
            let mut state = load_lane(&paths.state_path, &lane)?;
            let Some(current) = state
                .reservations
                .get(reservation.run_id.as_str())
                .map(|stored| stored.reservation.clone())
            else {
                return Err(RoleRoutingError::new(
                    "role-routing reservation does not exist",
                ));
            };
            if current != *reservation {
                return Err(RoleRoutingError::new(
                    "role-routing reservation does not match the durable reservation",
                ));
            }
            match (&current.state, &requested) {
                (
                    ReservationState::PendingApproval,
                    ReservationState::Committed | ReservationState::Canceled,
                ) => {
                    let stored = state
                        .reservations
                        .get_mut(reservation.run_id.as_str())
                        .expect("existing reservation was checked");
                    stored.reservation.state = requested;
                    let updated = stored.reservation.clone();
                    store_lane(&paths.state_path, &state)?;
                    Ok(updated)
                }
                (ReservationState::Committed, ReservationState::Canceled) => Err(
                    RoleRoutingError::new("cannot cancel a committed role-routing reservation"),
                ),
                (ReservationState::PendingApproval, ReservationState::PendingApproval)
                | (ReservationState::Committed, ReservationState::Committed)
                | (ReservationState::Canceled, ReservationState::Canceled) => Ok(current),
                (ReservationState::Canceled, ReservationState::Committed) => Err(
                    RoleRoutingError::new("cannot commit a canceled role-routing reservation"),
                ),
                (
                    ReservationState::Committed | ReservationState::Canceled,
                    ReservationState::PendingApproval,
                ) => Err(RoleRoutingError::new(
                    "role-routing reservation state cannot be rewound",
                )),
            }
        })();
        let _ = fs2::FileExt::unlock(&guard);
        outcome
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaneKey {
    policy_digest: String,
    role_id: RoleId,
    stage: PlanStage,
}

impl LaneKey {
    fn new(policy_digest: &str, role_id: RoleId, stage: PlanStage) -> Self {
        Self {
            policy_digest: policy_digest.to_string(),
            role_id,
            stage,
        }
    }

    fn identity(&self) -> String {
        format!(
            "{}\n{}\n{}",
            self.policy_digest,
            self.role_id.as_str(),
            stage_label(self.stage)
        )
    }
}

struct LanePaths {
    dir: PathBuf,
    lock_path: PathBuf,
    state_path: PathBuf,
    family_path: PathBuf,
}

impl LanePaths {
    fn new(root: &Path, lane: &LaneKey) -> Self {
        let lane_hash = hex_digest(lane.identity().as_bytes());
        let dir = root.join("role-routing").join("lanes").join(lane_hash);
        let family_identity = format!("{}\n{}", lane.role_id.as_str(), stage_label(lane.stage));
        Self {
            lock_path: dir.join("lane.lock"),
            state_path: dir.join("state.json"),
            family_path: root
                .join("role-routing")
                .join("families")
                .join(format!("{}.json", hex_digest(family_identity.as_bytes()))),
            dir,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaneState {
    schema: String,
    policy_digest: String,
    role_id: RoleId,
    stage: PlanStage,
    sequence: u64,
    scores: BTreeMap<String, i64>,
    reservations: BTreeMap<String, PersistedReservation>,
    #[serde(default)]
    reset_evidence: Option<PolicyResetEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyResetEvidence {
    previous_policy_digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaneFamilyState {
    policy_digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedReservation {
    reservation: Reservation,
}

fn load_lane(path: &Path, expected: &LaneKey) -> Result<LaneState> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let state: LaneState = serde_json::from_slice(&bytes).map_err(|error| {
                RoleRoutingError::new(format!(
                    "failed to parse durable role-routing lane {}: {error}",
                    path.display()
                ))
            })?;
            if state.schema != LANE_SCHEMA
                || state.policy_digest != expected.policy_digest
                || state.role_id != expected.role_id
                || state.stage != expected.stage
            {
                return Err(RoleRoutingError::new(
                    "durable role-routing lane identity does not match lock key",
                ));
            }
            Ok(state)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LaneState {
            schema: LANE_SCHEMA.to_string(),
            policy_digest: expected.policy_digest.clone(),
            role_id: expected.role_id.clone(),
            stage: expected.stage,
            sequence: 0,
            scores: BTreeMap::new(),
            reservations: BTreeMap::new(),
            reset_evidence: None,
        }),
        Err(error) => Err(RoleRoutingError::new(format!(
            "failed to read durable role-routing lane {}: {error}",
            path.display()
        ))),
    }
}

fn load_previous_policy(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let family: LaneFamilyState = serde_json::from_slice(&bytes).map_err(|error| {
                RoleRoutingError::new(format!(
                    "failed to parse role-routing family state {}: {error}",
                    path.display()
                ))
            })?;
            if !is_sha256(&family.policy_digest) {
                return Err(RoleRoutingError::new(
                    "role-routing family state has an invalid policy digest",
                ));
            }
            Ok(Some(family.policy_digest))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(RoleRoutingError::new(format!(
            "failed to read role-routing family state {}: {error}",
            path.display()
        ))),
    }
}

fn store_current_policy(path: &Path, policy_digest: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        RoleRoutingError::new(format!(
            "role-routing family path has no parent: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        RoleRoutingError::new(format!(
            "failed to create role-routing family directory {}: {error}",
            parent.display()
        ))
    })?;
    let bytes = serde_json::to_vec(&LaneFamilyState {
        policy_digest: policy_digest.to_string(),
    })
    .map_err(|error| {
        RoleRoutingError::new(format!("failed to serialize role-routing family: {error}"))
    })?;
    atomic_replace(path, &bytes)
}

fn apply_smooth_weighted_round_robin(
    state: &mut LaneState,
    eligible: &[&RoleBinding],
) -> Result<(ProfileId, i64)> {
    let mut total = 0_i64;
    for binding in eligible {
        let weight = i64::from(binding.weight.get());
        total = total
            .checked_add(weight)
            .ok_or_else(|| RoleRoutingError::new("eligible role-routing weight total overflow"))?;
        let score = state
            .scores
            .entry(binding.profile_id.as_str().to_string())
            .or_insert(0);
        *score = score
            .checked_add(weight)
            .ok_or_else(|| RoleRoutingError::new("role-routing score overflow"))?;
    }
    let winner = eligible
        .iter()
        .max_by(|left, right| {
            let left_score = state.scores[left.profile_id.as_str()];
            let right_score = state.scores[right.profile_id.as_str()];
            left_score
                .cmp(&right_score)
                .then_with(|| right.profile_id.cmp(&left.profile_id))
        })
        .ok_or_else(|| RoleRoutingError::new("no eligible role-routing profile"))?;
    let winner_score = state
        .scores
        .get_mut(winner.profile_id.as_str())
        .expect("winner score is initialized");
    *winner_score = winner_score
        .checked_sub(total)
        .ok_or_else(|| RoleRoutingError::new("role-routing score underflow"))?;
    Ok((winner.profile_id.clone(), total))
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to open durable role-routing lock {}: {error}",
                path.display()
            ))
        })
}

fn approved_execution(
    profile: &crate::bursar::RosterProfile,
    provider: &crate::bursar::RosterProvider,
) -> crate::run::ApprovedExecution {
    let coordinate = [
        profile.provider_id.as_str(),
        profile.model.as_str(),
        profile.harness.as_str(),
        profile.dispatch_id.as_str(),
        profile.reasoning_effort.as_deref().unwrap_or_default(),
    ];
    let mut canonical = Vec::new();
    for field in coordinate {
        canonical.extend_from_slice(field.len().to_string().as_bytes());
        canonical.push(b':');
        canonical.extend_from_slice(field.as_bytes());
        canonical.push(b'\n');
    }
    crate::run::ApprovedExecution {
        profile_id: profile.profile_id.clone(),
        provider_id: profile.provider_id.clone(),
        availability_key: provider.availability_key.clone(),
        execution_key: hex_digest(&canonical),
    }
}

fn hard_rejection_reasons(
    profile: &crate::bursar::RosterProfile,
    provider: &crate::bursar::RosterProvider,
    role_id: &RoleId,
    constraints: &HardEligibility,
    execution: &crate::run::ApprovedExecution,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if !constraints.budget_available {
        reasons.push("budget_exhausted".to_string());
    }
    if !constraints
        .allowed_profile_ids
        .contains(&ProfileId(profile.profile_id.clone()))
    {
        reasons.push("profile_not_approved".to_string());
    }
    if !constraints
        .allowed_provider_ids
        .contains(&profile.provider_id)
    {
        reasons.push("provider_not_approved".to_string());
    }
    if !constraints
        .approved_execution_keys
        .contains(&execution.execution_key)
    {
        reasons.push("execution_coordinate_not_approved".to_string());
    }
    if !profile.enabled || profile.state != "healthy" || !profile.eligible {
        reasons.push("profile_unavailable".to_string());
    }
    if !provider.enabled
        || provider.state != "healthy"
        || !provider.eligible
        || !matches!(
            provider.availability,
            crate::bursar::Availability::Healthy | crate::bursar::Availability::Caution
        )
    {
        reasons.push("provider_unavailable".to_string());
    }
    if !profile.roles.iter().any(|role| role == role_id.as_str())
        || constraints.required_roles.iter().any(|role| {
            !profile
                .roles
                .iter()
                .any(|candidate| candidate == role.as_str())
        })
    {
        reasons.push("role_missing".to_string());
    }
    if !constraints
        .allowed_data_policies
        .contains(&profile.data_policy)
    {
        reasons.push("data_policy_denied".to_string());
    }
    let tier = profile.tier.parse::<crate::config::Tier>();
    if tier.map_or(true, |tier| {
        tier_rank(tier) < tier_rank(constraints.minimum_tier)
    }) {
        reasons.push("tier_insufficient".to_string());
    }
    let ceiling = profile.ceiling.parse::<crate::config::Ceiling>();
    if ceiling.map_or(true, |ceiling| {
        ceiling_rank(ceiling) < ceiling_rank(constraints.minimum_ceiling)
    }) {
        reasons.push("ceiling_insufficient".to_string());
    }
    if constraints
        .provider_distinct_from
        .contains(&profile.provider_id)
    {
        reasons.push("provider_not_distinct".to_string());
    }
    reasons
}

const fn tier_rank(tier: crate::config::Tier) -> u8 {
    match tier {
        crate::config::Tier::Junior => 1,
        crate::config::Tier::Senior => 2,
        crate::config::Tier::Lead => 3,
    }
}

const fn ceiling_rank(ceiling: crate::config::Ceiling) -> u8 {
    match ceiling {
        crate::config::Ceiling::S => 1,
        crate::config::Ceiling::M => 2,
        crate::config::Ceiling::L => 3,
        crate::config::Ceiling::Xl => 4,
    }
}

fn store_lane(path: &Path, state: &LaneState) -> Result<()> {
    let mut bytes = serde_json::to_vec(state).map_err(|error| {
        RoleRoutingError::new(format!("failed to serialize role-routing lane: {error}"))
    })?;
    bytes.push(b'\n');
    atomic_replace(path, &bytes)
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        RoleRoutingError::new(format!(
            "role-routing state path has no parent: {}",
            path.display()
        ))
    })?;
    let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|error| {
                RoleRoutingError::new(format!("failed to create role-routing temp file: {error}"))
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                RoleRoutingError::new(format!("failed to sync role-routing temp file: {error}"))
            })?;
        std::fs::rename(&tmp, path).map_err(|error| {
            RoleRoutingError::new(format!(
                "failed to atomically replace role-routing state {}: {error}",
                path.display()
            ))
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                RoleRoutingError::new(format!(
                    "failed to sync role-routing state directory: {error}"
                ))
            })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn policy_digest(roster_policy_digest: &str, bindings: &[RoleBinding]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"conductor/role-policy@1\0");
    hasher.update(roster_policy_digest.as_bytes());
    hasher.update(b"\0");
    for binding in bindings {
        hasher.update(binding.role_id.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(binding.profile_id.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(binding.weight.get().to_be_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

const fn stage_label(stage: PlanStage) -> &'static str {
    match stage {
        PlanStage::Planner => "planner",
        PlanStage::PeerReview => "peer_review",
        PlanStage::SecondOpinion => "second_opinion",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{
        HardEligibility, PlanStage, ProfileId, RoleBinding, RoleId, RoleRouter, RoutingPolicy,
        RunId,
    };

    #[test]
    fn role_routing_scheduler_smooth_weighted_planner_lane_is_exactly_twelve_four_four() {
        let temp = TempDir::new("distribution");
        let policy = RoutingPolicy::new(
            "a".repeat(64),
            vec![
                binding("planner", "openai-codex--omp--gpt-5.6-sol--xhigh", 60),
                binding("planner", "anthropic--omp--claude-opus-4-8--max", 20),
                binding("planner", "opencode-go--omp--kimi-k3--max", 20),
            ],
        )
        .expect("valid policy");
        let router = RoleRouter::new(temp.path(), policy).expect("router");
        let mut counts = BTreeMap::new();

        for n in 0..20 {
            let reservation = router
                .reserve(
                    RunId::new(format!("run-{n}")).expect("run id"),
                    RoleId::new("planner").expect("role"),
                    PlanStage::Planner,
                    &[],
                )
                .expect("reservation");
            *counts
                .entry(reservation.selected_profile_id().as_str().to_string())
                .or_insert(0) += 1;
        }

        assert_eq!(counts["openai-codex--omp--gpt-5.6-sol--xhigh"], 12);
        assert_eq!(counts["anthropic--omp--claude-opus-4-8--max"], 4);
        assert_eq!(counts["opencode-go--omp--kimi-k3--max"], 4);
    }

    #[test]
    fn durable_scores_survive_restart_and_ineligible_profiles_accrue_no_credit() {
        let temp = TempDir::new("restart-and-credit");
        let policy = RoutingPolicy::new(
            "a".repeat(64),
            vec![
                binding("planner", "alpha", 1),
                binding("planner", "beta", 1),
            ],
        )
        .expect("policy");
        let role = RoleId::new("planner").expect("role");
        let first = RoleRouter::new(temp.path(), policy.clone()).expect("router");
        assert_eq!(
            first
                .reserve(
                    RunId::new("first").expect("run"),
                    role.clone(),
                    PlanStage::Planner,
                    &[],
                )
                .expect("first")
                .selected_profile_id()
                .as_str(),
            "alpha"
        );
        let restarted = RoleRouter::new(temp.path(), policy).expect("restarted router");
        assert_eq!(
            restarted
                .reserve(
                    RunId::new("temporary-ineligible").expect("run"),
                    role.clone(),
                    PlanStage::Planner,
                    &[ProfileId::new("alpha").expect("profile")],
                )
                .expect("second")
                .selected_profile_id()
                .as_str(),
            "beta"
        );
        assert_eq!(
            restarted
                .reserve(
                    RunId::new("eligible-again").expect("run"),
                    role,
                    PlanStage::Planner,
                    &[],
                )
                .expect("third")
                .selected_profile_id()
                .as_str(),
            "beta",
            "temporarily ineligible alpha retained its old score and gained no credit"
        );
    }

    #[test]
    fn concurrent_prepare_respects_single_profile_capacity_under_the_lane_lock() {
        let temp = TempDir::new("prepare-concurrency");
        let snapshot = pinned_snapshot("profile-a", "provider-a", &["plan"]);
        let policy = RoutingPolicy::new("a".repeat(64), vec![binding("plan", "profile-a", 1)])
            .expect("policy");
        let router = std::sync::Arc::new(
            RoleRouter::with_pinned_snapshot(temp.path(), policy, snapshot.clone())
                .expect("router"),
        );
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let outcomes = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for index in 0..2 {
                let router = router.clone();
                let barrier = barrier.clone();
                let snapshot = snapshot.clone();
                workers.push(scope.spawn(move || {
                    barrier.wait();
                    router.prepare_planner(
                        RunId::new(format!("concurrent-{index}")).expect("run"),
                        RoleId::new("plan").expect("role"),
                        strict_constraints(&snapshot, "plan"),
                    )
                }));
            }
            workers
                .into_iter()
                .map(|worker| worker.join().expect("worker"))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            outcomes.into_iter().flatten().count(),
            1,
            "the lane transaction must not over-reserve one profile's capacity"
        );
    }

    #[test]
    fn checked_score_overflow_fails_closed_without_wrapping() {
        let temp = TempDir::new("overflow");
        let policy = RoutingPolicy::new("a".repeat(64), vec![binding("planner", "alpha", 1)])
            .expect("policy");
        let router = RoleRouter::new(temp.path(), policy).expect("router");
        let role = RoleId::new("planner").expect("role");
        let lane = super::LaneKey::new(router.policy.digest(), role.clone(), PlanStage::Planner);
        let paths = super::LanePaths::new(temp.path(), &lane);
        std::fs::create_dir_all(&paths.dir).expect("lane");
        super::store_lane(
            &paths.state_path,
            &super::LaneState {
                schema: super::LANE_SCHEMA.to_string(),
                policy_digest: router.policy.digest().to_string(),
                role_id: role.clone(),
                stage: PlanStage::Planner,
                sequence: 0,
                scores: [("alpha".to_string(), i64::MAX)].into_iter().collect(),
                reservations: BTreeMap::new(),
                reset_evidence: None,
            },
        )
        .expect("seed state");
        assert!(
            router
                .reserve(
                    RunId::new("overflow-run").expect("run"),
                    role,
                    PlanStage::Planner,
                    &[],
                )
                .is_err()
        );
    }

    #[test]
    fn role_routing_reservation_cancel_and_commit_never_rewind_a_reserved_turn() {
        let temp = TempDir::new("irreversible");
        let policy =
            RoutingPolicy::new("a".repeat(64), vec![binding("planner", "only-profile", 1)])
                .expect("valid policy");
        let router = RoleRouter::new(temp.path(), policy).expect("router");
        let role = RoleId::new("planner").expect("role");
        let canceled_id = RunId::new("canceled-run").expect("run id");
        let committed_id = RunId::new("committed-run").expect("run id");

        let canceled = router
            .reserve(canceled_id.clone(), role.clone(), PlanStage::Planner, &[])
            .expect("reserve canceled");
        assert_eq!(canceled.sequence, 1);
        let canceled = router.cancel(&canceled).expect("cancel");
        assert_eq!(canceled.state, super::ReservationState::Canceled);

        let committed = router
            .reserve(committed_id.clone(), role, PlanStage::Planner, &[])
            .expect("reserve committed");
        assert_eq!(committed.sequence, 2);
        let committed = router.commit(&committed).expect("commit");
        assert_eq!(committed.state, super::ReservationState::Committed);

        assert_eq!(
            router
                .cancel(&committed)
                .expect_err("committed is irreversible")
                .to_string(),
            "cannot cancel a committed role-routing reservation"
        );
        assert_eq!(
            router
                .reservation(
                    &RoleId::new("planner").expect("role"),
                    PlanStage::Planner,
                    &canceled_id,
                )
                .expect("load canceled")
                .expect("reservation")
                .sequence,
            1
        );
    }

    #[test]
    fn orphan_reconciliation_cancels_only_unlinked_pending_capacity_without_rewinding() {
        let temp = TempDir::new("orphan");
        let router = RoleRouter::new(
            temp.path(),
            RoutingPolicy::new("a".repeat(64), vec![binding("planner", "only-profile", 1)])
                .expect("policy"),
        )
        .expect("router");
        let role = RoleId::new("planner").expect("role");
        let orphan = router
            .reserve(
                RunId::new("orphan-run").expect("run"),
                role.clone(),
                PlanStage::Planner,
                &[],
            )
            .expect("reserve");
        let linked = RunId::new("linked-run").expect("run");
        let canceled = router
            .reconcile_orphans(&role, PlanStage::Planner, &[linked])
            .expect("reconcile");
        assert_eq!(canceled, vec![orphan.run_id.clone()]);
        assert_eq!(
            router
                .reservation(&role, PlanStage::Planner, &orphan.run_id)
                .expect("load")
                .expect("reservation")
                .state,
            super::ReservationState::Canceled
        );
        let next = router
            .reserve(
                RunId::new("next-run").expect("run"),
                role,
                PlanStage::Planner,
                &[],
            )
            .expect("capacity released");
        assert_eq!(next.sequence, 2);
    }

    #[test]
    fn per_run_guard_excludes_duplicate_dispatch_resume_or_cancel_transition() {
        let temp = TempDir::new("run-guard");
        let router = RoleRouter::new(
            temp.path(),
            RoutingPolicy::new("a".repeat(64), vec![binding("planner", "profile-a", 1)])
                .expect("policy"),
        )
        .expect("router");
        let run = RunId::new("run-transition").expect("run");
        let first = router.acquire_run_transition(&run).expect("first guard");
        assert!(router.acquire_run_transition(&run).is_err());
        drop(first);
        router
            .acquire_run_transition(&run)
            .expect("guard released after transition");
    }

    #[test]
    fn role_routing_policy_change_uses_a_new_lane_and_records_reset_evidence() {
        let temp = TempDir::new("policy-reset");
        let binding = binding("planner", "profile-a", 1);
        let first = RoleRouter::new(
            temp.path(),
            RoutingPolicy::new("a".repeat(64), vec![binding.clone()]).expect("first policy"),
        )
        .expect("first router");
        first
            .reserve(
                RunId::new("first-run").expect("run"),
                RoleId::new("planner").expect("role"),
                PlanStage::Planner,
                &[],
            )
            .expect("first reservation");

        let second = RoleRouter::new(
            temp.path(),
            RoutingPolicy::new("b".repeat(64), vec![binding]).expect("second policy"),
        )
        .expect("second router");
        second
            .reserve(
                RunId::new("second-run").expect("run"),
                RoleId::new("planner").expect("role"),
                PlanStage::Planner,
                &[],
            )
            .expect("second reservation");
        assert_eq!(
            second
                .policy_reset_from(&RoleId::new("planner").expect("role"), PlanStage::Planner)
                .expect("reset evidence"),
            Some(first.policy.digest().to_string())
        );
    }

    #[test]
    fn role_routing_config_rejects_zero_disabled_duplicate_and_unknown_bindings() {
        let valid = crate::config::parse_str(
            r#"
                autonomy = "propose"
                [[role_binding]]
                role = "planner"
                profile_id = "profile-a"
                weight = 1
                enabled = true
            "#,
        )
        .expect("valid strict role binding");
        assert_eq!(valid.role_bindings.len(), 1);

        for source in [
            r#"[[role_binding]]
               role = "planner"
               profile_id = "profile-a"
               weight = 0
               enabled = true"#,
            r#"[[role_binding]]
               role = "planner"
               profile_id = "profile-a"
               weight = 1
               enabled = false"#,
            r#"[[role_binding]]
               role = "planner"
               profile_id = "profile-a"
               weight = 1
               enabled = true
               unknown = "fail""#,
        ] {
            assert!(
                crate::config::parse_str(source).is_err(),
                "must fail closed: {source}"
            );
        }
        assert!(
            crate::config::parse_str(
                r#"[[role_binding]]
                   role = "planner"
                   profile_id = "profile-a"
                   weight = 1
                   enabled = true
                   [[role_binding]]
                   role = "planner"
                   profile_id = "profile-a"
                   weight = 2
                   enabled = true"#,
            )
            .is_err()
        );
    }

    #[test]
    fn shipped_config_has_exact_initial_plan_weights() {
        let cfg = crate::config::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/conductor.toml"
        )))
        .expect("shipped config");
        let weights = cfg
            .role_bindings
            .iter()
            .filter(|binding| binding.role == "plan")
            .map(|binding| (binding.profile_id.as_str(), binding.weight.get()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            weights,
            BTreeMap::from([
                ("openai-codex--omp--gpt-5.6-sol--xhigh", 60),
                ("anthropic--omp--claude-opus-4-8--max", 20),
                ("opencode-go--omp--kimi-k3--max", 20),
            ])
        );
    }

    #[test]
    fn role_routing_policy_from_config_requires_pinned_exact_role_tagged_profiles() {
        let cfg = crate::config::parse_str(
            r#"[[role_binding]]
               role = "plan"
               profile_id = "profile-a"
               weight = 1
               enabled = true"#,
        )
        .expect("config");
        let snapshot = pinned_snapshot("profile-a", "provider-a", &["plan"]);
        let policy = RoutingPolicy::from_config(&cfg, &snapshot).expect("pinned binding");
        assert_eq!(policy.roster_policy_digest(), "a".repeat(64));

        let unknown_cfg = crate::config::parse_str(
            r#"[[role_binding]]
               role = "plan"
               profile_id = "missing-profile"
               weight = 1
               enabled = true"#,
        )
        .expect("config");
        assert!(RoutingPolicy::from_config(&unknown_cfg, &snapshot).is_err());

        let untagged = pinned_snapshot("profile-a", "provider-a", &["task"]);
        assert!(RoutingPolicy::from_config(&cfg, &untagged).is_err());
    }

    #[test]
    fn planner_prepare_pins_hard_gates_pool_fallbacks_and_score_evidence() {
        let cfg = crate::config::parse_str(
            r#"[[role_binding]]
               role = "plan"
               profile_id = "profile-a"
               weight = 1
               enabled = true"#,
        )
        .expect("config");
        let snapshot = pinned_snapshot("profile-a", "provider-a", &["plan"]);
        let policy = RoutingPolicy::from_config(&cfg, &snapshot).expect("policy");
        let temp = TempDir::new("prepare");
        let router = RoleRouter::with_pinned_snapshot(temp.path(), policy, snapshot.clone())
            .expect("router");
        let prepared = router
            .prepare_planner(
                RunId::new("prepared-run").expect("run"),
                RoleId::new("plan").expect("role"),
                strict_constraints(&snapshot, "plan"),
            )
            .expect("prepared");

        assert_eq!(prepared.audited_pool.len(), 1);
        assert_eq!(prepared.selected.profile_id, "profile-a");
        assert!(prepared.fallbacks.is_empty());
        assert_eq!(prepared.reservation.score_evidence.eligible_weight_total, 1);
        assert_eq!(
            prepared.reservation.score_evidence.scores_before["profile-a"],
            0
        );
        assert_eq!(
            prepared.reservation.score_evidence.scores_after["profile-a"],
            0
        );

        let mut denied = strict_constraints(&snapshot, "plan");
        denied.budget_available = false;
        assert!(
            router
                .prepare_planner(
                    RunId::new("budget-denied").expect("run"),
                    RoleId::new("plan").expect("role"),
                    denied,
                )
                .is_err()
        );
    }

    #[test]
    fn reviewer_routes_bind_late_in_distinct_stage_lanes_and_are_provider_distinct() {
        let snapshot = pinned_snapshot_many(&[
            ("alpha", "provider-a"),
            ("beta", "provider-b"),
            ("gamma", "provider-c"),
        ]);
        let policy = RoutingPolicy::new(
            "a".repeat(64),
            vec![
                binding("plan", "alpha", 60),
                binding("plan", "beta", 20),
                binding("plan", "gamma", 20),
            ],
        )
        .expect("policy");
        let temp = TempDir::new("reviewer");
        let router = RoleRouter::with_pinned_snapshot(temp.path(), policy, snapshot.clone())
            .expect("router");
        let author = super::approved_execution(&snapshot.profiles[0], &snapshot.providers[0]);

        assert!(
            router
                .bind_reviewer(
                    RunId::new("too-early").expect("run"),
                    RoleId::new("plan").expect("role"),
                    PlanStage::SecondOpinion,
                    author.clone(),
                    None,
                    strict_constraints(&snapshot, "plan"),
                )
                .is_err()
        );
        let peer = router
            .bind_reviewer(
                RunId::new("peer-run").expect("run"),
                RoleId::new("plan").expect("role"),
                PlanStage::PeerReview,
                author.clone(),
                None,
                strict_constraints(&snapshot, "plan"),
            )
            .expect("peer binding");
        assert_ne!(peer.route.selected.provider_id, author.provider_id);
        assert_eq!(peer.stage, PlanStage::PeerReview);

        let second = router
            .bind_reviewer(
                RunId::new("second-run").expect("run"),
                RoleId::new("plan").expect("role"),
                PlanStage::SecondOpinion,
                author.clone(),
                Some(peer.route.selected.clone()),
                strict_constraints(&snapshot, "plan"),
            )
            .expect("second binding");
        assert_ne!(second.route.selected.provider_id, author.provider_id);
        assert_ne!(
            second.route.selected.provider_id,
            peer.route.selected.provider_id
        );
        assert_eq!(second.stage, PlanStage::SecondOpinion);
    }

    #[test]
    fn planner_preapproval_requires_legal_distinct_peer_and_spec_team_for_every_candidate() {
        let three = pinned_snapshot_many(&[
            ("alpha", "provider-a"),
            ("beta", "provider-b"),
            ("gamma", "provider-c"),
        ]);
        let policy = RoutingPolicy::new(
            "a".repeat(64),
            vec![
                binding("plan", "alpha", 1),
                binding("plan", "beta", 1),
                binding("plan", "gamma", 1),
            ],
        )
        .expect("policy");
        let temp = TempDir::new("contingency");
        let router =
            RoleRouter::with_pinned_snapshot(temp.path(), policy, three.clone()).expect("router");
        router
            .validate_preapproval_contingencies(
                &RoleId::new("plan").expect("role"),
                &strict_constraints(&three, "plan"),
                true,
            )
            .expect("three-way contingency");

        let two = pinned_snapshot_many(&[("alpha", "provider-a"), ("beta", "provider-b")]);
        let two_policy = RoutingPolicy::new(
            "a".repeat(64),
            vec![binding("plan", "alpha", 1), binding("plan", "beta", 1)],
        )
        .expect("policy");
        let two_router = RoleRouter::with_pinned_snapshot(
            TempDir::new("two-contingency").path(),
            two_policy,
            two.clone(),
        )
        .expect("router");
        two_router
            .validate_preapproval_contingencies(
                &RoleId::new("plan").expect("role"),
                &strict_constraints(&two, "plan"),
                false,
            )
            .expect("peer contingency");
        assert!(
            two_router
                .validate_preapproval_contingencies(
                    &RoleId::new("plan").expect("role"),
                    &strict_constraints(&two, "plan"),
                    true,
                )
                .is_err()
        );
    }

    fn binding(role: &str, profile: &str, weight: u32) -> RoleBinding {
        RoleBinding::new(
            RoleId::new(role).expect("role"),
            ProfileId::new(profile).expect("profile"),
            weight.try_into().expect("nonzero"),
        )
    }

    fn pinned_snapshot(
        profile_id: &str,
        provider_id: &str,
        roles: &[&str],
    ) -> crate::bursar::RosterSnapshot {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema": "bursar/roster@2",
            "generated_at": "2026-07-22T00:00:00Z",
            "source_artifact": {
                "path": "/tmp/roster.toml",
                "sha256": "b".repeat(64),
            },
            "policy_sha256": "a".repeat(64),
            "providers": [{
                "provider_id": provider_id,
                "availability_key": format!("{provider_id}-availability"),
                "enabled": true,
                "state": "healthy",
                "availability": "healthy",
                "checked_at": "2026-07-22T00:00:00Z",
                "data_as_of": null,
                "expires_at": null,
                "reason": null,
                "eligible": true,
                "ineligibility_reason": null,
            }],
            "profiles": [{
                "profile_id": profile_id,
                "provider_id": provider_id,
                "model": "model",
                "harness": "omp",
                "dispatch_id": format!("{provider_id}/model"),
                "reasoning_effort": "max",
                "tier": "lead",
                "ceiling": "XL",
                "efficiency": "heavy",
                "cost": 1.0,
                "data_policy": "standard",
                "enabled": true,
                "roles": roles,
                "state": "healthy",
                "eligible": true,
                "ineligibility_reason": null,
            }],
        }))
        .expect("snapshot json");
        crate::bursar::parse_roster_snapshot(&bytes).expect("valid snapshot")
    }

    fn pinned_snapshot_many(profiles: &[(&str, &str)]) -> crate::bursar::RosterSnapshot {
        let providers = profiles
            .iter()
            .map(|(_, provider_id)| {
                serde_json::json!({
                    "provider_id": provider_id,
                    "availability_key": format!("{provider_id}-availability"),
                    "enabled": true,
                    "state": "healthy",
                    "availability": "healthy",
                    "checked_at": "2026-07-22T00:00:00Z",
                    "data_as_of": null,
                    "expires_at": null,
                    "reason": null,
                    "eligible": true,
                    "ineligibility_reason": null,
                })
            })
            .collect::<Vec<_>>();
        let profiles = profiles
            .iter()
            .map(|(profile_id, provider_id)| {
                serde_json::json!({
                    "profile_id": profile_id,
                    "provider_id": provider_id,
                    "model": "model",
                    "harness": "omp",
                    "dispatch_id": format!("{provider_id}/model"),
                    "reasoning_effort": "max",
                    "tier": "lead",
                    "ceiling": "XL",
                    "efficiency": "heavy",
                    "cost": 1.0,
                    "data_policy": "standard",
                    "enabled": true,
                    "roles": ["plan"],
                    "state": "healthy",
                    "eligible": true,
                    "ineligibility_reason": null,
                })
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema": "bursar/roster@2",
            "generated_at": "2026-07-22T00:00:00Z",
            "source_artifact": {
                "path": "/tmp/roster.toml",
                "sha256": "b".repeat(64),
            },
            "policy_sha256": "a".repeat(64),
            "providers": providers,
            "profiles": profiles,
        }))
        .expect("snapshot json");
        crate::bursar::parse_roster_snapshot(&bytes).expect("valid snapshot")
    }

    fn strict_constraints(snapshot: &crate::bursar::RosterSnapshot, role: &str) -> HardEligibility {
        let allowed_profile_ids = snapshot
            .profiles
            .iter()
            .map(|profile| ProfileId::new(profile.profile_id.clone()).expect("profile"))
            .collect();
        let allowed_provider_ids = snapshot
            .providers
            .iter()
            .map(|provider| provider.provider_id.clone())
            .collect();
        let approved_execution_keys = snapshot
            .profiles
            .iter()
            .map(|profile| {
                let provider = snapshot
                    .providers
                    .iter()
                    .find(|provider| provider.provider_id == profile.provider_id)
                    .expect("provider");
                super::approved_execution(profile, provider).execution_key
            })
            .collect();
        HardEligibility {
            allowed_profile_ids,
            allowed_provider_ids,
            approved_execution_keys,
            required_roles: [RoleId::new(role).expect("role")].into_iter().collect(),
            allowed_data_policies: ["standard".to_string()].into_iter().collect(),
            minimum_tier: crate::config::Tier::Lead,
            minimum_ceiling: crate::config::Ceiling::Xl,
            budget_available: true,
            max_in_flight_per_profile: 1.try_into().expect("nonzero"),
            provider_distinct_from: BTreeSet::new(),
        }
    }

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "conductor-role-routing-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("tempdir");
            Self { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
