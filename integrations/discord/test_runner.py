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
            queued = asyncio.create_task(runs.submit(3, first / ".", gate.wait))
            await asyncio.sleep(0)
            self.assertEqual(runs.queue_position(3), 1)
            self.assertEqual(runs.checkout_owner(first), 1)
            self.assertTrue(runs.cancel(1))
            with self.assertRaises(asyncio.CancelledError):
                await tasks[0]
            self.assertEqual(runs.status(1), "idle")
            self.assertFalse(tasks[1].done())
            self.assertFalse(runs.cancel(999))
            gate.set()
            await asyncio.wait_for(asyncio.gather(tasks[1], queued), 2)
            self.assertEqual(runs.active, {})

    async def test_fifo_skips_cancelled_waiter_and_puts_quota_resume_at_tail(self):
        runs = Runs()
        with tempfile.TemporaryDirectory() as root:
            pause = asyncio.Event()
            resume = asyncio.Event()
            quota_waiting = asyncio.Event()
            release = asyncio.Event()
            order = []

            async def notify(position):
                self.assertEqual(position, 3)
                quota_waiting.set()

            async def quota_turn():
                runs.event(1, {"type": "quota_paused"})
                pause.set()
                await resume.wait()
                await runs.reserve_after_quota(1, notify)
                order.append(1)

            async def turn(key):
                order.append(key)
                await release.wait()

            tasks = [asyncio.create_task(runs.submit(1, root, quota_turn))]
            try:
                await asyncio.wait_for(pause.wait(), 2)
                for key in (2, 3, 4):
                    tasks.append(asyncio.create_task(runs.submit(key, root, lambda key=key: turn(key))))
                    await asyncio.sleep(0)
                self.assertEqual(order, [2])
                self.assertEqual(runs.queue_position(4), 2)
                resume.set()
                await asyncio.wait_for(quota_waiting.wait(), 2)
                self.assertTrue(runs.cancel(3))
                with self.assertRaises(asyncio.CancelledError):
                    await tasks[2]
                self.assertEqual(runs.queue_position(4), 1)
                self.assertEqual(runs.queue_position(1), 2)
                release.set()
                await asyncio.wait_for(asyncio.gather(tasks[0], tasks[1], tasks[3]), 2)
                self.assertEqual(order, [2, 4, 1])
                self.assertFalse(runs.active)
                self.assertFalse(runs.waiting)
            finally:
                runs.cancel_all()
                await asyncio.gather(*tasks, return_exceptions=True)

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
