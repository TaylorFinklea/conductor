//! Terminal-safe runtime for the Undertake dashboard: input/tick/service-
//! message handling, the nonblocking event loop, and terminal restoration.
//!
//! [`state`] is the pure, read-only intent/state layer: keys and ticks
//! convert to [`state::DashboardIntent`] / cadence checks, [`state::DashboardApp`]
//! applies them and emits [`state::RuntimeAction`]s describing side effects —
//! never performing them itself, so this layer is fully unit-testable without
//! a terminal, a filesystem, or a subprocess.
//!
//! [`terminal`] owns raw-mode/alternate-screen setup and idempotent
//! restoration (panic hook, `Drop` guard, SIGTERM/SIGHUP).
//!
//! [`run_dashboard`] wires both to [`super::run_source::DashboardRunSource`]
//! and the Task 2 service adapters into the actual event loop.

pub(crate) mod state {
    //! Pure, read-only intent/state layer. `DashboardApp` holds only UI
    //! selection state and per-source cadence/in-flight bookkeeping — no
    //! reader, command, or mutable run handle — so every scenario here is
    //! tested with synthetic timestamps and plain string context, no
    //! terminal, filesystem, or subprocess involved.

    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::super::render::{Panel, UiState};
    use super::super::run_source::LogSelector;

    /// How often Musterroll refreshes automatically, independent of the
    /// local artifact refresh cadence.
    const MUSTERROLL_REFRESH_INTERVAL: chrono::Duration = chrono::Duration::seconds(30);
    /// The minimum spacing between on-demand Evidence (Afterfact) refreshes.
    /// Afterfact never refreshes automatically; only `r` while Evidence is
    /// focused triggers it, and never more often than this.
    const EVIDENCE_MIN_REFRESH_INTERVAL: chrono::Duration = chrono::Duration::seconds(300);
    /// Fallback local cadence if a configured interval somehow does not fit
    /// `chrono::Duration` (unreachable in practice: the CLI bounds
    /// `--refresh-ms` to 250-60000ms, astronomically inside `chrono`'s
    /// range). Matches the command contract's own stated default.
    const DEFAULT_LOCAL_REFRESH_INTERVAL: chrono::Duration = chrono::Duration::seconds(1);

    /// A read-only user-requested action, translated from a key press.
    /// Ticks and service replies are handled separately ([`DashboardApp::on_tick`],
    /// [`DashboardApp::complete_musterroll`], [`DashboardApp::complete_afterfact`])
    /// since they do not originate from the terminal input stream.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DashboardIntent {
        Quit,
        FocusNext,
        FocusPrevious,
        SelectionUp,
        SelectionDown,
        /// Enter: on Recent Runs switches to the highlighted run; on Active
        /// Run toggles the highlighted attempt's log (same handling as
        /// [`DashboardIntent::ToggleLogDetail`]). Providers and Evidence
        /// have neither a run nor a log, so it is inert there.
        Activate,
        /// `l`: toggles the *focused* panel's log detail. Only Active Run
        /// owns a log, so the key is inert on every other panel.
        ToggleLogDetail,
        /// `r`: immediate Evidence refresh, subject to focus/cadence/in-flight
        /// eligibility.
        RequestRefresh,
        ToggleHelp,
    }

    /// Translates one crossterm key press into a [`DashboardIntent`]; `None`
    /// for unrecognized keys and for anything but a `Press` event — a
    /// `Release`/`Repeat` event (Windows, or a Unix terminal with keyboard
    /// enhancement flags) must never double-fire an action.
    pub(crate) fn intent_for_key(key: KeyEvent) -> Option<DashboardIntent> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => Some(DashboardIntent::Quit),
            (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                Some(DashboardIntent::Quit)
            }
            (KeyCode::Char('j') | KeyCode::Down, _) => Some(DashboardIntent::SelectionDown),
            (KeyCode::Char('k') | KeyCode::Up, _) => Some(DashboardIntent::SelectionUp),
            (KeyCode::Tab, _) => Some(DashboardIntent::FocusNext),
            (KeyCode::BackTab, _) => Some(DashboardIntent::FocusPrevious),
            (KeyCode::Enter, _) => Some(DashboardIntent::Activate),
            (KeyCode::Char('l'), _) => Some(DashboardIntent::ToggleLogDetail),
            (KeyCode::Char('r'), _) => Some(DashboardIntent::RequestRefresh),
            (KeyCode::Char('?'), _) => Some(DashboardIntent::ToggleHelp),
            _ => None,
        }
    }

    /// A side effect [`DashboardApp`] wants performed. The app never performs
    /// these itself — it has no reader, command, or mutable run handle — so
    /// [`super::run_dashboard`]'s event loop is the only thing that acts on
    /// them.
    ///
    /// Deliberately no `RefreshCautionlight`: the spec calls it
    /// "roadmap-deferred" and Task 2's `CautionlightDashboardSource::read`
    /// exists fully implemented and fixture-tested, but v1's live
    /// acceptance explicitly expects Cautionlight to still read `deferred`
    /// even after a real Afterfact fetch (spec's Task 4 Step 4 criterion).
    /// That is only possible if the dashboard never calls `read` at all, on
    /// a tick or on `r` — so this runtime doesn't. `carried_services` (Task
    /// 1/2) already carries [`crate::dashboard::model::SourceState::Deferred`]
    /// forward unchanged on every tick with no help needed from here.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum RuntimeAction {
        Quit,
        RefreshLocal,
        RefreshMusterroll,
        RefreshAfterfact,
        ReadLog(LogSelector),
        CloseLog,
        SelectRun(String),
    }

    /// Plain facts [`DashboardApp::on_key`] needs to resolve an `Activate`/
    /// `ToggleLogDetail` intent into a concrete action: which attempt
    /// directories and recent run ids the current snapshot shows. Plain
    /// strings only — no snapshot, reader, or command reaches the app.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct KeyContext<'a> {
        pub(crate) attempt_dirs: &'a [Option<String>],
        pub(crate) recent_run_ids: &'a [String],
    }

    /// The dashboard's UI-only runtime state: [`UiState`] (what the renderer
    /// sees) plus cadence/in-flight bookkeeping for the sources this runtime
    /// schedules (local artifacts, Musterroll, on-demand Afterfact). Holds no
    /// reader, command, or mutable run handle — only plain data and pure
    /// transition methods, which is what makes it unit-testable without a
    /// terminal, filesystem, or subprocess.
    #[derive(Debug, Clone)]
    pub(crate) struct DashboardApp {
        pub(crate) ui: UiState,
        local_refresh_interval: Duration,
        last_local_refresh: Option<DateTime<Utc>>,
        last_musterroll_dispatch: Option<DateTime<Utc>>,
        musterroll_inflight: bool,
        last_afterfact_dispatch: Option<DateTime<Utc>>,
        afterfact_inflight: bool,
        log_open: bool,
        /// Bumped whenever the pinned run changes. An Afterfact reply tagged
        /// with an older generation was dispatched for a run the user has
        /// since navigated away from; applying it would show stale evidence
        /// under the new run's identity, so it is discarded instead.
        generation: u64,
    }

    impl DashboardApp {
        pub(crate) fn new(local_refresh_interval: Duration) -> Self {
            Self {
                ui: UiState::default(),
                local_refresh_interval,
                last_local_refresh: None,
                last_musterroll_dispatch: None,
                musterroll_inflight: false,
                last_afterfact_dispatch: None,
                afterfact_inflight: false,
                log_open: false,
                generation: 0,
            }
        }

        pub(crate) fn current_generation(&self) -> u64 {
            self.generation
        }

        /// Applies one key-derived intent, returning the side effects the
        /// runtime must perform. `now` drives cadence/eligibility checks;
        /// `ctx` supplies the plain facts needed to resolve `Activate`/
        /// `ToggleLogDetail` into a concrete action.
        pub(crate) fn on_key(
            &mut self,
            intent: DashboardIntent,
            now: DateTime<Utc>,
            ctx: &KeyContext<'_>,
        ) -> Vec<RuntimeAction> {
            match intent {
                DashboardIntent::Quit => vec![RuntimeAction::Quit],
                DashboardIntent::FocusNext => {
                    self.ui.focus = self.ui.focus.next();
                    Vec::new()
                }
                DashboardIntent::FocusPrevious => {
                    self.ui.focus = self.ui.focus.previous();
                    Vec::new()
                }
                DashboardIntent::SelectionDown => {
                    let last_row = self.last_row(ctx);
                    let selected = self.selected_mut();
                    *selected = selected.saturating_add(1).min(last_row);
                    Vec::new()
                }
                DashboardIntent::SelectionUp => {
                    let selected = self.selected_mut();
                    *selected = selected.saturating_sub(1);
                    Vec::new()
                }
                DashboardIntent::ToggleHelp => {
                    self.ui.help_visible = !self.ui.help_visible;
                    Vec::new()
                }
                DashboardIntent::Activate => self.activate(ctx),
                DashboardIntent::ToggleLogDetail => self.toggle_focused_log(ctx),
                DashboardIntent::RequestRefresh => self.request_evidence_refresh(now),
            }
        }

        fn selected_mut(&mut self) -> &mut usize {
            match self.ui.focus {
                Panel::RecentRuns => &mut self.ui.recent_selected,
                Panel::ActiveRun | Panel::Providers | Panel::Evidence => {
                    &mut self.ui.attempt_selected
                }
            }
        }

        /// The highest index the focused panel's list can address, or `0`
        /// when it is empty. The renderer clamps its *highlight* the same
        /// way (`render::active_run_attempts_or_stages_lines`,
        /// `render::recent_run_items`); clamping here too is what keeps the
        /// highlighted row and the row `Enter`/`l` acts on the same row.
        fn last_row(&self, ctx: &KeyContext<'_>) -> usize {
            match self.ui.focus {
                Panel::RecentRuns => ctx.recent_run_ids.len(),
                Panel::ActiveRun | Panel::Providers | Panel::Evidence => ctx.attempt_dirs.len(),
            }
            .saturating_sub(1)
        }

        fn activate(&mut self, ctx: &KeyContext<'_>) -> Vec<RuntimeAction> {
            match self.ui.focus {
                Panel::RecentRuns => {
                    let Some(run_id) = ctx.recent_run_ids.get(self.ui.recent_selected) else {
                        return Vec::new();
                    };
                    self.select_run(run_id.clone())
                }
                Panel::ActiveRun => self.toggle_focused_log(ctx),
                // Neither panel has a selected attempt or run to open, and
                // reaching across to the Active Run's log is precisely what
                // `l` must not do either.
                Panel::Providers | Panel::Evidence => Vec::new(),
            }
        }

        /// Pins the dashboard to a different run. Per-run UI state (the
        /// attempt cursor, any open log) belongs to the run that produced
        /// it, so both reset; focus jumps to Active Run so the newly
        /// selected run's detail is immediately visible.
        fn select_run(&mut self, run_id: String) -> Vec<RuntimeAction> {
            self.generation += 1;
            self.ui.attempt_selected = 0;
            self.ui.focus = Panel::ActiveRun;
            self.log_open = false;
            vec![RuntimeAction::SelectRun(run_id)]
        }

        /// `l`, and `Enter` on Active Run: toggles the *focused* panel's
        /// log detail (spec §200). Only Active Run owns a log — Providers,
        /// Evidence, and Recent Runs have none — so the key is inert there
        /// rather than silently reaching across panels to open, or worse
        /// close, a log belonging to a panel the user is not looking at.
        fn toggle_focused_log(&mut self, ctx: &KeyContext<'_>) -> Vec<RuntimeAction> {
            if self.ui.focus != Panel::ActiveRun {
                return Vec::new();
            }
            if self.log_open {
                self.log_open = false;
                return vec![RuntimeAction::CloseLog];
            }
            let Some(Some(attempt_dir)) = ctx.attempt_dirs.get(self.ui.attempt_selected) else {
                return Vec::new();
            };
            self.log_open = true;
            vec![RuntimeAction::ReadLog(LogSelector::WorkerStdout(
                attempt_dir.clone(),
            ))]
        }

        fn request_evidence_refresh(&mut self, now: DateTime<Utc>) -> Vec<RuntimeAction> {
            if self.ui.focus != Panel::Evidence || self.afterfact_inflight {
                return Vec::new();
            }
            if let Some(last) = self.last_afterfact_dispatch {
                if now - last < EVIDENCE_MIN_REFRESH_INTERVAL {
                    return Vec::new();
                }
            }
            self.afterfact_inflight = true;
            self.last_afterfact_dispatch = Some(now);
            vec![RuntimeAction::RefreshAfterfact]
        }

        /// Checks cadence-driven refreshes for a periodic wake. Local
        /// artifact reads happen on every configured interval; Musterroll
        /// refreshes on a fixed, coarser 30-second cadence and is skipped —
        /// dropped, never queued — while a request is already in flight.
        pub(crate) fn on_tick(&mut self, now: DateTime<Utc>) -> Vec<RuntimeAction> {
            let mut actions = Vec::new();

            let local_interval = chrono::Duration::from_std(self.local_refresh_interval)
                .unwrap_or(DEFAULT_LOCAL_REFRESH_INTERVAL);
            let local_due = self
                .last_local_refresh
                .is_none_or(|last| now - last >= local_interval);
            if local_due {
                self.last_local_refresh = Some(now);
                actions.push(RuntimeAction::RefreshLocal);
            }

            let musterroll_due = self
                .last_musterroll_dispatch
                .is_none_or(|last| now - last >= MUSTERROLL_REFRESH_INTERVAL);
            if musterroll_due && !self.musterroll_inflight {
                self.musterroll_inflight = true;
                self.last_musterroll_dispatch = Some(now);
                actions.push(RuntimeAction::RefreshMusterroll);
            }

            actions
        }

        /// Marks the Musterroll worker free to accept another request.
        pub(crate) fn complete_musterroll(&mut self) {
            self.musterroll_inflight = false;
        }

        /// Marks the Afterfact worker free, and reports whether the reply's
        /// generation still matches: `false` means the run selection changed
        /// since the request was dispatched, and the caller must discard the
        /// payload rather than apply it.
        pub(crate) fn complete_afterfact(&mut self, reply_generation: u64) -> bool {
            self.afterfact_inflight = false;
            reply_generation == self.generation
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn ts(text: &str) -> DateTime<Utc> {
            DateTime::parse_from_rfc3339(text)
                .expect("valid rfc3339")
                .with_timezone(&Utc)
        }

        fn key(code: KeyCode) -> KeyEvent {
            KeyEvent::new(code, KeyModifiers::NONE)
        }

        fn ctrl(code: KeyCode) -> KeyEvent {
            KeyEvent::new(code, KeyModifiers::CONTROL)
        }

        fn empty_ctx() -> KeyContext<'static> {
            KeyContext {
                attempt_dirs: &[],
                recent_run_ids: &[],
            }
        }

        fn app() -> DashboardApp {
            DashboardApp::new(Duration::from_secs(1))
        }

        #[test]
        fn j_and_down_move_selection_down() {
            assert_eq!(
                intent_for_key(key(KeyCode::Char('j'))),
                Some(DashboardIntent::SelectionDown)
            );
            assert_eq!(
                intent_for_key(key(KeyCode::Down)),
                Some(DashboardIntent::SelectionDown)
            );
            let mut app = app();
            let ctx = KeyContext {
                attempt_dirs: &[Some("001-a".to_string()), Some("002-b".to_string())],
                recent_run_ids: &[],
            };
            app.on_key(
                DashboardIntent::SelectionDown,
                ts("2026-01-01T00:00:00Z"),
                &ctx,
            );
            assert_eq!(app.ui.attempt_selected, 1);
        }

        #[test]
        fn k_and_up_move_selection_up_saturating_at_zero() {
            assert_eq!(
                intent_for_key(key(KeyCode::Char('k'))),
                Some(DashboardIntent::SelectionUp)
            );
            assert_eq!(
                intent_for_key(key(KeyCode::Up)),
                Some(DashboardIntent::SelectionUp)
            );
            let mut app = app();
            app.on_key(
                DashboardIntent::SelectionUp,
                ts("2026-01-01T00:00:00Z"),
                &empty_ctx(),
            );
            assert_eq!(
                app.ui.attempt_selected, 0,
                "selection must saturate, never underflow"
            );
        }

        #[test]
        fn selection_targets_the_focused_panels_own_list() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            let ctx = KeyContext {
                attempt_dirs: &[Some("001-a".to_string()), Some("002-b".to_string())],
                recent_run_ids: &["run-a".to_string(), "run-b".to_string()],
            };
            app.on_key(DashboardIntent::SelectionDown, now, &ctx);
            assert_eq!(app.ui.attempt_selected, 1);
            assert_eq!(app.ui.recent_selected, 0);
            app.ui.focus = Panel::RecentRuns;
            app.on_key(DashboardIntent::SelectionDown, now, &ctx);
            assert_eq!(app.ui.recent_selected, 1);
            assert_eq!(
                app.ui.attempt_selected, 1,
                "the other list's cursor is untouched"
            );
        }

        /// The renderer clamps the *highlight* to the last row
        /// (`render::active_run_attempts_or_stages_lines`), so a cursor that
        /// kept counting past the end would leave a visibly highlighted row
        /// that `Enter`/`l` silently refuse to act on. State and render must
        /// agree on which row is selected.
        #[test]
        fn selection_down_clamps_to_the_last_row_and_stays_actionable() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            let ctx = KeyContext {
                attempt_dirs: &[Some("001-a".to_string()), Some("002-b".to_string())],
                recent_run_ids: &[],
            };
            for _ in 0..10 {
                app.on_key(DashboardIntent::SelectionDown, now, &ctx);
            }
            assert_eq!(
                app.ui.attempt_selected, 1,
                "the cursor must stop on the last attempt, not run past the list"
            );
            assert_eq!(
                app.on_key(DashboardIntent::ToggleLogDetail, now, &ctx),
                vec![RuntimeAction::ReadLog(LogSelector::WorkerStdout(
                    "002-b".to_string()
                ))],
                "the highlighted row must remain actionable"
            );
        }

        #[test]
        fn recent_runs_selection_clamps_to_its_own_list_length() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            app.ui.focus = Panel::RecentRuns;
            // A longer attempt list on purpose: clamping must use the
            // *focused* panel's list, not whichever one happens to be
            // longer.
            let ctx = KeyContext {
                attempt_dirs: &[None, None, None, None, None],
                recent_run_ids: &["run-a".to_string(), "run-b".to_string()],
            };
            for _ in 0..10 {
                app.on_key(DashboardIntent::SelectionDown, now, &ctx);
            }
            assert_eq!(
                app.ui.recent_selected, 1,
                "Recent Runs must clamp against its own list, not the attempt list"
            );
        }

        #[test]
        fn selection_down_on_an_empty_list_stays_at_zero() {
            let mut app = app();
            app.on_key(
                DashboardIntent::SelectionDown,
                ts("2026-01-01T00:00:00Z"),
                &empty_ctx(),
            );
            assert_eq!(
                app.ui.attempt_selected, 0,
                "an empty list has no row to move onto"
            );
        }

        #[test]
        fn tab_and_backtab_cycle_focus_and_wrap() {
            assert_eq!(
                intent_for_key(key(KeyCode::Tab)),
                Some(DashboardIntent::FocusNext)
            );
            assert_eq!(
                intent_for_key(key(KeyCode::BackTab)),
                Some(DashboardIntent::FocusPrevious)
            );
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            assert_eq!(app.ui.focus, Panel::ActiveRun);
            app.on_key(DashboardIntent::FocusNext, now, &empty_ctx());
            assert_eq!(app.ui.focus, Panel::Providers);
            app.on_key(DashboardIntent::FocusPrevious, now, &empty_ctx());
            assert_eq!(app.ui.focus, Panel::ActiveRun);
            app.on_key(DashboardIntent::FocusPrevious, now, &empty_ctx());
            assert_eq!(
                app.ui.focus,
                Panel::RecentRuns,
                "focus must wrap at both ends"
            );
        }

        #[test]
        fn enter_on_recent_runs_selects_that_run_and_bumps_generation() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            app.ui.focus = Panel::RecentRuns;
            app.ui.recent_selected = 1;
            let ctx = KeyContext {
                attempt_dirs: &[],
                recent_run_ids: &["run-a".to_string(), "run-b".to_string()],
            };
            let generation_before = app.current_generation();
            let actions = app.on_key(DashboardIntent::Activate, now, &ctx);
            assert_eq!(actions, vec![RuntimeAction::SelectRun("run-b".to_string())]);
            assert!(
                app.current_generation() > generation_before,
                "switching runs must bump the generation"
            );
            assert_eq!(
                app.ui.focus,
                Panel::ActiveRun,
                "the newly selected run's detail should be shown"
            );
        }

        #[test]
        fn enter_and_l_on_active_run_toggle_the_highlighted_attempt_log() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            let ctx = KeyContext {
                attempt_dirs: &[Some("001-attempt".to_string())],
                recent_run_ids: &[],
            };
            let opened = app.on_key(DashboardIntent::ToggleLogDetail, now, &ctx);
            assert_eq!(
                opened,
                vec![RuntimeAction::ReadLog(LogSelector::WorkerStdout(
                    "001-attempt".to_string()
                ))]
            );
            let closed = app.on_key(DashboardIntent::Activate, now, &ctx);
            assert_eq!(
                closed,
                vec![RuntimeAction::CloseLog],
                "the second press must close, not reopen"
            );
        }

        #[test]
        fn toggle_log_with_no_attempts_is_a_no_op() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            assert_eq!(
                app.on_key(DashboardIntent::ToggleLogDetail, now, &empty_ctx()),
                Vec::new()
            );
        }

        /// Spec §200: `l` toggles the *focused* panel's log detail. Only
        /// Active Run has one, so `l` elsewhere must not reach across and
        /// open the Active Run's attempt log — a bounded file read the user
        /// never asked for, of a panel they are not looking at.
        #[test]
        fn l_is_inert_on_every_panel_that_owns_no_log() {
            let now = ts("2026-01-01T00:00:00Z");
            let ctx = KeyContext {
                attempt_dirs: &[Some("001-attempt".to_string())],
                recent_run_ids: &["run-a".to_string()],
            };
            for focus in [Panel::Providers, Panel::Evidence, Panel::RecentRuns] {
                let mut app = app();
                app.ui.focus = focus;
                assert_eq!(
                    app.on_key(DashboardIntent::ToggleLogDetail, now, &ctx),
                    Vec::new(),
                    "`l` must not open a log while {focus:?} is focused"
                );
            }
            // A focus gate, not a blanket refusal.
            let mut app = app();
            assert_eq!(
                app.on_key(DashboardIntent::ToggleLogDetail, now, &ctx),
                vec![RuntimeAction::ReadLog(LogSelector::WorkerStdout(
                    "001-attempt".to_string()
                ))],
                "`l` still works on the panel that owns the log"
            );
        }

        /// The destructive half of the same defect: with a log already open
        /// on Active Run, `l` from another panel used to emit `CloseLog` and
        /// clear `log_open`, so the log vanished off a screen the user was
        /// not even looking at and the *next* `l` back on Active Run
        /// reopened it instead of closing it.
        #[test]
        fn l_from_another_panel_leaves_an_open_log_open() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            let ctx = KeyContext {
                attempt_dirs: &[Some("001-attempt".to_string())],
                recent_run_ids: &["run-a".to_string()],
            };
            app.on_key(DashboardIntent::ToggleLogDetail, now, &ctx);
            app.ui.focus = Panel::RecentRuns;
            assert_eq!(
                app.on_key(DashboardIntent::ToggleLogDetail, now, &ctx),
                Vec::new(),
                "an open log must not be closed from a panel that cannot show it"
            );
            app.ui.focus = Panel::ActiveRun;
            assert_eq!(
                app.on_key(DashboardIntent::ToggleLogDetail, now, &ctx),
                vec![RuntimeAction::CloseLog],
                "returning to Active Run must close the still-open log, not reopen it"
            );
        }

        /// `Enter` resolves to "selected attempt or run detail" (spec §199).
        /// Providers and Evidence have neither, and both routed through the
        /// same toggle as `l`, so the gate has to hold for both keys.
        #[test]
        fn enter_is_inert_on_providers_and_evidence() {
            let now = ts("2026-01-01T00:00:00Z");
            let ctx = KeyContext {
                attempt_dirs: &[Some("001-attempt".to_string())],
                recent_run_ids: &["run-a".to_string()],
            };
            for focus in [Panel::Providers, Panel::Evidence] {
                let mut app = app();
                app.ui.focus = focus;
                assert_eq!(
                    app.on_key(DashboardIntent::Activate, now, &ctx),
                    Vec::new(),
                    "Enter has no attempt or run to open while {focus:?} is focused"
                );
            }
        }

        #[test]
        fn question_mark_toggles_help() {
            assert_eq!(
                intent_for_key(key(KeyCode::Char('?'))),
                Some(DashboardIntent::ToggleHelp)
            );
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            assert!(!app.ui.help_visible);
            app.on_key(DashboardIntent::ToggleHelp, now, &empty_ctx());
            assert!(app.ui.help_visible);
            app.on_key(DashboardIntent::ToggleHelp, now, &empty_ctx());
            assert!(!app.ui.help_visible);
        }

        #[test]
        fn q_and_ctrl_c_both_quit() {
            assert_eq!(
                intent_for_key(key(KeyCode::Char('q'))),
                Some(DashboardIntent::Quit)
            );
            assert_eq!(
                intent_for_key(ctrl(KeyCode::Char('c'))),
                Some(DashboardIntent::Quit)
            );
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            assert_eq!(
                app.on_key(DashboardIntent::Quit, now, &empty_ctx()),
                vec![RuntimeAction::Quit]
            );
        }

        #[test]
        fn unrecognized_key_yields_no_intent() {
            assert_eq!(intent_for_key(key(KeyCode::Char('z'))), None);
        }

        #[test]
        fn release_events_never_produce_an_intent() {
            let mut event = key(KeyCode::Char('q'));
            event.kind = KeyEventKind::Release;
            assert_eq!(intent_for_key(event), None);
        }

        #[test]
        fn refresh_is_ineligible_outside_evidence_panel() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            app.ui.focus = Panel::ActiveRun;
            assert_eq!(
                app.on_key(DashboardIntent::RequestRefresh, now, &empty_ctx()),
                Vec::new()
            );
        }

        #[test]
        fn refresh_is_eligible_on_first_evidence_focus() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            app.ui.focus = Panel::Evidence;
            assert_eq!(
                app.on_key(DashboardIntent::RequestRefresh, now, &empty_ctx()),
                vec![RuntimeAction::RefreshAfterfact]
            );
        }

        #[test]
        fn refresh_is_ineligible_while_already_in_flight() {
            let mut app = app();
            let now = ts("2026-01-01T00:00:00Z");
            app.ui.focus = Panel::Evidence;
            app.on_key(DashboardIntent::RequestRefresh, now, &empty_ctx());
            assert_eq!(
                app.on_key(DashboardIntent::RequestRefresh, now, &empty_ctx()),
                Vec::new(),
                "a second press before any reply must not issue a duplicate request"
            );
        }

        #[test]
        fn refresh_is_ineligible_within_the_300_second_minimum() {
            let mut app = app();
            let t0 = ts("2026-01-01T00:00:00Z");
            app.ui.focus = Panel::Evidence;
            app.on_key(DashboardIntent::RequestRefresh, t0, &empty_ctx());
            app.complete_afterfact(app.current_generation());
            let too_soon = t0 + chrono::Duration::seconds(299);
            assert_eq!(
                app.on_key(DashboardIntent::RequestRefresh, too_soon, &empty_ctx()),
                Vec::new()
            );
        }

        #[test]
        fn refresh_is_eligible_again_after_the_300_second_minimum() {
            let mut app = app();
            let t0 = ts("2026-01-01T00:00:00Z");
            app.ui.focus = Panel::Evidence;
            app.on_key(DashboardIntent::RequestRefresh, t0, &empty_ctx());
            app.complete_afterfact(app.current_generation());
            let later = t0 + chrono::Duration::seconds(300);
            assert_eq!(
                app.on_key(DashboardIntent::RequestRefresh, later, &empty_ctx()),
                vec![RuntimeAction::RefreshAfterfact]
            );
        }

        #[test]
        fn local_tick_refreshes_at_the_configured_interval_not_more_often() {
            let mut app = DashboardApp::new(Duration::from_secs(1));
            let t0 = ts("2026-01-01T00:00:00Z");
            assert!(app.on_tick(t0).contains(&RuntimeAction::RefreshLocal));
            let soon = t0 + chrono::Duration::milliseconds(500);
            assert!(
                !app.on_tick(soon).contains(&RuntimeAction::RefreshLocal),
                "must not refresh before the interval elapses"
            );
            let later = t0 + chrono::Duration::milliseconds(1000);
            assert!(app.on_tick(later).contains(&RuntimeAction::RefreshLocal));
        }

        #[test]
        fn musterroll_tick_dispatches_every_30_seconds_and_drops_while_in_flight() {
            let mut app = app();
            let t0 = ts("2026-01-01T00:00:00Z");
            assert!(app.on_tick(t0).contains(&RuntimeAction::RefreshMusterroll));
            let soon = t0 + chrono::Duration::seconds(10);
            assert!(
                !app.on_tick(soon)
                    .contains(&RuntimeAction::RefreshMusterroll),
                "must not dispatch again before the 30-second cadence"
            );
            let due_but_still_inflight = t0 + chrono::Duration::seconds(31);
            assert!(
                !app.on_tick(due_but_still_inflight)
                    .contains(&RuntimeAction::RefreshMusterroll),
                "the first request never replied, so a second must be dropped, not queued"
            );
            app.complete_musterroll();
            assert!(
                app.on_tick(due_but_still_inflight)
                    .contains(&RuntimeAction::RefreshMusterroll)
            );
        }

        #[test]
        fn afterfact_reply_is_applied_only_when_generation_matches() {
            let mut app = app();
            let t0 = ts("2026-01-01T00:00:00Z");
            app.ui.focus = Panel::Evidence;
            app.on_key(DashboardIntent::RequestRefresh, t0, &empty_ctx());
            let stale_generation = app.current_generation();

            // The run selection changes before the reply arrives.
            app.ui.focus = Panel::RecentRuns;
            let ctx = KeyContext {
                attempt_dirs: &[],
                recent_run_ids: &["run-other".to_string()],
            };
            app.on_key(DashboardIntent::Activate, t0, &ctx);
            assert!(app.current_generation() > stale_generation);

            // The late reply for the old generation is discarded...
            assert!(!app.complete_afterfact(stale_generation));
            // ...but a reply for the current generation is applied.
            assert!(app.complete_afterfact(app.current_generation()));
        }
    }
}

pub(crate) mod terminal {
    //! Idempotent terminal setup/restoration. Installed *before* raw mode or
    //! any worker starts: release builds use `panic = "abort"` (see
    //! `Cargo.toml`), so a panic never unwinds and `Drop` never runs — the
    //! panic hook is the *only* cleanup opportunity a release-profile panic
    //! gets, which is why it must exist before anything risky begins.

    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crossterm::cursor::{Hide, Show};
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    /// Guards the one-time restoration this process needs to perform.
    /// `run_dashboard` enters raw mode/the alternate screen exactly once per
    /// process, so "restore once" is the correct lifetime for this flag —
    /// see [`restore_terminal`].
    static RESTORED: AtomicBool = AtomicBool::new(false);

    /// Restores raw mode, the alternate screen, and cursor visibility.
    /// Idempotent (guarded by [`RESTORED`]) and cannot itself panic: every
    /// step swallows its own error rather than propagating or unwrapping,
    /// since this runs from inside a panic hook, where panicking again would
    /// abort with no diagnostic printed at all.
    pub(crate) fn restore_terminal() {
        if RESTORED.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }

    /// Installs a panic hook that restores the terminal before delegating to
    /// whatever hook was previously installed. Must run before raw mode or
    /// any worker starts — see the module doc.
    pub(crate) fn install_panic_hook() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    }

    /// Registers safe, dependency-provided SIGTERM/SIGHUP handling: each
    /// signal sets `shutdown` rather than running arbitrary code inside the
    /// handler (a signal handler may only perform async-signal-safe
    /// operations, and setting an atomic bool is one of the few that
    /// qualifies — see `signal_hook::flag`). The main loop polls the
    /// flag and exits its `while` loop normally, so [`TerminalGuard`]
    /// restores the terminal through ordinary `Drop`, exactly as it would
    /// for `q` — SIGTERM/SIGHUP never bypass that path. Ctrl-C is not
    /// registered here: raw mode disables `ISIG`, so crossterm reports it as
    /// an ordinary key event instead of raising `SIGINT`.
    pub(crate) fn install_shutdown_signal(shutdown: &Arc<AtomicBool>) -> io::Result<()> {
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(shutdown))?;
        signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(shutdown))?;
        Ok(())
    }

    /// RAII guard for raw mode and the alternate screen. `Drop` restores
    /// through the same idempotent [`restore_terminal`] the panic hook uses,
    /// so a normal return, an early `?`, and a panic can never disagree
    /// about how the terminal should end up.
    pub(crate) struct TerminalGuard {
        pub(crate) terminal: Terminal<CrosstermBackend<io::Stdout>>,
    }

    impl TerminalGuard {
        /// Enters raw mode and the alternate screen. On failure partway
        /// through, unwinds whatever already succeeded immediately — this
        /// runs before any `TerminalGuard` exists, so no later `Drop` would
        /// ever clean up a partial failure left here.
        pub(crate) fn enter() -> io::Result<Self> {
            enable_raw_mode()?;
            if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
                let _ = disable_raw_mode();
                return Err(error);
            }
            match Terminal::new(CrosstermBackend::new(io::stdout())) {
                Ok(terminal) => Ok(Self { terminal }),
                Err(error) => {
                    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
                    let _ = disable_raw_mode();
                    Err(error)
                }
            }
        }
    }

    impl Drop for TerminalGuard {
        fn drop(&mut self) {
            restore_terminal();
        }
    }

    #[cfg(test)]
    mod tests {
        //! PTY-backed terminal-restoration tests. These spawn the actual
        //! compiled `undertake` release binary (release, not dev: only
        //! `[profile.release]` sets `panic = "abort"`, and proving the
        //! panic hook survives an *abort* — not an unwind — is the whole
        //! point of the induced-panic scenario) attached to a real
        //! pseudo-terminal via `portable-pty`, and inspect the pty's
        //! termios and output bytes after normal quit, `SIGTERM`, `SIGHUP`,
        //! and an induced panic. None of this is reachable through the
        //! ordinary `undertake dashboard` CLI (Task 4 owns that surface);
        //! the subprocess entry point is `dashboard_pty_test_harness`.
        use std::io::{Read, Write};
        use std::path::PathBuf;
        use std::sync::{Arc, LazyLock};
        use std::time::{Duration, Instant};

        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;
        use nix_pty_termios::sys::termios::Termios as PtyTermios;
        use parking_lot::Mutex;
        use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

        /// Builds (once per test-binary run, however many tests call this)
        /// the release `undertake` binary this suite spawns, and returns its
        /// path. `cargo build --release` is a fast no-op once built; the
        /// *first* run pays the crate's real LTO/codegen-units=1 cost.
        fn release_binary() -> PathBuf {
            static BUILT: LazyLock<bool> = LazyLock::new(|| {
                std::process::Command::new(env!("CARGO"))
                    .args(["build", "--release", "--bin", "undertake"])
                    .current_dir(env!("CARGO_MANIFEST_DIR"))
                    .status()
                    .is_ok_and(|status| status.success())
            });
            assert!(*BUILT, "cargo build --release --bin undertake failed");
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/release/undertake")
        }

        /// A scratch state root the harness points the dashboard at. Its
        /// content does not matter for this suite, which tests terminal
        /// behavior, not run data (that is `dashboard::render`'s job) — an
        /// empty `runs-v2/` is enough for the dashboard to boot and render
        /// an absent-run state.
        struct ScratchStateRoot {
            path: PathBuf,
        }

        impl ScratchStateRoot {
            fn new() -> Self {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "undertake-dashboard-pty-{}-{nanos}",
                    std::process::id()
                ));
                std::fs::create_dir_all(path.join("runs-v2")).expect("mkdir scratch state root");
                Self { path }
            }
        }

        impl Drop for ScratchStateRoot {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }

        /// One dashboard session spawned inside a real pseudo-terminal, with
        /// its output continuously drained on a background thread — a pty's
        /// output buffer is small, and an unread child would otherwise block
        /// on write, wedging the very process this suite is trying to quit.
        struct PtySession {
            master: Box<dyn MasterPty + Send>,
            child: Box<dyn Child + Send + Sync>,
            output: Arc<Mutex<Vec<u8>>>,
            /// Taken once at spawn: a pty master hands out its writer
            /// exactly one time, so a session that drives several keystrokes
            /// has to keep it.
            writer: Mutex<Box<dyn std::io::Write + Send>>,
            termios_before_spawn: Option<PtyTermios>,
        }

        impl PtySession {
            fn spawn(state_root: &std::path::Path, induce_panic: bool) -> Self {
                let mut cmd = CommandBuilder::new(release_binary());
                cmd.arg("__dashboard_pty_test_harness");
                cmd.env(
                    "UNDERTAKE_PTY_TEST_STATE_ROOT",
                    state_root.display().to_string(),
                );
                if induce_panic {
                    cmd.env("UNDERTAKE_PTY_TEST_INDUCE_PANIC", "1");
                }
                Self::spawn_command(cmd, 24, 80)
            }

            /// Spawns an arbitrary already-configured command on a fresh pty
            /// slave. Split out so the same session plumbing drives both the
            /// sentinel harness (which the induced-panic scenario needs) and
            /// the real `undertake dashboard` command Task 4 ships.
            fn spawn_command(cmd: CommandBuilder, rows: u16, cols: u16) -> Self {
                let pty_system = native_pty_system();
                let pair = pty_system
                    .openpty(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .expect("open pty");
                let termios_before_spawn = pair.master.get_termios();

                let child = pair
                    .slave
                    .spawn_command(cmd)
                    .expect("spawn dashboard under pty");
                // The parent must not keep the slave's write end open: the
                // master's reader only sees EOF once every slave-side file
                // descriptor is closed, and this handle would wedge that
                // forever if left open.
                drop(pair.slave);

                let output = Arc::new(Mutex::new(Vec::new()));
                let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
                let output_writer = Arc::clone(&output);
                std::thread::spawn(move || {
                    let mut buffer = [0_u8; 4096];
                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(count) => output_writer.lock().extend_from_slice(&buffer[..count]),
                        }
                    }
                });

                let writer = Mutex::new(pair.master.take_writer().expect("pty writer"));

                Self {
                    master: pair.master,
                    child,
                    output,
                    writer,
                    termios_before_spawn,
                }
            }

            fn output_snapshot(&self) -> Vec<u8> {
                self.output.lock().clone()
            }

            /// Blocks (bounded) until the dashboard has written *something* —
            /// evidence it has entered raw mode/the alternate screen (the
            /// first bytes `TerminalGuard::enter` writes, before the loop
            /// even starts) or drawn a frame.
            fn wait_until_drawn(&self, timeout: Duration) {
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    if !self.output_snapshot().is_empty() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                panic!("dashboard produced no output within {timeout:?}");
            }

            fn send_text(&self, text: &str) {
                let mut writer = self.writer.lock();
                writer.write_all(text.as_bytes()).expect("write to pty");
                writer.flush().expect("flush pty writer");
            }

            fn send_signal(&self, signal: Signal) {
                let pid = self.child.process_id().expect("child pid");
                let pid = i32::try_from(pid).expect("real pids fit in i32");
                signal::kill(Pid::from_raw(pid), signal).expect("send signal");
            }

            /// Blocks (bounded) for the child to exit; `true` iff it did.
            fn wait_for_exit(&mut self, timeout: Duration) -> bool {
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    if matches!(self.child.try_wait(), Ok(Some(_))) {
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                false
            }

            fn termios_now(&self) -> Option<PtyTermios> {
                self.master.get_termios()
            }
        }

        fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
            haystack
                .windows(needle.len())
                .any(|window| window == needle)
        }

        /// The one assertion every scenario below makes: the pty's output
        /// left the alternate screen and re-showed the cursor, and its
        /// termios settings are back to their exact pre-spawn state —
        /// whether the process quit normally, was signalled, or aborted.
        fn assert_terminal_restored(session: &PtySession, output: &[u8]) {
            let text = String::from_utf8_lossy(output);
            assert!(
                contains_subsequence(output, b"\x1b[?1049l"),
                "output never left the alternate screen: {text:?}"
            );
            assert!(
                contains_subsequence(output, b"\x1b[?25h"),
                "output never re-showed the cursor: {text:?}"
            );
            assert_eq!(
                session.termios_now(),
                session.termios_before_spawn,
                "raw mode must be restored to the exact pre-spawn termios state"
            );
        }

        #[test]
        fn normal_quit_restores_the_terminal() {
            let state_root = ScratchStateRoot::new();
            let mut session = PtySession::spawn(&state_root.path, false);
            session.wait_until_drawn(Duration::from_secs(15));
            session.send_text("q");
            assert!(
                session.wait_for_exit(Duration::from_secs(15)),
                "dashboard did not exit after q"
            );
            let output = session.output_snapshot();
            assert_terminal_restored(&session, &output);
        }

        #[test]
        fn sigterm_restores_the_terminal() {
            let state_root = ScratchStateRoot::new();
            let mut session = PtySession::spawn(&state_root.path, false);
            session.wait_until_drawn(Duration::from_secs(15));
            session.send_signal(Signal::SIGTERM);
            assert!(
                session.wait_for_exit(Duration::from_secs(15)),
                "dashboard did not exit after SIGTERM"
            );
            let output = session.output_snapshot();
            assert_terminal_restored(&session, &output);
        }

        #[test]
        fn sighup_restores_the_terminal() {
            let state_root = ScratchStateRoot::new();
            let mut session = PtySession::spawn(&state_root.path, false);
            session.wait_until_drawn(Duration::from_secs(15));
            session.send_signal(Signal::SIGHUP);
            assert!(
                session.wait_for_exit(Duration::from_secs(15)),
                "dashboard did not exit after SIGHUP"
            );
            let output = session.output_snapshot();
            assert_terminal_restored(&session, &output);
        }

        /// The discriminating case: release builds set `panic = "abort"`
        /// (`Cargo.toml`), so `Drop` never runs for an aborting panic. If
        /// restoration depended on `TerminalGuard::drop` alone, this test
        /// would fail — only the panic hook installed before raw-mode entry
        /// can restore the terminal here.
        #[test]
        fn induced_release_panic_still_restores_the_terminal() {
            let state_root = ScratchStateRoot::new();
            let mut session = PtySession::spawn(&state_root.path, true);
            assert!(
                session.wait_for_exit(Duration::from_secs(15)),
                "the induced panic did not terminate the process"
            );
            let output = session.output_snapshot();
            assert_terminal_restored(&session, &output);
        }

        // -------------------------------------------------------------
        // The shipped `undertake dashboard` command (Task 4)
        // -------------------------------------------------------------

        /// A minimal terminal replay: enough of the CSI grammar to turn the
        /// pty byte stream back into the screen the operator would have
        /// seen. Ratatui addresses cells with absolute cursor moves and
        /// interleaves styling, so a raw substring search over the stream
        /// answers "were these bytes written" rather than "was this on the
        /// screen" — and only the second question is worth asserting.
        struct Screen {
            rows: Vec<Vec<char>>,
            row: usize,
            col: usize,
        }

        impl Screen {
            fn replay(bytes: &[u8], rows: usize, cols: usize) -> Self {
                let mut screen = Self {
                    rows: vec![vec![' '; cols]; rows],
                    row: 0,
                    col: 0,
                };
                let text = String::from_utf8_lossy(bytes).into_owned();
                let mut chars = text.chars().peekable();
                while let Some(ch) = chars.next() {
                    match ch {
                        '\x1b' => screen.escape(&mut chars),
                        '\r' => screen.col = 0,
                        '\n' => {
                            screen.row = screen.row.saturating_add(1);
                            screen.col = 0;
                        }
                        ch if (ch as u32) < 0x20 => {}
                        ch => screen.put(ch),
                    }
                }
                screen
            }

            fn put(&mut self, ch: char) {
                let cols = self.rows[0].len();
                if self.col >= cols {
                    self.col = 0;
                    self.row = self.row.saturating_add(1);
                }
                if let Some(row) = self.rows.get_mut(self.row) {
                    row[self.col] = ch;
                }
                self.col += 1;
            }

            /// Consumes one escape sequence and applies the few that move or
            /// erase. Everything else (SGR, alternate screen, cursor
            /// visibility, OSC) only changes appearance, so it is skipped.
            fn escape<I: Iterator<Item = char>>(&mut self, chars: &mut std::iter::Peekable<I>) {
                if chars.peek() != Some(&'[') {
                    chars.next();
                    return;
                }
                chars.next();
                let mut params = String::new();
                let mut final_byte = ' ';
                for ch in chars.by_ref() {
                    if ch.is_ascii_alphabetic() || ch == '@' {
                        final_byte = ch;
                        break;
                    }
                    params.push(ch);
                }
                let numbers: Vec<usize> = params
                    .trim_start_matches('?')
                    .split(';')
                    .map(|part| part.parse::<usize>().unwrap_or(0))
                    .collect();
                let first = numbers.first().copied().unwrap_or(0);
                match final_byte {
                    'H' | 'f' => {
                        self.row = first.saturating_sub(1);
                        self.col = numbers.get(1).copied().unwrap_or(1).saturating_sub(1);
                    }
                    'A' => self.row = self.row.saturating_sub(first.max(1)),
                    'B' => self.row = self.row.saturating_add(first.max(1)),
                    'C' => self.col = self.col.saturating_add(first.max(1)),
                    'D' => self.col = self.col.saturating_sub(first.max(1)),
                    'J' => {
                        for row in &mut self.rows {
                            row.fill(' ');
                        }
                        self.row = 0;
                        self.col = 0;
                    }
                    'K' => {
                        if let Some(row) = self.rows.get_mut(self.row) {
                            for cell in row.iter_mut().skip(self.col) {
                                *cell = ' ';
                            }
                        }
                    }
                    _ => {}
                }
            }

            fn text(&self) -> String {
                self.rows
                    .iter()
                    .map(|row| row.iter().collect::<String>().trim_end().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }

        /// Every file under `root`, mapped to its length, modification time,
        /// and content digest. Comparing two of these is the read-only
        /// proof: a dashboard that opened anything for write, rewrote a
        /// heartbeat, or dropped a lock file changes at least one entry.
        fn tree_fingerprint(root: &std::path::Path) -> std::collections::BTreeMap<String, String> {
            use sha2::{Digest as _, Sha256};
            let mut fingerprint = std::collections::BTreeMap::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    if metadata.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    let digest = std::fs::read(&path).map_or_else(
                        |error| format!("unreadable: {error}"),
                        |bytes| format!("{:x}", Sha256::digest(&bytes)),
                    );
                    let mtime = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or_else(|| "?".to_string(), |since| since.as_nanos().to_string());
                    fingerprint.insert(
                        path.display().to_string(),
                        format!("{} {mtime} {digest}", metadata.len()),
                    );
                }
            }
            fingerprint
        }

        /// Polls the replayed screen until `predicate` holds, so an
        /// assertion never races the 250 ms refresh or the render that
        /// follows a keystroke.
        fn wait_for_screen(
            session: &PtySession,
            rows: usize,
            cols: usize,
            what: &str,
            predicate: impl Fn(&str) -> bool,
        ) -> String {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut last = String::new();
            while Instant::now() < deadline {
                last = Screen::replay(&session.output_snapshot(), rows, cols).text();
                if predicate(&last) {
                    return last;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("never saw {what} on screen. Last frame:\n{last}");
        }

        /// Task 4 Step 3: the shipped command, end to end, against a
        /// synthetic *active* run — launch, observe a local refresh pick up
        /// a concurrent manifest write, navigate, open a log, open help,
        /// trigger on-demand Evidence, quit clean, and mutate nothing.
        ///
        /// Deliberately drives `undertake dashboard --run … --refresh-ms …
        /// --config …` rather than the sentinel harness the restoration
        /// scenarios above use: this is the surface an operator gets, so it
        /// is the surface the read-only and restoration contracts have to
        /// hold on.
        #[test]
        fn dashboard_cli_session_navigates_reads_evidence_and_leaves_state_untouched() {
            use crate::dashboard::run_source::test_support::{
                PATCHSTAND_ATTEMPT_DIR, PATCHSTAND_PROFILE_ID, PATCHSTAND_RUN_ID, TempState,
            };

            const ROWS: u16 = 40;
            const COLS: u16 = 120;
            let created = "2026-07-25T18:39:20.469500+00:00";
            let first_update = "2026-07-25T18:43:44.617226+00:00";
            let second_update = "2026-07-25T18:59:59.123456+00:00";

            let temp = TempState::new();
            // An *active* run: a heartbeat written now is younger than the
            // 60-second stale threshold, so liveness reads `live` — the
            // opposite of the pinned abandoned pilot fixture, and the state
            // a refresh actually has to keep up with.
            temp.write_patchstand_run(
                PATCHSTAND_RUN_ID,
                created,
                first_update,
                &chrono::Utc::now().to_rfc3339(),
                std::process::id(),
                std::process::id(),
            );
            // The run's Harness Deck report, where the join must find it.
            let reports_home = temp.root().join("reports-home");
            let report_dir =
                crate::deck::report_run_dir(&reports_home, "cycle-20260725-183823").unwrap();
            std::fs::create_dir_all(&report_dir).unwrap();
            std::fs::write(report_dir.join("report.json"), b"{}\n").unwrap();

            let before = tree_fingerprint(temp.root());

            let mut cmd = CommandBuilder::new(release_binary());
            cmd.args([
                "dashboard",
                "--run",
                PATCHSTAND_RUN_ID,
                "--refresh-ms",
                "250",
                "--config",
                &format!("{}/undertake.toml", env!("CARGO_MANIFEST_DIR")),
            ]);
            cmd.env("UNDERTAKE_STATE_DIR", temp.root().display().to_string());
            cmd.env("UNDERTAKE_REPORTS_HOME", reports_home.display().to_string());
            let mut session = PtySession::spawn_command(cmd, ROWS, COLS);
            let rows = ROWS as usize;
            let cols = COLS as usize;

            // The opening screen: the pinned run, live, with the resolved
            // report and the roster-resolved attempt.
            let opening = wait_for_screen(&session, rows, cols, "the opening screen", |screen| {
                screen.contains("liveness: live") && screen.contains(first_update)
            });
            assert!(opening.contains(PATCHSTAND_RUN_ID), "{opening}");
            assert!(opening.contains("stage: implementing"), "{opening}");
            assert!(opening.contains("Harness Deck: "), "{opening}");
            assert!(!opening.contains("no report at"), "{opening}");

            // A local refresh: a concurrent writer advances `updated_at`,
            // and the screen follows it. Only a re-read of the manifest can
            // change this field — a redraw of the retained snapshot cannot.
            temp.write_run(
                PATCHSTAND_RUN_ID,
                &temp.patchstand_manifest(
                    PATCHSTAND_RUN_ID,
                    created,
                    second_update,
                    std::process::id(),
                    std::process::id(),
                ),
            );
            let after_write = tree_fingerprint(temp.root());
            assert_ne!(
                before, after_write,
                "the fingerprint must notice the manifest rewrite, or it can never notice anything"
            );
            wait_for_screen(&session, rows, cols, "the refreshed manifest", |screen| {
                screen.contains(second_update)
            });

            // Navigate: `j` moves the attempt cursor, `l` opens that
            // attempt's bounded worker log.
            session.send_text("jl");
            let with_log = wait_for_screen(&session, rows, cols, "the opened log", |screen| {
                screen.contains("worker.stdout.log")
            });
            assert!(with_log.contains(PATCHSTAND_ATTEMPT_DIR), "{with_log}");

            // Help, then dismiss it.
            session.send_text("?");
            wait_for_screen(&session, rows, cols, "the help overlay", |screen| {
                screen.contains("read-only")
            });
            session.send_text("?");
            wait_for_screen(&session, rows, cols, "help dismissed", |screen| {
                !screen.contains("read-only") && screen.contains(PATCHSTAND_PROFILE_ID)
            });

            // Tab to Evidence and ask for it on demand. The reply may never
            // arrive (Afterfact is a real subprocess and may not exist on
            // this machine); what must hold is that the request never blocks
            // the input loop.
            session.send_text("\t\t");
            wait_for_screen(&session, rows, cols, "the Evidence panel", |screen| {
                screen.contains("cautionlight: deferred")
            });
            session.send_text("r");

            session.send_text("q");
            assert!(
                session.wait_for_exit(Duration::from_secs(15)),
                "the dashboard did not exit after q"
            );
            assert_eq!(
                session.child.wait().ok().map(|status| status.exit_code()),
                Some(0),
                "q must exit 0"
            );
            assert_terminal_restored(&session, &session.output_snapshot());

            assert_eq!(
                tree_fingerprint(temp.root()),
                after_write,
                "the dashboard mutated state: nothing under the state root may change"
            );
        }

        /// Task 4's exit-1 half: a terminal that cannot be set up. The child
        /// runs in its own session (no controlling terminal) with stdin on
        /// `/dev/null`, so crossterm can find no tty to put into raw mode.
        /// `python3` is the session-detaching spawner — the same dependency
        /// `crate::process`'s tests already take — because `std::process`
        /// offers no safe `setsid` and this crate forbids `unsafe_code`.
        #[test]
        fn dashboard_cli_without_a_controlling_terminal_exits_one() {
            use crate::dashboard::run_source::test_support::{PATCHSTAND_RUN_ID, TempState};

            let temp = TempState::new();
            temp.write_patchstand_run(
                PATCHSTAND_RUN_ID,
                "2026-07-25T18:39:20.469500+00:00",
                "2026-07-25T18:43:44.617226+00:00",
                "2026-07-25T18:43:44.617226+00:00",
                std::process::id(),
                std::process::id(),
            );
            let before = tree_fingerprint(temp.root());

            let script = "import subprocess, sys\n\
                 done = subprocess.run(sys.argv[1:], stdin=subprocess.DEVNULL,\n\
                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,\n\
                 timeout=60)\n\
                 sys.stderr.write(done.stderr.decode('utf-8', 'replace'))\n\
                 sys.exit(done.returncode)\n";
            let outcome = std::process::Command::new("python3")
                .arg("-c")
                .arg(script)
                .arg(release_binary())
                .args([
                    "dashboard",
                    "--run",
                    PATCHSTAND_RUN_ID,
                    "--config",
                    &format!("{}/undertake.toml", env!("CARGO_MANIFEST_DIR")),
                ])
                .env("UNDERTAKE_STATE_DIR", temp.root())
                .env("UNDERTAKE_REPORTS_HOME", temp.root().join("reports-home"))
                .output()
                .expect("spawn a session-detached dashboard");

            assert_eq!(
                outcome.status.code(),
                Some(1),
                "a terminal that cannot be set up must exit 1, stderr: {}",
                String::from_utf8_lossy(&outcome.stderr)
            );
            assert!(
                String::from_utf8_lossy(&outcome.stderr).contains("dashboard:"),
                "the failure must be reported, not silent"
            );
            assert_eq!(
                tree_fingerprint(temp.root()),
                before,
                "a failed launch must still mutate nothing"
            );
        }
    }
}

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use crossterm::event::{self, Event};

use self::state::{DashboardApp, KeyContext, RuntimeAction, intent_for_key};
use self::terminal::TerminalGuard;
use super::model::{DashboardSnapshot, LogTail, SourceState};
use super::render;
use super::run_source::{DashboardRunSource, LogSelector, RunSelection, RunSourceConfig};
use super::services::{
    AfterfactDashboardSource, AfterfactSnapshot, MusterrollDashboardSource, MusterrollSnapshot,
};
use crate::musterroll::CommandMusterrollClient;

/// Input-poll granularity: short enough that `q`, a tick, or the
/// SIGTERM/SIGHUP flag are all noticed promptly, long enough not to spin.
/// Local refresh and Musterroll cadence are independent, coarser intervals
/// layered on top of this poll (see [`state::DashboardApp::on_tick`]).
const INPUT_POLL_INTERVAL: StdDuration = StdDuration::from_millis(100);

/// One dispatched Afterfact request. `previous` is supplied by the caller
/// (not tracked inside the worker) because Afterfact evidence is per-run: on
/// a run switch the caller resets it to [`SourceState::never_read`] first,
/// so a failed read after switching degrades to `Absent`, never to a stale
/// value carried over from the run the user just navigated away from.
struct AfterfactRequest {
    generation: u64,
    run_dir: Option<std::path::PathBuf>,
    previous: SourceState<AfterfactSnapshot>,
}

struct AfterfactReply {
    generation: u64,
    state: SourceState<AfterfactSnapshot>,
}

/// Spawns the Musterroll worker thread. Musterroll is global, not per-run,
/// so — unlike Afterfact — tracking `previous` inside the worker's own loop
/// is correct: nothing about a run switch invalidates provider availability.
fn spawn_musterroll_worker() -> (SyncSender<()>, Receiver<SourceState<MusterrollSnapshot>>) {
    let (request_tx, request_rx) = mpsc::sync_channel::<()>(1);
    let (reply_tx, reply_rx) = mpsc::channel();
    thread::spawn(move || {
        let client = CommandMusterrollClient::new();
        let mut previous: Option<SourceState<MusterrollSnapshot>> = None;
        while request_rx.recv().is_ok() {
            let now = Utc::now();
            let state = MusterrollDashboardSource::read(&client, previous.as_ref(), now);
            previous = Some(state.clone());
            if reply_tx.send(state).is_err() {
                break;
            }
        }
    });
    (request_tx, reply_rx)
}

/// Spawns the Afterfact worker thread. Stateless per request — see
/// [`AfterfactRequest`] for why `previous` travels with the request instead
/// of living in the worker.
fn spawn_afterfact_worker() -> (SyncSender<AfterfactRequest>, Receiver<AfterfactReply>) {
    let (request_tx, request_rx) = mpsc::sync_channel::<AfterfactRequest>(1);
    let (reply_tx, reply_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(request) = request_rx.recv() {
            let now = Utc::now();
            // `worker_commits` stays empty, and that is Task 4's decision,
            // not an unfinished edge. Spec §131 allows commit correlation
            // against "a typed worker commit **when present**", and
            // `undertake/run@2` records no such commit: the only commit in a
            // work manifest is `details.state.before_head`, the repository
            // HEAD *before* the worker ran. Correlating on it would match
            // observations made prior to this run's work — including other
            // runs sitting at the same parent commit — which is strictly
            // worse than the cwd-prefix rule already in force. Passing it
            // here would be inventing a typed fact the manifest does not
            // carry. Correlation therefore stays cwd-prefix only, and
            // remains labeled heuristic.
            let state = AfterfactDashboardSource::read(
                None,
                request.run_dir.as_deref(),
                &[],
                Some(&request.previous),
                now,
            );
            if reply_tx
                .send(AfterfactReply {
                    generation: request.generation,
                    state,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (request_tx, reply_rx)
}

/// Renders the fixed-allowlist relative path a [`LogSelector`] names, for
/// display when the real read fails (the read path itself is never derived
/// from anything but this same fixed template — see
/// [`super::run_source::DashboardRunSource::read_log`]).
fn log_selector_display_path(selector: &LogSelector) -> String {
    match selector {
        LogSelector::WorkerStdout(dir) => format!("attempts/{dir}/worker.stdout.log"),
        LogSelector::WorkerStderr(dir) => format!("attempts/{dir}/worker.stderr.log"),
        LogSelector::VerifyStdout => "artifacts/verify/stdout.log".to_string(),
        LogSelector::VerifyStderr => "artifacts/verify/stderr.log".to_string(),
    }
}

/// Returns a copy of `state` with its `RunSnapshot.logs` replaced. Task 1
/// deliberately never speculatively fills `logs` on a refresh tick (an
/// on-demand read is cheaper and matches the `l` keybinding's cost model);
/// this is the runtime routing that on-demand read into the snapshot the
/// renderer sees, exactly as Task 1's report calls for. `Absent`/`Deferred`
/// have no value to attach logs to and pass through unchanged.
fn with_run_logs(
    state: SourceState<super::model::RunSnapshot>,
    logs: Vec<LogTail>,
) -> SourceState<super::model::RunSnapshot> {
    match state {
        SourceState::Fresh {
            mut value,
            last_ok,
            last_attempt,
            truncated,
        } => {
            value.logs = logs;
            SourceState::Fresh {
                value,
                last_ok,
                last_attempt,
                truncated,
            }
        }
        SourceState::Stale {
            mut value,
            last_ok,
            last_attempt,
            error,
            truncated,
        } => {
            value.logs = logs;
            SourceState::Stale {
                value,
                last_ok,
                last_attempt,
                error,
                truncated,
            }
        }
        other @ (SourceState::Absent { .. } | SourceState::Deferred { .. }) => other,
    }
}

/// Owns everything the event loop mutates across iterations: the local
/// reader, the current selection/snapshot, the pure UI/cadence state, and
/// the two service worker channels. Not one of Task 3's five named
/// interfaces (`DashboardApp` is [`state::DashboardApp`], the pure part) —
/// this is `run_dashboard`'s private loop bookkeeping.
struct LoopState {
    run_source: DashboardRunSource,
    selection: RunSelection,
    snapshot: DashboardSnapshot,
    app: DashboardApp,
    musterroll_tx: SyncSender<()>,
    afterfact_tx: SyncSender<AfterfactRequest>,
    /// The log the user opened with `l`/Enter, if any.
    ///
    /// Retained because [`DashboardRunSource::snapshot`] never fills
    /// `RunSnapshot.logs` — Task 1 reads tails on demand — so every refresh
    /// rebuilds the snapshot without it. Without this the log would vanish
    /// within one refresh interval while [`state::DashboardApp`] still
    /// believed it open, and the next `l` press would close an already-blank
    /// panel instead of reopening it.
    open_log: Option<LogSelector>,
    quit: bool,
}

impl LoopState {
    /// Builds the loop bookkeeping around an already-constructed reader and
    /// its first snapshot. Kept as a constructor rather than an inline
    /// struct literal so the event loop and the tests below agree on the
    /// initial state by construction.
    fn new(
        run_source: DashboardRunSource,
        selection: RunSelection,
        snapshot: DashboardSnapshot,
        app: DashboardApp,
        musterroll_tx: SyncSender<()>,
        afterfact_tx: SyncSender<AfterfactRequest>,
    ) -> Self {
        Self {
            run_source,
            selection,
            snapshot,
            app,
            musterroll_tx,
            afterfact_tx,
            open_log: None,
            quit: false,
        }
    }

    fn current_run_id(&self) -> Option<String> {
        self.snapshot
            .run
            .value()
            .map(|run| run.identity.run_id.clone())
    }

    fn key_context_parts(&self) -> (Vec<Option<String>>, Vec<String>) {
        let attempt_dirs = self
            .snapshot
            .run
            .value()
            .map(|run| {
                run.attempts
                    .iter()
                    .map(|attempt| attempt.attempt_dir.clone())
                    .collect()
            })
            .unwrap_or_default();
        let recent_run_ids = self
            .snapshot
            .recent
            .value()
            .map(|recent| recent.iter().map(|run| run.run_id.clone()).collect())
            .unwrap_or_default();
        (attempt_dirs, recent_run_ids)
    }

    fn set_run_logs(&mut self, logs: Vec<LogTail>) {
        let current = std::mem::replace(&mut self.snapshot.run, SourceState::never_read());
        self.snapshot.run = with_run_logs(current, logs);
    }

    fn refresh_local(&mut self, now: DateTime<Utc>) {
        self.snapshot = self
            .run_source
            .snapshot(Some(&self.snapshot), &self.selection, now);
        // The fresh snapshot carries no log tails at all, so an open log has
        // to be re-read against it (see [`Self::open_log`]).
        if let Some(selector) = self.open_log.take() {
            self.attach_log(&selector);
            self.open_log = Some(selector);
        }
    }

    fn dispatch_afterfact_refresh(&mut self) {
        let run_dir = self
            .current_run_id()
            .map(|id| self.run_source.config().runs_dir().join(id));
        let previous = (*self.snapshot.afterfact).clone();
        let _ = self.afterfact_tx.try_send(AfterfactRequest {
            generation: self.app.current_generation(),
            run_dir,
            previous,
        });
    }

    /// Opens a log and remembers it, so later refreshes keep showing it.
    fn open_log(&mut self, selector: LogSelector) {
        self.attach_log(&selector);
        self.open_log = Some(selector);
    }

    /// Closes the open log and stops re-attaching it.
    fn close_log(&mut self) {
        self.open_log = None;
        self.set_run_logs(Vec::new());
    }

    /// Performs one bounded on-demand log read and hangs the result on the
    /// current snapshot. A failed read is reported in place — the allowlisted
    /// path it tried plus the error — never as a silently empty panel.
    fn attach_log(&mut self, selector: &LogSelector) {
        let Some(run_id) = self.current_run_id() else {
            return;
        };
        let tail = match self.run_source.read_log(&run_id, selector) {
            Ok(tail) => tail,
            Err(error) => LogTail {
                path: log_selector_display_path(selector),
                text: format!("(log unavailable: {error})"),
                truncated: false,
            },
        };
        self.set_run_logs(vec![tail]);
    }

    fn select_run(&mut self, run_id: String, now: DateTime<Utc>) {
        self.selection = RunSelection::Explicit(run_id);
        // Afterfact evidence is per-run: the value we might otherwise carry
        // forward belongs to the run just navigated away from, and must not
        // linger mislabeled as current-run evidence (see `AfterfactRequest`).
        self.snapshot.afterfact = Arc::new(SourceState::never_read());
        // Same reasoning for the open log, and `DashboardApp::select_run`
        // resets its own `log_open` flag to match: an attempt directory
        // belongs to the run that produced it.
        self.open_log = None;
        self.refresh_local(now);
    }

    fn apply(&mut self, action: RuntimeAction) {
        let now = Utc::now();
        match action {
            RuntimeAction::Quit => self.quit = true,
            RuntimeAction::RefreshLocal => self.refresh_local(now),
            RuntimeAction::RefreshMusterroll => {
                let _ = self.musterroll_tx.try_send(());
            }
            RuntimeAction::RefreshAfterfact => self.dispatch_afterfact_refresh(),
            RuntimeAction::ReadLog(selector) => self.open_log(selector),
            RuntimeAction::CloseLog => self.close_log(),
            RuntimeAction::SelectRun(run_id) => self.select_run(run_id, now),
        }
    }
}

/// Runs the dashboard until `q`, Ctrl-C, SIGTERM, or SIGHUP. The main thread
/// owns terminal input and rendering and never blocks on an adapter: local
/// bounded reads are synchronous (bounded by construction — see
/// [`super::run_source`]), while Musterroll and Afterfact run on dedicated
/// worker threads reached only through bounded channels, at most one request
/// in flight each.
pub(crate) fn run_dashboard(config: RunSourceConfig, selection: RunSelection) -> io::Result<()> {
    // Must precede any worker or raw-mode entry: release builds use
    // `panic = "abort"`, so this is the only cleanup a panic gets.
    terminal::install_panic_hook();
    let shutdown = Arc::new(AtomicBool::new(false));
    terminal::install_shutdown_signal(&shutdown)?;

    let refresh_interval = config.refresh_interval;
    let run_source = DashboardRunSource::new(config);
    let initial_snapshot = run_source.snapshot(None, &selection, Utc::now());

    let (musterroll_tx, musterroll_rx) = spawn_musterroll_worker();
    let (afterfact_tx, afterfact_rx) = spawn_afterfact_worker();

    let mut guard = TerminalGuard::enter()?;

    let mut loop_state = LoopState::new(
        run_source,
        selection,
        initial_snapshot,
        DashboardApp::new(refresh_interval),
        musterroll_tx,
        afterfact_tx,
    );

    while !loop_state.quit && !shutdown.load(Ordering::SeqCst) {
        if event::poll(INPUT_POLL_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                if let Some(intent) = intent_for_key(key) {
                    let now = Utc::now();
                    let (attempt_dirs, recent_run_ids) = loop_state.key_context_parts();
                    let ctx = KeyContext {
                        attempt_dirs: &attempt_dirs,
                        recent_run_ids: &recent_run_ids,
                    };
                    for action in loop_state.app.on_key(intent, now, &ctx) {
                        loop_state.apply(action);
                    }
                }
            }
        }

        for action in loop_state.app.on_tick(Utc::now()) {
            loop_state.apply(action);
        }

        if let Ok(state) = musterroll_rx.try_recv() {
            loop_state.app.complete_musterroll();
            loop_state.snapshot.musterroll = Arc::new(state);
        }
        if let Ok(reply) = afterfact_rx.try_recv() {
            if loop_state.app.complete_afterfact(reply.generation) {
                loop_state.snapshot.afterfact = Arc::new(reply.state);
            }
        }

        let ui = loop_state.app.ui;
        let render_now = Utc::now();
        guard
            .terminal
            .draw(|frame| render::render(frame, &loop_state.snapshot, &ui, render_now))?;
    }

    Ok(())
}

/// Test-only subprocess entry point for the terminal-restoration PTY suite
/// ([`terminal::tests`]). Proving the panic hook survives a `panic = "abort"`
/// release build, or that SIGTERM/SIGHUP leave the terminal restored, needs
/// a real compiled process attached to a real pseudo-terminal — raw-mode
/// ioctls and signal delivery cannot be exercised in-process. Task 4 owns
/// the actual `undertake dashboard` CLI surface; this is not it and is
/// reachable only via the exact sentinel below, so it can never affect
/// normal usage. Returns `None` for every other invocation, letting `main`
/// fall through to the real CLI unchanged.
pub(crate) fn dashboard_pty_test_harness(args: &[String]) -> Option<std::process::ExitCode> {
    const SENTINEL: &str = "__dashboard_pty_test_harness";
    if args.first().map(String::as_str) != Some(SENTINEL) {
        return None;
    }

    let state_root = std::env::var("UNDERTAKE_PTY_TEST_STATE_ROOT")
        .expect("UNDERTAKE_PTY_TEST_STATE_ROOT must be set for the PTY test harness");
    let config = RunSourceConfig {
        state_root: std::path::PathBuf::from(&state_root),
        // This suite tests terminal behavior, not the report join; keeping
        // the reports home inside the scratch state root means it can never
        // stat anything under the real `$HOME`.
        reports_home: std::path::PathBuf::from(state_root).join("reports-home"),
        refresh_interval: StdDuration::from_millis(250),
    };

    if std::env::var_os("UNDERTAKE_PTY_TEST_INDUCE_PANIC").is_some() {
        // Mirrors `run_dashboard`'s own ordering (panic hook, then raw
        // mode) so the induced panic proves the real production sequence,
        // not a reimplementation of it.
        terminal::install_panic_hook();
        let _guard = match TerminalGuard::enter() {
            Ok(guard) => guard,
            Err(error) => {
                eprintln!("failed to enter terminal: {error}");
                return Some(std::process::ExitCode::FAILURE);
            }
        };
        panic!("induced test panic for terminal-restoration verification");
    }

    match run_dashboard(config, RunSelection::Newest) {
        Ok(()) => Some(std::process::ExitCode::SUCCESS),
        Err(error) => {
            eprintln!("dashboard error: {error}");
            Some(std::process::ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    //! Event-loop bookkeeping: the wiring that turns a pure
    //! [`state::RuntimeAction`] into a real bounded read. [`state`] covers
    //! the pure transitions and [`terminal`] covers restoration; this covers
    //! the seam between them, which is where the on-demand log tail lives.

    use std::sync::mpsc;

    use super::*;
    use crate::dashboard::run_source::test_support::{
        PATCHSTAND_ATTEMPT_DIR, PATCHSTAND_RUN_ID, PATCHSTAND_WORKER_STDOUT, TempState,
    };

    /// A `LoopState` over a real temporary state root. Both service
    /// receivers are dropped: this suite never dispatches a service request,
    /// and `apply` already ignores a `try_send` failure, so no worker thread
    /// and no subprocess is involved.
    fn loop_state(temp: &TempState) -> LoopState {
        let (musterroll_tx, _musterroll_rx) = mpsc::sync_channel(1);
        let (afterfact_tx, _afterfact_rx) = mpsc::sync_channel(1);
        let run_source = DashboardRunSource::new(temp.config());
        let selection = RunSelection::Newest;
        let snapshot = run_source.snapshot(None, &selection, Utc::now());
        LoopState::new(
            run_source,
            selection,
            snapshot,
            DashboardApp::new(StdDuration::from_secs(1)),
            musterroll_tx,
            afterfact_tx,
        )
    }

    fn patchstand_state() -> TempState {
        let temp = TempState::new();
        temp.write_patchstand_run(
            PATCHSTAND_RUN_ID,
            "2026-07-25T18:39:20.469500+00:00",
            "2026-07-25T18:43:44.617226+00:00",
            "2026-07-25T18:43:44.617226+00:00",
            std::process::id(),
            std::process::id(),
        );
        temp
    }

    fn open_log_paths(loop_state: &LoopState) -> Vec<String> {
        loop_state
            .snapshot
            .run
            .value()
            .map(|run| run.logs.iter().map(|tail| tail.path.clone()).collect())
            .unwrap_or_default()
    }

    fn worker_stdout() -> RuntimeAction {
        RuntimeAction::ReadLog(LogSelector::WorkerStdout(
            PATCHSTAND_ATTEMPT_DIR.to_string(),
        ))
    }

    #[test]
    fn reading_a_log_attaches_its_tail_to_the_rendered_snapshot() {
        let temp = patchstand_state();
        let mut loop_state = loop_state(&temp);
        loop_state.apply(worker_stdout());
        assert_eq!(open_log_paths(&loop_state), vec![PATCHSTAND_WORKER_STDOUT]);
    }

    /// `DashboardRunSource::snapshot` never fills `RunSnapshot.logs` — Task 1
    /// reads tails on demand — so a refresh tick rebuilds the snapshot
    /// without the open log. Left unhandled, the log vanishes within one
    /// refresh interval while `DashboardApp` still believes it is open, and
    /// the next `l` press *closes* an already-blank panel instead of
    /// reopening it. The loop must re-attach it.
    #[test]
    fn an_open_log_survives_a_local_refresh_tick() {
        let temp = patchstand_state();
        let mut loop_state = loop_state(&temp);
        loop_state.apply(worker_stdout());
        loop_state.apply(RuntimeAction::RefreshLocal);
        assert_eq!(
            open_log_paths(&loop_state),
            vec![PATCHSTAND_WORKER_STDOUT],
            "a refresh must not silently close the log the user opened"
        );
    }

    #[test]
    fn closing_a_log_keeps_it_closed_across_refreshes() {
        let temp = patchstand_state();
        let mut loop_state = loop_state(&temp);
        loop_state.apply(worker_stdout());
        loop_state.apply(RuntimeAction::CloseLog);
        assert!(open_log_paths(&loop_state).is_empty());
        loop_state.apply(RuntimeAction::RefreshLocal);
        assert!(
            open_log_paths(&loop_state).is_empty(),
            "a closed log must not be resurrected by the next refresh"
        );
    }

    /// `DashboardApp::select_run` resets its own `log_open` flag on a run
    /// switch, so the loop must drop the retained selector too. Otherwise
    /// the old run's attempt directory keeps being re-read against the new
    /// run on every tick.
    #[test]
    fn switching_runs_drops_the_open_log() {
        let temp = patchstand_state();
        let other = "run-work-20260724T100000.000000000-p1-000000";
        temp.write_run(
            other,
            &temp.work_manifest(other, "2026-07-24T10:00:00+00:00", "finished"),
        );
        let mut loop_state = loop_state(&temp);
        loop_state.apply(worker_stdout());
        loop_state.apply(RuntimeAction::SelectRun(other.to_string()));
        assert!(
            open_log_paths(&loop_state).is_empty(),
            "the previous run's log must not follow the selection to another run"
        );
        loop_state.apply(RuntimeAction::RefreshLocal);
        assert!(open_log_paths(&loop_state).is_empty());
    }

    /// An unreadable log is a display-only fact, never a silent blank: the
    /// panel shows the allowlisted path it tried and why it failed.
    #[test]
    fn an_unreadable_log_reports_the_failure_in_place() {
        let temp = patchstand_state();
        let mut loop_state = loop_state(&temp);
        loop_state.apply(RuntimeAction::ReadLog(LogSelector::WorkerStdout(
            "999-never-dispatched".to_string(),
        )));
        let logs: Vec<String> = loop_state
            .snapshot
            .run
            .value()
            .map(|run| run.logs.iter().map(|tail| tail.text.clone()).collect())
            .unwrap_or_default();
        assert_eq!(logs.len(), 1);
        assert!(
            logs[0].contains("log unavailable"),
            "expected an explicit failure line, got {:?}",
            logs[0]
        );
    }
}
