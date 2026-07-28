# Decisions

> Architecture decision records. Append-only — one entry per decision.

## [2026-07-01] Rust for the conductor binary

**Context**: Runtime choice delegated by user ("I love rust but decide for me"). Precedents: Go stdlib-only (harness-deck), Rust (larkline).
**Decision**: Rust, mirroring larkline's discipline (unsafe-forbid, LTO release profile, minimal deps: serde/serde_json; no tokio in v1 — dispatch is budget-bounded and serialized per repo, plain threads suffice).
**Alternatives considered**: Go stdlib-only (shares shape with harness-deck, incl. its unmerged Go bd client); TypeScript/Bun atop orchestra.
**Rationale**: User joy on a personal tool they'll maintain; larkline is their proven playbook for exactly this binary shape; the two biggest fleet backlogs (tesela, larkline) are Rust so the implementer fleet demonstrably works in Rust here; the correctness-critical triage core table-drives well under cargo test. Go's only unique edge (reusing harness-deck's bd client) is a small read contract, cheap to reimplement.

## [2026-07-01] Thin composer over bd-native or orchestra-superset

**Context**: Conductor must compose bd, ralph-era backends, orchestra, harness-deck.
**Decision**: Single binary delegating everything to existing tools over subprocess/file contracts; Conductor owns only routing, gates, budgets, serialization, state. Do NOT wrap ralph (Plan-file-scoped, ambiguous exit codes) — invoke backends directly using ralph's proven argv idioms. Do NOT use bd swarm/gate/mol in v1 (unverified semantics).
**Alternatives considered**: bd-native (drive swarm/gate/mol); growing orchestra (TS/Bun) into the conductor.
**Rationale**: Every component already speaks exit-codes/files; the missing piece is exactly the translation layer. bd primitives solve DAG-state, not routing/gates/budgets. orchestra stays a small leaf oracle per its own spec.

## [2026-07-01] Roster is config, scorecard is upstream

**Context**: The live model roster lives in `~/.claude/model-scorecard.md` — session-edited markdown prose.
**Decision**: `conductor.toml` carries the authoritative closed roster (dispatch IDs, tier, ceiling, efficiency); `conductor roster drift` diffs against the scorecard table and warns, never auto-edits.
**Rationale**: Ratchet auto-dispatch is only sound if triage is deterministic and reproducible from config + bead fields; parsing session-owned prose in the dispatch path would let routing silently shift between approval and execution. Also: orchestra's own DEFAULT_MODEL (kimi-k2.7-code) going stale vs the roster is the cautionary tale — Conductor always passes `--model` explicitly.

## [2026-07-01] Routing fields move to bd structured metadata, approval-gated

**Context**: tier_floor/complexity exist today as notes-prose on ~8/231 items; bd has an unused structured-metadata mechanism.
**Decision**: Read metadata first, notes-prose fallback (ranges like `S-M` → upper bound). Conductor may write fields via `bd set-metadata` only after the user approves triage suggestions in a cycle report (user-selected). New canonical keys: `tier_floor`, `complexity`, `verify_cmd`.
**Rationale**: Metadata is queryable/machine-native; prose is fragile. Approval gate keeps fail-closed posture — a mis-triage would otherwise silently steer future auto-dispatch.

## [2026-07-01] hermes-voice and larkline are out of v1

**Context**: User asked whether harness-voice/hermes-voice or larkline belong in Conductor.
**Decision**: Neither is a v1 component. hermes-voice (mid-rebrand to "Harness Voice") is a shipped personal voice UX surface — future (v2+) consumer of conductor events via a thin webhook, never a dispatch backend. larkline is precedent + free display: publishing harness-deck reports with live heartbeats makes conductor state visible in lark-plug-hdeck's "In Flight" view with zero larkline-specific code (its liveness window is 60s — heartbeat faster than that).
**Rationale**: Recon showed neither has any orchestration surface; integration seams are events/reports they already consume.

## [2026-07-01] Conductor joins the Guildhall suite; two reconciliation additions

**Context**: Conductor is the "master of works" member of the Guildhall suite (charter: `~/git/guildhall`). Two suite-level decisions (rationale in `guildhall/.docs/ai/decisions.md`) add scope to Conductor's backlog.
**Decision**:
1. **Tiered qualitative-review stage** (`conductor-review` bead) — an optional, config-gated pipeline stage after mechanical verify: junior-tier work gets a senior read-only review, senior work optionally a lead review, returning ship|revise + findings. Mirrors what the Lead session did by hand in cycle 1 (caught the `.gitignore` landmine, the agy no-op, evidence quality — none catchable by `verify_cmd`). Enforces `~/AGENTS.md`'s "review only by an equal-or-higher tier" inside the orchestrator. Config `review.enabled` (default true) + `review.min_tier_gap`; one extra dispatch per lower-tier completion, budget-counted.
2. **Bursar budget interface** (`conductor-bursar` bead) — consume `bursar status --json` before metered external dispatch; near-exhausted or "unknown" provider windows down-weight/defer (fail-closed: unknown = spend-cautiously). Retires the static-cap limitation; gives orchestra's dormant `ThrottleState`/`routeBoundary` a real data source via Bursar.
**Alternatives considered**: bake review into the existing m4b verify pipeline (rejected — keep mechanical vs qualitative separable/testable); leave budgets static (rejected — cycle 1 showed real quota exhaustion, agy gemini-flash down ~4.6 days).
**Rationale**: Cycle 1 was Conductor's own design run by hand; both additions crystallize what the manual Lead loop actually did. Cross-member dependency (Bursar must ship first) is noted in bead prose — bd has no cross-repo dep primitive.

## [2026-07-04] Arena mode deliberately routes through Ralph

**Context**: The v1 conductor dispatch path intentionally bypasses Ralph because ordinary fleet dispatch should own backend argv, budgets, and verify/close semantics directly. Arena has a different product question: compare how harnesses use the same model/prompt on the same bead.
**Decision**: Add a separate `conductor arena run` path that creates isolated worktrees, writes byte-identical `.docs/ai/current-state.md`/`loop-prompt.md`, invokes `ralph -n 1 -t <harness>` with model env vars, judges anonymized candidate diffs, and only cherry-picks a strict winner. This does not change the normal cycle/dispatch runner.
**Rationale**: Direct backend dispatch would measure model output while collapsing away the harness variable. Ralph is the existing cross-harness loop contract, so Arena must use it to compare Codex/Pi/OpenCode harness behavior fairly. The apply gate remains Conductor-owned: objective verify, unique safe winner, score threshold, clean worktrees, and real-repo HEAD/clean checks before cherry-pick.

## [2026-07-06] Audit-first roster/router refactor

**Context**: User wants Conductor roster management and routing to be inspectable instead of a black box, while preserving deliberate use of non-Claude models for both cheap offload and outside-perspective/adversarial review. NeuralWatt/Ollama lanes may be valuable fallback capacity even when Bursar has no live telemetry for them.
**Decision**: Keep `conductor.toml` canonical and hand-edited; add read-only validation/explain/dashboard surfaces first. Add explicit provider-outlook policy in config for no-telemetry lanes, explicit bead metadata for `routing_intent` and `provider_risk`, and full per-item candidate audit tables. Phase 1 must not change model selection; later phases may let intent/provider outlook reorder eligible same-tier candidates, with live signals labeled separately from declared policy.
**Alternatives considered**: split roster into a separate config file now; generate config from `~/.claude/model-scorecard.md`; implement behavior-changing provider-aware routing first; infer risk/intent from bead prose.
**Rationale**: The previous closed-roster ADR remains sound: deterministic dispatch must not depend on mutable prose parsing. Audit-first rollout lets humans inspect and tune policy before it changes dispatch behavior. Explicit intent prevents cheapest-model routing from erasing the useful “different model perspective” workflow, and explicit provider outlook avoids inventing telemetry while still making fallback-provider preference reviewable.

## [2026-07-09] GPT-5.6 uses direct Codex dispatch with explicit effort

**Context**: GPT-5.6 Sol, Terra, and Luna expose Codex reasoning levels that Pi cannot faithfully carry. Their chosen effort changes the capability band, especially for Luna.
**Decision**: Add `backend = "codex"` and require `reasoning_effort` on every Codex roster row, Arena profile, and Arena judge. Dispatch invokes `codex exec --model <id> --config model_reasoning_effort=\"<effort>\"`, never inheriting a local global setting. Sol is Lead/XL at `max`; Terra is Lead/XL at `xhigh`; Luna has stable Junior/S `medium` and Senior/L `high` roster rows. Luna accepts through `max` but rejects `ultra`; Sol and Terra accept all closed effort values through `ultra`. Codex counts against the existing metered-external cap and uses Bursar's `codex` provider key.
**Alternatives considered**: Route GPT-5.6 through Pi; use one global Codex effort; represent Luna variants with parenthetical display labels.
**Rationale**: Pi's thinking grammar cannot express the new `max`/`ultra` options, global settings make runs non-reproducible, and parenthetical labels collapse under scorecard normalization. Distinct stable Luna names plus an explicit Reasoning drift column keep routing, Arena, ledger, and scorecard evidence auditable.

## [2026-07-13] Provider state is fail-closed at plan and dispatch

**Context**: Bursar status was checked only at dispatch, missing Bursar fell
back to static caps, and a persisted 429 with no percentage could still be
retried.
**Decision**: Consume only Bursar status@2. Exhausted, unknown, missing,
malformed, stale, and unsupported status defer when Bursar is enabled;
`use_bursar=false` is the sole explicit static-caps override. Persist provider
decisions in plans, recheck before launch, and write classified runtime 429s
back before fallback. Details: `phases/provider-trust-integration-spec.md`.
**Alternatives considered**: Keep late warnings; fail open for unknown; encode
quota guesses in roster policy.
**Rationale**: Dispatch trust depends on provider truth being part of the
approved route. Explicit static mode remains available without letting missing
infrastructure silently change policy.

## [2026-07-13] Adversarial review is an isolated N-plus-one Conductor workflow

**Context**: Cross-provider architecture critiques were valuable but required
repeated prompts and ad-hoc model selection. Putting the logic in a skill would
duplicate Conductor's roster, provider, approval, ledger, and report policy.
**Decision**: Add a separate read-only `adversarial-review` command: N Senior
or Lead reviewers on N distinct providers plus one additional Lead judge. It
shares only closed-roster/provider/report/ledger primitives with Conductor and
does no cycle, bd, git, worktree, or apply operation. The approval pins the
artifact hash, panel, fallbacks, judge, and limits. Details:
`phases/adversarial-design-review-spec.md`.
**Alternatives considered**: Prose-only cross-harness skill; separate review
driver; fold review into normal cycle or Arena.
**Rationale**: A dedicated command is independently testable and inspectable
without creating a second router or increasing the normal cycle's black-box
surface.

## [2026-07-13] Approval scope is persisted and cannot widen at dispatch

**Context**: `conductor-xa5` showed that one fleet-wide approval could launch
every proposal observed under `~/git`.
**Decision**: Unscoped approval may launch only the existing dispatch bucket.
Explicit repo/item selectors are persisted in the plan and approval may cover
proposals only inside that immutable scope. Dispatch cannot add selectors or
substitute items. Each authorized item carries a SHA-256 digest over a
deterministically serialized, ordered input record. Use the in-process
`sha2 = "0.10"` crate. Details: `phases/bounded-dispatch-approval-spec.md`.
**Alternatives considered**: Keep blanket approval; parse free-form approval
notes; add dispatch-time selectors that were not part of the plan; use
process-dependent standard hashing; shell out to `shasum`.
**Rationale**: An approval is meaningful only when its maximum blast radius is
visible and immutable before the user grants it. Standard hashing is not a
stable cross-process contract, and a subprocess would add platform and PATH
failure modes to a correctness boundary.

## [2026-07-13] Roster enablement is config-level, with a provider gate

**Context**: Taking a model or a whole provider lane out of rotation meant
deleting `[[roster]]` rows and losing their config — and `fallback` chains name
roster entries, so a deletion silently orphans other models' chains.
**Decision**: `[[roster]]` rows gain an optional `enabled` (default `true`); a
new first-class `[[provider]]` table gains the same. `effective_enabled =
roster.enabled && provider.enabled` is resolved at parse time. A non-empty
`provider` MUST resolve to a declared `[[provider]]` block (fail closed on
typos); an empty `provider` bypasses the gate (legacy/test shape). A disabled
model is **never selected** and is **skipped** in the fallback walk — the same
rule, so there is no special case for `chain[0]`. Manual `enabled` is the hard
off knob; Bursar's per-cycle `Defer` remains the soft one. Details:
`phases/roster-tui-spec.md`.
**Alternatives considered**: Delete-only (loses config); a runtime overlay file
(leaves `conductor.toml` non-authoritative); "disabled primary is still
selectable, dispatch walks to its first enabled fallback" (routing-alias
framing).
**Rationale**: Rejected the routing-alias framing because `select_candidate`
must return a model that will actually run — otherwise the ledger names a model
that never executed. Provider is the natural toggle unit because it is the unit
that actually goes down (quota, rate limit), and it is where `provider_policy`
(conductor-d5j) will land.

## [2026-07-13] `enabled` must NOT enter `candidate_rejection`

**Context**: `candidate_rejection` (`triage.rs:183`) is shared by
`select_candidate`, the fallback walk, and `next_eligible_roster` — so folding
the enabled check into it is the obvious implementation.
**Decision**: Do not. Keep a separate effective-enabled predicate applied
*after* `candidate_rejection`, and add `Flag::AllDisabled` for "candidates
qualify but all are dark". A disabled link in the walk is a hard skip
(`record_fallback_skip`), never the Bursar `Deferred` path.
**Alternatives considered**: Fold it in (one predicate, less code).
**Rationale**: Folding it in makes `select_candidate` return `None` for a fully
darkened tier, and `route` flags that as `Flag::OverCeiling` (`triage.rs:351`) —
reporting "the operator turned these off" as "this item is too hard." Silent
misattribution, and worse for a ratchet-unlocked auto-dispatch item.

## [2026-07-13] TOML write-back is a line-span editor gated by structural equivalence

**Context**: A roster TUI must write `conductor.toml` — 535 lines carrying
load-bearing comments. `config.rs` hand-rolls a read-only TOML parser and the
crate holds only three dependencies.
**Decision**: Hand-roll a line-span editor (`src/config_edit.rs`) that splices
only the lines it touches, so comments survive by construction. Take ratatui +
crossterm for the UI (feature-gated). Do **not** add `toml_edit`. Every write is
gated by a **structural-equivalence check**: re-parse and assert the resulting
`Config` differs from the pre-edit `Config` only in the intended field.
**Alternatives considered**: `toml_edit` (battle-tested round-tripping);
hand-roll the terminal layer too; gate writes on `parse_str` alone.
**Rationale**: `toml_edit` would put two TOML semantics in one tree that can
disagree — the TUI could write what `config.rs` rejects. Rendering, by contrast,
is toil with no domain value, so ratatui is worth the dep. Critically, gating on
`parse_str` alone is **insufficient**: the parser accepts `[[roster]] # comment`
(`config.rs:1944`), so an indexer whose header match is stricter than the
parser's mis-attributes keys to the previous block and silently edits the WRONG
model while still emitting valid TOML. Parseability is not correctness — hence
structural equivalence. (Found by adversarial review, opencode-go/glm-5.2.)

## [2026-07-14] backnotprop/orchestrator is mined for design, never adopted as a dependency

**Context**: `backnotprop/orchestrator` (BUSL-1.1, ~31k LoC TS) is a kubectl-style
process-supervision layer for agent workers across Claude Code, Codex, Copilot,
Grok, and Pi. It explicitly disclaims Conductor's layer — ADR 0008 "do not require
structured worker output in v1", ADR 0011 "parent-directed, no prebaked recipes" —
so it competes with `dispatch.rs` alone, not with scan/triage/verify/ledger. Its
README and ADR titles advertise worktree isolation, session resume, and provider-limit
intelligence: three things we want. A title-deep comparison recommended adopting it
behind the `Exec` trait (`dispatch.rs:129`).

**Decision**: **Mine the design; take no dependency.** Reading the source
(`conductor-iz7`, pinned `583acf4`, memo `docs/notes/orchestrator-recon.md`) refuted
the premise. Concretely: (a) take their provider-limit detectors into Bursar —
endpoints, the two-tier CLI fallback, the window+reset shape, the typed auth-failure
taxonomy (`bursar-ejf`); (b) build a **native Rust client** over Codex's *own*
app-server protocol (`conductor-2d4`); (c) borrow the tri-state degrade-honestly
catalog shape for `roster_drift`; (d) build worktree isolation clean (`conductor-fia`).
Close `conductor-kfq` (wrap-orchestrator spike) as wont-fix. Do not vendor or copy
their code — BUSL-1.1 would carry into our tree.

**Alternatives considered**: Adopt as one `Exec` impl behind the existing trait
(the original recommendation); reject wholesale and revisit later.

**Rationale**: Adoption's two justifying capabilities did not survive verification.
**Worktree isolation does not exist** — `supportsWorktree`/`CwdPolicy` are unconsumed
type surface (`runtime/types.ts:30-31,40`) and orchestrator never invokes git at all;
our Arena worktrees already exceed it. **Session resume is codex-app-server-only** —
Claude Code, Pi, Copilot and Grok all use the process executor, whose handle is
`{completed, interrupt}` (`process.ts:399-401`), one-shot and identical to
`dispatch.rs`; `sendMessage?`/`startGoal?` are optional members precisely because
most executors lack them (`executors/types.ts:44-48`). Wrapping would therefore buy
nothing for three of our four backends. And the resume protocol we *do* want —
`thread/start`, `thread/resume`, `turn/start`, `account/rateLimits/read` — is
**Codex's API, not orchestrator's**; we already depend on the `codex` binary, so we
can speak it from Rust with no BUSL exposure and no Node toolchain.

The disqualifier is verification. Their success oracle is `code === 0`
(`process.ts:257-264`), cross-checked only against the worker's own output stream —
so an **agy quota-exhausted no-op reports `succeeded`**. That is the exact failure the
charter names ("exit codes are testimony; artifacts are evidence"), and `dispatch.rs`'s
`CommitProbe` classify (`dispatch.rs:140,483`, and the `exit_zero_with_no_new_commit_*`
tests) is materially better: it at least demands an artifact. We would have had to
override their status model on day one. A supervisor whose notion of success we must
overrule is not a supervisor we should depend on — it is a design to read and a
protocol to reimplement.

**Correction (same day, on discovering `conductor-1i9`)**: an earlier draft of this ADR
called our classify "strictly better." That overclaims. `conductor-1i9` establishes that
`CommitProbe` is **itself forgeable** — it declares success on *any* HEAD change
(`dispatch.rs:350-352`, `verify.rs:161-162,223`), not on the worker's *own* commit, so a
concurrent commit satisfies it. Ours demands an artifact but does not check who produced
it; theirs demands nothing but an exit code. The decision above is unchanged — we would
still be importing the weaker of two flawed oracles, and would still have to override it
— but the honest statement is that **both success models are broken, ours less badly**,
and `conductor-1i9` is the P0 that fixes ours. This ADR is not a claim that our
verification is sound; it is a claim that theirs is not worth adopting.

## [2026-07-15] Codex is the terminal approved fallback for fleet ownership

**Context**: Live Bursar evidence can fail closed for otherwise usable provider
accounts: Anthropic currently returns an authentication error, opencode-go has
no positive availability signal, and Ollama Cloud is not represented in
`bursar/status@2`. The GPT-5.6 Codex lane is positively healthy and already in
the closed roster, but was outside the legacy primary models' fallback chains.

**Decision**: With explicit human approval, append the matching-tier GPT-5.6
Codex profile to every non-Codex roster chain. Preserve opencode-go and Ollama
Cloud as preferred cheap-work lanes and NeuralWatt as reserve; Codex remains the
terminal fallback. Lead chains may use both Terra and Sol in capability order.

**Alternatives considered**: Disable Bursar; record operator assertions as
provider health; promote Codex to the default primary; bypass Conductor and
dispatch the blocked Bead manually.

**Rationale**: A configured fallback is reviewable policy and becomes part of
the immutable approved route. It restores forward progress from real positive
Codex evidence without treating prose as quota truth, weakening fail-closed
provider checks, or moving claim/verify/close ownership out of Conductor.

## [2026-07-22] Bursar snapshots and run artifacts are the sole active routing authority

**Context**: A mutable Conductor roster and v1/Arena artifacts let approved work
be reinterpreted at dispatch or resume. The next role-routing phases need
durable structure before they activate behavior.

**Decision**: Consume only strict `bursar/roster@2`; copy and pin its exact
bytes, source identity, and policy digest in every `conductor/run@2` before
selection. Use only strict `conductor/event@2` in `runs-v2/`. Finished legacy
history remains inert while deployment preflight blocks actionable v1 recovery.
Remove Arena outright. Represent `Plan` targets, routes, and progress as typed
structural state only; do not activate plan execution or a generic scheduler.

**Alternatives considered**: Compatibility parsers; rereading live roster
configuration on resume; carrying Arena alongside v2; activating role policy
with its state model.

**Rationale**: Authorization must name immutable inputs and runtime evidence
must remain interpretable after configuration changes. A clean namespace and
closed schemas fail safely rather than silently mixing historical semantics.

## [2026-07-27] An approval-gated supersession transition, never `dispatch --resume`, terminalizes a failed promoted run superseded by a verified descendant

**Context**: bd `conductor-0kc`. `dispatch --resume`'s promotion-receipt/HEAD-mismatch
discriminator (`dispatch_cycle.rs`'s `resume_promoted_work`/`resume_finished_promoted_work`,
the `promoted worker HEAD changed` and `finished promoted HEAD changed` checks) is correct
and load-bearing: once canonical HEAD no longer equals a run's promoted commit, that run
must stay recovery-required forever, not be silently resumed or re-verified. But a later,
separately approved run can legitimately continue directly from that exact promoted commit,
get verified, and advance HEAD past it — and Undertake had no way to close the earlier run's
Bead without either manually touching bd (forbidden) or weakening the HEAD-equality check
that makes `--resume` trustworthy.

**Decision**: Add one explicit, additive operator command, `undertake supersede` (`cli.rs`
`run_supersede`/`parse_supersede_options`), calling a new `dispatch_cycle::run_supersession`.
The operator pins every identity by hand — source and replacement run id, cycle id, Bead,
and promoted commit (`SupersessionPin`) — and the transition re-verifies every pinned value
against the actual durable evidence rather than trusting the run ids alone. It requires, in
one pass before any mutation: the source run's exact failed-promoted-verifier shape and event
history (reusing `validate_finished_promoted_failure`/`validate_finished_promoted_failure_events`
unchanged), the mirrored exact verified shape and event history for the replacement
(`validate_replacement_promoted_success_shape`/`validate_finished_promoted_success_events`),
the source owner provably dead or stale (reusing `authenticate_finished_promoted_owner`
unchanged), the replacement's `before_head` equal to the source's promoted commit, canonical
HEAD equal to the replacement's promoted commit, a clean repository, both runs' own approval
envelopes and authorization hashes self-consistent, and durable proof — the replacement run's
own `terminal_transition()` — that Undertake itself, not a manual `bd close`, closed the
replacement's Bead. Only then does it persist a schema-versioned `undertake/supersession@1`
receipt (source and replacement promotion-artifact sha256 hashes, both run/cycle/Bead ids) via
`crate::run::durable_atomic_replace` under the source run's own directory, and only then close
the source Bead with a reason naming both runs. It never opens `RunHandle` for write, never
touches either run's `promotion.json`/`manifest.json`/`events.jsonl`, never spawns a worker or
verifier (the function takes no `Exec`), and never rewrites Git history. A
`quarantine::RepoLease` held for the call's duration makes a concurrent invocation fail closed
on lease contention rather than double-close. Repeats are idempotent by construction: an
existing receipt is trusted only when every pinned field matches it exactly; a match with the
Bead already closed is a pure no-op, and a mismatch fails closed without touching bd.

**Alternatives considered**: Loosening `--resume`'s HEAD-equality check for a "known good"
descendant (rejected — this is exactly the weakening the bead prohibits and would let any
later HEAD move quietly re-authenticate a stale claim); auto-discovering the "latest" verified
run to supersede a given failed one (rejected — ambiguous by construction, and the bead
requires pinning one *exact* pair, not a heuristic search); folding supersession into the
ordinary per-cycle dispatch loop as another `dispatch_one` outcome (rejected — supersession
operates on two already-terminal runs from two different, already-closed cycles and needs no
`PlannedItem`/roster/cycle-plan machinery; forcing it through that path would either fabricate
a fake plan item or silently loosen the real one's invariants); marking the source run
`verified` (rejected outright — the bead is explicit that the failed run must never be reported
verified, only its Bead terminalized).

**Rationale**: Bounding the operator surface to one pinned, independently re-verified, receipted
transition keeps `dispatch --resume`'s HEAD-equality discriminator exactly as strict as before
while giving Undertake an evidence-bound way to stop carrying an inaccurate open P0 forever.
Retention: the `supersession.json` receipt is kept indefinitely alongside the source run's other
durable evidence (`promotion.json`, `events.jsonl`) — it is the only record proving why a Bead
with a `failed` terminal run outcome is nonetheless closed, and deleting it would make that
closure unauditable.

## [2026-07-28] v1 extracts a kernel from the proven engine instead of promoting the loop prototype

**Context.** The approved architecture requires all four jobs on one kernel (cutover gate
4). `src/loop.rs` was read as a finished kernel awaiting CLI wiring. It is not: it
unconditionally requires an authenticated direct-child commit (`loop.rs:346-359`), so
read-only `review`/`consult` can never succeed; `RunHandle::create` refuses Plan runs
outright (`run.rs:1021-1025`); it hardcodes `RunJob::Work`; its terminal model is only
`Completed|Failed`; `LoopClaim` cannot claim; `loop.json` is neither fsynced nor
integrity-bound, so a forged `terminal=completed` closes a Bead with no attempt; and
resume neither checks worker liveness nor binds the approved profile. Separately,
`job.rs`'s registry is never constructed — the accepted spelling is `[[job]]` and
`undertake.toml` contains none.

**Decision.** Extract a generic durable attempt runner from the proven
`dispatch.rs`/`run.rs`/`quarantine.rs` machinery, migrate one job at a time onto it, and
delete `loop.rs`. Do not grow the 431-line prototype to match 8,237 lines of hardened
behavior. Salvage the prototype's fresh-context-per-iteration model, durable phase
checkpoints, and bead/artifact target distinction as design input only.

**Consequences.** Freeze `dispatch_cycle`'s test corpus as a named parity corpus before
anything retires (required by the consolidation spec). Pass cutover gate 10's installed
vertical smoke *before* deleting the rollback engine, not after. Quiesce and resolve every
pending/implementing/reclaimable legacy run before removing its engine. Defer `-D
dead_code` to the end, since it would otherwise force removal of recovery APIs the moment
their only caller dies. Reviewed adversarially by GPT-5.6 Sol (reject, adopted) and GLM
5.2 (ship-with-changes); both adjudicated against source.

## [2026-07-28] v1 `work` writes the repository directly; attempt isolation is dropped

**Context.** `dispatch_cycle` runs each worker in an isolated `AttemptCheckout` and
promotes the resulting commit. That one choice is the root of verification-input
materialization, `undertake supersede`, promotion recovery records, and three of four
resume state machines — a large majority of the engine's complexity.

**Decision.** v1 `work` writes the target repository directly, Ralph-style. The
consolidation spec permits it ("Repo writes allowed inside approved scope"), its loop
requirement is "worker identity plus exclusive repo lease" rather than worktree isolation,
and it states the native loop preserves Ralph's earned behavior.

**Consequences.** Attempt checkout, commit promotion, verification-input materialization,
`undertake supersede` (1,492 lines), promotion recovery, and three resume state machines
leave v1 scope; Beads `8hz` and `1ls` close as moot. In exchange the runner MUST add a
clean-tree preflight before spawn, quarantine adoption of a failed attempt's partial work
carried forward as a patch reference, and retention of the post-verify HEAD/tree/claim
recheck. Without all three this is a safety regression, because `dispatch_cycle`
preflights both. Quarantine becomes load-bearing, which raises the severity of `jum`
(glob-interpreted `--exclude` at `quarantine.rs:321`) rather than retiring it.

## [2026-07-28] Review-panel diversity is by model family, amending the ProviderId rule

**Context.** The consolidation spec compares exact `ProviderId` for reviewer diversity,
and `adversarial.rs:565-602` implements that faithfully. Consequently
`ollama-cloud/glm-5.2` and `opencode-go/glm-5.2` count as two independent reviewers while
being the same weights behind two resellers. The operator policy in `AGENTS.md` requires
reviewers of a "different model family (developer lineage, not inference provider)." The
two conflict. This was observed live: dispatching this cycle's own review hit a quota
limit on opencode-go and fell back to ollama-cloud for the same model.

**Decision.** Model family governs. `conductor-ao8` stays in v1 as a contract amendment,
not a bug fix.

**Consequences.** Musterroll owns profile identity, so a `model_family` field must be added
to `musterroll/roster@2` and populated before Undertake can enforce this; `review`-job
diversity enforcement is cross-repo gated on that. Family must never be inferred by
parsing `ProfileId` — the spec forbids deriving execution coordinates from the opaque
label. `conductor-pzo`'s specific Fable-plus-provider panel remains cutover gate 11 work,
outside gates 4/10.

## [2026-07-28] Attempt isolation is out of Undertake's scope; CASE owns containment

**Context.** Adversarial review of the Phase 1a runner contract (GPT-5.6 Sol) established
that dropping the attempt checkout costs real safety properties: an unauthenticated
canonical commit has no recovery path (dirty-tree quarantine refuses once HEAD moved,
`quarantine.rs:357-365`), quarantine captures only *uncommitted* work
(`quarantine.rs:337-344`), transient unverified changes are visible to the operator and to
watchers during execution, and `CommitProbe::is_clean` runs
`git status --porcelain --untracked-files=normal`, which does not see ignored-file changes.
An earlier draft of the contract asserted three compensating controls were sufficient.
They are not.

**Decision (user).** Isolation is **out of scope for Undertake entirely**. It belongs to
CASE — the Controlled Autonomy Safety Engine (`~/git/case`, spec
`docs/spec/CASE_V0_1_SPEC.md`, transcribed 2026-07-27, not yet frozen and not authorized
for implementation). Undertake `work` writes the target repository directly. Undertake does
not build attempt checkouts, commit promotion, supersession, or a containment layer, and
must not grow one back under another name.

**Consequences.** This supersedes the framing of the `[2026-07-28]` D1 ADR: in-repo
execution is not a trade Undertake made for simplicity, it is a scope boundary. The
promotion subsystem, verification-input materialization, `undertake supersede`, promotion
recovery, and three of four resume state machines stay deleted.

What Undertake still owes is **detection, not containment**: a durable pre-attempt HEAD
checkpoint written before spawn, and a fail-closed refusal on resume when canonical HEAD
moved without a matching durable receipt — Undertake declines to touch the repository and
surfaces it for human resolution. Clean-tree preflight and quarantine adoption of
uncommitted work remain in scope as detection.

**Accepted residual risk, stated plainly.** CASE is unfrozen and unimplemented, so the
containment gap is unowned *today*, not merely delegated. Until CASE ships, a crash between
a worker's commit and its durable receipt can leave an unprovable commit on the branch that
Undertake will refuse to touch. Ralph — in daily use — has the same exposure with no
detection at all, so this is not a new hazard, and Undertake is strictly ahead of the
status quo. Revisit only if CASE's scope changes.

## [2026-07-28] Conditional Beads release: no bd CAS primitive exists; narrow the TOCTOU instead

**Context.** `BdClient::release` (`src/bd.rs`) ran `bd update <id> --status open --assignee
""` unconditionally. The repo lease (`quarantine::RepoLease`) only serializes concurrent
Undertake processes; it does nothing against a human running `bd close` between Undertake's
last observation of a claim and its eventual release. Adversarial review promoted this from
P3 to P1 because it is an ordinary single-operator race, not an exotic multi-process TOCTOU:
an operator who deliberately closes a Bead while Undertake is mid-run can have that close
silently undone the moment Undertake finishes and releases.

**Investigation.** Checked whether the installed `bd` (1.1.0, Homebrew) exposes any
conditional-update / compare-and-swap primitive for `status`/`assignee`. It does not:
- `bd update --help` has no `--if-status`, `--if-version`, ETag, or any other
  optimistic-concurrency flag — only unconditional field setters.
- `--claim` is documented as "Atomically claim the issue (sets assignee to you, status to
  in_progress; idempotent if already claimed by you)" and is server-side gated in the
  *claiming* direction only: live-probed, `bd update <id> --claim` against a `closed` issue
  fails with `Error claiming <id>: issue not claimable: status closed` (exit 1). There is no
  equivalent gate for the reverse (release) direction.
- `bd batch` runs multiple writes in one Dolt transaction but explicitly documents that read
  commands (`show`, `list`, `ready`, …) are "NOT accepted" inside it, so a read-then-write
  cannot be made atomic through batching either.
- `bd show` returns no version/revision counter that a future `--if-updated-at`-style flag
  could key off.
- Live-probed the actual vulnerability directly: claim an issue as `undertake`, `bd close`
  it (simulating the operator), then run the exact shape of the old `release()` primitive
  (`bd update <id> --status open --assignee ""`). It exits 0 and silently reopens the
  closed issue, clearing `closed_at`/`close_reason`. This confirms the raw primitive is
  genuinely unsafe, not theoretically so.
- Raw SQL (`bd sql`) could express a conditional `UPDATE … WHERE status='in_progress' AND
  assignee='undertake'`, but that bypasses bd's supported command surface, its Dolt
  versioning/audit trail, and the "don't invent semantics the backend doesn't support"
  constraint on this change. Rejected.

**Decision.** No supported atomic primitive exists, so implement the narrowest achievable
fail-closed mitigation instead: `BdClient::release_owned(repo, id, expected_assignee)` is a
new default trait method that re-fetches the issue via `bd show` immediately before the
mutating call, with nothing but the in-process status/assignee comparison in between, then:
- if `status == open && assignee.is_none()`, treats it as an idempotent no-op (some other
  completed release already happened) and returns without a redundant `bd` call;
- if `status == in_progress && assignee == expected_assignee`, proceeds to the existing raw
  `release()`;
- otherwise, refuses and returns a diagnostic naming the expected vs. observed
  status/assignee — the "explicit operator diagnostic" required instead of a silent reopen.

Every production call site that previously called `bd.release(...)` directly now calls
`bd.release_owned(..., "undertake")`: the three post-claim releases and
`finish_promotion_recovery_failure` in `dispatch_cycle.rs`, the quarantine-recovery release,
`apply_terminal_transition`'s Release branch, both releases inside `reclaim_stale_claim`, and
the two release sites in `verify.rs` (`fail_with_review`, `review_revise`). The raw `release`
stays as the low-level primitive `release_owned` itself calls, and as what `bd.rs`'s own
real-subprocess round-trip test intentionally exercises. `loop.rs`'s `LoopClaim::release` is
a distinct trait with no production `BdClient`-backed implementor today; out of scope here,
but a future implementation should route through the same pattern.

**Consequences / residual race.** This narrows the window from "however long Undertake's own
bookkeeping takes between its last observation and the release call" (in the worst case,
`reclaim_stale_claim` and the quarantine-recovery path did real filesystem/git work — run
artifact writes, HEAD checks, worktree cleanup — between their earlier re-fetch and the old
raw release) down to two back-to-back `bd` subprocess invocations (`show` then `update`)
with no other I/O between them. That window is not zero — `bd` gives no way to make it
zero — but a human would have to land a `bd close` in the literal gap between two sequential
CLI invocations, rather than at any point during a worker's run. If `bd` ever adds a
compare-and-swap/conditional-update primitive, `release_owned` should be revisited to use it
directly instead of the re-fetch-then-compare pattern.

## [2026-07-28] Lease and recovery owners bind to (pid, process generation), not bare pid

**Context.** `conductor-47p`: `RepoLease` and `WorkState.owner_pid`/`worker_pgid` stored and
checked a bare pid (`kill(pid, 0)` / `kill(-pgid, 0)`). After an ordinary crash, once the OS
reuses that pid number for an unrelated process, liveness reports the old owner alive
forever — no concurrency required, just one crash plus normal pid recycling. Review promoted
this P3 → P1 because it directly contradicts the resumability the product claims.

**Decision.** Bind every recorded pid/pgid to a process generation — the kernel-reported
process start time (`sysinfo::Process::start_time()`, the same signal
`dispatch::kernel_process_identity` already uses to authenticate commit-receipt peers) — and
add `quarantine::process_generation`, `owner_pid_authenticated_live`, and
`worker_group_authenticated_live` as the composed liveness+identity check. A pid that is
alive but whose *current* generation no longer matches what was recorded proves the original
owner is gone (the OS handed the number to someone else), so it authenticates as dead exactly
like an `ESRCH` pid — closing the hole a bare `kill(pid, 0)` cannot see. A recorded generation
that still matches keeps the owner live. Absent generation (legacy record, or the writer's own
generation was unreadable at write time) falls back to the pre-existing bare-pid behavior —
conservatively presumed live, never invented as dead either way, with the pre-existing
"still held" diagnostic as the actionable signal.

**The leader-vs-descendant composition.** `process_group_alive(pgid)` (`quarantine.rs`)
succeeds if *any* member of the group lives; `kernel_process_identity(pid)`
(`dispatch.rs`) only ever probes the one pid numerically equal to `pgid` (the group leader,
since workers lead their own group). These do not compose safely on their own: if the leader
dies but an orphaned descendant survives, the leader's pid slot resolves to nothing, and
naively reading that as "leader gone ⇒ reclaim" would discard a group still doing real work.
The resolved rule: the group is provably dead only when `process_group_alive` itself says so
(unchanged). When the group has a live member, checking the leader-pid slot's generation is
allowed to *convert* an apparent "still held" into "reclaimable" only when that slot resolves
to a *different* generation than recorded — never when it resolves to nothing. That asymmetry
is not mere caution: the kernel never reissues a number as a live pid while any process still
holds it as a process-group id, so an empty leader-pid slot with the group still alive can
only be the original group's own orphaned descendant, never an unrelated later group reusing
the number. A mismatched-but-present occupant, by the same invariant, proves the opposite —
the original group, leader and every descendant, is fully gone.

**Legacy records.** `WorkState` gained `owner_pid_generation`/`worker_pgid_generation`
(`Option<u64>`, `#[serde(default)]`) alongside the existing `owner_pid`/`worker_pgid`; the
`RepoLease` owner-file format gained an optional `pid_generation=` line under the existing
`lease_version=2` schema (no version bump — an unknown-to-old-readers extra line is inert to
them, and old records simply parse with the field absent). Every record written before this
change, or written by a process whose own generation could not be read, has no generation on
file. Those are handled exactly as before this change — presumed live — never reclassified as
dead purely for lacking a generation.

**Out of scope.** Kept to what conductor-47p asked for: the binding, the composition rule, and
every dispatch_cycle.rs recovery-owner check that reads `WorkState.owner_pid`/`worker_pgid`
(finished-promoted-owner, retained-unauthenticated, pending-review, and the stale-claim
reclaim path). Left untouched: `PromotionRecoveryRecord.owner_pid` (a separate, promotion-only
schema slated for deletion per the `[2026-07-28]` "Attempt isolation is out of Undertake's
scope" ADR above) and every in-process, same-lifetime liveness probe on a just-spawned child
(`dispatch.rs`/`process.rs` monitoring code) — those never cross a crash/restart boundary, so
they carry no pid-recycling exposure. `loop.rs`'s own resume path still blanket-refuses on any
`worker_pgid` without checking liveness at all (`undertake-runner-contract.md` "Resume" items
1-3, 5) — a separate, larger prep-4/`gtgf` deliverable that consumes this binding but was not
itself in scope here.
