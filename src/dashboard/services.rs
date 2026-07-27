//! Read-only evidence service adapters for the Undertake dashboard.
//!
//! Service adapters consume read-only service APIs and bounded subprocess
//! commands to produce immutable service snapshots (`ServiceSnapshot`,
//! `MusterrollSnapshot`, `AfterfactSnapshot`, `CautionlightSnapshot`). They
//! never mutate service state, run automatic background mutations, or open a
//! run directory for write.
//!
//! Every string an adapter parses out of a service is untrusted: it passes
//! through [`crate::sanitize`] before it can reach a snapshot, and
//! no parsed path is ever opened or canonicalized.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::dashboard::model::SourceState;
use crate::musterroll::{Availability, MusterrollClient, StatusReport, Window};
use crate::process::BoundedCommand;
use crate::sanitize::{sanitize_single_line, sanitize_text};

/// Degrades a source after a failed attempt: retain the last valid value as
/// [`SourceState::Stale`], or stay [`SourceState::Absent`] when nothing has
/// ever succeeded. Shared by all three adapters so they cannot drift on the
/// rule that a source which never succeeded is never given a `last_ok`.
fn degrade<T: Clone>(
    previous: Option<&SourceState<T>>,
    now: DateTime<Utc>,
    error: String,
) -> SourceState<T> {
    match previous {
        Some(
            SourceState::Fresh {
                value,
                last_ok,
                truncated,
                ..
            }
            | SourceState::Stale {
                value,
                last_ok,
                truncated,
                ..
            },
        ) => SourceState::Stale {
            value: value.clone(),
            last_ok: *last_ok,
            last_attempt: now,
            error,
            // The retained value is byte-identical to the one that was
            // truncated, so it stays marked truncated.
            truncated: *truncated,
        },
        _ => SourceState::Absent {
            last_attempt: Some(now),
            error: Some(error),
        },
    }
}

// ============================================================================
// Musterroll Adapter
// ============================================================================

/// The only `ProviderStatus.extra` keys the dashboard renders. Everything else
/// a newer Musterroll adds is dropped rather than displayed unreviewed.
const ALLOWLISTED_EXTRA_KEYS: [&str; 2] = ["observation_expiry_basis", "observation_model"];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderStatusSnapshot {
    pub(crate) availability: Availability,
    pub(crate) source: String,
    pub(crate) checked_at: String,
    pub(crate) data_as_of: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) windows: Vec<Window>,
    pub(crate) reason: Option<String>,
    pub(crate) extra: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MusterrollSnapshot {
    pub(crate) schema: String,
    pub(crate) checked_at: String,
    pub(crate) providers: BTreeMap<String, ProviderStatusSnapshot>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MusterrollDashboardSource;

impl MusterrollDashboardSource {
    /// Reads provider availability through the existing typed
    /// [`MusterrollClient`] seam rather than a second JSON parser, so the
    /// dashboard and the dispatcher can never disagree about what a provider
    /// status means.
    pub(crate) fn read<C: MusterrollClient + ?Sized>(
        client: &C,
        previous: Option<&SourceState<MusterrollSnapshot>>,
        now: DateTime<Utc>,
    ) -> SourceState<MusterrollSnapshot> {
        match client.status() {
            Ok(report) => SourceState::Fresh {
                value: convert_status_report(report),
                last_ok: now,
                last_attempt: now,
                // The typed client rejects a truncated or timed-out read
                // outright, so a value that arrives here is always complete.
                truncated: false,
            },
            Err(error) => degrade(previous, now, error.to_string()),
        }
    }
}

fn convert_status_report(report: StatusReport) -> MusterrollSnapshot {
    let mut providers = BTreeMap::new();

    for (name, provider) in report.providers {
        let windows = provider
            .windows
            .into_iter()
            .map(|window| Window {
                label: sanitize_single_line(&window.label),
                percent: window.percent,
                reset_at: window.reset_at.as_deref().map(sanitize_single_line),
            })
            .collect();

        let extra = provider
            .extra
            .into_iter()
            .filter(|(key, _)| ALLOWLISTED_EXTRA_KEYS.contains(&key.as_str()))
            .map(|(key, value)| {
                let rendered = match value {
                    serde_json::Value::String(text) => text,
                    other => other.to_string(),
                };
                (sanitize_single_line(&key), sanitize_single_line(&rendered))
            })
            .collect();

        providers.insert(
            sanitize_single_line(&name),
            ProviderStatusSnapshot {
                availability: provider.availability,
                source: sanitize_single_line(&provider.source),
                checked_at: sanitize_single_line(&provider.checked_at),
                data_as_of: provider.data_as_of.as_deref().map(sanitize_single_line),
                expires_at: provider.expires_at.as_deref().map(sanitize_single_line),
                windows,
                reason: provider.reason.as_deref().map(sanitize_single_line),
                extra,
            },
        );
    }

    MusterrollSnapshot {
        schema: sanitize_single_line(&report.schema),
        checked_at: sanitize_single_line(&report.checked_at),
        providers,
    }
}

// ============================================================================
// Afterfact Adapter
// ============================================================================

const AFTERFACT_PROGRAM: &str = "afterfact";
const AFTERFACT_ARGS: [&str; 3] = ["events", "--since", "1h"];
const AFTERFACT_SCHEMA: &str = "afterfact/event@2";
const AFTERFACT_MAX_LINES: usize = 20_000;
const AFTERFACT_STDOUT_CAP: usize = 4 * 1024 * 1024;
const AFTERFACT_STDERR_CAP: usize = 256 * 1024;
const AFTERFACT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct AfterfactRepo {
    pub(crate) cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct AfterfactEventRecord {
    pub(crate) schema: String,
    #[serde(default)]
    pub(crate) event_id: Option<String>,
    #[serde(default)]
    pub(crate) timestamp: Option<String>,
    #[serde(default)]
    pub(crate) repo: Option<AfterfactRepo>,
    #[serde(default)]
    pub(crate) git_commit: Option<String>,
    #[serde(default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AfterfactSnapshot {
    pub(crate) events: Vec<AfterfactEventRecord>,
    pub(crate) correlated_count: usize,
    pub(crate) uncorrelated_count: usize,
    /// The bounded exit-1 stderr summary explaining what the events do not
    /// cover. `None` on a complete (exit 0) read.
    pub(crate) coverage_gap_summary: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AfterfactDashboardSource;

impl AfterfactDashboardSource {
    /// The exact command this adapter runs: `afterfact events --since 1h`
    /// with stdin closed, 4 MiB of stdout, 256 KiB of stderr, and a 60-second
    /// deadline.
    pub(crate) fn default_command() -> BoundedCommand {
        BoundedCommand::new(AFTERFACT_PROGRAM)
            .args(AFTERFACT_ARGS)
            .stdout_cap(AFTERFACT_STDOUT_CAP)
            .stderr_cap(AFTERFACT_STDERR_CAP)
            .timeout(AFTERFACT_TIMEOUT)
    }

    /// Runs one bounded Afterfact query and reduces it to a snapshot.
    ///
    /// `command_override` exists for fixtures; `run_dir` and `worker_commits`
    /// are the typed run facts correlation is allowed to match against.
    pub(crate) fn read(
        command_override: Option<&BoundedCommand>,
        run_dir: Option<&Path>,
        worker_commits: &[String],
        previous: Option<&SourceState<AfterfactSnapshot>>,
        now: DateTime<Utc>,
    ) -> SourceState<AfterfactSnapshot> {
        // Constructed only when it is actually used: an override must not pay
        // to build (and, for Cautionlight, fill) the real command each refresh.
        let owned_default;
        let command = if let Some(command) = command_override {
            command
        } else {
            owned_default = Self::default_command();
            &owned_default
        };

        let outcome = match command.run() {
            Ok(outcome) => outcome,
            Err(error) => return degrade(previous, now, format!("run afterfact events: {error}")),
        };

        if outcome.timed_out() {
            return degrade(previous, now, "afterfact events timed out".to_string());
        }
        match outcome.exit_code {
            // 0 is complete; 1 is partial success with a coverage gap.
            Some(0 | 1) => {}
            Some(code) => {
                return degrade(previous, now, format!("afterfact events exited {code}"));
            }
            // Killed for overrunning its output cap, or died by signal: there
            // is no exit status, so this cannot be presented as any kind of
            // success.
            None => {
                return degrade(
                    previous,
                    now,
                    "afterfact events terminated without exiting".to_string(),
                );
            }
        }

        let (events, line_truncated) = parse_afterfact_stdout(&outcome.stdout);
        // Correlation runs on the raw parsed paths, before sanitization:
        // stripping control characters rewrites a path, and a rewritten path
        // could collide with the run directory it must not match.
        let (correlated_count, uncorrelated_count) =
            correlate_events(&events, run_dir, worker_commits);
        let events = events.into_iter().map(sanitize_event).collect();

        // Exit 1 keeps its stderr summary even when the cap clipped it: a
        // partial explanation of a coverage gap still beats silence, and the
        // clipping is reported through `truncated`.
        let coverage_gap_summary = (outcome.exit_code == Some(1))
            .then(|| sanitize_text(&String::from_utf8_lossy(&outcome.stderr)))
            .filter(|summary| !summary.trim().is_empty());

        SourceState::Fresh {
            value: AfterfactSnapshot {
                events,
                correlated_count,
                uncorrelated_count,
                coverage_gap_summary,
            },
            last_ok: now,
            last_attempt: now,
            // Any dropped byte or line is visible truncation, including a
            // clipped coverage summary.
            truncated: outcome.stdout_truncated || outcome.stderr_truncated || line_truncated,
        }
    }
}

/// Returns a copy of `event` with every rendered field stripped of control
/// characters.
///
/// Correlation must already have run: this rewrites `repo.cwd`, which is
/// comparison data before it is display data.
fn sanitize_event(event: AfterfactEventRecord) -> AfterfactEventRecord {
    AfterfactEventRecord {
        // Only records that matched `AFTERFACT_SCHEMA` exactly are retained,
        // so the schema is already a known literal.
        schema: event.schema,
        event_id: event.event_id.as_deref().map(sanitize_single_line),
        timestamp: event.timestamp.as_deref().map(sanitize_single_line),
        repo: event.repo.map(|repo| AfterfactRepo {
            cwd: sanitize_single_line(&repo.cwd),
        }),
        git_commit: event.git_commit.as_deref().map(sanitize_single_line),
        kind: event.kind.as_deref().map(sanitize_single_line),
        summary: event.summary.as_deref().map(sanitize_single_line),
    }
}

/// Parses bounded Afterfact stdout as JSONL, keeping only `afterfact/event@2`
/// records. Malformed and unknown-schema lines are skipped: a partial read is
/// the documented exit-1 contract, not a parse failure.
fn parse_afterfact_stdout(bytes: &[u8]) -> (Vec<AfterfactEventRecord>, bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut events = Vec::new();

    for (index, line) in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if index >= AFTERFACT_MAX_LINES {
            return (events, true);
        }
        if let Ok(record) = serde_json::from_str::<AfterfactEventRecord>(line)
            && record.schema == AFTERFACT_SCHEMA
        {
            events.push(record);
        }
    }

    (events, false)
}

/// Counts how many events plausibly belong to this run.
///
/// Explicitly heuristic, never typed: an event correlates when it carries a
/// commit this run's worker produced, or when it happened at or under the run
/// directory.
fn correlate_events(
    events: &[AfterfactEventRecord],
    run_dir: Option<&Path>,
    worker_commits: &[String],
) -> (usize, usize) {
    let prefixes = run_prefixes(run_dir);

    let correlated = events
        .iter()
        .filter(|event| is_correlated(event, &prefixes, worker_commits))
        .count();

    (correlated, events.len() - correlated)
}

/// The run directory as the caller spelled it, plus its canonical form when
/// that differs.
///
/// Both are trusted spellings of *our own* directory, so accepting either only
/// widens recall — and it has to, because the untrusted event path is never
/// resolved: a state root reached through a symlink (`/var` on macOS is one)
/// would otherwise never match the cwd a worker actually reported.
fn run_prefixes(run_dir: Option<&Path>) -> Vec<PathBuf> {
    let Some(dir) = run_dir else {
        return Vec::new();
    };
    let mut prefixes = vec![dir.to_path_buf()];
    if let Ok(canonical) = std::fs::canonicalize(dir)
        && canonical != *dir
    {
        prefixes.push(canonical);
    }
    prefixes
}

fn is_correlated(
    event: &AfterfactEventRecord,
    run_prefixes: &[PathBuf],
    worker_commits: &[String],
) -> bool {
    if let Some(commit) = &event.git_commit
        && worker_commits.iter().any(|known| known == commit)
    {
        return true;
    }

    let Some(repo) = &event.repo else {
        return false;
    };

    // One direction, whole components only: the event must have happened at or
    // under the run directory. `Path::starts_with` compares components, so a
    // sibling `<run>-other` is not a match; and the reverse direction is
    // deliberately absent, so an ancestor such as `/tmp` never correlates
    // every run beneath it. The event path is compared as reported — never
    // canonicalized, never opened.
    let cwd = Path::new(&repo.cwd);
    run_prefixes.iter().any(|prefix| cwd.starts_with(prefix))
}

// ============================================================================
// Cautionlight Adapter
// ============================================================================

const CAUTIONLIGHT_PROGRAM: &str = "cautionlight";
const CAUTIONLIGHT_ARGS: [&str; 2] = ["inspect", "--stdin"];
const CAUTIONLIGHT_SCHEMA: &str = "cautionlight/finding@1";
const CAUTIONLIGHT_MAX_LINES: usize = 20_000;
const CAUTIONLIGHT_STDOUT_CAP: usize = 4 * 1024 * 1024;
const CAUTIONLIGHT_STDERR_CAP: usize = 256 * 1024;
const CAUTIONLIGHT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct CautionlightFindingRecord {
    pub(crate) schema: String,
    #[serde(default)]
    pub(crate) finding_id: Option<String>,
    #[serde(default)]
    pub(crate) severity: Option<String>,
    #[serde(default)]
    pub(crate) rule: Option<String>,
    #[serde(default)]
    pub(crate) message: Option<String>,
    #[serde(default)]
    pub(crate) file: Option<String>,
    #[serde(default)]
    pub(crate) line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CautionlightSnapshot {
    pub(crate) findings: Vec<CautionlightFindingRecord>,
    pub(crate) coverage_warnings: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CautionlightDashboardSource;

impl CautionlightDashboardSource {
    /// Cautionlight is roadmap-deferred in v1: the parser and the bounded
    /// adapter ship, but nothing runs the pipeline automatically. The panel
    /// says so explicitly rather than rendering an empty success.
    pub(crate) fn default_state() -> SourceState<CautionlightSnapshot> {
        SourceState::Deferred {
            last_attempt: None,
            error: None,
        }
    }

    /// The exact command this adapter runs when explicitly requested:
    /// `cautionlight inspect --stdin`, fed the already-bounded Afterfact
    /// bytes under the same caps and deadline.
    pub(crate) fn default_command(afterfact_bytes: &Arc<Vec<u8>>) -> BoundedCommand {
        BoundedCommand::new(CAUTIONLIGHT_PROGRAM)
            .args(CAUTIONLIGHT_ARGS)
            // Shared, not copied: this is the 4 MiB Afterfact stdout buffer.
            .stdin(Arc::clone(afterfact_bytes))
            .stdout_cap(CAUTIONLIGHT_STDOUT_CAP)
            .stderr_cap(CAUTIONLIGHT_STDERR_CAP)
            .timeout(CAUTIONLIGHT_TIMEOUT)
    }

    /// Runs one on-demand Cautionlight pass. Never called by a refresh tick in
    /// v1 — [`Self::default_state`] is what the panel shows.
    pub(crate) fn read(
        command_override: Option<&BoundedCommand>,
        afterfact_bytes: &Arc<Vec<u8>>,
        previous: Option<&SourceState<CautionlightSnapshot>>,
        now: DateTime<Utc>,
    ) -> SourceState<CautionlightSnapshot> {
        let owned_default;
        let command = if let Some(command) = command_override {
            command
        } else {
            owned_default = Self::default_command(afterfact_bytes);
            &owned_default
        };

        let outcome = match command.run() {
            Ok(outcome) => outcome,
            Err(error) => {
                return degrade(previous, now, format!("run cautionlight inspect: {error}"));
            }
        };

        if outcome.timed_out() {
            return degrade(previous, now, "cautionlight inspect timed out".to_string());
        }
        match outcome.exit_code {
            Some(0 | 1) => {}
            Some(code) => {
                return degrade(previous, now, format!("cautionlight inspect exited {code}"));
            }
            None => {
                return degrade(
                    previous,
                    now,
                    "cautionlight inspect terminated without exiting".to_string(),
                );
            }
        }

        let (findings, line_truncated) = parse_cautionlight_stdout(&outcome.stdout);

        // Exit 1 is partial success; the coverage warnings are the reason.
        let coverage_warnings = (outcome.exit_code == Some(1))
            .then(|| sanitize_text(&String::from_utf8_lossy(&outcome.stderr)))
            .filter(|warnings| !warnings.trim().is_empty());

        SourceState::Fresh {
            value: CautionlightSnapshot {
                findings,
                coverage_warnings,
            },
            last_ok: now,
            last_attempt: now,
            truncated: outcome.stdout_truncated || outcome.stderr_truncated || line_truncated,
        }
    }
}

/// Parses bounded Cautionlight stdout as JSONL, keeping only
/// `cautionlight/finding@1` records with every rendered field sanitized.
fn parse_cautionlight_stdout(bytes: &[u8]) -> (Vec<CautionlightFindingRecord>, bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut findings = Vec::new();

    for (index, line) in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if index >= CAUTIONLIGHT_MAX_LINES {
            return (findings, true);
        }
        if let Ok(record) = serde_json::from_str::<CautionlightFindingRecord>(line)
            && record.schema == CAUTIONLIGHT_SCHEMA
        {
            findings.push(sanitize_finding(record));
        }
    }

    (findings, false)
}

/// Returns a copy of `finding` with every rendered field stripped of control
/// characters. Unlike an Afterfact event, nothing here is compared first, so
/// sanitization happens at parse time.
fn sanitize_finding(finding: CautionlightFindingRecord) -> CautionlightFindingRecord {
    CautionlightFindingRecord {
        schema: finding.schema,
        finding_id: finding.finding_id.as_deref().map(sanitize_single_line),
        severity: finding.severity.as_deref().map(sanitize_single_line),
        rule: finding.rule.as_deref().map(sanitize_single_line),
        message: finding.message.as_deref().map(sanitize_single_line),
        file: finding.file.as_deref().map(sanitize_single_line),
        line: finding.line,
    }
}

// ============================================================================
// Combined Service Snapshot
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ServiceSnapshot {
    pub(crate) musterroll: SourceState<MusterrollSnapshot>,
    pub(crate) afterfact: SourceState<AfterfactSnapshot>,
    pub(crate) cautionlight: SourceState<CautionlightSnapshot>,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::musterroll::{MusterrollError, Result as MusterrollResult};
    use serde_json::json;

    struct MockMusterrollClient {
        report: MusterrollResult<StatusReport>,
    }

    impl MusterrollClient for MockMusterrollClient {
        fn status(&self) -> MusterrollResult<StatusReport> {
            match &self.report {
                Ok(report) => Ok(report.clone()),
                Err(error) => Err(error.clone()),
            }
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-25T12:00:00Z")
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    /// A scratch directory that removes itself, mirroring the manual pattern
    /// used elsewhere in this crate (no `tempfile` dev-dependency).
    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "undertake-services-{label}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("mkdir temp");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Builds a `BoundedCommand` that emits fixed stdout/stderr and exits with
    /// a fixed code, so adapter behavior is pinned without a real service.
    fn fixture(stdout: &str, stderr: &str, exit: i32) -> BoundedCommand {
        let script = format!(
            "import sys\nsys.stdout.write({stdout})\nsys.stderr.write({stderr})\nsys.exit({exit})\n",
            stdout = python_literal(stdout),
            stderr = python_literal(stderr),
        );
        BoundedCommand::new("python3")
            .args(["-c", &script])
            .timeout(Duration::from_secs(30))
    }

    fn python_literal(text: &str) -> String {
        let escaped: String = text
            .chars()
            .map(|character| match character {
                '\\' => "\\\\".to_string(),
                '\'' => "\\'".to_string(),
                '\n' => "\\n".to_string(),
                other if other.is_control() => format!("\\x{:02x}", other as u32),
                other => other.to_string(),
            })
            .collect();
        format!("'{escaped}'")
    }

    pub(crate) mod musterroll {
        use super::*;

        fn provider(availability: &str, reason: Option<&str>) -> serde_json::Value {
            json!({
                "availability": availability,
                "source": "api",
                "checked_at": "2026-07-25T12:00:00Z",
                "data_as_of": null,
                "expires_at": null,
                "windows": [],
                "reason": reason,
                "extra": {}
            })
        }

        fn report(providers: &serde_json::Value) -> StatusReport {
            serde_json::from_value(json!({
                "schema": "musterroll/status@2",
                "checked_at": "2026-07-25T12:00:00Z",
                "providers": providers,
            }))
            .expect("valid status report")
        }

        /// Availability stays typed end to end; the four states never collapse
        /// into a single "unhealthy".
        #[test]
        fn typed_availability_distinctions() {
            let report = report(&json!({
                "anthropic": provider("healthy", None),
                "codex": provider("caution", Some("near limit")),
                "opencode": provider("exhausted", Some("rate limited")),
                "unknown_provider": provider("unknown", None),
            }));
            let client = MockMusterrollClient { report: Ok(report) };

            let state = MusterrollDashboardSource::read(&client, None, now());

            let value = state.value().expect("fresh value");
            let availability =
                |name: &str| value.providers.get(name).expect("provider").availability;
            assert_eq!(availability("anthropic"), Availability::Healthy);
            assert_eq!(availability("codex"), Availability::Caution);
            assert_eq!(availability("opencode"), Availability::Exhausted);
            assert_eq!(availability("unknown_provider"), Availability::Unknown);
            assert_eq!(
                value
                    .providers
                    .get("codex")
                    .expect("provider")
                    .reason
                    .as_deref(),
                Some("near limit")
            );
        }

        /// Exactly the two allowlisted `extra` keys survive; anything else a
        /// newer Musterroll adds is dropped rather than rendered unreviewed.
        #[test]
        fn only_allowlisted_extra_keys_are_retained() {
            let mut anthropic = provider("healthy", None);
            anthropic["extra"] = json!({
                "observation_expiry_basis": "fixed_window",
                "observation_model": "claude-opus-5",
                "secret_token": "shh",
                "arbitrary_key": "drop_me",
            });
            let client = MockMusterrollClient {
                report: Ok(report(&json!({ "anthropic": anthropic }))),
            };

            let state = MusterrollDashboardSource::read(&client, None, now());

            let extra = &state.value().expect("fresh value").providers["anthropic"].extra;
            assert_eq!(
                extra.keys().collect::<Vec<_>>(),
                vec!["observation_expiry_basis", "observation_model"]
            );
            assert_eq!(extra["observation_model"], "claude-opus-5");
        }

        /// Control bytes anywhere in the status payload are stripped before
        /// they can reach the renderer.
        #[test]
        fn control_bytes_are_stripped_from_every_rendered_field() {
            let mut hostile = provider("healthy", Some("bad\u{1b}[0m"));
            hostile["source"] = json!("api\u{7}");
            hostile["data_as_of"] = json!("2026-07-25\u{7}");
            hostile["windows"] =
                json!([{ "label": "5h\u{1b}[31m", "percent": 12.5, "reset_at": "soon\u{7}" }]);
            hostile["extra"] = json!({ "observation_model": "gpt\u{0}-5.6" });
            let report = serde_json::from_value::<StatusReport>(json!({
                "schema": "musterroll/status@2\u{7}",
                "checked_at": "2026-07-25T12:00:00Z\u{1b}[31m",
                "providers": { "anthropic\u{0}": hostile },
            }))
            .expect("valid status report");
            let client = MockMusterrollClient { report: Ok(report) };

            let state = MusterrollDashboardSource::read(&client, None, now());

            let value = state.value().expect("fresh value");
            assert_eq!(value.schema, "musterroll/status@2");
            assert_eq!(value.checked_at, "2026-07-25T12:00:00Z[31m");
            let provider = value.providers.get("anthropic").expect("sanitized key");
            assert_eq!(provider.source, "api");
            assert_eq!(provider.data_as_of.as_deref(), Some("2026-07-25"));
            assert_eq!(provider.reason.as_deref(), Some("bad[0m"));
            assert_eq!(provider.windows[0].label, "5h[31m");
            assert_eq!(provider.windows[0].reset_at.as_deref(), Some("soon"));
            assert_eq!(provider.extra["observation_model"], "gpt-5.6");
        }

        /// A failed read retains the last good value as stale rather than
        /// blanking the panel, and never invents a `last_ok` when nothing ever
        /// succeeded.
        #[test]
        fn failure_retains_last_value_and_never_fabricates_last_ok() {
            let ok = MockMusterrollClient {
                report: Ok(report(&json!({ "anthropic": provider("healthy", None) }))),
            };
            let broken = MockMusterrollClient {
                report: Err(MusterrollError::command("musterroll exploded")),
            };
            let first = MusterrollDashboardSource::read(&ok, None, now());
            let later = now() + chrono::Duration::seconds(30);

            let degraded = MusterrollDashboardSource::read(&broken, Some(&first), later);
            let cold = MusterrollDashboardSource::read(&broken, None, later);

            match degraded {
                SourceState::Stale {
                    last_ok,
                    last_attempt,
                    ref error,
                    ..
                } => {
                    assert_eq!(last_ok, now(), "the stale value keeps its real last_ok");
                    assert_eq!(last_attempt, later);
                    assert!(error.contains("musterroll exploded"));
                }
                other => panic!("expected stale retention, got {other:?}"),
            }
            assert!(
                matches!(
                    cold,
                    SourceState::Absent {
                        last_attempt: Some(_),
                        ..
                    }
                ),
                "a source that never succeeded must stay absent"
            );
        }
    }

    pub(crate) mod afterfact {
        use super::*;

        fn event_line(event_id: &str) -> String {
            format!("{{\"schema\":\"afterfact/event@2\",\"event_id\":\"{event_id}\"}}\n")
        }

        fn record(cwd: Option<&str>, commit: Option<&str>) -> AfterfactEventRecord {
            AfterfactEventRecord {
                schema: AFTERFACT_SCHEMA.to_string(),
                event_id: Some("e".to_string()),
                timestamp: None,
                repo: cwd.map(|cwd| AfterfactRepo {
                    cwd: cwd.to_string(),
                }),
                git_commit: commit.map(str::to_string),
                kind: None,
                summary: None,
            }
        }

        /// The spec's command line and bounds are the contract, so they are
        /// asserted rather than assumed.
        #[test]
        fn default_command_pins_the_spec_contract() {
            let command = AfterfactDashboardSource::default_command();

            assert_eq!(command.program, std::path::Path::new("afterfact"));
            assert_eq!(command.args, ["events", "--since", "1h"]);
            assert!(command.stdin.is_none(), "stdin must be closed");
            assert_eq!(command.stdout_cap, 4 * 1024 * 1024);
            assert_eq!(command.stderr_cap, 256 * 1024);
            assert_eq!(command.timeout, Duration::from_secs(60));
            assert_eq!(AFTERFACT_MAX_LINES, 20_000);
        }

        /// Exit 0 is complete, exit 1 is partial success with a coverage
        /// summary, exit 2 is an error that never becomes a snapshot.
        #[test]
        fn exit_code_semantics() {
            let complete = AfterfactDashboardSource::read(
                Some(&fixture(&event_line("e1"), "", 0)),
                None,
                &[],
                None,
                now(),
            );
            let partial = AfterfactDashboardSource::read(
                Some(&fixture(
                    &event_line("e2"),
                    "coverage gap: 3 repos unscanned\n",
                    1,
                )),
                None,
                &[],
                None,
                now(),
            );
            let failed = AfterfactDashboardSource::read(
                Some(&fixture("", "boom\n", 2)),
                None,
                &[],
                None,
                now(),
            );

            let complete = complete.value().expect("exit 0 is fresh");
            assert_eq!(complete.events.len(), 1);
            assert_eq!(complete.coverage_gap_summary, None);

            let partial = partial
                .value()
                .expect("exit 1 is partial success, not failure");
            assert_eq!(partial.events.len(), 1, "valid events survive exit 1");
            assert_eq!(
                partial.coverage_gap_summary.as_deref(),
                Some("coverage gap: 3 repos unscanned\n")
            );

            assert!(
                matches!(failed, SourceState::Absent { .. }),
                "exit 2 is an error, got {failed:?}"
            );
        }

        /// A clipped stderr summary must not cost the events: the partial
        /// success survives and the truncation is reported.
        #[test]
        fn clipped_coverage_summary_keeps_events_and_marks_truncated() {
            let command = fixture(&event_line("e1"), &"g".repeat(4096), 1).stderr_cap(64);

            let state = AfterfactDashboardSource::read(Some(&command), None, &[], None, now());

            match state {
                SourceState::Fresh {
                    ref value,
                    truncated,
                    ..
                } => {
                    assert_eq!(value.events.len(), 1, "events survive a clipped summary");
                    assert_eq!(
                        value.coverage_gap_summary.as_deref(),
                        Some("g".repeat(64).as_str())
                    );
                    assert!(truncated, "a clipped summary must be visible as truncation");
                }
                other => panic!("expected fresh partial success, got {other:?}"),
            }
        }

        /// The 20,000-line cap bounds the retained events and is reported.
        #[test]
        fn line_cap_bounds_retained_events() {
            let script = format!(
                "import sys\nfor i in range({}):\n    sys.stdout.write('{{\"schema\":\"afterfact/event@2\",\"event_id\":\"e%d\"}}\\n' % i)\n",
                AFTERFACT_MAX_LINES + 500
            );
            let command = BoundedCommand::new("python3")
                .args(["-c", &script])
                .timeout(Duration::from_secs(30));

            let state = AfterfactDashboardSource::read(Some(&command), None, &[], None, now());

            match state {
                SourceState::Fresh {
                    ref value,
                    truncated,
                    ..
                } => {
                    assert_eq!(value.events.len(), AFTERFACT_MAX_LINES);
                    assert!(truncated);
                }
                other => panic!("expected fresh truncated read, got {other:?}"),
            }
        }

        /// Correlation matches a commit the run's worker produced, and only
        /// that commit.
        #[test]
        fn commit_correlation_is_exact() {
            let commits = vec!["c0ffee1".to_string()];
            let events = [
                record(None, Some("c0ffee1")),
                record(None, Some("c0ffee12")),
                record(None, Some("c0ffee")),
                record(None, None),
            ];

            assert_eq!(correlate_events(&events, None, &commits), (1, 3));
        }

        /// The run directory is a component prefix in one direction only: the
        /// run directory itself and paths under it correlate; a sibling that
        /// merely shares a string prefix, and an ancestor that contains the
        /// run, must not. Both the run directory as spelled and its canonical
        /// form are accepted, because the event path is never resolved.
        #[test]
        fn cwd_correlation_is_a_one_way_component_prefix() {
            let temp = TempDir::new("prefix");
            let run_dir = temp.path.join("runs-v2").join("run-work-1");
            std::fs::create_dir_all(&run_dir).expect("mkdir run");
            let shown = run_dir.to_str().expect("utf-8 path").to_string();
            let canonical = std::fs::canonicalize(&run_dir).expect("canonicalize run dir");
            let canonical = canonical.to_str().expect("utf-8 path").to_string();

            let events = [
                record(Some(&shown), None),
                record(Some(&format!("{shown}/attempts/001")), None),
                record(Some(&canonical), None),
                record(Some(&format!("{shown}-other")), None),
                record(Some(temp.path.to_str().expect("utf-8 path")), None),
                record(Some("/"), None),
            ];

            let (correlated, uncorrelated) = correlate_events(&events, Some(&run_dir), &[]);

            assert_eq!(
                (correlated, uncorrelated),
                (3, 3),
                "only the run directory, its canonical spelling, and paths under it correlate"
            );
        }

        /// The event-reported path is comparison data: it is never resolved
        /// through the filesystem. A symlink that *points at* the run
        /// directory is a different path and must not correlate.
        #[test]
        fn event_cwd_is_never_canonicalized() {
            let temp = TempDir::new("symlink");
            let run_dir = temp.path.join("runs-v2").join("run-work-1");
            std::fs::create_dir_all(&run_dir).expect("mkdir run");
            let link = temp.path.join("alias");
            #[cfg(unix)]
            std::os::unix::fs::symlink(&run_dir, &link).expect("symlink");

            let events = [record(Some(link.to_str().expect("utf-8 path")), None)];

            assert_eq!(
                correlate_events(&events, Some(&run_dir), &[]),
                (0, 1),
                "resolving the event path would have made this correlate"
            );
        }

        /// Sanitization runs *after* correlation. A path whose control bytes
        /// would sanitize into the run directory must not be able to buy
        /// correlation with them, yet must still render sanitized.
        #[test]
        fn sanitization_cannot_manufacture_correlation() {
            let temp = TempDir::new("sanitize-order");
            let run_dir = temp.path.join("runs-v2").join("run-work-1");
            std::fs::create_dir_all(&run_dir).expect("mkdir run");
            let canonical = std::fs::canonicalize(&run_dir).expect("canonicalize run dir");
            let shown = canonical.to_str().expect("utf-8 path");
            // Sanitizing this first would yield exactly `shown`.
            let smuggled = format!(
                "{}\u{7}{}",
                &shown[..shown.len() - 1],
                &shown[shown.len() - 1..]
            );
            let line = format!(
                "{{\"schema\":\"afterfact/event@2\",\"repo\":{{\"cwd\":{}}},\"summary\":\"boom\\u001b[31m\"}}\n",
                serde_json::to_string(&smuggled).expect("json string")
            );

            let state = AfterfactDashboardSource::read(
                Some(&fixture(&line, "", 0)),
                Some(&run_dir),
                &[],
                None,
                now(),
            );

            let value = state.value().expect("fresh value");
            assert_eq!(
                (value.correlated_count, value.uncorrelated_count),
                (0, 1),
                "control bytes must not be laundered into a correlating path"
            );
            let event = &value.events[0];
            assert_eq!(
                event.repo.as_ref().expect("repo").cwd,
                shown,
                "the rendered path is still sanitized"
            );
            assert_eq!(event.summary.as_deref(), Some("boom[31m"));
        }
    }

    pub(crate) mod cautionlight {
        use super::*;

        fn finding_line(id: &str) -> String {
            format!("{{\"schema\":\"cautionlight/finding@1\",\"finding_id\":\"{id}\"}}\n")
        }

        /// Cautionlight is deferred in v1: the default state says so, and is
        /// distinguishable from both "empty" and "failed".
        #[test]
        fn deferred_by_default() {
            let state = CautionlightDashboardSource::default_state();

            assert!(
                matches!(
                    state,
                    SourceState::Deferred {
                        last_attempt: None,
                        error: None
                    }
                ),
                "got {state:?}"
            );
            assert!(state.value().is_none());
        }

        /// The pipeline command and bounds are pinned, and the Afterfact bytes
        /// are shared into it rather than copied.
        #[test]
        fn default_command_pins_the_spec_contract_and_shares_stdin() {
            let bytes = Arc::new(b"{\"schema\":\"afterfact/event@2\"}\n".to_vec());

            let command = CautionlightDashboardSource::default_command(&bytes);

            assert_eq!(command.program, std::path::Path::new("cautionlight"));
            assert_eq!(command.args, ["inspect", "--stdin"]);
            assert_eq!(command.stdout_cap, 4 * 1024 * 1024);
            assert_eq!(command.stderr_cap, 256 * 1024);
            assert_eq!(command.timeout, Duration::from_secs(60));
            assert_eq!(command.stdin.as_deref(), Some(&*bytes));
            assert_eq!(
                Arc::strong_count(&bytes),
                2,
                "the payload must be shared, not copied"
            );
            assert_eq!(CAUTIONLIGHT_MAX_LINES, 20_000);
        }

        /// An override must not pay to build the real (stdin-filled) command.
        #[test]
        fn an_override_never_builds_the_default_command() {
            let bytes = Arc::new(b"unused".to_vec());
            let command = fixture(&finding_line("f1"), "", 0);

            let state = CautionlightDashboardSource::read(Some(&command), &bytes, None, now());

            assert!(state.value().is_some());
            assert_eq!(
                Arc::strong_count(&bytes),
                1,
                "the default command was constructed despite the override"
            );
        }

        /// Exit 1 is partial success: the findings survive and the coverage
        /// warnings are preserved.
        #[test]
        fn exit_1_preserves_findings_and_coverage_warnings() {
            let bytes = Arc::new(Vec::new());
            let command = fixture(&finding_line("f1"), "warning: 2 rules skipped\n", 1);

            let state = CautionlightDashboardSource::read(Some(&command), &bytes, None, now());

            let value = state.value().expect("exit 1 is partial success");
            assert_eq!(value.findings.len(), 1);
            assert_eq!(value.findings[0].schema, CAUTIONLIGHT_SCHEMA);
            assert_eq!(value.findings[0].finding_id.as_deref(), Some("f1"));
            assert_eq!(
                value.coverage_warnings.as_deref(),
                Some("warning: 2 rules skipped\n")
            );
        }

        /// Findings are display data from an untrusted process; control bytes
        /// never reach the snapshot, and a foreign schema is dropped.
        #[test]
        fn findings_are_sanitized_and_foreign_schemas_dropped() {
            let bytes = Arc::new(Vec::new());
            let stdout = concat!(
                r#"{"schema":"cautionlight/finding@1","rule":"no-\u001b[31mescape","message":"a\u0007b","file":"src/\u0000x.rs","line":7}"#,
                "\n",
                r#"{"schema":"cautionlight/finding@2","rule":"future"}"#,
                "\n",
                "not json at all\n",
            );

            let state = CautionlightDashboardSource::read(
                Some(&fixture(stdout, "", 0)),
                &bytes,
                None,
                now(),
            );

            let value = state.value().expect("fresh value");
            assert_eq!(value.findings.len(), 1, "only the known schema is retained");
            let finding = &value.findings[0];
            assert_eq!(finding.rule.as_deref(), Some("no-[31mescape"));
            assert_eq!(finding.message.as_deref(), Some("ab"));
            assert_eq!(finding.file.as_deref(), Some("src/x.rs"));
            assert_eq!(finding.line, Some(7));
        }
    }
}
