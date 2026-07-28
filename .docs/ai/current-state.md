# current-state.md — undertake

Branch: `main` at `b88da79`; 26 commits ahead of `origin/main`, not pushed.

## Plan

v1 execution lives in beads. Specs: `phases/undertake-v1-finish-spec.md` (plan),
`phases/undertake-runner-contract.md` (Phase 1a deliverable, `y6kv` CLOSED).

Chain: `pu5` + four prep beads → `mkct` → `vd3y` → `bxb` → `eueb`/`utwq`/`ed12` → `sq4a` → `qtfu` → `bnc`.

- [ ] `conductor-8nth` — prep 1: `undertake/event@3` (senior/L) — ready
- [ ] `conductor-44hc` — prep 2: terminal reconciliation + write order (senior/M) — ready
- [ ] `conductor-0yxz` — prep 3: read-only spawn identity (senior/M) — ready
- [ ] `conductor-q6b6` — prep 5: decouple `role_routing` from plan (senior/M) — ready
- [ ] `conductor-gtgf` — prep 4: per-slot identity — blocked on `47p`
- [x] `conductor-pu5` — Phase 0 done (`f5e1c0a`); parity selector = `cargo test --bin undertake dispatch_cycle::tests::` = 123 tests
- [x] `conductor-jum` — done (`9c90c5c`); wildmatch escaping, 4 real-git tests
- [x] `conductor-moe` — done (`b88da79`); `release_owned` fails closed, no bd CAS exists
- [ ] `conductor-47p` — last promoted correctness bug; blocks prep 4 (`gtgf`)

## Blockers

- None. Four prep beads plus Phase 0 are ready in parallel.

## Open questions

- Musterroll must add `model_family` to `roster@2` before `ao8` can be enforced (cross-repo).
- Phase 6 needs the chezmoi `AGENTS.md` + guildhall-orchestration migration applied by a human.
- CASE is unfrozen/unimplemented, so containment is unowned today — see `decisions.md [2026-07-28]`.
