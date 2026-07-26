# Undertake Dashboard Report

- Shipped read-only `undertake dashboard` TUI on `feat/undertake-dashboard`.
- Fixed P1 dead-TTY shutdown defect (`conductor-5cq`).
- Proven root cause: Crossterm 0.28.1 Unix event sources repeatedly read a permanently readable dead TTY after `read(2)` returned EOF or macOS `EIO`; `event::poll` therefore could starve Undertake's shutdown atomic.
- Deterministic fix: vendored crates.io Crossterm 0.28.1 and backported upstream issue #793 / PR #1067 across MIO, `/dev/tty`, and cursor-position polling.
- Runtime policy: terminal EOF, broken pipe, and Unix `EIO` are graceful exit 0; unrelated I/O errors remain exit 1; startup without a controlling terminal remains exit 1.
- Regression: release dashboard renders a real fixture frame, then exits successfully within two seconds after final PTY-master closure at `--refresh-ms 250` and `60000`; post-hangup SIGTERM/SIGHUP cannot be starved; cleanup kills/reaps failed children and process groups.
- Verification: vendored EOF tests (MIO and `use-dev-tty`) pass; cursor poll propagation test passes; `cargo tree --features tui -i crossterm@0.28.1` shows one patched package; `cargo test --features tui` passes 801 tests; `cargo test --no-default-features` passes 606 tests.
- Postmortem: `docs/postmortems/2026-07-25-macos-resource-exhaustion.md`.
- Isolated dependency rerun: three fixed runs passed at both refresh extremes; three stock 0.28.1 controls also exited, so the short control did not reproduce the scheduler-dependent live failure documented by three contemporaneous stacks/counters.
- Fresh gates: focused PTY 3/3, runtime 7/7, TUI 801 passed/8 ignored, non-TUI 606 passed/8 ignored, and single vendored Crossterm resolution. Current-toolchain strict Clippy fails on two pre-existing test-only lints; no unrelated source change was made.
- Residual risk: exact PID-to-reviewer assertion ancestry and peak per-process energy/GPU/disk/network shares were not captured. Bounded 1/4/12 viewer measurements scaled approximately linearly and left no descendants.
