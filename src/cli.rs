//! subcommand parsing, exit codes (0 ok; 1 cycle had flags/failures; 2 config/env error)

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

use crate::config;

const USAGE: &str = "usage: undertake [--version] [adversarial-review plan --artifact <path> --reviewers <N> [--question <text>] [--models <a,b,...>] [--config <path>]] [adversarial-review dispatch <review-id> [--config <path>]] [config check [--config <path>]] [plan prepare --repo <path> (--bead <id>|--artifact <path> --tier-floor <lead|senior|junior> --complexity <S|M|L|XL>) --output-kind <spec|implementation-plan> [--max-plan-revisions <0..3>] [--require-second-opinion] [--config <path>]] [plan dispatch <run-id> [--config <path>]] [plan status <run-id> [--config <path>]] [plan cancel <run-id> [--config <path>]] [migrate state --from <legacy-root> --to <undertake-root> [--config <path>]] [roster drift [--config <path>]] [route explain --repo <path> --tier-floor <lead|senior|junior> --complexity <S|M|L|XL> [--intent <cheap-work|outside-perspective>] [--json] [--config <path>]] [scan [--json] [--config <path>]] [status] [cycle --dry-run [--repo <name|path>]... [--only <repo>:<issue-id>]... [--config <path>]] [dispatch <cycle-id> [--resume] [--config <path>]] [supersede --repo <path> --source-run <run-id> --source-cycle <cycle-id> --source-bead <id> --source-commit <sha> --replacement-run <run-id> --replacement-cycle <cycle-id> --replacement-bead <id> --replacement-commit <sha>] [work --repo <path> --bead <id> [--config <path>]] [consult --repo <path> --question <text> [--config <path>]]";

/// The dashboard segment of the usage line. Empty in a
/// `--no-default-features` build, where the command does not exist at all;
/// kept separate from [`USAGE`] so that build's usage text stays
/// byte-identical to what it has always printed.
#[cfg(feature = "tui")]
const DASHBOARD_USAGE: &str =
    " [dashboard [--run <run-id>] [--refresh-ms <milliseconds>] [--config <path>]]";
#[cfg(not(feature = "tui"))]
const DASHBOARD_USAGE: &str = "";

const DEFAULT_ADVERSARIAL_QUESTION: &str =
    "What are the highest-risk flaws in this artifact, and what must change before proceeding?";

pub(crate) fn run(args: Vec<String>) -> ExitCode {
    let mut it = args.into_iter();
    match it.next().as_deref() {
        None => {
            print_usage();
            ExitCode::from(2)
        }
        Some("--help" | "-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--version") => {
            println!("undertake {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("adversarial-review") => run_adversarial(&mut it),
        Some("config") => run_config(&mut it),
        Some("consult") => run_consult(&mut it),
        #[cfg(feature = "tui")]
        Some("dashboard") => run_dashboard_command(&mut it),
        Some("cycle") => run_cycle(&mut it),
        Some("dispatch") => run_dispatch(&mut it),
        Some("plan") => run_plan(&mut it),
        Some("migrate") => run_migrate(&mut it),
        Some("roster") => run_roster(&mut it),
        Some("route") => run_route(&mut it),
        Some("scan") => run_scan(&mut it),
        Some("status") => run_status(&mut it),
        Some("supersede") => run_supersede(&mut it),
        Some("work") => run_work(&mut it),
        Some(cmd) => {
            eprintln!("unknown subcommand: {cmd}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdversarialPlanOptions {
    artifact: PathBuf,
    reviewers: usize,
    question: String,
    models: Option<Vec<String>>,
    config: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdversarialDispatchOptions {
    review_id: String,
    config: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdversarialPaths {
    state_root: PathBuf,
    reports_home: PathBuf,
    ledger_path: PathBuf,
}

impl AdversarialPaths {
    fn from_environment() -> Self {
        Self {
            state_root: state_dir().join("adversarial-reviews"),
            reports_home: reports_home(),
            ledger_path: ledger_path(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlanCliTarget {
    Bead(String),
    Artifact(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanPrepareOptions {
    repo: PathBuf,
    target: PlanCliTarget,
    output_kind: crate::plan_job::PlanOutputKind,
    tier: Option<crate::run::PlanTier>,
    complexity: Option<crate::run::PlanComplexity>,
    max_plan_revisions: u8,
    require_second_opinion: bool,
    config: PathBuf,
}

#[expect(
    clippy::too_many_lines,
    reason = "strict flag grammar keeps every duplicate and mutual-exclusion rejection explicit"
)]
fn parse_plan_prepare_options(args: &[String]) -> Result<PlanPrepareOptions, String> {
    let mut repo = None;
    let mut target = None;
    let mut output_kind = None;
    let mut tier = None;
    let mut complexity = None;
    let mut max_plan_revisions = 1;
    let mut revisions_seen = false;
    let mut require_second_opinion = false;
    let mut config = PathBuf::from("undertake.toml");
    let mut config_seen = false;
    let mut it = args.iter();
    while let Some(argument) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .ok_or_else(|| format!("{flag} requires a value"))
                .cloned()
        };
        match argument.as_str() {
            "--repo" => {
                let value = value("--repo")?;
                if repo.replace(PathBuf::from(value)).is_some() {
                    return Err("--repo may only be supplied once".to_string());
                }
            }
            "--bead" => {
                let value = value("--bead")?;
                if target.replace(PlanCliTarget::Bead(value)).is_some() {
                    return Err(
                        "plan prepare requires exactly one of --bead or --artifact".to_string()
                    );
                }
            }
            "--artifact" => {
                let value = value("--artifact")?;
                if target
                    .replace(PlanCliTarget::Artifact(PathBuf::from(value)))
                    .is_some()
                {
                    return Err(
                        "plan prepare requires exactly one of --bead or --artifact".to_string()
                    );
                }
            }
            "--output-kind" => {
                let value = value("--output-kind")?;
                let parsed = match value.as_str() {
                    "spec" => crate::plan_job::PlanOutputKind::Spec,
                    "implementation-plan" => crate::plan_job::PlanOutputKind::ImplementationPlan,
                    _ => {
                        return Err("--output-kind must be spec or implementation-plan".to_string());
                    }
                };
                if output_kind.replace(parsed).is_some() {
                    return Err("--output-kind may only be supplied once".to_string());
                }
            }
            "--tier-floor" => {
                let value = value("--tier-floor")?;
                let parsed = match value.as_str() {
                    "junior" => crate::run::PlanTier::Junior,
                    "senior" => crate::run::PlanTier::Senior,
                    "lead" => crate::run::PlanTier::Lead,
                    _ => return Err("--tier-floor must be lead, senior, or junior".to_string()),
                };
                if tier.replace(parsed).is_some() {
                    return Err("--tier-floor may only be supplied once".to_string());
                }
            }
            "--complexity" => {
                let value = value("--complexity")?;
                let parsed = match value.as_str() {
                    "S" => crate::run::PlanComplexity::S,
                    "M" => crate::run::PlanComplexity::M,
                    "L" => crate::run::PlanComplexity::L,
                    "XL" => crate::run::PlanComplexity::XL,
                    _ => return Err("--complexity must be S, M, L, or XL".to_string()),
                };
                if complexity.replace(parsed).is_some() {
                    return Err("--complexity may only be supplied once".to_string());
                }
            }
            "--max-plan-revisions" => {
                let value = value("--max-plan-revisions")?;
                if revisions_seen {
                    return Err("--max-plan-revisions may only be supplied once".to_string());
                }
                revisions_seen = true;
                max_plan_revisions = value
                    .parse()
                    .map_err(|_| "--max-plan-revisions must be an integer in 0..=3".to_string())?;
                if max_plan_revisions > 3 {
                    return Err("--max-plan-revisions must be in 0..=3".to_string());
                }
            }
            "--require-second-opinion" => {
                if require_second_opinion {
                    return Err("--require-second-opinion may only be supplied once".to_string());
                }
                require_second_opinion = true;
            }
            "--config" => {
                let value = value("--config")?;
                if config_seen {
                    return Err("--config may only be supplied once".to_string());
                }
                config_seen = true;
                config = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let target = target
        .ok_or_else(|| "plan prepare requires exactly one of --bead or --artifact".to_string())?;
    match (&target, tier, complexity) {
        (PlanCliTarget::Artifact(_), Some(_), Some(_)) | (PlanCliTarget::Bead(_), None, None) => {}
        (PlanCliTarget::Artifact(_), _, _) => {
            return Err("artifact input requires --tier-floor and --complexity".to_string());
        }
        (PlanCliTarget::Bead(_), _, _) => return Err(
            "Bead input derives tier and complexity; do not supply --tier-floor or --complexity"
                .to_string(),
        ),
    }
    let output_kind = output_kind.ok_or_else(|| {
        "plan prepare requires --output-kind <spec|implementation-plan>".to_string()
    })?;
    let require_second_opinion =
        require_second_opinion || output_kind == crate::plan_job::PlanOutputKind::Spec;
    Ok(PlanPrepareOptions {
        repo: repo.ok_or_else(|| "plan prepare requires --repo <path>".to_string())?,
        target,
        output_kind,
        tier,
        complexity,
        max_plan_revisions,
        require_second_opinion,
        config,
    })
}

fn run_plan(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        Some("prepare") => run_plan_prepare(&it.collect::<Vec<_>>()),
        Some("dispatch") => run_plan_dispatch(&it.collect::<Vec<_>>()),
        Some("status") => run_plan_status(&it.collect::<Vec<_>>()),
        Some("cancel") => run_plan_cancel(&it.collect::<Vec<_>>()),
        Some(other) => {
            eprintln!("unknown plan subcommand: {other}");
            ExitCode::from(2)
        }
        None => {
            eprintln!("usage: undertake plan <prepare|dispatch|status|cancel>");
            ExitCode::from(2)
        }
    }
}

fn plan_paths() -> crate::plan_job::PlanJobPaths {
    crate::plan_job::PlanJobPaths {
        state_dir: state_dir(),
        reports_home: reports_home(),
        ledger_path: ledger_path(),
    }
}

fn run_plan_prepare(args: &[String]) -> ExitCode {
    let options = match parse_plan_prepare_options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("plan prepare: {error}");
            return ExitCode::from(2);
        }
    };
    let config = match config::load(&options.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let input = match plan_cli_input(&options) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("plan prepare: {error}");
            return ExitCode::from(2);
        }
    };
    match crate::plan_job::prepare(
        &plan_paths(),
        &config,
        &crate::musterroll::CommandMusterrollClient::new(),
        crate::plan_job::PlanPrepareRequest {
            repo: options.repo,
            input,
            output_kind: options.output_kind,
            max_plan_revisions: options.max_plan_revisions,
            require_second_opinion: options.require_second_opinion,
        },
    ) {
        Ok(prepared) => {
            println!("plan {}: awaiting approval", prepared.run_id);
            println!("report: {}", prepared.report_path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("plan prepare: {error}");
            ExitCode::from(1)
        }
    }
}

fn plan_cli_input(
    options: &PlanPrepareOptions,
) -> Result<crate::plan_job::PlanPrepareInput, String> {
    match &options.target {
        PlanCliTarget::Artifact(path) => {
            let bytes = std::fs::read(path)
                .map_err(|error| format!("read plan artifact {}: {error}", path.display()))?;
            Ok(crate::plan_job::PlanPrepareInput::Artifact {
                bytes,
                tier: options.tier.expect("artifact grammar sets tier"),
                complexity: options
                    .complexity
                    .expect("artifact grammar sets complexity"),
            })
        }
        PlanCliTarget::Bead(bead_id) => {
            let issue = crate::bd::BdClient::show(
                &crate::bd::CommandBdClient::new(),
                &options.repo,
                bead_id,
            )
            .map_err(|error| format!("bd show {bead_id}: {error}"))?;
            let crate::fields::Triage::Triaged(fields) = crate::fields::extract(&issue) else {
                return Err("Bead lacks valid metadata tier_floor and complexity".to_string());
            };
            let tier = match fields.tier_floor {
                crate::config::Tier::Junior => crate::run::PlanTier::Junior,
                crate::config::Tier::Senior => crate::run::PlanTier::Senior,
                crate::config::Tier::Lead => crate::run::PlanTier::Lead,
            };
            let complexity = match fields.complexity {
                crate::config::Ceiling::S => crate::run::PlanComplexity::S,
                crate::config::Ceiling::M => crate::run::PlanComplexity::M,
                crate::config::Ceiling::L => crate::run::PlanComplexity::L,
                crate::config::Ceiling::Xl => crate::run::PlanComplexity::XL,
            };
            let bytes = serde_json::to_vec(&issue)
                .map_err(|error| format!("serialize captured Bead {bead_id}: {error}"))?;
            Ok(crate::plan_job::PlanPrepareInput::Bead {
                bead_id: bead_id.clone(),
                bytes,
                tier,
                complexity,
            })
        }
    }
}

pub(crate) fn parse_plan_run_options(
    args: &[String],
    verb: &str,
) -> Result<(String, PathBuf), String> {
    let Some(run_id) = args.first() else {
        return Err(format!("plan {verb} requires <run-id>"));
    };
    if !valid_cli_review_id(run_id) {
        return Err("plan run id must contain only alphanumeric, '_' or '-' bytes".to_string());
    }
    let mut config = PathBuf::from("undertake.toml");
    if args.len() == 1 {
        return Ok((run_id.clone(), config));
    }
    if args.len() == 3 && args[1] == "--config" {
        config = PathBuf::from(&args[2]);
        return Ok((run_id.clone(), config));
    }
    Err(format!(
        "plan {verb} accepts only <run-id> [--config <path>]"
    ))
}

fn run_plan_status(args: &[String]) -> ExitCode {
    let (run_id, _) = match parse_plan_run_options(args, "status") {
        Ok(options) => options,
        Err(error) => {
            eprintln!("plan status: {error}");
            return ExitCode::from(2);
        }
    };
    match crate::plan_job::status(&plan_paths(), &run_id) {
        Ok(status) => {
            println!("{status}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("plan status: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_plan_cancel(args: &[String]) -> ExitCode {
    let (run_id, config_path) = match parse_plan_run_options(args, "cancel") {
        Ok(options) => options,
        Err(error) => {
            eprintln!("plan cancel: {error}");
            return ExitCode::from(2);
        }
    };
    let config = match config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    match crate::plan_job::cancel(&plan_paths(), &config, &run_id) {
        Ok(()) => {
            println!("canceled");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("plan cancel: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_plan_dispatch(args: &[String]) -> ExitCode {
    let (run_id, config_path) = match parse_plan_run_options(args, "dispatch") {
        Ok(options) => options,
        Err(error) => {
            eprintln!("plan dispatch: {error}");
            return ExitCode::from(2);
        }
    };
    let config = match config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let paths = plan_paths();
    let author = CommandPlanAuthor;
    match crate::plan_job::dispatch(
        &paths,
        &config,
        &crate::musterroll::CommandMusterrollClient::new(),
        &run_id,
        &author,
    ) {
        Ok(()) => match crate::plan_job::status(&paths, &run_id) {
            Ok(state) => {
                println!("{state}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("plan dispatch status: {error}");
                ExitCode::from(1)
            }
        },
        Err(error) => {
            eprintln!("plan dispatch: {error}");
            ExitCode::from(1)
        }
    }
}

struct CommandPlanAuthor;

impl crate::plan_job::PlanAuthor for CommandPlanAuthor {
    fn author(&self, request: &crate::plan_job::PlanAuthorRequest) -> Result<Vec<u8>, String> {
        let prompt = plan_author_prompt(request)?;
        let argv = plan_author_argv(request, &prompt)?;
        let (program, command_args) = argv
            .split_first()
            .ok_or_else(|| "plan author argv was empty".to_string())?;
        let mut command = Command::new(program);
        command.args(command_args).current_dir(&request.worktree);
        let output = crate::dispatch::run_bounded_command(&mut command)
            .map_err(|error| format!("plan author {program}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "plan author {program}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output.stdout)
    }

    fn revise(&self, request: &crate::plan_job::PlanRevisionRequest) -> Result<Vec<u8>, String> {
        let prompt = plan_revision_prompt(request)?;
        run_plan_backend(
            &request.profile,
            &request.execution,
            &prompt,
            &request.worktree,
            "plan revision",
        )
    }

    fn peer_review(
        &self,
        request: &crate::plan_job::PlanPeerReviewRequest,
    ) -> Result<Vec<u8>, String> {
        let prompt = plan_peer_review_prompt(request)?;
        run_plan_backend(
            &request.profile,
            &request.execution,
            &prompt,
            &request.worktree,
            "plan peer review",
        )
    }

    fn second_opinion(
        &self,
        request: &crate::plan_job::PlanSecondOpinionRequest,
    ) -> Result<Vec<u8>, String> {
        let prompt = plan_second_opinion_prompt(request)?;
        run_plan_backend(
            &request.profile,
            &request.execution,
            &prompt,
            &request.worktree,
            "plan second opinion",
        )
    }
}

fn plan_author_prompt(request: &crate::plan_job::PlanAuthorRequest) -> Result<String, String> {
    let contract = crate::plan_job::plan_document_prompt_contract(request.output_kind)?;
    Ok(format!(
        "Return ONLY one strict JSON object: no Markdown fences, commentary, or surrounding text. \
         The `kind` field is required and must exactly match the approved output kind. \
         Use this checked complete JSON shape, replacing example values without adding or omitting fields:\n\
         {contract}\n\
         Plan this immutable input without applying changes:\n{}",
        String::from_utf8_lossy(&request.input)
    ))
}

fn plan_revision_prompt(request: &crate::plan_job::PlanRevisionRequest) -> Result<String, String> {
    let contract = crate::plan_job::plan_document_prompt_contract(request.output_kind)?;
    let findings = serde_json::to_string(&request.findings)
        .map_err(|error| format!("plan revision findings: {error}"))?;
    Ok(format!(
        "Return ONLY one strict JSON object: no Markdown fences, commentary, or surrounding text. \
         The `kind` field is required and must exactly match the approved output kind. \
         Use this checked complete JSON shape, replacing example values without adding or omitting fields:\n\
         {contract}\n\
         Revise this prior immutable plan to address these required peer findings without applying changes.\n\
         Prior plan:\n{}\n\
         Findings:\n{findings}",
        String::from_utf8_lossy(&request.prior_plan),
    ))
}

fn plan_peer_review_prompt(
    request: &crate::plan_job::PlanPeerReviewRequest,
) -> Result<String, String> {
    let target = serde_json::to_string(&request.target)
        .map_err(|error| format!("plan peer target: {error}"))?;
    let contract = crate::plan_job::peer_review_prompt_contract()?;
    Ok(format!(
        "Return ONLY one strict JSON object: no Markdown fences, commentary, or surrounding text. \
         Use one checked shape below. `approve` requires an empty findings array; `revise` requires \
         at least one finding. The serialized examples enumerate every allowed verdict and severity:\n\
         {contract}\n\
         {}\n\
         Target:\n{target}\n\
         Canonical plan:\n{}",
        request.rubric,
        String::from_utf8_lossy(&request.canonical_plan),
    ))
}

fn plan_second_opinion_prompt(
    request: &crate::plan_job::PlanSecondOpinionRequest,
) -> Result<String, String> {
    let target = serde_json::to_string(&request.target)
        .map_err(|error| format!("plan second-opinion target: {error}"))?;
    let contract = crate::plan_job::second_opinion_prompt_contract()?;
    Ok(format!(
        "Return ONLY one strict JSON object: no Markdown fences, commentary, or surrounding text. \
         Use one checked shape below; the serialized examples enumerate every allowed verdict. \
         Independently assess this final canonical plan. Do not discuss any peer verdict.\n\
         {contract}\n\
         Target:\n{target}\n\
         Canonical plan:\n{}",
        String::from_utf8_lossy(&request.canonical_plan),
    ))
}

fn run_plan_backend(
    profile: &crate::musterroll::RosterProfile,
    execution: &crate::run::ApprovedExecution,
    prompt: &str,
    worktree: &Path,
    stage: &str,
) -> Result<Vec<u8>, String> {
    let argv = plan_backend_argv(profile, execution, prompt, worktree)?;
    let (program, command_args) = argv
        .split_first()
        .ok_or_else(|| format!("{stage} argv was empty"))?;
    let mut command = Command::new(program);
    command.args(command_args).current_dir(worktree);
    let output = crate::dispatch::run_bounded_command(&mut command)
        .map_err(|error| format!("{stage} {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{stage} {program}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}
fn plan_author_argv(
    request: &crate::plan_job::PlanAuthorRequest,
    prompt: &str,
) -> Result<Vec<String>, String> {
    plan_backend_argv(
        &request.profile,
        &request.execution,
        prompt,
        &request.worktree,
    )
}

fn plan_backend_argv(
    profile: &crate::musterroll::RosterProfile,
    execution: &crate::run::ApprovedExecution,
    prompt: &str,
    worktree: &Path,
) -> Result<Vec<String>, String> {
    if profile.profile_id != execution.profile_id || profile.provider_id != execution.provider_id {
        return Err("plan profile differs from approved execution".to_string());
    }
    let backend = crate::musterroll::backend_from_harness(&profile.harness)
        .map_err(|error| format!("plan harness: {error}"))?;
    let reasoning_effort = profile
        .reasoning_effort
        .as_deref()
        .map(str::parse::<crate::config::ReasoningEffort>)
        .transpose()
        .map_err(|error| format!("plan reasoning_effort: {error}"))?;
    crate::dispatch::readonly_argv_for_backend(
        backend,
        &profile.dispatch_id,
        reasoning_effort,
        prompt,
        worktree,
    )
    .map_err(|error| format!("plan argv: {error}"))
}
fn run_adversarial(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        Some("plan") => run_adversarial_plan(it),
        Some("dispatch") => run_adversarial_dispatch(it),
        None => {
            eprintln!(
                "usage: undertake adversarial-review <plan --artifact <path> --reviewers <N>|dispatch <review-id>> [options]"
            );
            ExitCode::from(2)
        }
        Some(subcommand) => {
            eprintln!("unknown adversarial-review subcommand: {subcommand}");
            ExitCode::from(2)
        }
    }
}

fn parse_adversarial_plan_options(args: &[String]) -> Result<AdversarialPlanOptions, String> {
    let mut artifact = None;
    let mut reviewers = None;
    let mut question = None;
    let mut models = None;
    let mut config_path = PathBuf::from("undertake.toml");
    let mut config_seen = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--artifact" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--artifact requires a path".to_string())?;
                if artifact.replace(PathBuf::from(value)).is_some() {
                    return Err("--artifact may only be supplied once".to_string());
                }
            }
            "--reviewers" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--reviewers requires an integer".to_string())?;
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| "--reviewers must be a positive integer".to_string())?;
                if parsed == 0 {
                    return Err("--reviewers must be at least 1".to_string());
                }
                if reviewers.replace(parsed).is_some() {
                    return Err("--reviewers may only be supplied once".to_string());
                }
            }
            "--question" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--question requires text".to_string())?;
                if value.trim().is_empty() {
                    return Err("--question must not be empty".to_string());
                }
                if question.replace(value.clone()).is_some() {
                    return Err("--question may only be supplied once".to_string());
                }
            }
            "--models" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--models requires comma-separated roster names".to_string())?;
                let parsed = value
                    .split(',')
                    .map(str::trim)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if parsed.is_empty() || parsed.iter().any(String::is_empty) {
                    return Err(
                        "--models requires non-empty comma-separated roster names".to_string()
                    );
                }
                if models.replace(parsed).is_some() {
                    return Err("--models may only be supplied once".to_string());
                }
            }
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                if config_seen {
                    return Err("--config may only be supplied once".to_string());
                }
                config_seen = true;
                config_path = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let reviewers =
        reviewers.ok_or_else(|| "adversarial-review plan requires --reviewers <N>".to_string())?;
    if let Some(explicit) = &models
        && explicit.len() != reviewers
    {
        return Err(format!(
            "--models contains {} entries; expected {reviewers}",
            explicit.len()
        ));
    }
    Ok(AdversarialPlanOptions {
        artifact: artifact
            .ok_or_else(|| "adversarial-review plan requires --artifact <path>".to_string())?,
        reviewers,
        question: question.unwrap_or_else(|| DEFAULT_ADVERSARIAL_QUESTION.to_string()),
        models,
        config: config_path,
    })
}

fn parse_adversarial_dispatch_options(
    args: &[String],
) -> Result<AdversarialDispatchOptions, String> {
    let Some(review_id) = args.first() else {
        return Err("adversarial-review dispatch requires <review-id>".to_string());
    };
    if !valid_cli_review_id(review_id) {
        return Err("review id must contain only alphanumeric, '_' or '-' bytes".to_string());
    }
    let mut config_path = PathBuf::from("undertake.toml");
    let mut config_seen = false;
    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                if config_seen {
                    return Err("--config may only be supplied once".to_string());
                }
                config_seen = true;
                config_path = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(AdversarialDispatchOptions {
        review_id: review_id.clone(),
        config: config_path,
    })
}

fn valid_cli_review_id(review_id: &str) -> bool {
    let mut bytes = review_id.bytes();
    !review_id.is_empty()
        && review_id.len() <= 128
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn run_adversarial_plan(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args = it.collect::<Vec<_>>();
    let options = match parse_adversarial_plan_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("adversarial-review plan: {error}");
            return ExitCode::from(2);
        }
    };
    let cfg = match config::load(&options.config) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    if options.reviewers > cfg.adversarial_review.max_reviewers as usize {
        eprintln!(
            "adversarial-review plan: --reviewers must be between 1 and {}",
            cfg.adversarial_review.max_reviewers
        );
        return ExitCode::from(2);
    }
    let paths = AdversarialPaths::from_environment();
    let musterroll = crate::musterroll::CommandMusterrollClient::new();
    let validator = crate::deck::CommandDeckValidator::new();
    let review_id = new_adversarial_review_id();
    let created_at = chrono::Utc::now().to_rfc3339();
    match execute_adversarial_plan(
        &cfg,
        &options,
        &paths,
        &musterroll,
        &validator,
        &review_id,
        &created_at,
    ) {
        Ok(published) => {
            println!(
                "adversarial-review plan {}: awaiting approval",
                published.plan.review_id
            );
            println!(
                "calls: nominal {}, worst-case {}",
                published.plan.limits.nominal_calls, published.plan.limits.worst_case_calls
            );
            println!(
                "state: {}",
                paths.state_root.join(&published.plan.review_id).display()
            );
            println!("report: {}", published.report_path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("adversarial-review plan: {error}");
            ExitCode::from(1)
        }
    }
}

fn execute_adversarial_plan<C, V>(
    cfg: &crate::config::Config,
    options: &AdversarialPlanOptions,
    paths: &AdversarialPaths,
    musterroll: &C,
    validator: &V,
    review_id: &str,
    created_at: &str,
) -> Result<crate::adversarial::PublishedApproval, String>
where
    C: crate::musterroll::MusterrollClient + ?Sized,
    V: crate::deck::DeckValidator,
{
    let provider_snapshot = adversarial_provider_snapshot(cfg, musterroll);
    let panel = crate::adversarial::plan_panel(
        &cfg.roster,
        &cfg.adversarial_review,
        &provider_snapshot,
        options.reviewers,
        options.models.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    let snapshot =
        crate::adversarial::snapshot_artifact(&options.artifact, &paths.state_root, review_id)
            .map_err(|error| error.to_string())?;
    crate::adversarial::publish_approval_plan(
        crate::adversarial::ApprovalPlanRequest {
            snapshot: &snapshot,
            roster: &cfg.roster,
            config: &cfg.adversarial_review,
            provider_snapshot: &provider_snapshot,
            panel,
            question: &options.question,
            created_at,
            deck_home: &paths.reports_home,
        },
        validator,
    )
    .map_err(|error| error.to_string())
}

fn run_adversarial_dispatch(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args = it.collect::<Vec<_>>();
    let options = match parse_adversarial_dispatch_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("adversarial-review dispatch: {error}");
            return ExitCode::from(2);
        }
    };
    let cfg = match config::load(&options.config) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let paths = AdversarialPaths::from_environment();
    let musterroll = crate::musterroll::CommandMusterrollClient::new();
    let exec = crate::dispatch::CommandExec;
    let result = execute_adversarial_dispatch(&cfg, &options, &paths, &musterroll, &exec);
    match &result {
        Ok(run) => {
            let outcome = match run.outcome {
                crate::adversarial::ReviewLifecycleOutcome::Complete => "complete",
                crate::adversarial::ReviewLifecycleOutcome::Partial => "partial",
            };
            println!(
                "adversarial-review dispatch {}: {outcome}",
                options.review_id
            );
            println!("report: {}", run.report_path.display());
            for failure in &run.failures {
                eprintln!("failure: {failure}");
            }
        }
        Err(error) => eprintln!("adversarial-review dispatch {}: {error}", options.review_id),
    }
    adversarial_dispatch_result_exit_code(&result)
}

#[expect(
    clippy::too_many_lines,
    reason = "one production-path function threads run creation, reviewer dispatch, and judge \
              synthesis through a single durable run handle; splitting it would scatter that \
              sequencing"
)]
fn execute_adversarial_dispatch<C, E>(
    cfg: &crate::config::Config,
    options: &AdversarialDispatchOptions,
    paths: &AdversarialPaths,
    musterroll: &C,
    exec: &E,
) -> Result<crate::adversarial::AdversarialRun, String>
where
    C: crate::musterroll::MusterrollClient + ?Sized,
    E: crate::dispatch::Exec + Sync,
{
    let review_dir = paths.state_root.join(&options.review_id);
    let plan =
        crate::adversarial::load_review_plan(&review_dir).map_err(|error| error.to_string())?;
    let artifact_path = PathBuf::from(plan.artifact_source_path());
    let resolved_roster = crate::musterroll::resolve_roster(cfg, musterroll)
        .map_err(|error| format!("musterroll roster snapshot: {error}"))?;
    let provider_snapshot = adversarial_provider_snapshot(cfg, musterroll);
    let authorized = crate::adversarial::authorize_approved_execution(
        &review_dir,
        &paths.reports_home,
        &artifact_path,
        &cfg.roster,
        &cfg.adversarial_review,
        &provider_snapshot,
    )
    .map_err(|error| error.to_string())?;
    let approved_profiles = adversarial_approved_profiles(&authorized.plan);
    let approval = serde_json::json!({
        "schema": "undertake/review-approval@1",
        "decision": "approved",
        "plan": &authorized.plan,
    });
    let run_state_dir = paths.state_root.parent().unwrap_or(&paths.state_root);
    let mut run_artifacts = crate::run::RunHandle::create(
        run_state_dir,
        crate::run::RunJob::Review,
        crate::run::NewRun {
            target: crate::run::RunTarget {
                repo: authorized.plan.artifact_source_path().to_string(),
                bead: None,
            },
            approved_profiles,
            musterroll_roster_artifact: Some(crate::run::ArtifactRef {
                path: resolved_roster.source_artifact.path,
                sha256: resolved_roster.source_artifact.sha256,
            }),
            roster_snapshot: Some(crate::run::RosterSnapshotInput {
                bytes: resolved_roster.snapshot_bytes,
                policy_sha256: resolved_roster.policy_sha256,
            }),
            limits: crate::run::RunLimits {
                item_wall_clock_mins: Some(u64::from(cfg.budgets.item_wall_clock_mins)),
                max_attempts: Some(u64::from(authorized.plan.limits.worst_case_calls)),
            },
            verifier: crate::run::RunVerifier {
                mechanical: None,
                qualitative: Some("adversarial-synthesis-schema".to_string()),
            },
            work: None,
            approval: Some(approval),
        },
    )
    .map_err(|error| format!("run artifact: {error}"))?;
    let calls =
        crate::adversarial::ReviewerCallBudget::new(authorized.plan.limits.worst_case_calls);
    let timeout = std::time::Duration::from_secs(
        u64::from(cfg.budgets.item_wall_clock_mins).saturating_mul(60),
    );
    // Pinned once for the whole run: every reviewer and judge attempt reads
    // the same approved artifact, so this is the run-constant "input" for
    // each attempt's invocation evidence (mirrors plan's `input_bytes`,
    // which is likewise hashed once and reused across every plan stage).
    let review_input_sha256 = format!("{:x}", Sha256::digest(&authorized.artifact_bytes));
    let reviewer_run =
        match crate::adversarial::run_reviewers(&authorized, &cfg.roster, exec, timeout, &calls) {
            Ok(run) => run,
            Err(error) => {
                run_artifacts
                    .finish("reviewer_error")
                    .map_err(|run_error| format!("run artifact: {run_error}"))?;
                return Err(error.to_string());
            }
        };
    record_adversarial_reviewer_events(
        &mut run_artifacts,
        &reviewer_run,
        &cfg.roster,
        &review_input_sha256,
    )?;

    let judge_provider_snapshot = adversarial_provider_snapshot(cfg, musterroll);
    let adversarial_run =
        match crate::adversarial::finalize_review(crate::adversarial::SynthesisRequest {
            authorized: &authorized,
            reviewer_run,
            roster: &cfg.roster,
            judge_provider_snapshot: &judge_provider_snapshot,
            exec,
            timeout,
            calls: &calls,
            ledger_path: &paths.ledger_path,
            deck_home: &paths.reports_home,
        }) {
            Ok(run) => run,
            Err(error) => {
                run_artifacts
                    .finish("synthesis_error")
                    .map_err(|run_error| format!("run artifact: {run_error}"))?;
                return Err(error.to_string());
            }
        };
    record_adversarial_terminal_events(
        &mut run_artifacts,
        &adversarial_run,
        &cfg.roster,
        &review_input_sha256,
    )?;
    Ok(adversarial_run)
}

fn adversarial_approved_profiles(plan: &crate::adversarial::AdversarialReviewPlan) -> Vec<String> {
    let mut profiles = Vec::new();
    for reviewer in &plan.panel.reviewers {
        for model in std::iter::once(&reviewer.model).chain(reviewer.alternatives.iter()) {
            if !profiles.contains(model) {
                profiles.push(model.clone());
            }
        }
    }
    for model in std::iter::once(&plan.panel.judge.model).chain(plan.panel.judge.fallbacks.iter()) {
        if !profiles.contains(model) {
            profiles.push(model.clone());
        }
    }
    profiles
}

fn record_adversarial_reviewer_events(
    run_artifacts: &mut crate::run::RunHandle,
    reviewer_run: &crate::adversarial::ReviewerRun,
    roster: &[crate::config::RosterEntry],
    input_sha256: &str,
) -> Result<(), String> {
    for (index, attempt) in reviewer_run.attempts.iter().enumerate() {
        let execution = adversarial_execution_for(roster, &attempt.model)?;
        run_artifacts
            .append_event(
                crate::run::EventKind::AttemptStarted,
                crate::run::EventInput {
                    profile_id: Some(attempt.model.clone()),
                    outcome: Some(adversarial_reviewer_attempt_kind(attempt.kind).to_string()),
                    invocation: Some(crate::run::InvocationEvidence {
                        stage: "reviewer".to_string(),
                        slot: u32::try_from(attempt.slot).unwrap_or(u32::MAX),
                        attempt: adversarial_reviewer_attempt_number(attempt.kind),
                        execution,
                        input_sha256: input_sha256.to_string(),
                        output_sha256: None,
                        duration_ms: Some(attempt.duration_ms),
                        tokens: None,
                        retry_of: None,
                    }),
                    ..crate::run::EventInput::default()
                },
            )
            .map_err(|error| format!("run artifact: {error}"))?;
        let destination = PathBuf::from(format!(
            "attempts/reviewer-{:03}-slot-{}",
            index + 1,
            attempt.slot
        ));
        let artifact_refs = capture_adversarial_logs(
            run_artifacts,
            &attempt.stdout_path,
            &attempt.stderr_path,
            &destination,
        )?;
        run_artifacts
            .append_event(
                crate::run::EventKind::AttemptFinished,
                crate::run::EventInput {
                    profile_id: Some(attempt.model.clone()),
                    artifact_refs,
                    outcome: Some(adversarial_reviewer_outcome(&attempt.outcome)),
                    ..crate::run::EventInput::default()
                },
            )
            .map_err(|error| format!("run artifact: {error}"))?;
    }
    Ok(())
}

fn record_adversarial_terminal_events(
    run_artifacts: &mut crate::run::RunHandle,
    adversarial_run: &crate::adversarial::AdversarialRun,
    roster: &[crate::config::RosterEntry],
    input_sha256: &str,
) -> Result<(), String> {
    if let Some(attempt) = adversarial_run.judge_attempt.as_ref() {
        let execution = adversarial_execution_for(roster, &attempt.model)?;
        run_artifacts
            .append_event(
                crate::run::EventKind::AttemptStarted,
                crate::run::EventInput {
                    profile_id: Some(attempt.model.clone()),
                    outcome: Some(adversarial_judge_attempt_kind(attempt.kind).to_string()),
                    invocation: Some(crate::run::InvocationEvidence {
                        stage: "judge".to_string(),
                        slot: 0,
                        attempt: adversarial_judge_attempt_number(attempt.kind),
                        execution,
                        input_sha256: input_sha256.to_string(),
                        output_sha256: None,
                        duration_ms: Some(attempt.duration_ms),
                        tokens: None,
                        retry_of: None,
                    }),
                    ..crate::run::EventInput::default()
                },
            )
            .map_err(|error| format!("run artifact: {error}"))?;
        let artifact_refs = capture_adversarial_logs(
            run_artifacts,
            &attempt.stdout_path,
            &attempt.stderr_path,
            PathBuf::from("attempts/judge-001").as_path(),
        )?;
        run_artifacts
            .append_event(
                crate::run::EventKind::AttemptFinished,
                crate::run::EventInput {
                    profile_id: Some(attempt.model.clone()),
                    artifact_refs,
                    outcome: Some(adversarial_judge_outcome(&attempt.outcome)),
                    ..crate::run::EventInput::default()
                },
            )
            .map_err(|error| format!("run artifact: {error}"))?;
    } else {
        run_artifacts
            .append_event(
                crate::run::EventKind::CoverageGap,
                crate::run::EventInput {
                    outcome: Some("adversarial_judge_not_run".to_string()),
                    ..crate::run::EventInput::default()
                },
            )
            .map_err(|error| format!("run artifact: {error}"))?;
    }

    let report_ref = run_artifacts
        .capture_artifact(
            &adversarial_run.report_path,
            PathBuf::from("artifacts/report.json").as_path(),
        )
        .map_err(|error| format!("run artifact: {error}"))?;
    let outcome = match adversarial_run.outcome {
        crate::adversarial::ReviewLifecycleOutcome::Complete => "complete",
        crate::adversarial::ReviewLifecycleOutcome::Partial => "partial",
    };
    run_artifacts
        .append_event(
            crate::run::EventKind::ReviewFinished,
            crate::run::EventInput {
                artifact_refs: vec![report_ref],
                outcome: Some(outcome.to_string()),
                ..crate::run::EventInput::default()
            },
        )
        .map_err(|error| format!("run artifact: {error}"))?;
    run_artifacts
        .finish(outcome)
        .map_err(|error| format!("run artifact: {error}"))
}

fn capture_adversarial_logs(
    run_artifacts: &crate::run::RunHandle,
    stdout: &std::path::Path,
    stderr: &std::path::Path,
    destination: &std::path::Path,
) -> Result<Vec<crate::run::ArtifactRef>, String> {
    [
        (stdout, destination.join("stdout.log")),
        (stderr, destination.join("stderr.log")),
    ]
    .into_iter()
    .filter(|(source, _)| source.is_file())
    .map(|(source, destination)| {
        run_artifacts
            .capture_artifact(source, &destination)
            .map_err(|error| format!("run artifact: {error}"))
    })
    .collect()
}

fn adversarial_reviewer_attempt_kind(
    kind: crate::adversarial::ReviewerAttemptKind,
) -> &'static str {
    match kind {
        crate::adversarial::ReviewerAttemptKind::Initial => "initial",
        crate::adversarial::ReviewerAttemptKind::Repair => "repair",
        crate::adversarial::ReviewerAttemptKind::Fallback => "fallback",
    }
}

/// Attempt number within a reviewer slot's chain: 1 for the initial call,
/// 2 for whichever retry follows (schema repair or provider fallback —
/// they never both occur for the same slot). Mirrors `adversarial.rs`'s own
/// `attempt-{N}.out` log naming for the same attempt.
const fn adversarial_reviewer_attempt_number(kind: crate::adversarial::ReviewerAttemptKind) -> u32 {
    match kind {
        crate::adversarial::ReviewerAttemptKind::Initial => 1,
        crate::adversarial::ReviewerAttemptKind::Repair
        | crate::adversarial::ReviewerAttemptKind::Fallback => 2,
    }
}

/// Attempt number within the judge's chain: 1 for the primary judge, 2 for
/// its fallback.
const fn adversarial_judge_attempt_number(kind: crate::adversarial::JudgeAttemptKind) -> u32 {
    match kind {
        crate::adversarial::JudgeAttemptKind::Primary => 1,
        crate::adversarial::JudgeAttemptKind::Fallback => 2,
    }
}

/// Builds the pinned dispatch identity for one adversarial attempt from its
/// roster entry. `adversarial.rs`'s legacy static roster has no
/// Musterroll-style `availability_key`/`execution_key` pair, so both are
/// derived from the same fields the roster already carries: provider
/// identity doubles as the availability key (this roster has no separate
/// availability grouping), and the dispatch id — already the roster's
/// unique exact-execution identity — stands in for the execution key.
fn adversarial_execution_for(
    roster: &[crate::config::RosterEntry],
    profile_id: &str,
) -> Result<crate::run::ApprovedExecution, String> {
    let entry = roster
        .iter()
        .find(|entry| entry.name == profile_id)
        .ok_or_else(|| format!("no roster entry for adversarial profile {profile_id:?}"))?;
    Ok(crate::run::ApprovedExecution {
        profile_id: entry.name.clone(),
        provider_id: entry.provider.clone(),
        availability_key: entry.provider.clone(),
        execution_key: entry.dispatch_id.clone(),
    })
}

fn adversarial_reviewer_outcome(outcome: &crate::adversarial::ReviewerAttemptOutcome) -> String {
    match outcome {
        crate::adversarial::ReviewerAttemptOutcome::Valid => "schema_valid".to_string(),
        crate::adversarial::ReviewerAttemptOutcome::InvalidSchema { reason, .. } => {
            format!("invalid_schema: {reason}")
        }
        crate::adversarial::ReviewerAttemptOutcome::ProcessFailed(reason) => {
            format!("process_failed: {reason}")
        }
    }
}

fn adversarial_judge_attempt_kind(kind: crate::adversarial::JudgeAttemptKind) -> &'static str {
    match kind {
        crate::adversarial::JudgeAttemptKind::Primary => "primary",
        crate::adversarial::JudgeAttemptKind::Fallback => "fallback",
    }
}

fn adversarial_judge_outcome(outcome: &crate::adversarial::JudgeAttemptOutcome) -> String {
    match outcome {
        crate::adversarial::JudgeAttemptOutcome::Valid => "schema_valid".to_string(),
        crate::adversarial::JudgeAttemptOutcome::InvalidSchema { reason, .. } => {
            format!("invalid_schema: {reason}")
        }
        crate::adversarial::JudgeAttemptOutcome::ProcessFailed(reason) => {
            format!("process_failed: {reason}")
        }
    }
}

fn adversarial_provider_snapshot<C: crate::musterroll::MusterrollClient + ?Sized>(
    cfg: &crate::config::Config,
    musterroll: &C,
) -> std::collections::BTreeMap<String, crate::musterroll::BudgetDecision> {
    crate::musterroll::evaluate_provider_snapshot(
        musterroll,
        cfg.roster.iter().map(|entry| entry.provider.as_str()),
        cfg.budgets.use_musterroll,
    )
}

fn adversarial_dispatch_result_exit_code(
    result: &Result<crate::adversarial::AdversarialRun, String>,
) -> ExitCode {
    match result {
        Ok(run)
            if run.outcome == crate::adversarial::ReviewLifecycleOutcome::Complete
                && run.synthesis.is_some() =>
        {
            ExitCode::SUCCESS
        }
        Ok(_) | Err(_) => ExitCode::from(1),
    }
}

fn new_adversarial_review_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("adversarial-{nanos}-{}", std::process::id())
}

fn run_route(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        Some("explain") => run_route_explain(it),
        None => {
            eprintln!(
                "usage: undertake route explain --repo <path> --tier-floor <lead|senior|junior> --complexity <S|M|L|XL> [--intent <cheap-work|outside-perspective>] [--json] [--config <path>]"
            );
            ExitCode::from(2)
        }
        Some(sub) => {
            eprintln!("unknown route subcommand: {sub}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteExplainOptions {
    repo: PathBuf,
    tier_floor: crate::config::Tier,
    complexity: crate::config::Ceiling,
    intent: Option<crate::route::RouteIntent>,
    json: bool,
    config: PathBuf,
}

fn parse_route_explain_options(args: &[String]) -> Result<RouteExplainOptions, String> {
    let mut repo = None;
    let mut tier_floor = None;
    let mut complexity = None;
    let mut intent = None;
    let mut json = false;
    let mut config_path = PathBuf::from("undertake.toml");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--repo" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--repo requires a path".to_string())?;
                repo = Some(PathBuf::from(value));
            }
            "--tier-floor" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--tier-floor requires lead, senior, or junior".to_string())?;
                tier_floor = Some(
                    value
                        .parse()
                        .map_err(|error: crate::config::ConfigError| error.to_string())?,
                );
            }
            "--complexity" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--complexity requires S, M, L, or XL".to_string())?;
                complexity = Some(
                    value
                        .parse()
                        .map_err(|error: crate::config::ConfigError| error.to_string())?,
                );
            }
            "--intent" => {
                let value = it.next().ok_or_else(|| {
                    "--intent requires cheap-work or outside-perspective".to_string()
                })?;
                intent = Some(
                    value
                        .parse()
                        .map_err(|error: crate::route::RouteError| error.to_string())?,
                );
            }
            "--json" => json = true,
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                config_path = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(RouteExplainOptions {
        repo: repo.ok_or_else(|| "route explain requires --repo <path>".to_string())?,
        tier_floor: tier_floor
            .ok_or_else(|| "route explain requires --tier-floor <value>".to_string())?,
        complexity: complexity
            .ok_or_else(|| "route explain requires --complexity <value>".to_string())?,
        intent,
        json,
        config: config_path,
    })
}

fn run_route_explain(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args: Vec<String> = it.collect();
    let options = match parse_route_explain_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("route explain: {error}");
            return ExitCode::from(2);
        }
    };
    let config = match config::load(&options.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let musterroll = crate::musterroll::CommandMusterrollClient::new();
    let resolved_roster = match crate::musterroll::resolve_roster(&config, &musterroll) {
        Ok(roster) => roster,
        Err(error) => {
            eprintln!("musterroll roster snapshot: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let mut runtime_config = config;
    runtime_config.roster = resolved_roster.roster;
    let output = route_explain_output(&runtime_config, &options, &musterroll);
    println!("{output}");
    ExitCode::SUCCESS
}

fn route_explain_output(
    config: &crate::config::Config,
    options: &RouteExplainOptions,
    musterroll: &dyn crate::musterroll::MusterrollClient,
) -> String {
    let routing = crate::fields::RoutingFields {
        tier_floor: options.tier_floor,
        complexity: options.complexity,
        verify_cmd: None,
        trains_ok: false,
    };
    let advice = crate::route::explain(config, &options.repo, &routing, options.intent, musterroll);
    if options.json {
        serde_json::to_string_pretty(&advice.to_json()).expect("route advice JSON is serializable")
    } else {
        advice.human()
    }
}

fn run_config(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        None => {
            eprintln!("usage: undertake config check [--config <path>]");
            ExitCode::from(2)
        }
        Some("check") => run_config_check(it),
        Some(sub) => {
            eprintln!("unknown config subcommand: {sub}");
            ExitCode::from(2)
        }
    }
}

fn run_config_check(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let mut config_path = PathBuf::from("undertake.toml");
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let Some(p) = it.next() else {
                    eprintln!("--config requires a path argument");
                    return ExitCode::from(2);
                };
                config_path = PathBuf::from(p);
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: invalid — {e}");
            return ExitCode::from(2);
        }
    };
    let musterroll = crate::musterroll::CommandMusterrollClient::new();
    let resolved_roster = match crate::musterroll::resolve_roster(&cfg, &musterroll) {
        Ok(roster) => roster,
        Err(error) => {
            eprintln!("musterroll roster snapshot: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let pinned_snapshot =
        match crate::musterroll::parse_roster_snapshot(&resolved_roster.snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!("musterroll roster snapshot: invalid — {error}");
                return ExitCode::from(2);
            }
        };
    if let Err(error) = crate::plan_job::validate_initial_policy(&cfg, &pinned_snapshot) {
        eprintln!("plan policy preflight: invalid — {error}");
        return ExitCode::from(2);
    }
    println!(
        "plan policy: valid (every author has a provider-distinct peer and pairwise-distinct spec team)"
    );
    println!(
        "config: valid ({} Musterroll profiles; snapshot source {}#{}, policy {})",
        resolved_roster.roster.len(),
        resolved_roster.source_artifact.path,
        resolved_roster.source_artifact.sha256,
        resolved_roster.policy_sha256
    );

    let path_var = std::env::var("PATH").unwrap_or_default();
    let state_dir = home_state_dir();
    let checks = config::preflight_checks(&path_var, state_dir.as_deref());
    let mut all_ok = true;
    for check in &checks {
        let status = if check.ok { "ok" } else { "FAIL" };
        println!("{}: {status} — {}", check.name, check.message);
        if !check.ok {
            all_ok = false;
        }
    }

    // Reports the same classifier `dispatch` runs before Bead claim/attempt
    // mutation/worker spawn (bd `conductor-5p8`), once per distinct backend
    // the resolved roster actually selects, so an unauthenticated or
    // unreadable backend is visible at `config check` time rather than only
    // discovered mid-cycle.
    let mut selected_backends: Vec<crate::config::Backend> = resolved_roster
        .roster
        .iter()
        .map(|entry| entry.backend)
        .collect();
    selected_backends.sort_by_key(|backend| format!("{backend:?}"));
    selected_backends.dedup();
    for backend in selected_backends {
        let name = format!("{backend:?}").to_lowercase();
        if !matches!(backend, crate::config::Backend::Claude) {
            println!("backend auth ({name}): ok — no auth-readiness probe defined for this backend");
            continue;
        }
        match crate::dispatch::default_backend_auth_readiness(backend) {
            crate::dispatch::AuthReadiness::Ready => {
                println!("backend auth ({name}): ok — ready");
            }
            crate::dispatch::AuthReadiness::NotAuthenticated { message } => {
                println!("backend auth ({name}): FAIL — not authenticated: {message}");
                all_ok = false;
            }
            crate::dispatch::AuthReadiness::Unreadable { message } => {
                println!("backend auth ({name}): FAIL — unreadable: {message}");
                all_ok = false;
            }
        }
    }

    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn home_state_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("undertake"),
    )
}

fn reports_home() -> PathBuf {
    std::env::var("UNDERTAKE_REPORTS_HOME").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home)
        },
        PathBuf::from,
    )
}

fn state_dir() -> PathBuf {
    std::env::var("UNDERTAKE_STATE_DIR").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("undertake")
        },
        PathBuf::from,
    )
}

fn ledger_path() -> PathBuf {
    std::env::var("UNDERTAKE_LEDGER_PATH").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home)
                .join(".claude")
                .join("model-bench.jsonl")
        },
        PathBuf::from,
    )
}

/// Local artifact polling defaults (spec § Command contract). `--refresh-ms`
/// governs local polling only; Musterroll's 30-second and Evidence's
/// 300-second floors are the runtime's, not the CLI's.
#[cfg(feature = "tui")]
const DASHBOARD_DEFAULT_REFRESH_MS: u64 = 1000;
#[cfg(feature = "tui")]
const DASHBOARD_MIN_REFRESH_MS: u64 = 250;
#[cfg(feature = "tui")]
const DASHBOARD_MAX_REFRESH_MS: u64 = 60_000;

#[cfg(feature = "tui")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct DashboardOptions {
    run: Option<String>,
    refresh_ms: u64,
    config: PathBuf,
}

/// Parses `dashboard [--run <run-id>] [--refresh-ms <ms>] [--config <path>]`.
/// Duplicate, unknown, valueless, and positional arguments are all errors;
/// the caller turns any error into exit 2.
#[cfg(feature = "tui")]
fn parse_dashboard_options(args: &[String]) -> Result<DashboardOptions, String> {
    let mut run = None;
    let mut refresh_ms = None;
    let mut config = PathBuf::from("undertake.toml");
    let mut config_seen = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--run" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--run requires a run id".to_string())?;
                // The dashboard's own single-normal-component validation, not
                // a second copy of it living in the CLI.
                crate::dashboard::validate_run_id(value).map_err(|error| error.to_string())?;
                if run.replace(value.clone()).is_some() {
                    return Err("--run may only be supplied once".to_string());
                }
            }
            "--refresh-ms" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--refresh-ms requires a millisecond count".to_string())?;
                let parsed = value.parse::<u64>().map_err(|_| {
                    format!(
                        "--refresh-ms must be an integer between {DASHBOARD_MIN_REFRESH_MS} and {DASHBOARD_MAX_REFRESH_MS}"
                    )
                })?;
                if !(DASHBOARD_MIN_REFRESH_MS..=DASHBOARD_MAX_REFRESH_MS).contains(&parsed) {
                    return Err(format!(
                        "--refresh-ms {parsed} is outside {DASHBOARD_MIN_REFRESH_MS}..={DASHBOARD_MAX_REFRESH_MS}"
                    ));
                }
                if refresh_ms.replace(parsed).is_some() {
                    return Err("--refresh-ms may only be supplied once".to_string());
                }
            }
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                if config_seen {
                    return Err("--config may only be supplied once".to_string());
                }
                config_seen = true;
                config = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(DashboardOptions {
        run,
        refresh_ms: refresh_ms.unwrap_or(DASHBOARD_DEFAULT_REFRESH_MS),
        config,
    })
}

/// Turns a validated `--run` id into a selection, or reports why it cannot
/// be one. Delegates to the run source's own preflight so the CLI and a
/// refresh tick agree on what "unknown run id" means.
#[cfg(feature = "tui")]
fn dashboard_selection(
    run: Option<&str>,
    config: &crate::dashboard::RunSourceConfig,
) -> Result<crate::dashboard::RunSelection, String> {
    let selection = run.map_or(crate::dashboard::RunSelection::Newest, |run_id| {
        crate::dashboard::RunSelection::Explicit(run_id.to_string())
    });
    crate::dashboard::preflight_run_selection(config, &selection)
        .map_err(|error| error.to_string())?;
    Ok(selection)
}

/// `q`, SIGTERM/SIGHUP, and terminal loss after startup exit 0; terminal
/// setup failure and unrelated runtime I/O failures exit 1.
#[cfg(feature = "tui")]
fn dashboard_exit_code(result: &std::io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

/// Runs the read-only dashboard.
///
/// Everything this constructs is a reader: a validated config, the two
/// existing root resolvers (`state_dir`/`reports_home`, including
/// `UNDERTAKE_STATE_DIR`/`UNDERTAKE_REPORTS_HOME`), and a
/// [`crate::dashboard::RunSourceConfig`]. No bd client, no run handle, no
/// lease, no cycle or recovery entry point is reachable from here — the
/// command has no mutation-capable handle to pass on.
#[cfg(feature = "tui")]
fn run_dashboard_command(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args: Vec<String> = it.by_ref().collect();
    let options = match parse_dashboard_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("dashboard: {error}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    if let Err(error) = config::load(&options.config) {
        eprintln!("config: invalid — {error}");
        return ExitCode::from(2);
    }

    let source_config = crate::dashboard::RunSourceConfig {
        state_root: state_dir(),
        reports_home: reports_home(),
        refresh_interval: std::time::Duration::from_millis(options.refresh_ms),
    };
    let selection = match dashboard_selection(options.run.as_deref(), &source_config) {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("dashboard: {error}");
            return ExitCode::from(2);
        }
    };

    let result = crate::dashboard::run_dashboard(source_config, selection);
    if let Err(error) = &result {
        eprintln!("dashboard: {error}");
    }
    dashboard_exit_code(&result)
}

fn run_roster(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        None => {
            eprintln!(
                "usage: undertake roster is owned by Musterroll; use `musterroll roster snapshot --json`"
            );
            ExitCode::from(2)
        }
        Some("drift") => {
            eprintln!(
                "roster drift is retired: Undertake does not parse scorecards; use the pinned Musterroll snapshot"
            );
            ExitCode::from(2)
        }
        Some(sub) => {
            eprintln!("unknown roster subcommand: {sub}");
            ExitCode::from(2)
        }
    }
}

fn run_scan(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let mut json_output = false;
    let mut config_path = PathBuf::from("undertake.toml");
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_output = true,
            "--config" => {
                let Some(p) = it.next() else {
                    eprintln!("--config requires a path argument");
                    return ExitCode::from(2);
                };
                config_path = PathBuf::from(p);
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: invalid — {e}");
            return ExitCode::from(2);
        }
    };

    let client = crate::bd::CommandBdClient::new();
    let snapshots = match crate::scan::scan(&cfg.scan, &client) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scan: {e}");
            return ExitCode::from(2);
        }
    };

    if json_output {
        match serde_json::to_string_pretty(&snapshots) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("json: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        print_scan_table(&snapshots);
    }

    scan_exit_code(&snapshots)
}

fn scan_exit_code(snapshots: &[crate::scan::RepoSnapshot]) -> ExitCode {
    use crate::scan::SkipReason;

    // Ordinary skips (not-beads, excluded, in-progress, not-git) are expected
    // fleet composition, not failures. Only a real ScanGap is reportable.
    let has_scan_gap = snapshots
        .iter()
        .any(|s| matches!(s.skip_reason, Some(SkipReason::ScanGap { .. })));
    if has_scan_gap {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_scan_table(snapshots: &[crate::scan::RepoSnapshot]) {
    use crate::scan::{Freshness, SkipReason, ZeroState};

    let headers = ["REPO", "READY", "ZERO-STATE", "FRESH", "FLAGS"];

    let rows: Vec<[String; 5]> = snapshots
        .iter()
        .map(|s| {
            let ready = if s.is_beads_repo && s.skip_reason.is_none() {
                s.ready.len().to_string()
            } else {
                "-".to_string()
            };

            let zero_state = match s.zero_state {
                ZeroState::Drained => "drained".to_string(),
                ZeroState::Blocked => "blocked".to_string(),
                ZeroState::NotApplicable => "-".to_string(),
            };

            let freshness = if s.is_beads_repo {
                match s.freshness {
                    Freshness::Fresh => "fresh".to_string(),
                    Freshness::Recent => "recent".to_string(),
                    Freshness::Stale => "stale".to_string(),
                    Freshness::Unknown => "unknown".to_string(),
                }
            } else {
                "-".to_string()
            };

            let flags = match &s.skip_reason {
                Some(SkipReason::InProgress) => "in-progress".to_string(),
                Some(SkipReason::Excluded) => "excluded".to_string(),
                Some(SkipReason::NotBeadsRepo) => "not-beads".to_string(),
                Some(SkipReason::NotGitRepo) => "not-git".to_string(),
                Some(SkipReason::ScanGap { .. }) => "scan-gap".to_string(),
                None => "-".to_string(),
            };

            [s.name.clone(), ready, zero_state, freshness, flags]
        })
        .collect();

    let mut widths = [0usize; 5];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect();
    println!("{}", header_line.join("  "));

    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{:<width$}", cell, width = widths[i]))
            .collect();
        println!("{}", line.join("  "));
    }
}

fn run_status(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    // Reject unknown arguments
    if let Some(arg) = it.next() {
        eprintln!("unknown argument: {arg}");
        return ExitCode::from(2);
    }

    let Some(state_dir) = home_state_dir() else {
        eprintln!("status: HOME not set; cannot locate state directory");
        return ExitCode::from(2);
    };

    let journal_path = state_dir.join("journal.json");
    if !journal_path.is_file() {
        println!("no cycles recorded yet");
        println!();
        println!("state directory: {}", state_dir.display());
        return ExitCode::SUCCESS;
    }

    let content = match std::fs::read_to_string(&journal_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("status: cannot read journal: {e}");
            return ExitCode::from(2);
        }
    };

    let journal: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("status: invalid journal: {e}");
            return ExitCode::from(2);
        }
    };

    if let Some(last_cycle) = journal.get("last_cycle") {
        if let Some(id) = last_cycle.get("id").and_then(|v| v.as_str()) {
            println!("last cycle: {id}");
        }
        if let Some(ts) = last_cycle.get("completed_at").and_then(|v| v.as_str()) {
            println!("completed:  {ts}");
        }
        if let Some(summary) = last_cycle.get("summary").and_then(|v| v.as_object()) {
            let scanned = summary
                .get("scanned")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let ready = summary
                .get("ready")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let dispatched = summary
                .get("dispatched")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let verified = summary
                .get("verified")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let flagged = summary
                .get("flagged")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            println!(
                "summary:    scanned={scanned} ready={ready} dispatched={dispatched} verified={verified} flagged={flagged}"
            );
        }
    } else {
        println!("no cycles recorded yet");
    }

    println!();
    println!("state directory: {}", state_dir.display());
    ExitCode::SUCCESS
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkOptions {
    repo: PathBuf,
    bead: String,
    config: PathBuf,
}

fn parse_work_options(args: &[String]) -> Result<WorkOptions, String> {
    let mut repo = None;
    let mut bead = None;
    let mut config_path = PathBuf::from("undertake.toml");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--repo" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--repo requires a path".to_string())?;
                repo = Some(PathBuf::from(value));
            }
            "--bead" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--bead requires an id".to_string())?;
                bead = Some(value.clone());
            }
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                config_path = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(WorkOptions {
        repo: repo.ok_or_else(|| "work requires --repo <path>".to_string())?,
        bead: bead.ok_or_else(|| "work requires --bead <id>".to_string())?,
        config: config_path,
    })
}

/// `undertake work` — the first job migrated onto the generic
/// [`crate::runner::AttemptRunner`] (bead `conductor-vd3y`). Reads its
/// profile pool, fallback order, limits, verifier policy,
/// `approval_required`, and mutation posture from the validated
/// [`crate::job::JobRegistry`]'s `work` binding; a missing or invalid
/// binding is a fail-closed refusal with an actionable diagnostic, never a
/// built-in default.
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence: parse options, load and validate the closed job \
              registry, resolve the roster, fetch and triage the bead, create the run, \
              and dispatch it — splitting it would scatter the fail-closed checks each \
              step depends on the previous one having already passed"
)]
fn run_work(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args: Vec<String> = it.collect();
    let options = match parse_work_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("work: {error}");
            return ExitCode::from(2);
        }
    };
    let cfg = match config::load(&options.config) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    if cfg.jobs.is_empty() {
        eprintln!(
            "work: no [[job]] bindings configured in {}; the closed registry requires a \
             work, review, consult, and plan [[job]] entry (see .docs/ai/phases/\
             undertake-runner-contract.md) before `undertake work` can run",
            options.config.display()
        );
        return ExitCode::from(2);
    }
    let registry = match crate::job::JobRegistry::new(cfg.jobs.clone()) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!(
                "work: invalid [[job]] configuration in {} — {error}",
                options.config.display()
            );
            return ExitCode::from(2);
        }
    };
    let Some(binding) = registry.binding(crate::run::RunJob::Work) else {
        eprintln!(
            "work: no [[job]]\nkind = \"work\"\nbinding in {}",
            options.config.display()
        );
        return ExitCode::from(2);
    };

    let musterroll = crate::musterroll::CommandMusterrollClient::new();
    let snapshot = match crate::musterroll::MusterrollClient::roster_snapshot(&musterroll) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("work: musterroll roster snapshot unavailable — {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = registry.validate_pinned_profiles(&snapshot) {
        eprintln!("work: {error}");
        return ExitCode::from(2);
    }
    let (candidates, dispatch_facts) =
        match crate::work_policy::resolve_candidates(binding, &snapshot) {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!("work: {error}");
                return ExitCode::from(2);
            }
        };

    // Bootstrap deadlock (bead `conductor-bxb`): if every pinned profile is
    // `Unknown`, the pool above comes up empty and no ordinary dispatch can
    // ever generate the evidence that would change that. Before the bead is
    // claimed or the repo is touched, probe any Unknown+enabled pinned
    // profile with a bounded, tools-disabled, read-only invocation; a
    // validated probe appends exact-scope runtime-success evidence via
    // Musterroll and the pool is re-resolved against a fresh snapshot. See
    // `.docs/ai/phases/undertake-runner-contract.md`'s bootstrap-probe
    // section and the bead's own pinned design.
    let (candidates, dispatch_facts, roster_snapshot) = if candidates.is_empty() {
        let outcome = match crate::probe::resolve_with_bootstrap_probe(
            &state_dir(),
            &options.repo.display().to_string(),
            &options.bead,
            binding,
            &snapshot,
            &crate::dispatch::CommandExec,
            &musterroll,
            std::time::Duration::from_secs(90),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("work: bootstrap probe failed — {error}");
                return ExitCode::from(1);
            }
        };
        if !outcome.probed.is_empty() {
            let validated = outcome
                .probed
                .iter()
                .filter(|report| matches!(report.verdict, crate::probe::ProbeVerdict::Validated))
                .count();
            println!(
                "work: bootstrap probe attempted {} profile(s), {validated} validated",
                outcome.probed.len()
            );
        }
        (outcome.candidates, outcome.dispatch_facts, outcome.snapshot)
    } else {
        (candidates, dispatch_facts, snapshot)
    };

    if candidates.is_empty() {
        // No eligible profile, and either nothing was Unknown+enabled to
        // probe or probing did not produce one. Stop here — before any bead
        // claim or repo mutation, per the bead's preflight ordering.
        println!(
            "work {}: Blocked — no eligible profile in the work job's pinned pool",
            options.bead
        );
        return ExitCode::from(1);
    }

    let bd_client = crate::bd::CommandBdClient::new();
    let issue = match crate::bd::BdClient::show(&bd_client, &options.repo, &options.bead) {
        Ok(issue) => issue,
        Err(error) => {
            eprintln!("work: bd show {}: {error}", options.bead);
            return ExitCode::from(1);
        }
    };
    let verify_cmd = match crate::fields::extract(&issue) {
        crate::fields::Triage::Triaged(fields) => match fields.verify_cmd {
            Some(cmd) if !cmd.trim().is_empty() => cmd,
            _ => {
                eprintln!("work: issue {} has no verify_cmd", options.bead);
                return ExitCode::from(2);
            }
        },
        crate::fields::Triage::Untriaged { missing } => {
            eprintln!(
                "work: issue {} is untriaged (missing {missing:?}); undertake work only \
                 dispatches triaged items",
                options.bead
            );
            return ExitCode::from(2);
        }
    };

    let commits = crate::dispatch::GitCommitProbe;
    let before_head = match crate::dispatch::CommitProbe::head(&commits, &options.repo) {
        Ok(Some(head)) => head,
        Ok(None) => {
            eprintln!(
                "work: repository {} has no commits",
                options.repo.display()
            );
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("work: git head: {error}");
            return ExitCode::from(1);
        }
    };

    let authorization_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(options.bead.as_bytes());
        hasher.update(options.repo.display().to_string().as_bytes());
        for candidate in &candidates {
            hasher.update(candidate.profile_id.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    };
    let approval = serde_json::json!({
        "schema": "undertake/work-direct-approval@1",
        "approval_required": binding.approval_required,
        "repo": options.repo.display().to_string(),
        "bead": options.bead,
    });

    // `AttemptRunner::run` checks readiness for one backend up front, before
    // any stage runs; `work`'s pool can span several, so the primary (first
    // pinned, still-eligible) candidate's backend stands in for the run-wide
    // preflight. An empty pool falls back to `Pi` only because the request
    // still needs some value — the run ends `Blocked` on an empty pool
    // regardless of what this check reports.
    let request_backend = candidates
        .first()
        .and_then(|candidate| dispatch_facts.get(&candidate.profile_id))
        .map_or(crate::config::Backend::Pi, |facts| facts.backend);

    let attempt_budget = binding
        .limits
        .max_attempts
        .and_then(|value| u8::try_from(value).ok())
        .and_then(|value| crate::run::StageAttemptLimit::new(value).ok())
        .unwrap_or_else(|| crate::run::StageAttemptLimit::new(1).expect("nonzero"));
    let policy = crate::work_policy::WorkPolicy::new(
        issue,
        verify_cmd.clone(),
        options.repo.clone(),
        candidates,
        attempt_budget,
    );
    let max_attempts =
        u64::from(crate::runner::CallBudget::worst_case(&policy.stage_plan()).ceiling());

    let state = state_dir();
    let mut handle = match crate::run::RunHandle::create(
        &state,
        crate::run::RunJob::Work,
        crate::run::NewRun {
            target: crate::run::RunTarget {
                repo: options.repo.display().to_string(),
                bead: Some(options.bead.clone()),
            },
            approved_profiles: binding.pinned_profile_ids().map(str::to_string).collect(),
            musterroll_roster_artifact: None,
            roster_snapshot: Some(crate::run::RosterSnapshotInput {
                bytes: roster_snapshot.snapshot_bytes().to_vec(),
                policy_sha256: roster_snapshot.policy_sha256().to_string(),
            }),
            limits: crate::run::RunLimits {
                item_wall_clock_mins: binding.limits.item_wall_clock_mins,
                max_attempts: Some(max_attempts),
            },
            verifier: crate::run::RunVerifier {
                mechanical: Some(verify_cmd.clone()),
                qualitative: None,
            },
            work: Some(crate::run::WorkState {
                cycle_id: format!("work-{}", options.bead),
                authorization_sha256,
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
                stage: crate::run::WorkStage::Implementing,
            }),
            approval: Some(approval),
        },
    ) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("work: failed to create run — {error}");
            return ExitCode::from(1);
        }
    };

    let run_id = handle.run_id().to_string();
    let run_dir = handle.dir().to_path_buf();
    let exec = crate::dispatch::CommandExec;
    let recovery = crate::quarantine::GitRepoRecovery;
    let worker_resource_limits = crate::dispatch::WorkerResourceLimits::from_budgets(&cfg.budgets);
    let item_timeout = std::time::Duration::from_secs(
        binding
            .limits
            .item_wall_clock_mins
            .unwrap_or(u64::from(cfg.budgets.item_wall_clock_mins))
            * 60,
    );
    let executor = crate::work_policy::ProductionAttemptExecutor::new(
        &exec,
        &commits,
        &recovery,
        options.repo.clone(),
        run_id,
        options.bead.clone(),
        state.clone(),
        run_dir,
        Some(before_head.clone()),
        dispatch_facts,
        worker_resource_limits,
        item_timeout,
        std::time::Duration::from_secs(5),
    );
    // `WorkPolicy::revalidation_digests()` declares `RosterPolicySha256`
    // (see its own doc comment): `pinned_digests` carries the value just
    // pinned into this run's own manifest above, and `digests` reads that
    // same pinned copy back rather than re-querying live Musterroll, so the
    // check is pinned-vs-pinned and a resumed run with no pinned artifact
    // fails closed (bead `conductor-i9lq`).
    let roster_policy_sha256 = handle.manifest().roster_policy_sha256.clone();
    let digests = crate::work_policy::HeadDigestSource::new(
        &commits,
        options.repo.clone(),
        roster_policy_sha256.clone(),
    );
    let mut pinned_digests = std::collections::BTreeMap::new();
    if let Some(policy_sha256) = roster_policy_sha256 {
        pinned_digests.insert(crate::runner::DigestKind::RosterPolicySha256, policy_sha256);
    }
    let request = crate::runner::RunRequest {
        state_dir: state,
        backend: request_backend,
        owner: "undertake".to_string(),
        pinned_digests,
    };
    let ports = crate::runner::RunnerPorts {
        exec: &exec,
        commits: &commits,
        bd: &bd_client,
        executor: &executor,
        clock: &crate::runner::SystemClock,
        digests: &digests,
    };

    let terminal = match crate::runner::AttemptRunner::run(&policy, &ports, &mut handle, &request)
    {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("work: {error}");
            return ExitCode::from(1);
        }
    };

    if terminal.verdict != crate::run::TerminalVerdict::Completed {
        if let Some(reason) = &terminal.reason {
            let _ = crate::bd::BdClient::comment(
                &bd_client,
                &options.repo,
                &options.bead,
                &format!("undertake work: {reason}"),
            );
        }
    }

    println!(
        "work {}: {:?}{}",
        options.bead,
        terminal.verdict,
        terminal
            .reason
            .as_deref()
            .map(|reason| format!(" — {reason}"))
            .unwrap_or_default()
    );
    if terminal.verdict == crate::run::TerminalVerdict::Completed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsultOptions {
    repo: PathBuf,
    question: String,
    config: PathBuf,
}

fn parse_consult_options(args: &[String]) -> Result<ConsultOptions, String> {
    let mut repo = None;
    let mut question = None;
    let mut config_path = PathBuf::from("undertake.toml");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--repo" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--repo requires a path".to_string())?;
                repo = Some(PathBuf::from(value));
            }
            "--question" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--question requires text".to_string())?;
                question = Some(value.clone());
            }
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                config_path = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(ConsultOptions {
        repo: repo.ok_or_else(|| "consult requires --repo <path>".to_string())?,
        question: question.ok_or_else(|| "consult requires --question <text>".to_string())?,
        config: config_path,
    })
}

/// The accepted answer envelope's captured artifact — the run's product,
/// per the consult job's terminal rule — read back from `handle`'s own
/// journal: the last `stage_finished` event's `artifact_refs` (not
/// `handle.manifest().artifacts`, which accumulates every attempt's output,
/// including any rejected schema-repair attempts).
fn consult_completed_artifact(handle: &crate::run::RunHandle) -> Option<(PathBuf, Option<String>)> {
    let events = crate::run::read_events(&handle.events_path()).ok()?;
    let artifact = events
        .iter()
        .rev()
        .find(|event| event.kind == crate::run::EventKind::StageFinished)
        .and_then(|event| event.artifact_refs.first())?;
    let path = handle.dir().join(&artifact.path);
    let summary = std::fs::read(&path)
        .ok()
        .and_then(|bytes| crate::consult_policy::summarize_envelope(&bytes));
    Some((path, summary))
}

/// `undertake consult` — the `consult` job from the Envoy contract
/// (`.docs/ai/phases/undertake-runner-contract.md`'s consult row: "read-only;
/// explicit ordered profile IDs; terminal rule = evidence-or-gaps answer
/// envelope"). Mirrors `run_work`'s shape: reads the closed
/// [`crate::job::JobRegistry`]'s `consult` binding, pins one Musterroll
/// roster snapshot for both eligibility and the run manifest, and drives a
/// pure [`crate::consult_policy::ConsultPolicy`] through
/// [`crate::runner::AttemptRunner`]. Unlike `work`, consult never claims a
/// bead (no `--bead` argument exists) and runs no bootstrap probe — its
/// binding has `approval_required = false` and an empty pool is simply
/// `Blocked`, not a wedge to break.
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence mirroring run_work: parse options, load and validate the \
              closed job registry, resolve the roster, create the run, and dispatch it — \
              splitting it would scatter the fail-closed checks each step depends on the \
              previous one having already passed"
)]
fn run_consult(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args: Vec<String> = it.collect();
    let options = match parse_consult_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("consult: {error}");
            return ExitCode::from(2);
        }
    };
    let cfg = match config::load(&options.config) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    if cfg.jobs.is_empty() {
        eprintln!(
            "consult: no [[job]] bindings configured in {}; the closed registry requires a \
             work, review, consult, and plan [[job]] entry (see .docs/ai/phases/\
             undertake-runner-contract.md) before `undertake consult` can run",
            options.config.display()
        );
        return ExitCode::from(2);
    }
    let registry = match crate::job::JobRegistry::new(cfg.jobs.clone()) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!(
                "consult: invalid [[job]] configuration in {} — {error}",
                options.config.display()
            );
            return ExitCode::from(2);
        }
    };
    let Some(binding) = registry.binding(crate::run::RunJob::Consult) else {
        eprintln!(
            "consult: no [[job]]\nkind = \"consult\"\nbinding in {}",
            options.config.display()
        );
        return ExitCode::from(2);
    };

    let musterroll = crate::musterroll::CommandMusterrollClient::new();
    let snapshot = match crate::musterroll::MusterrollClient::roster_snapshot(&musterroll) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("consult: musterroll roster snapshot unavailable — {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = registry.validate_pinned_profiles(&snapshot) {
        eprintln!("consult: {error}");
        return ExitCode::from(2);
    }
    let (candidates, dispatch_facts) =
        match crate::work_policy::resolve_candidates(binding, &snapshot) {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!("consult: {error}");
                return ExitCode::from(2);
            }
        };
    if candidates.is_empty() {
        println!("consult: Blocked — no eligible profile in the consult job's pinned pool");
        return ExitCode::from(1);
    }

    let request_backend = candidates
        .first()
        .and_then(|candidate| dispatch_facts.get(&candidate.profile_id))
        .map_or(crate::config::Backend::Pi, |facts| facts.backend);

    let attempt_budget = binding
        .limits
        .max_attempts
        .and_then(|value| u8::try_from(value).ok())
        .and_then(|value| crate::run::StageAttemptLimit::new(value).ok())
        .unwrap_or_else(|| {
            crate::run::StageAttemptLimit::new(crate::consult_policy::DEFAULT_CONSULT_ATTEMPT_BUDGET)
                .expect("nonzero")
        });
    let policy = crate::consult_policy::ConsultPolicy::new(
        options.question.clone(),
        options.repo.clone(),
        candidates,
        attempt_budget,
    );
    let max_attempts =
        u64::from(crate::runner::CallBudget::worst_case(&policy.stage_plan()).ceiling());

    let state = state_dir();
    let mut handle = match crate::run::RunHandle::create(
        &state,
        crate::run::RunJob::Consult,
        crate::run::NewRun {
            target: crate::run::RunTarget {
                repo: options.repo.display().to_string(),
                bead: None,
            },
            approved_profiles: binding.pinned_profile_ids().map(str::to_string).collect(),
            musterroll_roster_artifact: None,
            roster_snapshot: Some(crate::run::RosterSnapshotInput {
                bytes: snapshot.snapshot_bytes().to_vec(),
                policy_sha256: snapshot.policy_sha256().to_string(),
            }),
            limits: crate::run::RunLimits {
                item_wall_clock_mins: binding.limits.item_wall_clock_mins,
                max_attempts: Some(max_attempts),
            },
            verifier: crate::run::RunVerifier::default(),
            work: None,
            approval: None,
        },
    ) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("consult: failed to create run — {error}");
            return ExitCode::from(1);
        }
    };

    let run_dir = handle.dir().to_path_buf();
    let exec = crate::dispatch::CommandExec;
    let commits = crate::dispatch::GitCommitProbe;
    let item_timeout = std::time::Duration::from_secs(
        binding
            .limits
            .item_wall_clock_mins
            .unwrap_or(u64::from(cfg.budgets.item_wall_clock_mins))
            * 60,
    );
    let executor = match crate::consult_policy::ConsultAttemptExecutor::new(
        &exec,
        &commits,
        options.repo.clone(),
        run_dir,
        dispatch_facts,
        item_timeout,
    ) {
        Ok(executor) => executor,
        Err(error) => {
            eprintln!("consult: failed to preflight target repo {}: {error}", options.repo.display());
            return ExitCode::from(1);
        }
    };

    let roster_policy_sha256 = handle.manifest().roster_policy_sha256.clone();
    let digests = crate::consult_policy::ConsultDigestSource::new(roster_policy_sha256.clone());
    let mut pinned_digests = std::collections::BTreeMap::new();
    if let Some(policy_sha256) = roster_policy_sha256 {
        pinned_digests.insert(crate::runner::DigestKind::RosterPolicySha256, policy_sha256);
    }
    let request = crate::runner::RunRequest {
        state_dir: state,
        backend: request_backend,
        owner: "undertake".to_string(),
        pinned_digests,
    };
    let bd_client = crate::bd::CommandBdClient::new();
    let ports = crate::runner::RunnerPorts {
        exec: &exec,
        commits: &commits,
        bd: &bd_client,
        executor: &executor,
        clock: &crate::runner::SystemClock,
        digests: &digests,
    };

    let terminal = match crate::runner::AttemptRunner::run(&policy, &ports, &mut handle, &request)
    {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("consult: {error}");
            return ExitCode::from(1);
        }
    };

    println!(
        "consult {}: {:?}{}",
        options.repo.display(),
        terminal.verdict,
        terminal
            .reason
            .as_deref()
            .map(|reason| format!(" — {reason}"))
            .unwrap_or_default()
    );
    if terminal.verdict == crate::run::TerminalVerdict::Completed {
        if let Some((path, summary)) = consult_completed_artifact(&handle) {
            println!("consult: envelope captured at {}", path.display());
            if let Some(summary) = summary {
                println!("consult: {summary}");
            }
        }
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CycleOptions {
    dry_run: bool,
    config: PathBuf,
    scope: crate::cycle::CycleScopeRequest,
}

fn parse_cycle_options(args: &[String]) -> Result<CycleOptions, String> {
    let mut dry_run = false;
    let mut config_path = PathBuf::from("undertake.toml");
    let mut repos = Vec::new();
    let mut only = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--repo" => repos.push(
                it.next()
                    .ok_or_else(|| "--repo requires a name or path".to_string())?
                    .clone(),
            ),
            "--only" => only.push(
                it.next()
                    .ok_or_else(|| "--only requires <repo>:<issue-id>".to_string())?
                    .clone(),
            ),
            "--config" => {
                config_path = PathBuf::from(
                    it.next()
                        .ok_or_else(|| "--config requires a path argument".to_string())?,
                );
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if !dry_run {
        return Err("only --dry-run is supported in this version".to_string());
    }
    Ok(CycleOptions {
        dry_run,
        config: config_path,
        scope: crate::cycle::CycleScopeRequest { repos, only },
    })
}

fn run_cycle(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args: Vec<String> = it.collect();
    let options = match parse_cycle_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("cycle: {error}");
            return ExitCode::from(2);
        }
    };
    debug_assert!(options.dry_run);

    let cfg = match config::load(&options.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: invalid — {e}");
            return ExitCode::from(2);
        }
    };

    let reports_home = reports_home();
    let state_dir = state_dir();

    let client = crate::bd::CommandBdClient::new();
    let musterroll = crate::musterroll::CommandMusterrollClient::new();
    match crate::cycle::run_dry_run_scoped(
        &cfg,
        &client,
        &musterroll,
        &reports_home,
        &state_dir,
        &options.scope,
    ) {
        Ok(result) => {
            println!("cycle {}: dry-run complete", result.cycle_id);
            println!("report: {}", result.report_path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cycle: {e}");
            if e.is_scope_error() {
                ExitCode::from(2)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

fn run_dispatch(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let Some(cycle_id) = it.next() else {
        eprintln!("usage: undertake dispatch <cycle-id> [--resume] [--config <path>]");
        return ExitCode::from(2);
    };
    let mut config_path = PathBuf::from("undertake.toml");
    let mut resume = false;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let Some(p) = it.next() else {
                    eprintln!("--config requires a path argument");
                    return ExitCode::from(2);
                };
                config_path = PathBuf::from(p);
            }
            "--resume" => {
                resume = true;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: invalid — {e}");
            return ExitCode::from(2);
        }
    };

    let bd = crate::bd::CommandBdClient::new();
    let musterroll = crate::musterroll::CommandMusterrollClient::new();
    let exec = crate::dispatch::CommandExec;
    let commits = crate::dispatch::GitCommitProbe;
    let live = crate::dispatch_cycle::DeckLiveSink;
    let options = crate::dispatch_cycle::DispatchCycleOptions::from_config(&cfg, resume);
    match crate::dispatch_cycle::run_dispatch_cycle(
        &cfg,
        &bd,
        &exec,
        &commits,
        &reports_home(),
        &state_dir(),
        &ledger_path(),
        &cycle_id,
        &options,
        &live,
        &musterroll,
    ) {
        Ok(result) => match result.gate {
            crate::dispatch_cycle::ApprovalGate::Approved => {
                println!(
                    "dispatch {cycle_id}: ran {} item(s), verified {}, failed {}",
                    result.dispatched, result.verified, result.failed
                );
                if result.failed == 0 {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
            crate::dispatch_cycle::ApprovalGate::ChangesRequested => {
                println!("dispatch {cycle_id}: changes requested; cycle closed");
                ExitCode::SUCCESS
            }
        },
        Err(e) if e.is_not_answered() => {
            eprintln!("dispatch {cycle_id}: {e}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("dispatch {cycle_id}: {e}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SupersedeOptions {
    repo: PathBuf,
    pin: crate::dispatch_cycle::SupersessionPin,
}

fn parse_supersede_options(args: &[String]) -> Result<SupersedeOptions, String> {
    let mut repo = None;
    let mut source_run_id = None;
    let mut source_cycle_id = None;
    let mut source_bead = None;
    let mut source_promoted_commit = None;
    let mut replacement_run_id = None;
    let mut replacement_cycle_id = None;
    let mut replacement_bead = None;
    let mut replacement_promoted_commit = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        index += 1;
        match flag {
            "--repo" if repo.is_none() => repo = Some(PathBuf::from(value)),
            "--source-run" if source_run_id.is_none() => source_run_id = Some(value.clone()),
            "--source-cycle" if source_cycle_id.is_none() => source_cycle_id = Some(value.clone()),
            "--source-bead" if source_bead.is_none() => source_bead = Some(value.clone()),
            "--source-commit" if source_promoted_commit.is_none() => {
                source_promoted_commit = Some(value.clone());
            }
            "--replacement-run" if replacement_run_id.is_none() => {
                replacement_run_id = Some(value.clone());
            }
            "--replacement-cycle" if replacement_cycle_id.is_none() => {
                replacement_cycle_id = Some(value.clone());
            }
            "--replacement-bead" if replacement_bead.is_none() => {
                replacement_bead = Some(value.clone());
            }
            "--replacement-commit" if replacement_promoted_commit.is_none() => {
                replacement_promoted_commit = Some(value.clone());
            }
            other => return Err(format!("unknown or duplicate supersede argument: {other}")),
        }
    }
    Ok(SupersedeOptions {
        repo: repo.ok_or_else(|| "supersede requires --repo <path>".to_string())?,
        pin: crate::dispatch_cycle::SupersessionPin {
            source_run_id: source_run_id
                .ok_or_else(|| "supersede requires --source-run <run-id>".to_string())?,
            source_cycle_id: source_cycle_id
                .ok_or_else(|| "supersede requires --source-cycle <cycle-id>".to_string())?,
            source_bead: source_bead
                .ok_or_else(|| "supersede requires --source-bead <id>".to_string())?,
            source_promoted_commit: source_promoted_commit
                .ok_or_else(|| "supersede requires --source-commit <sha>".to_string())?,
            replacement_run_id: replacement_run_id
                .ok_or_else(|| "supersede requires --replacement-run <run-id>".to_string())?,
            replacement_cycle_id: replacement_cycle_id
                .ok_or_else(|| "supersede requires --replacement-cycle <cycle-id>".to_string())?,
            replacement_bead: replacement_bead
                .ok_or_else(|| "supersede requires --replacement-bead <id>".to_string())?,
            replacement_promoted_commit: replacement_promoted_commit
                .ok_or_else(|| "supersede requires --replacement-commit <sha>".to_string())?,
        },
    })
}

/// `undertake supersede`: the explicit, approval-gated operator command for
/// bd `conductor-0kc`. Terminalizes a failed promoted run's Bead once a
/// later, separately approved run verifiably supersedes it — distinct from
/// `dispatch --resume`, which this command never invokes and whose
/// promotion-receipt/HEAD-mismatch recovery-required behavior it never
/// weakens. See `.docs/ai/decisions.md` for the design record.
fn run_supersede(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let options = match parse_supersede_options(&it.collect::<Vec<_>>()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("supersede: {error}");
            return ExitCode::from(2);
        }
    };
    let bd = crate::bd::CommandBdClient::new();
    let commits = crate::dispatch::GitCommitProbe;
    match crate::dispatch_cycle::run_supersession(
        &bd,
        &commits,
        &state_dir(),
        &options.repo,
        &options.pin,
    ) {
        Ok(outcome) if outcome.closed => {
            println!(
                "supersede: closed {} (superseded by verified run {} / Bead {})",
                outcome.source_bead, options.pin.replacement_run_id, outcome.replacement_bead
            );
            ExitCode::SUCCESS
        }
        Ok(outcome) => {
            println!(
                "supersede: {} is already closed by this exact supersession; no-op",
                outcome.source_bead
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("supersede: {error}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MigrateStateOptions {
    source: PathBuf,
    destination: PathBuf,
    config: PathBuf,
}

fn parse_migrate_state_options(args: &[String]) -> Result<MigrateStateOptions, String> {
    if args.first().map(String::as_str) != Some("state") {
        return Err("usage: undertake migrate state --from <legacy-root> --to <undertake-root> [--config <path>]".to_string());
    }
    let mut source = None;
    let mut destination = None;
    let mut config = PathBuf::from("undertake.toml");
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("{flag} requires a path argument"))?;
        index += 1;
        match flag {
            "--from" if source.is_none() => source = Some(PathBuf::from(value)),
            "--to" if destination.is_none() => destination = Some(PathBuf::from(value)),
            "--config" => config = PathBuf::from(value),
            other => return Err(format!("unknown or duplicate migrate argument: {other}")),
        }
    }
    Ok(MigrateStateOptions {
        source: source.ok_or_else(|| "--from is required".to_string())?,
        destination: destination.ok_or_else(|| "--to is required".to_string())?,
        config,
    })
}

fn run_migrate(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let options = match parse_migrate_state_options(&it.collect::<Vec<_>>()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let config = match config::load(&options.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let client = crate::musterroll::CommandMusterrollClient::new();
    let snapshot = match crate::musterroll::MusterrollClient::roster_snapshot(&client) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("migrate state: Musterroll roster snapshot: {error}");
            return ExitCode::from(2);
        }
    };
    let policy = match crate::role_routing::RoutingPolicy::from_config(&config, &snapshot) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("migrate state: role policy: {error}");
            return ExitCode::from(2);
        }
    };
    match crate::state::migrate_live_state(&options.source, &options.destination, &policy) {
        Ok(summary) => {
            println!("state: migrated {} files", summary.files_copied);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("migrate state: {error}");
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!("{USAGE}{DASHBOARD_USAGE}");
}

fn print_help() {
    println!("{USAGE}{DASHBOARD_USAGE}");
    println!();
    println!("Commands:");
    println!("  adversarial-review  Plan or dispatch an approval-gated read-only design review");
    println!("  config check   Validate undertake.toml and run preflight checks");
    #[cfg(feature = "tui")]
    println!("  dashboard      Read-only TUI over one Undertake run and its bounded evidence");
    println!("  plan           Prepare, inspect, dispatch, or cancel a bounded native plan");
    println!("  migrate state  Copy quiescent legacy state into a new Undertake root");
    println!(
        "  roster         Musterroll owns execution profiles; inspect `musterroll roster snapshot --json`"
    );
    println!("  scan           Enumerate fleet repos and snapshot ready work");
    println!("  status         Show the most recently recorded cycle");
    println!("  cycle          Dry-run scan -> triage -> plan and publish a report");
    println!("  dispatch       Dispatch an approved cycle's ready items");
    println!(
        "  supersede      Approval-gated: close a failed promoted run's Bead once a separately"
    );
    println!("                 approved run verifiably supersedes it (see .docs/ai/decisions.md)");
    println!();
    println!("Notes:");
    println!("  adversarial-review dispatch exits 0 only for complete validated synthesis.");
    #[cfg(feature = "tui")]
    println!("  dashboard reads only; it never approves, dispatches, retries, or writes state.");
    println!("  cycle --dry-run still writes a report file even though it makes no bd writes.");
    println!(
        "  dispatch --resume reclaims a bd claim stranded by a crashed undertake process (e.g."
    );
    println!(
        "    kill -9 mid-worker) once its run's heartbeat has gone stale, then retries the item."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::musterroll::Availability;
    use crate::scan::{Freshness, RepoSnapshot, SkipReason, ZeroState};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn single_contract_run(state_dir: &Path) -> PathBuf {
        let mut runs = std::fs::read_dir(crate::run::runs_dir(state_dir))
            .expect("runs dir")
            .map(|entry| entry.expect("run dir entry").path())
            .collect::<Vec<_>>();
        runs.sort();
        assert_eq!(runs.len(), 1, "expected exactly one contract run");
        runs.pop().expect("one run")
    }

    #[test]
    fn parse_consult_options_requires_repo_and_question() {
        let parsed = parse_consult_options(&[
            "--repo".to_string(),
            "/tmp/target-repo".to_string(),
            "--question".to_string(),
            "does this repo use rustfmt?".to_string(),
        ])
        .expect("valid consult options");
        assert_eq!(parsed.repo, PathBuf::from("/tmp/target-repo"));
        assert_eq!(parsed.question, "does this repo use rustfmt?");
        assert_eq!(parsed.config, PathBuf::from("undertake.toml"));

        assert!(parse_consult_options(&["--question".to_string(), "q".to_string()]).is_err());
        assert!(parse_consult_options(&["--repo".to_string(), "/tmp/x".to_string()]).is_err());
    }

    /// A config declaring `work`/`review`/`plan` but no `consult` binding
    /// fails closed before `undertake consult` ever queries Musterroll,
    /// claims a bead, or touches the target repository — the closed
    /// `JobRegistry` (exactly four bindings) rejects it outright.
    #[test]
    fn consult_fails_closed_when_the_job_registry_has_no_consult_binding() {
        let temp = CliTempDir::new("consult-missing-binding");
        let config_path = temp.path().join("undertake.toml");
        std::fs::write(
            &config_path,
            "[[job]]\nkind = \"work\"\nprofile_ids = [\"w\"]\n\
             mutation = \"repository_write\"\napproval_required = true\n\n\
             [[job]]\nkind = \"review\"\nprofile_ids = [\"r\"]\n\
             mutation = \"read_only\"\napproval_required = true\n\n\
             [[job]]\nkind = \"plan\"\nprofile_ids = [\"p\"]\n\
             mutation = \"read_only\"\napproval_required = true\n",
        )
        .expect("write config missing the consult binding");

        let exit = run(vec![
            "consult".to_string(),
            "--repo".to_string(),
            "/tmp/target-repo".to_string(),
            "--question".to_string(),
            "does this repo use rustfmt?".to_string(),
            "--config".to_string(),
            config_path.display().to_string(),
        ]);
        assert_eq!(exit, ExitCode::from(2));
    }

    /// An empty `[[job]]` table (no bindings configured at all) is the same
    /// fail-closed shape, hit even earlier — before `JobRegistry::new` is
    /// ever called.
    #[test]
    fn consult_fails_closed_when_no_job_bindings_are_configured_at_all() {
        let temp = CliTempDir::new("consult-no-jobs-at-all");
        let config_path = temp.path().join("undertake.toml");
        std::fs::write(&config_path, "").expect("write empty config");

        let exit = run(vec![
            "consult".to_string(),
            "--repo".to_string(),
            "/tmp/target-repo".to_string(),
            "--question".to_string(),
            "does this repo use rustfmt?".to_string(),
            "--config".to_string(),
            config_path.display().to_string(),
        ]);
        assert_eq!(exit, ExitCode::from(2));
    }

    #[test]
    fn plan_prepare_parser_requires_one_exact_target_and_artifact_metadata() {
        let parsed = parse_plan_prepare_options(&[
            "--repo".to_string(),
            "/tmp/repo".to_string(),
            "--artifact".to_string(),
            "/tmp/input.md".to_string(),
            "--output-kind".to_string(),
            "implementation-plan".to_string(),
            "--tier-floor".to_string(),
            "senior".to_string(),
            "--complexity".to_string(),
            "L".to_string(),
            "--max-plan-revisions".to_string(),
            "2".to_string(),
            "--require-second-opinion".to_string(),
        ])
        .expect("strict artifact prepare grammar");
        assert_eq!(parsed.max_plan_revisions, 2);
        assert!(parsed.require_second_opinion);
        assert!(
            parse_plan_prepare_options(&[
                "--repo".to_string(),
                "/tmp/repo".to_string(),
                "--bead".to_string(),
                "plan-1".to_string(),
                "--artifact".to_string(),
                "/tmp/input.md".to_string(),
                "--output-kind".to_string(),
                "spec".to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn plan_author_argv_uses_pinned_musterroll_dispatch_identity() {
        let profile: crate::musterroll::RosterProfile = serde_json::from_value(serde_json::json!({
            "profile_id": "planner-pi",
            "provider_id": "opencode-go",
            "model": "glm-5.2",
            "harness": "pi",
            "dispatch_id": "opencode-go/glm-5.2",
            "reasoning_effort": null,
            "tier": "lead",
            "ceiling": "XL",
            "efficiency": "lean",
            "cost": 0.0,
            "data_policy": "standard",
            "enabled": true,
            "roles": ["plan"],
            "state": "healthy",
            "eligible": true,
            "ineligibility_reason": null
        }))
        .expect("roster profile");
        let request = crate::plan_job::PlanAuthorRequest {
            worktree: PathBuf::from("/tmp/plan-author"),
            input: b"immutable input".to_vec(),
            output_kind: crate::plan_job::PlanOutputKind::Spec,
            execution: crate::run::ApprovedExecution {
                profile_id: "planner-pi".to_string(),
                provider_id: "opencode-go".to_string(),
                availability_key: "opencode-go".to_string(),
                execution_key: "ignored".to_string(),
            },
            profile,
        };

        let argv = plan_author_argv(&request, "strict prompt").expect("pinned argv");

        assert_eq!(
            argv,
            vec![
                "pi",
                "--model",
                "opencode-go/glm-5.2",
                "--thinking",
                "xhigh",
                "--no-tools",
                "-p",
                "strict prompt",
            ]
        );
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one production-path regression exercises each independently shaped prompt"
    )]
    fn production_plan_prompts_embed_complete_strict_wire_contracts() {
        let profile: crate::musterroll::RosterProfile = serde_json::from_value(serde_json::json!({
            "profile_id": "planner-pi",
            "provider_id": "opencode-go",
            "model": "glm-5.2",
            "harness": "pi",
            "dispatch_id": "opencode-go/glm-5.2",
            "reasoning_effort": null,
            "tier": "lead",
            "ceiling": "XL",
            "efficiency": "lean",
            "cost": 0.0,
            "data_policy": "standard",
            "enabled": true,
            "roles": ["plan"],
            "state": "healthy",
            "eligible": true,
            "ineligibility_reason": null
        }))
        .expect("roster profile");
        let execution = crate::run::ApprovedExecution {
            profile_id: "planner-pi".to_string(),
            provider_id: "opencode-go".to_string(),
            availability_key: "opencode-go".to_string(),
            execution_key: "planner-pi".to_string(),
        };
        let target = crate::run::PlanTarget {
            repo: "/repo".to_string(),
            input: crate::run::PlanInput::Artifact {
                artifact: crate::run::ArtifactRef {
                    path: "input.md".to_string(),
                    sha256: "a".repeat(64),
                },
                tier: crate::run::PlanTier::Lead,
                complexity: crate::run::PlanComplexity::XL,
            },
        };
        let author = crate::plan_job::PlanAuthorRequest {
            worktree: PathBuf::from("/tmp/plan-author"),
            input: b"immutable input".to_vec(),
            output_kind: crate::plan_job::PlanOutputKind::Spec,
            execution: execution.clone(),
            profile: profile.clone(),
        };
        let author_prompt = plan_author_prompt(&author).expect("author prompt");
        for required in [
            "\"schema\":\"undertake/plan-document@1\"",
            "\"kind\":\"spec\"",
            "\"title\"",
            "\"context\"",
            "\"goals\"",
            "\"constraints\"",
            "\"requirements\"",
            "\"acceptance\"",
            "\"verification\"",
            "\"non_goals\"",
            "\"risks\"",
            "\"assumptions\"",
            "\"open_questions\"",
        ] {
            assert!(
                author_prompt.contains(required),
                "missing {required}: {author_prompt}"
            );
        }
        let implementation = crate::plan_job::PlanAuthorRequest {
            output_kind: crate::plan_job::PlanOutputKind::ImplementationPlan,
            ..author.clone()
        };
        let implementation_prompt =
            plan_author_prompt(&implementation).expect("implementation prompt");
        for required in [
            "\"kind\":\"implementation-plan\"",
            "\"tasks\"",
            "\"id\"",
            "\"depends_on\"",
            "\"targets\"",
            "\"file\"",
            "\"symbol\"",
            "\"change\"",
            "\"acceptance\"",
            "\"verify\"",
        ] {
            assert!(
                implementation_prompt.contains(required),
                "missing {required}: {implementation_prompt}"
            );
        }

        let peer = crate::plan_job::PlanPeerReviewRequest {
            worktree: PathBuf::from("/tmp/plan-peer"),
            target: target.clone(),
            rubric: "Review for correctness.".to_string(),
            canonical_plan: br#"{"kind":"spec"}"#.to_vec(),
            execution: execution.clone(),
            profile: profile.clone(),
        };
        let peer_prompt = plan_peer_review_prompt(&peer).expect("peer prompt");
        for required in [
            "\"schema\":\"undertake/plan-peer-review@1\"",
            "\"verdict\":\"revise\"",
            "\"findings\"",
            "\"id\"",
            "\"severity\"",
            "\"location\"",
            "\"problem\"",
            "\"required_change\"",
            "approve",
            "revise",
            "low",
            "medium",
            "high",
            "critical",
        ] {
            assert!(
                peer_prompt.contains(required),
                "missing {required}: {peer_prompt}"
            );
        }

        let second = crate::plan_job::PlanSecondOpinionRequest {
            worktree: PathBuf::from("/tmp/plan-second"),
            target,
            canonical_plan: br#"{"kind":"spec"}"#.to_vec(),
            execution,
            profile,
        };
        let second_prompt = plan_second_opinion_prompt(&second).expect("second prompt");
        for required in [
            "\"schema\":\"undertake/plan-second-opinion@1\"",
            "\"verdict\":\"accept\"",
            "accept",
            "reject",
        ] {
            assert!(
                second_prompt.contains(required),
                "missing {required}: {second_prompt}"
            );
        }
        assert!(!author_prompt.contains("```"));
        assert!(!peer_prompt.contains("```"));
        assert!(!second_prompt.contains("```"));
    }
    #[test]
    fn plan_prepare_defaults_to_one_revision() {
        let parsed = parse_plan_prepare_options(&[
            "--repo".to_string(),
            "/tmp/repo".to_string(),
            "--bead".to_string(),
            "plan-1".to_string(),
            "--output-kind".to_string(),
            "spec".to_string(),
        ])
        .expect("strict Bead prepare grammar");

        assert_eq!(parsed.max_plan_revisions, 1);
        assert!(
            parsed.require_second_opinion,
            "spec CLI defaults must preserve the required second-opinion gate"
        );
    }
    #[test]
    fn adversarial_plan_and_dispatch_parsers_enforce_exact_grammar() {
        let plan = parse_adversarial_plan_options(&[
            "--artifact".to_string(),
            "/tmp/design.md".to_string(),
            "--reviewers".to_string(),
            "2".to_string(),
            "--question".to_string(),
            "Should this ship?".to_string(),
            "--models".to_string(),
            "reviewer-one,reviewer-two".to_string(),
            "--config".to_string(),
            "/tmp/undertake.toml".to_string(),
        ])
        .expect("exact plan grammar");
        assert_eq!(plan.artifact, PathBuf::from("/tmp/design.md"));
        assert_eq!(plan.reviewers, 2);
        assert_eq!(plan.question, "Should this ship?");
        assert_eq!(
            plan.models,
            Some(vec!["reviewer-one".to_string(), "reviewer-two".to_string()])
        );
        assert_eq!(plan.config, PathBuf::from("/tmp/undertake.toml"));

        let dispatch = parse_adversarial_dispatch_options(&[
            "review-123".to_string(),
            "--config".to_string(),
            "/tmp/undertake.toml".to_string(),
        ])
        .expect("exact dispatch grammar");
        assert_eq!(dispatch.review_id, "review-123");
        assert_eq!(dispatch.config, PathBuf::from("/tmp/undertake.toml"));

        for invalid in [
            vec![],
            vec!["--artifact".to_string(), "/tmp/design.md".to_string()],
            vec![
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
                "--reviewers".to_string(),
                "0".to_string(),
            ],
            vec![
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
                "--reviewers".to_string(),
                "2".to_string(),
                "--models".to_string(),
                ",".to_string(),
            ],
            vec![
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
                "--reviewers".to_string(),
                "2".to_string(),
                "--wide".to_string(),
            ],
        ] {
            assert!(
                parse_adversarial_plan_options(&invalid).is_err(),
                "invalid plan parsed: {invalid:?}"
            );
        }
        assert!(parse_adversarial_dispatch_options(&[]).is_err());
        assert!(
            parse_adversarial_dispatch_options(&[
                "review-123".to_string(),
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn adversarial_usage_and_config_errors_exit_two() {
        assert_eq!(
            run(vec!["adversarial-review".to_string()]),
            ExitCode::from(2)
        );
        assert_eq!(
            run(vec![
                "adversarial-review".to_string(),
                "plan".to_string(),
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
            ]),
            ExitCode::from(2)
        );
        assert_eq!(
            run(vec![
                "adversarial-review".to_string(),
                "dispatch".to_string(),
                "review-123".to_string(),
                "--config".to_string(),
                "/definitely/missing/undertake.toml".to_string(),
            ]),
            ExitCode::from(2)
        );
    }

    #[test]
    fn adversarial_explicit_models_must_match_reviewer_count_before_state_write() {
        let fixture = AdversarialCliFixture::new("cli-explicit-count");
        let mut options = fixture.plan_options();
        options.models = Some(vec!["reviewer-one".to_string()]);

        let error = execute_adversarial_plan(
            &fixture.config,
            &options,
            &fixture.paths,
            &fixture.musterroll,
            &NoopDeckValidator,
            "review-explicit-count",
            "2026-07-15T12:00:00Z",
        )
        .expect_err("one explicit model cannot fill two slots");

        assert!(error.contains("expected 2"));
        assert!(!fixture.paths.state_root.exists());
    }

    #[test]
    fn adversarial_reviewer_upper_bound_exits_two_before_state_write() {
        let fixture = AdversarialCliFixture::new("cli-reviewer-bound");
        let mut options = fixture.plan_options();
        options.reviewers = 4;
        options.models = None;

        let error = execute_adversarial_plan(
            &fixture.config,
            &options,
            &fixture.paths,
            &fixture.musterroll,
            &NoopDeckValidator,
            "review-upper-bound",
            "2026-07-15T12:00:00Z",
        )
        .expect_err("configured reviewer maximum is enforced");

        assert!(error.contains("between 1 and 3"));
        assert!(!fixture.paths.state_root.exists());
        assert_eq!(
            run(vec![
                "adversarial-review".to_string(),
                "plan".to_string(),
                "--artifact".to_string(),
                fixture.artifact.display().to_string(),
                "--reviewers".to_string(),
                "4".to_string(),
                "--config".to_string(),
                fixture.config_path.display().to_string(),
            ]),
            ExitCode::from(2)
        );
    }

    #[test]
    fn adversarial_missing_approval_exits_one() {
        let fixture = AdversarialCliFixture::new("cli-approval-failure");
        let published = fixture.plan("review-approval-failure");
        let exec = CliReviewExec::default();
        let result = execute_adversarial_dispatch(
            &fixture.config,
            &AdversarialDispatchOptions {
                review_id: published.plan.review_id.clone(),
                config: fixture.config_path.clone(),
            },
            &fixture.paths,
            &fixture.musterroll,
            &exec,
        );

        assert_eq!(
            adversarial_dispatch_result_exit_code(&result),
            ExitCode::from(1)
        );
        assert!(result.unwrap_err().contains("awaiting approval"));
        assert!(exec.spawns().is_empty());
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end regression keeps mutation-sentinel, manifest, and event assertions \
                  (including the new invocation-evidence checks) together against one real dispatch"
    )]
    fn adversarial_successful_dispatch_keeps_all_mutation_sentinels_untouched() {
        let fixture = AdversarialCliFixture::new("cli-no-mutation");
        let sentinels = fixture.seed_mutation_sentinels();
        let published = fixture.plan("review-no-mutation");
        fixture.approve(&published.plan);
        let exec = CliReviewExec::default();

        let result = execute_adversarial_dispatch(
            &fixture.config,
            &AdversarialDispatchOptions {
                review_id: published.plan.review_id.clone(),
                config: fixture.config_path.clone(),
            },
            &fixture.paths,
            &fixture.musterroll,
            &exec,
        );

        assert_eq!(
            adversarial_dispatch_result_exit_code(&result),
            ExitCode::SUCCESS
        );
        let run = result.expect("approved fake dispatch completes");
        assert_eq!(
            run.outcome,
            crate::adversarial::ReviewLifecycleOutcome::Complete
        );
        assert!(run.synthesis.is_some());
        assert_eq!(run.report_path, published.report_path);
        assert_eq!(exec.spawns().len(), 3);
        assert!(fixture.paths.ledger_path.is_file());
        assert_eq!(
            std::fs::read_to_string(&fixture.artifact).unwrap(),
            "immutable design"
        );
        for (path, expected) in sentinels {
            assert_eq!(
                std::fs::read(&path).unwrap(),
                expected,
                "mutation sentinel changed: {}",
                path.display()
            );
        }
        for spawn in exec.spawns() {
            assert!(spawn.cwd.starts_with(&fixture.paths.state_root));
            assert!(!spawn.cwd.starts_with(&fixture.target_repo));
        }

        let state_dir = fixture
            .paths
            .state_root
            .parent()
            .expect("adversarial state parent");
        let run_dir = single_contract_run(state_dir);
        let manifest = crate::run::read_manifest(&run_dir.join("manifest.json"))
            .expect("adversarial run manifest");
        assert_eq!(manifest.job, crate::run::RunJob::Review);
        assert!(
            manifest.roster_snapshot.is_some(),
            "every prepared v2 run pins a copied Musterroll roster snapshot"
        );
        assert_eq!(
            manifest.target.repo,
            std::fs::canonicalize(&fixture.artifact)
                .expect("canonical artifact")
                .display()
                .to_string()
        );
        assert_eq!(manifest.lifecycle, crate::run::RunLifecycle::Finished);
        assert_eq!(manifest.outcome.as_deref(), Some("complete"));
        assert!(run_dir.join("approval.json").is_file());
        let events =
            crate::run::read_events(&run_dir.join("events.jsonl")).expect("adversarial run events");
        assert!(events.iter().any(|event| {
            event.kind == crate::run::EventKind::ReviewFinished
                && event.outcome.as_deref() == Some("complete")
        }));
        assert_eq!(
            events.last().map(|event| event.kind),
            Some(crate::run::EventKind::RunFinished)
        );
        let started = events
            .iter()
            .filter(|event| event.kind == crate::run::EventKind::AttemptStarted)
            .collect::<Vec<_>>();
        assert_eq!(started.len(), 3, "two reviewers plus one judge");
        assert!(
            started
                .iter()
                .all(|event| event.invocation.is_some()),
            "every review AttemptStarted must attach generic invocation evidence"
        );
        let stages = started
            .iter()
            .filter_map(|event| event.invocation.as_ref())
            .map(|evidence| evidence.stage.as_str())
            .collect::<Vec<_>>();
        assert_eq!(stages, vec!["reviewer", "reviewer", "judge"]);
        assert!(started.iter().all(|event| {
            let evidence = event.invocation.as_ref().expect("invocation");
            evidence.input_sha256.len() == 64
                && evidence.output_sha256.is_none()
                && !evidence.execution.profile_id.is_empty()
                && !evidence.execution.provider_id.is_empty()
                && evidence.retry_of.is_none()
        }));
    }

    #[test]
    fn adversarial_partial_dispatch_exits_one_without_spawning_judge() {
        let fixture = AdversarialCliFixture::new("cli-partial-exit");
        let published = fixture.plan("review-partial-exit");
        fixture.approve(&published.plan);
        let exec = CliReviewExec::malformed_reviewers();

        let result = execute_adversarial_dispatch(
            &fixture.config,
            &AdversarialDispatchOptions {
                review_id: published.plan.review_id.clone(),
                config: fixture.config_path.clone(),
            },
            &fixture.paths,
            &fixture.musterroll,
            &exec,
        );

        assert_eq!(
            adversarial_dispatch_result_exit_code(&result),
            ExitCode::from(1)
        );
        let run = result.expect("reviewer schema failures produce a partial result");
        assert_eq!(
            run.outcome,
            crate::adversarial::ReviewLifecycleOutcome::Partial
        );
        assert!(run.synthesis.is_none());
        assert!(run.judge_attempt.is_none());
        assert_eq!(exec.spawns().len(), 4);
        let state_dir = fixture
            .paths
            .state_root
            .parent()
            .expect("adversarial state parent");
        let run_dir = single_contract_run(state_dir);
        let manifest = crate::run::read_manifest(&run_dir.join("manifest.json"))
            .expect("partial adversarial manifest");
        assert_eq!(manifest.outcome.as_deref(), Some("partial"));
    }

    #[test]
    fn dispatch_rejects_scope_selectors_that_could_widen_an_approved_plan() {
        assert_eq!(
            run(vec![
                "dispatch".to_string(),
                "cycle-1".to_string(),
                "--repo".to_string(),
                "alpha".to_string(),
            ]),
            ExitCode::from(2)
        );
        assert_eq!(
            run(vec![
                "dispatch".to_string(),
                "cycle-1".to_string(),
                "--only".to_string(),
                "alpha:a-1".to_string(),
            ]),
            ExitCode::from(2)
        );
    }

    #[test]
    fn route_explain_accepts_read_only_provider_advice_arguments() {
        let options = parse_route_explain_options(&[
            "--repo".to_string(),
            "/tmp/chezmoi-personal".to_string(),
            "--tier-floor".to_string(),
            "senior".to_string(),
            "--complexity".to_string(),
            "M".to_string(),
            "--intent".to_string(),
            "outside-perspective".to_string(),
            "--json".to_string(),
            "--config".to_string(),
            "fixture.toml".to_string(),
        ])
        .expect("valid route explain arguments");

        assert_eq!(options.repo, PathBuf::from("/tmp/chezmoi-personal"));
        assert_eq!(options.tier_floor, crate::config::Tier::Senior);
        assert_eq!(options.complexity, crate::config::Ceiling::M);
        assert_eq!(
            options.intent,
            Some(crate::route::RouteIntent::OutsidePerspective)
        );
        assert!(options.json);
        assert_eq!(options.config, PathBuf::from("fixture.toml"));
    }

    #[test]
    fn route_explain_render_path_has_no_scan_bd_or_mutation_seam() {
        let source = include_str!("cli.rs");
        let route_body = source
            .split("fn run_route_explain")
            .nth(1)
            .expect("route command exists")
            .split("\nfn ")
            .next()
            .expect("route command body exists");
        assert!(!route_body.contains("scan::scan"));
        assert!(!route_body.contains("CommandBdClient"));
        assert!(!route_body.contains("claim"));
        assert!(!route_body.contains("dispatch"));
        assert!(!route_body.contains("write"));
    }

    #[test]
    fn route_explain_renders_human_and_json_from_the_shared_advice() {
        let config = crate::config::parse_str(
            r#"
[budgets]
use_musterroll = false

[[roster]]
name = "fixture-model"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "fixture-dispatch"
provider = "fixture-provider"
"#,
        )
        .expect("fixture config parses");
        let human = parse_route_explain_options(&[
            "--repo".to_string(),
            "/tmp/advice-repo".to_string(),
            "--tier-floor".to_string(),
            "senior".to_string(),
            "--complexity".to_string(),
        ])
        .expect_err("incomplete options are rejected");
        assert!(human.contains("--complexity"));

        let options = parse_route_explain_options(&[
            "--repo".to_string(),
            "/tmp/advice-repo".to_string(),
            "--tier-floor".to_string(),
            "senior".to_string(),
            "--complexity".to_string(),
            "M".to_string(),
        ])
        .expect("complete options parse");
        let musterroll = crate::musterroll::test_support::FakeMusterrollClient::unavailable();
        let human = route_explain_output(&config, &options, &musterroll);
        assert!(human.contains("selected: fixture-model"));
        assert!(human.contains("backend=pi"));
        assert!(human.contains("dispatch_id=fixture-dispatch"));
        assert!(human.contains("provider=fixture-provider"));
        assert!(human.contains("action=static-caps"));
        assert!(human.contains("CANDIDATE AUDIT"));

        let json_options = RouteExplainOptions {
            json: true,
            ..options
        };
        let json = route_explain_output(&config, &json_options, &musterroll);
        assert!(json.contains("\"selected\""));
        assert!(json.contains("\"audit\""));
    }

    fn make_snapshot(name: &str, ready_count: usize, skip: Option<SkipReason>) -> RepoSnapshot {
        let is_beads_repo =
            skip != Some(SkipReason::NotBeadsRepo) && skip != Some(SkipReason::Excluded);
        let zero_state = if ready_count == 0 && skip.is_none() {
            ZeroState::Drained
        } else {
            ZeroState::NotApplicable
        };
        let freshness = if skip.is_some() {
            Freshness::Unknown
        } else {
            Freshness::Fresh
        };

        let mut ready = Vec::new();
        for i in 0..ready_count {
            ready.push(crate::bd::Issue {
                id: format!("{name}-{i}"),
                title: format!("Issue {i}"),
                description: String::new(),
                acceptance_criteria: String::new(),
                notes: String::new(),
                status: "open".to_string(),
                priority: 1,
                issue_type: "task".to_string(),
                assignee: None,
                owner: Some("test".to_string()),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                created_by: "test".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                started_at: None,
                labels: None,
                estimated_minutes: None,
                metadata: None,
                parent: None,
                dependencies: None,
                dependency_count: None,
                dependent_count: None,
                comment_count: None,
            });
        }

        RepoSnapshot {
            path: PathBuf::from(format!("/test/{name}")),
            name: name.to_string(),
            is_beads_repo,
            skip_reason: skip,
            ready,
            count: ready_count as u64,
            blocked: Vec::new(),
            zero_state,
            freshness,
        }
    }

    #[test]
    fn scan_subcommand_json_outputs_snapshots() {
        let snapshots = vec![
            make_snapshot("repo-a", 3, None),
            make_snapshot("repo-b", 0, Some(SkipReason::Excluded)),
        ];

        let json = serde_json::to_string(&snapshots).expect("serialize");
        assert!(json.contains("repo-a"));
        assert!(json.contains("repo-b"));
        assert!(json.contains("Excluded"));
    }

    #[test]
    fn scan_exit_code_is_success_for_ordinary_skips() {
        let snapshots = vec![
            make_snapshot("a", 3, None),
            make_snapshot("b", 0, Some(SkipReason::NotBeadsRepo)),
            make_snapshot("c", 0, Some(SkipReason::Excluded)),
            make_snapshot("d", 0, Some(SkipReason::InProgress)),
            make_snapshot("e", 0, Some(SkipReason::NotGitRepo)),
        ];

        assert_eq!(scan_exit_code(&snapshots), ExitCode::SUCCESS);
    }

    #[test]
    fn cycle_scope_parser_collects_repeatable_repo_and_only_selectors() {
        let args = [
            "--dry-run",
            "--repo",
            "alpha",
            "--repo",
            "/repos/bravo",
            "--only",
            "alpha:a-1",
            "--only",
            "/repos/bravo:b-2",
            "--config",
            "/tmp/undertake.toml",
        ]
        .map(str::to_string);
        let options = parse_cycle_options(&args).expect("cycle options");
        assert!(options.dry_run);
        assert_eq!(options.scope.repos, ["alpha", "/repos/bravo"]);
        assert_eq!(options.scope.only, ["alpha:a-1", "/repos/bravo:b-2"]);
        assert_eq!(options.config, PathBuf::from("/tmp/undertake.toml"));
    }

    #[test]
    fn cycle_scope_parser_rejects_missing_values_and_unknown_arguments() {
        assert!(parse_cycle_options(&["--repo".to_string()]).is_err());
        assert!(parse_cycle_options(&["--only".to_string()]).is_err());
        assert!(parse_cycle_options(&["--dry-run".to_string(), "--wide".to_string()]).is_err());
        assert!(parse_cycle_options(&[]).is_err());
    }

    #[test]
    fn scan_exit_code_fails_only_on_scan_gap() {
        let snapshots = vec![
            make_snapshot("a", 3, None),
            make_snapshot(
                "b",
                0,
                Some(SkipReason::ScanGap {
                    command: "bd ready --json".to_string(),
                    message: "boom".to_string(),
                }),
            ),
        ];

        assert_eq!(scan_exit_code(&snapshots), ExitCode::from(1));
    }

    #[test]
    fn scan_table_formats_columns() {
        let snapshots = vec![
            make_snapshot("alpha", 5, None),
            make_snapshot("beta-long-name", 12, None),
            make_snapshot("gamma", 0, Some(SkipReason::InProgress)),
        ];

        // Capture output by calling the function and checking it doesn't panic
        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_handles_empty_list() {
        let snapshots: Vec<RepoSnapshot> = vec![];
        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_shows_zero_states() {
        let mut snap = make_snapshot("drained", 0, None);
        snap.zero_state = ZeroState::Drained;
        snap.freshness = Freshness::Stale;

        let snapshots = vec![snap];
        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_shows_blocked_zero_state() {
        let mut snap = make_snapshot("blocked", 0, None);
        snap.zero_state = ZeroState::Blocked;
        snap.freshness = Freshness::Recent;

        let snapshots = vec![snap];
        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_shows_all_skip_reasons() {
        let snapshots = vec![
            make_snapshot("a", 0, Some(SkipReason::InProgress)),
            make_snapshot("b", 0, Some(SkipReason::Excluded)),
            make_snapshot("c", 0, Some(SkipReason::NotBeadsRepo)),
            make_snapshot("d", 0, Some(SkipReason::NotGitRepo)),
            make_snapshot(
                "e",
                0,
                Some(SkipReason::ScanGap {
                    command: "bd ready --json".to_string(),
                    message: "failed to parse JSON from `bd ready`: fixture".to_string(),
                }),
            ),
        ];

        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_shows_all_freshness_levels() {
        let mut s1 = make_snapshot("fresh", 1, None);
        s1.freshness = Freshness::Fresh;

        let mut s2 = make_snapshot("recent", 1, None);
        s2.freshness = Freshness::Recent;

        let mut s3 = make_snapshot("stale", 1, None);
        s3.freshness = Freshness::Stale;

        let mut s4 = make_snapshot("unknown", 1, None);
        s4.freshness = Freshness::Unknown;

        let snapshots = vec![s1, s2, s3, s4];
        print_scan_table(&snapshots);
    }

    const ADVERSARIAL_CLI_CONFIG: &str = r#"
[budgets]
use_musterroll = true
item_wall_clock_mins = 1

[adversarial_review]
max_reviewers = 3
parallel = 2
judge = "judge"

[[roster]]
name = "reviewer-one"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "reviewer-one"
provider = "opencode-go"

[[roster]]
name = "reviewer-two"
tier = "lead"
ceiling = "L"
efficiency = "std"
backend = "pi"
dispatch_id = "reviewer-two"
provider = "agy"

[[roster]]
name = "judge"
tier = "lead"
ceiling = "XL"
efficiency = "heavy"
backend = "pi"
dispatch_id = "judge"
provider = "codex"
"#;

    struct AdversarialCliFixture {
        _temp: CliTempDir,
        target_repo: PathBuf,
        artifact: PathBuf,
        config_path: PathBuf,
        config: crate::config::Config,
        paths: AdversarialPaths,
        musterroll: crate::musterroll::test_support::FakeMusterrollClient,
    }

    impl AdversarialCliFixture {
        fn new(label: &str) -> Self {
            let temp = CliTempDir::new(label);
            let target_repo = temp.path().join("target-repo");
            std::fs::create_dir_all(&target_repo).unwrap();
            let artifact = target_repo.join("design.md");
            std::fs::write(&artifact, b"immutable design").unwrap();
            let config_path = temp.path().join("undertake.toml");
            let config = crate::config::parse_str(ADVERSARIAL_CLI_CONFIG).unwrap();
            std::fs::write(&config_path, ADVERSARIAL_CLI_CONFIG).unwrap();
            let paths = AdversarialPaths {
                state_root: temp.path().join("state").join("adversarial-reviews"),
                reports_home: temp.path().join("reports-home"),
                ledger_path: temp.path().join("ledger").join("model-bench.jsonl"),
            };
            let musterroll =
                crate::musterroll::test_support::FakeMusterrollClient::with_provider_availabilities(
                    &[
                        ("opencode-go", Availability::Healthy),
                        ("agy", Availability::Healthy),
                        ("codex", Availability::Healthy),
                    ],
                );
            Self {
                _temp: temp,
                target_repo,
                artifact,
                config_path,
                config,
                paths,
                musterroll,
            }
        }

        fn plan_options(&self) -> AdversarialPlanOptions {
            AdversarialPlanOptions {
                artifact: self.artifact.clone(),
                reviewers: 2,
                question: "Should this architecture proceed?".to_string(),
                models: Some(vec!["reviewer-one".to_string(), "reviewer-two".to_string()]),
                config: self.config_path.clone(),
            }
        }

        fn plan(&self, review_id: &str) -> crate::adversarial::PublishedApproval {
            execute_adversarial_plan(
                &self.config,
                &self.plan_options(),
                &self.paths,
                &self.musterroll,
                &NoopDeckValidator,
                review_id,
                "2026-07-15T12:00:00Z",
            )
            .unwrap()
        }

        fn approve(&self, plan: &crate::adversarial::AdversarialReviewPlan) {
            let run_dir =
                crate::deck::report_run_dir(&self.paths.reports_home, &plan.review_id).unwrap();
            std::fs::write(
                run_dir.join("responses.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "version": 1,
                    "responses": {
                        crate::adversarial::approval_block_id(plan): {
                            "value": "approved",
                            "at": "2026-07-15T12:01:00Z"
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        fn seed_mutation_sentinels(&self) -> Vec<(PathBuf, Vec<u8>)> {
            [
                "beads.sentinel",
                "git.sentinel",
                "worktree.sentinel",
                "cycle.sentinel",
                "repository.sentinel",
                "chezmoi-apply.sentinel",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let path = self.target_repo.join(name);
                let bytes = format!("sentinel-{index}").into_bytes();
                std::fs::write(&path, &bytes).unwrap();
                (path, bytes)
            })
            .collect()
        }
    }

    struct CliTempDir(PathBuf);

    impl CliTempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "undertake-cli-{label}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for CliTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct NoopDeckValidator;

    impl crate::deck::DeckValidator for NoopDeckValidator {
        fn validate(&self, report_path: &Path) -> crate::deck::Result<()> {
            assert!(report_path.is_file());
            Ok(())
        }
    }

    #[derive(Default)]
    struct CliReviewExec {
        spawns: Mutex<Vec<crate::dispatch::SpawnRequest>>,
        malformed_reviewers: bool,
    }

    impl CliReviewExec {
        fn malformed_reviewers() -> Self {
            Self {
                spawns: Mutex::new(Vec::new()),
                malformed_reviewers: true,
            }
        }

        fn spawns(&self) -> Vec<crate::dispatch::SpawnRequest> {
            self.spawns.lock().unwrap().clone()
        }
    }

    impl crate::dispatch::Exec for CliReviewExec {
        fn spawn(
            &self,
            request: &crate::dispatch::SpawnRequest,
        ) -> crate::dispatch::Result<Box<dyn crate::dispatch::ChildProcess>> {
            let prompt_index = request
                .argv
                .iter()
                .position(|arg| arg == "-p")
                .expect("read-only prompt flag");
            let prompt = &request.argv[prompt_index + 1];
            let output = if prompt.contains("adversarial synthesis") {
                serde_json::json!({
                    "verdict": "conditional-go",
                    "consensus": ["preserve the boundary"],
                    "disagreements": [{
                        "topic": "timing",
                        "positions": [
                            {"reviewers": ["R1"], "position": "ship"},
                            {"reviewers": ["R2"], "position": "wait"}
                        ]
                    }],
                    "unique_risks": [{"reviewer": "R2", "risk": "timing"}],
                    "required_changes": ["document the boundary"],
                    "deferred_questions": ["none"],
                    "confidence": "high",
                    "coverage": ["R1", "R2"]
                })
                .to_string()
            } else if self.malformed_reviewers {
                "not-json".to_string()
            } else {
                serde_json::json!({
                    "verdict": "conditional-go",
                    "findings": [{
                        "id": "boundary",
                        "severity": "high",
                        "claim": "boundary required",
                        "evidence": "artifact",
                        "consequence": "drift",
                        "recommendation": "document it"
                    }],
                    "assumptions": ["artifact is authoritative"],
                    "scope_to_cut": ["migration"],
                    "recommended_sequencing": ["boundary first"]
                })
                .to_string()
            };
            std::fs::create_dir_all(request.stdout_path.parent().unwrap()).unwrap();
            std::fs::write(&request.stdout_path, output).unwrap();
            std::fs::write(&request.stderr_path, b"").unwrap();
            self.spawns.lock().unwrap().push(request.clone());
            Ok(Box::new(CliReviewChild))
        }
    }

    struct CliReviewChild;

    impl crate::dispatch::ChildProcess for CliReviewChild {
        fn wait_for(
            &mut self,
            _timeout: std::time::Duration,
        ) -> crate::dispatch::Result<Option<crate::dispatch::ProcessStatus>> {
            Ok(Some(crate::dispatch::ProcessStatus::code(0)))
        }

        fn terminate(&mut self) -> crate::dispatch::Result<()> {
            Ok(())
        }

        fn kill(&mut self) -> crate::dispatch::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> crate::dispatch::Result<crate::dispatch::ProcessStatus> {
            Ok(crate::dispatch::ProcessStatus::code(0))
        }
    }
    #[test]
    fn active_cli_has_no_arena_surface() {
        assert!(!USAGE.contains("arena"));
    }
    #[test]
    fn active_cli_uses_only_undertake_identity() {
        assert!(USAGE.starts_with("usage: undertake "));
        assert!(USAGE.contains("[plan prepare --repo <path>"));
        assert!(!USAGE.contains("conductor"));
        assert_eq!(
            parse_cycle_options(&["--dry-run".to_string()])
                .unwrap()
                .config,
            PathBuf::from("undertake.toml")
        );
    }

    #[test]
    fn migrate_state_requires_explicit_source_and_destination() {
        let options = parse_migrate_state_options(&[
            "state".to_string(),
            "--from".to_string(),
            "/snapshot/conductor".to_string(),
            "--to".to_string(),
            "/state/undertake".to_string(),
        ])
        .unwrap();
        assert_eq!(options.source, PathBuf::from("/snapshot/conductor"));
        assert_eq!(options.destination, PathBuf::from("/state/undertake"));
        assert_eq!(options.config, PathBuf::from("undertake.toml"));
        assert!(parse_migrate_state_options(&["state".to_string()]).is_err());
    }

    #[test]
    fn supersede_options_parser_requires_every_pinned_identity() {
        let options = parse_supersede_options(&[
            "--repo".to_string(),
            "/fleet/sandbox-repo".to_string(),
            "--source-run".to_string(),
            "run-work-source".to_string(),
            "--source-cycle".to_string(),
            "cycle-source".to_string(),
            "--source-bead".to_string(),
            "sandbox-1".to_string(),
            "--source-commit".to_string(),
            "a".repeat(40),
            "--replacement-run".to_string(),
            "run-work-replacement".to_string(),
            "--replacement-cycle".to_string(),
            "cycle-replacement".to_string(),
            "--replacement-bead".to_string(),
            "sandbox-2".to_string(),
            "--replacement-commit".to_string(),
            "b".repeat(40),
        ])
        .expect("every pinned identity supplied");
        assert_eq!(options.repo, PathBuf::from("/fleet/sandbox-repo"));
        assert_eq!(options.pin.source_run_id, "run-work-source");
        assert_eq!(options.pin.source_cycle_id, "cycle-source");
        assert_eq!(options.pin.source_bead, "sandbox-1");
        assert_eq!(options.pin.source_promoted_commit, "a".repeat(40));
        assert_eq!(options.pin.replacement_run_id, "run-work-replacement");
        assert_eq!(options.pin.replacement_cycle_id, "cycle-replacement");
        assert_eq!(options.pin.replacement_bead, "sandbox-2");
        assert_eq!(options.pin.replacement_promoted_commit, "b".repeat(40));

        assert!(
            parse_supersede_options(&[
                "--repo".to_string(),
                "/fleet/sandbox-repo".to_string(),
                "--source-run".to_string(),
                "run-work-source".to_string(),
            ])
            .is_err(),
            "missing pinned identities must be rejected, not defaulted"
        );
        assert!(
            USAGE.contains("[supersede --repo"),
            "the operator command must be advertised in usage"
        );
    }

    /// Without `tui` the dashboard command must not exist at all: the usage
    /// line never advertises it and `undertake dashboard` is an ordinary
    /// unknown subcommand. Deliberately ungated so the no-default-features
    /// build is the one that actually runs the negative half.
    #[test]
    fn dashboard_cli_exists_only_in_a_tui_build() {
        assert!(
            !USAGE.contains("dashboard"),
            "the shared usage line must stay feature-independent"
        );
        if cfg!(feature = "tui") {
            assert_eq!(
                DASHBOARD_USAGE,
                " [dashboard [--run <run-id>] [--refresh-ms <milliseconds>] [--config <path>]]"
            );
        } else {
            assert_eq!(DASHBOARD_USAGE, "");
            assert_eq!(run(vec!["dashboard".to_string()]), ExitCode::from(2));
        }
    }

    /// The `undertake dashboard` command contract (spec § Command contract).
    /// Named so the plan's `cargo test dashboard_cli` selects exactly this
    /// module plus the two PTY tests that drive the shipped binary.
    #[cfg(feature = "tui")]
    mod dashboard_cli {
        use super::*;
        use crate::dashboard::{RunSelection, RunSourceConfig};

        const PILOT: &str = "run-work-20260725T183920.469500000-p45813-000000";

        fn args(list: &[&str]) -> Vec<String> {
            list.iter().map(|arg| (*arg).to_string()).collect()
        }

        fn parse(list: &[&str]) -> Result<DashboardOptions, String> {
            parse_dashboard_options(&args(list))
        }

        #[test]
        fn defaults_select_the_newest_run_at_the_one_second_local_cadence() {
            assert_eq!(
                parse(&[]).expect("no arguments is the documented default"),
                DashboardOptions {
                    run: None,
                    refresh_ms: DASHBOARD_DEFAULT_REFRESH_MS,
                    config: PathBuf::from("undertake.toml"),
                }
            );
            assert_eq!(DASHBOARD_DEFAULT_REFRESH_MS, 1000);
        }

        #[test]
        fn an_explicit_run_refresh_and_config_are_carried_through() {
            let parsed = parse(&[
                "--run",
                PILOT,
                "--refresh-ms",
                "500",
                "--config",
                "/tmp/other.toml",
            ])
            .expect("the full grammar");
            assert_eq!(parsed.run.as_deref(), Some(PILOT));
            assert_eq!(parsed.refresh_ms, 500);
            assert_eq!(parsed.config, PathBuf::from("/tmp/other.toml"));
        }

        /// 250–60000 inclusive. The boundaries themselves are the test: an
        /// off-by-one here either spins the local reader four times faster
        /// than the floor allows or accepts a refresh slower than a minute.
        #[test]
        fn refresh_bounds_are_inclusive_and_closed_outside() {
            for accepted in ["250", "1000", "60000"] {
                let parsed = parse(&["--refresh-ms", accepted])
                    .unwrap_or_else(|error| panic!("{accepted} ms must be accepted: {error}"));
                assert_eq!(parsed.refresh_ms.to_string(), accepted);
            }
            for rejected in ["249", "60001", "0", "-1", "1.5", "", " 500", "1_000", "abc"] {
                assert!(
                    parse(&["--refresh-ms", rejected]).is_err(),
                    "{rejected:?} must not be accepted as a refresh interval"
                );
            }
        }

        #[test]
        fn duplicate_and_unknown_arguments_are_rejected() {
            assert!(parse(&["--run", PILOT, "--run", PILOT]).is_err());
            assert!(parse(&["--refresh-ms", "500", "--refresh-ms", "500"]).is_err());
            assert!(
                parse(&["--config", "a.toml", "--config", "a.toml"]).is_err(),
                "a duplicate --config must exit 2 even when both spellings agree"
            );
            assert!(parse(&["--wide"]).is_err());
            assert!(parse(&[PILOT]).is_err(), "the run id is not positional");
            assert!(parse(&["--run"]).is_err());
            assert!(parse(&["--refresh-ms"]).is_err());
            assert!(parse(&["--config"]).is_err());
        }

        /// The run id goes through the dashboard's own single-normal-component
        /// validation, not a second copy of it living in the CLI.
        #[test]
        fn the_run_id_passes_single_normal_component_validation() {
            for rejected in [
                "",
                ".",
                "..",
                "../escape",
                "a/b",
                "/abs",
                "./x",
                "runs/../x",
            ] {
                assert!(
                    parse(&["--run", rejected]).is_err(),
                    "{rejected:?} must not survive run-id validation"
                );
            }
            assert!(parse(&["--run", PILOT]).is_ok());
        }

        /// An unknown `--run` id exits 2, and it is refused from the plain
        /// terminal — before raw mode, the alternate screen, or any worker.
        #[test]
        fn an_unknown_run_id_is_refused_before_the_terminal_is_touched() {
            let temp = CliTempDir::new("dashboard-selection");
            let config = RunSourceConfig {
                state_root: temp.path().to_path_buf(),
                reports_home: temp.path().join("reports-home"),
                refresh_interval: std::time::Duration::from_secs(1),
            };
            assert!(dashboard_selection(Some("run-work-absent"), &config).is_err());

            std::fs::create_dir_all(config.runs_dir().join("run-work-present")).unwrap();
            assert_eq!(
                dashboard_selection(Some("run-work-present"), &config).unwrap(),
                RunSelection::Explicit("run-work-present".to_string())
            );
            assert_eq!(
                dashboard_selection(None, &config).unwrap(),
                RunSelection::Newest,
                "an empty runs directory is a state the dashboard renders, not a launch failure"
            );
        }

        /// Every exit-2 path reachable without a terminal, driven through the
        /// real `run()` entry point. None of these may reach raw mode, so the
        /// test process's own terminal is never touched.
        #[test]
        fn argument_and_config_errors_exit_two() {
            for rejected in [
                args(&["dashboard", "--wide"]),
                args(&["dashboard", "--run"]),
                args(&["dashboard", "--run", "../escape"]),
                args(&["dashboard", "--refresh-ms", "10"]),
                args(&["dashboard", "--refresh-ms", "60001"]),
                args(&["dashboard", "--config", "a.toml", "--config", "a.toml"]),
                args(&[
                    "dashboard",
                    "--config",
                    "/nonexistent/undertake-dashboard-test.toml",
                ]),
            ] {
                assert_eq!(
                    run(rejected.clone()),
                    ExitCode::from(2),
                    "{rejected:?} must exit 2"
                );
            }
        }

        /// `q` exits 0; a terminal setup failure exits 1. The end-to-end
        /// proof of both lives in the PTY suite (`dashboard_cli_*` in
        /// `dashboard::runtime::terminal`), which can own a real terminal —
        /// and take one away. This pins the mapping itself.
        #[test]
        fn a_clean_exit_maps_to_zero_and_a_terminal_failure_to_one() {
            assert_eq!(dashboard_exit_code(&Ok(())), ExitCode::SUCCESS);
            assert_eq!(
                dashboard_exit_code(&Err(std::io::Error::other("no controlling terminal"))),
                ExitCode::from(1)
            );
        }

        /// The command constructs a read-only source and nothing else: no bd
        /// client, no dispatch or recovery entry point, no run handle, no
        /// lease, no write.
        #[test]
        fn construction_reaches_no_dispatch_or_recovery_mutation_handle() {
            let source = include_str!("cli.rs");
            let body = source
                .split("fn run_dashboard_command")
                .nth(1)
                .expect("the dashboard command exists")
                .split("\nfn ")
                .next()
                .expect("the dashboard command has a body");
            for forbidden in [
                "RunHandle",
                "CommandBdClient",
                "dispatch",
                "recovery",
                "claim",
                "lease",
                "heartbeat",
                "write",
            ] {
                assert!(
                    !body.contains(forbidden),
                    "the dashboard command body must not mention {forbidden}"
                );
            }
        }
    }
}
