//! Undertake — fleet cycles plus approval-gated, read-only adversarial design review.

mod adversarial;
mod bd;
mod musterroll;
mod cli;
mod config;
mod cycle;
mod deck;
mod dispatch;
mod dispatch_cycle;
mod fields;
mod job;
mod ledger;
mod r#loop;
mod plan;
mod plan_job;
mod process;
mod quarantine;
mod ratchet;
mod role_routing;
mod route;
mod run;
mod sanitize;
mod scan;
mod state;
mod triage;
mod verify;
#[cfg(feature = "tui")]
mod dashboard;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Test-only subprocess entry point for the dashboard's terminal-
    // restoration PTY suite (`dashboard::runtime::terminal::tests`); see
    // `dashboard_pty_test_harness`'s doc comment. `None` for every normal
    // invocation, so this never affects the real CLI.
    #[cfg(feature = "tui")]
    if let Some(code) = dashboard::runtime::dashboard_pty_test_harness(&args) {
        return code;
    }
    cli::run(args)
}
