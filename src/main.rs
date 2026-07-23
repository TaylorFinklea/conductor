//! Conductor — fleet cycles plus approval-gated, read-only adversarial design review.

mod adversarial;
mod bd;
mod bursar;
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
mod quarantine;
mod ratchet;
mod role_routing;
mod roster_drift;
mod route;
mod run;
mod scan;
mod state;
mod triage;
mod verify;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    cli::run(args)
}
