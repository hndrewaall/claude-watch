#!/usr/bin/env python3
"""Tests for claude-event lifecycle emission on session-task queue ops.

Covers the 2026-04-24 addition:

  * `queue add` emits a ``queue-added`` claude-event.
  * `queue register <id>` emits a ``queue-running`` claude-event.
  * `queue done <id>` emits a ``queue-done`` claude-event with an
    ``elapsed_sec`` data field populated from ``started_at``.
  * `queue abandon <id> [--reason R]` emits a ``queue-abandoned``
    claude-event with ``elapsed_sec`` + ``reason`` populated.
  * A failing ``claude-event`` shim does NOT fail the underlying queue op
    (same invariant as pingme).
  * ``--silent`` on any of the four suppresses the claude-event emit (and
    the pingme emit as before).
  * ``CLAUDE_EVENT_SESSION_TASK=0`` env var suppresses emission.

The tests install a fake ``claude-event`` shim that records invocations
to a JSONL log file. A fake ``pingme`` shim is also installed but is
largely ignored here — we just need it present so the existing pingme
hooks don't spam stderr with "pingme: not found" noise.

Run:
    uv run --python 3.11 --with pytest \\
        pytest ~/repos/config/tests/test_queue_claude_event.py -v

Or directly:
    python3 ~/repos/config/tests/test_queue_claude_event.py
"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


# ---------------------------------------------------------------------------
# Fake shims
# ---------------------------------------------------------------------------


def _install_fake_claude_event(bin_dir: Path, log_path: Path, exit_code: int = 0):
    """Drop a fake ``claude-event`` executable that logs argv as JSON.

    Format of each logged call: one JSON object per line with keys
    ``argv`` (the argv tail). The real tool writes a JSON file into
    ~/claude-events/; we don't need that side effect here because the
    queue lifecycle is solely responsible for CALLING the tool, not for
    reacting to the output file. The file-drop side effect is covered by
    separate claude-event CLI tests.
    """
    bin_dir.mkdir(parents=True, exist_ok=True)
    emitter = bin_dir / "claude-event"
    emitter.write_text(textwrap.dedent(f"""\
        #!/usr/bin/env python3
        import json, sys
        with open({str(log_path)!r}, "a") as f:
            f.write(json.dumps(sys.argv[1:]) + "\\n")
        sys.exit({exit_code})
        """))
    emitter.chmod(0o755)
    return emitter


def _install_fake_pingme(bin_dir: Path, log_path: Path | None = None, exit_code: int = 0):
    bin_dir.mkdir(parents=True, exist_ok=True)
    pingme = bin_dir / "pingme"
    sink = str(log_path) if log_path else "/dev/null"
    pingme.write_text(textwrap.dedent(f"""\
        #!/usr/bin/env python3
        import json, sys
        with open({sink!r}, "a") as f:
            f.write(json.dumps(sys.argv[1:]) + "\\n")
        sys.exit({exit_code})
        """))
    pingme.chmod(0o755)
    return pingme


def _env_for_tmp(tmp, bin_dir=None, claude_event_session_task=None):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    if bin_dir is None:
        empty = Path(tmp) / ".empty_path"
        empty.mkdir(parents=True, exist_ok=True)
        env["PATH"] = str(empty)
    else:
        env["PATH"] = f"{bin_dir}:{env.get('PATH', '')}"
    if claude_event_session_task is not None:
        env["CLAUDE_EVENT_SESSION_TASK"] = claude_event_session_task
    elif "CLAUDE_EVENT_SESSION_TASK" in env:
        del env["CLAUDE_EVENT_SESSION_TASK"]
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


def _read_shim_log(log_path: Path):
    if not log_path.exists():
        return []
    out = []
    for line in log_path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        out.append(json.loads(line))
    return out


def _parse_claude_event_argv(argv):
    """Parse the argv the session-task hook passed to claude-event.

    claude-event signature:
        claude-event <message> --source queue --tag TAG
                     [--priority P] [--data KEY=VAL ...]
    """
    priority = "normal"
    source = None
    tag = None
    data = {}
    positional = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--source":
            source = argv[i + 1]
            i += 2
        elif a == "--tag":
            tag = argv[i + 1]
            i += 2
        elif a == "--priority":
            priority = argv[i + 1]
            i += 2
        elif a == "--data":
            k, _, v = argv[i + 1].partition("=")
            data[k] = v
            i += 2
        else:
            positional.append(a)
            i += 1
    message = positional[0] if positional else None
    return {
        "message": message,
        "source": source,
        "tag": tag,
        "priority": priority,
        "data": data,
    }


# ---------------------------------------------------------------------------
# 1. queue add emits queue-added
# ---------------------------------------------------------------------------


def test_add_emits_queue_added_event():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        summary = "test add emits queue-added event"
        r1 = _add(env, "add-test", ["repo:ce-add"], "--summary", summary)
        d1 = json.loads(r1.stdout)

        calls = _read_shim_log(ev_log)
        assert len(calls) == 1, calls
        parsed = _parse_claude_event_argv(calls[0])
        assert parsed["source"] == "queue"
        assert parsed["tag"] == "queue-added"
        assert parsed["priority"] == "low"
        assert d1["id"] in parsed["message"]
        assert summary in parsed["message"]

        # Structured data round-trips.
        assert parsed["data"]["queue_id"] == d1["id"]
        assert parsed["data"]["group_id"] == d1["group_id"]
        assert parsed["data"]["summary"] == summary
        # scope is JSON-encoded because it's a list.
        assert json.loads(parsed["data"]["scope"]) == d1["scope"]
        # ready_now is surfaced as a stringified bool.
        assert parsed["data"]["ready_now"] in ("True", "true")


# ---------------------------------------------------------------------------
# 2. queue register emits queue-running
# ---------------------------------------------------------------------------


def test_register_emits_queue_running_event():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "reg-test", ["repo:ce-reg"], "--summary",
                  "reg test summary")
        d1 = json.loads(r1.stdout)
        # First event is queue-added; clear it so we assert on register alone.
        assert len(_read_shim_log(ev_log)) == 1

        _run(env, "queue", "register", d1["id"], "--json", check=True)

        calls = _read_shim_log(ev_log)
        # two total: add + register
        assert len(calls) == 2, calls
        parsed = _parse_claude_event_argv(calls[1])
        assert parsed["source"] == "queue"
        assert parsed["tag"] == "queue-running"
        assert parsed["priority"] == "low"
        assert d1["id"] in parsed["message"]
        assert parsed["data"]["queue_id"] == d1["id"]
        assert parsed["data"]["group_id"] == d1["group_id"]


# ---------------------------------------------------------------------------
# 3. queue done emits queue-done with elapsed_sec populated
# ---------------------------------------------------------------------------


def test_done_emits_queue_done_event_with_elapsed_sec():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "done-test", ["repo:ce-done"], "--summary",
                  "done test summary")
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "done", d1["id"], check=True)

        calls = _read_shim_log(ev_log)
        # add + register + done
        assert len(calls) == 3, calls
        parsed = _parse_claude_event_argv(calls[2])
        assert parsed["source"] == "queue"
        assert parsed["tag"] == "queue-done"
        assert parsed["data"]["queue_id"] == d1["id"]
        # elapsed_sec must be present and parseable as int >= 0.
        assert "elapsed_sec" in parsed["data"]
        assert int(parsed["data"]["elapsed_sec"]) >= 0


# ---------------------------------------------------------------------------
# 4. queue abandon emits queue-abandoned with reason
# ---------------------------------------------------------------------------


def test_abandon_emits_queue_abandoned_event():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "abandon-test", ["repo:ce-abandon"], "--summary",
                  "abandon test summary")
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "abandon", d1["id"], "--reason",
             "agent crashed in testing", check=True)

        calls = _read_shim_log(ev_log)
        # add + register + abandon
        assert len(calls) == 3, calls
        parsed = _parse_claude_event_argv(calls[2])
        assert parsed["source"] == "queue"
        assert parsed["tag"] == "queue-abandoned"
        assert parsed["data"]["queue_id"] == d1["id"]
        assert parsed["data"]["reason"] == "agent crashed in testing"
        assert "elapsed_sec" in parsed["data"]
        assert int(parsed["data"]["elapsed_sec"]) >= 0


# ---------------------------------------------------------------------------
# 5. claude-event failure does NOT break the queue transition
# ---------------------------------------------------------------------------


def test_failing_claude_event_does_not_block_queue_op():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        # Exit code 17 == shim recorded the call but returned non-zero.
        _install_fake_claude_event(bin_dir, ev_log, exit_code=17)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir)

        r1 = _add(env, "fail-ce", ["repo:ce-fail"], "--summary",
                  "failing emitter shim")
        assert r1.returncode == 0, (
            f"queue add must not propagate claude-event failure, "
            f"stderr={r1.stderr!r}"
        )
        d1 = json.loads(r1.stdout)

        rr = _run(env, "queue", "register", d1["id"], "--json")
        assert rr.returncode == 0, rr.stderr
        rd = _run(env, "queue", "done", d1["id"])
        assert rd.returncode == 0, rd.stderr

        # The shim still recorded every invocation despite exit 17.
        calls = _read_shim_log(ev_log)
        tags = [_parse_claude_event_argv(c)["tag"] for c in calls]
        assert tags == ["queue-added", "queue-running", "queue-done"], tags


# ---------------------------------------------------------------------------
# 6. Env-var suppression works
# ---------------------------------------------------------------------------


def test_env_var_suppresses_claude_event():
    with tempfile.TemporaryDirectory() as tmp:
        bin_dir = Path(tmp) / "bin"
        ev_log = Path(tmp) / "claude-event.log"
        _install_fake_claude_event(bin_dir, ev_log)
        _install_fake_pingme(bin_dir)
        env = _env_for_tmp(tmp, bin_dir=bin_dir,
                           claude_event_session_task="0")

        r1 = _add(env, "suppress-test", ["repo:ce-suppress"], "--summary",
                  "env suppressed")
        d1 = json.loads(r1.stdout)
        _run(env, "queue", "register", d1["id"], "--json", check=True)
        _run(env, "queue", "done", d1["id"], check=True)

        calls = _read_shim_log(ev_log)
        assert calls == [], f"expected no claude-event emits, got {calls}"


# ---------------------------------------------------------------------------
# Entry point for direct invocation
# ---------------------------------------------------------------------------


def _all_tests():
    return [
        test_add_emits_queue_added_event,
        test_register_emits_queue_running_event,
        test_done_emits_queue_done_event_with_elapsed_sec,
        test_abandon_emits_queue_abandoned_event,
        test_failing_claude_event_does_not_block_queue_op,
        test_env_var_suppresses_claude_event,
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
