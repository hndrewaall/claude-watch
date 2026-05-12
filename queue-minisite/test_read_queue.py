#!/usr/bin/env python3
"""Tests for queue-minisite's ``_read_queue()`` error-vs-empty handling.

The original behavior treated ENOENT as a hard error, surfacing a red
"queue.json unreadable: [Errno 2] No such file or directory" banner on a
fresh dev box where ``$HOME/.config/session/queue.json`` doesn't exist
yet. The fix distinguishes FileNotFoundError (legitimate fresh-install
state — return an empty skeleton, no error) from other OSErrors
(permission denied etc. — still an error).

Run::

    python3 queue-minisite/test_read_queue.py
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _load_app(queue_path: Path):
    """Import app.py with QUEUE_JSON pointed at ``queue_path``.

    Other env knobs are pointed at scratch paths that don't need to
    exist — _read_queue() only consults QUEUE_PATH (= QUEUE_JSON).
    """
    os.environ["QUEUE_JSON"] = str(queue_path)
    os.environ.setdefault("AGENT_STATE_JSON", str(queue_path.parent / "no-agents.json"))
    os.environ.setdefault("AGENTS_JSONL_ROOT", str(queue_path.parent / "no-jsonl"))
    os.environ.setdefault("QUEUE_LOG_ARCHIVE_DIR", str(queue_path.parent / "no-archive"))
    os.environ.setdefault("WORKLOAD_LOG_DIR", str(queue_path.parent / "no-workloads"))

    sys.path.insert(0, str(HERE))
    for mod in list(sys.modules):
        if mod in ("app", "claude_agents"):
            del sys.modules[mod]
    import app as appmod  # noqa: E402

    return appmod


class ReadQueueTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="qmin-read-queue-")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_read_queue_returns_empty_on_enoent(self):
        """Missing queue.json -> empty skeleton + no error.

        Reproduces the fresh-macOS-dev-box smoke-test bug: ``app.py``
        used to surface ENOENT as a red error banner. After the fix the
        UI sees the same shape as a real-but-empty queue.
        """
        missing = Path(self.tmp) / "does-not-exist.json"
        self.assertFalse(missing.exists())

        appmod = _load_app(missing)
        data, err = appmod._read_queue()

        self.assertIsNone(err)
        self.assertIsInstance(data, dict)
        self.assertEqual(data.get("items"), [])
        self.assertEqual(data.get("locked_scopes"), {})
        # Downstream consumers all use .get("items", []) — make sure the
        # skeleton is compatible with the dict-shape branch.
        self.assertTrue(isinstance(data, dict))

    def test_read_queue_returns_error_on_permission_denied(self):
        """Unreadable (mode 000) queue.json -> non-None error.

        Skipped when running as root (root bypasses file-mode perms).
        """
        if hasattr(os, "geteuid") and os.geteuid() == 0:
            self.skipTest("running as root — file-mode perms ignored")

        path = Path(self.tmp) / "queue.json"
        path.write_text(json.dumps({"items": [], "locked_scopes": {}}))
        os.chmod(path, 0o000)
        try:
            appmod = _load_app(path)
            data, err = appmod._read_queue()
            self.assertIsNotNone(err)
            self.assertIn("queue.json unreadable", err)
            self.assertEqual(data, {})
        finally:
            # Restore perms so tearDown's rmtree can clean up.
            os.chmod(path, 0o644)

    def test_read_queue_returns_error_on_malformed_json(self):
        """Non-JSON content -> ValueError path returns non-None error."""
        path = Path(self.tmp) / "queue.json"
        path.write_text("not json {{{")

        appmod = _load_app(path)
        data, err = appmod._read_queue()

        self.assertIsNotNone(err)
        self.assertIn("queue.json non-JSON", err)
        self.assertEqual(data, {})

    def test_read_queue_returns_error_on_unexpected_shape(self):
        """Top-level JSON that isn't an object -> shape error preserved."""
        path = Path(self.tmp) / "queue.json"
        path.write_text(json.dumps(["not", "a", "dict"]))

        appmod = _load_app(path)
        data, err = appmod._read_queue()

        self.assertIsNotNone(err)
        self.assertIn("unexpected queue.json shape", err)
        self.assertEqual(data, {})

    def test_read_queue_returns_real_payload(self):
        """Healthy queue.json -> data passes through, error is None."""
        path = Path(self.tmp) / "queue.json"
        payload = {
            "schema_version": 2,
            "items": [{"id": "q-test-0001", "status": "pending"}],
            "locked_scopes": {},
        }
        path.write_text(json.dumps(payload))

        appmod = _load_app(path)
        data, err = appmod._read_queue()

        self.assertIsNone(err)
        self.assertEqual(data, payload)


if __name__ == "__main__":
    unittest.main(verbosity=2)
