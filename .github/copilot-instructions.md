# Copilot instructions

See [`CLAUDE.md`](../CLAUDE.md) at the repo root for agent-facing guidance:
build commands, test layout, key modules, pre-commit gates, and the
dashboard scripts.

Quick reference:

- Build: `cargo build --release` (or `make build`).
- Tests: `cargo nextest run` (or `make test`); unit + fixture only: `make test-unit`.
- Pre-commit hook runs warning-free build + unit tests. Don't push without all tests green.
- Subsystem READMEs: `queue-minisite/`, `container/`, `tools/session-task/`, `examples/compose/`.
