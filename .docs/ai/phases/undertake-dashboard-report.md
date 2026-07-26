# Undertake Dashboard Report

- Shipped read-only `undertake dashboard` TUI on `feat/undertake-dashboard`.
- Fixed P1 dead-TTY shutdown defect (`conductor-5cq`).
- Proven root cause: Crossterm 0.28.1 Unix event sources repeatedly read a permanently readable dead TTY after `read(2)` returned EOF or macOS `EIO`; `event::poll` therefore could starve Undertake's shutdown atomic.
- Deterministic fix: vendored crates.io Crossterm 0.28.1 and backported upstream issue #793 / PR #1067 across MIO, `/dev/tty`, and cursor-position polling.
- Runtime policy: terminal EOF, broken pipe, and Unix `EIO` are graceful exit 0; unrelated I/O errors remain exit 1; startup without a controlling terminal remains exit 1.
- Regression: release dashboard renders a real fixture frame, then exits successfully within two seconds after final PTY-master closure at `--refresh-ms 250` and `60000`; post-hangup SIGTERM/SIGHUP cannot be starved; cleanup kills/reaps failed children and process groups.
- Verification: vendored EOF tests (MIO and `use-dev-tty`) pass; cursor poll propagation test passes; `cargo tree --features tui -i crossterm@0.28.1` shows one patched package; `cargo test --features tui` passes 801 tests; `cargo test --no-default-features` passes 606 tests.
