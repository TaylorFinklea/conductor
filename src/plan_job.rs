//! Native approval-gated plan-job lifecycle.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const PLAN_DOCUMENT_SCHEMA: &str = "conductor/plan-document@1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PlanOutputKind {
    Spec,
    ImplementationPlan,
}

impl PlanOutputKind {
    pub(crate) const fn minimum_tier(self, target: crate::run::PlanTier) -> crate::run::PlanTier {
        match self {
            Self::Spec => crate::run::PlanTier::Lead,
            Self::ImplementationPlan => match target {
                crate::run::PlanTier::Lead => crate::run::PlanTier::Lead,
                crate::run::PlanTier::Senior | crate::run::PlanTier::Junior => {
                    crate::run::PlanTier::Senior
                }
            },
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::ImplementationPlan => "implementation-plan",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum PlanDocument {
    Spec {
        schema: String,
        title: String,
        context: String,
        goals: Vec<String>,
        constraints: Vec<String>,
        requirements: Vec<String>,
        acceptance: Vec<String>,
        verification: Vec<String>,
        non_goals: Vec<String>,
        risks: Vec<String>,
        assumptions: Vec<String>,
        open_questions: Vec<String>,
    },
    ImplementationPlan {
        schema: String,
        title: String,
        context: String,
        tasks: Vec<PlanTask>,
        risks: Vec<String>,
        assumptions: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanTask {
    id: String,
    depends_on: Vec<String>,
    targets: Vec<PlanTargetSymbol>,
    change: String,
    acceptance: String,
    verify: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanTargetSymbol {
    file: String,
    symbol: String,
}

impl PlanDocument {
    fn validate(&self) -> Result<(), String> {
        let (schema, title, context) = match self {
            Self::Spec {
                schema,
                title,
                context,
                ..
            }
            | Self::ImplementationPlan {
                schema,
                title,
                context,
                ..
            } => (schema, title, context),
        };
        if schema != PLAN_DOCUMENT_SCHEMA {
            return Err(format!("unsupported plan document schema {schema:?}"));
        }
        required_text(title, "title")?;
        required_text(context, "context")?;
        match self {
            Self::Spec {
                goals,
                constraints,
                requirements,
                acceptance,
                verification,
                non_goals,
                risks,
                assumptions,
                open_questions,
                ..
            } => {
                required_list(goals, "goals")?;
                required_list(constraints, "constraints")?;
                required_list(requirements, "requirements")?;
                required_list(acceptance, "acceptance")?;
                required_list(verification, "verification")?;
                optional_list(non_goals, "non_goals")?;
                optional_list(risks, "risks")?;
                optional_list(assumptions, "assumptions")?;
                optional_list(open_questions, "open_questions")?;
            }
            Self::ImplementationPlan {
                tasks,
                risks,
                assumptions,
                ..
            } => {
                if tasks.is_empty() {
                    return Err("implementation plan tasks must not be empty".to_string());
                }
                let mut task_ids = BTreeSet::new();
                for task in tasks {
                    required_identifier(&task.id, "task id")?;
                    if !task_ids.insert(task.id.as_str()) {
                        return Err(format!("duplicate implementation plan task id {}", task.id));
                    }
                    if task.targets.is_empty() {
                        return Err(format!(
                            "task {} must name at least one file/symbol target",
                            task.id
                        ));
                    }
                    for target in &task.targets {
                        required_file_path(&target.file)?;
                        required_text(&target.symbol, "task target symbol")?;
                    }
                    required_text(&task.change, "task change")?;
                    required_text(&task.acceptance, "task acceptance")?;
                    required_text(&task.verify, "task verify")?;
                }
                let mut earlier = BTreeSet::new();
                for task in tasks {
                    let mut seen_dependencies = BTreeSet::new();
                    for dependency in &task.depends_on {
                        required_identifier(dependency, "task dependency")?;
                        if !seen_dependencies.insert(dependency.as_str()) {
                            return Err(format!(
                                "task {} has duplicate dependency {dependency}",
                                task.id
                            ));
                        }
                        if !earlier.contains(dependency.as_str()) {
                            return Err(format!(
                                "task {} dependency {dependency} must reference an earlier task",
                                task.id
                            ));
                        }
                    }
                    earlier.insert(task.id.as_str());
                }
                optional_list(risks, "risks")?;
                optional_list(assumptions, "assumptions")?;
            }
        }
        Ok(())
    }

    fn has_open_questions(&self) -> bool {
        matches!(self, Self::Spec { open_questions, .. } if !open_questions.is_empty())
    }
}

fn required_text(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("plan document {field} must not be empty"));
    }
    Ok(())
}

fn required_list(values: &[String], field: &str) -> Result<(), String> {
    if values.is_empty() {
        return Err(format!("plan document {field} must not be empty"));
    }
    optional_list(values, field)
}

fn optional_list(values: &[String], field: &str) -> Result<(), String> {
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(format!("plan document {field} contains empty text"));
    }
    Ok(())
}

fn required_identifier(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "plan document {field} must be an opaque identifier"
        ));
    }
    Ok(())
}

fn required_file_path(value: &str) -> Result<(), String> {
    let path = std::path::Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("plan document task target file must be a nonempty relative path".to_string());
    }
    Ok(())
}

/// Parses a strict wire document and returns only after validating its selected
/// output-kind invariants.
fn parse_document(kind: PlanOutputKind, bytes: &[u8]) -> Result<PlanDocument, String> {
    let document: PlanDocument = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid plan document JSON: {error}"))?;
    let actual_kind = match document {
        PlanDocument::Spec { .. } => PlanOutputKind::Spec,
        PlanDocument::ImplementationPlan { .. } => PlanOutputKind::ImplementationPlan,
    };
    if actual_kind != kind {
        return Err(format!(
            "plan document kind {} does not match approved output kind {}",
            actual_kind.label(),
            kind.label()
        ));
    }
    document.validate()?;
    Ok(document)
}

fn canonical_document_json(document: &PlanDocument) -> Result<Vec<u8>, String> {
    serde_json::to_vec(document)
        .map_err(|error| format!("failed to serialize plan document: {error}"))
}

fn render_markdown(document: &PlanDocument) -> String {
    let mut markdown = String::new();
    match document {
        PlanDocument::Spec {
            title,
            context,
            goals,
            constraints,
            requirements,
            acceptance,
            verification,
            non_goals,
            risks,
            assumptions,
            open_questions,
            ..
        } => {
            let _ = writeln!(markdown, "# {title}\n\n{context}\n");
            render_section(&mut markdown, "Goals", goals);
            render_section(&mut markdown, "Constraints and invariants", constraints);
            render_section(&mut markdown, "Requirements", requirements);
            render_section(&mut markdown, "Acceptance", acceptance);
            render_section(&mut markdown, "Verification", verification);
            render_section(&mut markdown, "Non-goals", non_goals);
            render_section(&mut markdown, "Risks", risks);
            render_section(&mut markdown, "Assumptions", assumptions);
            render_section(&mut markdown, "Open questions", open_questions);
        }
        PlanDocument::ImplementationPlan {
            title,
            context,
            tasks,
            risks,
            assumptions,
            ..
        } => {
            let _ = writeln!(markdown, "# {title}\n\n{context}\n\n## Tasks\n");
            for task in tasks {
                let _ = writeln!(markdown, "### {}\n", task.id);
                if !task.depends_on.is_empty() {
                    let _ = writeln!(markdown, "Depends on: {}\n", task.depends_on.join(", "));
                }
                let _ = writeln!(markdown, "Targets:");
                for target in &task.targets {
                    let _ = writeln!(markdown, "- `{}` — `{}`", target.file, target.symbol);
                }
                let _ = writeln!(
                    markdown,
                    "\nChange: {}\n\nAcceptance: {}\n\nVerify: `{}`\n",
                    task.change, task.acceptance, task.verify
                );
            }
            render_section(&mut markdown, "Risks", risks);
            render_section(&mut markdown, "Assumptions", assumptions);
        }
    }
    markdown
}

fn render_section(markdown: &mut String, heading: &str, items: &[String]) {
    let _ = writeln!(markdown, "\n## {heading}\n");
    if items.is_empty() {
        let _ = writeln!(markdown, "None.");
    } else {
        for item in items {
            let _ = writeln!(markdown, "- {item}");
        }
    }
}

static PLAN_RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One exact source captured for an approval-gated plan.
#[derive(Debug, Clone)]
pub(crate) enum PlanPrepareInput {
    Bead {
        bead_id: String,
        bytes: Vec<u8>,
        tier: crate::run::PlanTier,
        complexity: crate::run::PlanComplexity,
    },
    Artifact {
        bytes: Vec<u8>,
        tier: crate::run::PlanTier,
        complexity: crate::run::PlanComplexity,
    },
}

/// CLI-independent plan preparation request.
#[derive(Debug, Clone)]
pub(crate) struct PlanPrepareRequest {
    pub(crate) repo: PathBuf,
    pub(crate) input: PlanPrepareInput,
    pub(crate) output_kind: PlanOutputKind,
    pub(crate) max_plan_revisions: u8,
    pub(crate) require_second_opinion: bool,
}

/// Durable plan state and approval-report roots.
#[derive(Debug, Clone)]
pub(crate) struct PlanJobPaths {
    pub(crate) state_dir: PathBuf,
    pub(crate) reports_home: PathBuf,
}

/// A planner invocation deliberately accepts only a disposable worktree and
/// returns bytes; it cannot receive authority to mutate the target checkout.
pub(crate) trait PlanAuthor {
    fn author(&self, request: &PlanAuthorRequest) -> Result<Vec<u8>, String>;
}

/// Immutable author invocation context.
#[derive(Debug, Clone)]
pub(crate) struct PlanAuthorRequest {
    pub(crate) worktree: PathBuf,
    pub(crate) input: Vec<u8>,
    pub(crate) output_kind: PlanOutputKind,
    pub(crate) execution: crate::run::ApprovedExecution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanApproval {
    schema: String,
    decision: String,
    run_id: String,
    output_kind: PlanOutputKind,
    target_head: String,
    target_status: String,
    target_sha256: String,
    roster_policy_sha256: String,
    scheduler_policy_sha256: String,
    reservation: crate::role_routing::Reservation,
    author: crate::run::ApprovedExecution,
    approval_block_id: String,
    approval_watermark: String,
    require_second_opinion: bool,
}

/// Returned after all immutable preparation artifacts and the approval report
/// exist. It deliberately contains no model output.
#[derive(Debug, Clone)]
pub(crate) struct PreparedPlan {
    pub(crate) run_id: String,
    pub(crate) report_path: PathBuf,
}

fn plan_run_id() -> String {
    let sequence = PLAN_RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "plan-{}-p{}-{sequence:06}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9f"),
        std::process::id()
    )
}

/// Captures all plan inputs and publishes a human approval gate without
/// starting a model or mutating the target repository.
#[expect(
    clippy::too_many_lines,
    reason = "preparation intentionally captures every immutable authorization input before approval"
)]
pub(crate) fn prepare<C: crate::bursar::BursarClient + ?Sized>(
    paths: &PlanJobPaths,
    config: &crate::config::Config,
    bursar: &C,
    request: PlanPrepareRequest,
) -> Result<PreparedPlan, String> {
    let repo = canonical_repo(&request.repo)?;
    let (target_head, target_status) = git_identity(&repo)?;
    if !target_status.is_empty() {
        return Err("plan target repository must be clean at preparation".to_string());
    }
    let snapshot = bursar
        .roster_snapshot()
        .map_err(|error| format!("bursar roster snapshot: {error}"))?;
    bursar
        .status()
        .map_err(|error| format!("bursar provider state: {error}"))?;
    let roster_bytes = snapshot.snapshot_bytes().to_vec();
    let policy_sha256 = snapshot.policy_sha256().to_string();
    let source_artifact = snapshot.source_artifact().clone();
    let captured_input_bytes = raw_input_bytes(&request.input).to_vec();
    let input = plan_input_from_request(request.input)?;
    let tier = plan_input_tier(&input);
    let minimum_tier = request.output_kind.minimum_tier(tier);
    let run_id = plan_run_id();
    let policy = crate::role_routing::RoutingPolicy::from_config(config, &snapshot)
        .map_err(|error| format!("role policy: {error}"))?;
    let constraints = planner_constraints(&snapshot, config_tier(minimum_tier))?;
    let router =
        crate::role_routing::RoleRouter::with_pinned_snapshot(&paths.state_dir, policy, snapshot)
            .map_err(|error| format!("role router: {error}"))?;
    let role = crate::role_routing::RoleId::new("planner")
        .map_err(|error| format!("planner role: {error}"))?;
    router
        .validate_preapproval_contingencies(&role, &constraints, request.require_second_opinion)
        .map_err(|error| format!("plan peer contingency: {error}"))?;
    let prepared = router
        .prepare_planner(
            crate::role_routing::RunId::new(run_id.clone())
                .map_err(|error| format!("plan run id: {error}"))?,
            role,
            constraints,
        )
        .map_err(|error| format!("planner selection: {error}"))?;
    let plan_routes = planned_routes(&prepared);
    let revision_limit = crate::run::RevisionLimit::new(request.max_plan_revisions)
        .map_err(|error| error.to_string())?;
    let stage_attempt_limit =
        crate::run::StageAttemptLimit::new(2).map_err(|error| error.to_string())?;
    let approval_watermark = chrono::Utc::now().to_rfc3339();
    let approval = PlanApproval {
        schema: "conductor/plan-approval@1".to_string(),
        decision: "awaiting_approval".to_string(),
        run_id: run_id.clone(),
        output_kind: request.output_kind,
        target_head,
        target_status,
        target_sha256: input_artifact_sha256(&input),
        roster_policy_sha256: policy_sha256.clone(),
        scheduler_policy_sha256: prepared.policy_digest.clone(),
        reservation: prepared.reservation.clone(),
        author: prepared.selected.clone(),
        approval_block_id: "dispatch-plan".to_string(),
        approval_watermark: approval_watermark.clone(),
        require_second_opinion: request.require_second_opinion,
    };
    let input_bytes = captured_input_bytes;
    let target = crate::run::PlanTarget {
        repo: repo.to_string_lossy().into_owned(),
        input,
    };
    let details = crate::run::PlanRunDetails {
        target,
        routes: plan_routes,
        progress: crate::run::PlanProgress::Prepared,
        revision_limit,
        stage_attempt_limit,
    };
    let handle = crate::run::RunHandle::create_plan(
        &paths.state_dir,
        crate::run::NewPlanRun {
            run_id: run_id.clone(),
            target: crate::run::RunTarget {
                repo: repo.to_string_lossy().into_owned(),
                bead: plan_bead(&details.target.input),
            },
            details,
            approved_profiles: prepared
                .audited_pool
                .iter()
                .filter(|candidate| candidate.eligible)
                .map(|candidate| candidate.execution.profile_id.clone())
                .collect(),
            bursar_roster_artifact: Some(crate::run::ArtifactRef {
                path: source_artifact.path,
                sha256: source_artifact.sha256,
            }),
            roster_snapshot: crate::run::RosterSnapshotInput {
                bytes: roster_bytes,
                policy_sha256,
            },
            limits: crate::run::RunLimits {
                item_wall_clock_mins: Some(u64::from(config.budgets.item_wall_clock_mins)),
                max_attempts: Some(2),
            },
            verifier: crate::run::RunVerifier {
                mechanical: Some("conductor/plan-document@1 validation".to_string()),
                qualitative: None,
            },
            approval: serde_json::to_value(&approval)
                .map_err(|error| format!("plan approval serialization: {error}"))?,
            input_bytes,
        },
    )
    .map_err(|error| format!("plan run artifact: {error}"))?;
    let report = crate::deck::Report::new(
        &run_id,
        format!("Plan approval: {run_id}"),
        approval_watermark,
        crate::deck::ReportStatus::AwaitingReview,
        vec![
            crate::deck::Block::metrics(
                "Plan",
                vec![
                    crate::deck::Metric::new("Output", request.output_kind.label()),
                    crate::deck::Metric::new("Planner", prepared.selected.profile_id),
                ],
                Vec::new(),
            ),
            crate::deck::Block::approval(
                approval.approval_block_id.clone(),
                "Approve this exact immutable plan authorization before model dispatch.",
            ),
        ],
    )
    .map_err(|error| format!("plan approval report: {error}"))?;
    let report_path = crate::deck::write_report(&paths.reports_home, &report)
        .map_err(|error| format!("plan approval report: {error}"))?;
    let _ = handle;
    Ok(PreparedPlan {
        run_id,
        report_path,
    })
}

fn canonical_repo(path: &Path) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("plan repo {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err("plan repo must be a directory".to_string());
    }
    Ok(canonical)
}

fn git_identity(repo: &Path) -> Result<(String, String), String> {
    let head = git_output(repo, ["rev-parse", "HEAD"])?;
    let status = git_output(repo, ["status", "--porcelain=v1"])?;
    if head.len() != 40 || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("plan repo has no immutable HEAD".to_string());
    }
    Ok((head, status))
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("git in {}: {error}", repo.display()))?;
    if !output.status.success() {
        return Err(format!(
            "git in {} failed: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "git output was not UTF-8".to_string())
        .map(|value| value.trim_end().to_string())
}

fn plan_input_from_request(request: PlanPrepareInput) -> Result<crate::run::PlanInput, String> {
    let (bytes, tier, complexity, bead) = match request {
        PlanPrepareInput::Bead {
            bead_id,
            bytes,
            tier,
            complexity,
        } => {
            if !valid_identifier(&bead_id) {
                return Err("plan Bead id must be an opaque identifier".to_string());
            }
            (bytes, tier, complexity, Some(bead_id))
        }
        PlanPrepareInput::Artifact {
            bytes,
            tier,
            complexity,
        } => (bytes, tier, complexity, None),
    };
    if bytes.is_empty() {
        return Err("plan input artifact must not be empty".to_string());
    }
    let artifact = crate::run::ArtifactRef {
        path: if bead.is_some() {
            "artifacts/input-bead.json".to_string()
        } else {
            "artifacts/input-artifact".to_string()
        },
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    Ok(match bead {
        Some(bead_id) => crate::run::PlanInput::Bead {
            bead_id,
            artifact,
            tier,
            complexity,
        },
        None => crate::run::PlanInput::Artifact {
            artifact,
            tier,
            complexity,
        },
    })
}
fn raw_input_bytes(input: &PlanPrepareInput) -> &[u8] {
    match input {
        PlanPrepareInput::Bead { bytes, .. } | PlanPrepareInput::Artifact { bytes, .. } => bytes,
    }
}

fn plan_input_tier(input: &crate::run::PlanInput) -> crate::run::PlanTier {
    match input {
        crate::run::PlanInput::Bead { tier, .. } | crate::run::PlanInput::Artifact { tier, .. } => {
            *tier
        }
    }
}

fn input_artifact_sha256(input: &crate::run::PlanInput) -> String {
    match input {
        crate::run::PlanInput::Bead { artifact, .. }
        | crate::run::PlanInput::Artifact { artifact, .. } => artifact.sha256.clone(),
    }
}

fn plan_bead(input: &crate::run::PlanInput) -> Option<String> {
    match input {
        crate::run::PlanInput::Bead { bead_id, .. } => Some(bead_id.clone()),
        crate::run::PlanInput::Artifact { .. } => None,
    }
}

const fn config_tier(tier: crate::run::PlanTier) -> crate::config::Tier {
    match tier {
        crate::run::PlanTier::Junior => crate::config::Tier::Junior,
        crate::run::PlanTier::Senior => crate::config::Tier::Senior,
        crate::run::PlanTier::Lead => crate::config::Tier::Lead,
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn planner_constraints(
    snapshot: &crate::bursar::RosterSnapshot,
    minimum_tier: crate::config::Tier,
) -> Result<crate::role_routing::HardEligibility, String> {
    let mut profiles = BTreeSet::new();
    let mut providers = BTreeSet::new();
    let mut execution_keys = BTreeSet::new();
    for profile in &snapshot.profiles {
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.provider_id == profile.provider_id)
            .ok_or_else(|| "Bursar profile has no provider".to_string())?;
        profiles.insert(
            crate::role_routing::ProfileId::new(profile.profile_id.clone())
                .map_err(|error| error.to_string())?,
        );
        providers.insert(profile.provider_id.clone());
        execution_keys
            .insert(crate::role_routing::approved_execution(profile, provider).execution_key);
    }
    Ok(crate::role_routing::HardEligibility {
        allowed_profile_ids: profiles,
        allowed_provider_ids: providers,
        approved_execution_keys: execution_keys,
        required_roles: [
            crate::role_routing::RoleId::new("planner").map_err(|error| error.to_string())?
        ]
        .into_iter()
        .collect(),
        allowed_data_policies: [
            "standard".to_string(),
            "zero-retention".to_string(),
            "local-only".to_string(),
        ]
        .into_iter()
        .collect(),
        minimum_tier,
        minimum_ceiling: crate::config::Ceiling::Xl,
        budget_available: true,
        max_in_flight_per_profile: NonZeroU32::new(1).expect("one is nonzero"),
        provider_distinct_from: BTreeSet::new(),
    })
}

fn planned_routes(prepared: &crate::role_routing::PreparedPlanner) -> crate::run::PlanRoutes {
    let candidates = prepared
        .audited_pool
        .iter()
        .filter(|candidate| candidate.eligible)
        .map(|candidate| candidate.execution.clone())
        .collect::<Vec<_>>();
    crate::run::PlanRoutes {
        stages: vec![
            crate::run::PlanStageRoute {
                stage: crate::run::PlanStage::Planner,
                capability_role: "planner".to_string(),
                candidates: candidates.clone(),
                provider_distinct_from: Vec::new(),
            },
            crate::run::PlanStageRoute {
                stage: crate::run::PlanStage::PeerReview,
                capability_role: "planner".to_string(),
                candidates: candidates.clone(),
                provider_distinct_from: vec![crate::run::PlanStage::Planner],
            },
            crate::run::PlanStageRoute {
                stage: crate::run::PlanStage::SecondOpinion,
                capability_role: "planner".to_string(),
                candidates,
                provider_distinct_from: vec![
                    crate::run::PlanStage::Planner,
                    crate::run::PlanStage::PeerReview,
                ],
            },
        ],
    }
}

/// Runs only the initial author stage after the exact published approval is
/// present. Peer verdicts and second opinion remain intentionally out of scope.
#[expect(
    clippy::too_many_lines,
    reason = "dispatch keeps all approval, input, provider, isolation, and checkpoint gates explicit"
)]
pub(crate) fn dispatch<C, A>(
    paths: &PlanJobPaths,
    config: &crate::config::Config,
    bursar: &C,
    run_id: &str,
    author: &A,
) -> Result<(), String>
where
    C: crate::bursar::BursarClient + ?Sized,
    A: PlanAuthor + ?Sized,
{
    let mut run = crate::run::RunHandle::open(&paths.state_dir, run_id)
        .map_err(|error| format!("plan run: {error}"))?;
    let approval = load_approval(&run)?;
    let plan = run.plan().map_err(|error| error.to_string())?;
    if plan.target.repo != run.manifest().target.repo {
        return Err("plan manifest target does not match structural plan target".to_string());
    }
    if matches!(plan.progress, crate::run::PlanProgress::AwaitingPeer { .. }) {
        return Ok(());
    }
    if matches!(plan.progress, crate::run::PlanProgress::Terminal { .. }) {
        return Err("terminal plan runs cannot be dispatched".to_string());
    }
    approval_response(paths, &approval)?;
    let repo = PathBuf::from(&plan.target.repo);
    let (head, status) = git_identity(&repo)?;
    if head != approval.target_head || status != approval.target_status {
        return Err("plan target HEAD or status changed after approval".to_string());
    }
    let input_ref = match &plan.target.input {
        crate::run::PlanInput::Bead { artifact, .. }
        | crate::run::PlanInput::Artifact { artifact, .. } => artifact,
    };
    if input_ref.sha256 != approval.target_sha256 {
        return Err("plan input digest does not match approval".to_string());
    }
    let input_bytes = std::fs::read(run.dir().join(&input_ref.path))
        .map_err(|error| format!("captured plan input: {error}"))?;
    if format!("{:x}", Sha256::digest(&input_bytes)) != input_ref.sha256 {
        return Err("captured plan input digest changed".to_string());
    }
    let roster_bytes = std::fs::read(run.dir().join("roster.json"))
        .map_err(|error| format!("captured Bursar roster: {error}"))?;
    let captured_snapshot = crate::bursar::parse_roster_snapshot(&roster_bytes)
        .map_err(|error| format!("captured Bursar roster: {error}"))?;
    if captured_snapshot.policy_sha256() != approval.roster_policy_sha256 {
        return Err("captured Bursar policy digest does not match approval".to_string());
    }
    let policy = crate::role_routing::RoutingPolicy::from_config(config, &captured_snapshot)
        .map_err(|error| format!("role policy changed: {error}"))?;
    if policy.digest() != approval.scheduler_policy_sha256 {
        return Err("scheduler policy digest changed after approval".to_string());
    }
    let router = crate::role_routing::RoleRouter::with_pinned_snapshot(
        &paths.state_dir,
        policy,
        captured_snapshot,
    )
    .map_err(|error| format!("role router: {error}"))?;
    let _run_guard = router
        .acquire_run_transition(
            &crate::role_routing::RunId::new(run_id.to_string())
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("plan run guard: {error}"))?;
    let selected = match run
        .plan()
        .map_err(|error| error.to_string())?
        .progress
        .clone()
    {
        crate::run::PlanProgress::Prepared => {
            recheck_author(bursar, &approval.author)?;
            router
                .commit(&approval.reservation)
                .map_err(|error| format!("planner reservation: {error}"))?;
            run.start_plan_authoring(approval.author.clone())
                .map_err(|error| format!("plan author checkpoint: {error}"))?;
            approval.author.clone()
        }
        crate::run::PlanProgress::Authoring { author, .. } => {
            if author != approval.author {
                return Err("persisted plan author differs from immutable approval".to_string());
            }
            recheck_author(bursar, &author)?;
            author
        }
        _ => return Err("plan dispatch state is not resumable authoring".to_string()),
    };
    let output = with_isolated_worktree(&repo, &approval.target_head, |worktree| {
        author.author(&PlanAuthorRequest {
            worktree: worktree.to_path_buf(),
            input: input_bytes.clone(),
            output_kind: approval.output_kind,
            execution: selected.clone(),
        })
    })?;
    let document = match parse_document(approval.output_kind, &output) {
        Ok(document) => document,
        Err(first_error) => {
            let repair = with_isolated_worktree(&repo, &approval.target_head, |worktree| {
                author.author(&PlanAuthorRequest {
                    worktree: worktree.to_path_buf(),
                    input: input_bytes.clone(),
                    output_kind: approval.output_kind,
                    execution: selected.clone(),
                })
            })?;
            parse_document(approval.output_kind, &repair).map_err(|second_error| {
                format!("plan author output invalid after repair: {first_error}; {second_error}")
            })?
        }
    };
    let canonical = canonical_document_json(&document)?;
    let json = run
        .capture_plan_artifact(Path::new("artifacts/plan-document.json"), &canonical)
        .map_err(|error| format!("plan JSON artifact: {error}"))?;
    let markdown = render_markdown(&document);
    run.capture_plan_artifact(Path::new("artifacts/plan-document.md"), markdown.as_bytes())
        .map_err(|error| format!("plan Markdown artifact: {error}"))?;
    let (after_head, after_status) = git_identity(&repo)?;
    if after_head != approval.target_head || after_status != approval.target_status {
        return Err("plan author changed target repository HEAD or status".to_string());
    }
    if document.has_open_questions() {
        run.finish_plan_needs_input(json)
            .map_err(|error| format!("plan needs-input checkpoint: {error}"))
    } else {
        run.await_plan_peer(json)
            .map_err(|error| format!("plan peer checkpoint: {error}"))
    }
}

/// Cancels only a plan that has not started authoring, releasing the pending
/// reservation while preserving the scheduler's rotation evidence.
pub(crate) fn cancel(
    paths: &PlanJobPaths,
    config: &crate::config::Config,
    run_id: &str,
) -> Result<(), String> {
    let mut run = crate::run::RunHandle::open(&paths.state_dir, run_id)
        .map_err(|error| format!("plan run: {error}"))?;
    let approval = load_approval(&run)?;
    if !matches!(
        run.plan().map_err(|error| error.to_string())?.progress,
        crate::run::PlanProgress::Prepared
    ) {
        return Err("plan cancel is legal only before authoring starts".to_string());
    }
    let roster_bytes = std::fs::read(run.dir().join("roster.json"))
        .map_err(|error| format!("captured Bursar roster: {error}"))?;
    let snapshot = crate::bursar::parse_roster_snapshot(&roster_bytes)
        .map_err(|error| format!("captured Bursar roster: {error}"))?;
    let policy = crate::role_routing::RoutingPolicy::from_config(config, &snapshot)
        .map_err(|error| format!("role policy changed: {error}"))?;
    let router =
        crate::role_routing::RoleRouter::with_pinned_snapshot(&paths.state_dir, policy, snapshot)
            .map_err(|error| format!("role router: {error}"))?;
    let _guard = router
        .acquire_run_transition(
            &crate::role_routing::RunId::new(run_id.to_string())
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("plan run guard: {error}"))?;
    router
        .cancel(&approval.reservation)
        .map_err(|error| format!("planner reservation: {error}"))?;
    run.cancel_prepared_plan()
        .map_err(|error| format!("plan cancellation: {error}"))
}

pub(crate) fn status(paths: &PlanJobPaths, run_id: &str) -> Result<String, String> {
    let run = crate::run::RunHandle::open(&paths.state_dir, run_id)
        .map_err(|error| format!("plan run: {error}"))?;
    let plan = run.plan().map_err(|error| error.to_string())?;
    let state = match &plan.progress {
        crate::run::PlanProgress::Prepared => "awaiting_approval",
        crate::run::PlanProgress::Authoring { .. } => "authoring",
        crate::run::PlanProgress::AwaitingPeer { .. } => "awaiting_peer",
        crate::run::PlanProgress::Revising { .. } => "revising",
        crate::run::PlanProgress::AwaitingSecondOpinion { .. } => "awaiting_second_opinion",
        crate::run::PlanProgress::Terminal { verdict } => match verdict {
            crate::run::PlanTerminalVerdict::Accepted => "accepted",
            crate::run::PlanTerminalVerdict::Rejected => "rejected",
            crate::run::PlanTerminalVerdict::Blocked => "blocked",
            crate::run::PlanTerminalVerdict::NeedsInput => "needs_input",
        },
    };
    Ok(state.to_string())
}

fn load_approval(run: &crate::run::RunHandle) -> Result<PlanApproval, String> {
    let approval: PlanApproval =
        serde_json::from_value(run.approval().map_err(|error| error.to_string())?)
            .map_err(|error| format!("plan approval: {error}"))?;
    if approval.schema != "conductor/plan-approval@1"
        || approval.decision != "awaiting_approval"
        || approval.run_id != run.run_id()
        || !valid_identifier(&approval.approval_block_id)
    {
        return Err("plan approval artifact is malformed".to_string());
    }
    Ok(approval)
}

fn approval_response(paths: &PlanJobPaths, approval: &PlanApproval) -> Result<(), String> {
    let report_dir = crate::deck::report_run_dir(&paths.reports_home, &approval.run_id)
        .map_err(|error| format!("plan approval report: {error}"))?;
    let responses = crate::deck::read_responses(&report_dir)
        .map_err(|error| format!("plan approval responses: {error}"))?;
    let response = responses
        .response_after(
            &approval.approval_block_id,
            Some(&approval.approval_watermark),
        )
        .ok_or_else(|| "plan dispatch requires an exact later approval".to_string())?;
    if response.value() != "approved" {
        return Err("plan approval did not authorize dispatch".to_string());
    }
    Ok(())
}

fn recheck_author<C: crate::bursar::BursarClient + ?Sized>(
    bursar: &C,
    approved: &crate::run::ApprovedExecution,
) -> Result<(), String> {
    let live = bursar
        .roster_snapshot()
        .map_err(|error| format!("live Bursar roster snapshot: {error}"))?;
    let profile = live
        .profiles
        .iter()
        .find(|profile| profile.profile_id == approved.profile_id)
        .ok_or_else(|| "approved plan author is absent from live Bursar roster".to_string())?;
    let provider = live
        .providers
        .iter()
        .find(|provider| provider.provider_id == profile.provider_id)
        .ok_or_else(|| {
            "approved plan author provider is absent from live Bursar roster".to_string()
        })?;
    if !profile.eligible
        || !provider.eligible
        || crate::role_routing::approved_execution(profile, provider) != *approved
    {
        return Err("approved plan author is no longer exactly eligible".to_string());
    }
    let report = bursar
        .status()
        .map_err(|error| format!("live Bursar provider status: {error}"))?;
    let status = report.providers.get(&approved.provider_id).ok_or_else(|| {
        "approved plan author provider is absent from live Bursar status".to_string()
    })?;
    if !matches!(
        status.availability,
        crate::bursar::Availability::Healthy | crate::bursar::Availability::Caution
    ) {
        return Err("approved plan author provider is no longer available".to_string());
    }
    Ok(())
}

fn with_isolated_worktree<T>(
    repo: &Path,
    head: &str,
    action: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let sequence = PLAN_RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "conductor-plan-worktree-{}-{sequence}",
        std::process::id()
    ));
    let add = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .arg(head)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("create isolated plan worktree: {error}"))?;
    if !add.status.success() {
        return Err(format!(
            "create isolated plan worktree: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }
    let result = action(&path);
    let remove = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(&path)
        .stdin(Stdio::null())
        .output();
    let _ = std::fs::remove_dir_all(&path);
    if let Err(error) = remove {
        return Err(format!("remove isolated plan worktree: {error}"));
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_documents_require_substantive_required_sections() {
        let error = parse_document(
            PlanOutputKind::Spec,
            br#"{
                "schema": "conductor/plan-document@1",
                "kind": "spec",
                "title": " " ,
                "context": "context",
                "goals": [],
                "constraints": [],
                "requirements": [],
                "acceptance": [],
                "verification": [],
                "non_goals": [],
                "risks": [],
                "assumptions": [],
                "open_questions": []
            }"#,
        )
        .expect_err("schema-shaped empty specs must fail");

        assert!(error.contains("title"));
    }

    #[test]
    fn implementation_plan_rejects_dangling_task_dependencies() {
        let error = parse_document(
            PlanOutputKind::ImplementationPlan,
            br#"{
                "schema": "conductor/plan-document@1",
                "kind": "implementation-plan",
                "title": "Route plans",
                "context": "Conductor",
                "tasks": [{
                    "id": "author",
                    "depends_on": ["missing"],
                    "targets": [{"file": "src/plan_job.rs", "symbol": "dispatch"}],
                    "change": "Add dispatch",
                    "acceptance": "Works",
                    "verify": "cargo test plan_job"
                }],
                "risks": [],
                "assumptions": []
            }"#,
        )
        .expect_err("dangling graph edges must fail");

        assert!(error.contains("dependency"));
    }

    #[derive(Clone)]
    struct FakeBursar {
        snapshot: crate::bursar::RosterSnapshot,
        status: crate::bursar::StatusReport,
    }

    impl crate::bursar::BursarClient for FakeBursar {
        fn status(&self) -> crate::bursar::Result<crate::bursar::StatusReport> {
            Ok(self.status.clone())
        }

        fn roster_snapshot(&self) -> crate::bursar::Result<crate::bursar::RosterSnapshot> {
            Ok(self.snapshot.clone())
        }
    }

    struct FakeAuthor(Vec<Vec<u8>>);

    impl PlanAuthor for FakeAuthor {
        fn author(&self, _request: &PlanAuthorRequest) -> Result<Vec<u8>, String> {
            self.0
                .first()
                .cloned()
                .ok_or_else(|| "fake author has no output".to_string())
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "fixture constructs the exact multi-provider authoring environment"
    )]
    fn plan_fixture(label: &str) -> (TestDir, PlanJobPaths, crate::config::Config, FakeBursar) {
        let temp = TestDir::new(label);
        let checked_at = chrono::Utc::now().to_rfc3339();
        let providers = ["anthropic", "codex", "opencode-go"]
            .into_iter()
            .map(|provider| {
                (
                    provider.to_string(),
                    crate::bursar::ProviderStatus {
                        availability: crate::bursar::Availability::Healthy,
                        source: "test".to_string(),
                        checked_at: checked_at.clone(),
                        data_as_of: None,
                        expires_at: Some("2100-01-01T00:00:00Z".to_string()),
                        windows: vec![crate::bursar::Window {
                            label: "test".to_string(),
                            percent: Some(1.0),
                            reset_at: None,
                        }],
                        reason: None,
                        extra: serde_json::Map::new(),
                    },
                )
            })
            .collect();
        let provider_rows = ["anthropic", "codex", "opencode-go"]
            .into_iter()
            .map(|provider| {
                serde_json::json!({
                    "provider_id": provider, "availability_key": provider, "enabled": true,
                    "state": "healthy", "availability": "healthy", "checked_at": checked_at,
                    "data_as_of": null, "expires_at": "2100-01-01T00:00:00Z", "reason": null,
                    "eligible": true, "ineligibility_reason": null
                })
            })
            .collect::<Vec<_>>();
        let profile_rows = [
            ("planner-a", "anthropic"),
            ("planner-b", "codex"),
            ("planner-c", "opencode-go"),
        ]
        .into_iter()
        .map(|(profile_id, provider_id)| {
            serde_json::json!({
                "profile_id": profile_id, "provider_id": provider_id, "model": profile_id,
                "harness": "pi", "dispatch_id": profile_id, "reasoning_effort": null,
                "tier": "lead", "ceiling": "XL", "efficiency": "lean", "cost": 0.0,
                "data_policy": "standard", "enabled": true, "roles": ["planner"],
                "state": "healthy", "eligible": true, "ineligibility_reason": null
            })
        })
        .collect::<Vec<_>>();
        let roster = serde_json::json!({
            "schema": "bursar/roster@2",
            "generated_at": checked_at,
            "source_artifact": {"path": "/fixture/roster.toml", "sha256": "a".repeat(64)},
            "policy_sha256": "b".repeat(64),
            "providers": provider_rows,
            "profiles": profile_rows
        });
        let snapshot = crate::bursar::parse_roster_snapshot(roster.to_string().as_bytes())
            .expect("strict fixture roster");
        let config = crate::config::parse_str(
            r#"
[[role_binding]]
role = "planner"
profile_id = "planner-a"
weight = 1
enabled = true

[[role_binding]]
role = "planner"
profile_id = "planner-b"
weight = 1
enabled = true

[[role_binding]]
role = "planner"
profile_id = "planner-c"
weight = 1
enabled = true
"#,
        )
        .expect("planner config");
        (
            temp,
            PlanJobPaths {
                state_dir: std::env::temp_dir().join(format!(
                    "conductor-plan-state-{label}-{}",
                    std::process::id()
                )),
                reports_home: std::env::temp_dir().join(format!(
                    "conductor-plan-reports-{label}-{}",
                    std::process::id()
                )),
            },
            config,
            FakeBursar {
                snapshot,
                status: crate::bursar::StatusReport {
                    schema: "bursar/status@2".to_string(),
                    checked_at,
                    providers,
                },
            },
        )
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "conductor-plan-test-{label}-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().expect("nanos")
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn initialized_repo(temp: &TestDir) -> PathBuf {
        let repo = temp.0.join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        git(&repo, &["init"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Conductor Test"]);
        std::fs::write(repo.join("README"), "immutable").expect("input");
        git(&repo, &["add", "README"]);
        git(&repo, &["commit", "-m", "initial"]);
        repo
    }

    fn approve(paths: &PlanJobPaths, run_id: &str) {
        let report_dir =
            crate::deck::report_run_dir(&paths.reports_home, run_id).expect("report dir");
        std::fs::write(
            report_dir.join("responses.json"),
            serde_json::to_vec(&serde_json::json!({
                "responses": {"dispatch-plan": {"value": "approved", "at": "2100-01-01T00:00:00Z"}}
            }))
            .expect("responses"),
        )
        .expect("write responses");
    }

    #[test]
    fn artifact_implementation_plan_authoring_ends_at_awaiting_peer_without_repo_mutation() {
        let (temp, paths, config, bursar) = plan_fixture("artifact");
        let repo = initialized_repo(&temp);
        let before = git_output(&repo, ["rev-parse", "HEAD"]).expect("head");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo: repo.clone(),
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 0,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let author = FakeAuthor(vec![br#"{"schema":"conductor/plan-document@1","kind":"implementation-plan","title":"Plan","context":"Context","tasks":[{"id":"one","depends_on":[],"targets":[{"file":"src/x.rs","symbol":"x"}],"change":"Change","acceptance":"Accept","verify":"cargo test"}],"risks":[],"assumptions":[]}"#.to_vec()]);
        dispatch(&paths, &config, &bursar, &prepared.run_id, &author).expect("dispatch");
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "awaiting_peer"
        );
        assert_eq!(
            git_output(&repo, ["rev-parse", "HEAD"]).expect("head"),
            before
        );
    }

    #[test]
    fn bead_spec_with_open_questions_terminates_needs_input_without_target_mutation() {
        let (temp, paths, config, bursar) = plan_fixture("bead");
        let repo = initialized_repo(&temp);
        let before = git_output(&repo, ["rev-parse", "HEAD"]).expect("head");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo: repo.clone(),
                input: PlanPrepareInput::Bead {
                    bead_id: "conductor-plan-job".to_string(),
                    bytes: b"{\"title\":\"plan\"}".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::Spec,
                max_plan_revisions: 0,
                require_second_opinion: true,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let author = FakeAuthor(vec![br#"{"schema":"conductor/plan-document@1","kind":"spec","title":"Spec","context":"Context","goals":["Goal"],"constraints":["Invariant"],"requirements":["Requirement"],"acceptance":["Acceptance"],"verification":["cargo test"],"non_goals":[],"risks":[],"assumptions":[],"open_questions":["Needs operator answer"]}"#.to_vec()]);
        dispatch(&paths, &config, &bursar, &prepared.run_id, &author).expect("dispatch");
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "needs_input"
        );
        assert_eq!(
            git_output(&repo, ["rev-parse", "HEAD"]).expect("head"),
            before
        );
    }
}
