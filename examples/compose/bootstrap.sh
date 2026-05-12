#!/usr/bin/env bash
# Fresh-laptop bootstrap helper for examples/compose/.
#
# Idempotent. Safe to re-run. Side effects:
#   1. Checks for docker, git, gh; warns (does not fail) if any missing.
#   2. Clones https://github.com/hndrewaall/eichi as a sibling of this
#      repo if no sibling clone exists.
#   3. Seeds examples/compose/.env from .env.example if .env doesn't
#      exist yet — left for the operator to fill in ANTHROPIC_API_KEY.
#   4. Prints next-step guidance.
#
# Usage:
#   bash examples/compose/bootstrap.sh
# Or via the top-level Makefile:
#   make bootstrap

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_DIR="$HERE"
CW_ROOT="$(cd "$HERE/../.." && pwd)"
SIBLING_ROOT="$(dirname "$CW_ROOT")"
EICHI_DIR="$SIBLING_ROOT/eichi"

ok()    { printf '  [ok]   %s\n' "$*"; }
warn()  { printf '  [warn] %s\n' "$*"; }
info()  { printf '  [info] %s\n' "$*"; }

echo "claude-watch examples/compose bootstrap"
echo

echo "Prerequisites"
for tool in docker git; do
    if command -v "$tool" >/dev/null 2>&1; then
        ok "$tool found ($(command -v "$tool"))"
    else
        warn "$tool not found on PATH — install before \`docker compose up\`"
    fi
done

# Compose v2 lives under `docker compose` (subcommand), not the legacy
# `docker-compose` binary.
if docker compose version >/dev/null 2>&1; then
    ok "docker compose v2 plugin available"
else
    warn "docker compose v2 plugin missing — install docker-compose-plugin"
fi

if command -v gh >/dev/null 2>&1; then
    ok "gh CLI found (optional, only needed for PR workflows)"
else
    info "gh CLI not installed (optional)"
fi

echo
echo "Sibling eichi clone"
if [ -d "$EICHI_DIR/.git" ]; then
    ok "$EICHI_DIR exists"
else
    info "Cloning https://github.com/hndrewaall/eichi into $EICHI_DIR"
    if git clone https://github.com/hndrewaall/eichi.git "$EICHI_DIR"; then
        ok "Clone complete"
    else
        warn "git clone failed — clone $EICHI_DIR manually before docker compose up"
    fi
fi

echo
echo "Environment file"
ENV_FILE="$COMPOSE_DIR/.env"
ENV_EXAMPLE="$COMPOSE_DIR/.env.example"
if [ -f "$ENV_FILE" ]; then
    ok ".env already exists at $ENV_FILE (left untouched)"
elif [ -f "$ENV_EXAMPLE" ]; then
    cp "$ENV_EXAMPLE" "$ENV_FILE"
    ok "Created $ENV_FILE from .env.example"
    info "Fill in ANTHROPIC_API_KEY before docker compose up"
else
    warn ".env.example missing — expected at $ENV_EXAMPLE"
fi

echo
echo "Next steps"
echo "  1. Edit $ENV_FILE (set ANTHROPIC_API_KEY if you want the claude-container service to talk to the API)."
echo "  2. (Optional) bootstrap an eichi index on the host:"
echo "       cd $EICHI_DIR && uv venv --python 3.11 && uv pip install -e ."
echo "       eichi index ~/Documents/notes   # or any corpus you want searchable"
echo "  3. From $COMPOSE_DIR run:"
echo "       docker compose up"
echo "  4. Open http://localhost:8000/ (queue) and http://localhost:8001/ (search)."
echo
