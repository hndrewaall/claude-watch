#!/bin/bash
# install-hooks.sh -- install or uninstall the claude-watch hybrid-model
# hooks into ~/.claude/settings.json (default) or .claude/settings.json.
#
# Usage:
#   scripts/install-hooks.sh install [--scope global|project]
#   scripts/install-hooks.sh uninstall [--scope global|project]
#
# Idempotent: install skips events that already have a `claude-watch hook-fire`
# command. Uninstall removes exactly the hooks this script installs.
#
# The three hooks installed:
#   - SessionStart (matcher startup|resume) -> version_update
#   - Stop                                  -> context_high
#   - PreCompact (matcher auto)             -> pre_compact
#
# Uses python3 for JSON manipulation (portable, no jq dependency).

set -euo pipefail

action=""
scope="global"

while [[ $# -gt 0 ]]; do
    case "$1" in
        install|uninstall)
            action="$1"; shift ;;
        --scope)
            scope="$2"; shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 {install|uninstall} [--scope global|project]" >&2
            exit 2 ;;
    esac
done

if [[ -z "$action" ]]; then
    echo "Usage: $0 {install|uninstall} [--scope global|project]" >&2
    exit 2
fi

case "$scope" in
    global)  target="${HOME}/.claude/settings.json" ;;
    project) target=".claude/settings.json" ;;
    *) echo "Unknown scope: $scope" >&2; exit 2 ;;
esac

if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 is required" >&2
    exit 1
fi

mkdir -p "$(dirname "$target")"
if [[ ! -f "$target" ]]; then
    echo "{}" > "$target"
fi

# Pass action + target to the embedded python script via env vars so we
# don't have to juggle quoting here.
ACTION="$action" TARGET="$target" python3 - <<'PY'
import json
import os
import sys
from pathlib import Path

ACTION = os.environ["ACTION"]
TARGET = Path(os.environ["TARGET"])

# Spec of the three hooks we manage.
HOOKS = {
    "SessionStart": {
        "matcher": "startup|resume",
        "cmd": "claude-watch hook-fire version_update",
    },
    "Stop": {
        "matcher": None,
        "cmd": "claude-watch hook-fire context_high",
    },
    "PreCompact": {
        "matcher": "auto",
        "cmd": "claude-watch hook-fire pre_compact",
    },
}


def is_managed(entry):
    """Return True if a settings.json hook-entry is one we installed."""
    hooks = entry.get("hooks", [])
    for h in hooks:
        cmd = h.get("command", "") if isinstance(h, dict) else ""
        if isinstance(cmd, str) and "claude-watch hook-fire" in cmd:
            return True
    return False


with TARGET.open() as f:
    data = json.load(f)

if not isinstance(data, dict):
    print(f"ERROR: {TARGET} does not contain a JSON object", file=sys.stderr)
    sys.exit(1)

data.setdefault("hooks", {})

if ACTION == "install":
    for event, spec in HOOKS.items():
        existing = data["hooks"].get(event, [])
        if any(is_managed(e) for e in existing):
            continue  # idempotent: already installed
        entry = {"hooks": [{"type": "command", "command": spec["cmd"], "timeout": 10}]}
        if spec["matcher"] is not None:
            entry["matcher"] = spec["matcher"]
        existing = existing + [entry]
        data["hooks"][event] = existing

elif ACTION == "uninstall":
    for event in list(data["hooks"].keys()):
        filtered = [e for e in data["hooks"][event] if not is_managed(e)]
        if filtered:
            data["hooks"][event] = filtered
        else:
            del data["hooks"][event]
    if not data["hooks"]:
        del data["hooks"]

else:
    print(f"ERROR: unknown action {ACTION}", file=sys.stderr)
    sys.exit(2)

tmp = TARGET.with_suffix(TARGET.suffix + ".tmp")
with tmp.open("w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
tmp.replace(TARGET)
PY

echo "${action}ed claude-watch hooks in $target"
for ev in SessionStart Stop PreCompact; do
    n=$(python3 -c "import json, sys; d = json.load(open('$target')); print(len(d.get('hooks', {}).get('$ev', [])))")
    echo "  ${ev}: ${n} hook entr(y|ies)"
done
