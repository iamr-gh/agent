"""Offline run-controller tests, independent of Discord."""
import asyncio
from pathlib import Path
import tempfile
import unittest

from runner import Busy, Runs


class RunTests(unittest.IsolatedAsyncioTestCase):
    async def test_parallel_sessions_conflicts_status_and_targeted_cancel(self):
        runs = Runs()
        with tempfile.TemporaryDirectory() as root:
            first = Path(root) / "first"
            second = Path(root) / "second"
            first.mkdir()
            second.mkdir()
            gate = asyncio.Event()
            tasks = [asyncio.create_task(runs.submit(key, path, gate.wait))
                     for key, path in [(1, first), (2, second)]]
            await asyncio.sleep(0)
            self.assertEqual(set(runs.active), {1, 2})
            self.assertEqual(runs.status(1), "running")
            runs.event(1, {"type": "approval"})
            self.assertEqual(runs.status(1), "awaiting approval")
            with self.assertRaises(Busy):
                await runs.submit(1, second, gate.wait)
            with self.assertRaises(Busy):
                await runs.submit(3, first / ".", gate.wait)
            self.assertTrue(runs.cancel(1))
            with self.assertRaises(asyncio.CancelledError):
                await tasks[0]
            self.assertEqual(runs.status(1), "idle")
            self.assertFalse(tasks[1].done())
            self.assertFalse(runs.cancel(999))
            gate.set()
            await tasks[1]
            self.assertEqual(runs.active, {})
            await runs.submit(3, first, gate.wait)

    async def test_error_and_disconnect_release_all_reservations(self):
        runs = Runs()
        with tempfile.TemporaryDirectory() as root:
            async def fail():
                raise RuntimeError("synthetic")
            with self.assertRaises(RuntimeError):
                await runs.submit(1, root, fail)
            self.assertEqual(runs.status(1), "idle")
            task = asyncio.create_task(runs.submit(2, root, asyncio.Event().wait))
            await asyncio.sleep(0)
            runs.cancel_all()
            with self.assertRaises(asyncio.CancelledError):
                await task
            self.assertEqual(runs.active, {})


if __name__ == "__main__":
    unittest.main()
