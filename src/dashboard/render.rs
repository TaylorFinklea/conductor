//! Pure, read-only rendering for the Undertake dashboard.
//!
//! The renderer consumes only a [`DashboardSnapshot`] plus UI-only selection
//! state ([`UiState`]); it never opens a file, runs a command, or receives a
//! mutable run handle. Every externally-sourced string (Bead text, model
//! output, log bytes, event outcomes, provider extras, coverage summaries)
//! passes through [`display_text`] or [`display_block`] — the render
//! boundary's one sanitization/length-cap function, exposed as two thin call
//! shapes for single-line versus multi-line text — before it reaches a
//! widget. Color is supplemental only: every state distinction is also
//! carried in text or symbols, proven by rendering with `color: false`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use chrono::{DateTime, Utc};

use std::fmt::Write as _;

use super::model::{
    AttemptRecord, DashboardSnapshot, RecentRun, RunLiveness, RunSnapshot, SourceState,
    StageMarker, VerificationRecord, VerificationSource,
};
use super::sanitize::{sanitize_single_line, sanitize_text};
use super::services::ProviderStatusSnapshot;
use crate::musterroll::Window;
use crate::run::{RunJob, RunLifecycle};

/// Below this width or height the layout cannot stay legible; render a
/// resize message instead of a cramped or garbled screen.
pub(crate) const MIN_WIDTH: u16 = 60;
pub(crate) const MIN_HEIGHT: u16 = 16;

/// At or above this width the main area splits into a focused detail pane
/// plus an overview column summarizing the other three panels. Below it a
/// sidebar narrow enough to fit would be too narrow to say anything, so the
/// layout stays a single focused panel. Derived from the two panes rather
/// than guessed: the threshold *is* the point at which both are usable.
pub(crate) const NORMAL_MIN_WIDTH: u16 = MIN_DETAIL_WIDTH + OVERVIEW_WIDTH;

/// The narrowest focused detail pane worth splitting for.
const MIN_DETAIL_WIDTH: u16 = 56;

/// The overview column's width. Fixed rather than proportional, so the
/// summaries read identically at every normal-or-wider size and all extra
/// width goes to the panel that has the focus.
const OVERVIEW_WIDTH: u16 = 44;

/// How the main area is divided. Spec §205 requires both layouts, and the
/// choice is deterministic in the frame width alone — height never binds,
/// because the overview column is a vertical stack of three panes that
/// still fits the main area at [`MIN_HEIGHT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutMode {
    /// One focused panel filling the main area.
    Compact,
    /// Focused detail pane, plus an overview column of the other three.
    Normal,
}

impl LayoutMode {
    pub(crate) const fn for_width(width: u16) -> Self {
        if width >= NORMAL_MIN_WIDTH {
            Self::Normal
        } else {
            Self::Compact
        }
    }
}

/// The dashboard's focusable panels. `Tab`/`Shift-Tab` cycle through them.
/// Help is a separate overlay toggled by `?`, not a fifth tab stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Panel {
    ActiveRun,
    Providers,
    Evidence,
    RecentRuns,
}

impl Panel {
    pub(crate) const ALL: [Panel; 4] = [
        Panel::ActiveRun,
        Panel::Providers,
        Panel::Evidence,
        Panel::RecentRuns,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Panel::ActiveRun => "Active Run",
            Panel::Providers => "Providers",
            Panel::Evidence => "Evidence",
            Panel::RecentRuns => "Recent Runs",
        }
    }

    /// Cycles forward, wrapping. Used by `Tab`.
    pub(crate) fn next(self) -> Panel {
        let index = Self::ALL
            .iter()
            .position(|panel| *panel == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Cycles backward, wrapping. Used by `Shift-Tab`.
    pub(crate) fn previous(self) -> Panel {
        let index = Self::ALL
            .iter()
            .position(|panel| *panel == self)
            .unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// UI-only selection state the renderer needs: which panel is focused, which
/// row is highlighted in each list, whether Help is showing, and whether
/// color is enabled. No reader, command, or mutable run handle lives here —
/// [`super::runtime`] owns the fuller runtime state and derives this view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UiState {
    pub(crate) focus: Panel,
    pub(crate) attempt_selected: usize,
    pub(crate) recent_selected: usize,
    pub(crate) help_visible: bool,
    pub(crate) color: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            focus: Panel::ActiveRun,
            attempt_selected: 0,
            recent_selected: 0,
            help_visible: false,
            color: true,
        }
    }
}

const TRUNCATION_MARKER: char = '…';

/// Shared core of the render boundary's sanitization/length cap: truncate to
/// `max_chars`, marking truncation with a single trailing character rather
/// than silently cutting mid-thought.
fn cap_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let budget = max_chars.saturating_sub(1);
    let mut capped: String = text.chars().take(budget).collect();
    capped.push(TRUNCATION_MARKER);
    capped
}

/// Sanitizes and caps a value rendered on one line: a run id, provider name,
/// timestamp, path, or short label. The one function every single-line
/// external string passes through before reaching a widget.
pub(crate) fn display_text(text: &str, max_chars: usize) -> String {
    cap_chars(&sanitize_single_line(text), max_chars)
}

/// Sanitizes and caps multi-line text (log tails, coverage summaries),
/// preserving line structure. Shares [`cap_chars`] with [`display_text`];
/// the two differ only in which sanitizer feeds it, matching
/// [`super::sanitize`]'s own single-line/block split.
pub(crate) fn display_block(text: &str, max_chars: usize) -> String {
    cap_chars(&sanitize_text(text), max_chars)
}

/// Draws one frame. Below the minimum size, only a resize message is drawn —
/// no layout math is attempted against a too-small `Rect`.
pub(crate) fn render(
    frame: &mut Frame,
    snapshot: &DashboardSnapshot,
    ui: &UiState,
    now: DateTime<Utc>,
) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_resize_message(frame, area);
        return;
    }

    let [banner_area, tabs_area, main_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(area);

    render_banner(frame, banner_area, snapshot, ui);
    render_tabs(frame, tabs_area, ui);
    match LayoutMode::for_width(area.width) {
        LayoutMode::Compact => render_panel(frame, main_area, ui.focus, snapshot, ui, now),
        LayoutMode::Normal => {
            let [detail_area, overview_area] =
                Layout::horizontal([Constraint::Fill(1), Constraint::Length(OVERVIEW_WIDTH)])
                    .areas(main_area);
            render_panel(frame, detail_area, ui.focus, snapshot, ui, now);
            render_overview_column(frame, overview_area, snapshot, ui, now);
        }
    }
    render_footer(frame, footer_area, snapshot);

    if ui.help_visible {
        render_help_overlay(frame, area);
    }
}

/// Draws one panel's full detail into `area`. The focused panel gets this
/// treatment at every size; under [`LayoutMode::Normal`] the other three
/// get [`overview_lines`] instead.
fn render_panel(
    frame: &mut Frame,
    area: Rect,
    panel: Panel,
    snapshot: &DashboardSnapshot,
    ui: &UiState,
    now: DateTime<Utc>,
) {
    match panel {
        Panel::ActiveRun => render_active_run(frame, area, snapshot, ui, now),
        Panel::Providers => render_providers(frame, area, snapshot),
        Panel::Evidence => render_evidence(frame, area, snapshot),
        Panel::RecentRuns => render_recent_runs(frame, area, snapshot, ui),
    }
}

fn render_resize_message(frame: &mut Frame, area: Rect) {
    let message = format!(
        "Terminal too small — resize to at least {MIN_WIDTH}x{MIN_HEIGHT} (current {}x{})",
        area.width, area.height
    );
    frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), area);
}

fn liveness_style(liveness: RunLiveness, color: bool) -> Style {
    if !color {
        return Style::default();
    }
    match liveness {
        RunLiveness::Live => Style::default().fg(Color::Green),
        RunLiveness::Silent => Style::default().fg(Color::Yellow),
        RunLiveness::Abandoned => Style::default().fg(Color::Red),
        RunLiveness::Unknown => Style::default().fg(Color::DarkGray),
        RunLiveness::Finished => Style::default().fg(Color::Blue),
    }
}

fn error_style(color: bool) -> Style {
    if color {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    }
}

fn warn_style(color: bool) -> Style {
    if color {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

fn highlight_style(highlighted: bool, color: bool) -> Style {
    let mut style = Style::default();
    if highlighted {
        style = style.add_modifier(Modifier::REVERSED);
        if color {
            style = style.fg(Color::Cyan);
        }
    }
    style
}

const fn job_label(job: RunJob) -> &'static str {
    match job {
        RunJob::Work => "work",
        RunJob::Review => "review",
        RunJob::Consult => "consult",
        RunJob::Plan => "plan",
    }
}

const fn lifecycle_label(lifecycle: RunLifecycle) -> &'static str {
    match lifecycle {
        RunLifecycle::Started => "started",
        RunLifecycle::Running => "running",
        RunLifecycle::Finished => "finished",
    }
}

fn render_banner(frame: &mut Frame, area: Rect, snapshot: &DashboardSnapshot, ui: &UiState) {
    let spans = match &snapshot.run {
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => {
            banner_spans_for_run(value, ui)
        }
        SourceState::Absent { error, .. } => {
            let mut spans = vec![Span::raw("no run selected")];
            if let Some(error) = error {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    display_text(error, 100),
                    error_style(ui.color),
                ));
            }
            spans
        }
        SourceState::Deferred { .. } => vec![Span::raw("run source deferred")],
    };
    frame.render_widget(Line::from(spans), area);
}

fn banner_spans_for_run<'a>(value: &'a RunSnapshot, ui: &UiState) -> Vec<Span<'a>> {
    let identity = &value.identity;
    // Liveness first: ratatui's `Line` clips overflow from the *end*, so the
    // primary badge must never sit behind wide, low-priority fields (a full
    // run id, job, lifecycle) on a narrow terminal.
    let mut spans = vec![
        Span::styled(
            format!("liveness: {}", identity.liveness.label()),
            liveness_style(identity.liveness, ui.color),
        ),
        Span::raw("  "),
        Span::raw(display_text(&identity.run_id, 55)),
    ];
    if let Some(job) = identity.job {
        spans.push(Span::raw(format!("  job: {}", job_label(job))));
    }
    if let Some(lifecycle) = identity.lifecycle {
        spans.push(Span::raw(format!(
            "  lifecycle: {}",
            lifecycle_label(lifecycle)
        )));
    }
    spans
}

fn render_tabs(frame: &mut Frame, area: Rect, ui: &UiState) {
    let mut spans = Vec::with_capacity(Panel::ALL.len() * 2);
    for (index, panel) in Panel::ALL.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let focused = *panel == ui.focus;
        let mut style = Style::default();
        if focused {
            style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        spans.push(Span::styled(format!("[{}]", panel.label()), style));
    }
    frame.render_widget(Line::from(spans), area);
}

fn render_footer(frame: &mut Frame, area: Rect, snapshot: &DashboardSnapshot) {
    // The warning is rare but must never be clipped by the (always-present)
    // keys hint, so it comes first; the hint may clip on a narrow terminal
    // since the full key list is always available via `?`.
    let mut first = String::new();
    if let Some(warning) = &snapshot.discovery_warning {
        first.push_str("warning: ");
        first.push_str(&display_text(warning, 120));
        first.push_str("   ");
    }
    first.push_str("j/k Tab Enter l r ? q \u{2014} press ? for help");
    let second = format!(
        "{}   {}   {}",
        source_freshness_label("run", &snapshot.run),
        source_freshness_label("recent", &snapshot.recent),
        source_freshness_label("musterroll", snapshot.musterroll.as_ref()),
    );
    frame.render_widget(
        Paragraph::new(vec![Line::from(first), Line::from(second)]),
        area,
    );
}

fn source_freshness_label<T>(name: &str, state: &SourceState<T>) -> String {
    match state {
        SourceState::Fresh { truncated, .. } => {
            format!(
                "{name}: fresh{}",
                if *truncated { " (truncated)" } else { "" }
            )
        }
        SourceState::Stale { error, .. } => format!("{name}: stale ({})", display_text(error, 60)),
        SourceState::Absent { error, .. } => format!(
            "{name}: absent{}",
            error
                .as_deref()
                .map(|error| format!(" ({})", display_text(error, 60)))
                .unwrap_or_default()
        ),
        SourceState::Deferred { .. } => format!("{name}: deferred"),
    }
}

fn render_active_run(
    frame: &mut Frame,
    area: Rect,
    snapshot: &DashboardSnapshot,
    ui: &UiState,
    now: DateTime<Utc>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Panel::ActiveRun.label());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let value = match &snapshot.run {
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => value,
        SourceState::Absent { .. } | SourceState::Deferred { .. } => {
            frame.render_widget(Paragraph::new("No run data available."), inner);
            return;
        }
    };

    let mut lines: Vec<Line> = Vec::new();
    if let Some(error) = &value.selection_error {
        lines.push(Line::from(Span::styled(
            display_text(&format!("manifest error: {error}"), 200),
            error_style(ui.color),
        )));
    }
    lines.extend(active_run_header_lines(value, now));
    lines.push(Line::from(String::new()));
    lines.push(Line::from(format_verification(&value.verification)));
    lines.push(Line::from(String::new()));
    lines.extend(active_run_attempts_or_stages_lines(value, ui));
    lines.push(Line::from(String::new()));
    lines.push(Line::from(harness_deck_note(value.identity.job)));
    lines.extend(active_run_diagnostics_lines(value, ui));
    lines.extend(active_run_log_lines(value));
    lines.push(Line::from(String::new()));
    lines.push(Line::from(evidence_summary_line(snapshot)));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Target, Bead, elapsed time, last update, and current stage.
fn active_run_header_lines(value: &RunSnapshot, now: DateTime<Utc>) -> Vec<Line<'static>> {
    let elapsed = value.identity.created_at.map_or_else(
        || "n/a".to_string(),
        |created| format_duration_chrono(now.signed_duration_since(created)),
    );
    let mut lines = vec![
        Line::from(format!(
            "target: {}{}",
            display_text(&value.identity.target_repo, 80),
            value
                .identity
                .target_bead
                .as_deref()
                .map(|bead| format!("   bead: {}", display_text(bead, 40)))
                .unwrap_or_default(),
        )),
        Line::from(format!(
            "elapsed: {elapsed}   updated: {}",
            display_text(&value.identity.updated_at_text, 40),
        )),
    ];
    if let Some(stage) = &value.identity.stage {
        lines.push(Line::from(format!("stage: {}", display_text(stage, 80))));
    }
    lines
}

/// Plan stage markers for a Plan job, or reconstructed attempts otherwise —
/// each job shows exactly one of the two, per the spec's per-job empty
/// states (an explicit "no attempts" message, never blank space).
fn active_run_attempts_or_stages_lines(value: &RunSnapshot, ui: &UiState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if value.identity.job == Some(RunJob::Plan) {
        lines.push(Line::from("Plan stages:"));
        if value.stage_markers.is_empty() {
            lines.push(Line::from("No plan stages recorded yet."));
        } else {
            for marker in &value.stage_markers {
                lines.push(Line::from(format_stage_marker(marker)));
            }
        }
        return lines;
    }
    lines.push(Line::from("Attempts:"));
    if value.attempts.is_empty() {
        let job_word = value.identity.job.map_or("this", job_label);
        lines.push(Line::from(format!("No attempts for this {job_word} run.")));
        return lines;
    }
    let last_index = value.attempts.len() - 1;
    for (index, attempt) in value.attempts.iter().enumerate() {
        let highlighted =
            ui.focus == Panel::ActiveRun && index == ui.attempt_selected.min(last_index);
        lines.push(Line::from(Span::styled(
            format_attempt(attempt),
            highlight_style(highlighted, ui.color),
        )));
    }
    lines
}

/// Visible truncation/error markers: the event tail stalling, or the
/// run-local roster being unparseable. Each is a display-only fact
/// alongside — never instead of — whatever data is still available.
fn active_run_diagnostics_lines(value: &RunSnapshot, ui: &UiState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if value.events_truncated {
        lines.push(Line::from(Span::styled(
            "event tail truncated at cap",
            warn_style(ui.color),
        )));
    }
    if let Some(error) = &value.events_error {
        lines.push(Line::from(Span::styled(
            display_text(&format!("event tail error: {error}"), 200),
            error_style(ui.color),
        )));
    }
    if let Some(error) = &value.roster_error {
        lines.push(Line::from(Span::styled(
            display_text(&format!("roster error: {error}"), 200),
            error_style(ui.color),
        )));
    }
    lines
}

/// The on-demand log tail the runtime routed into `value.logs` (see
/// `runtime::LoopState::read_log`); empty until `l`/Enter opens one.
fn active_run_log_lines(value: &RunSnapshot) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let Some(tail) = value.logs.first() else {
        return lines;
    };
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        format!(
            "log: {}{}",
            display_text(&tail.path, 80),
            if tail.truncated { " (truncated)" } else { "" }
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for line in display_block(&tail.text, 4000).lines() {
        lines.push(Line::from(display_text(line, 200)));
    }
    lines
}

fn format_duration_chrono(duration: chrono::Duration) -> String {
    format_seconds(duration.num_seconds().max(0).unsigned_abs())
}

fn format_std_duration(duration: std::time::Duration) -> String {
    format_seconds(duration.as_secs())
}

fn format_seconds(total: u64) -> String {
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_attempt(attempt: &AttemptRecord) -> String {
    let identity = if attempt.resolved {
        format!(
            "{}/{}/{}",
            attempt.provider_id.as_deref().unwrap_or("?"),
            attempt.harness.as_deref().unwrap_or("?"),
            attempt.model.as_deref().unwrap_or("?"),
        )
    } else {
        format!(
            "{} (unresolved)",
            display_text(
                attempt.profile_id.as_deref().unwrap_or("unknown profile"),
                60
            )
        )
    };
    let duration = match (attempt.duration, attempt.finished_at) {
        (Some(duration), _) => format_std_duration(duration),
        (None, None) if attempt.started_at.is_some() => "no finish event".to_string(),
        _ => "n/a".to_string(),
    };
    let outcome = attempt
        .outcome
        .as_deref()
        .map_or_else(|| "pending".to_string(), |value| display_text(value, 40));
    format!("#{:02} {identity}  {duration}  {outcome}", attempt.ordinal)
}

fn format_stage_marker(marker: &StageMarker) -> String {
    let identity = if marker.resolved {
        format!(
            "{}/{}/{}",
            marker.provider_id.as_deref().unwrap_or("?"),
            marker.harness.as_deref().unwrap_or("?"),
            marker.model.as_deref().unwrap_or("?"),
        )
    } else {
        format!(
            "{} (unresolved)",
            display_text(
                marker.profile_id.as_deref().unwrap_or("unknown profile"),
                60
            )
        )
    };
    let role = marker
        .role
        .as_deref()
        .map(|role| format!(" ({})", display_text(role, 30)))
        .unwrap_or_default();
    let duration = match (marker.duration, marker.finished_at) {
        (Some(duration), _) => format_std_duration(duration),
        (None, None) if marker.started_at.is_some() => "no finish event".to_string(),
        _ => "n/a".to_string(),
    };
    let outcome = marker
        .outcome
        .as_deref()
        .map_or_else(|| "pending".to_string(), |value| display_text(value, 40));
    format!(
        "#{:02} {}{role}  {identity}  {duration}  {outcome}",
        marker.ordinal,
        display_text(&marker.stage, 40)
    )
}

/// Shared with the overview column's shorter form, so the two can never
/// disagree about what `passed`/`failed`/`not run` means.
const fn verification_result_label(passed: Option<bool>) -> &'static str {
    match passed {
        Some(true) => "passed",
        Some(false) => "failed",
        None => "not run",
    }
}

const fn verification_source_label(source: VerificationSource) -> &'static str {
    match source {
        VerificationSource::Mechanical => "mechanical",
        VerificationSource::Event => "event",
        VerificationSource::NotRun => "n/a",
    }
}

fn format_verification(record: &VerificationRecord) -> String {
    let mut text = format!(
        "verification: {} (source: {})",
        verification_result_label(record.passed),
        verification_source_label(record.source),
    );
    if let Some(command) = &record.command {
        let _ = write!(text, "  cmd: {}", display_text(command, 60));
    }
    if record.disagreement {
        text.push_str("  [disagreement: mechanical/event differ]");
    }
    text
}

/// The Harness Deck join Task 1/2 never implemented (no report path exists
/// on `RunSnapshot`/`RunIdentity`): for the two jobs the spec defines as
/// having *no* join regardless of data availability, this is a static,
/// job-only fact requiring no report data at all. For jobs the spec would
/// join (Work/Plan), disclose the gap honestly rather than fabricate a link.
fn harness_deck_note(job: Option<RunJob>) -> String {
    match job {
        Some(RunJob::Consult | RunJob::Review) => "no Harness Deck report for this job".to_string(),
        _ => "Harness Deck: not available in this view".to_string(),
    }
}

fn afterfact_summary(snapshot: &DashboardSnapshot) -> String {
    match snapshot.afterfact.as_ref() {
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => format!(
            "afterfact: {} correlated / {} uncorrelated",
            value.correlated_count, value.uncorrelated_count
        ),
        SourceState::Absent { .. } => "afterfact: not yet fetched".to_string(),
        SourceState::Deferred { .. } => "afterfact: deferred".to_string(),
    }
}

fn cautionlight_summary(snapshot: &DashboardSnapshot) -> String {
    match snapshot.cautionlight.as_ref() {
        SourceState::Deferred { .. } => "cautionlight: deferred".to_string(),
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => {
            format!("cautionlight: {} findings", value.findings.len())
        }
        SourceState::Absent { .. } => "cautionlight: not yet fetched".to_string(),
    }
}

fn evidence_summary_line(snapshot: &DashboardSnapshot) -> String {
    format!(
        "{}   {}",
        afterfact_summary(snapshot),
        cautionlight_summary(snapshot)
    )
}

fn render_providers(frame: &mut Frame, area: Rect, snapshot: &DashboardSnapshot) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Panel::Providers.label());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = match snapshot.musterroll.as_ref() {
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => {
            if value.providers.is_empty() {
                vec![ListItem::new("No providers reported.")]
            } else {
                value
                    .providers
                    .iter()
                    .map(|(name, status)| ListItem::new(provider_lines(name, status)))
                    .collect()
            }
        }
        SourceState::Absent { error, .. } => vec![ListItem::new(display_text(
            error.as_deref().unwrap_or("Musterroll unavailable."),
            200,
        ))],
        SourceState::Deferred { .. } => vec![ListItem::new("Musterroll deferred.")],
    };
    frame.render_widget(List::new(items), inner);
}

/// One provider's rows: the headline, then only those detail rows the
/// status actually carries. Every field the Musterroll adapter preserves —
/// availability, source, checked/data-as-of/expiry timestamps, windows,
/// exclusion reason, and the allowlisted `extra` subset — reaches the
/// screen; parsing a field and never showing it makes it dead data.
fn provider_lines(name: &str, status: &ProviderStatusSnapshot) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!(
        "{}  {}  source: {}  checked: {}",
        display_text(name, 40),
        status.availability,
        display_text(&status.source, 30),
        display_text(&status.checked_at, 32),
    ))];

    if !status.windows.is_empty() {
        let windows = status
            .windows
            .iter()
            .map(format_window)
            .collect::<Vec<_>>()
            .join("   ");
        lines.push(Line::from(format!("  windows: {windows}")));
    }

    let mut freshness = String::new();
    if let Some(data_as_of) = &status.data_as_of {
        let _ = write!(freshness, "  data as of: {}", display_text(data_as_of, 32));
    }
    if let Some(expires_at) = &status.expires_at {
        let _ = write!(freshness, "  expires: {}", display_text(expires_at, 32));
    }
    if !freshness.is_empty() {
        lines.push(Line::from(freshness));
    }

    if let Some(reason) = &status.reason {
        lines.push(Line::from(format!(
            "  reason: {}",
            display_text(reason, 80)
        )));
    }

    if !status.extra.is_empty() {
        let extra = status
            .extra
            .iter()
            .map(|(key, value)| format!("{}={}", display_text(key, 40), display_text(value, 40)))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(format!("  {extra}")));
    }

    lines
}

/// A usage window. A `null` percentage renders `?%`, never `0%`: an
/// unreported budget is not an exhausted one, and the two must not look
/// alike.
fn format_window(window: &Window) -> String {
    let percent = window
        .percent
        .map_or_else(|| "?".to_string(), |percent| format!("{percent:.1}"));
    let reset = window
        .reset_at
        .as_deref()
        .map(|reset| format!(" (resets {})", display_text(reset, 32)))
        .unwrap_or_default();
    format!("{} {percent}%{reset}", display_text(&window.label, 24))
}

fn render_evidence(frame: &mut Frame, area: Rect, snapshot: &DashboardSnapshot) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Panel::Evidence.label());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = vec![Line::from("Afterfact")];
    match snapshot.afterfact.as_ref() {
        SourceState::Fresh {
            value, truncated, ..
        }
        | SourceState::Stale {
            value, truncated, ..
        } => {
            lines.push(Line::from(format!(
                "correlated: {}   uncorrelated: {}{}",
                value.correlated_count,
                value.uncorrelated_count,
                if *truncated { "   (truncated)" } else { "" },
            )));
            if let Some(summary) = &value.coverage_gap_summary {
                lines.push(Line::from(display_text(
                    &format!("coverage gap: {summary}"),
                    200,
                )));
            }
        }
        SourceState::Absent { error, .. } => lines.push(Line::from(match error {
            Some(error) => display_text(error, 200),
            None => "not yet fetched — press r while this panel is focused".to_string(),
        })),
        SourceState::Deferred { .. } => lines.push(Line::from("deferred")),
    }

    lines.push(Line::from(String::new()));
    lines.push(Line::from("Cautionlight"));
    match snapshot.cautionlight.as_ref() {
        SourceState::Deferred { .. } => {
            lines.push(Line::from(
                "deferred — roadmap feature, not run in this version",
            ));
        }
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => {
            lines.push(Line::from(format!("{} findings", value.findings.len())));
            if let Some(warnings) = &value.coverage_warnings {
                lines.push(Line::from(display_text(
                    &format!("coverage warning: {warnings}"),
                    200,
                )));
            }
        }
        SourceState::Absent { error, .. } => lines.push(Line::from(match error {
            Some(error) => display_text(error, 200),
            None => "not yet fetched".to_string(),
        })),
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_recent_runs(frame: &mut Frame, area: Rect, snapshot: &DashboardSnapshot, ui: &UiState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Panel::RecentRuns.label());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = match &snapshot.recent {
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => {
            recent_run_items(value, ui)
        }
        SourceState::Absent { error, .. } => vec![ListItem::new(display_text(
            error.as_deref().unwrap_or("recent runs unavailable"),
            200,
        ))],
        SourceState::Deferred { .. } => vec![ListItem::new("recent runs deferred")],
    };
    frame.render_widget(List::new(items), inner);
}

fn recent_run_items<'a>(runs: &'a [RecentRun], ui: &UiState) -> Vec<ListItem<'a>> {
    if runs.is_empty() {
        return vec![ListItem::new("No recent terminal runs.")];
    }
    let last_index = runs.len() - 1;
    runs.iter()
        .enumerate()
        .map(|(index, run)| {
            let highlighted =
                ui.focus == Panel::RecentRuns && index == ui.recent_selected.min(last_index);
            let text = format!(
                "{}  {}  {}  {}",
                display_text(&run.run_id, 60),
                job_label(run.job),
                run.outcome
                    .as_deref()
                    .map_or_else(|| "n/a".to_string(), |outcome| display_text(outcome, 30)),
                display_text(&run.target_repo, 40),
            );
            ListItem::new(text).style(highlight_style(highlighted, ui.color))
        })
        .collect()
}

/// The three panels the overview column summarizes: every panel except the
/// focused one, in [`Panel::ALL`] order, so the column's structure is a
/// function of focus alone.
fn overview_panels(focus: Panel) -> [Panel; 3] {
    let mut panels = [Panel::ActiveRun; 3];
    let mut next = 0;
    for panel in Panel::ALL {
        if panel != focus {
            panels[next] = panel;
            next += 1;
        }
    }
    panels
}

/// The normal layout's overview column: the three unfocused panels, stacked
/// and summarized, so a provider going `exhausted` or a run finishing stays
/// visible without leaving the panel in focus. Deliberately unwrapped —
/// each summary row is one item, and wrapping one would silently push the
/// items below it off a fixed-height pane.
fn render_overview_column(
    frame: &mut Frame,
    area: Rect,
    snapshot: &DashboardSnapshot,
    ui: &UiState,
    now: DateTime<Utc>,
) {
    let panes: [Rect; 3] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    for (panel, pane) in overview_panels(ui.focus).into_iter().zip(panes) {
        let block = Block::default().borders(Borders::ALL).title(panel.label());
        let inner = block.inner(pane);
        frame.render_widget(block, pane);
        frame.render_widget(
            Paragraph::new(overview_lines(panel, snapshot, ui, now)),
            inner,
        );
    }
}

/// One unfocused panel's summary. Source-state distinctions survive it: an
/// overview that collapsed `stale`, `absent`, and `deferred` into one blank
/// pane would be worse than no overview at all.
fn overview_lines(
    panel: Panel,
    snapshot: &DashboardSnapshot,
    ui: &UiState,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    match panel {
        Panel::ActiveRun => active_run_overview_lines(snapshot, ui, now),
        Panel::Providers => providers_overview_lines(snapshot),
        Panel::Evidence => evidence_overview_lines(snapshot),
        Panel::RecentRuns => recent_runs_overview_lines(snapshot),
    }
}

fn active_run_overview_lines(
    snapshot: &DashboardSnapshot,
    ui: &UiState,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let value = match &snapshot.run {
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => value,
        SourceState::Absent { error, .. } => {
            return vec![Line::from(display_text(
                error.as_deref().unwrap_or("no run data"),
                120,
            ))];
        }
        SourceState::Deferred { .. } => return vec![Line::from("run source deferred")],
    };
    let identity = &value.identity;
    let mut lines = vec![Line::from(Span::styled(
        format!("liveness: {}", identity.liveness.label()),
        liveness_style(identity.liveness, ui.color),
    ))];
    if let Some(stage) = &identity.stage {
        lines.push(Line::from(format!("stage: {}", display_text(stage, 30))));
    }
    lines.push(Line::from(format!(
        "elapsed: {}",
        identity.created_at.map_or_else(
            || "n/a".to_string(),
            |created| format_duration_chrono(now.signed_duration_since(created)),
        )
    )));
    lines.push(Line::from(format!(
        "verification: {} ({})",
        verification_result_label(value.verification.passed),
        verification_source_label(value.verification.source),
    )));
    lines.push(Line::from(if identity.job == Some(RunJob::Plan) {
        format!("plan stages: {}", value.stage_markers.len())
    } else {
        format!("attempts: {}", value.attempts.len())
    }));
    lines
}

fn providers_overview_lines(snapshot: &DashboardSnapshot) -> Vec<Line<'static>> {
    match snapshot.musterroll.as_ref() {
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => {
            if value.providers.is_empty() {
                return vec![Line::from("no providers reported")];
            }
            value
                .providers
                .iter()
                .map(|(name, status)| {
                    Line::from(format!(
                        "{}  {}",
                        display_text(name, 24),
                        status.availability
                    ))
                })
                .collect()
        }
        SourceState::Absent { error, .. } => vec![Line::from(display_text(
            error.as_deref().unwrap_or("musterroll unavailable"),
            120,
        ))],
        SourceState::Deferred { .. } => vec![Line::from("musterroll deferred")],
    }
}

fn evidence_overview_lines(snapshot: &DashboardSnapshot) -> Vec<Line<'static>> {
    vec![
        Line::from(afterfact_summary(snapshot)),
        Line::from(cautionlight_summary(snapshot)),
    ]
}

fn recent_runs_overview_lines(snapshot: &DashboardSnapshot) -> Vec<Line<'static>> {
    match &snapshot.recent {
        SourceState::Fresh { value, .. } | SourceState::Stale { value, .. } => {
            if value.is_empty() {
                return vec![Line::from("no recent terminal runs")];
            }
            value
                .iter()
                .map(|run| {
                    // Outcome first: the column clips from the right, and a
                    // clipped run id still identifies the run while a
                    // clipped-away outcome tells the reader nothing.
                    Line::from(format!(
                        "{}  {}",
                        run.outcome
                            .as_deref()
                            .map_or_else(|| "n/a".to_string(), |outcome| display_text(outcome, 12)),
                        display_text(&run.run_id, 60),
                    ))
                })
                .collect()
        }
        SourceState::Absent { error, .. } => vec![Line::from(display_text(
            error.as_deref().unwrap_or("recent runs unavailable"),
            120,
        ))],
        SourceState::Deferred { .. } => vec![Line::from("recent runs deferred")],
    }
}

fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let width = area.width.min(72);
    let height = area.height.min(16);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let text = vec![
        Line::from("Undertake dashboard — read-only observation. No approve, dispatch,"),
        Line::from("retry, cancel, resume, or state mutation happens from this screen."),
        Line::from(""),
        Line::from("j/k, ↑/↓        move selection"),
        Line::from("Tab / Shift-Tab  change focused panel"),
        Line::from("Enter            Recent Runs: open run  ·  Active Run: toggle log"),
        Line::from("l                toggle the focused panel's log (Active Run only)"),
        Line::from("r                refresh Evidence (Afterfact) on demand"),
        Line::from("?                toggle this help"),
        Line::from("q, Ctrl-C        quit"),
        Line::from(""),
        Line::from("live/silent/abandoned/unknown/finished are distinct liveness facts;"),
        Line::from("a stale source keeps its last value and shows an error alongside it."),
    ];
    let block = Block::default().borders(Borders::ALL).title("Help");
    frame.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::dashboard::model::{LogTail, RunIdentity};
    use crate::dashboard::services::{
        AfterfactSnapshot, CautionlightDashboardSource, MusterrollSnapshot, ProviderStatusSnapshot,
    };
    use crate::musterroll::{Availability, Window};

    const COMPACT: (u16, u16) = (64, 18);
    const NORMAL: (u16, u16) = (110, 30);
    const WIDE: (u16, u16) = (170, 45);
    const BELOW_MINIMUM: (u16, u16) = (40, 10);

    fn ts(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        ts("2026-07-25T18:41:00Z")
    }

    fn base_identity() -> RunIdentity {
        RunIdentity {
            run_id: "run-work-20260725T183920.469500000-p45813-000000".to_string(),
            job: Some(RunJob::Work),
            lifecycle: Some(RunLifecycle::Running),
            liveness: RunLiveness::Live,
            created_at: Some(ts("2026-07-25T18:39:20Z")),
            created_at_text: "2026-07-25T18:39:20Z".to_string(),
            updated_at_text: "2026-07-25T18:40:00Z".to_string(),
            target_repo: "/Users/tfinklea/git/patchstand".to_string(),
            target_bead: Some("patchstand-thk".to_string()),
            stage: Some("implementing".to_string()),
            schema: "undertake/run@2".to_string(),
            roster_snapshot: None,
            roster_policy_sha256: None,
            musterroll_roster_artifact: None,
        }
    }

    fn base_attempt() -> AttemptRecord {
        AttemptRecord {
            ordinal: 1,
            attempt_dir: Some("001-openai-codex--codex--gpt-5.6-luna--high".to_string()),
            profile_id: Some("openai-codex--codex--gpt-5.6-luna--high".to_string()),
            provider_id: Some("openai-codex".to_string()),
            model: Some("gpt-5.6-luna".to_string()),
            harness: Some("codex".to_string()),
            dispatch_id: Some("gpt-5.6-luna".to_string()),
            resolved: true,
            started_at: Some(ts("2026-07-25T18:39:20Z")),
            finished_at: Some(ts("2026-07-25T18:43:40Z")),
            duration: Some(Duration::from_secs(260)),
            outcome: Some("success".to_string()),
        }
    }

    fn base_verification() -> VerificationRecord {
        VerificationRecord {
            passed: Some(true),
            source: VerificationSource::Mechanical,
            command: Some("pnpm check".to_string()),
            event_outcome: Some("passed".to_string()),
            disagreement: false,
        }
    }

    fn base_run_snapshot() -> RunSnapshot {
        RunSnapshot {
            identity: base_identity(),
            attempts: vec![base_attempt()],
            stage_markers: vec![],
            verification: base_verification(),
            logs: vec![],
            event_count: 5,
            events_truncated: false,
            selection_error: None,
            events_error: None,
            roster_error: None,
        }
    }

    fn base_recent() -> RecentRun {
        RecentRun {
            run_id: "run-work-20260724T100000.000000000-p1-000000".to_string(),
            job: RunJob::Work,
            lifecycle: RunLifecycle::Finished,
            liveness: RunLiveness::Finished,
            target_repo: "/Users/tfinklea/git/patchstand".to_string(),
            target_bead: Some("patchstand-old".to_string()),
            created_at: Some(ts("2026-07-24T10:00:00Z")),
            created_at_text: "2026-07-24T10:00:00Z".to_string(),
            outcome: Some("success".to_string()),
        }
    }

    fn base_provider(availability: Availability) -> ProviderStatusSnapshot {
        ProviderStatusSnapshot {
            availability,
            source: "api".to_string(),
            checked_at: "2026-07-25T18:40:00Z".to_string(),
            data_as_of: None,
            expires_at: None,
            windows: vec![],
            reason: None,
            extra: BTreeMap::new(),
        }
    }

    fn base_musterroll() -> MusterrollSnapshot {
        let mut providers = BTreeMap::new();
        providers.insert(
            "openai-codex".to_string(),
            base_provider(Availability::Healthy),
        );
        MusterrollSnapshot {
            schema: "musterroll/status@1".to_string(),
            checked_at: "2026-07-25T18:40:00Z".to_string(),
            providers,
        }
    }

    fn base_afterfact() -> AfterfactSnapshot {
        AfterfactSnapshot {
            events: vec![],
            correlated_count: 3,
            uncorrelated_count: 1,
            coverage_gap_summary: None,
        }
    }

    /// A fully "healthy" baseline snapshot: live work run, one resolved
    /// attempt, fresh Musterroll/Afterfact, deferred Cautionlight. Individual
    /// tests override exactly the field(s) under test.
    fn base_snapshot() -> DashboardSnapshot {
        let at = now();
        DashboardSnapshot {
            run: SourceState::Fresh {
                value: base_run_snapshot(),
                last_ok: at,
                last_attempt: at,
                truncated: false,
            },
            recent: SourceState::Fresh {
                value: vec![base_recent()],
                last_ok: at,
                last_attempt: at,
                truncated: false,
            },
            discovery_warning: None,
            musterroll: Arc::new(SourceState::Fresh {
                value: base_musterroll(),
                last_ok: at,
                last_attempt: at,
                truncated: false,
            }),
            afterfact: Arc::new(SourceState::Fresh {
                value: base_afterfact(),
                last_ok: at,
                last_attempt: at,
                truncated: false,
            }),
            cautionlight: Arc::new(CautionlightDashboardSource::default_state()),
        }
    }

    fn rendered_lines(
        width: u16,
        height: u16,
        snapshot: &DashboardSnapshot,
        ui: &UiState,
    ) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("construct test terminal");
        terminal
            .draw(|frame| render(frame, snapshot, ui, now()))
            .expect("draw frame");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map_or(" ", ratatui::buffer::Cell::symbol)
                    })
                    .collect::<String>()
            })
            .collect()
    }

    fn contains(lines: &[String], needle: &str) -> bool {
        lines.iter().any(|line| line.contains(needle))
    }

    /// ratatui draws a bordered block's title one cell right of its
    /// top-left corner, so `┌<label>` identifies a real pane and can never
    /// be confused with the tab strip's `[<label>]`.
    const PANE_TITLE_PREFIX: char = '┌';

    fn framed_panel_count(lines: &[String], panel: Panel) -> usize {
        let needle = format!("{PANE_TITLE_PREFIX}{}", panel.label());
        lines.iter().filter(|line| line.contains(&needle)).count()
    }

    /// The bordered panes actually drawn, in [`Panel::ALL`] order. Reads the
    /// frame's *structure*, independent of what any panel contains.
    fn framed_panels(lines: &[String]) -> Vec<&'static str> {
        Panel::ALL
            .iter()
            .copied()
            .filter(|panel| framed_panel_count(lines, *panel) > 0)
            .map(Panel::label)
            .collect()
    }

    /// `base_snapshot` with panel-distinct content: a provider name that
    /// appears nowhere else on screen, so a test can tell "the Providers
    /// panel rendered" apart from "the attempt's own provider id rendered".
    fn overview_snapshot() -> DashboardSnapshot {
        let at = now();
        let mut status = base_provider(Availability::Exhausted);
        status.reason = Some("weekly budget nearly spent".to_string());
        let mut providers = BTreeMap::new();
        providers.insert("neuralwatt".to_string(), status);
        let mut snapshot = base_snapshot();
        snapshot.musterroll = Arc::new(SourceState::Fresh {
            value: MusterrollSnapshot {
                schema: "musterroll/status@1".to_string(),
                checked_at: "2026-07-25T18:40:00Z".to_string(),
                providers,
            },
            last_ok: at,
            last_attempt: at,
            truncated: false,
        });
        snapshot
    }

    /// The plain text of one panel's overview form, spans joined.
    fn overview_text(panel: Panel, snapshot: &DashboardSnapshot) -> Vec<String> {
        overview_lines(panel, snapshot, &UiState::default(), now())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn has_row(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|row| row == needle)
    }

    #[test]
    fn compact_layout_shows_active_run_identity() {
        let snapshot = base_snapshot();
        let lines = rendered_lines(COMPACT.0, COMPACT.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "run-work-20260725T183920"));
        assert!(contains(&lines, "live"));
    }

    #[test]
    fn normal_layout_shows_active_run_identity() {
        let snapshot = base_snapshot();
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "run-work-20260725T183920"));
        assert!(contains(&lines, "patchstand-thk"));
        assert!(contains(&lines, "implementing"));
    }

    #[test]
    fn wide_layout_shows_all_panel_tabs() {
        let snapshot = base_snapshot();
        let lines = rendered_lines(WIDE.0, WIDE.1, &snapshot, &UiState::default());
        for label in [
            Panel::ActiveRun.label(),
            Panel::Providers.label(),
            Panel::Evidence.label(),
            Panel::RecentRuns.label(),
        ] {
            assert!(contains(&lines, label), "missing tab label {label:?}");
        }
    }

    /// Spec §205 requires *both* a compact and a normal layout. Compact
    /// cannot afford a sidebar and stays a single focused panel; normal must
    /// put the other three panels on screen at the same time. If the two
    /// ever collapse back into one structure, this fails.
    #[test]
    fn compact_and_normal_layouts_are_structurally_distinct() {
        let snapshot = base_snapshot();
        let ui = UiState::default();
        assert_eq!(
            framed_panels(&rendered_lines(COMPACT.0, COMPACT.1, &snapshot, &ui)),
            vec![Panel::ActiveRun.label()],
            "compact draws the focused panel and nothing else"
        );
        assert_eq!(
            framed_panels(&rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui)),
            vec![
                Panel::ActiveRun.label(),
                Panel::Providers.label(),
                Panel::Evidence.label(),
                Panel::RecentRuns.label(),
            ],
            "normal adds an overview pane for every unfocused panel"
        );
    }

    /// One deterministic threshold, and it is exactly the width at which
    /// both panes become usable: one column narrower must fall back to
    /// compact rather than render a useless sliver.
    #[test]
    fn the_layout_switches_at_one_deterministic_width() {
        let snapshot = base_snapshot();
        let ui = UiState::default();
        assert_eq!(
            LayoutMode::for_width(NORMAL_MIN_WIDTH - 1),
            LayoutMode::Compact
        );
        assert_eq!(LayoutMode::for_width(NORMAL_MIN_WIDTH), LayoutMode::Normal);
        let below = rendered_lines(NORMAL_MIN_WIDTH - 1, NORMAL.1, &snapshot, &ui);
        let at_threshold = rendered_lines(NORMAL_MIN_WIDTH, NORMAL.1, &snapshot, &ui);
        assert_eq!(
            framed_panels(&below).len(),
            1,
            "one column below the threshold is still compact"
        );
        assert_eq!(
            framed_panels(&at_threshold).len(),
            4,
            "the threshold itself already fits the overview column"
        );
    }

    /// Compact is single-panel in content as well as chrome: nothing from an
    /// unfocused panel leaks in.
    #[test]
    fn compact_layout_shows_no_unfocused_panel_content() {
        let snapshot = overview_snapshot();
        let lines = rendered_lines(COMPACT.0, COMPACT.1, &snapshot, &UiState::default());
        assert!(
            contains(&lines, "implementing"),
            "the focused panel keeps its full detail"
        );
        assert!(
            !contains(&lines, "neuralwatt"),
            "a provider must not appear while Providers is unfocused"
        );
        assert!(
            !contains(&lines, "run-work-20260724T100000"),
            "a recent run must not appear while Recent Runs is unfocused"
        );
        assert!(
            !contains(&lines, "attempts: 1"),
            "the summary form of Active Run belongs to the overview column only"
        );
    }

    /// Normal is a genuine multi-pane overview: the focused panel keeps its
    /// full detail and the other three are summarized beside it.
    #[test]
    fn normal_layout_summarizes_unfocused_panels_beside_the_focused_detail() {
        let snapshot = overview_snapshot();
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(
            contains(&lines, "#01 openai-codex/codex/gpt-5.6-luna"),
            "the focused detail pane is unchanged by the split"
        );
        assert!(
            contains(&lines, "neuralwatt  exhausted"),
            "the Providers summary carries its availability"
        );
        assert!(
            contains(&lines, "run-work-20260724T100000"),
            "the Recent Runs summary carries its run"
        );
    }

    /// Every overview pane must carry its own panel's state: a column of
    /// empty boxes is not an overview.
    #[test]
    fn every_overview_pane_summarizes_its_own_panels_state() {
        let snapshot = overview_snapshot();
        assert!(has_row(
            &overview_text(Panel::ActiveRun, &snapshot),
            "attempts: 1"
        ));
        assert!(has_row(
            &overview_text(Panel::Providers, &snapshot),
            "neuralwatt  exhausted"
        ));
        assert!(has_row(
            &overview_text(Panel::Evidence, &snapshot),
            "cautionlight: deferred"
        ));
        assert!(
            overview_text(Panel::RecentRuns, &snapshot)
                .iter()
                .any(|row| row.contains("run-work-20260724T100000"))
        );
    }

    /// A summarized panel must not launder its source state: `absent` and
    /// `deferred` stay as distinguishable in the overview column as they are
    /// in the full panel.
    #[test]
    fn overview_summaries_preserve_source_state_distinctions() {
        let mut snapshot = overview_snapshot();
        snapshot.musterroll = Arc::new(SourceState::Absent {
            last_attempt: Some(now()),
            error: Some("musterroll status exited 1".to_string()),
        });
        assert!(has_row(
            &overview_text(Panel::Providers, &snapshot),
            "musterroll status exited 1"
        ));
        assert!(
            has_row(
                &overview_text(Panel::Evidence, &snapshot),
                "cautionlight: deferred"
            ),
            "a deferred service must not read like a missing one"
        );
        snapshot.afterfact = Arc::new(SourceState::never_read());
        assert!(has_row(
            &overview_text(Panel::Evidence, &snapshot),
            "afterfact: not yet fetched"
        ));
    }

    /// Focus still decides which panel gets the detail pane, and the panel
    /// that has it is never repeated in the overview column.
    #[test]
    fn focus_moves_the_detail_pane_without_duplicating_it() {
        let snapshot = overview_snapshot();
        let ui = UiState {
            focus: Panel::Providers,
            ..UiState::default()
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui);
        assert!(
            contains(&lines, "weekly budget nearly spent"),
            "the focused panel shows detail its summary omits"
        );
        assert!(
            contains(&lines, "attempts: 1"),
            "the now-unfocused Active Run is summarized in the overview column"
        );
        assert!(
            !contains(&lines, "Harness Deck"),
            "a summary is a summary, not the whole panel"
        );
        assert_eq!(
            framed_panel_count(&lines, Panel::Providers),
            1,
            "the focused panel must not also appear in the overview column"
        );
    }

    #[test]
    fn below_minimum_size_renders_resize_message() {
        let snapshot = base_snapshot();
        let lines = rendered_lines(
            BELOW_MINIMUM.0,
            BELOW_MINIMUM.1,
            &snapshot,
            &UiState::default(),
        );
        let joined = lines.join(" ").to_lowercase();
        assert!(
            joined.contains("resize") || joined.contains("too small"),
            "expected a resize message, got: {joined:?}"
        );
        // A too-small frame must never attempt real content: layout math
        // against panel chrome could misbehave below the minimum size.
        assert!(!contains(&lines, "run-work-20260725T183920"));
    }

    #[test]
    fn live_liveness_badge_is_visible() {
        let snapshot = base_snapshot();
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "liveness: live"));
    }

    /// The Patchstand pilot regression: a nonterminal (`Running`) manifest
    /// whose heartbeat evidence is stale/dead must show `abandoned` as the
    /// *liveness* badge. Lifecycle stays a separate, still-visible fact
    /// (`lifecycle: running`) — the two must never collapse into one label.
    #[test]
    fn abandoned_liveness_badge_is_visible_lifecycle_stays_separate() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.identity.lifecycle = Some(RunLifecycle::Running);
            value.identity.liveness = RunLiveness::Abandoned;
        }
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "liveness: abandoned"));
        assert!(contains(&lines, "lifecycle: running"));
    }

    #[test]
    fn finished_lifecycle_shows_finished_badge() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.identity.lifecycle = Some(RunLifecycle::Finished);
            value.identity.liveness = RunLiveness::Finished;
        }
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "liveness: finished"));
    }

    #[test]
    fn stale_run_source_shows_retained_value_and_error() {
        let at = now();
        let mut snapshot = base_snapshot();
        snapshot.run = SourceState::Stale {
            value: base_run_snapshot(),
            last_ok: at,
            last_attempt: at,
            error: "manifest read timed out".to_string(),
            truncated: false,
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        // The retained identity is still shown...
        assert!(contains(&lines, "run-work-20260725T183920"));
        // ...alongside a visibly distinct stale marker and the real error.
        assert!(contains(&lines, "stale"));
        assert!(contains(&lines, "manifest read timed out"));
    }

    /// A source that has *never* produced a value must never render a
    /// fabricated identity — `Absent` looks nothing like `Fresh`.
    #[test]
    fn absent_run_source_shows_no_fabricated_identity() {
        let mut snapshot = base_snapshot();
        snapshot.run = SourceState::Absent {
            last_attempt: Some(now()),
            error: Some("runs directory unreadable".to_string()),
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(!contains(&lines, "run-work-20260725T183920"));
        assert!(contains(&lines, "runs directory unreadable"));
    }

    /// A malformed *selected* manifest is a different case from a dead
    /// source: the read succeeded, the parse did not. `RunIdentity::unknown`
    /// must render — job/lifecycle blank, liveness `unknown`, the run id
    /// from the directory name, and the manifest error visible — never a
    /// fabricated `work`/`started` guess.
    #[test]
    fn malformed_selected_manifest_shows_unknown_identity_and_error() {
        let at = now();
        let mut snapshot = base_snapshot();
        let mut broken = base_run_snapshot();
        broken.identity = RunIdentity::unknown("run-work-broken-000000");
        broken.selection_error = Some("invalid type: map, expected a sequence".to_string());
        broken.attempts = vec![];
        snapshot.run = SourceState::Fresh {
            value: broken,
            last_ok: at,
            last_attempt: at,
            truncated: false,
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "run-work-broken-000000"));
        assert!(contains(&lines, "liveness: unknown"));
        assert!(!contains(&lines, "job: work"));
        assert!(!contains(&lines, "lifecycle: started"));
        assert!(contains(&lines, "invalid type: map, expected a sequence"));
    }

    #[test]
    fn truncated_event_tail_is_indicated() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.events_truncated = true;
        }
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(
            contains(&lines, "truncated"),
            "expected a visible truncation indicator"
        );
    }

    #[test]
    fn deferred_cautionlight_panel_shows_deferred() {
        let snapshot = base_snapshot();
        let ui = UiState {
            focus: Panel::Evidence,
            ..UiState::default()
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui);
        assert!(contains(&lines, "deferred"));
    }

    #[test]
    fn partial_afterfact_coverage_gap_is_shown() {
        let at = now();
        let mut snapshot = base_snapshot();
        snapshot.afterfact = Arc::new(SourceState::Fresh {
            value: AfterfactSnapshot {
                events: vec![],
                correlated_count: 2,
                uncorrelated_count: 0,
                coverage_gap_summary: Some("window truncated at 4 MiB".to_string()),
            },
            last_ok: at,
            last_attempt: at,
            truncated: true,
        });
        let ui = UiState {
            focus: Panel::Evidence,
            ..UiState::default()
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui);
        assert!(contains(&lines, "window truncated at 4 MiB"));
        assert!(contains(&lines, "truncated"));
    }

    #[test]
    fn unresolved_profile_shows_marker_not_fabricated_identity() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.attempts = vec![AttemptRecord {
                ordinal: 1,
                attempt_dir: Some("001-opaque-profile".to_string()),
                profile_id: Some("opaque-profile-id".to_string()),
                provider_id: None,
                model: None,
                harness: None,
                dispatch_id: None,
                resolved: false,
                started_at: Some(ts("2026-07-25T18:39:20Z")),
                finished_at: None,
                duration: None,
                outcome: None,
            }];
        }
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "opaque-profile-id"));
        assert!(
            contains(&lines, "unresolved"),
            "an unresolved profile must show an explicit marker"
        );
    }

    #[test]
    fn no_attempts_shows_explicit_empty_state() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.identity.job = Some(RunJob::Consult);
            value.attempts = vec![];
        }
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(
            contains(&lines, "No attempts"),
            "an empty attempts list must show explicit text, not blank space"
        );
    }

    #[test]
    fn plan_job_shows_stage_markers_not_attempts() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.identity.job = Some(RunJob::Plan);
            value.attempts = vec![];
            value.stage_markers = vec![StageMarker {
                stage: "planner".to_string(),
                role: Some("author".to_string()),
                ordinal: 1,
                profile_id: Some("anthropic--claude--opus-5--max".to_string()),
                provider_id: Some("anthropic".to_string()),
                model: Some("opus-5".to_string()),
                harness: Some("claude".to_string()),
                dispatch_id: Some("opus-5".to_string()),
                resolved: true,
                started_at: Some(ts("2026-07-25T18:39:20Z")),
                finished_at: Some(ts("2026-07-25T18:40:00Z")),
                duration: Some(Duration::from_secs(40)),
                outcome: Some("success".to_string()),
            }];
        }
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "planner"));
    }

    #[test]
    fn consult_job_shows_no_harness_deck_report() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.identity.job = Some(RunJob::Consult);
        }
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "no Harness Deck report"));
    }

    /// Disabling color must not remove any information: every state
    /// distinction is carried in text/symbols, and color only supplements
    /// it. This is the direct proof of that invariant.
    #[test]
    fn color_disabled_still_shows_state_text() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.identity.liveness = RunLiveness::Abandoned;
        }
        let ui = UiState {
            color: false,
            ..UiState::default()
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui);
        assert!(contains(&lines, "liveness: abandoned"));
        assert!(contains(&lines, "run-work-20260725T183920"));
    }

    #[test]
    fn discovery_warning_is_visible_when_present() {
        let mut snapshot = base_snapshot();
        snapshot.discovery_warning =
            Some("3 run directories were unreadable and were skipped".to_string());
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "3 run directories were unreadable"));
    }

    #[test]
    fn recent_runs_panel_lists_recent_runs() {
        let snapshot = base_snapshot();
        let ui = UiState {
            focus: Panel::RecentRuns,
            ..UiState::default()
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui);
        assert!(contains(&lines, "run-work-20260724T100000"));
    }

    #[test]
    fn providers_panel_shows_availability() {
        let snapshot = base_snapshot();
        let ui = UiState {
            focus: Panel::Providers,
            ..UiState::default()
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui);
        assert!(contains(&lines, "openai-codex"));
        assert!(contains(&lines, "healthy"));
    }

    #[test]
    fn exhausted_provider_availability_is_visible() {
        let at = now();
        let mut snapshot = base_snapshot();
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            base_provider(Availability::Exhausted),
        );
        snapshot.musterroll = Arc::new(SourceState::Fresh {
            value: MusterrollSnapshot {
                schema: "musterroll/status@1".to_string(),
                checked_at: "2026-07-25T18:40:00Z".to_string(),
                providers,
            },
            last_ok: at,
            last_attempt: at,
            truncated: false,
        });
        let ui = UiState {
            focus: Panel::Providers,
            ..UiState::default()
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui);
        assert!(contains(&lines, "exhausted"));
    }

    #[test]
    fn help_overlay_shows_keybindings_and_readonly_notice() {
        let snapshot = base_snapshot();
        let ui = UiState {
            help_visible: true,
            ..UiState::default()
        };
        let lines = rendered_lines(NORMAL.0, NORMAL.1, &snapshot, &ui);
        assert!(contains(&lines, "quit"));
        assert!(
            contains(&lines, "read-only"),
            "help must state the dashboard is read-only"
        );
    }

    #[test]
    fn display_text_strips_control_bytes_and_caps_length() {
        let hostile = "red\u{1b}[31malert-that-is-quite-long-indeed";
        let capped = display_text(hostile, 10);
        assert!(!capped.contains('\u{1b}'));
        assert!(capped.chars().count() <= 10);
    }

    #[test]
    fn display_block_preserves_newlines_while_sanitizing() {
        let hostile = "line one\u{1b}[2J\nline two";
        let capped = display_block(hostile, 100);
        assert!(!capped.contains('\u{1b}'));
        assert_eq!(capped.lines().count(), 2);
    }

    /// Spec §120: the Providers panel must preserve availability, source,
    /// checked/data-as-of/expiry timestamps, windows, and the exclusion
    /// reason — plus the allowlisted `extra` subset Task 2 already
    /// sanitized down to `observation_expiry_basis`/`observation_model`.
    /// Parsing those and then not rendering them makes the whole payload
    /// dead data.
    #[test]
    fn provider_windows_expiry_and_allowlisted_extra_are_rendered() {
        let at = now();
        let mut extra = BTreeMap::new();
        extra.insert(
            "observation_expiry_basis".to_string(),
            "provider-reset".to_string(),
        );
        extra.insert("observation_model".to_string(), "gpt-5.6-luna".to_string());
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            ProviderStatusSnapshot {
                availability: Availability::Caution,
                source: "api".to_string(),
                checked_at: "2026-07-25T18:40:00Z".to_string(),
                data_as_of: Some("2026-07-25T18:35:00Z".to_string()),
                expires_at: Some("2026-07-25T23:00:00Z".to_string()),
                windows: vec![Window {
                    label: "5h".to_string(),
                    percent: Some(82.5),
                    reset_at: Some("2026-07-25T22:00:00Z".to_string()),
                }],
                reason: Some("weekly budget nearly spent".to_string()),
                extra,
            },
        );
        let mut snapshot = base_snapshot();
        snapshot.musterroll = Arc::new(SourceState::Fresh {
            value: MusterrollSnapshot {
                schema: "musterroll/status@1".to_string(),
                checked_at: "2026-07-25T18:40:00Z".to_string(),
                providers,
            },
            last_ok: at,
            last_attempt: at,
            truncated: false,
        });
        let ui = UiState {
            focus: Panel::Providers,
            ..UiState::default()
        };
        let lines = rendered_lines(WIDE.0, WIDE.1, &snapshot, &ui);
        assert!(contains(&lines, "caution"));
        assert!(contains(&lines, "5h 82.5%"), "window label and percent");
        assert!(
            contains(&lines, "2026-07-25T22:00:00Z"),
            "window reset timestamp"
        );
        assert!(contains(&lines, "2026-07-25T23:00:00Z"), "expiry timestamp");
        assert!(contains(&lines, "2026-07-25T18:35:00Z"), "data-as-of");
        assert!(contains(&lines, "weekly budget nearly spent"));
        assert!(contains(&lines, "observation_model=gpt-5.6-luna"));
        assert!(contains(&lines, "observation_expiry_basis=provider-reset"));
    }

    /// A window whose percentage Musterroll reported as null must say so,
    /// never render as `0%` — an exhausted-looking budget that no source
    /// actually claimed.
    #[test]
    fn a_window_without_a_percentage_says_unknown_not_zero() {
        let at = now();
        let mut providers = BTreeMap::new();
        let mut status = base_provider(Availability::Unknown);
        status.windows = vec![Window {
            label: "weekly".to_string(),
            percent: None,
            reset_at: None,
        }];
        providers.insert("neuralwatt".to_string(), status);
        let mut snapshot = base_snapshot();
        snapshot.musterroll = Arc::new(SourceState::Fresh {
            value: MusterrollSnapshot {
                schema: "musterroll/status@1".to_string(),
                checked_at: "2026-07-25T18:40:00Z".to_string(),
                providers,
            },
            last_ok: at,
            last_attempt: at,
            truncated: false,
        });
        let ui = UiState {
            focus: Panel::Providers,
            ..UiState::default()
        };
        let lines = rendered_lines(WIDE.0, WIDE.1, &snapshot, &ui);
        assert!(contains(&lines, "weekly ?%"));
        assert!(!contains(&lines, "weekly 0"));
    }

    /// The on-demand log tail the runtime attaches to `RunSnapshot.logs`
    /// must render — with its allowlisted path, its truncation marker, and
    /// no control bytes from the log's own content.
    #[test]
    fn an_open_log_tail_renders_sanitized_with_its_path_and_truncation() {
        let mut snapshot = base_snapshot();
        if let SourceState::Fresh { value, .. } = &mut snapshot.run {
            value.logs = vec![LogTail {
                path: "attempts/001-openai-codex--codex--gpt-5.6-luna--high/worker.stdout.log"
                    .to_string(),
                text: "first line\u{1b}[2J\nsecond line".to_string(),
                truncated: true,
            }];
        }
        let lines = rendered_lines(WIDE.0, WIDE.1, &snapshot, &UiState::default());
        assert!(contains(&lines, "worker.stdout.log"));
        assert!(contains(&lines, "truncated"));
        assert!(contains(&lines, "first line"));
        assert!(contains(&lines, "second line"));
        assert!(
            !lines.iter().any(|line| line.contains('\u{1b}')),
            "a log tail must never emit terminal control bytes"
        );
    }
}
