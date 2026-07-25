# Undertake Dashboard Specification

## Goal

Add a read-only `undertake dashboard` TUI that makes one active Undertake run understandable without opening state files, reports, or service-specific CLIs. The opening view prioritizes current work; fleet and service health remain secondary. This first version establishes a clean command-intent boundary for later OMP-powered actions but performs no mutations.

## Product boundary

- Lives in the existing `undertake` binary as `undertake dashboard`.
- Reads local authoritative artifacts and read-only service outputs directly.
- Never approves, dispatches, retries, cancels, resumes, edits routing, writes service state, or launches a model.
- No daemon, socket, database, background service, or new cross-service wire protocol.
- A later phase may add full operations through explicit command intents; it must not grant mutation authority to the snapshot reader or render layer.

## Command contract

```text
undertake dashboard [--run <run-id>] [--refresh-ms <milliseconds>]
```

- No `--run`: select the newest nonterminal run, otherwise the newest terminal run.
- `--run`: pin the selected run; an unknown ID is a visible terminal error with exit code 2.
- Default refresh: 1000 ms. Accepted range: 250–60000 ms; invalid values exit 2.
- `q` exits 0. Terminal setup failures and unrecoverable initial source errors exit 1.
- The command restores the terminal on normal exit, error, panic, and interrupt.

## Architecture

### Source adapters

Each adapter is read-only and returns typed data plus freshness/error metadata.

1. **Undertake run adapter**
   - Reads active `runs-v2/<run-id>/manifest.json` and `events.jsonl`.
   - Reads the matching Harness Deck report when present, including `status` and `live`.
   - Reads bounded tails from attempt stdout/stderr logs; never loads an unbounded log into memory.
2. **Musterroll adapter**
   - Invokes `musterroll status --json` with stdin closed and a bounded timeout.
   - Preserves provider availability, source, checked/data-as-of/expiry timestamps, usage windows, and reason.
3. **Afterfact adapter**
   - Invokes `afterfact events --since 24h` with stdin closed, bounded output, and a bounded timeout.
   - Parses only `afterfact/event@2` JSONL and locally correlates typed run/attempt identifiers; it never enables `--unsafe-include-tool-input`.
4. **Cautionlight adapter**
   - Pipes the already bounded Afterfact event bytes to `cautionlight inspect --stdin` with bounded output and a bounded timeout.
   - Parses only `cautionlight/finding@1` JSONL and associates findings by typed artifact/run identifiers.

### Snapshot reducer

Adapters reduce into an immutable `DashboardSnapshot` containing:

- selected run identity, job, lifecycle, stage, target repo/Bead, start/update timestamps;
- progress and elapsed time;
- attempts with role, ordinal, profile ID, provider ID, harness, lifecycle, duration, and failure summary;
- verification state and command outcome;
- bounded log tails with source paths and truncation indicators;
- provider availability;
- Afterfact correlation/ingestion state;
- Cautionlight findings by severity;
- recent terminal runs;
- per-source freshness and errors.

The renderer consumes only `DashboardSnapshot`; it never reads files or launches commands.

A refresh is generation-based: build the next complete snapshot off-screen, then replace the current snapshot. A source failure retains that source's last valid value and marks it stale with the current error. Data from different generations is visibly timestamped; it is never presented as one atomic distributed transaction.

### TUI runtime

Use Ratatui and Crossterm. One event loop owns terminal input, refresh scheduling, and rendering. Slow service commands run outside the render-critical section with one in-flight request per adapter; refresh ticks do not accumulate work. Subprocesses have bounded output and timeouts and are reaped before replacement or exit.

## Views

### Active run

Opening screen:

- header: run ID, job, lifecycle/stage, target, elapsed time, last update, stale indicator;
- progress: current step and bounded progress when reported;
- attempts: role, model/profile, provider, harness, status, elapsed time, retry/fallback relationship;
- verification: pending/running/passed/failed plus concise evidence;
- log tail: latest selected attempt output, source label, truncation/staleness state;
- downstream: Afterfact correlation and Cautionlight finding summary.

### Secondary panels

- **Providers:** Musterroll availability, usage windows, expiry, and exclusion reason.
- **Evidence:** Afterfact attempts/correlation and Cautionlight findings.
- **Recent runs:** newest terminal runs with outcome and target.
- **Help:** keys, source meanings, freshness semantics, and read-only notice.

### Navigation

- `j`/`k`, arrows: selection/scroll.
- `Tab`/`Shift-Tab`: panel focus.
- `Enter`: selected attempt or run detail.
- `l`: toggle focused log detail.
- `r`: immediate refresh without starting a duplicate in-flight refresh.
- `?`: help.
- `q`: quit.

Layouts support normal terminals and a compact mode. Below the documented minimum dimensions, render a resize message instead of clipping or panicking. Color is supplemental; status always has text or symbols.

## Error and trust model

- Treat Bead text, model output, logs, report prose, and findings as untrusted display data.
- Never interpret ANSI/control sequences from artifacts or logs. Sanitize control characters before rendering.
- Never follow paths supplied by displayed model output. Read only paths derived from configured state roots and validated run artifacts.
- Bound file sizes, line counts, subprocess output, and render string lengths.
- A malformed newest run does not silently select an older run; display the malformed run and its error.
- Missing optional services degrade their panels only.
- Schema mismatches are explicit and fail closed for that source.
- Provider `unknown`, `exhausted`, and stale states remain distinct.
- The dashboard is observational. It must not acquire dispatch leases or alter run recovery behavior.

## Future action boundary

Define UI actions as internal, typed intents even though v1 implements only navigation, refresh, and quit. Future mutation intents—approve, dispatch, cancel, resume, retry, and routing changes—must be handled by a separate authorized command executor, likely OMP-powered. The executor must call public Undertake/service commands, preserve approval scopes and concurrency guards, emit durable evidence, and return results that re-enter through normal source adapters. It must never mutate private state files directly.

## Verification

1. Parser fixtures for current and terminal `undertake/run@2`, `undertake/event@2`, Harness Deck live reports, and Musterroll status.
2. Reducer tests for lifecycle transitions, retries/fallbacks, stale-source retention, malformed schemas, missing optional services, and out-of-order event rejection/labeling.
3. Security tests for ANSI/control-sequence stripping, oversized logs/events, path containment, and bounded subprocess output.
4. Deterministic render snapshots at compact, normal, and wide terminal sizes; status remains legible without color.
5. PTY smoke test: launch against a synthetic active run, observe refresh, navigate, show logs/help, quit, and verify terminal restoration.
6. Live acceptance: run against the bounded Patchstand pilot and confirm active stage, selected Codex profile/provider/harness, attempt progression, verification outcome, Afterfact correlation state, and Cautionlight state match authoritative artifacts.
7. Existing full test suite and strict Clippy remain green. Formatting is scoped to edited files because this repository is not baseline-rustfmt-clean.

## Acceptance criteria

- `undertake dashboard` tracks a real active run without mutating any service or repository.
- The opening screen answers: what is running, where, at which stage, under which model/profile/provider/harness, for how long, and what failed or remains.
- Stale, missing, malformed, and unsupported sources are distinguishable.
- Logs and untrusted prose cannot inject terminal controls or cause arbitrary path reads.
- Refresh remains responsive while service queries are slow.
- Terminal state is restored on every exit path.
- The architecture permits later full actions without moving mutation authority into readers or rendering code.

## Mandatory pre-implementation review gate

Before implementation, this specification must receive adversarial reviews from both:

- Anthropic Claude Opus 5 through OMP;
- Ollama Cloud GLM-5.2 through OMP.

Both reviews must assess specification completeness, source-contract assumptions, terminal safety, concurrency, failure semantics, test discriminating power, and the future action boundary. Findings must be reconciled into this specification, and any unresolved blocker prevents implementation. After the gate passes, implementation may be dispatched among Opus, Sonnet, Ollama Cloud GLM-5.2, and Ollama Cloud MiniMax M3 as appropriate.
