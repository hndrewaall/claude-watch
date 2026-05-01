.PHONY: test test-verbose test-unit test-e2e test-live test-session-task test-hooks test-agent-msg test-claude-event test-self-clear test-watchers build deploy install install-hooks clean

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

# Run the agent-msg embedded test suite (CLI for delivering async
# messages to running Claude Code agents via the obligations gate).
# The script's `--test` flag runs all 38 cases in-process against
# isolated tmpdirs, no obligations side effects.
test-agent-msg:
	python3 tools/agent-msg/agent-msg --test

# Run the claude-event + claude-event-tail unit tests.
test-claude-event:
	python3 tools/claude-event/tests/test_claude_event.py

# Run the self-clear config-only smoke tests (the full inject flow needs
# a live Claude Code tmux pane, which can't be reproduced in unit tests).
test-self-clear:
	python3 tools/watchers/tests/test_self_clear_config.py

# Run the claude-event-watch fast-path smoke test.
test-watchers: test-self-clear
	tools/watchers/tests/test_claude_event_watch.sh

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
	@install -m 0755 tools/agent-msg/agent-msg $(BIN_DIR)/agent-msg
	@install -m 0755 tools/claude-event/claude-event $(BIN_DIR)/claude-event
	@install -m 0755 tools/claude-event/claude-event-tail $(BIN_DIR)/claude-event-tail
	@install -m 0755 tools/watchers/claude-event-watch $(BIN_DIR)/claude-event-watch
	@install -m 0755 tools/watchers/self-clear $(BIN_DIR)/self-clear
	@echo "Installed to $(BIN_DIR):"
	@echo "  - claude-watch"
	@echo "  - session-task"
	@echo "  - obligations"
	@echo "  - pre-agent-queue-gate-hook"
	@echo "  - pre-tool-obligations-gate-hook"
	@echo "  - post-tool-obligations-update-hook"
	@echo "  - post-tool-mark-attachment-read-hook"
	@echo "  - agent-msg"
	@echo "  - claude-event"
	@echo "  - claude-event-tail"
	@echo "  - claude-event-watch"
	@echo "  - self-clear"

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
