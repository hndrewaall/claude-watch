"""claude_agents — shared helpers for Claude Code subagent identification.

A single source of truth for "what is an agent_id" and "how do I look up
liveness for a queue item's owning agent" across the Python tools that
need to consume `claude-watch active-agents` output:

  - agent-msg                 (claude-config/bin/agent-msg)
  - work-queue-exporter       (claude-watch/exporters/work-queue-exporter/)
  - queue-minisite            (docker-gomorrah/queue-minisite/)
  - cron-queue-check          (server-config/bin/cron-queue-check)

Canonical agent_id format: the JSONL filename stem WITHOUT the `agent-`
prefix and without the `.jsonl` suffix. Example:

  ~/.claude/projects/-home-hndrewaall/<session>/subagents/agent-ac9e993a105a6ef41.jsonl
                                                             ^^^^^^^^^^^^^^^^^^
                                                             this is `agent_id`

The same identifier is used by:

  - claude-watch active-agents JSON (`agents[].agent_id`, `agent-` stripped)
  - claude-watch agent list / agent-ctl (`agent-` stripped via load_agents)
  - agent-msg inbox file path (~/.config/claude/agent-inbox/<agent_id>.json)
  - agent-msg index entries

Functions:

  load_agent_state(path)
      Read the JSON written by `claude-watch active-agents --write-state`.
      Returns the parsed dict (always has `subagents`/`workloads`/`agents`
      keys, even on failure — empty arrays).

  agents_by_queue_id(state)
      Build a queue_id -> agent record map. Dedup rule when multiple
      agents reference the same queue_id (rare — happens after a retry):
      live > stale; among same liveness, smaller jsonl_age_seconds wins.

  agent_for_queue(state, queue_id)
      Convenience: load+lookup. Returns None if not found.

This module is INTENTIONALLY pure-Python with NO third-party deps so it
vendors cleanly into Docker images that don't get a full uv venv. Stick
to stdlib.
"""

from __future__ import annotations

import json
from typing import Any, Optional

DEFAULT_AGENT_STATE_PATH = "/var/lib/claude-watch/active-agents.json"


def load_agent_state(path: str = DEFAULT_AGENT_STATE_PATH) -> dict[str, Any]:
    """Read claude-watch's active-agents JSON state file.

    Returns a dict with keys `subagents`, `workloads`, `agents` (always
    present, defaulting to empty lists). Failures (missing file, parse
    error) yield the empty-shape dict so callers can treat the file as
    "no signal" without try/except.
    """
    empty = {"subagents": [], "workloads": [], "agents": []}
    try:
        with open(path, "r") as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError):
        return empty
    if not isinstance(data, dict):
        return empty
    # Normalize missing keys.
    return {
        "subagents": list(data.get("subagents") or []),
        "workloads": list(data.get("workloads") or []),
        "agents": list(data.get("agents") or []),
    }


def agents_by_queue_id(state: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Map queue_id -> agent record from a loaded state dict.

    Dedup rule when the same queue_id appears on multiple records:
      1. live > stale
      2. among same liveness, smaller jsonl_age_seconds wins
      3. if both have age=None and same liveness, first-seen wins

    Records without a queue_id are skipped.
    """
    by_qid: dict[str, dict[str, Any]] = {}
    for rec in state.get("agents", []):
        if not isinstance(rec, dict):
            continue
        qid = rec.get("queue_id")
        if not qid:
            continue
        prev = by_qid.get(qid)
        if prev is None:
            by_qid[qid] = rec
            continue
        prev_alive = bool(prev.get("alive"))
        rec_alive = bool(rec.get("alive"))
        if rec_alive and not prev_alive:
            by_qid[qid] = rec
            continue
        if rec_alive == prev_alive:
            prev_age = prev.get("jsonl_age_seconds")
            rec_age = rec.get("jsonl_age_seconds")
            if (
                rec_age is not None
                and (prev_age is None or rec_age < prev_age)
            ):
                by_qid[qid] = rec
    return by_qid


def agent_for_queue(
    queue_id: str,
    path: str = DEFAULT_AGENT_STATE_PATH,
) -> Optional[dict[str, Any]]:
    """One-shot helper: load state file, return the record for `queue_id`."""
    state = load_agent_state(path)
    return agents_by_queue_id(state).get(queue_id)
