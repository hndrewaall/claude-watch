.PHONY: test test-verbose test-unit test-e2e test-live test-session-task test-hooks build deploy install install-hooks clean

# Default: run all tests in parallel via nextest (preferred) or cargo test
test:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run || \
		cargo test

# Verbose output (show stdout/stderr from passing tests too)
test-verbose:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run --no-capture || \
		cargo test -- --nocapture

# Unit + fixture tests only (fast, ~0.1s)
test-unit:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run -E 'not binary(~e2e_)' || \
		cargo test --lib --test unit_activity_detection

# e2e tests only (tmux-based, ~10s)
test-e2e:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run -E 'binary(~e2e_) and not test(~live)' || \
		cargo test --test 'e2e_*' -- --skip live

# Live e2e tests (spawn real Claude Code, ~1-2 min each, #[ignore] by default)
test-live:
	@command -v cargo-nextest >/dev/null 2>&1 && \
		cargo nextest run --run-ignored=only || \
		cargo test -- --ignored

# Run the session-task Python tests (cross-session queue CLI under tools/).
# Pre-existing 5 failures in test_queue_claude_event.py are tracked in
# tools/session-task/README.md and are unrelated to this migration.
test-session-task:
	uv run --python 3.11 --with pytest pytest tools/session-task/tests/ -v

# Run the obligations / hooks Python tests. These are self-contained
# scripts (not pytest), so we just exec them directly. Each runs against
# an isolated $HOME tmpdir so the live obligations.json is never touched.
# The pre-agent-queue-gate-hook test exercises the real `session-task`
# binary; it must be on PATH (or installed via `make install`).
test-hooks:
	tools/hooks/tests/pre-tool-obligations-gate-hook.test
	tools/hooks/tests/pre-agent-queue-gate-hook.test

# Release build
build:
	cargo build --release

# Build + restart systemd service
deploy: build
	sudo systemctl restart claude-watch

# Install built binaries + scripts onto $PATH ($BIN_DIR, default ~/bin).
# Targets:
#   - claude-watch                          : the Rust daemon
#   - session-task                          : Python CLI (queue + resume action)
#   - obligations                           : obligations gate CLI
#   - pre-agent-queue-gate-hook             : PreToolUse hook (Agent matcher)
#   - pre-tool-obligations-gate-hook        : PreToolUse hook (* matcher)
#   - post-tool-obligations-update-hook     : PostToolUse hook (* matcher)
#   - post-tool-mark-attachment-read-hook   : PostToolUse hook (Read matcher)
#
# All Python scripts ship as the source itself (no compile step). The Rust
# binary depends on `make build` having been run first.
BIN_DIR ?= $(HOME)/bin

install: build
	@mkdir -p $(BIN_DIR)
	@install -m 0755 target/release/claude-watch $(BIN_DIR)/claude-watch
	@install -m 0755 tools/session-task/session-task $(BIN_DIR)/session-task
	@install -m 0755 tools/obligations/obligations $(BIN_DIR)/obligations
	@install -m 0755 tools/hooks/pre-agent-queue-gate-hook $(BIN_DIR)/pre-agent-queue-gate-hook
	@install -m 0755 tools/hooks/pre-tool-obligations-gate-hook $(BIN_DIR)/pre-tool-obligations-gate-hook
	@install -m 0755 tools/hooks/post-tool-obligations-update-hook $(BIN_DIR)/post-tool-obligations-update-hook
	@install -m 0755 tools/hooks/post-tool-mark-attachment-read-hook $(BIN_DIR)/post-tool-mark-attachment-read-hook
	@echo "Installed to $(BIN_DIR):"
	@echo "  - claude-watch"
	@echo "  - session-task"
	@echo "  - obligations"
	@echo "  - pre-agent-queue-gate-hook"
	@echo "  - pre-tool-obligations-gate-hook"
	@echo "  - post-tool-obligations-update-hook"
	@echo "  - post-tool-mark-attachment-read-hook"

# Install git pre-commit hook (warning-free build + unit/fixture tests).
# Symlinks scripts/git-hooks/pre-commit into .git/hooks so script edits
# take effect without re-running this target. Removes any previous file
# at .git/hooks/pre-commit (including older inline-generated versions).
install-hooks:
	@rm -f .git/hooks/pre-commit
	@ln -s ../../scripts/git-hooks/pre-commit .git/hooks/pre-commit
	@echo "Pre-commit hook installed (symlink -> scripts/git-hooks/pre-commit)."

# Clean build artifacts
clean:
	cargo clean
