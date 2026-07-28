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

```
AttemptRunner::run(policy, ports, request) ->  Terminal

  acquire RepoLease
  preflight:  auth_readiness -> is_clean -> approval revalidation -> bead claim
  for each stage the policy yields:
      for each attempt within the stage's budget:
          select next candidate from the stage's PINNED pool
          execute (posture-selected: mutating | read-only)
          classify -> Accept | RetrySameCandidate | AdvanceCandidate | Fatal
      verify (mechanical, then optional qualitative)
  terminal: policy.terminal(...) -> close | release, then durable evidence, then Bead mutation
```

Everything above the `policy.` calls is the runner's, identical for all four jobs.

## Types to define

### `Terminal` — replaces `Completed | Failed`

`loop.rs:137-141` offers only two outcomes. The approved contracts need more: `plan` ends
`needs_input` on unresolved open questions and `blocked` on loss of a required legal
candidate; `review` ends `blocked` when a required reviewer is unavailable (`conductor-koi`
shipped this as a distinct gap); `consult` returns evidence-or-gaps.

```
Terminal = Completed | Failed | Blocked { reason } | NeedsInput { reason } | Canceled
```

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

### `Stage`

A snake-case stage id plus its own pinned candidate pool and attempt budget. `work` has
one stage; `review` has reviewer stages plus a judge stage; `plan` has author, peer_review,
revision, second_opinion. Stages are how multi-call jobs fit one engine.

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
- `ReadOnly` → `run_readonly` (`dispatch.rs:490`) is **not sufficient as written**: it
  returns `Result<()>`, so it reports only pass/fail and discards the output. For `review`,
  `consult`, and `plan` the output *is* the result — the verdict, the envelope, the plan
  document. Widen it to return the same captured-artifact shape as `DispatchResult`
  (stdout/stderr paths plus byte counts), or add a sibling that does. Check what
  `adversarial.rs` and `plan_job.rs` do for capture today and lift that rather than
  inventing a third mechanism.
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
claims_bead()             -> bool
requires_pinned_roster()  -> bool               // false only for the bootstrap probe
stages(progress)          -> Option<Stage>      // None ends the stage sequence
prompt(stage, attempt)    -> SpawnRequest       // built from the pinned candidate
classify(stage, output)   -> Option<AttemptOutcome>  // job-specific reading; None = runner default
terminal(evidence)        -> Terminal
```

`plan`'s revision cap, `review`'s minority preservation, and `consult`'s evidence-or-gaps
rule all live behind `stages` + `classify` + `terminal`. **If a policy needs to reach past
these, that is a finding to surface — not a reason to keep a second engine.**

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

1. Every runner state write goes through `run::durable_atomic_replace`
   (`run.rs:3578-3585`), which fsyncs the file before rename and the parent afterward.
   There are two other hand-rolled atomic writers (`deck.rs:661`, `role_routing.rs:1805`);
   do not add a fourth.
2. The runner's state artifact is **hash-pinned in the run manifest**. A state file whose
   hash does not match is a fail-closed error, not a recoverable state.
3. A terminal state is only trusted if the corresponding `AttemptFinished` /
   `VerifyFinished` events exist in the append-only journal. Evidence is durable *before*
   the Bead mutation, and the Bead mutation is the last step.

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

## Folded-in requirements

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
