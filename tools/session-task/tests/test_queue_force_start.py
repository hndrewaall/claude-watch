#!/usr/bin/env python3
"""Tests for `session-task queue force-start`.

Covers:
  * happy path: pending+blocked item promoted to running, scope-conflict
    blockers ignored, audit log written, JSON output shape correct.
  * refuse: item already running -> exit 1, descriptive stderr.
  * refuse: --reason omitted -> argparse exit 2.
  * refuse: empty --reason -> exit 1.
  * refuse: id not found -> exit 1.
  * audit log: row appended to QUEUE_FORCE_START_LOG with reason +
    overridden blockers.
  * claude-event emit: queue-running event written with force_started=true.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_force_start.py -v

Or directly:
    python3 tools/session-task/tests/test_queue_force_start.py
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
    Path(tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
    Path(tmp, "claude-events").mkdir(parents=True, exist_ok=True)
    # Force the audit log into the temp HOME so each test is isolated.
    env["QUEUE_FORCE_START_LOG"] = str(
        Path(tmp) / ".config" / "claude" / "queue-force-start.log"
    )
    # Force the per-force-start recovery bundle dir into temp too.
    env["FORCE_START_BUNDLE_DIR"] = str(
        Path(tmp) / ".config" / "session-task" / "force-start-bundles"
    )
    # Disable pingme noise during tests.
    env["PINGME_DISABLE"] = "1"
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


def _show(env, qid):
    r = _run(env, "queue", "show", qid, check=True)
    return json.loads(r.stdout)


# ---------------------------------------------------------------------------
# 1. Happy path: blocked-pending promoted, audit log written
# ---------------------------------------------------------------------------


def test_force_start_promotes_blocked_pending():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Establish a running item that blocks scope:foo.
        r1 = _add(env, "running", ["scope:foo"])
        d1 = json.loads(r1.stdout)
        assert _register(env, d1["id"], "--json").returncode == 0

        # Force-enqueue a blocked-pending sibling.
        r2 = _add(env, "blocked", ["scope:foo"], "--force-enqueue")
        d2 = json.loads(r2.stdout)
        assert d2["ready_now"] is False, "expected blocked-pending state"

        # spawn-check refuses (sanity)
        rc = _run(env, "queue", "spawn-check", d2["id"])
        assert rc.returncode == 2, rc.stderr

        # Force-start the blocked item.
        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "operator decided", "--json",
        )
        assert rs.returncode == 0, f"force-start failed: {rs.stderr}"
        promoted = json.loads(rs.stdout)
        assert promoted["status"] == "running"
        assert promoted["force_started_reason"] == "operator decided"
        assert "force_started_at" in promoted
        assert isinstance(promoted["force_started_at"], int)
        # The original running item should appear in the overridden-blockers
        # list (cross-scope overlap).
        overridden_ids = [
            b["id"] for b in promoted["force_started_blockers_overridden"]
        ]
        assert d1["id"] in overridden_ids, (
            f"expected {d1['id']} in overridden blockers, got {overridden_ids}"
        )

        # Re-read via `queue show` to confirm persistence.
        shown = _show(env, d2["id"])
        assert shown["status"] == "running"
        assert shown["force_started_reason"] == "operator decided"


# ---------------------------------------------------------------------------
# 2. Refuse: item already running
# ---------------------------------------------------------------------------


def test_force_start_refuses_already_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "first", ["scope:foo"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")

        rc = _run(
            env, "queue", "force-start", d1["id"],
            "--reason", "trying anyway",
        )
        assert rc.returncode == 1, (
            f"expected exit 1 on already-running, got {rc.returncode}\n"
            f"stderr: {rc.stderr}"
        )
        assert "must be pending" in rc.stderr, rc.stderr


# ---------------------------------------------------------------------------
# 3. Refuse: --reason omitted (argparse hard-fails)
# ---------------------------------------------------------------------------


def test_force_start_refuses_no_reason():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "blocked", ["scope:bar"])
        d1 = json.loads(r1.stdout)

        # No --reason at all -- argparse rejects with exit 2.
        rc = _run(env, "queue", "force-start", d1["id"])
        assert rc.returncode == 2, rc.stderr
        assert "reason" in (rc.stderr.lower() + rc.stdout.lower())


def test_force_start_refuses_empty_reason():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "blocked", ["scope:bar"])
        d1 = json.loads(r1.stdout)

        # Whitespace-only --reason -- our own check, exit 1.
        rc = _run(
            env, "queue", "force-start", d1["id"], "--reason", "   ",
        )
        assert rc.returncode == 1, rc.stderr
        assert "reason" in rc.stderr.lower()


# ---------------------------------------------------------------------------
# 4. Refuse: id not found
# ---------------------------------------------------------------------------


def test_force_start_refuses_not_found():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        rc = _run(
            env, "queue", "force-start", "q-does-not-exist",
            "--reason", "ghost",
        )
        assert rc.returncode == 1, rc.stderr
        assert "not found" in rc.stderr.lower()


# ---------------------------------------------------------------------------
# 5. Audit log row written
# ---------------------------------------------------------------------------


def test_force_start_writes_audit_log():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        log_path = Path(env["QUEUE_FORCE_START_LOG"])

        r1 = _add(env, "blocker", ["scope:bar"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:bar"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "audit-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        assert log_path.exists(), "audit log file not created"
        rows = [
            json.loads(line)
            for line in log_path.read_text().splitlines() if line.strip()
        ]
        assert len(rows) == 1, rows
        row = rows[0]
        assert row["queue_id"] == d2["id"]
        assert row["reason"] == "audit-test"
        assert "blockers_overridden" in row
        overridden_ids = [b["id"] for b in row["blockers_overridden"]]
        assert d1["id"] in overridden_ids, overridden_ids
        # Timestamp is unix epoch (int) and matches what's on the queue item.
        assert isinstance(row["timestamp"], int)
        promoted = _show(env, d2["id"])
        assert row["timestamp"] == promoted["force_started_at"]


# ---------------------------------------------------------------------------
# 6. Claude-event emitted with force_started=true
# ---------------------------------------------------------------------------


def test_force_start_emits_claude_event():
    """The lifecycle emit should include `force_started=true` in the data
    block so downstream consumers (work-queue-exporter, signal bot, etc.)
    can branch on the override path. We assert by reading the emitted
    JSON file out of the per-test claude-events dir.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        events_dir = Path(tmp) / "claude-events"

        r1 = _add(env, "blocker", ["scope:baz"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        # Drain any events from the register call so we only see the
        # force-start emit below.
        for f in events_dir.iterdir():
            f.unlink()

        r2 = _add(env, "blocked", ["scope:baz"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "event-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        emitted = []
        for f in sorted(events_dir.iterdir()):
            try:
                emitted.append(json.loads(f.read_text()))
            except (OSError, ValueError):
                continue

        # Find a queue-running event whose data carries force_started=true.
        # Note: claude-event's `--data KEY=VAL` flattens the value through a
        # shell argument, so booleans/lists land in the JSON event as their
        # `str()` rendering ("True", "[\"q-...\"]"). The semantic check is
        # that the field is present AND truthy after str-coercion.
        def _truthy(v):
            return str(v).lower() in ("true", "1", "yes")

        matching = [
            e for e in emitted
            if e.get("tag") == "queue-running"
            and _truthy((e.get("data") or {}).get("force_started"))
        ]
        assert matching, (
            f"expected a queue-running event with force_started=true, got "
            f"{[(e.get('tag'), (e.get('data') or {}).get('force_started')) for e in emitted]}"
        )
        ev = matching[0]
        assert ev["data"]["queue_id"] == d2["id"]
        assert ev["data"]["force_started_reason"] == "event-test"


# ---------------------------------------------------------------------------
# 7. Dedicated `force-start` claude-event emitted alongside `queue-running`
# ---------------------------------------------------------------------------


def test_force_start_emits_dedicated_force_start_event():
    """A force-start should emit BOTH a `queue-running` event (for the
    standard lifecycle bus) AND a dedicated `force-start` event so
    `claude-event-watch` surfaces force-starts to the main loop with a
    distinct tag (Andrew DM 2026-05-02 19:54 ET: "force starting should
    both emit an event AND add a hard obligation to spawn").
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        events_dir = Path(tmp) / "claude-events"

        r1 = _add(env, "blocker", ["scope:fs"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        # Drain register's events.
        for f in events_dir.iterdir():
            f.unlink()

        r2 = _add(env, "blocked", ["scope:fs"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "fs-event-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        emitted = []
        for f in sorted(events_dir.iterdir()):
            try:
                emitted.append(json.loads(f.read_text()))
            except (OSError, ValueError):
                continue

        force_events = [e for e in emitted if e.get("tag") == "force-start"]
        assert force_events, (
            f"expected a `force-start` event, got tags="
            f"{[e.get('tag') for e in emitted]}"
        )
        ev = force_events[0]
        data = ev.get("data") or {}
        assert data.get("queue_id") == d2["id"]
        assert data.get("force_started_reason") == "fs-event-test"


# ---------------------------------------------------------------------------
# 8. Force-start registers a `force_started_unspawned` obligation
# ---------------------------------------------------------------------------


def test_force_start_registers_obligation():
    """Force-start should register a hard-gate obligation that DENIES every
    non-exempt main-loop tool call until an Agent has been dispatched for
    the promoted queue id. Verified by inspecting the per-test
    obligations.json (HOME-isolated -- the live ~/.config/claude/
    obligations.json is never touched).
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "blocker", ["scope:obx"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:obx"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "obligation-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        assert ob_path.exists(), (
            f"expected obligations.json at {ob_path}, but it was not written"
        )
        ob_data = json.loads(ob_path.read_text())
        matching = [
            ob for ob in ob_data.get("obligations", [])
            if ob.get("predicate", {}).get("kind") == "force_started_unspawned"
            and ob.get("predicate", {}).get("params", {}).get("queue_id")
                == d2["id"]
        ]
        assert matching, (
            f"expected a force_started_unspawned obligation for {d2['id']!r}, "
            f"got {[ob.get('predicate') for ob in ob_data.get('obligations',[])]}"
        )
        ob = matching[0]
        assert ob.get("tool_pattern") == "*"
        assert ob.get("enforcement", "gate") == "gate"
        assert ob.get("created_by", "").startswith("force-start:")
        assert ob.get("ttl_secs", 0) > 0  # has a TTL safety net


def test_force_start_obligation_suppressed_by_env():
    """`OBLIGATIONS_FORCE_START=0` skips the obligation register call.
    Used by upstream test harnesses (e.g. queue-minisite) that exercise
    the force-start endpoint without wanting to mutate obligations state.
    The claude-event still emits and the queue still flips -- ONLY the
    obligation register is suppressed.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        env["OBLIGATIONS_FORCE_START"] = "0"

        r1 = _add(env, "blocker", ["scope:obx2"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:obx2"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "ob-suppressed", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        # File may exist from an unrelated read, but must not contain a
        # force_started_unspawned row for d2.
        if ob_path.exists():
            ob_data = json.loads(ob_path.read_text())
            matching = [
                ob for ob in ob_data.get("obligations", [])
                if (ob.get("predicate", {}).get("kind")
                    == "force_started_unspawned")
                and ob.get("predicate", {}).get("params", {}).get("queue_id")
                    == d2["id"]
            ]
            assert not matching, (
                f"expected NO obligation registered when "
                f"OBLIGATIONS_FORCE_START=0, got {matching}"
            )


# ---------------------------------------------------------------------------
# 9. Autostop overlapping running peers + recovery bundle
# ---------------------------------------------------------------------------


def test_force_start_autostops_overlapping_running_peer():
    """A force-start must abandon every RUNNING item whose scope OVERLAPS
    the force-started item's scope, with a clear abandon_reason. Andrew
    2026-05-02 21:10 UTC: "force-start should ALSO autostop any RUNNING
    work whose scope OVERLAPS the force-started item".
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Establish a running item that holds scope:foo. This is the peer
        # that should get autostopped.
        r1 = _add(env, "the-running-peer", ["scope:foo"])
        d1 = json.loads(r1.stdout)
        assert _register(env, d1["id"], "--json").returncode == 0

        # Force-enqueue a blocked-pending sibling on the same scope.
        r2 = _add(env, "the-interrupter", ["scope:foo"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        # Force-start: should autostop d1.
        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "interrupting", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        # d1 should now be abandoned with the autostop reason.
        d1_after = _show(env, d1["id"])
        assert d1_after["status"] == "abandoned", d1_after
        assert "autostopped by force-start" in (
            d1_after.get("abandon_reason", "")
        ), d1_after.get("abandon_reason")
        assert d1_after.get("autostopped_by_force_start") == d2["id"]

        # The promoted record should reference its autostopped peers.
        promoted_out = json.loads(rs.stdout)
        assert d1["id"] in promoted_out.get(
            "force_started_autostopped_peers", []
        ), promoted_out


def test_force_start_does_not_touch_disjoint_running_peer():
    """A running peer whose scope does NOT overlap the force-started item
    must be left RUNNING. The autostop is scope-overlap-driven.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        # Running peer on a disjoint scope.
        r1 = _add(env, "unrelated", ["scope:other"])
        d1 = json.loads(r1.stdout)
        assert _register(env, d1["id"], "--json").returncode == 0

        # Running peer on a SECOND disjoint scope, just for good measure.
        r2 = _add(env, "also-unrelated", ["scope:third"])
        d2 = json.loads(r2.stdout)
        assert _register(env, d2["id"], "--json").returncode == 0

        # Force-start a fresh item on scope:foo (overlaps neither).
        r3 = _add(env, "interrupter", ["scope:foo"])
        d3 = json.loads(r3.stdout)

        rs = _run(
            env, "queue", "force-start", d3["id"],
            "--reason", "no-conflict", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        # Both unrelated peers should still be running.
        assert _show(env, d1["id"])["status"] == "running"
        assert _show(env, d2["id"])["status"] == "running"

        # Promoted record's autostopped-peers list is empty.
        promoted_out = json.loads(rs.stdout)
        assert promoted_out.get("force_started_autostopped_peers", []) == []


def test_force_start_writes_recovery_bundle_with_autostop():
    """When a peer is autostopped, the recovery bundle JSON must be written
    at FORCE_START_BUNDLE_DIR/<q-X>.json with the peer's queue context.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "running-peer", ["scope:bundle"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "interrupter", ["scope:bundle"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "bundle-write", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        bundle_dir = Path(env["FORCE_START_BUNDLE_DIR"])
        bundle_path = bundle_dir / f"{d2['id']}.json"
        assert bundle_path.exists(), (
            f"expected bundle at {bundle_path}, dir contents = "
            f"{list(bundle_dir.iterdir()) if bundle_dir.exists() else 'no dir'}"
        )

        bundle = json.loads(bundle_path.read_text())
        assert bundle["force_started_queue_id"] == d2["id"]
        assert bundle["force_started_reason"] == "bundle-write"
        peers = bundle["autostopped_peers"]
        assert len(peers) == 1, peers
        peer = peers[0]
        assert peer["queue_id"] == d1["id"]
        assert peer["summary"]
        assert peer["scope"] == ["scope:bundle"]
        assert peer["abandon_reason"].startswith("autostopped by force-start")
        # Repo snapshots and prompt are best-effort and may be empty in
        # the test sandbox (no claude-watch active-agents.json), but the
        # keys must exist.
        assert "repo_snapshots" in peer
        assert "original_prompt" in peer
        assert "agent_kill_outcome" in peer

        # Promoted JSON should also surface the bundle path.
        promoted_out = json.loads(rs.stdout)
        assert promoted_out.get("force_started_recovery_bundle_path") == \
            str(bundle_path), promoted_out


def test_force_start_writes_empty_bundle_when_no_autostop():
    """Bundle is always written (per spec — empty-list case is a useful
    "force-started in the clear" signal). Verify the empty-peers shape.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "lonely", ["scope:empty"])
        d1 = json.loads(r1.stdout)

        rs = _run(
            env, "queue", "force-start", d1["id"],
            "--reason", "empty-bundle", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        bundle_dir = Path(env["FORCE_START_BUNDLE_DIR"])
        bundle_path = bundle_dir / f"{d1['id']}.json"
        assert bundle_path.exists()
        bundle = json.loads(bundle_path.read_text())
        assert bundle["autostopped_peers"] == []


def test_force_start_event_carries_recovery_bundle_path():
    """The dedicated `force-start` claude-event's data must include both
    `recovery_bundle_path` and `autostopped_peers` so the main loop can
    paste them into the spawned Agent's prompt.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        events_dir = Path(tmp) / "claude-events"

        r1 = _add(env, "blocker", ["scope:evt"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        for f in events_dir.iterdir():
            f.unlink()

        r2 = _add(env, "blocked", ["scope:evt"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "evt-test", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        emitted = []
        for f in sorted(events_dir.iterdir()):
            try:
                emitted.append(json.loads(f.read_text()))
            except (OSError, ValueError):
                continue

        force_events = [e for e in emitted if e.get("tag") == "force-start"]
        assert force_events, [e.get("tag") for e in emitted]
        ev = force_events[0]
        data = ev.get("data") or {}
        # claude-event flattens list/None values via str() so we assert by
        # presence + truthy-substring rather than direct typed equality.
        bundle_path = data.get("recovery_bundle_path")
        assert bundle_path, data
        assert d2["id"] in str(bundle_path), data
        assert d1["id"] in str(data.get("autostopped_peers")), data


def test_force_start_audit_log_records_autostopped_peers():
    """The QUEUE_FORCE_START_LOG row must include the autostopped peers'
    queue ids so post-incident auditors can reconstruct the override.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        log_path = Path(env["QUEUE_FORCE_START_LOG"])

        r1 = _add(env, "blocker", ["scope:audit"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:audit"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "audit-autostop", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        rows = [
            json.loads(line)
            for line in log_path.read_text().splitlines() if line.strip()
        ]
        assert rows
        row = rows[-1]
        assert row["queue_id"] == d2["id"]
        assert d1["id"] in row.get("autostopped_peers", []), row


def test_force_start_obligation_message_includes_bundle_path():
    """The deny banner persisted in obligations.json should reference the
    recovery bundle path so the main loop sees it on the gate fire.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        r1 = _add(env, "blocker", ["scope:obmsg"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "blocked", ["scope:obmsg"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "deny-banner", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        ob_path = Path(tmp) / ".config" / "claude" / "obligations.json"
        assert ob_path.exists(), "obligations.json not written"
        ob_data = json.loads(ob_path.read_text())
        matching = [
            ob for ob in ob_data.get("obligations", [])
            if ob.get("predicate", {}).get("kind") == "force_started_unspawned"
            and ob.get("predicate", {}).get("params", {}).get("queue_id") == d2["id"]
        ]
        assert matching, "force_started_unspawned obligation not registered"
        # The obligations CLI stores the deny banner under `deny_message`
        # (writable via `obligations add --deny-msg ...`). Older deploys
        # may have used `deny_msg`; fall back gracefully.
        ob = matching[0]
        deny_msg = (
            ob.get("deny_message")
            or ob.get("deny_msg")
            or ob.get("message", "")
        )
        assert d2["id"] in deny_msg, f"deny_msg missing q-id: {deny_msg!r}"
        # Bundle path mentions the queue id (deterministic filename).
        assert f"{d2['id']}.json" in deny_msg, deny_msg


def test_force_start_repo_snapshot_captured_in_bundle():
    """When a scope token resolves to a real git repo, the bundle should
    capture `git status` / `git diff` for that working tree. We seed a
    tiny git repo under HOME=tmp and assert the snapshot lands.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # Make ~/repos/myrepo a real git working tree with a dirty file.
        repos_dir = Path(tmp) / "repos" / "myrepo"
        repos_dir.mkdir(parents=True)
        subprocess.run(
            ["git", "init", "-q", str(repos_dir)],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["git", "-C", str(repos_dir), "config", "user.email", "t@t"],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["git", "-C", str(repos_dir), "config", "user.name", "t"],
            check=True, capture_output=True,
        )
        # Initial commit so HEAD exists.
        (repos_dir / "README.md").write_text("hello\n")
        subprocess.run(
            ["git", "-C", str(repos_dir), "add", "README.md"],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["git", "-C", str(repos_dir), "commit", "-q", "-m", "init"],
            check=True, capture_output=True,
        )
        # Now leave a dirty modification + an untracked file.
        (repos_dir / "README.md").write_text("hello\nworld\n")
        (repos_dir / "scratch.txt").write_text("uncommitted\n")

        # Running peer scoped to that repo.
        r1 = _add(env, "peer", ["repo:myrepo"])
        d1 = json.loads(r1.stdout)
        _register(env, d1["id"], "--json")
        r2 = _add(env, "interrupter", ["repo:myrepo"], "--force-enqueue")
        d2 = json.loads(r2.stdout)

        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "repo-snap", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        bundle_path = Path(env["FORCE_START_BUNDLE_DIR"]) / f"{d2['id']}.json"
        bundle = json.loads(bundle_path.read_text())
        peers = bundle["autostopped_peers"]
        assert len(peers) == 1
        snaps = peers[0]["repo_snapshots"]
        assert snaps, "expected at least one repo snapshot"
        snap = snaps[0]
        assert "myrepo" in snap["path"]
        # Untracked + modified files should both surface in porcelain status.
        assert "scratch.txt" in snap["status"]
        assert "README.md" in snap["status"]
        # Diff carries the README.md edit.
        assert "world" in snap["diff"], snap["diff"]


# ---------------------------------------------------------------------------
# Entry point for direct invocation
# ---------------------------------------------------------------------------


def _all_tests():
    return [
        test_force_start_promotes_blocked_pending,
        test_force_start_refuses_already_running,
        test_force_start_refuses_no_reason,
        test_force_start_refuses_empty_reason,
        test_force_start_refuses_not_found,
        test_force_start_writes_audit_log,
        test_force_start_emits_claude_event,
        test_force_start_emits_dedicated_force_start_event,
        test_force_start_registers_obligation,
        test_force_start_obligation_suppressed_by_env,
        test_force_start_autostops_overlapping_running_peer,
        test_force_start_does_not_touch_disjoint_running_peer,
        test_force_start_writes_recovery_bundle_with_autostop,
        test_force_start_writes_empty_bundle_when_no_autostop,
        test_force_start_event_carries_recovery_bundle_path,
        test_force_start_audit_log_records_autostopped_peers,
        test_force_start_obligation_message_includes_bundle_path,
        test_force_start_repo_snapshot_captured_in_bundle,
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
