# current-state.md — conductor

Branch: feat/omp-role-aware-routing

Strict Conductor v2 cutover COMPLETE (2026-07-22): active runs now use only
`bursar/roster@2`, `conductor/run@2`, and `conductor/event@2`; each prepared
run owns the exact copied roster snapshot and its validated policy digest.
Arena has no active source, configuration, CLI, ledger, or parser surface.
`runs-v2/` is the sole active namespace; activation preflight blocks only
actionable v1 recovery and leaves finished v1 history inert. Work/review/
consult retain their behavior through structural v2 adapters. `Plan` now has a
generic Conductor-owned role-policy scheduler: strict enabled bindings pin
opaque Bursar profiles, durable smooth weighted lanes use the policy/role/stage
key, and reservations, delayed reviewer routes, capacity, reset, orphan, and
per-run transition guards persist under atomic fs2-protected state. The initial
`plan` pool is Sol/Opus/Kimi at 60/20/20. No plan model invocation or plan CLI
activation exists yet.

Verified on this branch: `cargo test role_routing`, exact scheduler/reservation
gates, full `cargo test`, and `cargo clippy --all-targets -- -D warnings`.
The real-binary no-model `config check` reached strict v2 Bursar parsing with
the local v2 Bursar binary; its final preflight correctly remained blocked by
missing `ralph`/`orchestra` executables and an actionable legacy implementing
run in the user's state root.

## Plan

## Blockers

## Open questions
