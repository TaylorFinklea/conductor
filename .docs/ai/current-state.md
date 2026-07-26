# current-state.md — undertake

Branch: `feat/undertake-dashboard` (worktree `.worktrees/undertake-dashboard`), off `main` at `c9d3ab6`; not pushed. `main` is in sync with `origin/main`; rename cutover complete and `conductor-043` closed 2026-07-25 (guildhall `decisions.md` [2026-07-25]).

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

Undertake dashboard SHIPPED (2026-07-25): `undertake dashboard [--run <run-id>]
[--refresh-ms <250..60000>] [--config <path>]`, behind the default-on `tui`
feature; a `--no-default-features` build keeps the non-TUI CLI and rejects the
subcommand. Read-only by construction: no lease, heartbeat, `RunHandle`, or
service write is reachable from the command. Live acceptance against the real
state root, pinned to `run-work-20260725T183920.469500000-p45813-000000`:
22/22 checks — work job, `abandoned` liveness against a `running` lifecycle,
`implementing` stage, `openai-codex--codex--gpt-5.6-luna--high` resolved
through the run-local roster, the canonical `pnpm check` failure, on-demand
Afterfact 0 correlated / 233 uncorrelated with its coverage-gap summary,
deferred Cautionlight, and the Harness Deck report joined through
`details.state.cycle_id`; `q` exits 0, the terminal is restored, and the
state root, report directory, and Patchstand repo are byte-identical after.
Stripped release binary 1.55 MiB → 1.79 MiB (+245 KiB, +15.5%).

That pilot's underlying defect is still open: bd P0 `conductor-pux`
(verification environment parity across isolated promotion). The dashboard
makes the failure legible; it does not fix it.

V1 ships read-only intents only. The later OMP-powered action phase
(approve/dispatch/cancel/resume/retry/routing) must reach them through a
separate authorized executor calling public CLIs — never by moving mutation
authority into the readers or the renderer.

## Plan

- [ ] Orchestrator project verification for the dashboard branch.
      Verify: `cargo test && cargo clippy --all-targets --all-features -- -D warnings && cargo check --no-default-features && cargo build --release`

## Blockers

## Open questions
