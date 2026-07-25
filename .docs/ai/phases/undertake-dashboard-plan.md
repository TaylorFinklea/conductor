# Undertake Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a read-only `undertake dashboard` TUI that accurately tracks active and abandoned Undertake runs plus bounded Musterroll, Afterfact, and deferred Cautionlight evidence.

**Architecture:** Dashboard-local forward-compatible readers reduce bounded local artifacts and independently refreshed service results into an immutable snapshot. A Ratatui/Crossterm renderer consumes only that snapshot; a separate runtime owns terminal restoration, input, refresh scheduling, and bounded subprocess workers. The command has no mutation-capable handle or private-state write path.

**Tech Stack:** Rust 1.85, edition 2024, Ratatui 0.29.0, Crossterm 0.28.1, existing serde/chrono/nix/sysinfo/fs2 infrastructure.

## Global Constraints

- `ratatui = 0.29.0` and `crossterm = 0.28.1` are optional behind a default-on `tui` feature; Ratatui 0.30 is prohibited because it requires Rust 1.88.
- Dashboard code never writes service/repository state, acquires leases, touches heartbeat, or receives mutable `RunHandle` access.
- State-derived paths are fixed-allowlist, canonicalized, and containment-checked; manifest/event artifact paths are display-only.
- Every file, collection, subprocess stream, displayed string, and refresh operation is bounded as specified.
- Lifecycle and liveness are separate; the failed Patchstand run must display `abandoned`, not `running`.
- Unknown fields are tolerated in dashboard-local run/event mirrors; unknown schemas fail only that source.
- Cautionlight is fixture-backed/deferred in live acceptance.
- Formatting is scoped to edited files; never run bare `cargo fmt`.

---

### Task 1: Forward-compatible run discovery and snapshot model

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/main.rs`
- Create: `src/dashboard/mod.rs`
- Create: `src/dashboard/model.rs`
- Create: `src/dashboard/run_source.rs`

**Interfaces:**
- Produces: `DashboardSnapshot`, `RunSnapshot`, `RunLiveness`, `SourceState<T>`, `DashboardRunSource`, `RunSelection`, and `RunSourceConfig`.
- `DashboardRunSource::select(&RunSelection) -> Result<RunSnapshot, DashboardError>` reads bounded artifacts without writes.
- Later tasks consume only these public-within-crate types; renderer never reads files.

- [ ] **Step 1: Add failing tests for dependency gating**

Add Cargo metadata tests in `src/dashboard/mod.rs` or the existing CLI/config test module that assert the `tui` feature is default-on and a no-default-features build does not expose dashboard runtime dependencies. Add `ratatui 0.29.0` and `crossterm 0.28.1` only after the test demonstrates the missing feature contract.

Run: `cargo test dashboard_feature_gate`
Expected: FAIL before the feature/dependencies exist.

- [ ] **Step 2: Add optional dependencies and module gate**

Configure:

```toml
[features]
default = ["tui"]
tui = ["dep:ratatui", "dep:crossterm"]

ratatui = { version = "=0.29.0", optional = true, default-features = false, features = ["crossterm"] }
crossterm = { version = "=0.28.1", optional = true }
```

Gate `mod dashboard` with `#[cfg(feature = "tui")]`. Confirm `cargo check --no-default-features` succeeds.

- [ ] **Step 3: Define immutable dashboard model types**

In `model.rs`, define typed, nonmutating state for run identity, lifecycle, liveness (`Live`, `Silent`, `Abandoned`, `Unknown`, `Finished`), job-specific attempts/stage markers, verification precedence/source, bounded log tails, recent runs, and per-source freshness/error/truncation. Every displayed external value remains a string payload to be sanitized by the render boundary.

Write tests that distinguish all liveness and source states and prohibit collapsing `Unknown`, `Silent`, and `Abandoned`.

Run: `cargo test dashboard::model`
Expected: PASS.

- [ ] **Step 4: Implement bounded discovery with RED tests first**

Tests must create more than 200 synthetic run directories, mixed plan/work IDs, malformed newest manifests, equal timestamps, oversized manifests, and traversal IDs. Assert selection by `created_at`, deterministic tie-break, 200-candidate cap, 128 KiB manifest cap, validated explicit ID, and malformed-newest visibility.

Implement dashboard-local manifest mirrors without `deny_unknown_fields`. Reuse existing root and run-ID validation conventions rather than duplicating mutation readers.

Run: `cargo test dashboard::run_source::discovery`
Expected: PASS after implementation.

- [ ] **Step 5: Implement liveness evidence**

Fixture tests cover fresh heartbeat, stale heartbeat/live PID, stale heartbeat/dead PIDs/no finish event, missing heartbeat/PIDs, and terminal lifecycle. Use the existing 60-second stale threshold and nonmutating process checks. Copy the shape of `run-work-20260725T183920.469500000-p45813-000000` into a deterministic fixture and require `Abandoned`.

Run: `cargo test dashboard::run_source::liveness`
Expected: PASS.

- [ ] **Step 6: Implement incremental bounded events**

Tests first: partial final line is retained/retried without error; completed line appears once; complete malformed line, sequence gap, and unknown schema create source errors; unknown extra fields succeed; 8 MiB/5,000-event caps truncate visibly.

Implement newline-offset state owned by `DashboardRunSource`; never call the strict operational `read_events` for live tailing.

Run: `cargo test dashboard::run_source::events`
Expected: PASS.

- [ ] **Step 7: Implement per-job attempt and verification reduction**

Tests cover work `running:<attempt-dir>` joins, nested plan stage markers, unresolved opaque profiles, empty consult/review states, duration pairing, unpaired starts, and mechanical-state-over-event precedence. Parse run-local `roster.json` with the existing Musterroll snapshot parser; never split profile IDs.

Run: `cargo test dashboard::run_source::attempts dashboard::run_source::verification`
Expected: PASS.

- [ ] **Step 8: Implement fixed-allowlist log tails**

Tests attempt absolute and traversal artifact paths and verify they are never opened. Tail fixtures split UTF-8 and CSI sequences at the 64 KiB boundary. Implement canonical containment, fixed relative patterns, newline alignment, lossy decoding, leading partial-escape removal, and control sanitization.

Run: `cargo test dashboard::run_source::logs`
Expected: PASS.

- [ ] **Step 9: Commit Task 1**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/dashboard
git commit -m "feat: add bounded dashboard run snapshots"
```

---

### Task 2: Bounded read-only service adapters

**Files:**
- Create: `src/dashboard/process.rs`
- Create: `src/dashboard/services.rs`
- Modify: `src/dashboard/mod.rs`
- Modify: `src/musterroll.rs`

**Interfaces:**
- Produces `BoundedCommand`, `CommandOutcome`, `ServiceSnapshot`, `MusterrollDashboardSource`, `AfterfactDashboardSource`, and `CautionlightDashboardSource`.
- `BoundedCommand::run` returns capped stdout/stderr plus exit/timeout/truncation, always reaping its process group.
- Service sources return `SourceState<T>` and never mutate service state.

- [ ] **Step 1: Write failing bounded-process tests**

Use helper executables/scripts that emit 100 MiB, never exit, spawn descendants, exit 1 with valid JSONL, and exit 2. Assert output caps, process-group termination/reaping, timeout classification, and no orphan descendants.

Run: `cargo test dashboard::process`
Expected: FAIL before `BoundedCommand` exists.

- [ ] **Step 2: Implement bounded process execution**

Mirror proven dispatch process-group/timeout mechanics without exposing worker mutation APIs. Close stdin unless explicitly supplying bounded Cautionlight input. Read stdout/stderr concurrently under caps. Timeout and cap breach terminate and reap the group.

Run: `cargo test dashboard::process`
Expected: PASS.

- [ ] **Step 3: Wrap existing Musterroll trait**

Add a bounded command implementation behind `MusterrollClient` rather than a dashboard-specific JSON parser. Tests assert typed availability distinctions and the exact allowlisted `extra` keys; arbitrary keys/control bytes never reach the snapshot.

Run: `cargo test dashboard::services::musterroll`
Expected: PASS.

- [ ] **Step 4: Implement Afterfact partial-success adapter**

Fixtures assert `afterfact/event@2`, exit 0/1/2 semantics, 1-hour window, 4 MiB/20,000-line caps, 256 KiB stderr cap, 60-second timeout, exact canonical-prefix correlation, commit correlation, and substring rejection. Event cwd is comparison-only.

Run: `cargo test dashboard::services::afterfact`
Expected: PASS.

- [ ] **Step 5: Implement deferred Cautionlight adapter**

Parse `cautionlight/finding@1`, preserve exit-1 coverage warnings, and expose an explicit `Deferred` state by default. No automatic pipeline runs in v1.

Run: `cargo test dashboard::services::cautionlight`
Expected: PASS.

- [ ] **Step 6: Commit Task 2**

```bash
git add src/dashboard src/musterroll.rs
git commit -m "feat: add bounded dashboard evidence adapters"
```

---

### Task 3: Ratatui rendering and terminal-safe runtime

**Files:**
- Create: `src/dashboard/render.rs`
- Create: `src/dashboard/runtime.rs`
- Modify: `src/dashboard/mod.rs`

**Interfaces:**
- Produces `DashboardApp`, `DashboardIntent`, `Panel`, `TerminalGuard`, and `run_dashboard`.
- Renderer accepts `&DashboardSnapshot` plus UI-only selection state; no readers, commands, or mutable run handles.
- Runtime converts keys/ticks/service messages into read-only intents and snapshots.

- [ ] **Step 1: Write renderer snapshot tests**

Create compact, normal, and wide deterministic buffers covering live/abandoned/finished, stale/partial/truncated/deferred sources, unresolved profile, no report, no attempts, plan stages, and color-disabled output. Below minimum size must render a resize message.

Run: `cargo test dashboard::render`
Expected: FAIL before renderer exists.

- [ ] **Step 2: Implement pure renderer**

Build Active Run, Providers, Evidence, Recent Runs, and Help panels. Primary badge uses liveness. Every external string passes one sanitization/length-cap function. Color supplements text/symbols.

Run: `cargo test dashboard::render`
Expected: PASS.

- [ ] **Step 3: Write input/runtime state tests**

Cover `j/k`, arrows, Tab/Shift-Tab, Enter, `l`, eligible/ineligible `r`, `?`, and `q`; dropped refresh while in flight; local 1-second cadence; Musterroll 30-second cadence; Evidence on-demand/300-second minimum; service message generation replacement.

Run: `cargo test dashboard::runtime::state`
Expected: FAIL before runtime exists.

- [ ] **Step 4: Implement nonblocking event loop**

Main thread owns input/rendering. Local bounded reads are synchronous. Service workers use bounded mpsc channels, one in flight each; input polling never waits on adapters. Snapshot generations replace atomically in memory.

Run: `cargo test dashboard::runtime::state`
Expected: PASS.

- [ ] **Step 5: Implement terminal restoration before raw mode**

Install an idempotent panic-hook restorer before any worker or raw-mode entry, then a normal-exit guard. Use safe dependency-provided SIGTERM/SIGHUP handling; Ctrl-C is a key event. Restoration cannot panic and delegates to the prior panic hook.

Add PTY helpers that inspect terminal flags/screen state after normal quit, SIGTERM, and an induced release-profile panic.

Run: `cargo test dashboard::runtime::terminal -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit Task 3**

```bash
git add src/dashboard
git commit -m "feat: render terminal-safe Undertake dashboard"
```

---

### Task 4: CLI contract and live Patchstand acceptance

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/dashboard/mod.rs`
- Modify: `.docs/ai/current-state.md`
- Test: existing CLI test module plus dashboard PTY/integration tests

**Interfaces:**
- Adds exact command `undertake dashboard [--run <run-id>] [--refresh-ms <milliseconds>] [--config <path>]` behind `tui`.
- No-default-features build returns the existing CLI without a dashboard command.

- [ ] **Step 1: Write failing CLI parser tests**

Cover defaults, explicit run, refresh bounds, duplicate/unknown args, duplicate config, validated run ID, missing `tui`, and exit codes 0/1/2.

Run: `cargo test dashboard_cli`
Expected: FAIL before CLI wiring.

- [ ] **Step 2: Wire CLI to runtime**

Follow existing CLI parsing/config/state/report-root helpers. Do not invent a parallel HOME/config resolver. Ensure dashboard construction cannot reach dispatch/recovery mutation handles.

Run: `cargo test dashboard_cli`
Expected: PASS.

- [ ] **Step 3: Run synthetic PTY smoke test**

Launch against a synthetic active run, observe refresh, navigate, open log/help, trigger on-demand Evidence, quit, and assert terminal restoration and no file mutations by before/after tree hashes.

Run: the exact dashboard PTY test target added by Task 3.
Expected: PASS.

- [ ] **Step 4: Run live Patchstand acceptance**

Launch pinned to `run-work-20260725T183920.469500000-p45813-000000`. Verify the screen shows work job, abandoned liveness, implementing stage, `openai-codex--codex--gpt-5.6-luna--high` resolved through run-local roster, canonical `pnpm check` failure, Afterfact heuristic/coverage counts, and deferred Cautionlight. Quit and confirm Undertake/Patchstand states are unchanged.

- [ ] **Step 5: Run project verification**

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo check --no-default-features
cargo build --release
```

Record stripped release binary size before/after. Run scoped rustfmt only on edited Rust files, then rerun the relevant tests.

- [ ] **Step 6: Update handoff state and commit**

Record the shipped command, live acceptance result, remaining P0 `conductor-pux`, and future OMP action phase in `.docs/ai/current-state.md` without duplicating decisions.

```bash
git add src Cargo.toml Cargo.lock .docs/ai/current-state.md
git commit -m "feat: ship read-only Undertake dashboard"
```

---

## Review and dispatch policy

- Task 1: implement with Sonnet or GLM-5.2; adversarial review by Opus 5.
- Task 2: implement with GLM-5.2 or MiniMax M3; review by Opus 5 because subprocess containment is load-bearing.
- Task 3: implement with Opus 5 or Sonnet; independent review by GLM-5.2.
- Task 4: retain integration and live acceptance in the orchestrator; final adversarial review by Opus 5 plus GLM-5.2.
- Every worker skips project-wide validation; the orchestrator runs the full commands once after integration.
