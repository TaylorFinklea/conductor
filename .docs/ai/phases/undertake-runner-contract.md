# The generic attempt runner — contract

**Status**: draft for review (Opus 5, 2026-07-28). Bead `conductor-y6kv` (v1 Phase 1a).
Implements the design half of `undertake-v1-finish-spec.md` Phase 1. `conductor-mkct`
(Phase 1b) implements it; a Senior should be able to build from this without further
design decisions.

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

Both existing engines already draw exactly this line, which is evidence the seam is in the
right place: a reviewer slot answers `InvalidSchema` with a same-model repair and
`ProcessFailed` with the next chain entry, mutually exclusive, at most two attempts
(`adversarial.rs:1913-1951`); `plan` answers a schema failure with one same-author repair
in a fresh worktree (`plan_job.rs:1744-1827`) and an eligibility loss by walking to the
next pinned candidate. So `RetrySameCandidate` ≙ schema repair, `AdvanceCandidate` ≙
process/eligibility failure.

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

**Isolation is per stage, not per job.** `plan` creates and destroys a worktree around each
*author* invocation (`with_isolated_worktree:3047-3085` — `git worktree add --detach`, run,
then unconditional `--force` removal), not across the run. `review` uses none. `work` uses
none under D1. So a stage declares whether it runs in a disposable worktree.

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

**`conductor-moe` applies here**: `release` currently runs `bd update --status open
--assignee ""` unconditionally (`bd.rs:235-238`), which reopens work an operator closed by
hand mid-run. Release must be conditional on Undertake still holding the claim.

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
prompt(ctx)               -> SpawnRequest
classify_attempt(ctx, output) -> Option<AttemptOutcome>   // None = runner default
aggregate_stage(stage, slot_results) -> StageOutcome
terminal(ledger)          -> Terminal
```

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
   belongs in the **runner**, conditioned on `JobPolicy`: a policy declares whether a
   pinned roster snapshot is required, and the runner refuses when a requiring policy lacks
   one. The probe's policy declares it is not required, and its coverage gap stays a
   coverage gap.

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

## Blocking prerequisite for `plan`

`RunHandle::create` refuses Plan runs outright: `run.rs:1021-1025` returns
`Err("plan runs require explicit structural PlanRunDetails and are not activated")`.
Generic run creation must accept Plan before Phase 4c is possible. Resolve this in Phase
1b, not at migration time.

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

## Acceptance

A Senior can implement `conductor-mkct` from this document without making a design
decision. Every trait named here has a stated home, every requirement cites the source it
derives from, and no signature is prescribed that was not read first.

## Verify

Human review of this document by a Lead of a different model family from the author
(Anthropic), per `AGENTS.md`. No code change in this bead.
