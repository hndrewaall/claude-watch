# claude-watch

Rust daemon that monitors Claude Code health via tmux pane capture. Detects activity states (Thinking, ToolRunning, Writing, Idle), heartbeat stalls, token stalls, zombie sessions, and foreground blocks. Runs as a systemd service (`claude-watch.service`).

## Build & Test

```bash
make test              # all tests in parallel (nextest if available, else cargo test)
make test-unit         # unit + fixture tests only (~0.1s)
make test-e2e          # e2e tmux tests only (~10s)
make test-live         # live e2e tests (spawn real Claude Code, ~1-2 min each)
make test-verbose      # all tests with stdout/stderr visible
make build             # release build
make deploy            # release build + systemctl restart
make install-hooks     # install git pre-commit hook
```

Or directly:
```bash
cargo nextest run                    # preferred — parallel test runner
cargo test                           # also parallel (via .cargo/config.toml RUST_TEST_THREADS)
cargo test -- --ignored              # live e2e tests only
```

## Test Parallelism

Tests run fully parallel by default. Configuration:

- **cargo-nextest** (preferred): `.config/nextest.toml` sets `test-threads = "num-cpus"` with 60s slow-timeout for e2e tests.
- **cargo test** (fallback): defaults to num_cpus threads. Override with `RUST_TEST_THREADS=1` for serial execution if debugging.
- **No ordering dependencies**: e2e tests use unique tmux session names (PID + atomic counter), so parallel execution is safe.

## Pre-commit Hook

Run `make install-hooks` to set up the pre-commit hook. It runs `cargo nextest run -E 'not binary(~e2e_)'` (unit + fixture tests only, skips e2e tests that do real sleeps). **Completes in ~0.1 seconds** (~260 tests in parallel via cargo-nextest).

For RED-phase TDD commits (tests that intentionally fail), use `--no-verify` to skip the hook.

Full test suite (including e2e): `cargo nextest run` (~49s, 292 tests in parallel).

## Test Categories

- **Unit tests** (`src/` inline `#[cfg(test)]`): Fast, pure logic tests. ~0s.
- **Integration tests** (`tests/e2e_*.rs`): Spawn tmux sessions with mock processes. ~7-10s each.
- **Live e2e tests** (`tests/e2e_live_detection.rs`): Spawn real Claude Code instances. `#[ignore]` by default. ~1-2 min each. Run with `--ignored`.
- **Fixture tests** (`tests/unit_activity_detection.rs`): Test `detect_activity()` against saved tmux captures. Fast, ~0s.

## Key Files

- `src/tmux.rs` — tmux pane capture, `detect_activity()`, activity state detection
- `src/daemon.rs` — main monitoring loop, heartbeat/token tracking
- `src/config.rs` — configuration loading
- `src/actions.rs` — recovery actions (inject resume, etc.)
- `tests/fixtures/` — saved tmux captures for fixture tests

## Dashboard Scripts

The repo also includes shell scripts that manage the tmux session claude-watch operates in:

- **`dashboard`** — Creates and manages the `dashboard` tmux session where Claude Code runs. Reads layout from `~/.config/dashboard/layout.conf` to configure panes (sidebar, extra windows for monitoring). Supports `--recreate`, `--attach`, `--read-only`, and `--detach` (systemd boot) modes.
- **`dashboard-refit`** — Snaps the sidebar pane to its configured width. Called by tmux hooks on resize/attach events.

These scripts live here (rather than in server-config) because the dashboard layout is tightly coupled to claude-watch's pane detection and monitoring. claude-watch's `[tmux]` config section (`dashboard_pane`, `dashboard_session`) must match the layout these scripts create. The layout config file (`~/.config/dashboard/layout.conf`) controls both the dashboard structure and how claude-watch finds the Claude Code pane.

## Service Management

```bash
sudo systemctl restart claude-watch   # restart with new binary
journalctl -u claude-watch -f         # follow logs
```

Binary path: `target/release/claude-watch` (the systemd unit points here directly).
