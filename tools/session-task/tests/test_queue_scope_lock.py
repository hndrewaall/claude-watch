#!/usr/bin/env python3
"""Tests for the manual scope-lock subcommands (lock/unlock/locks).

Background (2026-05-08): operator wanted a way to enqueue an item but
hold it pending a real-world condition (hardware install, manual config
change, etc.). The lock subcommand parks any pending queue item whose
scope overlaps a locked token until `unlock` releases it.

Coverage:
    1. `lock <scope>` writes the token into queue.json and survives reload
    2. `queue add` of an item with a locked scope returns ready_now=false
    3. `queue ready` excludes items blocked solely by a lock
    4. `queue list --ready` excludes items blocked solely by a lock
    5. `queue spawn-check` refuses items blocked solely by a lock
    6. `unlock <scope>` flips items back to ready_now=true immediately
    7. Double-lock is idempotent (no error, locked_at preserved)
    8. Unlocking a non-locked scope is a no-op (exit 0)
    9. `queue locks` JSON shape is stable

Run:
    uv run --python 3.11 --with pytest \\
        pytest path/to/test_queue_scope_lock.py -v

Or directly: `python3 test_queue_scope_lock.py`.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, check=False, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(
        cmd, capture_output=True, text=True, env=env, timeout=timeout
    )
    if check and r.returncode != 0:
        raise RuntimeError(
            f"command failed rc={r.returncode}\n"
            f"  cmd: {' '.join(argv)}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
    return r


def _add(env, desc, scope_args, *extra):
    cmd = ["queue", "add", desc, "--summary", "t", "--json"]
    for s in scope_args:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return _run(env, *cmd)


# ---------------------------------------------------------------------------
# 1. lock writes a token into queue.json and persists across invocations
# ---------------------------------------------------------------------------


def test_lock_persists_in_queue_json():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _run(env, "queue", "lock", "new-aqi-meter",
                 "--reason", "wait for hardware", "--json", check=True)
        d = json.loads(r.stdout)
        assert d["scope"] == "new-aqi-meter"
        assert d["reason"] == "wait for hardware"
        assert d["locked_at"]  # iso timestamp set
        assert d["already_locked"] is False

        # Second invocation should see the persisted lock.
        r2 = _run(env, "queue", "locks", "--json", check=True)
        rows = json.loads(r2.stdout)
        assert len(rows) == 1
        assert rows[0]["scope"] == "new-aqi-meter"
        assert rows[0]["reason"] == "wait for hardware"
        assert rows[0]["items_held"] == 0


# ---------------------------------------------------------------------------
# 2. queue add of an item with a locked scope returns ready_now=false
# ---------------------------------------------------------------------------


def test_queue_add_during_lock_blocks_ready():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter",
             "--reason", "wait for hardware", check=True)

        r = _add(env, "update ecowitt", ["new-aqi-meter"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["ready_now"] is False
        assert d["lock_blockers"] == ["new-aqi-meter"]
        assert "locks: new-aqi-meter" in d["spawn_instruction"]
        assert "BLOCKED" in d["spawn_instruction"]


# ---------------------------------------------------------------------------
# 3. queue ready excludes items blocked solely by a lock
# ---------------------------------------------------------------------------


def test_queue_ready_excludes_locked():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter", check=True)
        r_add = _add(env, "update ecowitt", ["new-aqi-meter"])
        assert r_add.returncode == 0

        r_ready = _run(env, "queue", "ready", "--json", check=True)
        ready = json.loads(r_ready.stdout)
        assert ready == [], ready


# ---------------------------------------------------------------------------
# 4. queue list --ready excludes items blocked solely by a lock
# ---------------------------------------------------------------------------


def test_queue_list_ready_excludes_locked():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter", check=True)
        _add(env, "update ecowitt", ["new-aqi-meter"])

        r = _run(env, "queue", "list", "--ready", "--json", check=True)
        items = json.loads(r.stdout)
        assert items == [], items


# ---------------------------------------------------------------------------
# 5. queue spawn-check refuses items blocked solely by a lock
# ---------------------------------------------------------------------------


def test_spawn_check_refuses_locked_item():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter", check=True)
        r_add = _add(env, "update ecowitt", ["new-aqi-meter"])
        d = json.loads(r_add.stdout)
        qid = d["id"]

        r = _run(env, "queue", "spawn-check", qid, "--json")
        assert r.returncode == 2, (
            f"expected exit 2 (BLOCKED), got {r.returncode}\n"
            f"  stdout: {r.stdout}\n  stderr: {r.stderr}"
        )
        body = json.loads(r.stdout)
        assert body["ok"] is False
        assert any("locked" in reason for reason in body["reasons"]), body


# ---------------------------------------------------------------------------
# 6. unlock flips items to ready_now=true immediately
# ---------------------------------------------------------------------------


def test_unlock_releases_blocked_items():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "new-aqi-meter", check=True)
        r_add = _add(env, "update ecowitt", ["new-aqi-meter"])
        d = json.loads(r_add.stdout)
        qid = d["id"]

        # Confirm BLOCKED first.
        r_check = _run(env, "queue", "spawn-check", qid, "--json")
        assert r_check.returncode == 2

        # Unlock.
        r_un = _run(env, "queue", "unlock", "new-aqi-meter",
                    "--json", check=True)
        un = json.loads(r_un.stdout)
        assert un["was_locked"] is True

        # spawn-check should now succeed.
        r_check2 = _run(env, "queue", "spawn-check", qid, "--json", check=True)
        body = json.loads(r_check2.stdout)
        assert body["ok"] is True, body

        # And the item appears in queue ready.
        r_ready = _run(env, "queue", "ready", "--json", check=True)
        ready = json.loads(r_ready.stdout)
        assert len(ready) == 1
        assert ready[0]["id"] == qid


# ---------------------------------------------------------------------------
# 7. Double-lock is idempotent (no error; preserves original locked_at)
# ---------------------------------------------------------------------------


def test_double_lock_idempotent():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _run(env, "queue", "lock", "new-aqi-meter",
                  "--reason", "first reason", "--json", check=True)
        d1 = json.loads(r1.stdout)
        assert d1["already_locked"] is False
        first_locked_at = d1["locked_at"]

        r2 = _run(env, "queue", "lock", "new-aqi-meter",
                  "--json", check=True)
        d2 = json.loads(r2.stdout)
        assert d2["already_locked"] is True
        # locked_at must NOT be reset when re-locking.
        assert d2["locked_at"] == first_locked_at
        # No new reason passed, prior reason preserved.
        assert d2["reason"] == "first reason"

        # Re-lock with a new reason updates the reason but keeps locked_at.
        r3 = _run(env, "queue", "lock", "new-aqi-meter",
                  "--reason", "updated reason", "--json", check=True)
        d3 = json.loads(r3.stdout)
        assert d3["already_locked"] is True
        assert d3["locked_at"] == first_locked_at
        assert d3["reason"] == "updated reason"


# ---------------------------------------------------------------------------
# 8. Unlocking a non-locked scope is a no-op (exit 0)
# ---------------------------------------------------------------------------


def test_unlock_non_locked_is_noop():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r = _run(env, "queue", "unlock", "never-locked", "--json")
        assert r.returncode == 0, (r.stdout, r.stderr)
        d = json.loads(r.stdout)
        assert d["was_locked"] is False
        assert d["scope"] == "never-locked"


# ---------------------------------------------------------------------------
# 9. queue locks JSON shape: scope, reason, locked_at, items_held
# ---------------------------------------------------------------------------


def test_queue_locks_json_shape():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "scope-a", "--reason", "ra", check=True)
        _run(env, "queue", "lock", "scope-b", "--reason", "rb", check=True)
        # Add two items: one held by scope-a, one not held at all (different
        # scope, no lock).
        _add(env, "held-item", ["scope-a"])
        _add(env, "free-item", ["scope-c"])

        r = _run(env, "queue", "locks", "--json", check=True)
        rows = json.loads(r.stdout)
        rows_by_scope = {row["scope"]: row for row in rows}
        assert "scope-a" in rows_by_scope
        assert "scope-b" in rows_by_scope
        assert rows_by_scope["scope-a"]["items_held"] == 1
        assert rows_by_scope["scope-b"]["items_held"] == 0
        assert rows_by_scope["scope-a"]["reason"] == "ra"
        assert rows_by_scope["scope-b"]["reason"] == "rb"
        for row in rows:
            assert "locked_at" in row and row["locked_at"]


# ---------------------------------------------------------------------------
# 10. Lock survives a fresh queue.json read (persistence smoke)
# ---------------------------------------------------------------------------


def test_lock_persists_across_invocations():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "persistent-scope",
             "--reason", "outlives the process", check=True)

        # Inspect queue.json directly to confirm storage.
        qjson = json.loads(
            (Path(tmp) / ".config" / "session" / "queue.json").read_text()
        )
        assert "locked_scopes" in qjson
        assert "persistent-scope" in qjson["locked_scopes"]
        assert qjson["locked_scopes"]["persistent-scope"]["reason"] == \
            "outlives the process"


# ---------------------------------------------------------------------------
# 11. Lock does not affect items in other (non-overlapping) scopes
# ---------------------------------------------------------------------------


def test_lock_only_affects_overlapping_scopes():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        _run(env, "queue", "lock", "scope-locked", check=True)

        # Item with a totally different scope should be ready_now=true.
        r = _add(env, "unaffected", ["scope-elsewhere"])
        assert r.returncode == 0, r.stderr
        d = json.loads(r.stdout)
        assert d["ready_now"] is True, d
        assert d["lock_blockers"] == []


if __name__ == "__main__":
    import traceback
    tests = [v for k, v in list(globals().items()) if k.startswith("test_")]
    failed = 0
    for t in tests:
        try:
            t()
            print(f"PASS {t.__name__}")
        except Exception:
            failed += 1
            print(f"FAIL {t.__name__}")
            traceback.print_exc()
    if failed:
        print(f"\n{failed}/{len(tests)} failed")
        sys.exit(1)
    print(f"\n{len(tests)}/{len(tests)} passed")
