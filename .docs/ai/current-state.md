# current-state.md — undertake

Branch: `main` at `d911c38`; 13 commits ahead of `origin/main`, not pushed.

## Plan

v1 execution lives in beads, not here. Spec: `phases/undertake-v1-finish-spec.md`.
Chain: `pu5`+`y6kv` → `mkct` → `vd3y` → `bxb` → `eueb`/`utwq`/`ed12` → `sq4a` → `qtfu` → `bnc`.

- [ ] `conductor-y6kv` — Phase 1a runner contract (lead/L; gates everything after it)
- [ ] `conductor-pu5` — Phase 0 parity corpus + delete roster_drift (senior/S; parallel)
- [ ] Three promoted correctness bugs, unblocked now: `47p`, `moe`, `jum`
- [ ] `conductor-1qj` — re-verify premise first; `cargo test` is green at `ed5b638`

## Blockers

- None. `y6kv` and `pu5` are ready.

## Open questions

- Musterroll must add `model_family` to `roster@2` before `ao8` diversity can be enforced (cross-repo).
- Phase 6 needs the chezmoi `AGENTS.md` + guildhall-orchestration migration applied by a human before `cycle`/`dispatch` are deleted.
