# Undertake v1 finish — spec

**Status**: draft v2, revised after two adversarial reviews (Opus 5, 2026-07-28).
Draft v1 was **rejected**; see § Adversarial review record. Direction inverted.
Implements cutover gates 4 and 10 of `guildhall/.docs/ai/phases/undertake-core-consolidation-spec.md`.
**Owner**: Opus (Lead) specs and adjudicates. Senior implements per phase.

## The diagnosis

The approved architecture is *one kernel, four jobs, explicit targets*. Today there are
**four independent engines** and no kernel.

| Engine | Lines | Job | State |
|---|---|---|---|
| `dispatch_cycle.rs` | 19,370 (8,237 prod / 11,133 test) | `work`, fleet-scoped | The only live, hardened, end-to-end mutating path. |
| `plan_job.rs` | 5,995 | `plan` | Live. Own approval, weighted reservations, author/peer/revision/second-opinion stages, isolated worktrees, cancellation, recovery. |
| `adversarial.rs` | 5,110 | `review` | Live as `adversarial-review`. Own N-reviewer panel, per-slot fallback, schema repair, anonymity, judge recheck. |
| `loop.rs` | 989 (431 prod) | — | **Production-dead prototype.** Zero callers. |
| `consult` | 0 | — | Does not exist. `cycle.rs:268` writes a breadcrumb record only. |

Each hand-rolls "spawn → wait → classify → retry/fallback → terminal → close or release."
Only the lowest-level process primitives in `dispatch.rs` are shared.

### Why draft v1 was wrong

Draft v1 assumed `loop.rs` was a finished kernel needing only wiring. Verified against
source, it is not:

- **It cannot run read-only jobs.** `loop.rs:346-359` unconditionally requires the worker
  to produce an authenticated direct-child commit; without one it calls `fail_attempt`.
  `review` and `consult` produce no commit and can therefore never succeed.
- **It cannot run `plan` at all.** `LoopKernel::start` calls `RunHandle::create`, which
  returns `Err("plan runs require explicit structural PlanRunDetails and are not
  activated")` (`run.rs:1021-1025`).
- **It hardcodes `RunJob::Work` and `WorkState`** (`loop.rs:214-245`).
- **Its terminal model is only `Completed | Failed`** (`loop.rs:137-141`) — no `blocked`,
  `needs_input`, or `canceled`, all of which the approved plan/consult contracts require.
- **`LoopClaim` cannot claim.** Only `release` and `close` (`loop.rs:133-135`). Claiming
  outside the kernel puts a mutation before its durable boundary and leaves resume unable
  to prove ownership.
- **`loop.json` is neither fsynced nor integrity-bound** (`loop.rs:537-546`). A forged
  `terminal=completed` is trusted at `loop.rs:288-290` and would close a bead with no
  attempt — a direct violation of the fail-closed artifact-hash invariant.
- **Resume is not resumable.** It refuses on any `worker_pgid` without checking liveness
  (`loop.rs:291-295`), and if a worker committed but crashed before verification it resets
  to `Ready` and reruns the worker instead of verifying the existing commit
  (`loop.rs:296-308`). `validate_run_target` (`loop.rs:515-523`) binds neither profile,
  attempts, timeout, nor original authorization — a resume can dispatch a profile the
  manifest never approved.

**`job.rs` is not merely unconsumed — it is never constructed.** The accepted TOML
spelling is `[[job]]`, not `[[jobs]]` (`config.rs:1471-1477`), and `undertake.toml`
contains **zero** `[[job]]` tables. `parse_native_jobs` returns an empty vec and
`JobRegistry::new` is never called in production.

### The real signal problem

**19 modules carry a blanket `#![allow(dead_code)]`** (not 22 — that figure counted files
with any dead-code allowance, including legitimate `cfg_attr` gates). Reachability lint is
off across the crate. `roster_drift.rs` (1,017 lines) is production-retired at
`cli.rs:1764` and still compiles clean. `ratchet.rs` is operationally disconnected —
`cycle.rs:193-205` passes an empty map — so `autonomy = "propose"` and the entire
`[ratchet]` table accept operator input with no runtime effect.

But restoring `-D dead_code` proves *symbol reachability, not behavioral parity*, and
flipping it early would force deletion of recovery and artifact APIs the moment their only
caller dies — making rollback harder. It belongs at the **end**, not the start.

### The one genuine P0

`conductor-bxb`: when every provider is `Unknown`, dry-run proposes zero work and no
Undertake call can produce the evidence that would make a provider known. **This is why
the 2026-07-27 dogfood cycle reported 251 proposed / 0 dispatched** — recorded as a clean
propose-only run.

## Locked decisions (user, 2026-07-27)

| Decision | Value |
|---|---|
| v1 target | All four jobs through one kernel. |
| Dashboard | **In v1.** Same verification bar as the kernel. |
| Legacy fleet surface | **Deleted as part of v1** — now gated far more strictly (Phase 6). |

## Direction: extract from the proven engine, do not promote the prototype

Draft v1 proposed growing `loop.rs` to parity. Rejected. Growing a 431-line prototype to
match 8,237 lines of hardened behavior means rewriting nearly all of it while carrying its
wrong assumptions (commit-required, work-only, forgeable state).

**Instead: extract a generic durable attempt runner from the proven
`dispatch.rs` / `run.rs` / `quarantine.rs` machinery, migrate one job at a time onto it,
and delete `loop.rs`.**

Salvage from `loop.rs` as design input, not code: fresh-context-per-iteration, durable
phase checkpoints, and the bead/artifact target distinction.

The generic boundary must carry what two engines already prove they need — and what the
prototype has no slot for:

```
job + stage identity            immutable selection + pinned approval
total item deadline             durable attempt lifecycle (start/finish/retry lineage)
output + artifact capture       mutation posture (read-only vs repo-write)
process hooks (heartbeat)       terminal states beyond Completed|Failed
bead terminal actions           candidate pool + fallback + 429 classification
```

## Resolved decisions

### D1 — `work` writes the repository directly (user, 2026-07-28)

Attempt isolation is dropped. `dispatch_cycle` runs each worker in an isolated
`AttemptCheckout` (`5961`) and promotes the commit (`1775`); v1 does not.

Grounds: the consolidation spec's `work` row reads "Repo writes allowed inside approved
scope," its loop requirement 7 is "worker identity plus exclusive repo lease" — a lease
and an identity check, not worktree isolation — and it states the native loop "preserves
Ralph's earned behavior." Ralph works in-repo.

**Removed from v1 scope entirely**: attempt checkout, commit promotion, verification-input
materialization (`1285` — it exists *only* because a fresh checkout lacks gitignored
files), `undertake supersede` (1,492 lines, `de954c8` — it terminalizes failed *promoted*
runs, and there are none), promotion recovery records, and three of four resume state
machines (`resume_promoted_work:4492`, `resume_unauthenticated_implementing_work:5172`,
`resume_finished_promoted_work:3986`).

**Required in exchange — these are not optional.** The runner must add, and Phase 1's
contract must pin:

- a **clean-tree preflight** before spawn (fail closed on a dirty target);
- **quarantine adoption** of a failed attempt's partial work, carried into the next
  attempt as a patch reference rather than the prototype's plain-text feedback;
- retention of the post-verify HEAD/tree/claim recheck, which currently lives in the
  promotion path.

Without all three, D1 ships a real safety regression against `dispatch_cycle`, which
preflights both.

### D2 — review-panel diversity is by model family (user, 2026-07-28)

`conductor-ao8` stays in v1. This **amends** the approved architecture: the consolidation
spec compares exact `ProviderId` (`undertake-core-consolidation-spec.md:93-97`) and
`adversarial.rs:565-602` implements that faithfully, so `ollama-cloud/glm-5.2` and
`opencode-go/glm-5.2` count as two independent reviewers today. They are the same weights
behind two resellers.

The operator policy in `AGENTS.md` — reviewers of a *"different model family (developer
lineage, not inference provider)"* — governs. Record as an ADR in `decisions.md`; it is a
contract amendment, not a bug fix.

**Cross-repo dependency**: Musterroll owns profile identity, so a `model_family` (developer
lineage) field must be added to `musterroll/roster@2` and populated before Undertake can
enforce it. That work is Musterroll's, and `review`-job diversity enforcement is gated on
it. Do not infer family by parsing `ProfileId` — the spec forbids deriving execution
coordinates from the opaque label (`undertake-core-consolidation-spec.md:116-117`).

## Scope test

A change enters v1 only if it is required to make this sentence true:

> `undertake <work|review|consult|plan> --repo <path> --target <bead|artifact>` runs to a
> verifier-backed terminal state through one kernel, resumably, on this machine.

## Phases

### Phase 0 — Freeze the corpus, close what is already fixed

The consolidation spec requires a golden/parity corpus **before** an implementation
retires (`undertake-core-consolidation-spec.md:489-490`). Draft v1 omitted this.

- Freeze `dispatch_cycle`'s 11,133 test lines as the named behavioral parity corpus for
  `work`. Every later phase re-runs it; deletion is gated on it.
  - **Named selector**: `cargo test --bin undertake dispatch_cycle::tests::` (bin-only
    crate, no `lib.rs` — `--bin undertake` is required, `--lib` will not find it). The
    `dispatch_cycle::tests::` substring matches every `#[test]` fn in the file's single
    flat `mod tests` block (`src/dispatch_cycle.rs:8238`–EOF) and nothing outside it.
  - **The gate is "every test under the selector passes, and the count never shrinks
    without explicit justification"** — *not* a fixed number. The corpus legitimately
    grows as later phases add coverage to `dispatch_cycle`; it must never silently lose
    tests, which is what the count guards against.
  - **Measured 2026-07-28 at `f5e1c0a`**: 123 passing. **Re-measured at `c3488fa`: 126
    passing**, and the +3 was traced to our own prep work — `moe` (`b88da79`), `47p`
    (`5224787`), `gtgf` (`bf44828`), `8nth` (`ca03137`), and `44hc` (`1078a1f`) each
    added tests to that module. Growth from added coverage is expected and fine.
    Confirmed via `#[test]` grep count in that line range matching the `--list` count exactly.
  - This selector must pass unchanged (same 123, all green) before the legacy engine
    (`dispatch_cycle.rs`) may be deleted — that deletion is Phase 6.
- Delete `roster_drift.rs` (1,017) and its `main.rs` mod declaration. Uncontroversially
  retired, zero callers.
- **Close three beads as stale, not deferred** — their premises are already fixed:
  `3ce` (stable-guard inode + kernel lock make reclamation single-winner,
  `quarantine.rs:542-574`), `t7q` (manifest writes already use fsynced durable
  replacement, `run.rs:3500-3579`), `4wq` (liveness uses the `nix::kill` syscall, not
  shell argv, `quarantine.rs:978-988`). Re-verify each before closing.
- **Do not** flip `-D dead_code`, prune `verify.rs` wrappers, or consolidate test fakes.
  The fakes have genuinely different state and failure seams; merging them is unrelated
  refactoring that increases coupling before a risky cutover.

**Verify**: `cargo test && cargo clippy --all-targets -- -D warnings`
**Tier**: junior/S–senior/S. Sonnet 5.

### Phase 1 — Design and extract the generic attempt runner

**1a — contract design (lead).** Pin the boundary listed above before any code moves.
Name explicitly: where `bd.claim` and `bd.show` happen relative to the durable boundary;
how the candidate pool and fallback are expressed on retry; where approval fails closed;
how mutation posture is enforced; the full terminal-state enum.

**1b — extraction (senior/L).** Lift the generic runner out of the proven machinery —
`dispatch.rs:513 run_with_heartbeat`, `classify_retryable_failure:7719`,
`contains_contextual_429:7893`, the `5p8` auth classifier, `SpawnRequest`'s existing
`sandbox_profile` / `worker_resource_limits` fields, `run.rs`'s durable event journal,
`quarantine`'s leases and capture. Extraction, not reimplementation.

**Verify**: the frozen corpus still passes with `work` routed through the extracted runner.
**Tier**: 1a lead, 1b senior/L. **This is the whole bet.**

### Phase 2 — Migrate `work`; delete the prototype

Route `undertake work --repo <path> --bead <id>` through the extracted runner. Delete
`loop.rs` and `job.rs`'s unused authority, or wire `job.rs` properly — but stop shipping a
registry that is never constructed. Add a real `[[job]]` block to `undertake.toml`.

**Verify**: frozen corpus green + an integration test driving the CLI against a sandbox
git repo with a **scripted local backend** (not the live roster — see Phase 3).
**Tier**: senior/L.

### Phase 3 — Break the bootstrap deadlock (moved earlier)

Draft v1 put this after all job migrations. Wrong: with an all-`Unknown` roster, no
migrated job can be live-dogfooded. Bootstrap belongs immediately after generic profile
selection exists.

Minimum that breaks the cycle: a bounded, tools-disabled, non-repo-cwd probe targeting
**only** `Unknown` **and** enabled profiles; one approval covering probe set plus target;
validated probes append exact-scope evidence via Musterroll, re-snapshot and re-hash the
roster, continue only if normal eligibility now passes; anything unexpected stops before
bead claim or repo mutation. A probe is a preflight phase, never a fifth `JobKind`.

**De-scoped** from the bead as written: cost-posture pinning, TTL policy, replay tests,
full scorecard coupling.
**Tier**: senior/M (down from lead/L).

### Phase 4 — Migrate `review`, `consult`, `plan` one at a time

Not three Senior/M items — two are full engine migrations and one is new construction.

- **`review`** (`adversarial.rs`): N reviewers, per-slot fallback, schema repair,
  anonymity, immutable approval, judge recheck, minority preservation. Preserve every one.
  Keep `adversarial-review` as a warning-free alias. senior/L.
- **`plan`** (`plan_job.rs`): preparation/approval, weighted durable reservations,
  author/peer/revision/second-opinion stages, schema validation, isolated worktrees,
  cancellation, recovery. Its stage machine becomes job *policy* on the shared runner.
  **Requires `RunHandle::create` to stop refusing Plan runs.** lead-specced senior/L.
- **`consult`**: no implementation exists. Import Envoy's prompt, evidence-or-gaps schema,
  validator, and fixtures. senior/M.

Each migration defines its own CLI front door — `undertake review|consult|plan --repo
<path> --target <...>` — including how two-step human approval maps onto one invocation.
Draft v1 never specified these.

### Phase 5 — Adapt the dashboard, then pass gate 10 *before* deleting anything

Draft v1 deleted the rollback engine and *then* attempted the installed smoke. Inverted.

- **Dashboard compile breaks** (draft v1 wrongly called this "verified" decoupled):
  `dashboard/mod.rs:72-77` → `dispatch_cycle::STALE_CLAIM_THRESHOLD`;
  `run_source.rs:1282-1319` → `deck::report_run_dir`;
  `run_source.rs:1221-1225,1377-1381` → `quarantine::{process_alive,process_group_alive}`.
  Relocate the const; the other two are survivors.
- The dashboard's `live`/`abandoned` split is keyed on heartbeat freshness. The runner must
  emit heartbeats or that view goes static — an accepted, stated regression, not a
  surprise.
- **Gate 10**: `scripts/smoke-installed-loop-product.sh --isolated --no-metered` — the
  installed binary, isolated state roots, Musterroll → Undertake → Afterfact →
  Cautionlight, verifying every artifact hash and schema boundary. Neither the script nor
  `scripts/` exists. This is substantially more than senior/M.

### Phase 6 — Quiesce, migrate guidance, then delete

**Prerequisites, all mandatory** — draft v1 had none of the first two:

1. **Legacy quiescence.** The architecture requires quiescing cycle/dispatch and resolving
   every pending, implementing, or reclaimable legacy run before deployment
   (`undertake-core-consolidation-spec.md:175-178`). Without it, deletion strands claims,
   promoted commits, pending reviews, and recovery receipts.
2. **Operator guidance migrated.** `AGENTS.md` and the `guildhall-orchestration` skill
   still invoke `cycle`/`dispatch`. They live in chezmoi and are **human-applied**. Either
   retain warning shims until that lands, or make the human migration a hard prerequisite.
   Producing an unapplied diff and deleting anyway guarantees a broken operator.
3. Frozen parity corpus green against the new runner. Gate 10 passed.
4. `conductor-guildhall-dogfood` redefined in kernel terms.

**Then delete** (~27,000 lines): `dispatch_cycle.rs`, `cycle.rs`, `scan.rs`, `ratchet.rs`,
`plan.rs`, and the fleet-only bulk of `triage.rs`.

**Retained** — corrections found while verifying: `fields.rs` (579) survives; `cli.rs:340-369`
(bead-backed plan input) and `cli.rs:1417-1429` (`route explain`) need it. `triage::candidate_rejection`
survives for `route.rs:415-417`. `ratchet::RatchetStore` survives for `migrate state`
(`state.rs:113-120`) or its schema moves. **Either `route explain` and `migrate state` join
the disappearing-commands list, or these pieces are extracted first.**

Removing `dispatch_cycle` also orphans most of `verify.rs` (its only production consumers
are `dispatch_cycle` ×6 and `adversarial` ×1), with fallout in `run.rs`, `quarantine.rs`,
`dispatch.rs`, `ledger.rs`, `config.rs`, `state.rs`, `cli.rs`, `main.rs`.

**Not breaks** (verified, so they are not re-flagged): `adversarial.rs:3180-3181` are
string literals in a module-isolation deny-list test; `cycle::` matches in
`quarantine.rs`/`run.rs` are substrings of `RunLifecycle::`.

**Only now** enable `-D dead_code` and prune the resulting fallout.

## Backlog surgery

**Cut two dependency edges**: `7hb → bxb` (scorecard completeness is Afterfact parity; a
probe needs only to emit an attempt record, which `run.rs` already does) and
`plan-review-eval-fold → bnc` (test-infra, does not gate the kernel).

**Promoted to v1 — reachable single-operator, contra draft v1:**

- `47p` — lease ownership stores and checks **PID only** (`quarantine.rs:651-666`). One
  crash plus later PID reuse by any unrelated process wedges resume permanently. No
  concurrency required. This directly contradicts the v1 resumability claim.
- `moe` — the operator manually closes a bead while a run finishes; `bd.release`
  unconditionally runs `bd update --status open --assignee ""` (`bd.rs:235-238`),
  reopening completed work. A repo lease cannot serialize a human `bd` command.

- `jum` — **promoted, and D1 raises its severity.** The glob-interpreted `--exclude` is in
  `quarantine.rs:321`, not the promotion path: it is quarantine's transactional recovery
  reapplying a captured dirty-tree patch while excluding survivors. D1 makes quarantine
  capture/adoption a hard requirement, so this path is now load-bearing. A survivor named
  `src/[id].ts` breaks it. senior/S.

**Closed as moot under D1** — their subject matter no longer exists: `8hz` (crash between
promotion Intent and `merge --ff-only`) and `1ls` (integrity-binding `promotion.json`;
its fsync premise was already stale per `dispatch_cycle.rs:952-971`). Confirm each is
genuinely promotion-only before closing.

**Closed as stale** (Phase 0): `3ce`, `t7q`, `4wq` — premises already fixed in source.

**Deferred**: `038` (address via the Phase 6 quiesce gate rather than mixed-version lease
support), `2bh` (macOS-only v1), `eel`, `2d4`, `88v`, `tdj`, `7hb`,
`plan-review-eval-fold`, `7rs`. `pzo` is **gate 11** work
(`undertake-core-consolidation-spec.md:507`), not gates 4/10 — keep only the generic
review-job selection.

**Kept, small**: `blv` (a relative state dir yields artifact paths a worker cannot resolve
from its cwd).

## Invariants

1. **One kernel.** A new execution path is a defect, not a feature.
2. Deletion is gated on a frozen parity corpus and a passed gate 10, never on confidence.
3. Legacy runs are quiesced and resolved before their engine is removed.
4. One writer per repo.
5. Every execution starts from an explicit target and immutable maximum scope.
6. Unknown roster, provider, schema, artifact hash, verifier, or approval state fails closed.
7. Read-only jobs cannot mutate their repo; a mutation is an infrastructure failure.
8. No push, no `chezmoi apply`.

## Non-goals

A fifth job kind; a workflow DSL; reviving the ratchet or fleet-wide unattended cycling;
test-double consolidation; wholesale lint cleanup before migration; applying the chezmoi
diff.

## Adversarial review record

Draft v1 went to two Lead-tier reviewers of different model families from the author
(Opus 5 / Anthropic).

**GLM 5.2 (Zhipu, via ollama-cloud) — SHIP WITH CHANGES.** The opencode-go lane returned a
live `GoUsageLimitError`; the ollama-cloud fallback carried it. Three mandatory findings,
all verified against source and accepted: the dashboard is a *production* dependency on
`dispatch_cycle` (draft v1's "verified, not assumed" was the opposite of true);
`route.rs` and `state.rs:117` are production compile breaks; Phase 1 was a redesign
mislabeled as wiring.

**GPT-5.6 Sol (OpenAI, via omp at `max`) — REJECT.** Sol authored much of the current
codebase and was told the draft criticized that work; it was asked to rebut on evidence.
Its review was the stronger of the two and its central finding is **accepted**: `loop.rs`
is a work-only, commit-requiring prototype structurally incapable of hosting read-only
review/consult or staged plan, with a forgeable state file and non-functional resume.
Draft v1's premise — "the kernel is built, just turn it on" — was false. Verified
independently at `loop.rs:346-359`, `run.rs:1021-1025`, and the absent `[[job]]` table.

Also accepted from Sol: parity corpus before retirement; gate 10 before deletion; the
legacy quiescence gate; bootstrap moved earlier; `-D dead_code` moved to the end; three
beads closed as stale rather than deferred; `47p`/`moe` promoted to v1; `pzo` reclassified
as gate 11; test-fake consolidation dropped; corrected counts (19 blanket allows, not 22;
`job.rs` never *constructed*, not merely unconsumed).

**Partially rejected — D2.** Sol argues `ao8` contradicts the approved `ProviderId`
diversity rule (`undertake-core-consolidation-spec.md:93-97`) and is therefore not a v1
bug. Correct as to the spec. But the user's `AGENTS.md` requires reviewers of a *different
model family (developer lineage, not inference provider)*, which the spec's rule does not
deliver. The spec and the operator's standing policy conflict. Recorded as decision D2 for
the user rather than settled by either reviewer.

**Not adopted**: Sol's claim that deleting `dispatch_cycle` destroys 11,000 lines of
irreplaceable specification is directionally right but overstated — much of that corpus is
~20 bespoke `Exec` fakes shaped around one god-function and will not transfer verbatim.
Freezing it as a parity corpus (Phase 0) captures the value without blocking the
architecture change.

## Verified facts (measured 2026-07-27/28, re-check before relying)

- `cargo test`: 873 passed, 0 failed, 8 ignored.
- `src/` totals 87,148 lines; 39,878 (45.8%) inside `#[cfg(test)]`; 986 `#[test]` fns.
- `tests/` contains one file, a static template assertion. No test drives `cli::run`, the
  installed binary, real `bd`, and a real worker subprocess end to end.
- 19 modules with blanket `#![allow(dead_code)]`.
- `undertake.toml` contains zero `[[job]]` tables.
- Dispatching this spec's own review hit a live provider quota limit on opencode-go,
  requiring the ollama-cloud fallback — evidence that candidate-pool fallback belongs in
  the kernel, which the prototype could not express.
