//! Read-only Undertake dashboard snapshot model and bounded run-source readers.
//!
//! The dashboard lives behind a default-on `tui` feature so a
//! `--no-default-features` build keeps the existing non-TUI CLI. This module
//! owns only the immutable snapshot the renderer consumes and the bounded,
//! forward-compatible artifact readers that produce it; it never writes run,
//! service, or repository state and never receives a mutable `RunHandle`.
//!
//! Task 1 establishes the snapshot interfaces (`DashboardSnapshot`,
//! `RunSnapshot`, `RunLiveness`, `SourceState<T>`, `DashboardRunSource`,
//! `RunSelection`, and `RunSourceConfig`) and the bounded run discovery,
//! liveness, events, attempts, verification, and log-tail readers. Task 2
//! adds the bounded service adapters. Task 3 adds the pure renderer
//! (`render`), the read-only intent/state layer and terminal-safe event
//! loop (`runtime`), and the crate's only mutation-free dashboard entry
//! point, `run_dashboard`. Task 4 wires the `undertake dashboard` CLI
//! command to it.

// `run_dashboard` (this task) is reachable from `main.rs` via the PTY
// test-harness dispatch, so Task 3's own code is no longer dead by itself —
// but plenty of Task 1/2 surface still is: `DashboardRunSource::select`/
// `recent_runs` (the runtime drives everything through `snapshot()`
// instead, per Task 1's own concern about the discovery warning living only
// there), three of the four `LogSelector` variants (only `WorkerStdout` is
// wired to a key), and the whole Cautionlight adapter (deliberately never
// invoked — see `runtime`'s module doc). These are real, intentional gaps
// for *this* task, not bugs; Task 4's CLI wiring is expected to close some
// of them. Lint levels are lexically scoped, so this one allow covers the
// whole subtree.
#![allow(dead_code)]

pub(crate) mod model;
pub(crate) mod render;
pub(crate) mod run_source;
pub(crate) mod runtime;
pub(crate) mod sanitize;
pub(crate) mod services;

// Re-exported for later tasks: the renderer (Task 3), service adapters
// (Task 2), and CLI wiring (Task 4) consume these; nothing in-tree uses the
// full set yet.
#[allow(unused_imports)]
pub(crate) use model::{
    AttemptRecord, DashboardSnapshot, LogTail, RecentRun, RunIdentity, RunLiveness, RunSnapshot,
    SourceState, StageMarker, VerificationRecord, VerificationSource,
};
#[allow(unused_imports)]
pub(crate) use run_source::{
    DashboardError, DashboardRunSource, LogSelector, RunSelection, RunSourceConfig,
};
// Bounded subprocess execution lives at the crate root, not under this
// `tui`-gated module: `CommandMusterrollClient` needs it in a
// `--no-default-features` build.
#[allow(unused_imports)]
pub(crate) use crate::process::{BoundedCommand, CommandOutcome};
#[allow(unused_imports)]
pub(crate) use render::{Panel, UiState};
#[allow(unused_imports)]
pub(crate) use runtime::run_dashboard;
#[allow(unused_imports)]
pub(crate) use runtime::state::{DashboardApp, DashboardIntent};
#[allow(unused_imports)]
pub(crate) use runtime::terminal::TerminalGuard;
#[allow(unused_imports)]
pub(crate) use services::{
    AfterfactDashboardSource, AfterfactSnapshot, CautionlightDashboardSource, CautionlightSnapshot,
    MusterrollDashboardSource, MusterrollSnapshot, ServiceSnapshot,
};

/// The stale-claim threshold, taken from operational recovery rather than
/// redeclared: a heartbeat quiet for longer than this is no longer fresh
/// evidence of liveness. Binding to
/// [`crate::dispatch_cycle::STALE_CLAIM_THRESHOLD`] keeps the dashboard's
/// `live`/`abandoned` split identical to the one `dispatch --resume` uses to
/// decide a run is reclaimable.
pub(crate) const STALE_HEARTBEAT_THRESHOLD: chrono::Duration =
    crate::dispatch_cycle::STALE_CLAIM_THRESHOLD;

#[cfg(test)]
mod tests {
    //! Feature-gate and module-level invariants. Per-task tests live alongside
    //! the implementation they cover (`model::tests`, and `run_source`'s
    //! per-step modules `discovery`, `liveness`, `events`, `attempts`,
    //! `verification`, `logs`).

    /// The `tui` feature must be declared default-on in `Cargo.toml` so
    /// `undertake dashboard` is compiled in a normal build, while a
    /// `--no-default-features` build retains the non-TUI CLI without the
    /// dashboard runtime dependencies. Reads the manifest directly rather
    /// than asserting `cfg!(feature = "tui")` — that would be tautological
    /// here, since this whole module only compiles when the feature is
    /// already on.
    #[test]
    fn dashboard_feature_gate_tui_is_default_on() {
        let manifest = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        let features_section = manifest
            .split("[features]")
            .nth(1)
            .expect("Cargo.toml has a [features] section")
            .split("[dependencies]")
            .next()
            .expect("[features] section is followed by [dependencies]");
        assert!(
            features_section.contains(r#"default = ["tui"]"#),
            "Cargo.toml [features] must declare `default = [\"tui\"]`"
        );
        assert!(
            features_section
                .contains(r#"tui = ["dep:ratatui", "dep:crossterm", "dep:signal-hook"]"#),
            "Cargo.toml [features] must declare `tui = [\"dep:ratatui\", \"dep:crossterm\", \"dep:signal-hook\"]`"
        );
    }

    /// A no-default-features build must not pull the dashboard runtime
    /// dependencies. This is checked at compile time by the module gate:
    /// `mod dashboard` is compiled only with `tui`, so a
    /// `--no-default-features` build never links ratatui/crossterm/
    /// signal-hook and never compiles this test module at all. `cargo check
    /// --no-default-features` succeeding without those dependencies is the
    /// direct runtime proof; this test additionally confirms all three
    /// dependencies are marked `optional = true` in the manifest, the
    /// mechanism that makes the gate possible.
    #[test]
    fn dashboard_feature_gate_no_default_features_omits_runtime_deps() {
        let manifest = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        for dep_line_prefix in ["ratatui = {", "crossterm = {", "signal-hook = {"] {
            let line = manifest
                .lines()
                .find(|line| line.starts_with(dep_line_prefix))
                .unwrap_or_else(|| {
                    panic!("Cargo.toml must declare a `{dep_line_prefix}` dependency line")
                });
            assert!(
                line.contains("optional = true"),
                "{dep_line_prefix} dependency must be optional so --no-default-features omits it, got: {line}"
            );
        }
    }
}
