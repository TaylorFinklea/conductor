# The generic attempt runner — contract

**Status**: draft 3 — **NOT YET BUILDABLE**. Bead `conductor-y6kv` (v1 Phase 1a).
Design half of `undertake-v1-finish-spec.md` Phase 1; `conductor-mkct` implements it.

Two adversarial reviews (GLM 5.2: SHIP WITH CHANGES; GPT-5.6 Sol: **REJECT**) agree the
extraction direction is right and the seam is not yet implementable. Draft 3 fixes every
verified factual error and the seam holes both reviewers named. **Three questions remain
open** (§ Open questions) and one is the user's. Do not start `conductor-mkct` until they
close.

## What is actually missing

`dispatch.rs` already provides good, job-agnostic **process primitives**. Verified:

| Primitive | Location | Already generic? |
|---|---|---|
| `Exec` (`spawn`, `auth_readiness`) | `dispatch.rs:322` | Yes |
| `SpawnRequest` (argv, cwd, env, stdin, `sandbox_profile`, `worker_resource_limits`, `commit_receipt_socket`, stdout/stderr paths) | `dispatch.rs:240` | Yes |
| `ChildProcess` (`wait_for`, `terminate`, `kill`, `id`, `commit_receipt_evidence`, `ensure_worker_quiescent`) | `dispatch.rs:372` | Yes |
| `WorkerHooks` (`on_pre_spawn`, `on_spawn`, `on_worker_quiescent`, `on_heartbeat`) | `dispatch.rs:429` | Yes |
| `CommitProbe` (`head`, `is_clean`, `is_direct_child`) | `dispatch.rs:466` | Yes |
| `AuthReadiness` (Ready / NotAuthenticated / Unreadable) | `dispatch.rs:305` | Yes |
| `run_with_heartbeat` — one *mutating* attempt | `dispatch.rs:513` | Yes |
| `DispatchFailure`, `CommitAuthenticationRejection` | `dispatch.rs:202`, `211` | Yes |
| `RunHandle`, `EventKind`, `RunLimits`, `RunVerifier`, `ArtifactRef`, `durable_atomic_replace` | `run.rs` | Yes |
| `RepoLease`, dirty-tree capture | `quarantine.rs:557` | Yes |

**Exactly one layer is missing: the durable attempt *sequencer*.** That layer —
select candidate → preflight → execute → classify → retry or advance → verify → terminal
— is what is hand-rolled four times (`dispatch_cycle::run_worker_chain:5823`,
`loop::LoopKernel::run:257`, `plan_job::dispatch:1549`, `adversarial::run_reviewers:973`).

This contract specifies that one layer. **It does not redesign the primitives.** Phase 1b
is extraction: the sequencer is lifted from `dispatch_cycle`, and the primitives are used
as they stand.

## Shape

One runner owns the sequence. Per-job variation enters through **one** policy trait, never
through a second engine.

**The attempt loop is two-level, not one.** A single-level candidate walk cannot host
`review`, whose real shape (`run_reviewers:973`) is *the whole panel concurrently, and each
slot running its own internal chain* (`run_reviewer_slot:1913`). Conflating the two makes
`AdvanceCandidate` ambiguous — next reviewer, or next fallback within this reviewer?

```
AttemptRunner::run(policy, ports, request) -> Terminal

  acquire RepoLease
  preflight: auth_readiness -> is_clean -> approval+digest revalidation -> bead claim

  for each stage the policy yields (from the stage ledger):
      revalidate approval + policy digests            // drift since approval = fail closed
      slots = stage.slots                             // N for review's panel; 1 elsewhere
      run slots with stage.concurrency:
          per slot, walk its OWN ordered candidate chain:
              execute (posture-selected: mutating | read-only)
              classify_attempt -> Accept | RetrySameCandidate | AdvanceCandidate | Fatal
              //  RetrySameCandidate = schema repair, same profile, prompt sees the failed output
              //  AdvanceCandidate   = next entry in THIS slot's chain
      join
      aggregate_stage(slot results) -> StageOutcome   // review's panel-completeness lives here
      record stage artifacts into the ledger          // later stages read them
      verify (mechanical, then optional qualitative)  // work only

  terminal: policy.terminal(ledger) -> durable evidence FIRST, then the one Bead mutation
```

Everything outside the `policy.` calls is the runner's, identical for all four jobs.

**The runner is the sole writer.** Slots produce *results*, never writes. See § Concurrency.

## Types to define

### `Terminal` — replaces `Completed | Failed`

`loop.rs:137-141` offers only two outcomes. The approved contracts need more: `plan` ends
`needs_input` on unresolved open questions and `blocked` on loss of a required legal
candidate; `review` ends `blocked` when a required reviewer is unavailable (`conductor-koi`
shipped this as a distinct gap); `consult` returns evidence-or-gaps.

```
Terminal = Completed | Failed | Blocked { reason } | NeedsInput { reason } | Canceled
```

This is not speculative: `PlanTerminalVerdict` (`run.rs:477-482`) already ships
`Accepted | Rejected | Blocked | NeedsInput`, and `RunHandle::finish` already accepts
`"canceled"`. The generic enum is that set with `Accepted → Completed` and
`Rejected → Failed`. Adopt the existing vocabulary.

Only `Completed` may close a Bead. `Blocked` and `NeedsInput` are **not** degraded
success and must never be reported as shipped work.

### `AttemptOutcome` — the classification union

Union of the existing taxonomy plus what the policy contributes:

- from `DispatchFailure` (`dispatch.rs:202`): `TimedOut`, `ExitNonZero`, `NoNewCommit`,
  `UnauthenticatedCommit`, `BackendFlakeZeroStdoutNoCommit`
- from `CommitAuthenticationRejection` (`dispatch.rs:211`): all nine variants
- from runtime-limit classification: mirror `classify_retryable_failure:7719`,
  `contains_contextual_429:7893`, `classify_canonical_harness_session_limit:7744`,
  `extract_provider_reset:7763` — do not re-derive these, move them
- from the policy: `SchemaInvalid`, `SchemaRepaired`, `VerdictRejected` and any other
  job-specific reading of a successful process

The runner maps an outcome to one of `Accept | RetrySameCandidate | AdvanceCandidate |
Fatal`. Quota, rate-limit, and session-limit classifications **advance the candidate**;
they never retry the same profile. This is not theoretical — dispatching this cycle's own
adversarial review hit `GoUsageLimitError` on `opencode-go` and required the
`ollama-cloud` lane.

**The mapping is per stage and slot, never global.** An earlier draft asserted
`AdvanceCandidate ≙ process/eligibility failure` as shared behavior. It is not:

- `review`: process failure takes the slot's fallback (`adversarial.rs:1943-1951`), and
  alternatives must stay inside the slot's **approved provider envelope**
  (`adversarial.rs:2093-2112`). So a provider-wide 429 must not blindly advance — the next
  entry may sit on the same dead provider, and leaving the envelope violates the approved
  panel.
- `plan`: process failure returns immediately **without** advancing
  (`plan_job.rs:1727-1741`, `1973-1988`, `2350-2366`). Author fallback happens only on
  *pre-call* eligibility loss (`1663-1685`), and a **bound** peer losing eligibility ends
  the run `Blocked` (`1915-1922`), never a fallback.

So the runner supplies the four actions; the *stage* declares which outcome maps to which,
including whether advancing is legal at all. A global rule would silently change both
engines' behavior. `RetrySameCandidate` ≙ schema repair is the one mapping both share.

The union of abandon reasons the runner must express: process/spawn failure; schema or
parse failure; eligibility loss mid-run; budget, revision, or attempt-cap exhaustion; and
external-state drift since approval. Neither engine has a stale-claim reclaim concept —
that is `work`-only and comes from `dispatch_cycle`.

### `Stage`

A snake-case stage id plus its own pinned candidate pool, attempt budget, **concurrency**,
and **isolation**. Stages are how multi-call jobs fit one engine.

`plan` already models this durably and correctly — reuse it rather than inventing a
parallel vocabulary: `PlanStage` (`run.rs:280-284`) is `Planner | PeerReview |
SecondOpinion`, and `PlanProgress` (`run.rs:487-524`) is a tagged transition system
`Blocked → Prepared → Authoring → AwaitingPeer → {Revising ⇄ AwaitingPeer} →
AwaitingSecondOpinion → Terminal`. Generalize that shape; do not replace it.

**Concurrency is not optional.** `review` runs its reviewer slots in thread-scoped batches
(`adversarial.rs:998`, `parallel` default 3). A strictly sequential runner cannot host the
review job. A stage therefore declares `concurrency: NonZeroUsize`; `work`, `plan`, and
`consult` declare 1.

**Isolation is per stage, not per job.** `plan` wraps a disposable worktree around **every**
model invocation — authoring (`plan_job.rs:1717`), its repair (`1779`), peer review
(`1958`, `2019`), revision (`2147`, `2202`), and second opinion (`2337`, `2395`), all via
`with_isolated_worktree:3047` (`git worktree add --detach`, run, unconditional `--force`
removal). Eight call sites, not the author alone, as an earlier draft claimed. `review` and
`consult` use none; `work` uses none under D1.

### Stage constraints — how live recheck stays out of the policy

The seam's hardest case: both engines re-check eligibility against **live** provider state
at selection time, not just at approval time. `plan` calls `recheck_author` / `recheck`
against a fresh Musterroll snapshot (`plan_job.rs:2931`); `review` re-walks its judge chain
via `select_rechecked_judge` (`adversarial.rs:1224`) at synthesis time. If that logic lived
in the policy, policies would need live roster access and would stop being pure.

**Resolution: the pool is authorization, the recheck is eligibility, and both belong to the
runner. The policy supplies only declarative constraints, carried as data on the `Stage`.**

`plan` already has exactly the right vocabulary — reuse `PlanStageConstraints`
(`run.rs:301-306`), generalized off `PlanStage`:

```
distinct_execution_from: Vec<StageId>   // this stage must not reuse those stages' executions
tier_at_least:           Vec<StageId>   // must be >= those stages' tiers
diversity:               None | CrossOrDegraded | PairwiseDistinct
```

The runner then: takes the stage's pinned candidate pool (authorization), filters by live
eligibility (a fresh snapshot), applies the constraints, and picks in pinned order. First
eligible wins — no re-scoring. `CrossOrDegraded` falls back to same-group only when no
cross-group candidate is alive; `PairwiseDistinct` has **no** degraded path and ends
`Blocked` when it cannot be satisfied (`run.rs:294-296`).

### D2 applies here too — a second site

`PlanProviderDiversity` (`run.rs:288-297`) is a **second** implementation of the
provider-diversity rule that `decisions.md [2026-07-28]` amends, alongside
`adversarial.rs:565-602`. A spec's author, peer, and second opinion must be pairwise
distinct; under D2 that means pairwise-distinct **model families**, not provider lanes.

`conductor-ao8` as filed names only the review panel. It must cover both sites, or `plan`
keeps selecting `ollama-cloud/glm-5.2` as a "distinct" peer for an
`opencode-go/glm-5.2` author. The constraint field is renamed accordingly, and both sites
read family from the Musterroll `model_family` field — never by parsing `ProfileId`.

### `CallBudget`

`review` enforces a per-run model-call ceiling with an atomic counter that fails closed
once exhausted (`ReviewerCallBudget`, `adversarial.rs:158-189`); the worst-case formula
`reviewer_count * (REPAIR_RETRIES + 1) + 1` is at `adversarial.rs:868-871`. The runner owns
this for every job — a stage's attempt budget alone does not bound total spend across a
fan-out.

`JobPolicy::call_budget(stage_plan)` derives it. Only `review`'s formula exists today;
`work`, `plan`, and `consult` need one stated rather than inferred. Default:
`sum over stages of (slots x chain_length x attempts_per_candidate)`.

## The ports

Four traits. Everything else is a concrete type.

### `BeadGateway`

**`LoopClaim` (`loop.rs:132`) cannot claim — it has only `release` and `close`.** Both
reviewers independently flagged that claiming outside the kernel places a mutation before
the durable boundary and leaves resume unable to prove ownership.

```
show(repo, bead) -> Issue          // the worker prompt needs the body; the kernel never had this
claim(repo, bead, owner) -> ()
release(repo, bead, reason) -> ()
close(repo, bead, reason) -> ()
comment(repo, bead, text) -> ()
```

The **runner** calls these, inside its durable boundary. Jobs never touch bd.
`JobPolicy::claims_bead()` declares participation; only `work` returns true.

Implement over the existing `BdClient` (`bd.rs:154`) — do not write a second bd client.

**`conductor-moe` is FIXED (`b88da79`) and the gateway must inherit it.** `release` used to
run `bd update --status open --assignee ""` unconditionally, silently reopening work an
operator closed by hand mid-run. `BdClient::release_owned(repo, id, expected_assignee)` now
re-fetches immediately before mutating, no-ops when already open+unassigned, and otherwise
**fails closed** with a diagnostic naming expected vs observed state.

`BeadGateway::release` must route through `release_owned`, never the raw primitive.
Investigation proved bd 1.1.0 exposes **no** compare-and-swap: `bd update` has no
conditional flag, `--claim` is atomic only in the claiming direction, `bd batch` cannot mix
reads and writes, and `bd show` carries no version counter. So the residual window — two
back-to-back `bd` subprocess calls with no intervening I/O — is irreducible at this layer.
ADR in `decisions.md [2026-07-28]`. Do not attempt to close it with raw `bd sql`; that
bypasses bd's audit trail and was explicitly rejected.

### `AttemptExecutor`

Posture-selected. The mutating half exists; the read-only half needs widening.

- `RepositoryWrite` → `run_with_heartbeat` (`dispatch.rs:513`), which preflights HEAD, runs
  the hooks, spawns, heartbeats, and authenticates the commit. Returns `DispatchResult`
  with stdout/stderr paths and byte counts. Use as-is.
- `ReadOnly` → `run_readonly` (`dispatch.rs:490`) is **not sufficient as written**, though
  the precise defect is narrower than it first appears: output *is* captured, to
  `SpawnRequest.stdout_path` / `stderr_path` (`dispatch.rs:248-249`). What it discards is
  the *return* — `Result<()>` hands back no paths, byte counts, or status detail, so a
  caller cannot classify the attempt. Widen the return to `DispatchResult`'s shape, or add
  a sibling that does. **Do not add capture that already exists.** Note that neither
  `adversarial` nor `plan_job` calls `run_readonly` today — both drive `exec.spawn`
  directly; deciding which becomes the one read-only path is Phase 1b's call, and either
  is acceptable so long as there is exactly one.
- Both paths, `ReadOnly` only: a mandatory post-attempt HEAD/index/worktree check. A
  read-only job that mutated its repo is an *infrastructure failure*, never a result.
  Mirror the check shipped in `8a8f1fe`.

`MutationPosture` (`job.rs:43`) already declares `Work => RepositoryWrite`,
`Review|Consult|Plan => ReadOnly`. Nothing enforces it today. The runner enforces it.

### `JobPolicy`

The only per-job seam.

```
job()                     -> RunJob
posture()                 -> MutationPosture
claims_bead()             -> bool               // work only; verified neither plan nor review touches bd
requires_pinned_roster()  -> bool               // false only for the bootstrap probe
revalidation_digests()    -> &[DigestKind]      // which digests this job re-checks per stage
call_budget(stage_plan)   -> CallBudget         // worst-case model calls for the whole run

next_stage(ledger)        -> Option<Stage>      // None ends the sequence
prompt(ctx)               -> PromptMaterial     // NOT SpawnRequest -- see below
classify_attempt(ctx, output) -> Option<AttemptOutcome>   // None = runner default
aggregate_stage(stage, slot_results) -> StageOutcome
transition(ledger, stage_outcome) -> Transition // durable progress; the reducer
terminal(ledger)          -> Terminal
```

**`prompt` must not return `SpawnRequest`.** `SpawnRequest` (`dispatch.rs:240-250`) carries
`cwd`, `env`, `stdout_path`, `stderr_path`, `sandbox_profile`, `worker_resource_limits`,
and `commit_receipt_socket`. Handing a policy that struct hands it process authority and
lets it bypass runner-enforced posture — directly contradicting the purity rule. The policy
returns **prompt and schema material only**; the `AttemptExecutor` constructs the trusted
spawn envelope from the candidate, the stage's target kind, and runner-owned paths.

**`transition` is the missing reducer.** `classify_attempt` returns an outcome and
`terminal` returns a verdict, but nothing turned `(progress, accepted output)` into new
*durable* progress. Plan needs exactly this: persist the peer's findings, move
`AwaitingPeer → Revising`, feed those findings plus the prior artifact to the same
immutable author, and return to peer review (`plan_job.rs:2104-2155`). Doing that by
mutating inside a policy would be neither durable nor pure. The policy computes the
transition; the runner persists it.

**`AttemptOutput` must carry a payload.** An `AttemptOutcome` alone cannot express what a
successful attempt produced: a Plan output is canonicalized and captured as JSON and
Markdown (`plan_job.rs:1829-1862`); a review stage retains typed `ReviewerResponse`s,
anonymizes them, and builds the judge prompt from them (`adversarial.rs:1160-1221`). The
output channel carries canonical bytes plus their `ArtifactRef`, and the runner hashes and
captures before the policy sees them.

**Domain verdict is not execution failure.** `ReviewerVerdict::NoGo` is a *valid result*,
and `review` stays `Complete` when synthesis is valid (`adversarial.rs:216-263`,
`1122-1129`); a rejected plan document is likewise a real verdict. An earlier draft listed
`VerdictRejected` as an `AttemptOutcome`, conflating the two. Attempt outcomes describe
*execution*; verdicts live in the payload and reach `Terminal` through `transition`.

**`AttemptContext` is the fix for the seam's biggest hole.** A first draft had
`prompt(stage, attempt)`, which cannot build the two prompts that matter most: the judge
prompt is assembled from the reviewers' outputs (`finalize_review:1045` →
`run_judge_attempt:1288`), the peer-review prompt needs the author's plan document
(`PlanProgress::AwaitingPeer.artifact`, `run.rs:503`), and a schema-repair prompt embeds
the *failed attempt's own stdout* (`reviewer_repair_prompt`). None of those are functions
of the stage and candidate alone.

```
AttemptContext = {
    stage, slot, attempt_index,
    candidate:            &ApprovedExecution,
    prior_stages:         &StageLedger,          // hash-pinned artifacts of completed stages
    prior_attempt_output: Option<&ArtifactRef>,  // Some() exactly on RetrySameCandidate
}
```

The ledger holds `ArtifactRef` (`run.rs:115`) — path plus sha256 — so a prompt is built
from *pinned* evidence, never from ambient state. The runner captures and hashes; the
policy reads.

**`next_stage(ledger)` replaces `stages(progress)`.** The `progress` parameter was
undefined, and plan's `Revising ⇄ AwaitingPeer` loop (`PlanProgress:487`) turns on the
*prior peer verdict*. The ledger carries completed stages, their `StageOutcome`s, and their
artifacts, so revise-versus-advance is a pure function of it. Plan's `PlanProgress` becomes
a projection the plan policy computes; the runner keeps one generic ledger.

A policy is **pure**: prompts in, classification and a verdict out. It never touches
`RunHandle`, bd, git, or a process — the runner owns all of those. This is what lets
`adversarial.rs` keep its self-enforced isolation invariant after migration (below).

`plan`'s revision cap (`RevisionLimit` 0..=3, `run.rs:390-413`), `review`'s minority
preservation and coverage invariants (`parse_judge_response:1372-1420`), and `consult`'s
evidence-or-gaps rule all live behind `stages` + `classify` + `terminal`. **If a policy
needs to reach past these, that is a finding to surface — not a reason to keep a second
engine.**

### `WorktreePort`

Not a `dispatch.rs` primitive — `plan_job` hand-rolls it (`with_isolated_worktree:3048`:
`git worktree add --detach <tmp> <head>`, run, then unconditional `--force` removal plus
`remove_dir_all`). A stage that declares isolation needs it, and it cannot live in a policy:
`Command::new(` and `git worktree` are both on `adversarial.rs`'s forbidden list.

```
create(repo, head) -> Worktree      // Drop removes, unconditionally, even on panic
```

Lift `with_isolated_worktree` into this port. `plan`'s author stage is its only v1 consumer.

### `Clock`

Injectable. One `ItemDeadline` (mirror `dispatch_cycle.rs:90`) is shared across worker,
mechanical verify, and qualitative review — not a per-phase timeout. Every phase checks
remaining time before starting.

## Durability and integrity

**`loop.json` is the anti-pattern to avoid.** It is written with plain write+rename and no
fsync (`loop.rs:537-546`); on read only its schema and target string are checked
(`loop.rs:526-534`); and a forged `terminal=completed` is trusted at `loop.rs:288-290`,
which would close a Bead with no attempt ever having run. That violates the fail-closed
artifact-hash invariant.

Requirements:

1. Every runner state write goes through `run::durable_atomic_replace` (`run.rs:3501`,
   logic in `durable_atomic_replace_with_observer:3505`), which fsyncs the file before
   rename and the parent afterward. `atomic_replace:3578` is its thin `Result` wrapper.
   There are two other hand-rolled atomic writers (`deck.rs:661`, `role_routing.rs:1805`);
   do not add a fourth.
2. The runner's state artifact is **hash-pinned in the run manifest**. A state file whose
   hash does not match is a fail-closed error, not a recoverable state.
3. A terminal state is only trusted if the corresponding `AttemptFinished` /
   `VerifyFinished` events exist in the append-only journal. Evidence is durable *before*
   the Bead mutation, and the Bead mutation is the last step.

## Concurrency

Fan-out interacts badly with the durable substrate, and a first draft of this contract got
it wrong. `append_event_line` (`run.rs:3589`) is **read-modify-write** — it reads the whole
journal, appends one line, and `atomic_replace`s the file. Its own doc comment states the
invariant: *"Run journals have one owning process."* Today `adversarial` respects this by
collecting attempts in memory and letting `cli.rs` serialize them after the join. An
earlier draft here said to delete that bridge and "emit events natively," which under
concurrency would have had N slots racing read-modify-write and **losing events**.

**The invariant is preserved, not violated. The runner is the sole writer.**

1. Concurrent slots return *results*; they never touch `RunHandle`, the state file, or the
   journal. The runner writes every event after the join, in deterministic slot order.
2. The per-run state file has exactly one writer for the same reason.
3. `CallBudget` is reserved before spawn through an atomic counter that fails closed once
   exhausted — mirror `ReviewerCallBudget` (`adversarial.rs:158-189`); its worst-case
   formula is at `adversarial.rs:868-871`. Without atomicity a fan-out double-spends.
4. **Resume across N workers.** The resume model below probes *a* process group; a
   concurrent stage has N. The run state records a worker identity per in-flight slot.
   Reclaim requires **every** one to be provably dead; any alive or unreadable → refuse.
5. **Dirty-tree attribution under fan-out.** N read-only slots share one checkout, so a
   mutation cannot be attributed to a slot. Therefore the read-only HEAD/index/worktree
   check runs **once at stage join**, not per slot: it proves the repo was unmutated across
   the stage. Failure fails the whole stage as an infrastructure error. That is sufficient
   and honest; per-slot attribution is not achievable and should not be claimed.

## Resume

`loop.rs`'s resume is not resumable: it refuses on any `worker_pgid` without checking
liveness (`loop.rs:291-295`); if a worker committed but crashed before verification it
resets to `Ready` and reruns the worker instead of verifying the existing commit
(`loop.rs:296-308`); and `validate_run_target` (`loop.rs:515-523`) binds only repo, bead,
and verifier.

Requirements:

1. **Resume authorization is immutable.** Re-read the pinned `ApprovedProfileEnvelope`
   (`run.rs:151`), limits, verifier, approval, and roster snapshot **from the manifest**,
   never from live config. A resume must not be able to dispatch a profile the manifest
   never approved.
2. **Check liveness, do not blanket-refuse.** Probe the recorded process group via the
   syscall path (`quarantine.rs:978-988`). Provably dead → reclaim. Alive → refuse.
   Unreadable → refuse (fail closed).
3. **Resume at the recorded phase.** A durably recorded worker commit resumes at
   verification, not at a fresh worker.
4. `conductor-47p` is in scope here: lease ownership stores and checks PID only
   (`quarantine.rs:651-666`), so one crash plus later PID reuse wedges resume forever.
   Owners bind to a process generation, not a bare PID.
5. Missing approval or roster snapshot when required is a **fail-closed refusal**. Verified:
   `RunHandle::create` currently appends a `CoverageGap` event with outcome
   `musterroll_roster_artifact_unavailable` and then returns `Ok(handle)`
   (`run.rs:1119-1131`) — it proceeds.

   **Do not fix this by making `RunHandle::create` refuse unconditionally.** The Phase 3
   bootstrap probe (`conductor-bxb`) exists precisely to run when roster eligibility cannot
   yet be established; a blanket refusal would make the deadlock permanent. The refusal
   belongs in the **runner**, conditioned on the *invocation*, not on a `JobPolicy`.

   **The probe cannot be a policy at all.** `RunJob` has exactly four variants
   (`run.rs:81-86`), the v1 spec states the probe is a preflight and never a fifth job, and
   it must append Musterroll evidence and re-snapshot the roster — mutations a pure policy
   may not perform. An earlier draft's `JobPolicy::requires_pinned_roster()` therefore had
   no legal implementation. The probe is a **runner preflight phase** with its own
   authorization, and `requires_pinned_roster` is a property of that phase.

## D1 obligations

Per `decisions.md [2026-07-28]`, `work` writes the repository directly; there is no attempt
checkout and no commit promotion. These are the compensating controls, and they are **not
optional** — without them D1 is a safety regression against `dispatch_cycle`, which
preflights both:

1. **Clean-tree preflight** before spawn, via `CommitProbe::is_clean` (`dispatch.rs:468`).
   Dirty target → fail closed before claiming. `loop.rs` never calls this in production.
2. **Quarantine adoption.** A failed attempt's partial work is captured via `quarantine`
   and carried into the next attempt as a **patch reference**, not the prototype's plain
   text (`LoopIteration.feedback`). `conductor-jum` is in scope: `quarantine.rs:321` passes
   survivor filenames into `git apply --exclude`, which glob-interprets them, so a survivor
   named `src/[id].ts` breaks reapply. D1 makes this path load-bearing.
3. **Post-verify recheck** of HEAD, tree, claim, and authorization before the Bead
   mutation. This currently lives in the promotion path and must be retained.

`plan` keeps its disposable isolated worktree — the consolidation spec assigns it one
("Disposable isolated worktree; no target mutation"). D1 governs `work` only.

### Isolation is CASE's scope, not Undertake's (user, 2026-07-28)

Adversarial review established that the three controls above buy **detection and
fail-closed refusal**, not isolation and rollback. An earlier draft called them sufficient;
they are not. The resolution is a scope boundary, not a stronger control set: **containment
belongs to CASE** (the Controlled Autonomy Safety Engine, `~/git/case`). See
`decisions.md [2026-07-28]`.

Undertake therefore owes **detection only**, and must not grow a containment layer back
under another name:

- a durable **pre-attempt HEAD checkpoint** written before spawn;
- on resume, canonical HEAD moved without a matching durable receipt ⇒ **fail closed**.
  Undertake declines to touch the repository and surfaces it for human resolution;
- clean-tree preflight and quarantine adoption of *uncommitted* work.

What Undertake does **not** attempt, and no Phase 1b design may reintroduce:

1. **An unauthenticated canonical commit has no recovery path.** A worker can commit and
   the parent can crash before the receipt and `AttemptFinished` checkpoint are durable.
   Canonical HEAD is then advanced by an unprovable commit. Dirty-tree quarantine refuses
   whenever HEAD moved from `before_head` (`quarantine.rs:357-365`), so it cannot capture
   this.
2. **Quarantine captures only uncommitted work** (`quarantine.rs:337-344`). Failed but
   *committed* work is detected after the fact and not restored.
3. **No isolation during execution.** Transient, unverified changes are visible to the
   human, editors, watchers, and builds. `RepoLease` is advisory among cooperating
   Undertake processes only; foreign Git activity is discovered after both actors have
   touched the tree.
4. **Ignored files are unprotected.** `CommitProbe::is_clean` runs
   `git status --porcelain --untracked-files=normal`, which does not see ignored-file
   changes. The isolated path copied only *declared* verification inputs; direct execution
   exposes every ignored file in the tree.
5. **"Patch reference" is evidence, not adoption.** The existing carry-forward records a
   path and hash and explicitly does not require the next worker to use it
   (`dispatch_cycle.rs:6827-6851`).

**Accepted residual, stated plainly.** CASE is unfrozen and unimplemented, so this gap is
unowned *today*, not merely delegated. Until CASE ships, a crash between a worker's commit
and its durable receipt can leave an unprovable commit on the branch that Undertake will
refuse to touch, resolved by hand. Ralph — in daily use — has the same exposure with **no
detection at all**, so this is not a new hazard and Undertake is strictly ahead of the
status quo. Closed as a scope decision; revisit only if CASE's scope changes.

## Plan creation — an earlier draft got this backwards

Draft 1 called `RunHandle::create`'s Plan refusal (`run.rs:1022`) a blocking prerequisite
and proposed making generic creation accept Plan. **Both halves were wrong.**

`RunHandle::create_plan(NewPlanRun)` already exists (`run.rs:1147`) and production uses it
(`plan_job.rs:752`). The generic refusal is a *guard*, not a gap: `NewRun` carries no
`PlanRunDetails`, approval, roster snapshot, or input bytes, so a Plan run built through it
would be structurally incomplete. Weakening the guard would discard required state.

**Retain and abstract `create_plan`.** The runner's creation seam admits a job-specific
constructor; it does not flatten four jobs into one `NewRun`.

## Target kinds — not every job has a repository

The runner pseudocode's unconditional `RepoLease` and `CommitProbe::is_clean` cannot host
`review`, which is **artifact-targeted**: `cli.rs:1004` puts `artifact_source_path()` into
`RunTarget.repo`. That field is a target label, not necessarily a Git working tree.

A `Stage` declares its target kind, and the runner conditions on it:

| Target kind | Repo lease | `is_clean` preflight | Git postcheck | Jobs |
|---|---|---|---|---|
| `GitWorkingTree` | yes | yes | yes | `work` |
| `GitWorktreeIsolated` | yes, on the parent | no (fresh checkout) | no | `plan` |
| `ArtifactOnly` | **no** | no | no | `review`, `consult` |

Taking a repo lease for artifact-only review would needlessly serialize unrelated
read-only work *and* contradict `adversarial.rs`'s no-git invariant. An earlier draft
required it globally while simultaneously claiming that invariant survives; those cannot
both hold.

## Migration hazard: `adversarial.rs` enforces its own isolation

`adversarial.rs` **never touches `RunHandle`** — it has no `crate::run::` reference at all.
Its durable event bookkeeping is bolted on externally by `cli.rs`
(`record_adversarial_reviewer_events:1089`, `record_adversarial_terminal_events:1130`),
which translates its in-memory `ReviewerAttempt` / `JudgeAttempt` into `AttemptStarted` /
`AttemptFinished` / `CoverageGap` / `ReviewFinished` and calls `finish(...)`.

This is enforced by a production-code string scan (`adversarial.rs:3172-3193`) asserting
the module — excluding its test block — contains none of **eleven** strings
(`adversarial.rs:3178-3190`): `"crate::bd::"`, `"crate::cycle::"`,
`"crate::dispatch_cycle::"`, `"crate::verify::"`, `"CommandExec"`, `"GitCommitProbe"`,
`"run_dispatch_cycle"`, `"std::process::Command"`, `"Command::new("`, `"git worktree"`,
`"chezmoi apply"`. Preserve **all eleven** — an implementer working from a truncated list
would silently weaken the invariant.

A naive migration either trips that test or silently drops the event bridge.

**Resolution — the invariant gets stronger, not relaxed.** The runner owns *all* durable
writes and *all* mutation authority. A `JobPolicy` is pure: it builds prompts, classifies
output, and decides a terminal verdict. It never touches `RunHandle`, bd, git, or a
process. So `adversarial.rs` becomes a policy that still satisfies its own scan, and
`cli.rs`'s bridging layer is **deleted** rather than preserved — the runner emits those
events natively and uniformly for all four jobs.

Extend the forbidden list with `"crate::run::"` when the migration lands. If a policy ever
needs one of those seams, that is a finding to surface, not a reason to widen the list.

## Approval must be re-validated per stage

The two engines disagree, and the resumable runner needs the stricter model.

- `plan_job` re-validates on **every** `dispatch()` call, which is what makes it resumable
  across stages: it re-checks target HEAD and status (`1573`), input sha256 (`1580`),
  roster policy sha256 (`1592`), and scheduler policy digest (`1597`) before any model
  call, plus a deck response strictly after the approval watermark (`2914`).
- `adversarial` authorizes **once** in `authorize_approved_execution:942` and then runs to
  completion in one process. There is no later re-check.

The runner adopts plan's model for all jobs: re-validate the pinned approval and the job's
declared digests at each stage boundary. Drift since approval is a fail-closed terminal,
never a retry. `review` gains resumability it does not have today.

"Every policy digest" is too vague to build from — the sets genuinely differ, so
`JobPolicy::revalidation_digests()` declares them:

| Job | Digests re-checked per stage |
|---|---|
| `plan` | `target_head`, `target_status`, `target_sha256`, `roster_policy_sha256`, `scheduler_policy_sha256` (`plan_job.rs:1573,1580,1592,1597`) plus a deck response strictly after `approval_watermark` (`2914`) |
| `review` | `plan_sha256`, `roster_sha256` (`authorize_approved_execution:942`) |
| `work` | `target_head`, bead status/claim ownership, `roster_policy_sha256` |
| `consult` | `roster_policy_sha256` |

Extend the enum rather than adding a job-specific escape hatch.

Two beads were deferred as standalone items but their requirements land here:

- `conductor-2d1` — pre-spawn worker-group invalidation must not silently revert to a
  no-op default. `WorkerHooks::on_pre_spawn` (`dispatch.rs:435`) already returns `Err` to
  prevent hook creation and spawn; the runner must not supply a permissive default.
- `conductor-74d` — a fail-closed refusal must be legible from the runner's terminal
  reporting, not require reading raw state files.

## Non-goals

A workflow DSL. A plugin surface. A second telemetry store, lease implementation, or
atomic-write helper — three of the last already exist. Redesigning `dispatch.rs`'s
primitives, which are sound. Any per-job escape hatch past `JobPolicy`.

## The genericity is not there yet — and that reorders Phase 1

Five design questions were resolved and each adversarially verified against source. **All
five resolutions failed verification**, with 61 defects and 53 citation corrections. They
failed the *same way*, and that convergence is the finding:

> Every resolution assumed the existing machinery is more generic than it is. It is not.
> Below the process primitives, this codebase is four job-specific implementations.

Verified instances:

| Assumed generic | Actually | Evidence |
|---|---|---|
| `role_routing` is job-agnostic | It parses **plan manifests** — reads `job == "plan"` and `/details/state/progress/state == "terminal"` | `role_routing.rs:1335`, `1341-1343` |
| Terminal reconciliation is generic | `reconcile_terminal_manifest` mutates only `RunDetails::Work` | `run.rs:2262-2288`, esp. `2272-2274` |
| Spawn identity hooks exist for all attempts | `on_pre_spawn`/`on_spawn` fire **only** from `run_with_heartbeat` (the write path). `run_readonly` takes no hooks at all — so `review`, the one fan-out job, has no spawn identity | `dispatch.rs:544`, `568`, `490-511` |
| Terminal write order is uniform | `work` writes event-then-manifest; **every** plan terminal path writes manifest-then-event, at six sites | `run.rs:1764`, `1863-1889`, `1971-1988`, `2008-2013`, `2022-2028`, `2039-2044` |
| Invocation evidence can be generalized in place | `RunEvent` is `deny_unknown_fields` and `read_events` fails closed, so renaming `plan_invocation` makes **every existing plan journal permanently unresumable** | `run.rs:872`, `3162-3168` |
| Attempt evidence already discriminates model calls | Only `plan` attaches it. `work` and `review` pass `..EventInput::default()`, so a reconstructed budget reads **0** for them | `dispatch_cycle.rs:6011-6019`, `cli.rs:1094-1102` |
| Group liveness + PID-recycle check compose | `process_group_alive` succeeds if **any** member lives; `kernel_process_identity` probes only the leader. Orphaned descendants defeat the pairing | `quarantine.rs:969,978-989`, `dispatch.rs:1533-1569` |

**Consequence: Phase 1b is not one extraction.** The genericity must be *built* first, and
each piece is an independently verifiable refactor against the engines that exist today —
far safer than discovering these while a half-built runner is in the tree.

### Phase 1b-prep — sequenced before the extraction

Each item stands alone, is verified by the existing suite plus the frozen parity corpus,
and reintroduces no isolation (CASE owns that).

1. **`undertake/event@3`.** Generic invocation evidence carrying stage, slot, attempt, and
   retry lineage. `@2` journals must stay readable — a rename is not available under
   `deny_unknown_fields`, so this is an additive `@3` with a compatibility read path.
   Without it, fan-out attempts cannot be correlated after a crash and no budget can be
   reconstructed.
2. **Job-generic terminal reconciliation.** `reconcile_terminal_manifest` handles all four
   `RunDetails`, and the terminal write order is unified. Today plan's six manifest-first
   sites can leave `progress = Terminal` with `lifecycle = Running` and no terminal event,
   which `open()` passes as resumable and nothing repairs (`run.rs:1317-1323`).
3. **Spawn identity on the read-only path.** `run_readonly` gains the `WorkerHooks`
   parameter and returns `DispatchResult`'s shape. This is the prerequisite for both
   fan-out resume and read-only attempt classification.
4. **Per-slot worker identity.** `WorkState.worker_pgid` (`run.rs:245-261`) becomes a set,
   and reclaim requires **every** recorded group provably dead. Pair it with a
   leader-vs-descendant-safe liveness rule, since the two current probes do not compose.
5. **Decouple `role_routing` from plan.** It must stop parsing plan manifests before it can
   serve a generic runner.

Only then does `conductor-mkct` extract the runner onto genuinely generic substrate.

## Open questions — close these before `conductor-mkct` starts

1. ~~D1's residual risk~~ — **CLOSED 2026-07-28 (user).** Isolation is out of Undertake's
   scope entirely; CASE owns containment. Undertake owes detection only. See
   `decisions.md [2026-07-28]` and § Isolation is CASE's scope.
2. **Durable call budget.** Direction confirmed — reconstruct from durable
   `AttemptStarted` records rather than persisting a counter, compared against the ceiling
   already pinned in `RunLimits.max_attempts` (`run.rs:162`), which every job writes today
   and no production code reads. Reserve-never-refund: after a crash you cannot know
   whether the spawn happened, so over-counting is the only safe direction.
   **Blocked on prep 1** — the discriminator does not exist until `work` and `review` also
   attach invocation evidence.
3. **The two-file commit.** Direction confirmed — the append-only journal is the source of
   truth and mutable state is a rebuildable projection, mirroring the Afterfact SQLite
   posture. **Blocked on prep 2** — the rule cannot be stated while plan writes
   manifest-first at six sites and reconciliation handles only `Work`.

Both are now sequencing consequences rather than open design questions. The remaining
genuine unknowns are inside the prep items above.

## Acceptance

A Senior can implement `conductor-mkct` from this document without making a design
decision. Every trait named here has a stated home, every requirement cites the source it
derives from, and no signature is prescribed that was not read first.

## Verify

Human review of this document by a Lead of a different model family from the author
(Anthropic), per `AGENTS.md`. No code change in this bead.
