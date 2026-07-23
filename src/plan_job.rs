//! Native approval-gated plan-job lifecycle.

use chrono::Utc;
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerReviewWire {
    schema: String,
    verdict: crate::run::PeerVerdict,
    findings: Vec<PlanFinding>,
}

fn parse_peer_review(bytes: &[u8]) -> Result<(crate::run::PeerVerdict, Vec<PlanFinding>), String> {
    let review: PeerReviewWire = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid plan peer-review JSON: {error}"))?;
    if review.schema != "conductor/plan-peer-review@1" {
        return Err("plan peer-review schema must be conductor/plan-peer-review@1".to_string());
    }
    for finding in &review.findings {
        if !valid_identifier(&finding.id)
            || !matches!(
                finding.severity.as_str(),
                "low" | "medium" | "high" | "critical"
            )
            || finding.location.trim().is_empty()
            || finding.problem.trim().is_empty()
            || finding.required_change.trim().is_empty()
        {
            return Err("plan peer-review finding is malformed".to_string());
        }
    }
    match review.verdict {
        crate::run::PeerVerdict::Approve if review.findings.is_empty() => {
            Ok((crate::run::PeerVerdict::Approve, Vec::new()))
        }
        crate::run::PeerVerdict::Approve => {
            Err("approved plan peer-review must not include findings".to_string())
        }
        crate::run::PeerVerdict::Revise if !review.findings.is_empty() => {
            Ok((crate::run::PeerVerdict::Revise, review.findings))
        }
        crate::run::PeerVerdict::Revise => {
            Err("revise plan peer-review requires findings".to_string())
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecondOpinionWire {
    schema: String,
    verdict: crate::run::SecondOpinionVerdict,
}

fn parse_second_opinion(bytes: &[u8]) -> Result<crate::run::SecondOpinionVerdict, String> {
    let opinion: SecondOpinionWire = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid plan second-opinion JSON: {error}"))?;
    if opinion.schema != "conductor/plan-second-opinion@1" {
        return Err(
            "plan second-opinion schema must be conductor/plan-second-opinion@1".to_string(),
        );
    }
    Ok(opinion.verdict)
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
    pub(crate) ledger_path: PathBuf,
}

/// A plan executor receives only an isolated authoring request or the blinded
/// review material for the bounded stage it is authorized to perform.
pub(crate) trait PlanAuthor {
    fn author(&self, request: &PlanAuthorRequest) -> Result<Vec<u8>, String>;
    fn revise(&self, _request: &PlanRevisionRequest) -> Result<Vec<u8>, String> {
        Err("plan executor has no revision capability".to_string())
    }
    fn peer_review(&self, _request: &PlanPeerReviewRequest) -> Result<Vec<u8>, String> {
        Err("plan executor has no peer-review capability".to_string())
    }
    fn second_opinion(&self, _request: &PlanSecondOpinionRequest) -> Result<Vec<u8>, String> {
        Err("plan executor has no second-opinion capability".to_string())
    }
}

/// Immutable author invocation context.
#[derive(Debug, Clone)]
pub(crate) struct PlanAuthorRequest {
    pub(crate) worktree: PathBuf,
    pub(crate) input: Vec<u8>,
    pub(crate) output_kind: PlanOutputKind,
    pub(crate) execution: crate::run::ApprovedExecution,
    pub(crate) profile: crate::bursar::RosterProfile,
}

/// Blind peer-review context. It deliberately has no author identity and no
/// prior verdict: only the target, rubric, and canonical plan are visible.
#[derive(Debug, Clone)]
pub(crate) struct PlanPeerReviewRequest {
    pub(crate) worktree: PathBuf,
    pub(crate) target: crate::run::PlanTarget,
    pub(crate) rubric: String,
    pub(crate) canonical_plan: Vec<u8>,
    pub(crate) execution: crate::run::ApprovedExecution,
    pub(crate) profile: crate::bursar::RosterProfile,
}

/// A revision returns the peer's strict findings and the previous canonical
/// artifact to the same immutable author in an isolated worktree.
#[derive(Debug, Clone)]
pub(crate) struct PlanRevisionRequest {
    pub(crate) worktree: PathBuf,
    pub(crate) prior_plan: Vec<u8>,
    pub(crate) findings: Vec<PlanFinding>,
    pub(crate) output_kind: PlanOutputKind,
    pub(crate) execution: crate::run::ApprovedExecution,
    pub(crate) profile: crate::bursar::RosterProfile,
}

/// Final spec opinion context. It excludes both author identity and peer
/// verdict, leaving only the target and accepted canonical artifact.
#[derive(Debug, Clone)]
pub(crate) struct PlanSecondOpinionRequest {
    pub(crate) worktree: PathBuf,
    pub(crate) target: crate::run::PlanTarget,
    pub(crate) canonical_plan: Vec<u8>,
    pub(crate) execution: crate::run::ApprovedExecution,
    pub(crate) profile: crate::bursar::RosterProfile,
}

/// Typed peer finding returned from the strict review wire format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanFinding {
    pub(crate) id: String,
    pub(crate) severity: String,
    pub(crate) location: String,
    pub(crate) problem: String,
    pub(crate) required_change: String,
}

/// Returns an exact, checked compact JSON shape for the requested plan output.
/// The prompt embeds this serialization rather than a separately-maintained
/// prose schema, so every advertised key and nested object is parser-valid.
pub(crate) fn plan_document_prompt_contract(kind: PlanOutputKind) -> Result<String, String> {
    let document = match kind {
        PlanOutputKind::Spec => PlanDocument::Spec {
            schema: PLAN_DOCUMENT_SCHEMA.to_string(),
            title: "Example plan title".to_string(),
            context: "The immutable target and its constraints.".to_string(),
            goals: vec!["Deliver the approved outcome.".to_string()],
            constraints: vec!["Do not mutate the target repository.".to_string()],
            requirements: vec!["Preserve the strict wire contract.".to_string()],
            acceptance: vec!["The stated acceptance criteria hold.".to_string()],
            verification: vec!["cargo test".to_string()],
            non_goals: vec![],
            risks: vec![],
            assumptions: vec![],
            open_questions: vec![],
        },
        PlanOutputKind::ImplementationPlan => PlanDocument::ImplementationPlan {
            schema: PLAN_DOCUMENT_SCHEMA.to_string(),
            title: "Example implementation plan".to_string(),
            context: "The immutable target and its constraints.".to_string(),
            tasks: vec![PlanTask {
                id: "task-1".to_string(),
                depends_on: vec![],
                targets: vec![PlanTargetSymbol {
                    file: "src/example.rs".to_string(),
                    symbol: "example_symbol".to_string(),
                }],
                change: "Make the required implementation change.".to_string(),
                acceptance: "The observable contract holds.".to_string(),
                verify: "cargo test".to_string(),
            }],
            risks: vec![],
            assumptions: vec![],
        },
    };
    document.validate()?;
    String::from_utf8(canonical_document_json(&document)?)
        .map_err(|error| format!("plan prompt contract is not UTF-8: {error}"))
}

/// Returns checked peer-review examples spanning every permitted verdict and
/// finding shape. The wire type supplies the serialized field names and enums.
pub(crate) fn peer_review_prompt_contract() -> Result<String, String> {
    serde_json::to_string(&[
        PeerReviewWire {
            schema: "conductor/plan-peer-review@1".to_string(),
            verdict: crate::run::PeerVerdict::Approve,
            findings: vec![],
        },
        PeerReviewWire {
            schema: "conductor/plan-peer-review@1".to_string(),
            verdict: crate::run::PeerVerdict::Revise,
            findings: vec![PlanFinding {
                id: "finding-low".to_string(),
                severity: "low".to_string(),
                location: "src/example.rs".to_string(),
                problem: "The plan omits an observable contract.".to_string(),
                required_change: "Add the missing contract.".to_string(),
            }],
        },
        PeerReviewWire {
            schema: "conductor/plan-peer-review@1".to_string(),
            verdict: crate::run::PeerVerdict::Revise,
            findings: vec![PlanFinding {
                id: "finding-medium".to_string(),
                severity: "medium".to_string(),
                location: "src/example.rs".to_string(),
                problem: "The plan omits an observable contract.".to_string(),
                required_change: "Add the missing contract.".to_string(),
            }],
        },
        PeerReviewWire {
            schema: "conductor/plan-peer-review@1".to_string(),
            verdict: crate::run::PeerVerdict::Revise,
            findings: vec![PlanFinding {
                id: "finding-high".to_string(),
                severity: "high".to_string(),
                location: "src/example.rs".to_string(),
                problem: "The plan omits an observable contract.".to_string(),
                required_change: "Add the missing contract.".to_string(),
            }],
        },
        PeerReviewWire {
            schema: "conductor/plan-peer-review@1".to_string(),
            verdict: crate::run::PeerVerdict::Revise,
            findings: vec![PlanFinding {
                id: "finding-critical".to_string(),
                severity: "critical".to_string(),
                location: "src/example.rs".to_string(),
                problem: "The plan omits an observable contract.".to_string(),
                required_change: "Add the missing contract.".to_string(),
            }],
        },
    ])
    .map_err(|error| format!("failed to serialize plan peer-review prompt contract: {error}"))
}

/// Returns checked final-opinion examples spanning both permitted verdicts.
pub(crate) fn second_opinion_prompt_contract() -> Result<String, String> {
    serde_json::to_string(&[
        SecondOpinionWire {
            schema: "conductor/plan-second-opinion@1".to_string(),
            verdict: crate::run::SecondOpinionVerdict::Accept,
        },
        SecondOpinionWire {
            schema: "conductor/plan-second-opinion@1".to_string(),
            verdict: crate::run::SecondOpinionVerdict::Reject,
        },
    ])
    .map_err(|error| format!("failed to serialize plan second-opinion prompt contract: {error}"))
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
        chrono::Utc::now().format("%Y%m%dT%H%M%S%9f"),
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
    let require_second_opinion =
        request.output_kind == PlanOutputKind::Spec || request.require_second_opinion;
    if request.max_plan_revisions > config.budgets.max_plan_revisions {
        return Err(format!(
            "plan revision request {} exceeds configured maximum {}",
            request.max_plan_revisions, config.budgets.max_plan_revisions
        ));
    }
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
    reconcile_plan_reservations(&router, &paths.state_dir)?;
    let role = crate::role_routing::RoleId::new("plan")
        .map_err(|error| format!("planner role: {error}"))?;
    router
        .validate_preapproval_contingencies(&role, &constraints, require_second_opinion)
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
        require_second_opinion,
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
        stage_attempts: crate::run::PlanStageAttempts::default(),
        revision_limit,
        stage_attempt_limit,
    };
    let mut created_run = None;
    let result = (|| -> Result<PathBuf, String> {
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
        created_run = Some(handle);
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
        Ok(report_path)
    })();
    match result {
        Ok(report_path) => Ok(PreparedPlan {
            run_id,
            report_path,
        }),
        Err(error) => {
            let mut cleanup_errors = Vec::new();
            if let Err(cleanup) = router.cancel(&prepared.reservation) {
                cleanup_errors.push(format!("planner reservation cleanup: {cleanup}"));
            }
            if let Some(handle) = created_run {
                if let Err(cleanup) = std::fs::remove_dir_all(handle.dir()) {
                    cleanup_errors.push(format!("plan run cleanup: {cleanup}"));
                }
            }
            if cleanup_errors.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", cleanup_errors.join("; ")))
            }
        }
    }
}

/// Reconciles reservation capacity against fully readable plan manifests before
/// any new planner or reviewer selection. Planner reservations stay pending
/// until approval; a reviewer binding already persisted in a manifest is
/// promoted to committed so a crash cannot consume capacity indefinitely.
fn reconcile_plan_reservations(
    router: &crate::role_routing::RoleRouter,
    state_dir: &Path,
) -> Result<(), String> {
    let role = crate::role_routing::RoleId::new("plan").map_err(|error| error.to_string())?;
    let mut planner_links = Vec::new();
    let mut peer_links = Vec::new();
    let mut second_links = Vec::new();
    let entries = match std::fs::read_dir(crate::run::runs_dir(state_dir)) {
        Ok(entries) => entries,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(format!("plan reservation reconciliation: {error}")),
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("plan reservation reconciliation: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("plan reservation reconciliation: {error}"))?
            .is_dir()
        {
            continue;
        }
        let Some(run_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(run) = crate::run::RunHandle::open(state_dir, &run_id) else {
            continue;
        };
        let Ok(plan) = run.plan() else {
            continue;
        };
        planner_links.push(
            crate::role_routing::RunId::new(run.run_id().to_string())
                .map_err(|error| format!("plan reservation reconciliation: {error}"))?,
        );
        if let crate::run::PlanProgress::AwaitingPeer {
            peer: Some(_),
            peer_binding_run_id,
            ..
        } = &plan.progress
        {
            peer_links.push(
                peer_binding_run_id
                    .as_deref()
                    .map(|binding| {
                        crate::role_routing::RunId::new(binding.to_string())
                            .map_err(|error| format!("plan reservation reconciliation: {error}"))
                    })
                    .transpose()?
                    .unwrap_or(reviewer_binding_run_id(
                        run.run_id(),
                        crate::role_routing::PlanStage::PeerReview,
                    )?),
            );
        }
        if let crate::run::PlanProgress::AwaitingSecondOpinion {
            second: Some(_),
            second_binding_run_id,
            ..
        } = &plan.progress
        {
            second_links.push(
                second_binding_run_id
                    .as_deref()
                    .map(|binding| {
                        crate::role_routing::RunId::new(binding.to_string())
                            .map_err(|error| format!("plan reservation reconciliation: {error}"))
                    })
                    .transpose()?
                    .unwrap_or(reviewer_binding_run_id(
                        run.run_id(),
                        crate::role_routing::PlanStage::SecondOpinion,
                    )?),
            );
        }
    }
    reconcile_reservation_stage(
        router,
        &role,
        crate::role_routing::PlanStage::Planner,
        &planner_links,
        false,
    )?;
    reconcile_reservation_stage(
        router,
        &role,
        crate::role_routing::PlanStage::PeerReview,
        &peer_links,
        true,
    )?;
    reconcile_reservation_stage(
        router,
        &role,
        crate::role_routing::PlanStage::SecondOpinion,
        &second_links,
        true,
    )
}

fn reconcile_reservation_stage(
    router: &crate::role_routing::RoleRouter,
    role: &crate::role_routing::RoleId,
    stage: crate::role_routing::PlanStage,
    linked: &[crate::role_routing::RunId],
    commit_bound: bool,
) -> Result<(), String> {
    router
        .reconcile_orphans(role, stage, linked)
        .map_err(|error| format!("plan reservation reconciliation: {error}"))?;
    if !commit_bound {
        return Ok(());
    }
    for run_id in linked {
        let Some(reservation) = router
            .reservation(role, stage, run_id)
            .map_err(|error| format!("plan reservation reconciliation: {error}"))?
        else {
            continue;
        };
        if matches!(
            reservation.state,
            crate::role_routing::ReservationState::PendingApproval
        ) {
            router
                .commit(&reservation)
                .map_err(|error| format!("plan reviewer reservation reconciliation: {error}"))?;
        }
    }
    Ok(())
}

fn reviewer_binding_run_id(
    run_id: &str,
    stage: crate::role_routing::PlanStage,
) -> Result<crate::role_routing::RunId, String> {
    let stage = match stage {
        crate::role_routing::PlanStage::Planner => {
            return Err("planner has no delayed reviewer binding".to_string());
        }
        crate::role_routing::PlanStage::PeerReview => "peer-review",
        crate::role_routing::PlanStage::SecondOpinion => "second-opinion",
    };
    crate::role_routing::RunId::new(format!("{run_id}-{stage}-bind"))
        .map_err(|error| format!("reviewer binding run id: {error}"))
}

/// Chooses the first unused durable generation for a reviewer binding. A
/// canceled generation is historical score evidence, never a binding that may
/// be revived; a pending or committed generation is retained for its matching
/// manifest/recovery path.
fn next_reviewer_binding_run_id(
    router: &crate::role_routing::RoleRouter,
    role: &crate::role_routing::RoleId,
    run_id: &str,
    stage: crate::role_routing::PlanStage,
) -> Result<crate::role_routing::RunId, String> {
    let base = reviewer_binding_run_id(run_id, stage)?;
    let mut generation = 0_u64;
    loop {
        let candidate = if generation == 0 {
            base.clone()
        } else {
            crate::role_routing::RunId::new(format!("{}-retry-{generation}", base.as_str()))
                .map_err(|error| format!("reviewer binding retry run id: {error}"))?
        };
        match router
            .reservation(role, stage, &candidate)
            .map_err(|error| format!("reviewer binding retry lookup: {error}"))?
        {
            Some(crate::role_routing::Reservation {
                state: crate::role_routing::ReservationState::Canceled,
                ..
            }) => {
                generation = generation
                    .checked_add(1)
                    .ok_or_else(|| "reviewer binding retry generation overflow".to_string())?;
            }
            _ => return Ok(candidate),
        }
    }
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

/// Validates the shipped plan policy against one exact Bursar v2 snapshot
/// without preparing a run, allocating a scheduler turn, or making a model
/// call. The initial pool must support every possible author as a spec author:
/// a provider-distinct peer and a third, pairwise-distinct final opinion.
pub(crate) fn validate_initial_policy(
    config: &crate::config::Config,
    snapshot: &crate::bursar::RosterSnapshot,
) -> Result<(), String> {
    let policy = crate::role_routing::RoutingPolicy::from_config(config, snapshot)
        .map_err(|error| format!("role policy: {error}"))?;
    let role = crate::role_routing::RoleId::new("plan")
        .map_err(|error| format!("planner role: {error}"))?;
    let constraints = planner_constraints(snapshot, crate::config::Tier::Lead)?;
    crate::role_routing::validate_preapproval_contingencies_for_snapshot(
        &policy,
        snapshot,
        &role,
        &constraints,
        true,
    )
    .map_err(|error| format!("plan team contingency: {error}"))?;
    crate::run::RevisionLimit::new(config.budgets.max_plan_revisions)
        .map_err(|error| format!("plan revision policy: {error}"))?;
    crate::run::StageAttemptLimit::new(2)
        .map_err(|error| format!("plan stage-attempt policy: {error}"))?;
    Ok(())
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
            crate::role_routing::RoleId::new("plan").map_err(|error| error.to_string())?
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
    let planner_candidates = std::iter::once(prepared.selected.clone())
        .chain(prepared.fallbacks.iter().cloned())
        .collect::<Vec<_>>();
    crate::run::PlanRoutes {
        stages: vec![
            crate::run::PlanStageRoute {
                stage: crate::run::PlanStage::Planner,
                capability_role: "plan".to_string(),
                candidates: planner_candidates,
                provider_distinct_from: Vec::new(),
                constraints: crate::run::PlanStageConstraints::unconstrained(),
            },
            crate::run::PlanStageRoute {
                stage: crate::run::PlanStage::PeerReview,
                capability_role: "plan".to_string(),
                candidates: candidates.clone(),
                provider_distinct_from: Vec::new(),
                constraints: crate::run::PlanStageConstraints {
                    distinct_execution_from: vec![crate::run::PlanStage::Planner],
                    tier_at_least: vec![crate::run::PlanStage::Planner],
                    provider_diversity: crate::run::PlanProviderDiversity::CrossProviderOrDegraded,
                },
            },
            crate::run::PlanStageRoute {
                stage: crate::run::PlanStage::SecondOpinion,
                capability_role: "plan".to_string(),
                candidates,
                provider_distinct_from: vec![
                    crate::run::PlanStage::Planner,
                    crate::run::PlanStage::PeerReview,
                ],
                constraints: crate::run::PlanStageConstraints {
                    distinct_execution_from: vec![
                        crate::run::PlanStage::Planner,
                        crate::run::PlanStage::PeerReview,
                    ],
                    tier_at_least: vec![crate::run::PlanStage::Planner],
                    provider_diversity: crate::run::PlanProviderDiversity::PairwiseDistinct,
                },
            },
        ],
    }
}

/// Terminalizes an active stage once its immutable call budget has already
/// been consumed. This protects resume after a crash between an attempt
/// checkpoint and its terminal failure checkpoint.
fn finish_exhausted_stage(
    run: &mut crate::run::RunHandle,
    stage: crate::run::PlanStage,
) -> Result<bool, String> {
    let plan = run.plan().map_err(|error| error.to_string())?;
    let attempts = match stage {
        crate::run::PlanStage::Planner => plan.stage_attempts.planner,
        crate::run::PlanStage::PeerReview => plan.stage_attempts.peer_review,
        crate::run::PlanStage::SecondOpinion => plan.stage_attempts.second_opinion,
    };
    if attempts < plan.stage_attempt_limit.value() {
        return Ok(false);
    }
    run.finish_plan_blocked()
        .map_err(|error| format!("plan blocked checkpoint: {error}"))?;
    Ok(true)
}

/// Checks every no-call precondition before persisting the crash/resume
/// invocation boundary. If an already-persisted boundary is at capacity,
/// terminalize it rather than propagating a resumable limit error.
#[expect(
    clippy::too_many_arguments,
    reason = "the durable invocation boundary needs exact review evidence"
)]
fn start_plan_stage_invocation<C: crate::bursar::BursarClient + ?Sized>(
    run: &mut crate::run::RunHandle,
    bursar: &C,
    snapshot: &crate::bursar::RosterSnapshot,
    ledger_path: &Path,
    stage: crate::run::PlanStage,
    execution: &crate::run::ApprovedExecution,
    profile: &crate::bursar::RosterProfile,
    input: &[u8],
) -> Result<u8, String> {
    recheck_author(bursar, snapshot, execution)?;
    if finish_exhausted_stage(run, stage)? {
        return Err(format!(
            "plan {} attempt limit exhausted",
            plan_stage_label(stage)
        ));
    }
    let attempt = match run.record_plan_stage_attempt(stage) {
        Ok(attempt) => attempt,
        Err(error) => {
            if finish_exhausted_stage(run, stage)? {
                return Err(format!(
                    "plan {} attempt limit exhausted: {error}",
                    plan_stage_label(stage)
                ));
            }
            return Err(format!(
                "plan {} attempt: {error}",
                plan_stage_label(stage)
            ));
        }
    };
    if let Err(error) = append_plan_invocation(
        run,
        ledger_path,
        crate::run::EventKind::AttemptStarted,
        stage,
        execution,
        profile,
        input,
        None,
        attempt,
        "started",
    ) {
        let _ = finish_exhausted_stage(run, stage)?;
        return Err(error);
    }
    Ok(attempt)
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
        captured_snapshot.clone(),
    )
    .map_err(|error| format!("role router: {error}"))?;
    let _run_guard = router
        .acquire_run_transition(
            &crate::role_routing::RunId::new(run_id.to_string())
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("plan run guard: {error}"))?;
    reconcile_plan_reservations(&router, &paths.state_dir)?;
    if matches!(
        run.plan().map_err(|error| error.to_string())?.progress,
        crate::run::PlanProgress::AwaitingPeer { .. }
            | crate::run::PlanProgress::Revising { .. }
            | crate::run::PlanProgress::AwaitingSecondOpinion { .. }
    ) {
        return dispatch_review_stages(
            &mut run,
            &router,
            bursar,
            &paths.ledger_path,
            author,
            &captured_snapshot,
            &repo,
            &approval,
        );
    }
    if finish_exhausted_stage(&mut run, crate::run::PlanStage::Planner)? {
        return Err("plan author attempt limit exhausted".to_string());
    }
    let planner_candidates = planner_candidates(run.plan().map_err(|error| error.to_string())?)?;
    let selected = match run
        .plan()
        .map_err(|error| error.to_string())?
        .progress
        .clone()
    {
        crate::run::PlanProgress::Prepared => {
            let selected =
                match available_planner(bursar, &captured_snapshot, &planner_candidates, None) {
                    Ok(selected) => selected,
                    Err(error) => {
                        run.block_plan_before_authoring()
                            .map_err(|error| format!("plan blocked checkpoint: {error}"))?;
                        return Err(error);
                    }
                };
            router
                .commit(&approval.reservation)
                .map_err(|error| format!("planner reservation: {error}"))?;
            run.start_plan_authoring(selected.clone())
                .map_err(|error| format!("plan author checkpoint: {error}"))?;
            selected
        }
        crate::run::PlanProgress::Authoring { author, .. } => {
            if !planner_candidates.contains(&author) {
                return Err(
                    "persisted plan author is outside immutable approved planner route".to_string(),
                );
            }
            match recheck_author(bursar, &captured_snapshot, &author) {
                Ok(()) => author,
                Err(current_error) => {
                    match available_planner(
                        bursar,
                        &captured_snapshot,
                        &planner_candidates,
                        Some(&author),
                    ) {
                        Ok(replacement) => {
                            run.replace_plan_author_before_artifact(replacement.clone())
                                .map_err(|error| format!("plan fallback checkpoint: {error}"))?;
                            replacement
                        }
                        Err(fallback_error) => {
                            run.finish_plan_blocked()
                                .map_err(|error| format!("plan blocked checkpoint: {error}"))?;
                            return Err(format!(
                                "approved plan author is unavailable ({current_error}); no approved planner fallback is eligible ({fallback_error})"
                            ));
                        }
                    }
                }
            }
        }
        crate::run::PlanProgress::Blocked { .. } => {
            return Err(
                "plan is blocked before authoring; cancel or prepare a new authorization"
                    .to_string(),
            );
        }
        _ => return Err("plan dispatch state is not resumable authoring".to_string()),
    };
    let profile = profile_for_execution(&captured_snapshot, &selected)?;
    run.record_plan_author_attempt()
        .map_err(|error| format!("plan author attempt: {error}"))?;
    let author_attempt = run
        .plan()
        .map_err(|error| error.to_string())?
        .stage_attempts
        .planner;
    append_plan_invocation(
        &mut run,
        &paths.ledger_path,
        crate::run::EventKind::AttemptStarted,
        crate::run::PlanStage::Planner,
        &selected,
        &profile,
        &input_bytes,
        None,
        author_attempt,
        "started",
    )?;
    let output = match with_isolated_worktree(&repo, &approval.target_head, |worktree| {
        author.author(&PlanAuthorRequest {
            worktree: worktree.to_path_buf(),
            input: input_bytes.clone(),
            output_kind: approval.output_kind,
            execution: selected.clone(),
            profile: profile.clone(),
        })
    }) {
        Ok(output) => output,
        Err(error) => {
            append_plan_invocation(
                &mut run,
                &paths.ledger_path,
                crate::run::EventKind::AttemptFinished,
                crate::run::PlanStage::Planner,
                &selected,
                &profile,
                &input_bytes,
                None,
                author_attempt,
                "failed",
            )?;
            finish_exhausted_stage(&mut run, crate::run::PlanStage::Planner)?;
            return Err(error);
        }
    };
    let document = match parse_document(approval.output_kind, &output) {
        Ok(document) => document,
        Err(first_error) => {
            append_plan_invocation(
                &mut run,
                &paths.ledger_path,
                crate::run::EventKind::AttemptFinished,
                crate::run::PlanStage::Planner,
                &selected,
                &profile,
                &input_bytes,
                Some(&output),
                author_attempt,
                "malformed",
            )?;
            run.record_plan_author_attempt()
                .map_err(|error| format!("plan author repair attempt: {error}"))?;
            let repair_attempt = run
                .plan()
                .map_err(|error| error.to_string())?
                .stage_attempts
                .planner;
            append_plan_invocation(
                &mut run,
                &paths.ledger_path,
                crate::run::EventKind::AttemptStarted,
                crate::run::PlanStage::Planner,
                &selected,
                &profile,
                &input_bytes,
                None,
                repair_attempt,
                "started",
            )?;
            let repair = match with_isolated_worktree(&repo, &approval.target_head, |worktree| {
                author.author(&PlanAuthorRequest {
                    worktree: worktree.to_path_buf(),
                    input: input_bytes.clone(),
                    output_kind: approval.output_kind,
                    execution: selected.clone(),
                    profile: profile.clone(),
                })
            }) {
                Ok(output) => output,
                Err(error) => {
                    append_plan_invocation(
                        &mut run,
                        &paths.ledger_path,
                        crate::run::EventKind::AttemptFinished,
                        crate::run::PlanStage::Planner,
                        &selected,
                        &profile,
                        &input_bytes,
                        None,
                        repair_attempt,
                        "failed",
                    )?;
                    finish_exhausted_stage(&mut run, crate::run::PlanStage::Planner)?;
                    return Err(error);
                }
            };
            match parse_document(approval.output_kind, &repair) {
                Ok(document) => document,
                Err(second_error) => {
                    append_plan_invocation(
                        &mut run,
                        &paths.ledger_path,
                        crate::run::EventKind::AttemptFinished,
                        crate::run::PlanStage::Planner,
                        &selected,
                        &profile,
                        &input_bytes,
                        Some(&repair),
                        repair_attempt,
                        "malformed",
                    )?;
                    finish_exhausted_stage(&mut run, crate::run::PlanStage::Planner)?;
                    return Err(format!(
                        "plan author output invalid after repair: {first_error}; {second_error}"
                    ));
                }
            }
        }
    };
    let canonical = canonical_document_json(&document)?;
    let author_attempt = run
        .plan()
        .map_err(|error| error.to_string())?
        .stage_attempts
        .planner;
    append_plan_invocation(
        &mut run,
        &paths.ledger_path,
        crate::run::EventKind::AttemptFinished,
        crate::run::PlanStage::Planner,
        &selected,
        &profile,
        &input_bytes,
        Some(&canonical),
        author_attempt,
        "returned",
    )?;
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
            .map_err(|error| format!("plan peer checkpoint: {error}"))?;
        dispatch_review_stages(
            &mut run,
            &router,
            bursar,
            &paths.ledger_path,
            author,
            &captured_snapshot,
            &repo,
            &approval,
        )
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "each persisted plan review stage keeps its binding, provider, and artifact gates explicit"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the immutable plan execution boundary carries every authorization and evidence source"
)]
fn dispatch_review_stages<C, A>(
    run: &mut crate::run::RunHandle,
    router: &crate::role_routing::RoleRouter,
    bursar: &C,
    ledger_path: &Path,
    executor: &A,
    snapshot: &crate::bursar::RosterSnapshot,
    repo: &Path,
    approval: &PlanApproval,
) -> Result<(), String>
where
    C: crate::bursar::BursarClient + ?Sized,
    A: PlanAuthor + ?Sized,
{
    loop {
        match run
            .plan()
            .map_err(|error| error.to_string())?
            .progress
            .clone()
        {
            crate::run::PlanProgress::Terminal { .. } => return Ok(()),
            crate::run::PlanProgress::AwaitingPeer {
                author,
                peer,
                artifact,
                ..
            } => {
                if finish_exhausted_stage(run, crate::run::PlanStage::PeerReview)? {
                    return Err("plan peer-review attempt limit exhausted".to_string());
                }
                let (reviewer, degraded) = match peer {
                    Some(bound) => (bound, false),
                    None => match select_reviewer(
                        run,
                        router,
                        bursar,
                        snapshot,
                        crate::run::PlanStage::PeerReview,
                        &author,
                        None,
                    ) {
                        Ok(bound) => bound,
                        Err(error) => {
                            run.finish_plan_blocked().map_err(|checkpoint| {
                                format!(
                                    "plan blocked after peer candidate exhaustion: {checkpoint}"
                                )
                            })?;
                            return Err(error);
                        }
                    },
                };
                let profile = profile_for_execution(snapshot, &reviewer)?;
                let canonical_plan = plan_artifact_bytes(run, &artifact)?;
                let attempt = start_plan_stage_invocation(
                    run,
                    bursar,
                    snapshot,
                    ledger_path,
                    crate::run::PlanStage::PeerReview,
                    &reviewer,
                    &profile,
                    &canonical_plan,
                )?;
                let first = match with_isolated_worktree(repo, &approval.target_head, |worktree| {
                    executor.peer_review(&PlanPeerReviewRequest {
                        worktree: worktree.to_path_buf(),
                        target: run
                            .plan()
                            .map_err(|error| error.to_string())?
                            .target
                            .clone(),
                        rubric: peer_rubric(approval.output_kind),
                        canonical_plan: canonical_plan.clone(),
                        execution: reviewer.clone(),
                        profile: profile.clone(),
                    })
                }) {
                    Ok(output) => output,
                    Err(error) => {
                        append_plan_invocation(
                            run,
                            ledger_path,
                            crate::run::EventKind::AttemptFinished,
                            crate::run::PlanStage::PeerReview,
                            &reviewer,
                            &profile,
                            &canonical_plan,
                            None,
                            attempt,
                            "failed",
                        )?;
                        finish_exhausted_stage(run, crate::run::PlanStage::PeerReview)?;
                        return Err(error);
                    }
                };
                let (review_bytes, verdict, _findings, final_attempt) = match parse_peer_review(
                    &first,
                ) {
                    Ok((verdict, findings)) => (first, verdict, findings, attempt),
                    Err(first_error) => {
                        append_plan_invocation(
                            run,
                            ledger_path,
                            crate::run::EventKind::AttemptFinished,
                            crate::run::PlanStage::PeerReview,
                            &reviewer,
                            &profile,
                            &canonical_plan,
                            Some(&first),
                            attempt,
                            "malformed",
                        )?;
                        let repair_attempt = start_plan_stage_invocation(
                            run,
                            bursar,
                            snapshot,
                            ledger_path,
                            crate::run::PlanStage::PeerReview,
                            &reviewer,
                            &profile,
                            &canonical_plan,
                        )?;
                        let repair =
                            match with_isolated_worktree(repo, &approval.target_head, |worktree| {
                                executor.peer_review(&PlanPeerReviewRequest {
                                    worktree: worktree.to_path_buf(),
                                    target: run
                                        .plan()
                                        .map_err(|error| error.to_string())?
                                        .target
                                        .clone(),
                                    rubric: peer_rubric(approval.output_kind),
                                    canonical_plan: canonical_plan.clone(),
                                    execution: reviewer.clone(),
                                    profile: profile.clone(),
                                })
                            }) {
                                Ok(output) => output,
                                Err(error) => {
                                    append_plan_invocation(
                                        run,
                                        ledger_path,
                                        crate::run::EventKind::AttemptFinished,
                                        crate::run::PlanStage::PeerReview,
                                        &reviewer,
                                        &profile,
                                        &canonical_plan,
                                        None,
                                        repair_attempt,
                                        "failed",
                                    )?;
                                    finish_exhausted_stage(run, crate::run::PlanStage::PeerReview)?;
                                    return Err(error);
                                }
                            };
                        let (verdict, findings) = match parse_peer_review(&repair) {
                            Ok(parsed) => parsed,
                            Err(second_error) => {
                                append_plan_invocation(
                                    run,
                                    ledger_path,
                                    crate::run::EventKind::AttemptFinished,
                                    crate::run::PlanStage::PeerReview,
                                    &reviewer,
                                    &profile,
                                    &canonical_plan,
                                    Some(&repair),
                                    repair_attempt,
                                    "malformed",
                                )?;
                                finish_exhausted_stage(run, crate::run::PlanStage::PeerReview)?;
                                return Err(format!(
                                    "plan peer output invalid after repair: {first_error}; {second_error}"
                                ));
                            }
                        };
                        (repair, verdict, findings, repair_attempt)
                    }
                };
                let evidence_input = plan_artifact_bytes(run, &artifact)?;
                append_plan_invocation(
                    run,
                    ledger_path,
                    crate::run::EventKind::AttemptFinished,
                    crate::run::PlanStage::PeerReview,
                    &reviewer,
                    &profile,
                    &evidence_input,
                    Some(&review_bytes),
                    final_attempt,
                    "returned",
                )?;
                run.capture_plan_artifact(
                    Path::new(&format!("artifacts/peer-review-{final_attempt}.json")),
                    &review_bytes,
                )
                .map_err(|error| format!("plan peer artifact: {error}"))?;
                if degraded {
                    run.append_event(
                        crate::run::EventKind::CoverageGap,
                        crate::run::EventInput {
                            profile_id: Some(reviewer.profile_id.clone()),
                            outcome: Some("peer_provider_diversity_degraded".to_string()),
                            ..crate::run::EventInput::default()
                        },
                    )
                    .map_err(|error| format!("plan peer degradation event: {error}"))?;
                }
                run.record_plan_peer_verdict(reviewer, verdict, approval.require_second_opinion)
                    .map_err(|error| format!("plan peer verdict checkpoint: {error}"))?;
            }
            crate::run::PlanProgress::Revising {
                author,
                peer: _,
                artifact,
                ..
            } => {
                if finish_exhausted_stage(run, crate::run::PlanStage::Planner)? {
                    return Err("plan revision attempt limit exhausted".to_string());
                }
                let profile = profile_for_execution(snapshot, &author)?;
                let prior_plan = plan_artifact_bytes(run, &artifact)?;
                let peer_attempt = run
                    .plan()
                    .map_err(|error| error.to_string())?
                    .stage_attempts
                    .peer_review;
                let peer_bytes = std::fs::read(
                    run.dir()
                        .join(format!("artifacts/peer-review-{peer_attempt}.json")),
                )
                .map_err(|error| format!("captured plan peer findings: {error}"))?;
                let (verdict, findings) = parse_peer_review(&peer_bytes)?;
                if !matches!(verdict, crate::run::PeerVerdict::Revise) {
                    return Err("revision state has no persisted revise findings".to_string());
                }
                let attempt = start_plan_stage_invocation(
                    run,
                    bursar,
                    snapshot,
                    ledger_path,
                    crate::run::PlanStage::Planner,
                    &author,
                    &profile,
                    &prior_plan,
                )?;
                let first = match with_isolated_worktree(repo, &approval.target_head, |worktree| {
                    executor.revise(&PlanRevisionRequest {
                        worktree: worktree.to_path_buf(),
                        prior_plan: prior_plan.clone(),
                        findings: findings.clone(),
                        output_kind: approval.output_kind,
                        execution: author.clone(),
                        profile: profile.clone(),
                    })
                }) {
                    Ok(output) => output,
                    Err(error) => {
                        append_plan_invocation(
                            run,
                            ledger_path,
                            crate::run::EventKind::AttemptFinished,
                            crate::run::PlanStage::Planner,
                            &author,
                            &profile,
                            &prior_plan,
                            None,
                            attempt,
                            "failed",
                        )?;
                        finish_exhausted_stage(run, crate::run::PlanStage::Planner)?;
                        return Err(error);
                    }
                };
                let (document, final_attempt) = match parse_document(approval.output_kind, &first) {
                    Ok(document) => (document, attempt),
                    Err(first_error) => {
                        append_plan_invocation(
                            run,
                            ledger_path,
                            crate::run::EventKind::AttemptFinished,
                            crate::run::PlanStage::Planner,
                            &author,
                            &profile,
                            &prior_plan,
                            Some(&first),
                            attempt,
                            "malformed",
                        )?;
                        let repair_attempt = start_plan_stage_invocation(
                            run,
                            bursar,
                            snapshot,
                            ledger_path,
                            crate::run::PlanStage::Planner,
                            &author,
                            &profile,
                            &prior_plan,
                        )?;
                        let repair =
                            match with_isolated_worktree(repo, &approval.target_head, |worktree| {
                                executor.revise(&PlanRevisionRequest {
                                    worktree: worktree.to_path_buf(),
                                    prior_plan: prior_plan.clone(),
                                    findings: findings.clone(),
                                    output_kind: approval.output_kind,
                                    execution: author.clone(),
                                    profile: profile.clone(),
                                })
                            }) {
                                Ok(output) => output,
                                Err(error) => {
                                    append_plan_invocation(
                                        run,
                                        ledger_path,
                                        crate::run::EventKind::AttemptFinished,
                                        crate::run::PlanStage::Planner,
                                        &author,
                                        &profile,
                                        &prior_plan,
                                        None,
                                        repair_attempt,
                                        "failed",
                                    )?;
                                    finish_exhausted_stage(run, crate::run::PlanStage::Planner)?;
                                    return Err(error);
                                }
                            };
                        let document = match parse_document(approval.output_kind, &repair) {
                            Ok(document) => document,
                            Err(second_error) => {
                                append_plan_invocation(
                                    run,
                                    ledger_path,
                                    crate::run::EventKind::AttemptFinished,
                                    crate::run::PlanStage::Planner,
                                    &author,
                                    &profile,
                                    &prior_plan,
                                    Some(&repair),
                                    repair_attempt,
                                    "malformed",
                                )?;
                                finish_exhausted_stage(run, crate::run::PlanStage::Planner)?;
                                return Err(format!(
                                    "plan revision output invalid after repair: {first_error}; {second_error}"
                                ));
                            }
                        };
                        (document, repair_attempt)
                    }
                };
                let canonical = canonical_document_json(&document)?;
                let evidence_input = plan_artifact_bytes(run, &artifact)?;
                append_plan_invocation(
                    run,
                    ledger_path,
                    crate::run::EventKind::AttemptFinished,
                    crate::run::PlanStage::Planner,
                    &author,
                    &profile,
                    &evidence_input,
                    Some(&canonical),
                    final_attempt,
                    "returned",
                )?;
                let json = run
                    .capture_plan_artifact(
                        Path::new(&format!("artifacts/plan-revision-{final_attempt}.json")),
                        &canonical,
                    )
                    .map_err(|error| format!("plan revision JSON artifact: {error}"))?;
                run.capture_plan_artifact(
                    Path::new(&format!("artifacts/plan-revision-{final_attempt}.md")),
                    render_markdown(&document).as_bytes(),
                )
                .map_err(|error| format!("plan revision Markdown artifact: {error}"))?;
                let (head, status) = git_identity(repo)?;
                if head != approval.target_head || status != approval.target_status {
                    return Err(
                        "plan revision changed target repository HEAD or status".to_string()
                    );
                }
                run.complete_plan_revision(json)
                    .map_err(|error| format!("plan revision checkpoint: {error}"))?;
            }
            crate::run::PlanProgress::AwaitingSecondOpinion {
                author,
                peer,
                second,
                artifact,
                ..
            } => {
                if finish_exhausted_stage(run, crate::run::PlanStage::SecondOpinion)? {
                    return Err("plan second-opinion attempt limit exhausted".to_string());
                }
                let reviewer = if let Some(bound) = second {
                    *bound
                } else {
                    let (bound, _) = select_reviewer(
                        run,
                        router,
                        bursar,
                        snapshot,
                        crate::run::PlanStage::SecondOpinion,
                        &author,
                        Some(&peer),
                    )
                    .map_err(|error| {
                        let _ = run.finish_plan_blocked();
                        format!("plan blocked after second-opinion candidate exhaustion: {error}")
                    })?;
                    bound
                };
                let profile = profile_for_execution(snapshot, &reviewer)?;
                let canonical_plan = plan_artifact_bytes(run, &artifact)?;
                let attempt = start_plan_stage_invocation(
                    run,
                    bursar,
                    snapshot,
                    ledger_path,
                    crate::run::PlanStage::SecondOpinion,
                    &reviewer,
                    &profile,
                    &canonical_plan,
                )?;
                let first = match with_isolated_worktree(repo, &approval.target_head, |worktree| {
                    executor.second_opinion(&PlanSecondOpinionRequest {
                        worktree: worktree.to_path_buf(),
                        target: run
                            .plan()
                            .map_err(|error| error.to_string())?
                            .target
                            .clone(),
                        canonical_plan: canonical_plan.clone(),
                        execution: reviewer.clone(),
                        profile: profile.clone(),
                    })
                }) {
                    Ok(output) => output,
                    Err(error) => {
                        append_plan_invocation(
                            run,
                            ledger_path,
                            crate::run::EventKind::AttemptFinished,
                            crate::run::PlanStage::SecondOpinion,
                            &reviewer,
                            &profile,
                            &canonical_plan,
                            None,
                            attempt,
                            "failed",
                        )?;
                        finish_exhausted_stage(run, crate::run::PlanStage::SecondOpinion)?;
                        return Err(error);
                    }
                };
                let (opinion_bytes, verdict, final_attempt) = match parse_second_opinion(&first) {
                    Ok(verdict) => (first, verdict, attempt),
                    Err(first_error) => {
                        append_plan_invocation(
                            run,
                            ledger_path,
                            crate::run::EventKind::AttemptFinished,
                            crate::run::PlanStage::SecondOpinion,
                            &reviewer,
                            &profile,
                            &canonical_plan,
                            Some(&first),
                            attempt,
                            "malformed",
                        )?;
                        let repair_attempt = start_plan_stage_invocation(
                            run,
                            bursar,
                            snapshot,
                            ledger_path,
                            crate::run::PlanStage::SecondOpinion,
                            &reviewer,
                            &profile,
                            &canonical_plan,
                        )?;
                        let repair =
                            match with_isolated_worktree(repo, &approval.target_head, |worktree| {
                                executor.second_opinion(&PlanSecondOpinionRequest {
                                    worktree: worktree.to_path_buf(),
                                    target: run
                                        .plan()
                                        .map_err(|error| error.to_string())?
                                        .target
                                        .clone(),
                                    canonical_plan: canonical_plan.clone(),
                                    execution: reviewer.clone(),
                                    profile: profile.clone(),
                                })
                            }) {
                                Ok(output) => output,
                                Err(error) => {
                                    append_plan_invocation(
                                        run,
                                        ledger_path,
                                        crate::run::EventKind::AttemptFinished,
                                        crate::run::PlanStage::SecondOpinion,
                                        &reviewer,
                                        &profile,
                                        &canonical_plan,
                                        None,
                                        repair_attempt,
                                        "failed",
                                    )?;
                                    finish_exhausted_stage(
                                        run,
                                        crate::run::PlanStage::SecondOpinion,
                                    )?;
                                    return Err(error);
                                }
                            };
                        let verdict = match parse_second_opinion(&repair) {
                            Ok(verdict) => verdict,
                            Err(second_error) => {
                                append_plan_invocation(
                                    run,
                                    ledger_path,
                                    crate::run::EventKind::AttemptFinished,
                                    crate::run::PlanStage::SecondOpinion,
                                    &reviewer,
                                    &profile,
                                    &canonical_plan,
                                    Some(&repair),
                                    repair_attempt,
                                    "malformed",
                                )?;
                                finish_exhausted_stage(run, crate::run::PlanStage::SecondOpinion)?;
                                return Err(format!(
                                    "plan second-opinion output invalid after repair: {first_error}; {second_error}"
                                ));
                            }
                        };
                        (repair, verdict, repair_attempt)
                    }
                };
                let evidence_input = plan_artifact_bytes(run, &artifact)?;
                append_plan_invocation(
                    run,
                    ledger_path,
                    crate::run::EventKind::AttemptFinished,
                    crate::run::PlanStage::SecondOpinion,
                    &reviewer,
                    &profile,
                    &evidence_input,
                    Some(&opinion_bytes),
                    final_attempt,
                    "returned",
                )?;
                run.capture_plan_artifact(
                    Path::new(&format!("artifacts/second-opinion-{final_attempt}.json")),
                    &opinion_bytes,
                )
                .map_err(|error| format!("plan second-opinion artifact: {error}"))?;
                run.record_plan_second_opinion(reviewer, verdict)
                    .map_err(|error| format!("plan second-opinion checkpoint: {error}"))?;
            }
            crate::run::PlanProgress::Prepared
            | crate::run::PlanProgress::Authoring { .. }
            | crate::run::PlanProgress::Blocked { .. } => {
                return Err("plan review stages require a completed author artifact".to_string());
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the append boundary records the complete fixed invocation evidence tuple"
)]
fn append_plan_invocation(
    run: &mut crate::run::RunHandle,
    ledger_path: &Path,
    kind: crate::run::EventKind,
    stage: crate::run::PlanStage,
    execution: &crate::run::ApprovedExecution,
    profile: &crate::bursar::RosterProfile,
    input: &[u8],
    output: Option<&[u8]>,
    attempt: u8,
    outcome: &str,
) -> Result<(), String> {
    let input_sha256 = format!("{:x}", Sha256::digest(input));
    let output_sha256 = output.map(|bytes| format!("{:x}", Sha256::digest(bytes)));
    run.append_event(
        kind,
        crate::run::EventInput {
            profile_id: Some(execution.profile_id.clone()),
            outcome: Some(outcome.to_string()),
            plan_invocation: Some(crate::run::PlanInvocationEvidence {
                role: "plan".to_string(),
                stage,
                execution: execution.clone(),
                input_sha256: input_sha256.clone(),
                output_sha256: output_sha256.clone(),
                attempt,
                duration_ms: None,
                tokens: None,
            }),
            ..crate::run::EventInput::default()
        },
    )
    .map_err(|error| format!("plan invocation evidence: {error}"))?;
    if kind != crate::run::EventKind::AttemptFinished {
        return Ok(());
    }
    let plan = run.plan().map_err(|error| error.to_string())?;
    let complexity = match plan.target.input {
        crate::run::PlanInput::Bead { complexity, .. }
        | crate::run::PlanInput::Artifact { complexity, .. } => match complexity {
            crate::run::PlanComplexity::S => "S",
            crate::run::PlanComplexity::M => "M",
            crate::run::PlanComplexity::L => "L",
            crate::run::PlanComplexity::XL => "XL",
        },
    };
    crate::ledger::append(
        ledger_path,
        &crate::ledger::LedgerRow {
            date: Utc::now().format("%Y-%m-%d").to_string(),
            model: profile.dispatch_id.clone(),
            harness: Some(profile.harness.clone()),
            profile: Some(execution.profile_id.clone()),
            reasoning_effort: profile.reasoning_effort.clone(),
            role: "plan".to_string(),
            job: Some("plan".to_string()),
            stage: Some(plan_stage_label(stage).to_string()),
            execution_key: Some(execution.execution_key.clone()),
            provider: Some(execution.provider_id.clone()),
            input_sha256: Some(input_sha256),
            output_sha256,
            attempt: Some(attempt),
            outcome: Some(outcome.to_string()),
            tokens: None,
            task: run.run_id().to_string(),
            verify_passed: outcome == "returned",
            complexity: complexity.to_string(),
            project: plan.target.repo.clone(),
            notes: "conductor plan invocation".to_string(),
            failure_reason: None,
            duration_ms: None,
        },
    )
    .map_err(|error| format!("plan invocation ledger: {error}"))
}

fn plan_stage_label(stage: crate::run::PlanStage) -> &'static str {
    match stage {
        crate::run::PlanStage::Planner => "planner",
        crate::run::PlanStage::PeerReview => "peer-review",
        crate::run::PlanStage::SecondOpinion => "second-opinion",
    }
}

fn plan_artifact_bytes(
    run: &crate::run::RunHandle,
    artifact: &crate::run::ArtifactRef,
) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(run.dir().join(&artifact.path))
        .map_err(|error| format!("captured plan artifact: {error}"))?;
    if format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256 {
        return Err("captured plan artifact digest changed".to_string());
    }
    Ok(bytes)
}

fn peer_rubric(kind: PlanOutputKind) -> String {
    format!(
        "Review this {} for correctness, completeness, executable acceptance criteria, and target safety. Return only strict conductor/plan-peer-review@1 JSON.",
        kind.label()
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "all fail-closed review eligibility gates are deliberately visible at the bind boundary"
)]
fn select_reviewer<C: crate::bursar::BursarClient + ?Sized>(
    run: &mut crate::run::RunHandle,
    router: &crate::role_routing::RoleRouter,
    bursar: &C,
    snapshot: &crate::bursar::RosterSnapshot,
    stage: crate::run::PlanStage,
    author: &crate::run::ApprovedExecution,
    peer: Option<&crate::run::ApprovedExecution>,
) -> Result<(crate::run::ApprovedExecution, bool), String> {
    let plan = run.plan().map_err(|error| error.to_string())?;
    let route = plan
        .routes
        .stages
        .iter()
        .find(|route| route.stage == stage)
        .ok_or_else(|| "plan authorization omits required reviewer route".to_string())?;
    let author_tier = execution_tier(snapshot, author)?;
    let mut live = route
        .candidates
        .iter()
        .filter(|candidate| {
            route
                .constraints
                .distinct_execution_from
                .iter()
                .all(|dependency| match dependency {
                    crate::run::PlanStage::Planner => {
                        candidate.execution_key != author.execution_key
                    }
                    crate::run::PlanStage::PeerReview => {
                        peer.is_some_and(|known| candidate.execution_key != known.execution_key)
                    }
                    crate::run::PlanStage::SecondOpinion => false,
                })
        })
        .filter(|candidate| {
            route.constraints.tier_at_least.iter().all(|dependency| {
                !matches!(dependency, crate::run::PlanStage::Planner)
                    || execution_tier(snapshot, candidate).is_ok_and(|tier| tier >= author_tier)
            })
        })
        .filter(|candidate| {
            route
                .provider_distinct_from
                .iter()
                .all(|dependency| match dependency {
                    crate::run::PlanStage::Planner => candidate.provider_id != author.provider_id,
                    crate::run::PlanStage::PeerReview => {
                        peer.is_some_and(|known| candidate.provider_id != known.provider_id)
                    }
                    crate::run::PlanStage::SecondOpinion => false,
                })
        })
        .filter(|candidate| recheck_author(bursar, snapshot, candidate).is_ok())
        .cloned()
        .collect::<Vec<_>>();
    if live.is_empty() {
        return Err("no approved distinct reviewer candidate is currently eligible".to_string());
    }
    let (require_provider_distinct, degraded) = match route.constraints.provider_diversity {
        crate::run::PlanProviderDiversity::None => (false, false),
        crate::run::PlanProviderDiversity::CrossProviderOrDegraded => {
            let cross = live
                .iter()
                .filter(|candidate| candidate.provider_id != author.provider_id)
                .cloned()
                .collect::<Vec<_>>();
            if cross.is_empty() {
                (false, true)
            } else {
                live = cross;
                (true, false)
            }
        }
        crate::run::PlanProviderDiversity::PairwiseDistinct => {
            let peer = peer.ok_or_else(|| {
                "pairwise-distinct second opinion requires a bound peer".to_string()
            })?;
            live.retain(|candidate| {
                candidate.provider_id != author.provider_id
                    && candidate.provider_id != peer.provider_id
            });
            if live.is_empty() {
                return Err(
                    "no approved pairwise-provider-distinct second opinion is currently eligible"
                        .to_string(),
                );
            }
            (true, false)
        }
    };
    let minimum_tier = match author_tier {
        0 => crate::config::Tier::Junior,
        1 => crate::config::Tier::Senior,
        _ => crate::config::Tier::Lead,
    };
    let role = crate::role_routing::RoleId::new("plan").map_err(|error| error.to_string())?;
    let binding_run_id = next_reviewer_binding_run_id(router, &role, run.run_id(), stage)?;
    let mut constraints = planner_constraints(snapshot, minimum_tier)?;
    constraints.allowed_profile_ids = live
        .iter()
        .map(|candidate| {
            crate::role_routing::ProfileId::new(candidate.profile_id.clone())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<_, _>>()?;
    constraints.allowed_provider_ids = live
        .iter()
        .map(|candidate| candidate.provider_id.clone())
        .collect();
    constraints.approved_execution_keys = live
        .iter()
        .map(|candidate| candidate.execution_key.clone())
        .collect();
    let prepared = router
        .bind_reviewer(
            binding_run_id,
            role,
            stage,
            author.clone(),
            peer.cloned(),
            require_provider_distinct,
            constraints,
        )
        .map_err(|error| format!("reviewer scheduler binding: {error}"))?;
    let selected = prepared.route.selected.clone();
    let binding_evidence = prepared.route.reservation.run_id.as_str().to_string();
    let bind_result = match stage {
        crate::run::PlanStage::Planner => {
            return Err("planner has no delayed reviewer binding".to_string());
        }
        crate::run::PlanStage::PeerReview => run.bind_plan_peer(selected.clone(), binding_evidence),
        crate::run::PlanStage::SecondOpinion => {
            run.bind_plan_second_opinion(selected.clone(), binding_evidence)
        }
    };
    if let Err(error) = bind_result {
        let cancellation = router.cancel(&prepared.route.reservation);
        return match cancellation {
            Ok(_) => Err(format!("plan reviewer binding: {error}")),
            Err(cleanup) => Err(format!(
                "plan reviewer binding: {error}; reviewer reservation cleanup: {cleanup}"
            )),
        };
    }
    router
        .commit(&prepared.route.reservation)
        .map_err(|error| format!("reviewer reservation: {error}"))?;
    Ok((selected, degraded))
}

fn execution_tier(
    snapshot: &crate::bursar::RosterSnapshot,
    execution: &crate::run::ApprovedExecution,
) -> Result<u8, String> {
    let profile = snapshot
        .profiles
        .iter()
        .find(|profile| profile.profile_id == execution.profile_id)
        .ok_or_else(|| "approved plan execution is absent from captured roster".to_string())?;
    match profile.tier.as_str() {
        "junior" => Ok(0),
        "senior" => Ok(1),
        "lead" => Ok(2),
        _ => Err("approved plan execution has an invalid captured tier".to_string()),
    }
}

/// Cancels an unstarted plan or an explicitly cancellable pre-authoring block,
/// releasing the pending reservation while preserving scheduler rotation.
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
            | crate::run::PlanProgress::Blocked { cancellable: true }
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
        crate::run::PlanProgress::Blocked { .. } => "blocked",
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
    captured_snapshot: &crate::bursar::RosterSnapshot,
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
    let availability_key = captured_snapshot
        .providers
        .iter()
        .find(|provider| provider.provider_id == approved.provider_id)
        .filter(|provider| {
            captured_snapshot
                .profiles
                .iter()
                .find(|profile| profile.profile_id == approved.profile_id)
                .is_some_and(|profile| {
                    profile.provider_id == provider.provider_id
                        && crate::role_routing::approved_execution(profile, provider) == *approved
                })
        })
        .map(|provider| provider.availability_key.as_str())
        .ok_or_else(|| "approved plan author is absent from captured Bursar roster".to_string())?;
    let status = report.providers.get(availability_key).ok_or_else(|| {
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

fn planner_candidates(
    plan: &crate::run::PlanRunDetails,
) -> Result<Vec<crate::run::ApprovedExecution>, String> {
    let route = plan
        .routes
        .stages
        .iter()
        .find(|route| route.stage == crate::run::PlanStage::Planner)
        .ok_or_else(|| "plan authorization omits planner route".to_string())?;
    if route.candidates.is_empty() {
        return Err("plan authorization has no approved planner candidates".to_string());
    }
    Ok(route.candidates.clone())
}

fn available_planner<C: crate::bursar::BursarClient + ?Sized>(
    bursar: &C,
    captured_snapshot: &crate::bursar::RosterSnapshot,
    candidates: &[crate::run::ApprovedExecution],
    exclude: Option<&crate::run::ApprovedExecution>,
) -> Result<crate::run::ApprovedExecution, String> {
    let mut failures = Vec::new();
    for candidate in candidates {
        if exclude.is_some_and(|excluded| candidate == excluded) {
            continue;
        }
        match recheck_author(bursar, captured_snapshot, candidate) {
            Ok(()) => return Ok(candidate.clone()),
            Err(error) => failures.push(format!("{}: {error}", candidate.profile_id)),
        }
    }
    Err(format!(
        "no approved planner candidate is currently eligible ({})",
        failures.join("; ")
    ))
}

fn profile_for_execution(
    snapshot: &crate::bursar::RosterSnapshot,
    execution: &crate::run::ApprovedExecution,
) -> Result<crate::bursar::RosterProfile, String> {
    let profile = snapshot
        .profiles
        .iter()
        .find(|profile| profile.profile_id == execution.profile_id)
        .ok_or_else(|| "approved plan author is absent from captured Bursar roster".to_string())?;
    let provider = snapshot
        .providers
        .iter()
        .find(|provider| provider.provider_id == profile.provider_id)
        .ok_or_else(|| {
            "approved plan author provider is absent from captured Bursar roster".to_string()
        })?;
    if crate::role_routing::approved_execution(profile, provider) != *execution {
        return Err("approved plan author differs from captured Bursar execution".to_string());
    }
    Ok(profile.clone())
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
    fn plan_document_requires_explicit_kind_and_raw_json() {
        let missing_kind = br#"{
            "schema":"conductor/plan-document@1",
            "title":"Plan",
            "context":"Context",
            "tasks":[{"id":"one","depends_on":[],"targets":[{"file":"src/x.rs","symbol":"x"}],"change":"Change","acceptance":"Accept","verify":"cargo test"}],
            "risks":[],
            "assumptions":[]
        }"#;
        assert!(
            parse_document(PlanOutputKind::ImplementationPlan, missing_kind).is_err(),
            "the approved kind must be explicit rather than inferred"
        );

        let fenced = br#"```json
{"schema":"conductor/plan-document@1","kind":"implementation-plan","title":"Plan","context":"Context","tasks":[{"id":"one","depends_on":[],"targets":[{"file":"src/x.rs","symbol":"x"}],"change":"Change","acceptance":"Accept","verify":"cargo test"}],"risks":[],"assumptions":[]}
```"#;
        assert!(
            parse_document(PlanOutputKind::ImplementationPlan, fenced).is_err(),
            "fenced JSON is not a valid wire artifact"
        );
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

    struct FakeAuthor(std::sync::Mutex<Vec<Vec<u8>>>);

    impl FakeAuthor {
        fn new(outputs: Vec<Vec<u8>>) -> Self {
            Self(std::sync::Mutex::new(outputs))
        }
    }

    impl PlanAuthor for FakeAuthor {
        fn author(&self, _request: &PlanAuthorRequest) -> Result<Vec<u8>, String> {
            let mut outputs = self.0.lock().map_err(|_| "fake author lock".to_string())?;
            if outputs.is_empty() {
                return Err("fake author has no output".to_string());
            }
            Ok(outputs.remove(0))
        }
        fn revise(&self, _request: &PlanRevisionRequest) -> Result<Vec<u8>, String> {
            let mut outputs = self.0.lock().map_err(|_| "fake author lock".to_string())?;
            if outputs.is_empty() {
                return Err("fake author has no revision output".to_string());
            }
            Ok(outputs.remove(0))
        }

        fn peer_review(&self, _request: &PlanPeerReviewRequest) -> Result<Vec<u8>, String> {
            Ok(
                br#"{"schema":"conductor/plan-peer-review@1","verdict":"approve","findings":[]}"#
                    .to_vec(),
            )
        }

        fn second_opinion(&self, _request: &PlanSecondOpinionRequest) -> Result<Vec<u8>, String> {
            Ok(br#"{"schema":"conductor/plan-second-opinion@1","verdict":"accept"}"#.to_vec())
        }
    }

    struct StatusSwitchBursar {
        snapshot: crate::bursar::RosterSnapshot,
        status: std::sync::Mutex<crate::bursar::StatusReport>,
    }

    impl StatusSwitchBursar {
        fn from_fake(fake: &FakeBursar) -> Self {
            Self {
                snapshot: fake.snapshot.clone(),
                status: std::sync::Mutex::new(fake.status.clone()),
            }
        }

        fn exhaust(&self, provider_id: &str) {
            self.status
                .lock()
                .expect("status lock")
                .providers
                .get_mut(provider_id)
                .expect("provider")
                .availability = crate::bursar::Availability::Exhausted;
        }
    }

    impl crate::bursar::BursarClient for StatusSwitchBursar {
        fn status(&self) -> crate::bursar::Result<crate::bursar::StatusReport> {
            Ok(self.status.lock().expect("status lock").clone())
        }

        fn roster_snapshot(&self) -> crate::bursar::Result<crate::bursar::RosterSnapshot> {
            Ok(self.snapshot.clone())
        }
    }
    struct TransientStatusBursar {
        snapshot: crate::bursar::RosterSnapshot,
        status: crate::bursar::StatusReport,
        status_calls: std::sync::Mutex<usize>,
        unavailable_on_call: usize,
    }

    impl TransientStatusBursar {
        fn from_fake(fake: &FakeBursar, unavailable_on_call: usize) -> Self {
            Self {
                snapshot: fake.snapshot.clone(),
                status: fake.status.clone(),
                status_calls: std::sync::Mutex::new(0),
                unavailable_on_call,
            }
        }
    }

    impl crate::bursar::BursarClient for TransientStatusBursar {
        fn status(&self) -> crate::bursar::Result<crate::bursar::StatusReport> {
            let mut calls = self.status_calls.lock().expect("status call lock");
            *calls += 1;
            let mut status = self.status.clone();
            if *calls == self.unavailable_on_call {
                for provider in status.providers.values_mut() {
                    provider.availability = crate::bursar::Availability::Exhausted;
                }
            }
            Ok(status)
        }

        fn roster_snapshot(&self) -> crate::bursar::Result<crate::bursar::RosterSnapshot> {
            Ok(self.snapshot.clone())
        }
    }


    struct CapturingAuthor {
        output: Vec<u8>,
        calls: std::sync::Mutex<Vec<crate::run::ApprovedExecution>>,
    }

    impl PlanAuthor for CapturingAuthor {
        fn author(&self, request: &PlanAuthorRequest) -> Result<Vec<u8>, String> {
            self.calls
                .lock()
                .map_err(|_| "capturing author lock".to_string())?
                .push(request.execution.clone());
            Ok(self.output.clone())
        }
        fn peer_review(&self, _request: &PlanPeerReviewRequest) -> Result<Vec<u8>, String> {
            Ok(
                br#"{"schema":"conductor/plan-peer-review@1","verdict":"approve","findings":[]}"#
                    .to_vec(),
            )
        }
    }

    struct FailingAuthor(std::sync::Mutex<usize>);

    impl PlanAuthor for FailingAuthor {
        fn author(&self, _request: &PlanAuthorRequest) -> Result<Vec<u8>, String> {
            *self
                .0
                .lock()
                .map_err(|_| "failing author lock".to_string())? += 1;
            Err("simulated author crash".to_string())
        }
    }

    struct ScriptedExecutor {
        authors: std::sync::Mutex<Vec<Result<Vec<u8>, String>>>,
        revisions: std::sync::Mutex<Vec<Result<Vec<u8>, String>>>,
        peers: std::sync::Mutex<Vec<Result<Vec<u8>, String>>>,
        seconds: std::sync::Mutex<Vec<Result<Vec<u8>, String>>>,
        calls: std::sync::Mutex<Vec<(String, crate::run::ApprovedExecution)>>,
    }

    impl ScriptedExecutor {
        fn new(
            authors: Vec<Result<Vec<u8>, String>>,
            revisions: Vec<Result<Vec<u8>, String>>,
            peers: Vec<Result<Vec<u8>, String>>,
            seconds: Vec<Result<Vec<u8>, String>>,
        ) -> Self {
            Self {
                authors: std::sync::Mutex::new(authors),
                revisions: std::sync::Mutex::new(revisions),
                peers: std::sync::Mutex::new(peers),
                seconds: std::sync::Mutex::new(seconds),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn take(
            queue: &std::sync::Mutex<Vec<Result<Vec<u8>, String>>>,
            stage: &str,
        ) -> Result<Vec<u8>, String> {
            queue
                .lock()
                .map_err(|_| format!("{stage} script lock"))?
                .drain(..1)
                .next()
                .ok_or_else(|| format!("{stage} script is exhausted"))?
        }

        fn record(
            &self,
            stage: &str,
            execution: &crate::run::ApprovedExecution,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .map_err(|_| "script call lock".to_string())?
                .push((stage.to_string(), execution.clone()));
            Ok(())
        }
    }

    impl PlanAuthor for ScriptedExecutor {
        fn author(&self, request: &PlanAuthorRequest) -> Result<Vec<u8>, String> {
            self.record("author", &request.execution)?;
            Self::take(&self.authors, "author")
        }

        fn revise(&self, request: &PlanRevisionRequest) -> Result<Vec<u8>, String> {
            self.record("revision", &request.execution)?;
            Self::take(&self.revisions, "revision")
        }

        fn peer_review(&self, request: &PlanPeerReviewRequest) -> Result<Vec<u8>, String> {
            self.record("peer", &request.execution)?;
            Self::take(&self.peers, "peer")
        }

        fn second_opinion(&self, request: &PlanSecondOpinionRequest) -> Result<Vec<u8>, String> {
            self.record("second", &request.execution)?;
            Self::take(&self.seconds, "second")
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
            ("planner-d", "anthropic"),
        ]
        .into_iter()
        .map(|(profile_id, provider_id)| {
            serde_json::json!({
                "profile_id": profile_id, "provider_id": provider_id, "model": profile_id,
                "harness": "pi", "dispatch_id": profile_id, "reasoning_effort": null,
                "tier": "lead", "ceiling": "XL", "efficiency": "lean", "cost": 0.0,
                "data_policy": "standard", "enabled": true, "roles": ["plan"],
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
role = "plan"
profile_id = "planner-a"
weight = 1
enabled = true

[[role_binding]]
role = "plan"
profile_id = "planner-b"
weight = 1
enabled = true

[[role_binding]]
role = "plan"
profile_id = "planner-c"
weight = 1
enabled = true

[[role_binding]]
role = "plan"
profile_id = "planner-d"
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
                ledger_path: std::env::temp_dir().join(format!(
                    "conductor-plan-ledger-{label}-{}.jsonl",
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
    #[test]
    fn initial_policy_preflight_accepts_every_author_peer_and_spec_team_contingency() {
        let (_temp, _paths, config, bursar) = plan_fixture("initial-policy-preflight");

        validate_initial_policy(&config, &bursar.snapshot)
            .expect("three provider plan pool is valid for every author, peer, and spec team");
    }
    #[test]
    fn initial_policy_preflight_rejects_a_spec_pool_without_three_provider_contingencies() {
        let (_temp, _paths, _config, bursar) = plan_fixture("initial-policy-two-provider");
        let two_provider_config = crate::config::parse_str(
            r#"
[[role_binding]]
role = "plan"
profile_id = "planner-a"
weight = 60
enabled = true

[[role_binding]]
role = "plan"
profile_id = "planner-b"
weight = 40
enabled = true
"#,
        )
        .expect("two-provider policy parses before cross-provider preflight");

        let error = validate_initial_policy(&two_provider_config, &bursar.snapshot)
            .expect_err("spec policy needs a third provider contingency");

        assert!(error.contains("three-way team"), "{error}");
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

    fn implementation_request(repo: PathBuf) -> PlanPrepareRequest {
        PlanPrepareRequest {
            repo,
            input: PlanPrepareInput::Artifact {
                bytes: b"author this".to_vec(),
                tier: crate::run::PlanTier::Lead,
                complexity: crate::run::PlanComplexity::XL,
            },
            output_kind: PlanOutputKind::ImplementationPlan,
            max_plan_revisions: 1,
            require_second_opinion: false,
        }
    }

    #[test]
    fn repeated_prepare_create_plan_failures_cancel_capacity_without_rewinding_scores() {
        let (temp, paths, config, bursar) = plan_fixture("prepare-create-failure");
        let repo = initialized_repo(&temp);
        std::fs::create_dir_all(&paths.state_dir).expect("state dir");
        std::fs::write(paths.state_dir.join("runs-v2"), b"not a directory")
            .expect("block plan run creation");

        for _ in 0..4 {
            assert!(
                prepare(
                    &paths,
                    &config,
                    &bursar,
                    implementation_request(repo.clone())
                )
                .is_err(),
                "a failed RunHandle::create_plan must surface to the caller"
            );
        }

        std::fs::remove_file(paths.state_dir.join("runs-v2")).expect("unblock run creation");
        let prepared = prepare(&paths, &config, &bursar, implementation_request(repo))
            .expect("failed preparation must not consume every profile's in-flight capacity");
        let approval = load_approval(
            &crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run"),
        )
        .expect("approval");
        assert_eq!(
            approval.reservation.sequence, 5,
            "cancellation preserves irreversible scheduling score history"
        );
    }

    #[test]
    fn repeated_prepare_report_write_failures_cancel_capacity_and_remove_unpublished_runs() {
        let (temp, paths, config, bursar) = plan_fixture("prepare-report-failure");
        let repo = initialized_repo(&temp);
        std::fs::create_dir_all(&paths.reports_home).expect("reports home");
        std::fs::write(paths.reports_home.join(".harness"), b"not a directory")
            .expect("block report write");

        for _ in 0..4 {
            assert!(
                prepare(
                    &paths,
                    &config,
                    &bursar,
                    implementation_request(repo.clone())
                )
                .is_err(),
                "a failed approval report must surface to the caller"
            );
        }

        assert_eq!(
            std::fs::read_dir(crate::run::runs_dir(&paths.state_dir))
                .expect("run root")
                .count(),
            0,
            "a plan without its approval report is not a durable dispatchable run"
        );
        std::fs::remove_file(paths.reports_home.join(".harness")).expect("unblock report write");
        let prepared = prepare(&paths, &config, &bursar, implementation_request(repo))
            .expect("failed report writes must not consume every profile's in-flight capacity");
        let approval = load_approval(
            &crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run"),
        )
        .expect("approval");
        assert_eq!(approval.reservation.sequence, 5);
    }

    #[test]
    fn restart_reconciliation_cancels_an_unlinked_reviewer_binding_without_rewinding() {
        let (temp, paths, config, bursar) = plan_fixture("unlinked-reviewer-binding");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            implementation_request(initialized_repo(&temp)),
        )
        .expect("prepare");
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let approval = load_approval(&run).expect("approval");
        let router = crate::role_routing::RoleRouter::with_pinned_snapshot(
            &paths.state_dir,
            crate::role_routing::RoutingPolicy::from_config(&config, &bursar.snapshot)
                .expect("routing policy"),
            bursar.snapshot.clone(),
        )
        .expect("router");
        let reservation = router
            .bind_reviewer(
                crate::role_routing::RunId::new("orphan-peer-review-bind").expect("binding run id"),
                crate::role_routing::RoleId::new("plan").expect("role"),
                crate::role_routing::PlanStage::PeerReview,
                approval.author,
                None,
                true,
                planner_constraints(&bursar.snapshot, crate::config::Tier::Lead)
                    .expect("constraints"),
            )
            .expect("reservation before simulated bind crash")
            .route
            .reservation;

        reconcile_plan_reservations(&router, &paths.state_dir)
            .expect("production restart reconciliation");

        assert!(matches!(
            router
                .reservation(
                    &crate::role_routing::RoleId::new("plan").expect("role"),
                    crate::role_routing::PlanStage::PeerReview,
                    &reservation.run_id,
                )
                .expect("reservation lookup"),
            Some(crate::role_routing::Reservation {
                state: crate::role_routing::ReservationState::Canceled,
                sequence: 1,
                ..
            })
        ));
    }

    #[test]
    fn peer_bind_crash_retries_with_a_fresh_durable_generation() {
        let (temp, paths, config, bursar) = plan_fixture("peer-bind-crash-retry");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            implementation_request(initialized_repo(&temp)),
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let mut run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let approval = load_approval(&run).expect("approval");
        run.start_plan_authoring(approval.author.clone())
            .expect("author binding");
        let artifact = run
            .capture_plan_artifact(
                Path::new("artifacts/plan-document.json"),
                &implementation_document(),
            )
            .expect("artifact");
        run.await_plan_peer(artifact).expect("await peer");
        drop(run);
        let router = crate::role_routing::RoleRouter::with_pinned_snapshot(
            &paths.state_dir,
            crate::role_routing::RoutingPolicy::from_config(&config, &bursar.snapshot)
                .expect("routing policy"),
            bursar.snapshot.clone(),
        )
        .expect("router");
        let role = crate::role_routing::RoleId::new("plan").expect("role");
        let stale_id =
            reviewer_binding_run_id(&prepared.run_id, crate::role_routing::PlanStage::PeerReview)
                .expect("stable binding id");
        router
            .bind_reviewer(
                stale_id.clone(),
                role.clone(),
                crate::role_routing::PlanStage::PeerReview,
                approval.author,
                None,
                true,
                planner_constraints(&bursar.snapshot, crate::config::Tier::Lead)
                    .expect("constraints"),
            )
            .expect("fault-window reservation");

        let executor =
            ScriptedExecutor::new(Vec::new(), Vec::new(), vec![Ok(peer_approve())], Vec::new());
        dispatch(&paths, &config, &bursar, &prepared.run_id, &executor)
            .expect("restart retries after canceling the unlinked peer reservation");

        assert!(matches!(
            router
                .reservation(&role, crate::role_routing::PlanStage::PeerReview, &stale_id)
                .expect("stale reservation"),
            Some(crate::role_routing::Reservation {
                state: crate::role_routing::ReservationState::Canceled,
                ..
            })
        ));
        let retry_id = crate::role_routing::RunId::new(format!(
            "{}-peer-review-bind-retry-1",
            prepared.run_id
        ))
        .expect("retry binding id");
        assert!(matches!(
            router
                .reservation(&role, crate::role_routing::PlanStage::PeerReview, &retry_id)
                .expect("retry reservation"),
            Some(crate::role_routing::Reservation {
                state: crate::role_routing::ReservationState::Committed,
                ..
            })
        ));
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the regression constructs the exact second-opinion bind crash window"
    )]
    fn second_opinion_bind_crash_retries_with_a_fresh_durable_generation() {
        let (temp, paths, config, bursar) = plan_fixture("second-bind-crash-retry");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo: initialized_repo(&temp),
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::Spec,
                max_plan_revisions: 1,
                require_second_opinion: true,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let mut run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let approval = load_approval(&run).expect("approval");
        run.start_plan_authoring(approval.author.clone())
            .expect("author binding");
        let artifact = run
            .capture_plan_artifact(Path::new("artifacts/plan-document.json"), &spec_document())
            .expect("artifact");
        run.await_plan_peer(artifact).expect("await peer");
        let peer = run
            .plan()
            .expect("plan")
            .routes
            .stages
            .iter()
            .find(|route| route.stage == crate::run::PlanStage::PeerReview)
            .and_then(|route| {
                route
                    .candidates
                    .iter()
                    .find(|candidate| candidate.provider_id != approval.author.provider_id)
            })
            .cloned()
            .expect("provider-distinct peer");
        run.record_plan_peer_verdict(peer.clone(), crate::run::PeerVerdict::Approve, true)
            .expect("peer approval");
        drop(run);
        let router = crate::role_routing::RoleRouter::with_pinned_snapshot(
            &paths.state_dir,
            crate::role_routing::RoutingPolicy::from_config(&config, &bursar.snapshot)
                .expect("routing policy"),
            bursar.snapshot.clone(),
        )
        .expect("router");
        let role = crate::role_routing::RoleId::new("plan").expect("role");
        let stale_id = reviewer_binding_run_id(
            &prepared.run_id,
            crate::role_routing::PlanStage::SecondOpinion,
        )
        .expect("stable binding id");
        router
            .bind_reviewer(
                stale_id.clone(),
                role.clone(),
                crate::role_routing::PlanStage::SecondOpinion,
                approval.author,
                Some(peer),
                true,
                planner_constraints(&bursar.snapshot, crate::config::Tier::Lead)
                    .expect("constraints"),
            )
            .expect("fault-window reservation");

        let executor = ScriptedExecutor::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![Ok(
                br#"{"schema":"conductor/plan-second-opinion@1","verdict":"accept"}"#.to_vec(),
            )],
        );
        dispatch(&paths, &config, &bursar, &prepared.run_id, &executor)
            .expect("restart retries after canceling the unlinked opinion reservation");

        assert!(matches!(
            router
                .reservation(
                    &role,
                    crate::role_routing::PlanStage::SecondOpinion,
                    &stale_id,
                )
                .expect("stale reservation"),
            Some(crate::role_routing::Reservation {
                state: crate::role_routing::ReservationState::Canceled,
                ..
            })
        ));
        let retry_id = crate::role_routing::RunId::new(format!(
            "{}-second-opinion-bind-retry-1",
            prepared.run_id
        ))
        .expect("retry binding id");
        assert!(matches!(
            router
                .reservation(
                    &role,
                    crate::role_routing::PlanStage::SecondOpinion,
                    &retry_id,
                )
                .expect("retry reservation"),
            Some(crate::role_routing::Reservation {
                state: crate::role_routing::ReservationState::Committed,
                ..
            })
        ));
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
    fn plan_ledger_rows(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .expect("ledger")
            .lines()
            .map(|line| serde_json::from_str(line).expect("ledger row"))
            .collect()
    }

    fn assert_successful_plan_ledger_and_events(paths: &PlanJobPaths, run_id: &str) {
        assert!(
            paths.ledger_path.is_file(),
            "every fake plan backend call must leave a generic ledger artifact"
        );
        let rows = plan_ledger_rows(&paths.ledger_path);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .map(|row| (
                    row["role"].as_str(),
                    row["job"].as_str(),
                    row["stage"].as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                (Some("plan"), Some("plan"), Some("planner")),
                (Some("plan"), Some("plan"), Some("peer-review")),
                (Some("plan"), Some("plan"), Some("second-opinion")),
            ]
        );
        assert!(rows.iter().all(|row| {
            row["profile"].as_str().is_some()
                && row["execution_key"].as_str().is_some()
                && row["provider"].as_str().is_some()
                && row["input_sha256"]
                    .as_str()
                    .is_some_and(|digest| digest.len() == 64)
                && row["output_sha256"]
                    .as_str()
                    .is_some_and(|digest| digest.len() == 64)
                && row["attempt"] == 1
                && row["outcome"] == "returned"
                && row["duration_ms"].is_null()
                && row["tokens"].is_null()
        }));
        let run = crate::run::RunHandle::open(&paths.state_dir, run_id).expect("run");
        let events = crate::run::read_events(&run.events_path()).expect("events");
        let invocations = events
            .iter()
            .filter(|event| event.kind == crate::run::EventKind::AttemptFinished)
            .filter_map(|event| event.plan_invocation.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(
            invocations
                .iter()
                .map(|evidence| evidence.stage)
                .collect::<Vec<_>>(),
            vec![
                crate::run::PlanStage::Planner,
                crate::run::PlanStage::PeerReview,
                crate::run::PlanStage::SecondOpinion,
            ]
        );
        assert!(invocations.iter().all(|evidence| {
            evidence.role == "plan"
                && evidence.input_sha256.len() == 64
                && evidence
                    .output_sha256
                    .as_ref()
                    .is_some_and(|digest| digest.len() == 64)
                && evidence.attempt == 1
        }));
    }

    fn assert_malformed_peer_repair_ledger(paths: &PlanJobPaths) {
        let rows = plan_ledger_rows(&paths.ledger_path);
        assert_eq!(rows.len(), 3, "author plus both peer invocations");
        assert_eq!(
            rows.iter()
                .map(|row| {
                    (
                        row["stage"].as_str(),
                        row["attempt"].as_u64(),
                        row["outcome"].as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (Some("planner"), Some(1), Some("returned")),
                (Some("peer-review"), Some(1), Some("malformed")),
                (Some("peer-review"), Some(2), Some("returned")),
            ]
        );
        assert!(rows.iter().all(|row| {
            row["input_sha256"]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
                && row["output_sha256"]
                    .as_str()
                    .is_some_and(|digest| digest.len() == 64)
                && row["duration_ms"].is_null()
                && row["tokens"].is_null()
        }));
    }

    fn implementation_document() -> Vec<u8> {
        br#"{"schema":"conductor/plan-document@1","kind":"implementation-plan","title":"Plan","context":"Context","tasks":[{"id":"one","depends_on":[],"targets":[{"file":"src/x.rs","symbol":"x"}],"change":"Change","acceptance":"Accept","verify":"cargo test"}],"risks":[],"assumptions":[]}"#.to_vec()
    }

    fn spec_document() -> Vec<u8> {
        br#"{"schema":"conductor/plan-document@1","kind":"spec","title":"Spec","context":"Context","goals":["Goal"],"constraints":["Constraint"],"requirements":["Requirement"],"acceptance":["Acceptance"],"verification":["cargo test"],"non_goals":[],"risks":[],"assumptions":[],"open_questions":[]}"#.to_vec()
    }
    #[expect(
        clippy::too_many_lines,
        reason = "the fixture preserves the real ProviderId versus AvailabilityKey boundary"
    )]
    fn omp_plan_fixture(label: &str) -> (TestDir, PlanJobPaths, crate::config::Config, FakeBursar) {
        let temp = TestDir::new(label);
        let checked_at = chrono::Utc::now().to_rfc3339();
        let provider_rows = [
            ("openai-codex", "codex"),
            ("anthropic", "anthropic"),
            ("opencode-go", "opencode-go"),
        ]
        .into_iter()
        .map(|(provider_id, availability_key)| {
            serde_json::json!({
                "provider_id": provider_id, "availability_key": availability_key, "enabled": true,
                "state": "healthy", "availability": "healthy", "checked_at": checked_at,
                "data_as_of": null, "expires_at": "2100-01-01T00:00:00Z", "reason": null,
                "eligible": true, "ineligibility_reason": null
            })
        })
        .collect::<Vec<_>>();
        let profiles = [
            (
                "openai-codex--omp--gpt-5.6-sol--xhigh",
                "openai-codex",
                "gpt-5.6-sol",
            ),
            (
                "anthropic--omp--claude-opus-4-8--max",
                "anthropic",
                "claude-opus-4-8",
            ),
            ("opencode-go--omp--kimi-k3--max", "opencode-go", "kimi-k3"),
        ];
        let profile_rows = profiles
            .iter()
            .map(|(profile_id, provider_id, model)| {
                serde_json::json!({
                    "profile_id": profile_id, "provider_id": provider_id, "model": model,
                    "harness": "omp", "dispatch_id": model, "reasoning_effort": "max",
                    "tier": "lead", "ceiling": "XL", "efficiency": "lean", "cost": 0.0,
                    "data_policy": "standard", "enabled": true, "roles": ["plan"],
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
            .expect("strict OMP fixture roster");
        let providers = [
            ("codex", crate::bursar::Availability::Healthy),
            ("anthropic", crate::bursar::Availability::Healthy),
            ("opencode-go", crate::bursar::Availability::Healthy),
        ]
        .into_iter()
        .map(|(availability_key, availability)| {
            (
                availability_key.to_string(),
                crate::bursar::ProviderStatus {
                    availability,
                    source: "test".to_string(),
                    checked_at: checked_at.clone(),
                    data_as_of: None,
                    expires_at: Some("2100-01-01T00:00:00Z".to_string()),
                    windows: vec![],
                    reason: None,
                    extra: serde_json::Map::new(),
                },
            )
        })
        .collect();
        let config = crate::config::parse_str(
            r#"
[[role_binding]]
role = "plan"
profile_id = "openai-codex--omp--gpt-5.6-sol--xhigh"
weight = 60
enabled = true

[[role_binding]]
role = "plan"
profile_id = "anthropic--omp--claude-opus-4-8--max"
weight = 20
enabled = true

[[role_binding]]
role = "plan"
profile_id = "opencode-go--omp--kimi-k3--max"
weight = 20
enabled = true
"#,
        )
        .expect("60/20/20 OMP planner config");
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
                ledger_path: std::env::temp_dir().join(format!(
                    "conductor-plan-ledger-{label}-{}.jsonl",
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

    #[test]
    fn spec_revision_resume_uses_distinct_omp_availability_key_identities() {
        let (temp, paths, config, bursar) = omp_plan_fixture("omp-second-opinion");
        let repo = initialized_repo(&temp);
        let before_head = git_output(&repo, ["rev-parse", "HEAD"]).expect("head");
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
                output_kind: PlanOutputKind::Spec,
                max_plan_revisions: 1,
                require_second_opinion: true,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(spec_document())],
            vec![Ok(spec_document())],
            vec![Ok(peer_revise()), Ok(peer_approve())],
            vec![
                Err("simulated second-opinion crash".to_string()),
                Ok(br#"{"schema":"conductor/plan-second-opinion@1","verdict":"accept"}"#.to_vec()),
            ],
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "awaiting_second_opinion"
        );
        dispatch(&paths, &config, &bursar, &prepared.run_id, &executor)
            .expect("resume completes the bound second opinion");

        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "accepted"
        );
        assert_eq!(
            git_output(&repo, ["rev-parse", "HEAD"]).expect("head"),
            before_head,
            "planning never mutates the target"
        );
        let calls = executor.calls.lock().expect("calls");
        assert_eq!(
            calls
                .iter()
                .map(|(stage, _)| stage.as_str())
                .collect::<Vec<_>>(),
            ["author", "peer", "revision", "peer", "second", "second"]
        );
        assert_eq!(calls[0].1, calls[2].1, "revision reuses its author");
        assert_eq!(calls[1].1, calls[3].1, "approval reuses its peer");
        assert_eq!(calls[4].1, calls[5].1, "resume reuses its opinion binding");
        assert_eq!(
            calls[0].1.provider_id, "openai-codex",
            "the 60 weight author proves availability-key lookup is not ProviderId lookup"
        );
        assert_ne!(calls[0].1.execution_key, calls[1].1.execution_key);
        assert_ne!(calls[0].1.execution_key, calls[4].1.execution_key);
        assert_ne!(calls[1].1.execution_key, calls[4].1.execution_key);
        assert_ne!(calls[0].1.provider_id, calls[1].1.provider_id);
        assert_ne!(calls[0].1.provider_id, calls[4].1.provider_id);
        assert_ne!(calls[1].1.provider_id, calls[4].1.provider_id);
        drop(calls);
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let plan = run.plan().expect("plan");
        assert_eq!(plan.stage_attempts.planner, 2);
        assert_eq!(plan.stage_attempts.peer_review, 2);
        assert_eq!(plan.stage_attempts.second_opinion, 2);
        assert_eq!(
            plan.routes
                .stages
                .iter()
                .map(|route| route.candidates.len())
                .collect::<Vec<_>>(),
            vec![3, 3, 3],
            "all 60/20/20 candidates remain pinned in every stage route"
        );
    }

    fn peer_approve() -> Vec<u8> {
        br#"{"schema":"conductor/plan-peer-review@1","verdict":"approve","findings":[]}"#.to_vec()
    }

    fn peer_revise() -> Vec<u8> {
        br#"{"schema":"conductor/plan-peer-review@1","verdict":"revise","findings":[{"id":"F-1","severity":"high","location":"tasks.one","problem":"Missing detail","required_change":"Add the missing detail"}]}"#.to_vec()
    }

    #[test]
    fn prepared_plan_run_id_passes_cli_dispatch_parsing_and_dispatches() {
        let (temp, paths, config, bursar) = plan_fixture("cli-run-id");
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
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

        let (status_run_id, _) =
            crate::cli::parse_plan_run_options(std::slice::from_ref(&prepared.run_id), "status")
                .expect("prepare-generated run id must pass CLI status parsing");
        assert_eq!(
            status(&paths, &status_run_id).expect("prepared status"),
            "awaiting_approval"
        );

        let (run_id, _) =
            crate::cli::parse_plan_run_options(std::slice::from_ref(&prepared.run_id), "dispatch")
                .expect("prepare-generated run id must pass CLI dispatch parsing");

        approve(&paths, &run_id);
        let author = FakeAuthor::new(vec![implementation_document()]);
        dispatch(&paths, &config, &bursar, &run_id, &author).expect("dispatch");
        assert_eq!(status(&paths, &run_id).expect("status"), "accepted");
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
        let author = FakeAuthor::new(vec![br#"{"schema":"conductor/plan-document@1","kind":"implementation-plan","title":"Plan","context":"Context","tasks":[{"id":"one","depends_on":[],"targets":[{"file":"src/x.rs","symbol":"x"}],"change":"Change","acceptance":"Accept","verify":"cargo test"}],"risks":[],"assumptions":[]}"#.to_vec()]);
        dispatch(&paths, &config, &bursar, &prepared.run_id, &author).expect("dispatch");
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "accepted"
        );
        assert_eq!(
            git_output(&repo, ["rev-parse", "HEAD"]).expect("head"),
            before
        );
    }
    #[test]
    fn implementation_plan_executes_required_peer_review_to_acceptance() {
        let (temp, paths, config, bursar) = plan_fixture("implementation-peer");
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let author = FakeAuthor::new(vec![br#"{"schema":"conductor/plan-document@1","kind":"implementation-plan","title":"Plan","context":"Context","tasks":[{"id":"one","depends_on":[],"targets":[{"file":"src/x.rs","symbol":"x"}],"change":"Change","acceptance":"Accept","verify":"cargo test"}],"risks":[],"assumptions":[]}"#.to_vec()]);

        dispatch(&paths, &config, &bursar, &prepared.run_id, &author).expect("dispatch");

        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "accepted",
            "implementation plans require an executed peer verdict before acceptance"
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
        let author = FakeAuthor::new(vec![br#"{"schema":"conductor/plan-document@1","kind":"spec","title":"Spec","context":"Context","goals":["Goal"],"constraints":["Invariant"],"requirements":["Requirement"],"acceptance":["Acceptance"],"verification":["cargo test"],"non_goals":[],"risks":[],"assumptions":[],"open_questions":["Needs operator answer"]}"#.to_vec()]);
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
    #[test]
    fn provider_loss_before_first_artifact_uses_only_approved_planner_fallback() {
        let (temp, paths, config, fake) = plan_fixture("provider-fallback");
        let bursar = StatusSwitchBursar::from_fake(&fake);
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let approval = load_approval(&run).expect("approval");
        let candidates = planner_candidates(run.plan().expect("plan")).expect("route");
        assert_eq!(candidates.first(), Some(&approval.author));
        bursar.exhaust(&approval.author.provider_id);
        approve(&paths, &prepared.run_id);
        let author = CapturingAuthor {
            output: br#"{"schema":"conductor/plan-document@1","kind":"implementation-plan","title":"Plan","context":"Context","tasks":[{"id":"one","depends_on":[],"targets":[{"file":"src/x.rs","symbol":"x"}],"change":"Change","acceptance":"Accept","verify":"cargo test"}],"risks":[],"assumptions":[]}"#.to_vec(),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        dispatch(&paths, &config, &bursar, &prepared.run_id, &author).expect("fallback dispatch");

        let calls = author.calls.lock().expect("calls");
        assert_eq!(calls.len(), 1);
        assert_ne!(calls[0], approval.author);
        assert!(candidates.contains(&calls[0]));
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "accepted"
        );
    }

    #[test]
    fn malformed_initial_and_repair_exhaustion_finishes_terminal_blocked_with_evidence() {
        let (temp, paths, config, bursar) = plan_fixture("repair-attempts");
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let author = FakeAuthor::new(vec![b"not json".to_vec(), b"still not json".to_vec()]);

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err());
        assert!(
            matches!(
                crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id)
                    .expect("run")
                    .plan()
                    .expect("plan")
                    .progress,
                crate::run::PlanProgress::Terminal {
                    verdict: crate::run::PlanTerminalVerdict::Blocked
                }
            ),
            "schema repair exhaustion must not leave a resumable authoring manifest"
        );
        assert!(
            dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err(),
            "a terminal run must reject resume"
        );
        assert!(
            author.0.lock().expect("outputs").is_empty(),
            "terminal resume must make no additional backend call"
        );
        let rows = plan_ledger_rows(&paths.ledger_path);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["outcome"], "malformed");
        assert_eq!(rows[1]["outcome"], "malformed");
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let events = crate::run::read_events(&run.events_path()).expect("events");
        assert!(matches!(
            events.last(),
            Some(crate::run::RunEvent {
                kind: crate::run::EventKind::RunFinished,
                outcome: Some(outcome),
                ..
            }) if outcome == "blocked"
        ));
    }

    #[test]
    fn backend_failure_exhaustion_finishes_terminal_blocked_with_evidence() {
        let (temp, paths, config, bursar) = plan_fixture("backend-exhaustion");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            implementation_request(initialized_repo(&temp)),
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let author = FailingAuthor(std::sync::Mutex::new(0));

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err());
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err());
        assert_eq!(*author.0.lock().expect("calls"), 2);
        assert!(
            matches!(
                crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id)
                    .expect("run")
                    .plan()
                    .expect("plan")
                    .progress,
                crate::run::PlanProgress::Terminal {
                    verdict: crate::run::PlanTerminalVerdict::Blocked
                }
            ),
            "backend retry exhaustion must not leave a resumable authoring manifest"
        );
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err());
        assert_eq!(
            *author.0.lock().expect("calls"),
            2,
            "terminal resume must make no additional backend call"
        );
        let rows = plan_ledger_rows(&paths.ledger_path);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row["outcome"] == "failed"));
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let events = crate::run::read_events(&run.events_path()).expect("events");
        assert!(matches!(
            events.last(),
            Some(crate::run::RunEvent {
                kind: crate::run::EventKind::RunFinished,
                outcome: Some(outcome),
                ..
            }) if outcome == "blocked"
        ));
    }

    #[test]
    fn dispatch_without_exact_later_approval_never_calls_author() {
        let (temp, paths, config, bursar) = plan_fixture("approval-negative");
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        let author = CapturingAuthor {
            output: b"unused".to_vec(),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err());
        assert!(author.calls.lock().expect("calls").is_empty());
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "awaiting_approval"
        );
    }

    #[test]
    fn provider_block_before_authorship_is_cancellable() {
        let (temp, paths, config, fake) = plan_fixture("blocked-cancel");
        let bursar = StatusSwitchBursar::from_fake(&fake);
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        for provider in ["anthropic", "codex", "opencode-go"] {
            bursar.exhaust(provider);
        }
        approve(&paths, &prepared.run_id);
        let author = CapturingAuthor {
            output: b"must not run".to_vec(),
            calls: std::sync::Mutex::new(Vec::new()),
        };

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err());
        assert!(author.calls.lock().expect("calls").is_empty());
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "blocked");
        cancel(&paths, &config, &prepared.run_id).expect("cancel blocked plan");
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "blocked");
    }

    #[test]
    fn cancel_is_forbidden_once_authoring_has_started() {
        let (temp, paths, config, bursar) = plan_fixture("cancel-after-authoring");
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let author = FakeAuthor::new(vec![br#"{"schema":"conductor/plan-document@1","kind":"implementation-plan","title":"Plan","context":"Context","tasks":[{"id":"one","depends_on":[],"targets":[{"file":"src/x.rs","symbol":"x"}],"change":"Change","acceptance":"Accept","verify":"cargo test"}],"risks":[],"assumptions":[]}"#.to_vec()]);

        dispatch(&paths, &config, &bursar, &prepared.run_id, &author).expect("dispatch");

        assert!(cancel(&paths, &config, &prepared.run_id).is_err());
    }

    #[test]
    fn exhausted_author_and_approved_fallbacks_after_authorship_are_terminal_and_noncancellable() {
        let (temp, paths, config, fake) = plan_fixture("started-provider-block");
        let bursar = StatusSwitchBursar::from_fake(&fake);
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let author = FailingAuthor(std::sync::Mutex::new(0));

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err());
        assert_eq!(*author.0.lock().expect("calls"), 1);
        for provider in ["anthropic", "codex", "opencode-go"] {
            bursar.exhaust(provider);
        }

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &author).is_err());
        assert_eq!(
            *author.0.lock().expect("calls"),
            1,
            "terminal provider loss must not replace or invoke an author"
        );
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert!(matches!(
            run.plan().expect("plan").progress,
            crate::run::PlanProgress::Terminal {
                verdict: crate::run::PlanTerminalVerdict::Blocked
            }
        ));
        assert!(cancel(&paths, &config, &prepared.run_id).is_err());
    }
    #[test]
    fn spec_second_opinion_rejection_is_terminal_and_evidenced() {
        let (temp, paths, config, bursar) = plan_fixture("spec-second-reject");
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::Spec,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("spec preparation strengthens the required second opinion");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(spec_document())],
            Vec::new(),
            vec![Ok(peer_approve())],
            vec![Ok(
                br#"{"schema":"conductor/plan-second-opinion@1","verdict":"reject"}"#.to_vec(),
            )],
        );

        dispatch(&paths, &config, &bursar, &prepared.run_id, &executor)
            .expect("strict second opinion executes");

        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "rejected"
        );
        let calls = executor.calls.lock().expect("calls");
        assert_eq!(
            calls
                .iter()
                .map(|(stage, _)| stage.as_str())
                .collect::<Vec<_>>(),
            ["author", "peer", "second"]
        );
        assert_ne!(calls[0].1.execution_key, calls[1].1.execution_key);
        assert_ne!(calls[0].1.provider_id, calls[1].1.provider_id);
        assert_ne!(calls[0].1.provider_id, calls[2].1.provider_id);
        assert_ne!(calls[1].1.provider_id, calls[2].1.provider_id);
        drop(calls);
        assert_successful_plan_ledger_and_events(&paths, &prepared.run_id);
    }

    #[test]
    fn revision_reuses_immutable_author_and_peer_then_exhausts_at_authorized_limit() {
        let (temp, paths, config, bursar) = plan_fixture("same-author-peer");
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            vec![Ok(implementation_document())],
            vec![Ok(peer_revise()), Ok(peer_approve())],
            Vec::new(),
        );

        dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).expect("revision flow");

        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "accepted"
        );
        let calls = executor.calls.lock().expect("calls");
        assert_eq!(
            calls
                .iter()
                .map(|(stage, _)| stage.as_str())
                .collect::<Vec<_>>(),
            ["author", "peer", "revision", "peer"]
        );
        assert_eq!(calls[0].1, calls[2].1, "revision stays on its first author");
        assert_eq!(
            calls[1].1, calls[3].1,
            "a valid peer verdict pins that reviewer"
        );
        assert_ne!(calls[0].1.execution_key, calls[1].1.execution_key);
        drop(calls);

        let (temp, paths, config, bursar) = plan_fixture("revision-exhaustion");
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
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
        let exhausted = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            Vec::new(),
            vec![Ok(peer_revise())],
            Vec::new(),
        );
        dispatch(&paths, &config, &bursar, &prepared.run_id, &exhausted)
            .expect("exhaustion is a terminal completed review");
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "rejected"
        );
        assert_eq!(
            exhausted
                .calls
                .lock()
                .expect("calls")
                .iter()
                .map(|(stage, _)| stage.as_str())
                .collect::<Vec<_>>(),
            ["author", "peer"]
        );
        drop(temp);
    }
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the regression covers malformed repair and durable peer crash-resume boundaries"
    )]
    fn malformed_peer_repair_and_crash_resume_use_only_pinned_candidates() {
        let (temp, paths, config, fake) = plan_fixture("peer-repair-resume");
        let bursar = StatusSwitchBursar::from_fake(&fake);
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let repaired = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            Vec::new(),
            vec![Ok(b"not review json".to_vec()), Ok(peer_approve())],
            Vec::new(),
        );

        dispatch(&paths, &config, &bursar, &prepared.run_id, &repaired)
            .expect("one persisted peer schema repair");
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert_eq!(run.plan().expect("plan").stage_attempts.peer_review, 2);
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "accepted"
        );
        assert_malformed_peer_repair_ledger(&paths);

        let (temp, paths, config, fake) = plan_fixture("peer-resume-degraded");
        let bursar = StatusSwitchBursar::from_fake(&fake);
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let resumed = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            Vec::new(),
            vec![Err("simulated peer crash".to_string()), Ok(peer_approve())],
            Vec::new(),
        );
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &resumed).is_err());
        let failed_rows = std::fs::read_to_string(&paths.ledger_path)
            .expect("ledger after failed backend invocation")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("ledger row"))
            .collect::<Vec<_>>();
        assert_eq!(
            failed_rows.len(),
            2,
            "author and failed peer call are durable"
        );
        assert_eq!(failed_rows[1]["outcome"], "failed");
        assert!(failed_rows[1]["output_sha256"].is_null());
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let progress = run.plan().expect("plan").progress.clone();
        let bound_peer = match &progress {
            crate::run::PlanProgress::AwaitingPeer {
                peer: Some(peer), ..
            } => peer.clone(),
            other => panic!("peer crash must persist one bound reviewer, got {other:?}"),
        };
        let snapshot = crate::bursar::parse_roster_snapshot(
            &std::fs::read(run.dir().join("roster.json")).expect("captured roster"),
        )
        .expect("captured roster");
        let router = crate::role_routing::RoleRouter::with_pinned_snapshot(
            &paths.state_dir,
            crate::role_routing::RoutingPolicy::from_config(&config, &snapshot)
                .expect("routing policy"),
            snapshot,
        )
        .expect("router");
        let binding_id =
            crate::role_routing::RunId::new(format!("{}-peer-review-bind", prepared.run_id))
                .expect("stable peer binding id");
        assert!(matches!(
            router
                .reservation(
                    &crate::role_routing::RoleId::new("plan").expect("role"),
                    crate::role_routing::PlanStage::PeerReview,
                    &binding_id,
                )
                .expect("reservation lookup"),
            Some(crate::role_routing::Reservation {
                state: crate::role_routing::ReservationState::Committed,
                ..
            })
        ));

        dispatch(&paths, &config, &bursar, &prepared.run_id, &resumed)
            .expect("resume must use the durable peer binding");
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "accepted"
        );
        let peer_calls = resumed
            .calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|(stage, _)| stage == "peer")
            .map(|(_, execution)| execution.clone())
            .collect::<Vec<_>>();

        assert_eq!(peer_calls, vec![bound_peer.clone(), bound_peer]);
        drop(temp);
    }
    #[test]
    fn transient_peer_ineligibility_before_repair_does_not_start_a_phantom_attempt() {
        let (temp, paths, config, fake) = plan_fixture("peer-precall-eligibility");
        let repo = initialized_repo(&temp);
        let prepared =
            prepare(&paths, &config, &fake, implementation_request(repo)).expect("prepare");
        approve(&paths, &prepared.run_id);
        let mut run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let approval = load_approval(&run).expect("approval");
        run.start_plan_authoring(approval.author.clone())
            .expect("author binding");
        let artifact = run
            .capture_plan_artifact(
                Path::new("artifacts/plan-document.json"),
                &implementation_document(),
            )
            .expect("artifact");
        run.await_plan_peer(artifact).expect("await peer");
        drop(run);

        // Peer selection probes each of the three pinned candidates. The fourth
        // status check clears the first call; the fifth is the repair preflight.
        let bursar = TransientStatusBursar::from_fake(&fake, 5);
        let executor = ScriptedExecutor::new(
            Vec::new(),
            Vec::new(),
            vec![Ok(b"bad peer JSON".to_vec()), Ok(peer_approve())],
            Vec::new(),
        );

        assert!(
            dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err(),
            "a temporarily unavailable bound peer must stop before the repair call"
        );
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert_eq!(
            run.plan().expect("plan").stage_attempts.peer_review,
            1,
            "only the real malformed peer call may consume capacity"
        );
        assert_eq!(
            executor
                .calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|(stage, _)| stage == "peer")
                .count(),
            1,
            "the unavailable repair must not reach the backend"
        );
        let peer_starts = crate::run::read_events(&run.events_path())
            .expect("events")
            .iter()
            .filter(|event| {
                event.kind == crate::run::EventKind::AttemptStarted
                    && event.plan_invocation.as_ref().is_some_and(|evidence| {
                        evidence.stage == crate::run::PlanStage::PeerReview
                    })
            })
            .count();
        assert_eq!(peer_starts, 1, "no phantom invocation-start evidence");
        drop(run);

        dispatch(&paths, &config, &bursar, &prepared.run_id, &executor)
            .expect("restored eligibility resumes the immutable peer binding");
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "accepted");
        assert_eq!(
            executor
                .calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|(stage, _)| stage == "peer")
                .count(),
            2,
            "exactly the malformed call and its real repair reach the backend"
        );
        assert_eq!(
            plan_ledger_rows(&paths.ledger_path)
                .iter()
                .filter(|row| row["stage"] == "peer-review")
                .map(|row| row["outcome"].as_str())
                .collect::<Vec<_>>(),
            vec![Some("malformed"), Some("returned")]
        );
    }
    #[test]
    fn bound_peer_loss_after_crash_preserves_binding_without_replacement() {
        let (temp, paths, config, fake) = plan_fixture("bound-peer-loss-after-crash");
        let bursar = StatusSwitchBursar::from_fake(&fake);
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,

            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::ImplementationPlan,
                max_plan_revisions: 1,
                require_second_opinion: false,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            Vec::new(),
            vec![Err("simulated bound peer crash".to_string())],
            Vec::new(),
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert!(matches!(
            run.plan().expect("plan").progress,
            crate::run::PlanProgress::AwaitingPeer { peer: Some(_), .. }
        ));
        for provider in ["anthropic", "codex", "opencode-go"] {
            bursar.exhaust(provider);
        }
        let calls_before = executor.calls.lock().expect("calls").len();

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(executor.calls.lock().expect("calls").len(), calls_before);
        assert_eq!(
            status(&paths, &prepared.run_id).expect("status"),
            "awaiting_peer",
            "a transient no-call eligibility failure must remain resumable"
        );
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        let events = crate::run::read_events(&run.events_path()).expect("events");
        assert!(
            !events
                .iter()
                .any(|event| event.outcome.as_deref() == Some("peer_provider_diversity_degraded"))
        );
        assert!(
            !events.iter().any(|event| {
                event.kind == crate::run::EventKind::AttemptFinished
                    && event.outcome.as_deref() == Some("returned")
                    && event
                        .plan_invocation
                        .as_ref()
                        .is_some_and(|evidence| evidence.stage == crate::run::PlanStage::PeerReview)
            }),
            "the unavailable bound peer must not be replaced or produce a successful review"
        );
    }

    #[test]
    fn second_opinion_loss_after_binding_preserves_binding_without_call() {
        let (temp, paths, config, fake) = plan_fixture("unbound-second-exhaustion");
        let bursar = StatusSwitchBursar::from_fake(&fake);
        let repo = initialized_repo(&temp);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo,
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::Spec,
                max_plan_revisions: 1,
                require_second_opinion: true,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(spec_document())],
            Vec::new(),
            vec![Ok(peer_approve())],
            vec![Err("simulated unbound second-opinion crash".to_string())],
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert!(matches!(
            run.plan().expect("plan").progress,
            crate::run::PlanProgress::AwaitingSecondOpinion { .. }
        ));
        for provider in ["anthropic", "codex", "opencode-go"] {
            bursar.exhaust(provider);
        }
        let calls_before = executor.calls.lock().expect("calls").len();

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(executor.calls.lock().expect("calls").len(), calls_before);
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "awaiting_second_opinion");
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert!(
            !crate::run::read_events(&run.events_path())
                .expect("events")
                .iter()
                .any(|event| {
                    event.kind == crate::run::EventKind::AttemptFinished
                        && event.outcome.as_deref() == Some("returned")
                        && event.plan_invocation.as_ref().is_some_and(|evidence| {
                            evidence.stage == crate::run::PlanStage::SecondOpinion
                        })
                }),
            "the exhausted unbound second opinion must not produce a successful invocation"
        );
    }

    #[test]
    fn bound_peer_backend_exhaustion_after_revision_is_terminal_and_nonresumable() {
        let (temp, paths, config, bursar) = plan_fixture("bound-peer-exhaustion");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            implementation_request(initialized_repo(&temp)),
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            vec![Ok(implementation_document())],
            vec![
                Ok(peer_revise()),
                Err("bound peer backend failure".to_string()),
            ],
            Vec::new(),
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        let calls = executor.calls.lock().expect("calls").len();
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "blocked");
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(executor.calls.lock().expect("calls").len(), calls);
        assert_eq!(
            plan_ledger_rows(&paths.ledger_path)
                .iter()
                .map(|row| row["outcome"].as_str())
                .collect::<Vec<_>>(),
            vec![
                Some("returned"),
                Some("returned"),
                Some("returned"),
                Some("failed"),
            ]
        );
    }

    #[test]
    fn peer_backend_attempt_exhaustion_is_terminal_and_nonresumable() {
        let (temp, paths, config, bursar) = plan_fixture("peer-backend-exhaustion");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            implementation_request(initialized_repo(&temp)),
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            Vec::new(),
            vec![
                Err("first peer backend failure".to_string()),
                Err("second peer backend failure".to_string()),
            ],
            Vec::new(),
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        let calls = executor.calls.lock().expect("calls").len();
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "blocked");
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(executor.calls.lock().expect("calls").len(), calls);
        let rows = plan_ledger_rows(&paths.ledger_path);
        assert_eq!(
            rows.iter()
                .map(|row| row["outcome"].as_str())
                .collect::<Vec<_>>(),
            vec![Some("returned"), Some("failed"), Some("failed")]
        );
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert!(matches!(
            crate::run::read_events(&run.events_path())
                .expect("events")
                .last(),
            Some(crate::run::RunEvent {
                kind: crate::run::EventKind::RunFinished,
                outcome: Some(outcome),
                ..
            }) if outcome == "blocked"
        ));
    }

    #[test]
    fn peer_double_malformed_repair_is_terminal_and_nonresumable() {
        let (temp, paths, config, bursar) = plan_fixture("peer-malformed-exhaustion");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            implementation_request(initialized_repo(&temp)),
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            Vec::new(),
            vec![
                Ok(b"bad peer JSON".to_vec()),
                Ok(b"still bad peer JSON".to_vec()),
            ],
            Vec::new(),
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        let calls = executor.calls.lock().expect("calls").len();
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "blocked");
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(executor.calls.lock().expect("calls").len(), calls);
        assert_eq!(
            plan_ledger_rows(&paths.ledger_path)
                .iter()
                .map(|row| row["outcome"].as_str())
                .collect::<Vec<_>>(),
            vec![Some("returned"), Some("malformed"), Some("malformed")]
        );
    }

    #[test]
    fn second_opinion_backend_attempt_exhaustion_is_terminal_and_nonresumable() {
        let (temp, paths, config, bursar) = plan_fixture("second-backend-exhaustion");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo: initialized_repo(&temp),
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::Spec,
                max_plan_revisions: 1,
                require_second_opinion: true,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(spec_document())],
            Vec::new(),
            vec![Ok(peer_approve())],
            vec![
                Err("first second-opinion backend failure".to_string()),
                Err("second second-opinion backend failure".to_string()),
            ],
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        let calls = executor.calls.lock().expect("calls").len();
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "blocked");
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(executor.calls.lock().expect("calls").len(), calls);
        let rows = plan_ledger_rows(&paths.ledger_path);
        assert_eq!(
            rows.iter()
                .map(|row| (row["stage"].as_str(), row["outcome"].as_str()))
                .collect::<Vec<_>>(),
            vec![
                (Some("planner"), Some("returned")),
                (Some("peer-review"), Some("returned")),
                (Some("second-opinion"), Some("failed")),
                (Some("second-opinion"), Some("failed")),
            ]
        );
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert!(matches!(
            crate::run::read_events(&run.events_path())
                .expect("events")
                .last(),
            Some(crate::run::RunEvent {
                kind: crate::run::EventKind::RunFinished,
                outcome: Some(outcome),
                ..
            }) if outcome == "blocked"
        ));
    }

    #[test]
    fn second_opinion_double_malformed_repair_is_terminal_and_nonresumable() {
        let (temp, paths, config, bursar) = plan_fixture("second-malformed-exhaustion");
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            PlanPrepareRequest {
                repo: initialized_repo(&temp),
                input: PlanPrepareInput::Artifact {
                    bytes: b"author this".to_vec(),
                    tier: crate::run::PlanTier::Lead,
                    complexity: crate::run::PlanComplexity::XL,
                },
                output_kind: PlanOutputKind::Spec,
                max_plan_revisions: 1,
                require_second_opinion: true,
            },
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(spec_document())],
            Vec::new(),
            vec![Ok(peer_approve())],
            vec![
                Ok(b"bad second JSON".to_vec()),
                Ok(b"still bad second JSON".to_vec()),
            ],
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        let calls = executor.calls.lock().expect("calls").len();
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "blocked");
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(executor.calls.lock().expect("calls").len(), calls);
        assert_eq!(
            plan_ledger_rows(&paths.ledger_path)
                .iter()
                .map(|row| row["outcome"].as_str())
                .collect::<Vec<_>>(),
            vec![
                Some("returned"),
                Some("returned"),
                Some("malformed"),
                Some("malformed"),
            ]
        );
    }
    fn assert_revision_attempt_exhaustion(
        label: &str,
        revision: Result<Vec<u8>, String>,
        expected_outcome: &str,
    ) {
        let (temp, paths, config, bursar) = plan_fixture(label);
        let prepared = prepare(
            &paths,
            &config,
            &bursar,
            implementation_request(initialized_repo(&temp)),
        )
        .expect("prepare");
        approve(&paths, &prepared.run_id);
        let executor = ScriptedExecutor::new(
            vec![Ok(implementation_document())],
            vec![revision],
            vec![Ok(peer_revise())],
            Vec::new(),
        );

        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        let calls = executor.calls.lock().expect("calls").len();
        assert_eq!(status(&paths, &prepared.run_id).expect("status"), "blocked");
        assert!(dispatch(&paths, &config, &bursar, &prepared.run_id, &executor).is_err());
        assert_eq!(executor.calls.lock().expect("calls").len(), calls);
        assert_eq!(
            plan_ledger_rows(&paths.ledger_path)
                .iter()
                .map(|row| row["outcome"].as_str())
                .collect::<Vec<_>>(),
            vec![Some("returned"), Some("returned"), Some(expected_outcome)]
        );
        let run = crate::run::RunHandle::open(&paths.state_dir, &prepared.run_id).expect("run");
        assert!(matches!(
            crate::run::read_events(&run.events_path())
                .expect("events")
                .last(),
            Some(crate::run::RunEvent {
                kind: crate::run::EventKind::RunFinished,
                outcome: Some(outcome),
                ..
            }) if outcome == "blocked"
        ));
    }

    #[test]
    fn revision_backend_attempt_limit_is_terminal_and_nonresumable() {
        assert_revision_attempt_exhaustion(
            "revision-backend-exhaustion",
            Err("revision backend failure".to_string()),
            "failed",
        );
    }

    #[test]
    fn revision_malformed_attempt_limit_is_terminal_and_nonresumable() {
        assert_revision_attempt_exhaustion(
            "revision-malformed-exhaustion",

            Ok(b"bad revision JSON".to_vec()),
            "malformed",
        );
    }
}
