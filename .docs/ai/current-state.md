# current-state.md — undertake

Branch: `main` at `2ebfb26`; 48 commits ahead of `origin/main`, not pushed.

## Plan

v1 execution lives in beads. Specs: `phases/undertake-v1-finish-spec.md` (plan),
`phases/undertake-runner-contract.md` (Phase 1a deliverable, `y6kv` CLOSED).

Chain: prep ✓ → `mkct` ✓ → `v37z` ✓ → `vd3y` → `bxb` → `eueb`/`utwq`/`ed12` → `sq4a` → `qtfu` → `bnc`.

- [x] `conductor-8nth` — prep 1 done (`ca03137`); `event@3`, generic `InvocationEvidence`, real `@2` fixture resumes
- [x] `conductor-44hc` — prep 2 done (`1078a1f`); event-then-manifest everywhere, all four `RunDetails` reconciled
- [x] `conductor-0yxz` — prep 3 done (`48a21b9`); `run_readonly` takes hooks, returns `DispatchResult`
- [x] `conductor-q6b6` — prep 5 done (`0af8558`); `role_routing` has no job-name or plan-manifest knowledge
- [x] `conductor-mkct` — runner extracted (`c3488fa` types+ports, `befdfb6` loop); sole-writer verified, open questions 2 and 3 settled
- [x] `conductor-v37z` — resume reconstructs the `StageLedger` from a new `event@3` `StageFinished` record (extends `EventKind`) instead of restarting it empty; fail-closed on an uninterpretable stage record; `@2`/pre-fix `@3` journals still open (empty ledger, unchanged)
- [x] `conductor-gtgf` — prep 4 done (`bf44828`); `worker_slots` set, any-alive-or-inconclusive refuses reclaim
- [x] `conductor-pu5` — Phase 0 done (`f5e1c0a`); parity selector = `cargo test --bin undertake dispatch_cycle::tests::` = 123 tests
- [x] `conductor-jum` — done (`9c90c5c`); wildmatch escaping, 4 real-git tests
- [x] `conductor-moe` — done (`b88da79`); `release_owned` fails closed, no bd CAS exists
- [x] `conductor-47p` — done (`5224787`); owners bind to (pid, process generation)

## Blockers

- None. **All five prep beads, `conductor-mkct`, and `conductor-v37z` are done.**
- Next: `conductor-vd3y` (per the contract doc's `RunRequest` doc comment: populate `pinned_digests` from a run's pinned manifest/approval envelope — the generic loop takes it as an input and does not own sourcing it).
- Suite green at 917 passed / 0 failed / 3 ignored; clippy clean. Parity corpus 126, no loss.

## Open questions (updated)

- Musterroll must add `model_family` to `roster@2` before `ao8` can be enforced (cross-repo).
- Phase 6 needs the chezmoi `AGENTS.md` + guildhall-orchestration migration applied by a human.
- CASE is unfrozen/unimplemented, so containment is unowned today — see `decisions.md [2026-07-28]`.
