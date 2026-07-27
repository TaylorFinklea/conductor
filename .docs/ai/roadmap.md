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
- [ ] Cycle 1 COMPLETE (9 beads closed: m0a, m0b, m1a, m1b, m2a, m2b, prompt, bdro, rev1); `cargo test` passes 84 tests. Live ready queue (`bd ready`, 6 items): `undertake-m4a`/`undertake-m3a` (P1), `undertake-agy`/`undertake-m1c`/`undertake-m0c` (P2), `undertake-cov1` (P3). Routing fields are in bd metadata; every bead's Verify is its `verify_cmd`.

### Next
- [ ] M3 dry-run cycle has a human-verify tail (report renders on dashboard) — see `undertake-m3b` notes. `undertake-guildhall-dogfood` (lead, v1 integration proof) is now bd-blocked on `undertake-m3b` and carries its own human-verify tail (dry-run over 3+ real repos + dashboard spot-check; verify_cmd alone under-covers).

### Later
- [ ] M3 dry-run cycle → M4 dispatch+verify (m4a→m4b→m4c) → `undertake-review` → M5 triage backfill → M6 ratchet. `undertake-review` bumped P2→P1 and now GATES v1-done (user decision 2026-07-02, ADR in guildhall decisions.md); still bd-blocked on m4c + m4b.
- [ ] `undertake-cautionlight` set to deferred (self-labeled v1.5; not in the v1-done clause) — un-defer after undertake-m4c + cautionlight m3/m4/m6.
- [ ] Post-v1 spikes: bd swarm/gate/mol evaluation; hermes-voice notification channel; SSE response push

## Milestones

See the retained v1 specification's Milestones section (M0–M6); each has scope and Verify, and beads are the per-item backlog.

## Backlog

> Lives in beads (`bd ready`) once the repo is initialized — not in this file.

## Constraints

- Invariants in spec § Invariants are non-negotiable (closed roster, tier_floor gate, fail-closed, no push, no chezmoi, one writer per repo).
- Implementation is fleet-driven: Sonnet-5/GPT-5.5/minimax et al. own Senior/Junior beads; Opus/Fable own Lead beads. Mis-triaging down is the expensive error.
