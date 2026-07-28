# Undertake v1 finish — spec

**Status**: draft, under adversarial review (Opus 5, 2026-07-27).
Implements cutover gates 4 and 10 of `guildhall/.docs/ai/phases/undertake-core-consolidation-spec.md`.
**Owner**: Opus (Lead) specs and adjudicates. Senior implements per phase.

## The diagnosis

The approved architecture is *one kernel, four jobs, explicit targets*. The kernel
exists and is tested. **It has never been turned on.**

| Piece | Lines | State |
|---|---|---|
| `src/loop.rs` — native kernel | 989 | Complete state machine, 14 tests. Zero production adapters, zero CLI wiring. |
| `src/job.rs` — closed four-job registry | 431 | Validates exactly `work\|review\|consult\|plan`. `config.rs:1568` builds it to validate, then keeps only the binding vec — **its authority is never used at runtime**. |
| `Config.jobs` (`[[jobs]]` in `undertake.toml`) | — | Parsed and validated on every `config::load`, then discarded — zero readers outside `config.rs`. |
| `src/dispatch_cycle.rs` — legacy fleet engine | 19,370 | The **only** live end-to-end mutating path. |
| `plan` (`src/plan_job.rs`) | 5,995 | Live, wired, its own engine. |
| `review` (`src/adversarial.rs`) | 5,110 | Live as `adversarial-review`. Tags its run `RunJob::Review` but is not a job. |
| `consult` | 0 | Does not exist. `cycle.rs:268` writes a `RunJob::Consult` record purely as a dashboard breadcrumb. |

`LoopHarness` (`loop.rs:126`) and `LoopClaim` (`loop.rs:132`) have **zero production
implementors**. The kernel is a socket with nothing plugged in.

The "spawn → wait → classify → retry or fall back → record terminal → close or release"
arc is hand-rolled **four times**: `dispatch_cycle` (`run_worker_chain:5823`), `loop`
(`LoopKernel::run:257`, dead), `plan_job` (`dispatch:1549`), `adversarial`
(`run_reviewers:973`).

### Why it went unnoticed

**22 modules carry a blanket `#![allow(dead_code)]`** — reachability lint is globally
off. `roster_drift.rs` (1,017 lines) is retired at `cli.rs:1764` (prints "roster drift is
retired", exits 2, never calls the module) and still compiles clean. `ratchet.rs` (1,101
lines) is disconnected: `cycle.rs:194` passes `HashMap::new()` where ratchet state
belongs, so `autonomy = "propose"` and the entire `[ratchet]` table accept operator input
with **zero runtime effect**. Not one warning.

Restoring that lint is not hygiene. It is the instrument that makes Phase 4 safe.

## Locked decisions (user, 2026-07-27)

| Decision | Value |
|---|---|
| v1 target | Wire the kernel. All four jobs through one `LoopKernel`. |
| Dashboard | **In v1.** User-requested and in use. Same verification bar as the kernel. |
| Legacy fleet surface | **Deleted as part of v1** — gated on demonstrated parity (Phase 4). |

## Decision D1 — `work` runs in the repository, not in an attempt checkout

**This is the highest-leverage call in the plan.** It decides how much of
`dispatch_cycle` is a requirement versus an artifact.

`dispatch_cycle` runs each worker in an isolated `AttemptCheckout`
(`dispatch_cycle.rs:5961`) and then promotes the commit
(`promote_attempt_commit:1775`). That single design choice is the root of:

- verification-input materialization (`materialize_declared_verification_inputs:1285`) —
  needed *only* because a fresh checkout lacks gitignored files the verifier requires
- `undertake supersede` (1,492 lines, `de954c8`) — terminalizing failed **promoted** runs
- promotion recovery records (beads `1ls`, `8hz`)
- rollback survivor paths (bead `jum`)
- three of the four resume state machines (`resume_promoted_work:4492`,
  `resume_unauthenticated_implementing_work:5172`, `resume_finished_promoted_work:3986`)

**Decision: v1 `work` writes the target repository directly.**

Grounds: the consolidation spec states the native loop "preserves Ralph's earned
behavior"; Ralph works in-repo. Its `work` row reads "Repo writes allowed inside approved
scope." Its loop requirement 7 is "worker identity plus exclusive repo lease" — a lease
and an identity check, **not** worktree isolation. `loop.rs` is already built this shape.

Consequence: the promotion subsystem and everything above is **not ported**. It is
deleted. Four deferred beads and one two-day-old subsystem stop being requirements.

Cost, stated plainly: a failed worker can leave the working tree dirty, and **the kernel
does not currently guard against it** — `loop.rs` reads `head` before spawning but never
`is_clean`, and it never calls `quarantine`. The mitigations exist but are unwired:
`quarantine.rs` dirty-tree capture, `RepoLease` (already acquired at `loop.rs:274`), and
the identity check that stops an unrelated commit counting as success. Phase 1 must add
the `is_clean` preflight and quarantine adoption, or D1 ships a real safety regression
against `dispatch_cycle`, which preflights both.

A second downgrade to accept: on retry, `dispatch_cycle` hands the next attempt a
`prior_capture` with patch path and hash; the kernel hands it plain text in
`LoopIteration.feedback`. Weaker, acceptable for v1, recorded here so it is a choice.

**If a reviewer can show in-repo execution is unsafe for this operator, D1 flips and
Phase 1 roughly doubles.** It is called out as a decision, not assumed.

## What the kernel actually lacks

My first draft called Phase 1 "adapters over existing primitives." That was wrong.
Verified against source:

| Capability | Legacy location | v1 call |
|---|---|---|
| **Claim a bead** | `dispatch_cycle.rs:2468` | **Port.** `LoopClaim` has only `release` and `close` — there is no `claim`. The kernel assumes a pre-claimed bead. |
| **Provider fallback chain** | `fallback_chain:7610` | **Port.** `LoopRequest` carries a single `profile_id`. No candidate pool, no fallback. |
| **429 / retryable classification** | `classify_retryable_failure:7719`, `contains_contextual_429:7893` | **Port.** Not speculative: dispatching this very review hit `GoUsageLimitError` on the opencode-go lane and required the ollama-cloud fallback. |
| **Backend auth classification** | bead `5p8`, `c9dd390` | **Port.** Prevents claiming a bead and then failing on auth. |
| **Worker resource containment** | `write_worker_sandbox_profile:1364` | **Port.** Real containment, cheap to carry. |
| **Qualitative review stage** | `verify.rs` `run_review_stage_until` etc. | **Port.** Kernel hardcodes `qualitative: None` and `review_resume_budget_secs: None` (`loop.rs:222-223`). The `review` job is v1 scope; the trait shape cannot currently express a review panel. |
| **Issue fetch (`bd.show`)** | `dispatch_one:2196` | **Port.** The worker prompt needs the bead body. `LoopClaim` cannot fetch it. |
| **Clean-tree preflight** | `dispatch_one` ~2380 | **Port.** `loop.rs` reads `head` but never `is_clean` before spawning. Under D1 this is the guard that makes in-repo execution safe. |
| **Heartbeat / live progress** | `dispatch.rs:513 run_with_heartbeat` | **Port.** The kernel writes no heartbeat. The dashboard is in v1 and its `live`/`abandoned` split is keyed on heartbeat freshness (`dashboard/mod.rs:71-77`). |
| Attempt checkout + commit promotion | `5961`, `promote_attempt_commit:1775` | **Drop** per D1. |
| Verification-input materialization | `1285` | **Drop** — exists only to serve the checkout. |
| Supersession | `run_supersession:3915` | **Drop** — only meaningful for promoted runs. |

Phase 1 is therefore *extend the kernel's contract and port eight bounded capabilities*,
not *write two adapters*. Still far smaller than porting `dispatch_cycle` wholesale,
because D1 deletes the majority of what makes that file large — but it is design work,
not wiring, and the spec should not pretend otherwise.

## Scope test

A change enters v1 only if it is required to make this sentence true:

> `undertake <work|review|consult|plan> --repo <path> --target <bead|artifact>` runs to a
> verifier-backed terminal state through one kernel, resumably, on this machine.

## Phases

Each phase is independently shippable, ends in one commit, and has a runnable Verify.

### Phase 0 — Restore signal

- Remove all 22 blanket `#![allow(dead_code)]`. Keep only the legitimate
  `#[cfg_attr(not(feature = "tui"), allow(dead_code))]` gates (`sanitize.rs:31`,
  `process.rs:136`). `loop.rs`/`job.rs` keep theirs until Phase 1 closes.
- Delete confirmed-dead: `roster_drift.rs` (1,017); the 11 orphaned non-deadline wrappers
  in `verify.rs` (`run:199`, `run_with_review:208`, `run_with_backoff:225`,
  `run_with_review_backoff:235`, `run_with_optional_review_backoff:246`,
  `run_mechanical:266`, `run_review_stage:298`, `run_review_stage_deferred:329`,
  `run_mechanical_with_backoff:456`, `review_or_pass:874`, `run_review:1002` — each
  superseded by a live `_until`/`_deadline` sibling); orphaned `dispatch.rs`
  `run`/`spawn_request`/`prepare_worker_lineage_lease`;
  `dispatch_cycle.rs:7715 is_retryable_worker_stderr`; the unused Cautionlight polling
  stack in `dashboard/services.rs`.
- Collapse the 3× duplicated `FakeBdClient`, `FakeChild`, `FakeCommits` into one
  `src/test_support.rs`.
- Re-verify `conductor-1qj` before acting. **`cargo test` is green today — 873 passed, 0
  failed, 8 ignored, measured 2026-07-27.** The bead's "test rejects approved Codex
  terminal fallbacks" claim is stale or conditional. Close it as stale or fix it; do not
  carry an unexamined red-build claim into v1.

**Verify**: `cargo test && cargo clippy --all-targets -- -D warnings -D dead_code`
**Tier**: senior/M — mechanical. Sonnet 5.

### Phase 1 — Design the binding→kernel contract, then turn on the kernel

**Phase 1a — the contract (lead, design).** `JobBinding` (`job.rs:61`) carries
`profile_ids` + `fallback_profile_ids`, `mutation`, `limits`, `verifier` (mechanical
**and** qualitative), `approval_required`, `role_policy`. `LoopRequest` (`loop.rs:83`)
has slots for none of them. Making `JobRegistry` authoritative therefore requires
extending the kernel — **not** a policy-bearing adapter, which would be a fifth engine
and violate invariant 1.

The contract to design and pin before any implementation:

```
LoopRequest  gains: candidates: Vec<ProfileId>   (pool + fallback, ordered)
                    mutation:   MutationPosture
                    approval:   Option<ApprovalRef>   (fails closed when required)
                    verifier:   RunVerifier           (mechanical + optional qualitative)
LoopClaim    gains: show(repo, target) -> Issue       (worker prompt needs the bead)
                    claim(repo, target, owner)
LoopHarness  gains: reviewer(iteration, stage) -> Option<SpawnRequest>
```

Kernel loop order, pinned: backend-auth preflight (fail closed **before** claim) →
`is_clean` preflight → claim → per attempt: select next candidate, spawn, heartbeat while
running → on failure classify (429 / quota / session / other) and either retry the same
candidate or advance the pool → mechanical verify → optional qualitative → terminal close
or release.

**Phase 1b — implementation.** Port the eight capabilities named above. Reuse existing
primitives rather than rewriting: `dispatch.rs:513 run_with_heartbeat`,
`classify_retryable_failure:7719`, `contains_contextual_429:7893`, the `5p8` auth
classifier, `SpawnRequest`'s existing `sandbox_profile` / `worker_resource_limits` fields
(the kernel simply never populates them). Implement production `LoopHarness` / `LoopClaim`
over `BdClient`, `Exec`/`SpawnRequest`/`ChildProcess`, `RunHandle`. Add
`undertake work --repo <path> --bead <id> [--config <path>]`.

**Enforce `MutationPosture`.** `job.rs:49` declares `Work => RepositoryWrite`,
`Review|Consult|Plan => ReadOnly` and nothing enforces it. Mirror the post-review
HEAD/index/worktree check from `8a8f1fe`.

**Verify**: `cargo test loop && cargo test job && cargo test cli`, plus an integration
test driving `undertake work` against a sandbox git repo the test creates: bead claimed →
worker spawned → commit appears → `verify_cmd` runs → bead closed. Not fake-only.

**The test uses a scripted local backend, not the live roster.** If the live roster is
all-`Unknown` (the Phase 3 deadlock), a live-provider test cannot pass before Phase 3
exists. The live-roster proof is Phase 4 gate item 1, deliberately after Phase 3.

**Tier**: 1a lead. 1b senior/L. **This phase is the whole bet** — it ships `work` alone
and proves the kernel's shape before Phase 2 commits three more jobs to it.

### Phase 2 — Move the other three jobs onto the kernel

- **`review`** — route `adversarial.rs` through `JobKind::Review` without weakening
  artifact hashing, immutable approval, read-only execution, schema repair, anonymous
  synthesis, or minority preservation. Keep `adversarial-review` as a warning-free alias.
  (`conductor-adversarial-job`, ready, senior/M)
- **`consult`** — fold Envoy's read-only evidence-or-gaps envelope into a `consult` job.
  (`conductor-consult-job`, ready, senior/M)
- **`plan`** — re-home `plan_job` onto the kernel's attempt/event lifecycle. Its
  peer-review / second-opinion stage machine stays **job policy, not a second engine**.
- Fold in `conductor-pzo` (Fable 5 + provider-diverse review fallbacks) as review-job
  config binding, and `conductor-ao8` with it.

**`conductor-ao8` is v1, not deferrable.** Review-panel independence is computed per
provider *lane*, so `ollama-cloud/glm-5.2` and `opencode-go/glm-5.2` count as two
independent reviewers. That is the same model twice, and the user's real roster contains
exactly that pair. A review job whose diversity guarantee is false is not shippable.

**Verify**: `cargo test job && cargo test adversarial && cargo test plan_job && cargo test
consult`; all four jobs observably dispatch through one `LoopKernel::run`.
**Tier**: senior/M per job — three separable Sonnet 5 items.

### Phase 3 — Break the bootstrap deadlock

`conductor-bxb` (P0) is a real, twice-reproduced deadlock: when every provider is
`Unknown`, dry-run proposes zero work and no Undertake call can produce the evidence that
would make a provider known. **This is why the 2026-07-27 dogfood cycle reported 251
proposed / 0 dispatched** — recorded as a successful propose-only run, at least partly
this bug.

Minimum that breaks the cycle:

- A bounded, tools-disabled, non-repo-cwd probe targeting **only** `Unknown` **and**
  enabled profiles. Exhausted, deferred, disabled, invalid, stale-config: ineligible.
- One approval covers the probe set plus the original bounded target.
- A validated probe appends exact-scope evidence via Musterroll, re-snapshots and
  re-hashes the roster, continues only if normal eligibility now passes.
- Anything unexpected — failure, timeout, no output, schema mismatch, changed profile,
  failed append — stops before bead claim or repo mutation.
- A probe is a preflight phase, never a fifth `JobKind`.

**De-scoped from the bead as written**: cut cost-posture pinning, TTL policy, replay
tests, and full scorecard coupling. Retain only "a probe emits a canonical attempt
record," which `run.rs` already provides.

**Verify**: `cargo test loop && cargo test job && cargo test musterroll` covering
all-Unknown bootstrap, mixed providers, partial success, crash-resume without re-approval.
**Tier**: senior/M (down from lead/L).

### Phase 4 — Parity, then delete

**Ordering is not negotiable.** `dispatch_cycle.rs` is today the only working end-to-end
path. Deleting it before the kernel does real work leaves a product that cannot do
anything.

**Gate — all must hold before one line is deleted:**

1. The kernel has verifier-closed at least one **real** bead in a real repo.
2. All four jobs dispatch through `LoopKernel::run` (Phase 2 complete).
3. The bootstrap probe works against the live roster (Phase 3 complete).
4. `conductor-guildhall-dogfood` is redefined in kernel terms. It currently *means*
   `cycle --dry-run`, a command about to stop existing.

**Delete** (~26,400 lines): `dispatch_cycle.rs` (19,370), `cycle.rs` (1,973), `scan.rs`
(1,128), `ratchet.rs` (1,101), `plan.rs` (1,021), and the fleet-only bulk of `triage.rs`
(~1,800 of 1,836). Remove the config that dies with them: `autonomy`, `[ratchet]`,
`[scan]`, fleet-only `[budgets]` knobs.

**Retained from the original deletion list** — corrections found while verifying:

- **`fields.rs` (579) survives.** The kernel needs routing-field extraction to read
  `tier_floor` / `complexity` / `verify_cmd` from a bead. Listing it for deletion was an
  error.
- **`triage.rs` survives in part.** `route.rs:417` uses `CandidateRejection` /
  `candidate_rejection`. Keep that slice or drop `route explain` deliberately.
- **`verify.rs` (3,589) needs a decision.** Its only consumers are `dispatch_cycle` (6
  refs) and `adversarial` (1). The kernel currently runs its verifier directly via
  `LoopHarness::verifier() -> SpawnRequest`, bypassing `verify.rs` entirely. Either the
  kernel adopts `verify.rs` (and its review stages per D2) or most of it dies with
  `dispatch_cycle`. **Do not discover this mid-deletion.**
- **`state.rs`** migrate paths reference `ratchet` (5), `plan`, `triage` — prune with the
  legacy state they migrate.

**Compile breaks in surviving modules — every one must be resolved in the same commit.**
An earlier draft of this spec claimed the dashboard was "unaffected… verified, not
assumed." That was wrong, and the error is recorded rather than quietly fixed:

| Survivor | Broken reference | Resolution |
|---|---|---|
| `dashboard/mod.rs:77` | `crate::dispatch_cycle::STALE_CLAIM_THRESHOLD` — **production** `pub(crate) const` | Relocate the const (60s, `dispatch_cycle.rs:7035`) to a survivor and re-point. |
| `route.rs:416` | `use crate::fields::RoutingFields` | `fields.rs` survives — resolved by the retention above. |
| `route.rs:417` | `use crate::triage::{CandidateRejection, candidate_rejection}` | Keep that slice of `triage.rs`, or delete `route explain` deliberately and add it to the disappearing-commands list. |
| `state.rs:117` | `copy_typed_file::<crate::ratchet::RatchetStore>` — **production** | Prune the ratchet leg of legacy-state migration. |
| `state.rs:763-890` | `ratchet`, `plan::CyclePlan::from_triage`, `triage::Plan` (tests) | Prune with the tests they cover. |
| `cli.rs:347, 1422` | `crate::fields::Triage` / `extract`, `RoutingFields` | Survives via `fields.rs` retention; re-check after the command removals. |
| `main.rs:8-28` | `mod` declarations for every deleted module | Mechanical. `mod roster_drift;` must also go in **Phase 0**. |

**Not breaks** (verified, listed so they are not re-flagged): `adversarial.rs:3180-3181`
are *string literals* inside a module-isolation deny-list test. Matches on `cycle::` in
`quarantine.rs` / `run.rs` are substrings of `RunLifecycle::`.

**The dashboard needs more than a const move.** Its `live`/`abandoned` split is keyed on
heartbeat freshness (`dashboard/mod.rs:71-77`). The kernel writes no heartbeat. Phase 1
ports `run_with_heartbeat`; if that slips, the dashboard's liveness view goes static and
that must be an accepted, stated regression — not a surprise.

**Consequences to accept, not discover:**

- Five commands disappear: `scan`, `cycle`, `dispatch`, `supersede`, and `status` in its
  journal-reading form.
- `AGENTS.md` and the `guildhall-orchestration` skill document the
  `undertake cycle` / `undertake dispatch` flow. Both live in chezmoi and are
  **human-applied**. This phase produces a proposed diff; it never applies it.
- `undertake supersede` (1,492 lines, `de954c8`, two days old) loses its reason to exist
  under D1. Stated plainly rather than discovered mid-deletion.

**Verify**: `cargo test && cargo clippy --all-targets -- -D warnings -D dead_code`;
`undertake --help` advertises only kernel commands; the dashboard renders a
kernel-produced run.
**Tier**: senior/M mechanical; **lead reviews the gate**, not the diff.

### Phase 5 — The v1 gate

- `scripts/smoke-installed-loop-product.sh --isolated --no-metered`: the **installed**
  binary, isolated state roots, Musterroll → Undertake → Afterfact → Cautionlight,
  verifying every artifact hash and schema boundary. (Cutover gate 10. Neither the script
  nor `scripts/` exists yet.)
- Real CLI integration tests under `tests/`. Today `cli::run` is called **only from
  `main.rs`** — 986 tests and the front door is untested. The single file in `tests/`
  is a static template check.
- Close `conductor-bnc`.

**Verify**: the bead's own `verify_cmd`.
**Tier**: senior/M.

## Backlog surgery

Two dependency edges cut, with justification:

- **`conductor-7hb` → `conductor-bxb`**: 7hb is scorecard completeness for Afterfact
  (evidence/reporting). A bootstrap probe needs to *emit* an attempt record — `run.rs`
  already does that — not to complete Afterfact parity. Cut; fold the one-line
  requirement into Phase 3.
- **`conductor-plan-review-eval-fold` → `conductor-bnc`**: importing the corrected
  Gauntlet corpus into plan/review test expectations is test-infra. It does not gate the
  kernel running four jobs.

**Deferred wholesale — 9 robustness-speculative beads** (29% of the open backlog):
`038` (lease races during mid-upgrade binary replacement), `8hz`, `1ls` (power-loss
fsync), `3ce` (two concurrent lease reclaimers), `2bh` (fs2→fs4 + non-macOS semantics),
`47p` (PID reuse), `t7q` (power-loss fsync ordering), `4wq` (portable `kill -0 -PGID` on
platforms not run here), `moe` (sub-second TOCTOU between a human's manual `bd close` and
Undertake's release).

Each requires a second concurrent Undertake process on one repo, a literal mid-upgrade
binary swap, an OS this machine does not run, or a crash landing in a several-instruction
window. Single-operator macOS. Note that `1ls`, `8hz`, and `jum` additionally lose their
subject matter under D1.

**Also deferred**: `eel` (Managed Agents POC), `2d4` (native Codex app-server client),
`88v` (local Ollama admission), `tdj` (quota-aware load spreading), `7hb`,
`plan-review-eval-fold`, `7rs` (legacy ledger retirement — gated on Afterfact parity).

**Kept, small, real**: `blv` (relative state dir breaks worker-cwd artifact paths), `jum`
(survivor filenames glob-interpreted by `git apply --exclude`; breaks on `src/[id].ts` —
**re-check under D1**, it may vanish). Both senior/S. `dpo` and `74d` die with the legacy
path.

## Invariants

1. **One kernel.** A new execution path is a defect, not a feature.
2. Deletion is gated on demonstrated parity, never on confidence.
3. One writer per repo.
4. Every execution starts from an explicit target and immutable maximum scope.
5. Unknown roster, provider, schema, artifact hash, verifier, or approval state fails closed.
6. Read-only jobs cannot mutate their repo; a mutation is an infrastructure failure.
7. No push, no `chezmoi apply`.

## Non-goals

A fifth job kind; a workflow DSL; reviving the ratchet or fleet-wide unattended cycling;
any new abstraction over the four job engines beyond the kernel; any hardening in the
deferred bucket; applying the chezmoi diff.

## Risks

- **Phase 1 is the whole bet.** If the kernel's traits are the wrong shape for real
  adapters, Phase 1 becomes a redesign. Mitigated by shipping `work` alone first.
- **D1 is load-bearing.** If in-repo execution is judged unsafe, the promotion subsystem
  must be ported and Phase 1 roughly doubles.
- **Phase 4 is irreversible in practice.** Git recovers the code, not the context.
- **`plan` may resist re-homing.** If it cannot sit on the kernel without distorting the
  kernel, that is a finding to surface, not route around.

## Adversarial review record

**Reviewer 1 — GLM 5.2 (Zhipu family, via ollama-cloud), 2026-07-28. Verdict: SHIP WITH
CHANGES.** The opencode-go lane returned a live `GoUsageLimitError` (monthly limit, resets
in ~15 days); the ollama-cloud fallback carried it. Adjudicated against source by the
author; **all three mandatory findings accepted and folded in above**:

1. *Dashboard is a production dependency on `dispatch_cycle`* — **accepted, verified.**
   `dashboard/mod.rs:77` is a `pub(crate) const` (test module starts line 79). The
   draft's "verified, not assumed" was the opposite of true. Also surfaced the deeper
   heartbeat gap.
2. *`route.rs` and `state.rs` break on deletion* — **accepted, verified.**
   `state.rs:117` is production. Break table added to Phase 4.
3. *Phase 1 is a redesign, not wiring* — **accepted.** `LoopRequest` has no slot for
   pool, fallback, approval, posture, or qualitative verifier; `LoopClaim` has no `claim`
   and no `show`. Phase 1 split into 1a (contract design) and 1b (implementation).

Also accepted: the missing `is_clean` preflight, the `prior_capture` → text-feedback
downgrade, "never consumed" softened, `mod roster_drift;` removal added to Phase 0,
Phase 3 ordering tension resolved by pinning Phase 1's test to a scripted local backend.

Rejected: none. One correction to the reviewer — its `fields.rs` break is already
resolved by this spec retaining `fields.rs`; its line-count arithmetic (27,008) assumed
`fields.rs` deletion.

**Reviewer 2 — GPT-5.6 Sol (OpenAI family, via omp at `max`)**: dispatched, still
running at time of writing. Sol authored much of the current codebase state and was told
so, and asked to rebut the overengineering criticism on evidence. Its findings must be
adjudicated and folded in before this spec is executed.

## Verified facts (measured 2026-07-27, re-check before relying)

- `cargo test`: 873 passed, 0 failed, 8 ignored.
- `src/` totals 87,148 lines; 39,878 (45.8%) inside `#[cfg(test)]`; 986 `#[test]` fns.
- `tests/` contains one file, a static template assertion. `cli::run` has no test caller.
- 22 modules with blanket `#![allow(dead_code)]`.
- Dispatching this spec's own review hit a live provider quota limit on opencode-go,
  requiring the ollama-cloud fallback lane.
