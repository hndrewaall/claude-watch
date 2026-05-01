#!/usr/bin/env python3
"""End-to-end test for the `claude-event` emitter + `claude-event-tail` reader.

These exercise the canonical `tools/claude-event/` scripts without
touching the live `~/claude-events/` queue or `~/.config/claude-events/`
log directory: every test sets `$CLAUDE_EVENT_QUEUE` and
`$CLAUDE_EVENT_LOG_DIR` to per-test tempdirs.

What's covered:

  * `claude-event` writes a well-formed JSON event with all required
    keys (timestamp, source, tag, message, priority, data).
  * `--data KEY=VAL` accumulates into the `data` dict.
  * `--source <invalid>` exits with code 2 and a stderr error.
  * Filename has the expected `<ns_ts>_<safe_tag>.json` shape.
  * `claude-event-tail` round-trips events written by the emitter when
    they're pre-loaded into the consumed-log file (the ring buffer that
    `claude-event-watch` populates after consuming a queue file).
  * `claude-event-tail --since 1h` filters by recency.
  * `claude-event-tail --tag` and `--source` filters work together.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
EMIT = REPO / "tools" / "claude-event" / "claude-event"
TAIL = REPO / "tools" / "claude-event" / "claude-event-tail"


def _run(args, env=None, stdin_text=None):
    proc = subprocess.run(
        [sys.executable, str(args[0]), *args[1:]],
        capture_output=True,
        text=True,
        env=env,
        input=stdin_text,
        timeout=30,
    )
    return proc.returncode, proc.stdout, proc.stderr


class ClaudeEventEmitterTest(unittest.TestCase):
    def setUp(self):
        self.td = tempfile.TemporaryDirectory()
        self.queue = Path(self.td.name) / "queue"
        self.queue.mkdir()
        self.env = {
            **os.environ,
            "CLAUDE_EVENT_QUEUE": str(self.queue),
            "CRON_EVENT_QUEUE": "",
            "USER": "test",
        }

    def tearDown(self):
        self.td.cleanup()

    def test_basic_emit_writes_well_formed_json(self):
        rc, out, err = _run(
            [EMIT, "hello world", "--tag", "smoke", "--source", "manual"],
            env=self.env,
        )
        self.assertEqual(rc, 0, f"rc={rc} stderr={err}")
        files = list(self.queue.glob("*.json"))
        self.assertEqual(len(files), 1, files)
        ev = json.loads(files[0].read_text())
        self.assertEqual(ev["message"], "hello world")
        self.assertEqual(ev["tag"], "smoke")
        self.assertEqual(ev["source"], "manual")
        self.assertEqual(ev["priority"], "normal")
        self.assertEqual(ev["data"], {})
        self.assertIn("timestamp", ev)
        self.assertIn("hostname", ev)

    def test_data_kv_accumulates(self):
        rc, _, err = _run(
            [
                EMIT, "msg",
                "--tag", "kv",
                "--source", "manual",
                "--data", "k1=v1",
                "--data", "k2=v2",
            ],
            env=self.env,
        )
        self.assertEqual(rc, 0, err)
        ev = json.loads(next(self.queue.glob("*.json")).read_text())
        self.assertEqual(ev["data"], {"k1": "v1", "k2": "v2"})

    def test_invalid_source_rejected(self):
        rc, _, err = _run(
            [EMIT, "msg", "--tag", "x", "--source", "not-a-valid-source"],
            env=self.env,
        )
        self.assertEqual(rc, 2)
        self.assertIn("source", err.lower())
        self.assertEqual(list(self.queue.glob("*.json")), [])

    def test_filename_shape(self):
        rc, out, _ = _run(
            [EMIT, "msg", "--tag", "abc-DEF_123", "--source", "cron"],
            env=self.env,
        )
        self.assertEqual(rc, 0)
        files = list(self.queue.glob("*.json"))
        self.assertEqual(len(files), 1)
        name = files[0].name
        # Expected: <ns_ts>_<safe_tag>.json
        ns, _, rest = name.partition("_")
        self.assertTrue(ns.isdigit(), name)
        self.assertTrue(rest.endswith(".json"), name)
        self.assertEqual(rest[: -len(".json")], "abc-DEF_123")

    def test_filename_sanitisation(self):
        rc, _, _ = _run(
            [EMIT, "msg", "--tag", "weird/path:tag", "--source", "cron"],
            env=self.env,
        )
        self.assertEqual(rc, 0)
        files = list(self.queue.glob("*.json"))
        self.assertEqual(len(files), 1)
        # Slashes / colons should be replaced with underscores
        self.assertNotIn("/", files[0].name)
        self.assertNotIn(":", files[0].name)


class ClaudeEventTailTest(unittest.TestCase):
    def setUp(self):
        self.td = tempfile.TemporaryDirectory()
        self.log_dir = Path(self.td.name) / "log"
        self.log_dir.mkdir()
        self.env = {
            **os.environ,
            "CLAUDE_EVENT_LOG_DIR": str(self.log_dir),
        }

    def tearDown(self):
        self.td.cleanup()

    def _write_log(self, events):
        log_file = self.log_dir / "consumed.jsonl"
        with log_file.open("w") as f:
            for ev in events:
                f.write(json.dumps(ev, separators=(",", ":")) + "\n")

    def test_tail_table_output(self):
        now = time.time()
        self._write_log([
            {"timestamp": now - 60, "source": "cron", "tag": "first", "message": "FIRST"},
            {"timestamp": now - 30, "source": "manual", "tag": "second", "message": "SECOND"},
        ])
        rc, out, err = _run([TAIL, "-n", "5"], env=self.env)
        self.assertEqual(rc, 0, err)
        self.assertIn("TIMESTAMP", out)
        self.assertIn("FIRST", out)
        self.assertIn("SECOND", out)

    def test_tail_json_output(self):
        now = time.time()
        self._write_log([
            {"timestamp": now, "source": "cron", "tag": "json", "message": "Hello"},
        ])
        rc, out, _ = _run([TAIL, "-n", "5", "--json"], env=self.env)
        self.assertEqual(rc, 0)
        ev = json.loads(out.strip())
        self.assertEqual(ev["message"], "Hello")
        self.assertEqual(ev["tag"], "json")

    def test_tail_since_filter(self):
        now = time.time()
        self._write_log([
            {"timestamp": now - 7200, "source": "cron", "tag": "old", "message": "OLD"},
            {"timestamp": now - 60, "source": "cron", "tag": "new", "message": "NEW"},
        ])
        rc, out, _ = _run([TAIL, "-n", "5", "--since", "30m"], env=self.env)
        self.assertEqual(rc, 0)
        self.assertIn("NEW", out)
        self.assertNotIn("OLD", out)

    def test_tail_tag_filter(self):
        now = time.time()
        self._write_log([
            {"timestamp": now, "source": "cron", "tag": "needle", "message": "MATCH"},
            {"timestamp": now, "source": "cron", "tag": "haystack", "message": "OTHER"},
        ])
        rc, out, _ = _run([TAIL, "-n", "5", "--tag", "needle"], env=self.env)
        self.assertEqual(rc, 0)
        self.assertIn("MATCH", out)
        self.assertNotIn("OTHER", out)

    def test_tail_source_filter(self):
        now = time.time()
        self._write_log([
            {"timestamp": now, "source": "cron", "tag": "x", "message": "FROM-CRON"},
            {"timestamp": now, "source": "manual", "tag": "x", "message": "FROM-MANUAL"},
        ])
        rc, out, _ = _run([TAIL, "-n", "5", "--source", "manual"], env=self.env)
        self.assertEqual(rc, 0)
        self.assertIn("FROM-MANUAL", out)
        self.assertNotIn("FROM-CRON", out)

    def test_tail_invalid_since_returns_2(self):
        rc, _, err = _run([TAIL, "--since", "not-a-duration"], env=self.env)
        self.assertEqual(rc, 2)
        self.assertIn("duration", err.lower())


if __name__ == "__main__":
    unittest.main(verbosity=2)
