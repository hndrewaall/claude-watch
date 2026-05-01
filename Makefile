.PHONY: test test-verbose test-unit test-e2e test-live test-session-task build deploy install install-hooks clean

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

# Release build
build:
	cargo build --release

# Build + restart systemd service
deploy: build
	sudo systemctl restart claude-watch

# Install built binaries + scripts onto $PATH ($BIN_DIR, default ~/bin).
# Targets:
#   - claude-watch  : the Rust daemon
#   - session-task  : Python CLI (cross-session queue + resume action)
#
# session-task ships as the script itself (no compile step). The Rust binary
# depends on `make build` having been run first.
BIN_DIR ?= $(HOME)/bin

install: build
	@mkdir -p $(BIN_DIR)
	@install -m 0755 target/release/claude-watch $(BIN_DIR)/claude-watch
	@install -m 0755 tools/session-task/session-task $(BIN_DIR)/session-task
	@echo "Installed to $(BIN_DIR):"
	@echo "  - claude-watch"
	@echo "  - session-task"

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
