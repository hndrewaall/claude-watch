#!/usr/bin/env python3
"""Hard-fail spawn-gate tests for session-task queue.

Covers the 2026-04-17 refinements:

  * `queue add` with non-conflicting scope succeeds.
  * `queue add` with scope overlapping a running item HARD-FAILS
    (non-zero exit, ALL CAPS "DO NOT SPAWN" stderr, nothing enqueued).
  * `queue add --force-enqueue` bypasses the hard-fail but leaves the
    item in a BLOCKED state (spawn_instruction + spawn-check reflect it).
  * `queue spawn-check` on a ready pending item exits 0.
  * `queue spawn-check` on a blocked pending item exits 2 with ALL CAPS
    "DO NOT SPAWN" stderr.
  * `queue spawn-check` on a non-existent id exits 2.
  * `queue register` atomically marks an item running and fails hard on
    a scope conflict (exit 2, ALL CAPS stderr).
  * `queue register` on an already-running item fails unless --if-absent.

Run:
    uv run --python 3.11 --with pytest \\
        pytest ~/repos/config/tests/test_queue_spawn_gate.py -v

Or directly:
    python3 ~/repos/config/tests/test_queue_spawn_gate.py
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
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, check=False, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env,
                       timeout=timeout)
    if check and r.returncode != 0:
        raise RuntimeError(
            f"command failed rc={r.returncode}\n"
            f"  cmd: {' '.join(argv)}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
    return r


def _add(env, desc, scopes, *extra):
    cmd = ["queue", "add", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return _run(env, *cmd)


def _register(env, qid, *extra):
    return _run(env, "queue", "register", qid, *extra)


def _done(env, qid):
    return _run(env, "queue", "done", qid, check=True)


def _spawn_check(env, qid, *extra):
    return _run(env, "queue", "spawn-check", qid, *extra)


# ---------------------------------------------------------------------------
# 1. Non-conflicting add -> ok
# ---------------------------------------------------------------------------


def test_add_nonconflicting():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "first", ["repo:foo"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        assert d1["ready_now"] is True
        assert d1["spawn_instruction"].startswith("READY:")

        # register + leave it running
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # Non-conflicting scope: adds fine.
        r2 = _add(env, "second", ["repo:bar"])
        assert r2.returncode == 0, r2.stderr
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is True
        assert d2["group_id"] != d1["group_id"]
        assert d2["spawn_instruction"].startswith("READY:")


# ---------------------------------------------------------------------------
# 2. Conflicting add -> HARD-FAIL
# ---------------------------------------------------------------------------


def test_add_conflict_hard_fails():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "first", ["repo:foo"])
        assert r1.returncode == 0, r1.stderr
        d1 = json.loads(r1.stdout)
        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr

        # Overlapping scope while first is running.
        r2 = _add(env, "conflict", ["repo:foo"])
        assert r2.returncode != 0, (
            f"expected hard-fail, got rc={r2.returncode}\n"
            f"stdout: {r2.stdout}\nstderr: {r2.stderr}"
        )
        # Must include ALL CAPS "DO NOT SPAWN" phrasing.
        assert "DO NOT SPAWN" in r2.stderr, r2.stderr
        assert "QUEUE ADD REFUSED" in r2.stderr, r2.stderr
        # Must include the conflicting id.
        assert d1["id"] in r2.stderr, r2.stderr

        # And the queue must NOT contain the failed item.
        r_list = _run(env, "queue", "list", "--all", "--json", check=True)
        items = json.loads(r_list.stdout)
        descs = [it.get("description") for it in items]
        assert "conflict" not in descs, descs


def test_add_conflict_force_enqueue_succeeds_but_blocked():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "first", ["repo:foo"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")

        # --force-enqueue: succeeds, but should be BLOCKED for spawn.
        r2 = _add(env, "queued-followup", ["repo:foo"], "--force-enqueue")
        assert r2.returncode == 0, r2.stderr
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False
        assert d2["spawn_instruction"].startswith("BLOCKED:"), d2
        # spawn-check on the blocked item must fail with exit 2.
        rc = _spawn_check(env, d2["id"])
        assert rc.returncode == 2, (
            f"expected exit 2, got {rc.returncode}\nstderr: {rc.stderr}"
        )
        assert "DO NOT SPAWN" in rc.stderr


# ---------------------------------------------------------------------------
# 3. spawn-check: ready vs blocked vs not-found
# ---------------------------------------------------------------------------


def test_spawn_check_ready_exits_zero():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "first", ["repo:foo"])
        d1 = json.loads(r1.stdout)
        r = _spawn_check(env, d1["id"])
        assert r.returncode == 0, f"stderr: {r.stderr}"
        assert "ok:" in r.stdout, r.stdout


def test_spawn_check_blocked_exits_two_all_caps():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "first", ["repo:foo"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")

        # Force-enqueue a blocked sibling.
        r2 = _add(env, "blocked", ["repo:foo"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rc = _spawn_check(env, d2["id"])
        assert rc.returncode == 2, rc.stderr
        # ALL CAPS "DO NOT SPAWN" banner
        assert "DO NOT SPAWN" in rc.stderr, rc.stderr
        assert "SPAWN-CHECK FAILED" in rc.stderr, rc.stderr


def test_spawn_check_not_found_exits_two():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        rc = _spawn_check(env, "q-does-not-exist")
        assert rc.returncode == 2, rc.stderr
        assert "DO NOT SPAWN" in rc.stderr
        assert "NOT FOUND" in rc.stderr


# ---------------------------------------------------------------------------
# 4. register: atomic claim + hard-fail on conflict
# ---------------------------------------------------------------------------


def test_register_atomically_marks_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "first", ["repo:foo"])
        d1 = json.loads(r1.stdout)

        rr = _register(env, d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr
        obj = json.loads(rr.stdout)
        assert obj["status"] == "running"
        assert "started_at" in obj

        # show it's running
        r_show = _run(env, "queue", "show", d1["id"], check=True)
        shown = json.loads(r_show.stdout)
        assert shown["status"] == "running"


def test_register_fails_on_conflict_all_caps():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "first", ["repo:foo"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")

        # Force-enqueue a blocked item, then try to register it.
        r2 = _add(env, "blocked", ["repo:foo"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rr = _register(env, d2["id"], "--json")
        assert rr.returncode == 2, rr.stderr
        assert "DO NOT SPAWN" in rr.stderr
        assert "REGISTER REFUSED" in rr.stderr


def test_register_double_fails():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "first", ["repo:foo"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")

        # Double register w/o --if-absent -> fails
        rr = _register(env, d1["id"])
        assert rr.returncode == 2, rr.stderr
        assert "ALREADY RUNNING" in rr.stderr

        # --if-absent -> no-op success
        rr2 = _register(env, d1["id"], "--if-absent")
        assert rr2.returncode == 0, rr2.stderr


# ---------------------------------------------------------------------------
# Entry point for direct invocation
# ---------------------------------------------------------------------------


def _all_tests():
    return [
        test_add_nonconflicting,
        test_add_conflict_hard_fails,
        test_add_conflict_force_enqueue_succeeds_but_blocked,
        test_spawn_check_ready_exits_zero,
        test_spawn_check_blocked_exits_two_all_caps,
        test_spawn_check_not_found_exits_two,
        test_register_atomically_marks_running,
        test_register_fails_on_conflict_all_caps,
        test_register_double_fails,
    ]


if __name__ == "__main__":
    fail = 0
    for t in _all_tests():
        try:
            t()
            print(f"PASS: {t.__name__}")
        except Exception as e:
            fail += 1
            print(f"FAIL: {t.__name__}: {e}")
    sys.exit(0 if fail == 0 else 1)
