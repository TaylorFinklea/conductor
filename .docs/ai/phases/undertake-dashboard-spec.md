# Undertake Dashboard Specification

## Goal

Add a read-only `undertake dashboard` TUI that makes one Undertake run understandable without opening state files, reports, or service-specific CLIs. The opening view prioritizes current work; fleet and service health remain secondary. This first version establishes a separate command-intent boundary for later OMP-powered actions but performs no mutations.

## Product boundary

- Lives in the existing `undertake` binary as `undertake dashboard`.
- Reads local authoritative artifacts and read-only service outputs directly.
- Never approves, dispatches, retries, cancels, resumes, edits routing, writes service state, or launches a model.
- No daemon, socket, database, background service, or new cross-service wire protocol.
- Snapshot readers and renderers receive no mutable `RunHandle` and never open a run directory for write.
- Never touches `leases/`, `heartbeat`, `promotion.json`, or `approval.json`; observation cannot perturb pending-work or quarantine scans.
- A later phase may add operations through an authorized executor calling public CLIs. Mutation authority never enters readers or rendering code.

## Command contract

```text
undertake dashboard [--run <run-id>] [--refresh-ms <milliseconds>] [--config <path>]
```

- `--config` follows existing CLI parsing and state/report-root resolution, including `UNDERTAKE_REPORTS_HOME`; duplicate or unknown arguments exit 2.
- `--run` passes the existing single-normal-component run-ID validation before joining `runs-v2/`. Unknown IDs exit 2.
- `--refresh-ms` governs local artifact polling only. Default 1000 ms; accepted range 250–60000; invalid values exit 2.
- Musterroll refreshes no more often than every 30 seconds.
- Afterfact and Cautionlight refresh only on demand via `r` while Evidence is focused, never more often than every 300 seconds.
- `q` exits 0. Terminal setup failures and unrecoverable initial source errors exit 1.

### Run discovery and selection

- “Newest” means greatest parsed RFC3339 `manifest.created_at`, never directory-name order. Ties break by directory name descending.
- Scan at most the 200 most recently modified `runs-v2/<id>/manifest.json` files, each capped at 128 KiB.
- Default selection is the newest run whose lifecycle is nonterminal, even when liveness is abandoned; otherwise select the newest terminal run.
- A malformed newest candidate is selected and displayed with its error rather than silently falling back.

## Dependencies and feature gate

- Add optional `ratatui = 0.29.0` and `crossterm = 0.28.1` dependencies behind a default-on `tui` feature, matching the existing roster-TUI decision.
- These releases support the repository’s Rust 1.85 MSRV (`ratatui` 0.29 requires 1.74; `crossterm` 0.28.1 requires 1.63). Ratatui 0.30 is prohibited because it requires Rust 1.88.
- `undertake dashboard` is compiled only with `tui`; a no-default-features build retains the non-TUI CLI.
- Record release-binary size before/after and accept the increase only if the stripped release remains operationally reasonable; size is evidence, not a fixed speculative threshold.

## Architecture

### Undertake run adapter

Reads only from the configured Undertake state root:

- atomic `runs-v2/<run-id>/manifest.json`;
- `heartbeat` metadata, without touching it;
- run-local `roster.json`;
- bounded incremental `events.jsonl`;
- fixed-allowlist local logs;
- matching Harness Deck report when the job has a defined join.

Use dashboard-local forward-compatible mirrors of `undertake/run@2` and `undertake/event@2`: ignore unknown fields, but fail the source closed on unknown schema. Do not weaken operational readers used for mutation or recovery.

Resolve opaque profile IDs through the run-local `musterroll/roster@2` snapshot using existing Musterroll parsing. Never parse identity from a profile-ID string and never reopen the source roster artifact. Display manifest roster path/hash/size and policy digest as provenance only.

#### Event tailing

- Read incrementally from the last successfully parsed newline.
- Cap input at 8 MiB and retain at most 5,000 events per run; show truncation.
- A trailing partial line is an ordinary concurrent append: do not advance the offset, do not mark stale, retry next tick.
- A complete invalid line, sequence gap, or schema mismatch is a source error; retain the last valid snapshot generation.
- Unknown outcome strings are displayed verbatim after sanitization and length caps, never interpreted as success.

#### Liveness

Lifecycle and liveness are separate. Derive liveness from heartbeat/`updated_at`, configured 60-second stale-claim threshold, 5-second expected heartbeat, and nonmutating owner/worker process probes:

- `live`: nonterminal and heartbeat younger than the stale threshold;
- `silent`: heartbeat stale but a recorded PID currently exists; PID reuse makes this evidence, not proof;
- `abandoned`: heartbeat stale, no recorded PID exists, and no `run_finished` event exists;
- `unknown`: no usable heartbeat or recorded PID evidence;
- `finished`: terminal lifecycle.

The primary badge is liveness; lifecycle and stage remain separate fields. The Patchstand pilot `run-work-20260725T183920.469500000-p45813-000000` must render `abandoned`, not `running`.

#### Attempts

Reconstruction is job-specific:

- Work/review/consult: join `attempt_started` outcomes shaped `running:<attempt-directory>` to fixed run-local `attempts/<NNN>-<opaque-profile-id>/`; ordinal comes from `<NNN>`. Resolve provider, harness, model, and dispatch ID only from `roster.json`.
- Plan: use typed `plan_invocation` evidence and route stages. Stage-marker events such as `planner_authoring` are markers, not worker attempts.
- Role appears only when typed source data supplies it. Other views omit the column.
- Duration is matching finish timestamp minus start timestamp. Unpaired starts show elapsed time and “no finish event.”
- Unresolvable profile identity remains visible as the opaque ID with an unresolved marker.
- A job with no attempts shows an explicit empty state; no synthetic placeholder.

#### Logs and artifacts

Open only this fixed allowlist after canonicalizing the run directory, joining relative components, canonicalizing the candidate, and confirming containment:

- `attempts/*/worker.stdout.log`;
- `attempts/*/worker.stderr.log`;
- `artifacts/verify/stdout.log`;
- `artifacts/verify/stderr.log`.

`manifest.artifacts[].path` and event artifact paths are display-only and are never opened; valid manifests can contain absolute out-of-run paths. Never derive a read path from model output, an artifact path string, an opaque profile ID, or an event-reported cwd.

Tail at most 64 KiB. Seek from EOF, discard through the first newline when starting mid-file, decode lossily, strip any leading partial escape sequence, then sanitize all control characters. Show truncation and source path.

#### Harness Deck join

- Work: `details.state.cycle_id` maps to the report run directory.
- Plan: `run_id` maps to the report run directory.
- Consult/review: no report; show “no Harness Deck report for this job.”
- Resolve paths only through existing report-root/run-directory validation.

#### Verification precedence

1. durable `details.state.mechanical` when present;
2. latest valid `verify_finished` event;
3. `not run`.

`verifier.mechanical` supplies the command string. Disagreement is visible rather than silently reconciled.

### Musterroll adapter

Reuse `MusterrollClient::status` through `CommandMusterrollClient`; do not add a second parser. Add a bounded subprocess implementation behind the existing trait seam if required. Preserve provider availability, source, checked/data-as-of/expiry timestamps, windows, and reason. Render only an allowlisted sanitized subset of `ProviderStatus.extra`, initially `observation_expiry_basis` and `observation_model`; drop other keys.

### Afterfact adapter

Optional, on-demand, and independently timestamped:

- Run `afterfact events --since 1h` with stdin closed.
- Cap stdout at 4 MiB/20,000 lines and stderr at 256 KiB; timeout after 60 seconds.
- Exit 0 is complete. Exit 1 is partial success: parse valid `afterfact/event@2` JSONL and show a bounded coverage-gap summary from stderr. Exit ≥2, spawn failure, or timeout is an error.
- Correlation is explicitly heuristic, never called typed: exact canonical prefix match of `event.repo.cwd` against `<state-root>/runs-v2/<run-id>/`, or exact `git_commit` equality with a typed worker commit when present. Event paths are comparison data only and are never opened. Show correlated and uncorrelated counts.

### Cautionlight adapter

Cautionlight remains roadmap-deferred. V1 includes its parser, bounded pipeline adapter, empty/deferred panel, and fixtures, but does not make it a live acceptance requirement. When enabled later, pipe the bounded Afterfact bytes into `cautionlight inspect --stdin` under the same process/output/timeout policy. Exit 1 is partial success; parse valid `cautionlight/finding@1` JSONL and preserve coverage warnings.

### Snapshot reducer

Adapters reduce into an immutable `DashboardSnapshot` containing:

- run identity, job, lifecycle, liveness, stage, target repo/Bead, and timestamps;
- progress and elapsed time;
- reconstructed attempts and stage markers;
- verification state/evidence;
- bounded sanitized log tails;
- provider availability;
- Afterfact correlation/coverage state;
- Cautionlight deferred/findings state;
- recent terminal runs;
- per-source last-ok, last-attempt, in-flight, next-attempt, truncation, freshness, and error metadata.

The renderer consumes only `DashboardSnapshot`. Build the next generation off-screen and replace the current snapshot atomically in memory. A source failure retains that source’s last valid value and marks it stale with the current error. Never present independently sampled services as one distributed transaction.

### Runtime and concurrency

- One main event loop owns terminal input and rendering and never blocks on an adapter.
- Local files are polled synchronously within strict byte/count bounds.
- Service subprocess workers communicate through bounded `std::sync::mpsc` channels. At most one request per adapter is in flight; refresh ticks drop while pending.
- Every worker entry catches ordinary errors. Because release uses `panic = "abort"`, install terminal restoration before any worker starts; a worker panic still aborts, but the panic hook restores the terminal first.
- Spawn subprocesses in process groups, cap output while reading, terminate the whole group on timeout/cap breach, and reap before replacement or exit. Reuse existing bounded dispatch primitives where safe rather than `Command::output()`.

### Terminal restoration

Before raw mode or alternate-screen entry:

- install a panic hook that restores raw mode/screen/cursor then delegates to the prior hook; panic hooks run under `panic = "abort"`;
- create an idempotent restoration guard for normal/error exit;
- handle Ctrl-C as a key event under raw mode;
- use safe dependency-provided signal handling for `SIGTERM`/`SIGHUP` restoration and, if supported, `SIGTSTP`/`SIGCONT`; project code remains `unsafe_code = "forbid"`.

Restoration is idempotent and cannot panic.

## Views

### Active run

Opening screen:

- run ID, job, lifecycle, primary liveness badge, target, elapsed time, last update;
- current stage and bounded progress;
- attempts/stage markers with profile/model/provider/harness when resolvable;
- mechanical and qualitative verification state with precedence/source;
- selected bounded log tail;
- Afterfact correlation/coverage summary and Cautionlight deferred/findings summary.

Per-job empty states are explicit. Consult can have empty state/no attempts; Plan shows route stages; work/review omit unavailable roles; jobs without reports say so.

### Secondary panels

- **Providers:** Musterroll availability, windows, expiry, and exclusion reason.
- **Evidence:** on-demand Afterfact correlation/coverage and Cautionlight deferred/findings state.
- **Recent runs:** newest terminal runs with outcome and target.
- **Help:** keys, freshness/liveness semantics, data-source caveats, and read-only notice.

### Navigation

- `j`/`k`, arrows: selection/scroll.
- `Tab`/`Shift-Tab`: panel focus.
- `Enter`: selected attempt or run detail.
- `l`: toggle focused log detail.
- `r`: immediate eligible refresh; no duplicate in-flight request.
- `?`: help.
- `q`: quit.

Compact and normal layouts are required. Below minimum dimensions, render a resize message. Color is supplemental; every state has text or symbols.

## Error and trust model

- Bead text, model output, logs, report prose, events, provider extras, and findings are untrusted display data.
- Strip ANSI/control sequences and length-cap every displayed string.
- Bound every file, retained collection, subprocess output, and read duration.
- Never follow data-derived paths outside the fixed canonical allowlist.
- Malformed newest runs remain visible; no silent fallback.
- Optional/missing services affect only their panels.
- Schema mismatch fails that source closed and retains prior valid data as stale.
- `unknown`, `silent`, `abandoned`, `exhausted`, `partial`, and `stale` remain distinct.
- Read-only observation acquires no dispatch lease and changes no recovery behavior.

## Future action boundary

V1 defines typed UI intents only for navigation, selection, refresh, help, and quit. Future approve/dispatch/cancel/resume/retry/routing intents go to a separate authorized executor, likely OMP-powered. That executor must:

- invoke public Undertake/service commands, never private file mutation;
- preserve exact approval scope, leases, provider gates, and concurrency guards;
- emit normal durable events/artifacts;
- return results through existing adapters;
- have no ability to hand mutable run state to readers or renderer.

## Discriminating verification

1. **Liveness:** nonterminal + heartbeat older than 60 seconds + dead recorded PIDs + no finish event renders `abandoned`; fresh heartbeat renders `live`. Use the failed Patchstand run as a regression fixture.
2. **Torn append:** partial final event line produces no error/stale marker; completing it yields exactly one event. Complete invalid line produces a durable source error with last-valid retention.
3. **Bounds:** event/log caps truncate visibly without unbounded allocation.
4. **Path containment:** absolute `/etc/passwd` artifact and traversal artifact are never opened; fixed allowlist still works.
5. **Attempt reconstruction:** nested plan markers are not attempts; work attempt joins opaque profile to run-local roster; unresolved profile is explicit.
6. **Correlation:** substring-only fake run path does not correlate; exact canonical prefix does. No correlated event path is opened.
7. **Partial service success:** fake Afterfact/Cautionlight emits valid JSONL then exits 1; data and coverage warning render. Exit 2 is error.
8. **Subprocess containment:** oversized and hanging fake tools are process-group killed/reaped; UI input remains responsive.
9. **Terminal restoration:** PTY verifies `q`, `SIGTERM`, and induced release-profile panic leave raw/alternate mode and restore cursor.
10. **Forward compatibility:** unknown extra manifest/event fields render; unknown schema fails only that source.
11. **Sanitization:** split UTF-8 and partial CSI log tails produce no terminal control output.
12. **Rendering:** compact/normal/wide deterministic snapshots remain legible without color and include explicit job empty states.
13. **Live Patchstand acceptance:** show the exact failed pilot as work job, `abandoned` liveness, implementing stage, Luna profile resolved to OpenAI Codex/Codex harness via `roster.json`, canonical `pnpm check` failure, and available Afterfact heuristic correlation/coverage counts. Cautionlight uses fixtures while deferred.
14. Full `cargo test`, strict `cargo clippy --all-targets --all-features -- -D warnings`, no-default-features build, scoped formatting, and release build pass.

## Acceptance criteria

- `undertake dashboard` tracks a real run without mutating any service or repository.
- The opening screen accurately distinguishes lifecycle from liveness and answers what ran, where, at which stage, under which resolved execution identity, for how long, and what failed or remains.
- The abandoned Patchstand pilot is never presented as actively running.
- Stale, missing, malformed, partial, truncated, deferred, and unsupported sources are distinguishable.
- Untrusted bytes cannot inject terminal controls or trigger arbitrary path reads.
- Input remains responsive while service queries are slow.
- Terminal state is restored on normal exit, error, signal, and aborting panic.
- The architecture permits later full actions without moving mutation authority into readers or rendering code.

## Mandatory pre-implementation review gate

This revision reconciles mandatory adversarial reviews from Anthropic Claude Opus 5 and Ollama Cloud GLM-5.2, both through OMP. No implementation begins until both reviewers’ blockers are incorporated and the human approves the reconciled specification. Implementation may then be dispatched among Opus, Sonnet, Ollama Cloud GLM-5.2, and Ollama Cloud MiniMax M3 as appropriate.
