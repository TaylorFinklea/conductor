# current-state.md — conductor

Branch: feat/omp-role-aware-routing

Strict Conductor v2 cutover COMPLETE (2026-07-22): active runs now use only
`bursar/roster@2`, `conductor/run@2`, and `conductor/event@2`; each prepared
run owns the exact copied roster snapshot and its validated policy digest.
Arena has no active source, configuration, CLI, ledger, or parser surface.
`runs-v2/` is the sole active namespace; activation preflight blocks only
actionable v1 recovery and leaves finished v1 history inert. Work/review/
consult retain their behavior through structural v2 adapters; `Plan` has
typed state, targets, constrained routes, and transition invariants but no
model execution or generic scheduler.

Verified: `cargo test run`, `cargo test quarantine`, `cargo test bursar`,
`cargo test`, and `cargo clippy --all-targets -- -D warnings`. The no-model
binary smoke passed `config check` with Bursar v2 and an isolated clean state
directory; the real state directory correctly reported its actionable v1
preflight block.

## Plan

## Blockers

## Open questions
