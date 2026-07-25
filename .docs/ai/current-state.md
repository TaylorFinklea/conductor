# current-state.md — undertake

Branch: `main` — fast-forwarded from `feat/four-tool-clean-rename` (`c9d3ab6`); pushed, in sync with `origin/main`. Rename cutover complete; `conductor-043` cross-repo gate fixed at this commit and closed 2026-07-25 (guildhall `decisions.md` [2026-07-25]).

Strict Undertake v2 cutover COMPLETE (2026-07-22): active runs now use only
`musterroll/roster@2`, `undertake/run@2`, and `undertake/event@2`; each prepared
run owns the exact copied roster snapshot and its validated policy digest.
Arena has no active source, configuration, CLI, ledger, or parser surface.
`runs-v2/` is the sole active namespace; activation preflight blocks only
actionable v1 recovery and leaves finished v1 history inert. Work/review/
consult retain their behavior through structural v2 adapters. `Plan` now has a
generic Undertake-owned role-policy scheduler: strict enabled bindings pin
opaque Musterroll profiles, durable smooth weighted lanes use the policy/role/stage
key, and reservations, delayed reviewer routes, capacity, reset, orphan, and
per-run transition guards persist under atomic fs2-protected state. The initial
`plan` pool is Sol/Opus/Kimi at 60/20/20. No plan model invocation or plan CLI
activation exists yet.

Verified on this branch: `cargo test role_routing`, exact scheduler/reservation
gates, full `cargo test`, and `cargo clippy --all-targets -- -D warnings`.
The real-binary no-model `config check` reached strict v2 Musterroll parsing with
the local v2 Musterroll binary; its final preflight correctly remained blocked by
missing `ralph`/`orchestra` executables and an actionable legacy implementing
run in the user's state root.

Task 3 clean source/contract cutover COMPLETE (2026-07-24): package and CLI
are `undertake`, configuration is `undertake.toml`, operational roots and
schemas are Undertake-owned, and the strict subprocess client consumes only
Musterroll status/roster/observation contracts. The explicit
`undertake migrate state` transaction is copy-based and quiescence-gated; it
carries journal, ratchet, current plans, terminal `runs-v2`, and the current
scheduler lanes/reservations into a destination that must not exist. It
preserves scheduler scores/history, verifies the source hash remains unchanged,
leaves archived legacy `runs/` behind, and never enables a dual read.

Verified on the clean-rename branch: focused RED, full `cargo test`, strict
Clippy, release build, Undertake help/version, isolated no-model configuration
activation against release Musterroll with bounded fake allowances, and the
migration/reconciliation/capacity filters.

## Plan

## Blockers

## Open questions
