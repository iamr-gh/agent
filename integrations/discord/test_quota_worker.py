"""Offline worker integration; set AGENT_TEST_BINARY to the Rust unit test executable."""
import asyncio
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import shlex
import signal
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

import bot
from runner import Worker, worker


BINARY = os.environ.get("AGENT_TEST_BINARY")


def quota(seconds=None):
    error = {"type": "usage_limit_reached"}
    if seconds is not None:
        error["resets_in_seconds"] = seconds
    return 429, json.dumps({"error": error})


def completed(item):
    return 200, "data: " + json.dumps({"type": "response.completed", "response": {
        "status": "completed", "output": [item]}}) + "\n\n"


def answer(text):
    return completed({"type": "message", "role": "assistant", "content": [
        {"type": "output_text", "text": text}]})


def tool():
    return completed({"type": "function_call", "call_id": "call-test", "name": "shell",
                      "arguments": json.dumps({"command": "printf x >> marker"})})


@contextmanager
def locked(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield
    finally:
        os.close(fd)


@unittest.skipUnless(BINARY, "set AGENT_TEST_BINARY to run the Rust worker integration")
class QuotaWorkerTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.checkout = self.root / "checkout"
        self.checkout.mkdir()
        self.wrapper = self.root / "worker"
        self.wrapper.write_text("#!/bin/sh\nexec " + shlex.quote(str(Path(BINARY).resolve()))
                                + " --exact worker::tests::loopback_worker --nocapture --quiet\n")
        self.wrapper.chmod(0o700)
        self.calls = []
        self.replies = []
        server = await asyncio.start_server(self.serve, "127.0.0.1", 0)
        self.addAsyncCleanup(server.wait_closed)
        self.addCleanup(server.close)
        self.env = {"PATH": os.defpath, "HOME": str(self.root), "AGENT_HOME": str(self.root),
                    "AGENT_TEST_WORKER_PORT": str(server.sockets[0].getsockname()[1]),
                    "AGENT_COMPACT_KIB": "0"}

        # The unit-test harness writes two startup lines before the worker protocol.
        original = Worker.events

        async def events(handle):
            self.assertEqual(await handle.process.stdout.readline(), b"\n")
            self.assertEqual(await handle.process.stdout.readline(), b"running 1 test\n")
            async for event in original(handle):
                yield event

        for obj, name, value in [(Worker, "events", events), (bot, "BINARY", self.wrapper),
                                 (bot, "STATE", self.root), (bot, "worker_env", lambda: self.env)]:
            p = patch.object(obj, name, value)
            p.start()
            self.addCleanup(p.stop)
        self.client = bot.AgentBot()
        self.addCleanup(self.client.db.close)
        self.addAsyncCleanup(self.client.close)
        self.thread = SimpleNamespace(id=123, send=AsyncMock())
        self.client.db.execute("INSERT INTO sessions (thread, workspace, auto_approve, max_turns) "
                               "VALUES (?, ?, 1, 2)", (self.thread.id, str(self.checkout)))
        self.client.db.commit()

    async def serve(self, reader, writer):
        try:
            headers = await reader.readuntil(b"\r\n\r\n")
            size = next(int(line.split(b":", 1)[1]) for line in headers.splitlines()
                        if line.lower().startswith(b"content-length:"))
            self.calls.append(json.loads(await reader.readexactly(size)))
            status, body = self.replies.pop(0) if self.replies else (500, "unexpected call")
            body = body.encode()
            writer.write(f"HTTP/1.1 {status} Test\r\nContent-Length: {len(body)}\r\n"
                         "Content-Type: text/event-stream\r\nConnection: close\r\n\r\n".encode() + body)
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    async def until(self, predicate):
        async with asyncio.timeout(5):
            while not predicate():
                await asyncio.sleep(0.01)

    def start(self, coro):
        task = asyncio.create_task(coro)
        self.addAsyncCleanup(self.finish_task, task)
        return task

    async def finish_task(self, task):
        if not task.done():
            task.cancel()
        await asyncio.gather(task, return_exceptions=True)

    def saved_id(self):
        return self.client.db.execute("SELECT session FROM sessions WHERE thread=?",
                                      (self.thread.id,)).fetchone()[0]

    def snapshot(self, sid):
        return json.loads((self.root / "sessions" / f"{sid}.json").read_text())

    def messages(self):
        return [call.args[0] for call in self.thread.send.await_args_list]

    async def test_resume_preserves_tools_budget_and_reserves_before_ack(self):
        self.replies = [tool(), quota(1), tool()]
        acked = asyncio.Event()
        original = Worker.send

        async def send(handle, value):
            if value == {"resume": True}:
                self.assertTrue(self.client.runs.active[self.thread.id]["owns_checkout"])
                acked.set()
            await original(handle, value)

        with patch.object(Worker, "send", send):
            task = self.start(self.client.submit(self.thread, "preserve this prompt"))
            await self.until(lambda: "paused" in self.client.runs.status(self.thread.id))
            sid = self.saved_id()
            self.assertFalse(self.snapshot(sid)["active"])
            with self.assertRaises(BlockingIOError), locked(self.root / "sessions" / f"{sid}.lock"):
                self.fail("session flock was released")
            with locked(self.checkout):
                await self.client.submit(self.thread, "duplicate turn")
                self.assertIn("already has an active turn", self.messages()[-1])
                blocker = asyncio.Event()
                other = self.start(self.client.runs.submit(456, self.checkout, blocker.wait))
                await self.until(lambda: "waiting for checkout" in self.client.runs.status(self.thread.id))
                self.assertFalse(acked.is_set())
                self.assertEqual(len(self.calls), 2)
                blocker.set()
                await other
                await asyncio.wait_for(acked.wait(), 3)
                self.assertEqual(len(self.calls), 2)  # Rust still needs the OS checkout lock.
            await asyncio.wait_for(task, 5)
        self.assertEqual(self.calls[1], self.calls[2])
        self.assertIn("preserve this prompt", json.dumps(self.calls[2]))
        self.assertEqual((self.checkout / "marker").read_text(), "xx")
        self.assertIn("2-model-call cap", self.messages()[-1])
        self.assertFalse(self.snapshot(sid)["active"])
        with locked(self.root / "sessions" / f"{sid}.lock"), locked(self.checkout):
            pass

    async def test_automatic_compaction_resumes_same_summary_then_infers(self):
        self.replies = [answer("seed")]
        await asyncio.wait_for(self.client.submit(self.thread, "seed"), 5)
        sid = self.saved_id()
        saved = self.snapshot(sid)
        saved["request"]["messages"] = [
            {"role": "user", "content": [{"type": "text", "text": f"history {i} " * 100}]}
            for i in range(12)]
        (self.root / "sessions" / f"{sid}.json").write_text(json.dumps(saved))
        self.env["AGENT_COMPACT_KIB"] = "1"
        self.replies = [quota(1), answer("compact handoff"), answer("task complete")]
        await asyncio.wait_for(self.client.submit(self.thread, "latest task"), 5)
        self.assertEqual(len(self.calls), 4)
        self.assertEqual(self.calls[1], self.calls[2])
        self.assertIn("Summarize the historical transcript", self.calls[2]["instructions"])
        self.assertIn("compact handoff", json.dumps(self.calls[3]["input"]))
        self.assertIn("latest task", json.dumps(self.calls[3]["input"]))
        self.assertEqual(self.messages()[-1], "task complete")

    async def test_stop_during_fallback_sleep(self):
        self.replies = [quota()]
        task = self.start(self.client.submit(self.thread, "stop test"))
        await self.until(lambda: any("5 hours (fallback" in msg for msg in self.messages()))
        interaction = SimpleNamespace(user=SimpleNamespace(id=bot.OWNER_ID), guild=object(),
                                      channel_id=self.thread.id,
                                      response=SimpleNamespace(send_message=AsyncMock()))
        await self.client.stop_turn(interaction)
        with self.assertRaises(asyncio.CancelledError):
            await asyncio.wait_for(task, 3)
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(self.client.runs.status(self.thread.id), "idle")
        with locked(self.checkout), locked(self.root / "sessions" / f"{self.saved_id()}.lock"):
            pass

    async def test_disconnect_while_waiting_for_adapter_checkout(self):
        self.replies = [quota(1)]
        task = self.start(self.client.submit(self.thread, "disconnect test"))
        await self.until(lambda: "paused" in self.client.runs.status(self.thread.id))
        other = self.start(self.client.runs.submit(456, self.checkout, asyncio.Event().wait))
        await self.until(lambda: "waiting for checkout" in self.client.runs.status(self.thread.id))
        await self.client.on_disconnect()
        for pending in (task, other):
            with self.assertRaises(asyncio.CancelledError):
                await asyncio.wait_for(pending, 3)
        self.assertEqual(len(self.calls), 1)
        self.assertFalse(self.client.runs.active)

    async def test_interrupt_while_waiting_for_resume_ack(self):
        self.replies = [quota(1)]
        async with worker(self.wrapper, self.checkout, self.env, self.root, 0,
                          {"prompt": "ack test"}) as handle:
            events = handle.events()
            await asyncio.wait_for(anext(events), 3)  # session
            self.assertEqual((await asyncio.wait_for(anext(events), 3))["type"], "quota_paused")
            self.assertEqual((await asyncio.wait_for(anext(events), 3))["type"], "quota_ready")
            handle.process.send_signal(signal.SIGINT)
            self.assertEqual(await asyncio.wait_for(handle.process.wait(), 3), 130)
        self.assertEqual(len(self.calls), 1)

    async def test_interrupt_while_waiting_for_os_checkout_after_ack(self):
        self.replies = [quota(1)]
        async with worker(self.wrapper, self.checkout, self.env, self.root, 0,
                          {"prompt": "lock test"}) as handle:
            events = handle.events()
            await asyncio.wait_for(anext(events), 3)  # session
            self.assertEqual((await asyncio.wait_for(anext(events), 3))["type"], "quota_paused")
            with locked(self.checkout):
                self.assertEqual((await asyncio.wait_for(anext(events), 3))["type"], "quota_ready")
                await handle.send({"resume": True})
                with self.assertRaises(asyncio.TimeoutError):
                    await asyncio.wait_for(anext(events), 0.25)
                handle.process.send_signal(signal.SIGINT)
                self.assertEqual(await asyncio.wait_for(handle.process.wait(), 3), 130)
        self.assertEqual(len(self.calls), 1)
