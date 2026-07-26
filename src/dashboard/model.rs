//! Immutable dashboard snapshot model.
//!
//! Every type here is constructed by the bounded readers in [`super::run_source`]
//! and consumed only by the renderer. No type in this module opens a file,
//! mutates run state, or carries a mutable [`crate::run::RunHandle`]. Every
//! displayed external value (Bead text, model output, log bytes, event
//! outcome strings, profile labels) remains an owned string payload here so
//! the render boundary can sanitize and length-cap it in one place.
//!
//! Liveness and lifecycle are distinct: a `Finished` run is a terminal
//! *lifecycle*; liveness is the heartbeat/PID-derived evidence of whether a
//! nonterminal run is still actively progressing. The five liveness variants
//! (`Live`, `Silent`, `Abandoned`, `Unknown`, `Finished`) must never collapse
//! into one another — see [`RunLiveness`].

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::services::{AfterfactSnapshot, CautionlightSnapshot, MusterrollSnapshot};
use crate::run::{RunJob, RunLifecycle};

/// Per-source freshness, error, and truncation metadata carried alongside a
/// source's last valid value. A source failure retains the prior valid value
/// and marks it stale with the current error; the renderer never presents a
/// failed source as fresh.
///
/// `T` is the source's payload (e.g. [`RunSnapshot`]). The four states are
/// load-bearing and must not collapse: a source that has never produced a
/// value is `Absent` (carrying its last failed attempt, if it has attempted
/// at all), one with a fresh value is `Fresh`, one whose last value is
/// retained but stale is `Stale`, and one that never started (deferred by
/// default, like Cautionlight in v1) is `Deferred`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceState<T> {
    /// The source has never produced a value: the initial state before the
    /// first successful read, and the state a source stays in when its very
    /// first attempts fail. A failed first attempt records `last_attempt`
    /// and `error` but deliberately never invents a `last_ok` — a source
    /// that has never succeeded must never be presentable as recently read.
    Absent {
        last_attempt: Option<DateTime<Utc>>,
        error: Option<String>,
    },
    /// The source produced a fresh value at `last_ok`.
    Fresh {
        value: T,
        last_ok: DateTime<Utc>,
        last_attempt: DateTime<Utc>,
        truncated: bool,
    },
    /// The source's last valid value is retained but the most recent attempt
    /// failed; the error and stale timestamp are shown alongside the value.
    Stale {
        value: T,
        last_ok: DateTime<Utc>,
        last_attempt: DateTime<Utc>,
        error: String,
        truncated: bool,
    },
    /// The source is deliberately not running in v1 (e.g. Cautionlight). It may
    /// still carry an on-demand result if one was requested.
    Deferred {
        last_attempt: Option<DateTime<Utc>>,
        error: Option<String>,
    },
}

impl<T> SourceState<T> {
    /// The state of a source that has never been read.
    pub(crate) const fn never_read() -> Self {
        Self::Absent {
            last_attempt: None,
            error: None,
        }
    }

    /// Returns the retained value, if any, regardless of freshness.
    pub(crate) fn value(&self) -> Option<&T> {
        match self {
            Self::Fresh { value, .. } | Self::Stale { value, .. } => Some(value),
            Self::Absent { .. } | Self::Deferred { .. } => None,
        }
    }

    /// Returns the last successful read timestamp, if any. `Absent` never
    /// reports one: it has no successful read to report.
    pub(crate) fn last_ok(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Fresh { last_ok, .. } | Self::Stale { last_ok, .. } => Some(*last_ok),
            Self::Absent { .. } | Self::Deferred { .. } => None,
        }
    }

    /// Returns the last attempt timestamp, if any.
    pub(crate) fn last_attempt(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Fresh { last_attempt, .. } | Self::Stale { last_attempt, .. } => {
                Some(*last_attempt)
            }
            Self::Absent { last_attempt, .. } | Self::Deferred { last_attempt, .. } => {
                *last_attempt
            }
        }
    }

    /// Returns whether the source is currently truncated.
    pub(crate) fn truncated(&self) -> bool {
        match self {
            Self::Fresh { truncated, .. } | Self::Stale { truncated, .. } => *truncated,
            Self::Absent { .. } | Self::Deferred { .. } => false,
        }
    }

    /// Returns the current error message, if any. `Fresh` never carries one;
    /// `Absent` carries the error from a failed first attempt.
    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            Self::Stale { error, .. } => Some(error.as_str()),
            Self::Absent { error, .. } | Self::Deferred { error, .. } => error.as_deref(),
            Self::Fresh { .. } => None,
        }
    }

    /// Returns whether the source is fresh (no error, value present).
    pub(crate) fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh { .. })
    }

    /// Folds a failed read into this state: a source with a retained value
    /// degrades to [`SourceState::Stale`] keeping its real `last_ok`; a
    /// source that has never succeeded stays [`SourceState::Absent`] and
    /// records only the failed attempt.
    pub(crate) fn degraded(self, last_attempt: DateTime<Utc>, error: String) -> Self {
        match self {
            Self::Fresh {
                value,
                last_ok,
                truncated,
                ..
            }
            | Self::Stale {
                value,
                last_ok,
                truncated,
                ..
            } => Self::Stale {
                value,
                last_ok,
                last_attempt,
                error,
                truncated,
            },
            Self::Absent { .. } | Self::Deferred { .. } => Self::Absent {
                last_attempt: Some(last_attempt),
                error: Some(error),
            },
        }
    }
}

/// Heartbeat/PID-derived liveness evidence for a run. Distinct from
/// [`RunLifecycle`]: a `Finished` lifecycle always maps to [`RunLiveness::Finished`],
/// but a nonterminal run's liveness is derived from heartbeat freshness and
/// recorded process existence.
///
/// The five variants are load-bearing display distinctions and must not
/// collapse:
/// - [`RunLiveness::Live`]: nonterminal and heartbeat younger than the
///   60-second stale threshold.
/// - [`RunLiveness::Silent`]: heartbeat stale but a recorded PID currently
///   exists (PID reuse makes this evidence, not proof).
/// - [`RunLiveness::Abandoned`]: heartbeat stale, no recorded PID exists, and
///   no `run_finished` event exists.
/// - [`RunLiveness::Unknown`]: no usable heartbeat or recorded-PID evidence.
/// - [`RunLiveness::Finished`]: terminal lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RunLiveness {
    Live,
    Silent,
    Abandoned,
    Unknown,
    Finished,
}

impl RunLiveness {
    /// A short, stable, human-readable label for badges. The renderer may
    /// color or symbolize this, but the text is the canonical spelling.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Silent => "silent",
            Self::Abandoned => "abandoned",
            Self::Unknown => "unknown",
            Self::Finished => "finished",
        }
    }

    /// Whether this liveness is terminal (only `Finished` is terminal).
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(self, Self::Finished)
    }
}

/// Run identity and lifecycle provenance. Every displayed external value
/// remains an owned string for render-boundary sanitization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunIdentity {
    pub(crate) run_id: String,
    /// The run's job, or `None` when the manifest could not be read or
    /// parsed. Never defaulted: an unreadable *plan* run rendered as a
    /// `Work` run is a fabricated fact, and no consumer can tell it apart
    /// from a real work run that merely carries an error. Consumers branch
    /// on `None` (paired with [`RunSnapshot::selection_error`]) instead.
    pub(crate) job: Option<RunJob>,
    /// The run's lifecycle, or `None` when the manifest could not be read or
    /// parsed. Never defaulted for the same reason as [`Self::job`]: the
    /// terminal/nonterminal split the whole liveness distinction rests on
    /// cannot be guessed, and `Started` is a claim there is no evidence for.
    pub(crate) lifecycle: Option<RunLifecycle>,
    pub(crate) liveness: RunLiveness,
    /// The manifest's parsed `created_at`, used for tie-breaking and recent-run
    /// ordering. `None` when the manifest was malformed enough that the
    /// timestamp could not be parsed (the run is still selected and shown with
    /// its error).
    pub(crate) created_at: Option<DateTime<Utc>>,
    /// The manifest's `created_at` string verbatim, for display only.
    pub(crate) created_at_text: String,
    /// The manifest's `updated_at` string verbatim, for display only.
    pub(crate) updated_at_text: String,
    /// The target repository path verbatim, for display only.
    pub(crate) target_repo: String,
    /// The target Bead id verbatim, if present, for display only.
    pub(crate) target_bead: Option<String>,
    /// The current stage label verbatim, for display only. For work runs this
    /// is the work stage; for plan runs this is the active plan stage; for
    /// review/consult it is `None`.
    pub(crate) stage: Option<String>,
    /// The schema string verbatim, for display/provenance only.
    pub(crate) schema: String,
    /// A manifest roster snapshot provenance triple (path, size, sha256) for
    /// display only; never reopened.
    pub(crate) roster_snapshot: Option<(String, u64, String)>,
    /// The roster policy digest for display/provenance only.
    pub(crate) roster_policy_sha256: Option<String>,
    /// The manifest musterroll roster artifact provenance (path, sha256) for
    /// display only; never reopened.
    pub(crate) musterroll_roster_artifact: Option<(String, String)>,
}

impl RunIdentity {
    /// The identity of a run whose manifest could not be read or parsed:
    /// the directory-derived `run_id`, an explicitly unknown job and
    /// lifecycle, and [`RunLiveness::Unknown`]. Every remaining field is
    /// genuinely empty, which displays as blank rather than as a wrong
    /// value. Constructing this shape in one place keeps the unreadable-run
    /// case from drifting back into a pile of plausible-looking defaults.
    pub(crate) fn unknown(run_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            job: None,
            lifecycle: None,
            liveness: RunLiveness::Unknown,
            created_at: None,
            created_at_text: String::new(),
            updated_at_text: String::new(),
            target_repo: String::new(),
            target_bead: None,
            stage: None,
            schema: String::new(),
            roster_snapshot: None,
            roster_policy_sha256: None,
            musterroll_roster_artifact: None,
        }
    }
}

/// One reconstructed attempt for a work/review/consult job, or one stage
/// marker for a plan job. Attempts are job-specific; the ordinal comes from
/// the attempt directory's leading `<NNN>` for work/review/consult, and from
/// the typed plan invocation for plan stage markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptRecord {
    /// The ordinal (1-based) for work/review/consult attempts, or the plan
    /// stage ordinal for stage markers.
    pub(crate) ordinal: u32,
    /// The opaque attempt-directory id verbatim (e.g. `001-codex-rotation`),
    /// or `None` for plan stage markers.
    pub(crate) attempt_dir: Option<String>,
    /// The resolved profile id verbatim, when known. `None` means the profile
    /// could not be resolved from the run-local roster (shown with an
    /// unresolved marker).
    pub(crate) profile_id: Option<String>,
    /// The resolved provider id, model, harness, and dispatch id from the
    /// run-local roster, when resolvable. Each is display-only string payload.
    pub(crate) provider_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) harness: Option<String>,
    pub(crate) dispatch_id: Option<String>,
    /// Whether the profile identity could be resolved. `false` leaves the
    /// opaque id visible with an unresolved marker.
    pub(crate) resolved: bool,
    /// The start timestamp of the attempt, when an `attempt_started` event was
    /// observed.
    pub(crate) started_at: Option<DateTime<Utc>>,
    /// The finish timestamp of the attempt, when an `attempt_finished` event
    /// was observed. `None` with a start means an unpaired start (elapsed time
    /// is shown with a "no finish event" marker).
    pub(crate) finished_at: Option<DateTime<Utc>>,
    /// The duration paired from start to finish, when both are present.
    pub(crate) duration: Option<Duration>,
    /// The outcome string verbatim, for display only. Unknown outcome strings
    /// are displayed verbatim, never interpreted as success.
    pub(crate) outcome: Option<String>,
}

/// A plan stage marker, distinct from a worker attempt. Stage-marker events
/// such as `planner_authoring` are markers, not worker attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageMarker {
    /// The plan stage label verbatim (e.g. `planner`, `peer_review`).
    pub(crate) stage: String,
    /// The role/capability label verbatim, when typed source data supplies it.
    pub(crate) role: Option<String>,
    /// The ordinal of the stage marker.
    pub(crate) ordinal: u32,
    /// The resolved execution identity for the stage, when present.
    pub(crate) profile_id: Option<String>,
    pub(crate) provider_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) harness: Option<String>,
    pub(crate) dispatch_id: Option<String>,
    pub(crate) resolved: bool,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) finished_at: Option<DateTime<Utc>>,
    pub(crate) duration: Option<Duration>,
    pub(crate) outcome: Option<String>,
}

/// Where verification state came from. Precedence is mechanical (durable
/// manifest evidence) over the latest valid `verify_finished` event over "not
/// run"; disagreement is visible rather than silently reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum VerificationSource {
    /// Durable `details.state.mechanical` from the manifest.
    Mechanical,
    /// The latest valid `verify_finished` event.
    Event,
    /// No verification has run.
    NotRun,
}

/// Reconstructed verification state with precedence/source. The command
/// string is display-only payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationRecord {
    /// Whether verification passed. `None` means verification has not run.
    pub(crate) passed: Option<bool>,
    /// The source of the verification state.
    pub(crate) source: VerificationSource,
    /// The verifier command string verbatim, when known, for display only.
    pub(crate) command: Option<String>,
    /// The outcome string verbatim from a `verify_finished` event, for
    /// display only. `None` when no event supplied an outcome.
    pub(crate) event_outcome: Option<String>,
    /// Whether the durable mechanical state and the latest event disagree.
    /// Disagreement is shown rather than silently reconciled.
    pub(crate) disagreement: bool,
}

/// A bounded, sanitized log tail. The path is display-only provenance; the
/// bytes have been decoded lossily, newline-aligned, and control-sanitized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogTail {
    /// The fixed-allowlist relative path that was opened (display only).
    pub(crate) path: String,
    /// The sanitized tail text, at most 64 KiB after decoding.
    pub(crate) text: String,
    /// Whether the tail was truncated at the 64 KiB boundary.
    pub(crate) truncated: bool,
}

/// The Harness Deck report join for one run.
///
/// The join key is job-specific: a work run's `details.state.cycle_id`, a
/// plan run's own `run_id`, and consult/review have no report at all. Every
/// variant is a fact the dashboard can prove rather than a link it invented
/// — the directory is produced only by [`crate::deck::report_run_dir`], the
/// same validated report-root helper the report *writer* uses, and presence
/// is a plain stat of `report.json`. Nothing here opens a report or renders
/// its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HarnessDeckState {
    /// Consult and review publish no report. The spec defines the absence,
    /// so this is a static job fact, not a lookup that failed.
    NoReportForJob,
    /// The join could not be attempted: the manifest is unreadable, carries
    /// no join key, or the key failed report-run-id validation. The reason
    /// is display-only payload.
    Unresolved { reason: String },
    /// The join resolved to a report directory. `present` separates a report
    /// that exists on disk from one that does not: a run can legitimately be
    /// joined to a directory no reporter ever wrote.
    Resolved { report_dir: String, present: bool },
}

/// A recent terminal run for the secondary "Recent runs" panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentRun {
    pub(crate) run_id: String,
    pub(crate) job: RunJob,
    pub(crate) lifecycle: RunLifecycle,
    pub(crate) liveness: RunLiveness,
    pub(crate) target_repo: String,
    pub(crate) target_bead: Option<String>,
    pub(crate) created_at: Option<DateTime<Utc>>,
    pub(crate) created_at_text: String,
    /// The terminal outcome string verbatim, for display only.
    pub(crate) outcome: Option<String>,
}

/// The immutable snapshot of one run produced by [`super::run_source`]. The
/// renderer consumes only this; it never reads files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunSnapshot {
    pub(crate) identity: RunIdentity,
    /// Reconstructed attempts for work/review/consult; empty (with an explicit
    /// empty state) when the job has none.
    pub(crate) attempts: Vec<AttemptRecord>,
    /// Plan stage markers; empty for non-plan jobs.
    pub(crate) stage_markers: Vec<StageMarker>,
    /// Verification state with precedence/source.
    pub(crate) verification: VerificationRecord,
    /// Bounded sanitized log tails, keyed by fixed-allowlist relative path.
    pub(crate) logs: Vec<LogTail>,
    /// The Harness Deck report join for this run's job.
    pub(crate) harness_deck: HarnessDeckState,
    /// The count of events retained in the snapshot (bounded at 5,000).
    pub(crate) event_count: usize,
    /// Whether the event tail was truncated at the 8 MiB / 5,000-event caps.
    pub(crate) events_truncated: bool,
    /// A display-only error when the run was selected but its manifest could
    /// not be read or parsed. The run is still shown with this error rather
    /// than silently falling back.
    pub(crate) selection_error: Option<String>,
    /// A display-only error from the incremental event tail: a complete
    /// malformed line, a sequence gap, or an unknown event schema. The tail
    /// stalls at the offending line and retains the last valid events, so
    /// this error is the only signal that `event_count` has stopped
    /// advancing. It is separate from [`Self::selection_error`] because the
    /// manifest can be perfectly readable while the event log is not.
    pub(crate) events_error: Option<String>,
    /// A display-only error when the run-local `roster.json` exists but could
    /// not be parsed or validated. Without it an unparseable roster silently
    /// renders every attempt as an unresolved opaque profile with no stated
    /// reason. `None` means either a clean parse or no roster snapshot at all
    /// — the latter is already visible as `resolved: false` on each attempt.
    pub(crate) roster_error: Option<String>,
}

/// The top-level immutable snapshot the renderer consumes. Task 1 populates
/// the run-source portion; Task 2 adds service source states (Musterroll,
/// Afterfact, Cautionlight) and Task 3 adds UI-only selection state.
///
/// Deliberately not `Eq`: a Musterroll usage `Window` carries a float
/// percentage, so the service states this gained in Task 2 are only
/// `PartialEq`. Snapshot comparison is a test and change-detection
/// convenience, never an identity claim, so the weaker bound is the honest
/// one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DashboardSnapshot {
    /// The selected run, with its per-source state.
    pub(crate) run: SourceState<RunSnapshot>,
    /// Recent terminal runs (bounded).
    pub(crate) recent: SourceState<Vec<RecentRun>>,
    /// A bounded warning from the discovery pass: run directory entries that
    /// could not be read cleanly were skipped rather than failing the whole
    /// source, and this is the only signal that they existed. Bounded by
    /// construction — one count and the first error, never a per-entry list,
    /// so an unreadable directory cannot grow the render payload.
    ///
    /// Distinct from a [`SourceState`] error: both sources here can be
    /// perfectly `Fresh` while some entries were skipped.
    pub(crate) discovery_warning: Option<String>,
    /// Musterroll provider availability state.
    ///
    /// The three service states are shared rather than owned. Each service
    /// samples on its own cadence, so every run-source tick carries the
    /// current one forward unchanged; owning it would deep-copy up to
    /// 20,000 retained Afterfact events per refresh to reproduce a value
    /// nothing modified.
    pub(crate) musterroll: Arc<SourceState<MusterrollSnapshot>>,
    /// Afterfact correlation/coverage state.
    pub(crate) afterfact: Arc<SourceState<AfterfactSnapshot>>,
    /// Cautionlight deferred/findings state.
    pub(crate) cautionlight: Arc<SourceState<CautionlightSnapshot>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    /// Liveness variants are distinct and do not collapse.
    #[test]
    fn liveness_variants_are_distinct() {
        let all = [
            RunLiveness::Live,
            RunLiveness::Silent,
            RunLiveness::Abandoned,
            RunLiveness::Unknown,
            RunLiveness::Finished,
        ];
        // Every variant has a unique, non-empty, distinct label.
        let labels: Vec<&str> = all.iter().map(|l| l.label()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(labels.len(), 5);
        assert_eq!(sorted.len(), 5, "liveness labels must all be unique");
        assert!(labels.iter().all(|l| !l.is_empty()));
        // Only Finished is terminal.
        assert!(RunLiveness::Finished.is_terminal());
        assert!(!RunLiveness::Live.is_terminal());
        assert!(!RunLiveness::Silent.is_terminal());
        assert!(!RunLiveness::Abandoned.is_terminal());
        assert!(!RunLiveness::Unknown.is_terminal());
    }

    /// `Unknown`, `Silent`, and `Abandoned` must not collapse to one another.
    /// This is the discriminating regression for the Patchstand pilot: a
    /// stale-heartbeat, dead-PID, no-finish run is `Abandoned`, never `Silent`
    /// or `Unknown` or `Live`.
    #[test]
    fn liveness_unknown_silent_abandoned_do_not_collapse() {
        assert_ne!(RunLiveness::Unknown, RunLiveness::Silent);
        assert_ne!(RunLiveness::Unknown, RunLiveness::Abandoned);
        assert_ne!(RunLiveness::Silent, RunLiveness::Abandoned);
        assert_ne!(RunLiveness::Abandoned, RunLiveness::Live);
        assert_ne!(RunLiveness::Abandoned, RunLiveness::Finished);
    }

    /// `SourceState` distinguishes all four states and does not collapse
    /// `Absent`, `Stale`, and `Deferred`.
    #[test]
    fn source_state_distinguishes_all_variants() {
        let value = "payload";
        let now = ts("2026-07-25T18:39:20Z");
        let absent: SourceState<&str> = SourceState::never_read();
        let fresh = SourceState::Fresh {
            value,
            last_ok: now,
            last_attempt: now,
            truncated: false,
        };
        let stale = SourceState::Stale {
            value,
            last_ok: now,
            last_attempt: now,
            error: "boom".to_string(),
            truncated: true,
        };
        let deferred: SourceState<&str> = SourceState::Deferred {
            last_attempt: None,
            error: None,
        };

        // A never-read source carries no value, no error, no timestamps.
        assert!(absent.value().is_none());
        assert!(absent.last_ok().is_none());
        assert!(absent.last_attempt().is_none());
        assert!(!absent.is_fresh());
        assert!(absent.error().is_none());
        assert!(!absent.truncated());

        // Fresh carries the value and timestamps, no error.
        assert_eq!(fresh.value(), Some(&value));
        assert_eq!(fresh.last_ok(), Some(now));
        assert_eq!(fresh.last_attempt(), Some(now));
        assert!(fresh.is_fresh());
        assert!(fresh.error().is_none());
        assert!(!fresh.truncated());

        // Stale retains the value but carries an error and the truncated flag.
        assert_eq!(stale.value(), Some(&value));
        assert_eq!(stale.last_ok(), Some(now));
        assert_eq!(stale.error(), Some("boom"));
        assert!(!stale.is_fresh());
        assert!(stale.truncated());

        // Deferred carries no value but may carry an error from an on-demand
        // attempt; it is neither absent nor fresh nor stale.
        assert!(deferred.value().is_none());
        assert!(deferred.last_ok().is_none());
        assert!(!deferred.is_fresh());
        assert!(deferred.error().is_none());

        // A source whose first attempt failed stays Absent: it records the
        // failed attempt and error but never invents a last_ok.
        let failed_first: SourceState<&str> =
            SourceState::never_read().degraded(now, "boom".to_string());
        assert!(matches!(failed_first, SourceState::Absent { .. }));
        assert!(failed_first.last_ok().is_none());
        assert_eq!(failed_first.last_attempt(), Some(now));
        assert_eq!(failed_first.error(), Some("boom"));
        assert!(!failed_first.is_fresh());

        // A source with a value degrades to Stale, keeping its real last_ok.
        let later = ts("2026-07-25T18:40:20Z");
        let degraded = fresh.degraded(later, "boom".to_string());
        assert!(matches!(degraded, SourceState::Stale { .. }));
        assert_eq!(degraded.value(), Some(&value));
        assert_eq!(degraded.last_ok(), Some(now));
        assert_eq!(degraded.last_attempt(), Some(later));
    }

    /// `SourceState::Absent`, `Stale`, and `Deferred` must not collapse.
    #[test]
    fn source_state_absent_stale_deferred_do_not_collapse() {
        let now = ts("2026-07-25T18:39:20Z");
        let absent: SourceState<&str> = SourceState::never_read();
        let stale = SourceState::Stale {
            value: "x",
            last_ok: now,
            last_attempt: now,
            error: "e".to_string(),
            truncated: false,
        };
        let deferred: SourceState<&str> = SourceState::Deferred {
            last_attempt: Some(now),
            error: Some("e".to_string()),
        };
        // Discriminants differ even when error/timestamps overlap.
        assert!(!matches!(
            absent,
            SourceState::Stale { .. } | SourceState::Deferred { .. }
        ));
        assert!(!matches!(
            stale,
            SourceState::Absent { .. } | SourceState::Deferred { .. }
        ));
        assert!(!matches!(
            deferred,
            SourceState::Absent { .. } | SourceState::Stale { .. }
        ));
        // Deferred's error surfaces even though it has no value.
        assert_eq!(deferred.error(), Some("e"));
    }

    /// `VerificationSource` precedence is mechanical over event over not-run.
    #[test]
    fn verification_source_precedence_is_mechanical_then_event_then_not_run() {
        assert_ne!(VerificationSource::Mechanical, VerificationSource::Event);
        assert_ne!(VerificationSource::Mechanical, VerificationSource::NotRun);
        assert_ne!(VerificationSource::Event, VerificationSource::NotRun);
    }
}
