//! Read-only evidence service adapters for the Undertake dashboard.
//!
//! Service adapters consume read-only service APIs and bounded subprocess commands
//! to produce immutable service snapshots (`ServiceSnapshot`, `MusterrollSnapshot`,
//! `AfterfactSnapshot`, `CautionlightSnapshot`). They never mutate service state,
//! run automatic background mutations, or open run directories for write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::dashboard::model::SourceState;
use crate::dashboard::process::BoundedCommand;
use crate::musterroll::{Availability, MusterrollClient, StatusReport, Window};

/// Sanitizes control characters from a single-line string (removes all control chars including newlines).
pub(crate) fn sanitize_single_line(text: &str) -> String {
    text.chars().filter(|&c| !c.is_control()).collect()
}

/// Sanitizes control characters from multi-line text (preserves newlines, removes other control chars).
pub(crate) fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect()
}

// ============================================================================
// Musterroll Adapter
// ============================================================================

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
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn read<C: MusterrollClient + ?Sized>(
        &self,
        client: &C,
        previous: Option<&SourceState<MusterrollSnapshot>>,
        now: DateTime<Utc>,
    ) -> SourceState<MusterrollSnapshot> {
        match client.status() {
            Ok(report) => {
                let snapshot = convert_status_report(report);
                SourceState::Fresh {
                    value: snapshot,
                    last_ok: now,
                    last_attempt: now,
                    truncated: false,
                }
            }
            Err(err) => {
                let err_msg = err.to_string();
                match previous {
                    Some(SourceState::Fresh { value, last_ok, .. })
                    | Some(SourceState::Stale { value, last_ok, .. }) => SourceState::Stale {
                        value: value.clone(),
                        last_ok: *last_ok,
                        last_attempt: now,
                        error: err_msg,
                        truncated: false,
                    },
                    _ => SourceState::Absent {
                        last_attempt: Some(now),
                        error: Some(err_msg),
                    },
                }
            }
        }
    }
}

fn convert_status_report(report: StatusReport) -> MusterrollSnapshot {
    let mut providers = BTreeMap::new();

    for (name, provider) in report.providers {
        let clean_name = sanitize_single_line(&name);
        let clean_source = sanitize_single_line(&provider.source);
        let clean_checked_at = sanitize_single_line(&provider.checked_at);
        let clean_data_as_of = provider.data_as_of.map(|s| sanitize_single_line(&s));
        let clean_expires_at = provider.expires_at.map(|s| sanitize_single_line(&s));
        let clean_reason = provider.reason.map(|s| sanitize_single_line(&s));

        let clean_windows = provider
            .windows
            .into_iter()
            .map(|w| Window {
                label: sanitize_single_line(&w.label),
                percent: w.percent,
                reset_at: w.reset_at.map(|s| sanitize_single_line(&s)),
            })
            .collect();

        let mut clean_extra = BTreeMap::new();
        for (k, v) in provider.extra {
            if ALLOWLISTED_EXTRA_KEYS.contains(&k.as_str()) {
                let clean_k = sanitize_single_line(&k);
                let val_str = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                let clean_v = sanitize_single_line(&val_str);
                clean_extra.insert(clean_k, clean_v);
            }
        }

        providers.insert(
            clean_name,
            ProviderStatusSnapshot {
                availability: provider.availability,
                source: clean_source,
                checked_at: clean_checked_at,
                data_as_of: clean_data_as_of,
                expires_at: clean_expires_at,
                windows: clean_windows,
                reason: clean_reason,
                extra: clean_extra,
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

const AFTERFACT_SCHEMA: &str = "afterfact/event@2";
const AFTERFACT_MAX_LINES: usize = 20_000;
const AFTERFACT_STDOUT_CAP: usize = 4 * 1024 * 1024;
const AFTERFACT_STDERR_CAP: usize = 256 * 1024;

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
    pub(crate) coverage_gap_summary: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AfterfactDashboardSource;

impl AfterfactDashboardSource {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn read(
        &self,
        command_override: Option<&BoundedCommand>,
        run_dir: Option<&Path>,
        worker_commits: &[String],
        previous: Option<&SourceState<AfterfactSnapshot>>,
        now: DateTime<Utc>,
    ) -> SourceState<AfterfactSnapshot> {
        let default_cmd = BoundedCommand::new("afterfact")
            .args(["events", "--since", "1h"])
            .stdout_cap(AFTERFACT_STDOUT_CAP)
            .stderr_cap(AFTERFACT_STDERR_CAP)
            .timeout(Duration::from_secs(60));

        let cmd = command_override.unwrap_or(&default_cmd);

        match cmd.run() {
            Ok(outcome) => {
                if outcome.timed_out || outcome.exit_code.map_or(true, |code| code >= 2) {
                    let err = if outcome.timed_out {
                        "afterfact events timed out".to_string()
                    } else {
                        format!("afterfact events failed with exit code {:?}", outcome.exit_code)
                    };
                    return Self::handle_error(previous, now, err);
                }

                let (events, line_truncated) = parse_afterfact_stdout(&outcome.stdout);
                let truncated = outcome.stdout_truncated || line_truncated;

                let (correlated_count, uncorrelated_count) =
                    correlate_events(&events, run_dir, worker_commits);

                let coverage_gap_summary = if outcome.exit_code == Some(1) {
                    let stderr_str = String::from_utf8_lossy(&outcome.stderr);
                    let sanitized = sanitize_text(&stderr_str);
                    if sanitized.trim().is_empty() {
                        None
                    } else {
                        Some(sanitized)
                    }
                } else {
                    None
                };

                let snapshot = AfterfactSnapshot {
                    events,
                    correlated_count,
                    uncorrelated_count,
                    coverage_gap_summary,
                };

                SourceState::Fresh {
                    value: snapshot,
                    last_ok: now,
                    last_attempt: now,
                    truncated,
                }
            }
            Err(err) => Self::handle_error(previous, now, format!("spawn afterfact error: {err}")),
        }
    }

    fn handle_error(
        previous: Option<&SourceState<AfterfactSnapshot>>,
        now: DateTime<Utc>,
        err_msg: String,
    ) -> SourceState<AfterfactSnapshot> {
        match previous {
            Some(SourceState::Fresh { value, last_ok, .. })
            | Some(SourceState::Stale { value, last_ok, .. }) => SourceState::Stale {
                value: value.clone(),
                last_ok: *last_ok,
                last_attempt: now,
                error: err_msg,
                truncated: false,
            },
            _ => SourceState::Absent {
                last_attempt: Some(now),
                error: Some(err_msg),
            },
        }
    }
}

fn parse_afterfact_stdout(bytes: &[u8]) -> (Vec<AfterfactEventRecord>, bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut events = Vec::new();
    let mut line_count = 0;
    let mut truncated = false;

    for line in text.lines() {
        if line_count >= AFTERFACT_MAX_LINES {
            truncated = true;
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        line_count += 1;
        if let Ok(rec) = serde_json::from_str::<AfterfactEventRecord>(line) {
            if rec.schema == AFTERFACT_SCHEMA {
                events.push(rec);
            }
        }
    }

    (events, truncated)
}

fn correlate_events(
    events: &[AfterfactEventRecord],
    run_dir: Option<&Path>,
    worker_commits: &[String],
) -> (usize, usize) {
    let canonical_run_dir = run_dir.and_then(|p| std::fs::canonicalize(p).ok().or_else(|| Some(p.to_path_buf())));

    let mut correlated = 0;
    let mut uncorrelated = 0;

    for event in events {
        let mut is_correlated = false;

        // 1. Exact commit match
        if let Some(commit) = &event.git_commit {
            if worker_commits.iter().any(|c| c == commit) {
                is_correlated = true;
            }
        }

        // 2. Exact canonical prefix match of event.repo.cwd against run_dir
        if !is_correlated {
            if let (Some(event_repo), Some(target_dir)) = (&event.repo, &canonical_run_dir) {
                let event_path = PathBuf::from(&event_repo.cwd);
                let canonical_event = std::fs::canonicalize(&event_path).unwrap_or(event_path);

                // Exact canonical prefix match using Path::starts_with
                if target_dir.starts_with(&canonical_event) || canonical_event.starts_with(target_dir) {
                    is_correlated = true;
                }
            }
        }

        if is_correlated {
            correlated += 1;
        } else {
            uncorrelated += 1;
        }
    }

    (correlated, uncorrelated)
}

// ============================================================================
// Cautionlight Adapter
// ============================================================================

const CAUTIONLIGHT_SCHEMA: &str = "cautionlight/finding@1";
const CAUTIONLIGHT_MAX_LINES: usize = 20_000;
const CAUTIONLIGHT_STDOUT_CAP: usize = 4 * 1024 * 1024;
const CAUTIONLIGHT_STDERR_CAP: usize = 256 * 1024;

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
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn default_state() -> SourceState<CautionlightSnapshot> {
        SourceState::Deferred {
            last_attempt: None,
            error: None,
        }
    }

    pub(crate) fn read(
        &self,
        command_override: Option<&BoundedCommand>,
        afterfact_bytes: &[u8],
        previous: Option<&SourceState<CautionlightSnapshot>>,
        now: DateTime<Utc>,
    ) -> SourceState<CautionlightSnapshot> {
        let default_cmd = BoundedCommand::new("cautionlight")
            .args(["inspect", "--stdin"])
            .stdin(afterfact_bytes.to_vec())
            .stdout_cap(CAUTIONLIGHT_STDOUT_CAP)
            .stderr_cap(CAUTIONLIGHT_STDERR_CAP)
            .timeout(Duration::from_secs(60));

        let cmd = command_override.unwrap_or(&default_cmd);

        match cmd.run() {
            Ok(outcome) => {
                if outcome.timed_out || outcome.exit_code.map_or(true, |code| code >= 2) {
                    let err = if outcome.timed_out {
                        "cautionlight inspect timed out".to_string()
                    } else {
                        format!("cautionlight inspect failed with exit code {:?}", outcome.exit_code)
                    };
                    return Self::handle_error(previous, now, err);
                }

                let (findings, line_truncated) = parse_cautionlight_stdout(&outcome.stdout);
                let truncated = outcome.stdout_truncated || line_truncated;

                let coverage_warnings = if outcome.exit_code == Some(1) {
                    let stderr_str = String::from_utf8_lossy(&outcome.stderr);
                    let sanitized = sanitize_text(&stderr_str);
                    if sanitized.trim().is_empty() {
                        None
                    } else {
                        Some(sanitized)
                    }
                } else {
                    None
                };

                let snapshot = CautionlightSnapshot {
                    findings,
                    coverage_warnings,
                };

                SourceState::Fresh {
                    value: snapshot,
                    last_ok: now,
                    last_attempt: now,
                    truncated,
                }
            }
            Err(err) => Self::handle_error(previous, now, format!("spawn cautionlight error: {err}")),
        }
    }

    fn handle_error(
        previous: Option<&SourceState<CautionlightSnapshot>>,
        now: DateTime<Utc>,
        err_msg: String,
    ) -> SourceState<CautionlightSnapshot> {
        match previous {
            Some(SourceState::Fresh { value, last_ok, .. })
            | Some(SourceState::Stale { value, last_ok, .. }) => SourceState::Stale {
                value: value.clone(),
                last_ok: *last_ok,
                last_attempt: now,
                error: err_msg,
                truncated: false,
            },
            _ => SourceState::Absent {
                last_attempt: Some(now),
                error: Some(err_msg),
            },
        }
    }
}

fn parse_cautionlight_stdout(bytes: &[u8]) -> (Vec<CautionlightFindingRecord>, bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut findings = Vec::new();
    let mut line_count = 0;
    let mut truncated = false;

    for line in text.lines() {
        if line_count >= CAUTIONLIGHT_MAX_LINES {
            truncated = true;
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        line_count += 1;
        if let Ok(rec) = serde_json::from_str::<CautionlightFindingRecord>(line) {
            if rec.schema == CAUTIONLIGHT_SCHEMA {
                findings.push(rec);
            }
        }
    }

    (findings, truncated)
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
    use crate::musterroll::{MusterrollError, Result as MusterrollResult, RosterSnapshot};
    use serde_json::json;

    struct MockMusterrollClient {
        report: MusterrollResult<StatusReport>,
    }

    impl MusterrollClient for MockMusterrollClient {
        fn status(&self) -> MusterrollResult<StatusReport> {
            match &self.report {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(MusterrollError::command(e.to_string())),
            }
        }
    }

    pub(crate) mod musterroll {
        use super::*;

        #[test]
        fn typed_availability_distinctions() {
            let json_data = json!({
                "schema": "musterroll/status@2",
                "checked_at": "2026-07-25T12:00:00Z",
                "providers": {
                    "anthropic": {
                        "availability": "healthy",
                        "source": "api",
                        "checked_at": "2026-07-25T12:00:00Z",
                        "data_as_of": null,
                        "expires_at": null,
                        "windows": [],
                        "reason": null,
                        "extra": {}
                    },
                    "codex": {
                        "availability": "caution",
                        "source": "api",
                        "checked_at": "2026-07-25T12:00:00Z",
                        "data_as_of": null,
                        "expires_at": null,
                        "windows": [],
                        "reason": "near limit",
                        "extra": {}
                    },
                    "opencode": {
                        "availability": "exhausted",
                        "source": "api",
                        "checked_at": "2026-07-25T12:00:00Z",
                        "data_as_of": null,
                        "expires_at": null,
                        "windows": [],
                        "reason": "rate limited",
                        "extra": {}
                    },
                    "unknown_provider": {
                        "availability": "unknown",
                        "source": "probe",
                        "checked_at": "2026-07-25T12:00:00Z",
                        "data_as_of": null,
                        "expires_at": null,
                        "windows": [],
                        "reason": null,
                        "extra": {}
                    }
                }
            });

            let report: StatusReport = serde_json::from_value(json_data).unwrap();
            let client = MockMusterrollClient { report: Ok(report) };
            let source = MusterrollDashboardSource::new();
            let now = Utc::now();
            let state = source.read(&client, None, now);

            let value = state.value().unwrap();
            assert_eq!(value.providers.get("anthropic").unwrap().availability, Availability::Healthy);
            assert_eq!(value.providers.get("codex").unwrap().availability, Availability::Caution);
            assert_eq!(value.providers.get("opencode").unwrap().availability, Availability::Exhausted);
            assert_eq!(value.providers.get("unknown_provider").unwrap().availability, Availability::Unknown);
        }

        #[test]
        fn allowlisted_extra_keys_retained_others_dropped() {
            let json_data = json!({
                "schema": "musterroll/status@2",
                "checked_at": "2026-07-25T12:00:00Z",
                "providers": {
                    "anthropic": {
                        "availability": "healthy",
                        "source": "api",
                        "checked_at": "2026-07-25T12:00:00Z",
                        "data_as_of": null,
                        "expires_at": null,
                        "windows": [],
                        "reason": null,
                        "extra": {
                            "observation_expiry_basis": "fixed_window",
                            "observation_model": "claude-3-5-sonnet",
                            "secret_token": "shh_secret",
                            "arbitrary_key": "drop_me"
                        }
                    }
                }
            });

            let report: StatusReport = serde_json::from_value(json_data).unwrap();
            let client = MockMusterrollClient { report: Ok(report) };
            let source = MusterrollDashboardSource::new();
            let now = Utc::now();
            let state = source.read(&client, None, now);

            let value = state.value().unwrap();
            let provider = value.providers.get("anthropic").unwrap();
            assert_eq!(provider.extra.len(), 2);
            assert_eq!(provider.extra.get("observation_expiry_basis").unwrap(), "fixed_window");
            assert_eq!(provider.extra.get("observation_model").unwrap(), "claude-3-5-sonnet");
            assert!(!provider.extra.contains_key("secret_token"));
            assert!(!provider.extra.contains_key("arbitrary_key"));
        }

        #[test]
        fn control_bytes_sanitized() {
            let json_data = json!({
                "schema": "musterroll/status@2\x07",
                "checked_at": "2026-07-25T12:00:00Z\x1b[31m",
                "providers": {
                    "anthropic\x00": {
                        "availability": "healthy",
                        "source": "api\x07",
                        "checked_at": "2026-07-25T12:00:00Z",
                        "data_as_of": null,
                        "expires_at": null,
                        "windows": [],
                        "reason": "bad\x1b[0m",
                        "extra": {
                            "observation_expiry_basis": "fixed\x07_window"
                        }
                    }
                }
            });

            let report: StatusReport = serde_json::from_value(json_data).unwrap();
            let client = MockMusterrollClient { report: Ok(report) };
            let source = MusterrollDashboardSource::new();
            let now = Utc::now();
            let state = source.read(&client, None, now);

            let value = state.value().unwrap();
            assert_eq!(value.schema, "musterroll/status@2");
            let provider = value.providers.get("anthropic").unwrap();
            assert_eq!(provider.source, "api");
            assert_eq!(provider.reason.as_deref(), Some("bad[0m"));
            assert_eq!(provider.extra.get("observation_expiry_basis").unwrap(), "fixed_window");
        }
    }

    pub(crate) mod afterfact {
        use super::*;

        #[test]
        fn exit_0_and_exit_1_and_exit_2_semantics() {
            let now = Utc::now();
            let source = AfterfactDashboardSource::new();

            // Exit 0: Complete success
            let script_0 = r#"import sys; sys.stdout.write('{"schema":"afterfact/event@2","event_id":"e1"}\n'); sys.exit(0)"#;
            let cmd_0 = BoundedCommand::new("python3").args(["-c", script_0]);
            let state_0 = source.read(Some(&cmd_0), None, &[], None, now);
            assert!(matches!(state_0, SourceState::Fresh { .. }));
            let val_0 = state_0.value().unwrap();
            assert_eq!(val_0.events.len(), 1);
            assert_eq!(val_0.coverage_gap_summary, None);

            // Exit 1: Partial success with coverage gap in stderr
            let script_1 = r#"import sys; sys.stdout.write('{"schema":"afterfact/event@2","event_id":"e2"}\n'); sys.stderr.write('coverage gap detected\n'); sys.exit(1)"#;
            let cmd_1 = BoundedCommand::new("python3").args(["-c", script_1]);
            let state_1 = source.read(Some(&cmd_1), None, &[], None, now);
            assert!(matches!(state_1, SourceState::Fresh { .. }));
            let val_1 = state_1.value().unwrap();
            assert_eq!(val_1.events.len(), 1);
            assert_eq!(val_1.coverage_gap_summary.as_deref(), Some("coverage gap detected\n"));

            // Exit 2: Error
            let script_2 = r#"import sys; sys.exit(2)"#;
            let cmd_2 = BoundedCommand::new("python3").args(["-c", script_2]);
            let state_2 = source.read(Some(&cmd_2), None, &[], None, now);
            assert!(matches!(state_2, SourceState::Absent { .. }));
        }

        #[test]
        fn prefix_and_commit_correlation_and_substring_rejection() {
            let temp_dir = std::env::temp_dir();
            let run_dir = temp_dir.join("undertake_test_run_repo");
            let _ = std::fs::create_dir_all(&run_dir);

            let event1 = AfterfactEventRecord {
                schema: AFTERFACT_SCHEMA.to_string(),
                event_id: Some("e1".to_string()),
                timestamp: None,
                repo: Some(AfterfactRepo { cwd: run_dir.to_str().unwrap().to_string() }),
                git_commit: None,
                kind: None,
                summary: None,
            };

            let event2 = AfterfactEventRecord {
                schema: AFTERFACT_SCHEMA.to_string(),
                event_id: Some("e2".to_string()),
                timestamp: None,
                repo: None,
                git_commit: Some("c123456".to_string()),
                kind: None,
                summary: None,
            };

            // Substring rejection test: repo cwd is prefix substring of different dir
            let event3 = AfterfactEventRecord {
                schema: AFTERFACT_SCHEMA.to_string(),
                event_id: Some("e3".to_string()),
                timestamp: None,
                repo: Some(AfterfactRepo { cwd: format!("{}-other", run_dir.to_str().unwrap()) }),
                git_commit: None,
                kind: None,
                summary: None,
            };

            let events = vec![event1, event2, event3];
            let worker_commits = vec!["c123456".to_string()];
            let (corr, uncorr) = correlate_events(&events, Some(&run_dir), &worker_commits);

            assert_eq!(corr, 2);
            assert_eq!(uncorr, 1);

            let _ = std::fs::remove_dir_all(&run_dir);
        }
    }

    pub(crate) mod cautionlight {
        use super::*;

        #[test]
        fn deferred_by_default_and_exit_1_coverage_warnings() {
            let source = CautionlightDashboardSource::new();
            let default_state = CautionlightDashboardSource::default_state();
            assert!(matches!(default_state, SourceState::Deferred { .. }));

            let now = Utc::now();
            let script = r#"import sys; sys.stdout.write('{"schema":"cautionlight/finding@1","finding_id":"f1"}\n'); sys.stderr.write('warning: gap\n'); sys.exit(1)"#;
            let cmd = BoundedCommand::new("python3").args(["-c", script]);
            let state = source.read(Some(&cmd), b"stdin_data", None, now);

            assert!(matches!(state, SourceState::Fresh { .. }));
            let val = state.value().unwrap();
            assert_eq!(val.findings.len(), 1);
            assert_eq!(val.findings[0].schema, CAUTIONLIGHT_SCHEMA);
            assert_eq!(val.coverage_warnings.as_deref(), Some("warning: gap\n"));
        }
    }
}
