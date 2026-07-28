# Roadmap

> Durable goals and milestones. Updated when scope changes, not every session.

## Vision

Undertake: a single Rust binary that runs autonomous work-routing cycles over the ~24 beads-tracked repos under `~/git` — scan → triage → plan → approval → dispatch → verify → report — composing bd, pi/agy/claude, orchestra, and harness-deck over subprocess/file contracts. The retained v1 specification is historical.

## Now / Next / Later

### Now
- [x] Provider-trust integration + bounded approvals + adversarial design
  review — COMPLETE (2026-07-15). Provider state fails closed, runtime 429
  observations precede approved fallback, approval cannot exceed its persisted
  repository/item scope, and isolated adversarial review runs `N` distinct-
  provider reviewers plus one Lead synthesis judge. Specs:
  `phases/provider-trust-integration-spec.md`,
  `phases/bounded-dispatch-approval-spec.md`, and
  `phases/adversarial-design-review-spec.md`. **Landmine:** adversarial review
  performs no bd/git/worktree/apply mutation and must not share normal-cycle
  dispatch semantics. The later Undertake-core consolidation and migration to
  a `review` job are not implemented here.
- [x] Strict role-routing v2 cutover — COMPLETE (2026-07-23). Musterroll owns
  profile identity and unordered role capabilities through `musterroll/roster@2`;
  every active `undertake/run@2` pins its exact snapshot. Undertake owns
  durable role-lane scheduling, plan author/peer/second-opinion execution,
  generic ledger/event evidence, and fail-closed activation preflight.
  `runs-v2/` is isolated from inert legacy history. Arena is removed.
  Canonical contract: Guildhall `undertake-core-consolidation-spec.md`.
- [x] The prior suite rebrand and managed-source cutover are complete; their exact migration evidence remains in dated records and git history.
- [x] Reliability, containment, and reviewer-trust hardening — COMPLETE
  (2026-07-27). A three-family adversarial audit (Opus 5, Ollama Cloud GLM 5.2,
  MiniMax M3) was adjudicated against source, then closed as beads `zzw`
  (bounded helper subprocesses), `b41` (cycle/recovery deadlines), `ptj` (worker
  resource + deny-default write containment), `u9t` (durable state growth and
  fsync), `pux` (declared verification inputs), `0kc` (`undertake supersede`),
  `5p8` (pre-claim backend auth classification), and `z8z`/`zg9`/`5tg`/`koi`/`0ya`
  (reviewer trust boundary). **Landmines:** Darwin `RLIMIT_AS` bounds virtual
  address space, not RSS; `claude auth status` hangs past 300s non-interactively,
  so every backend probe must stay bounded and stdin-closed; undeclared ignored
  verification inputs now fail closed and need a `[[verification_input]]`
  `materialize` or `acknowledge` declaration.
- [ ] **v1 finish — the kernel cutover.** Spec: `phases/undertake-v1-finish-spec.md`
  (2026-07-28). Diagnosis: there is no kernel. There are four independent engines
  (`dispatch_cycle` 19,370 / `plan_job` 5,995 / `adversarial` 5,110 / a dead 989-line
  `loop.rs` prototype) and no `consult` at all. Draft v1 of the plan assumed `loop.rs`
  was a finished kernel needing wiring; adversarial review (GPT-5.6 Sol, REJECT) proved
  otherwise and the direction was inverted — see the three `[2026-07-28]` ADRs.
  **Landmines:** `loop.rs` requires an authenticated direct-child commit
  (`loop.rs:346-359`), so read-only jobs can never pass it; `RunHandle::create` refuses
  Plan runs (`run.rs:1021`); `job.rs`'s registry is never *constructed* because the TOML
  spelling is `[[job]]` and `undertake.toml` has none; the dashboard has a **production**
  dependency on `dispatch_cycle` (`dashboard/mod.rs:77`); `-D dead_code` must stay off
  until Phase 6 or rollback gets harder.

### Next
- [ ] Chain: `pu5` + `y6kv` → `mkct` → `vd3y` → `bxb` → `eueb`/`utwq`/`ed12` → `sq4a` →
  `qtfu` → `bnc`. Gate order is load-bearing: freeze a parity corpus before retiring
  anything, break the all-Unknown bootstrap deadlock (`bxb`, the only P0) before
  migrating jobs that would otherwise be undogfoodable, pass cutover gate 10 **before**
  deleting the rollback engine, and quiesce every pending/implementing/reclaimable legacy
  run first.
- [ ] Human tails: apply the chezmoi `AGENTS.md` + guildhall-orchestration migration
  before `cycle`/`dispatch` are deleted (Phase 6 prerequisite; never `chezmoi apply`
  headless). Redefine `undertake-guildhall-dogfood` in kernel terms — its 2026-07-27
  evidence (251 proposed / 0 dispatched) is at least partly the `bxb` deadlock, not a
  clean propose-only result.

### Later
- [ ] Cross-repo: Musterroll adds `model_family` to `roster@2` so `ao8` review-panel
  diversity can be enforced by developer lineage rather than provider lane
  (ADR `[2026-07-28]`).
- [ ] Post-v1: the 11 deferred beads (scorecard-complete evidence `7hb`, Gauntlet corpus
  fold, Managed Agents POC, native Codex app-server client, local Ollama admission,
  quota-aware load spreading, legacy ledger retirement, gate-11 Fable panel `pzo`,
  mixed-version lease fencing, fs4 migration, Cautionlight policy).
- [ ] Post-v1 spikes: bd swarm/gate/mol evaluation; hermes-voice notification channel; SSE response push

## Milestones

See the retained v1 specification's Milestones section (M0–M6); each has scope and Verify, and beads are the per-item backlog.

## Backlog

> Lives in beads (`bd ready`) once the repo is initialized — not in this file.

## Constraints

- Invariants in spec § Invariants are non-negotiable (closed roster, tier_floor gate, fail-closed, no push, no chezmoi, one writer per repo).
- Implementation is fleet-driven: Sonnet-5/GPT-5.5/minimax et al. own Senior/Junior beads; Opus/Fable own Lead beads. Mis-triaging down is the expensive error.
