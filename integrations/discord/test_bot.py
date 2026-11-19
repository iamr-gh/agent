"""Offline adapter tests: no Discord connection, credentials, or model calls."""
import asyncio
import json
import os
from pathlib import Path
import signal
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

import bot


class ConfigTests(unittest.TestCase):
    def test_private_token_file(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "discord.env"
            path.write_text("# comment\nDISCORD_BOT_TOKEN=synthetic-test-token\n")
            path.chmod(0o600)
            self.assertEqual(bot.read_token(path), "synthetic-test-token")
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                bot.read_token(path)
            path.chmod(0o600)
            link = Path(root) / "link"
            link.symlink_to(path)
            with self.assertRaises(ValueError):
                bot.read_token(link)
            path.write_text("DISCORD_BOT_TOKEN=\n")
            with self.assertRaises(ValueError):
                bot.read_token(path)

    def test_worker_does_not_inherit_token(self):
        with patch.dict(os.environ, {"DISCORD_BOT_TOKEN": "synthetic"}):
            self.assertNotIn("DISCORD_BOT_TOKEN", bot.worker_env())


class AdapterTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.state = patch.object(bot, "STATE", Path(self.temp.name))
        self.state.start()
        self.client = bot.AgentBot()
        self.thread = SimpleNamespace(id=1234, send=AsyncMock())
        self.client.db.execute("INSERT INTO sessions VALUES (?, NULL)", (self.thread.id,))
        self.client.db.commit()

    async def asyncTearDown(self):
        self.client.db.close()
        await self.client.close()
        self.state.stop()
        self.temp.cleanup()

    def interaction(self, owner=bot.OWNER_ID):
        return SimpleNamespace(user=SimpleNamespace(id=owner), guild=object(),
                               response=SimpleNamespace(send_message=AsyncMock(), edit_message=AsyncMock()))

    async def test_only_owner_can_start_stop_or_approve(self):
        interaction = self.interaction(42)
        await self.client.start_session(interaction, "hello")
        await self.client.stop_turn(interaction)
        view = bot.Approval()
        self.assertFalse(await view.interaction_check(interaction))
        self.assertEqual(interaction.response.send_message.await_count, 3)
        self.assertFalse(view.allowed)
        view.stop()

    async def test_approval_is_single_use(self):
        view = bot.Approval()
        interaction = self.interaction()
        await view.decide(interaction, True)
        await view.decide(interaction, False)
        self.assertTrue(view.allowed)
        interaction.response.edit_message.assert_awaited_once()
        interaction.response.send_message.assert_awaited_once()

    async def test_expired_approval_cannot_be_approved(self):
        view = bot.Approval()
        view.stop()
        await view.decide(self.interaction(), True)
        self.assertFalse(view.allowed)

    async def test_deduplication_survives_restart(self):
        self.assertFalse(self.client.seen(5678))
        self.assertTrue(self.client.seen(5678))
        other = bot.AgentBot()
        self.assertTrue(other.seen(5678))
        other.db.close()
        await other.close()

    async def test_discord_client_start_is_not_shadowed_by_command(self):
        with patch.object(self.client, "login", AsyncMock()) as login, patch.object(self.client, "connect", AsyncMock()) as connect:
            await self.client.start("synthetic-token", reconnect=True)
        login.assert_awaited_once_with("synthetic-token")
        connect.assert_awaited_once_with(reconnect=True)

    async def test_disconnect_cancels_active_task(self):
        task = asyncio.create_task(asyncio.sleep(100))
        self.client.active = task
        await self.client.on_disconnect()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.client.active = None

    async def test_worker_protocol_and_session_recovery(self):
        stream = asyncio.StreamReader()
        for event in [{"type": "session", "id": "test-session"},
                      {"type": "answer", "text": "hello"}, {"type": "done"}]:
            stream.feed_data((json.dumps(event) + "\n").encode())
        stream.feed_eof()
        process = SimpleNamespace(stdout=stream, returncode=0, wait=AsyncMock(return_value=0),
                                  stdin=SimpleNamespace(write=lambda _: None, drain=AsyncMock()))
        with patch.object(asyncio, "create_subprocess_exec", AsyncMock(return_value=process)) as spawn:
            await self.client.execute(self.thread, "task")
            self.assertNotIn("DISCORD_BOT_TOKEN", spawn.call_args.kwargs["env"])
        row = self.client.db.execute("SELECT session FROM sessions WHERE thread=1234").fetchone()
        self.assertEqual(row[0], "test-session")
        self.assertIn("Turn complete", self.thread.send.call_args.args[0])

    async def test_worker_approval_returns_only_explicit_decision(self):
        for allowed in (False, True):
            with self.subTest(allowed=allowed):
                stream = asyncio.StreamReader()
                args = {"command": "printf 'test'"}
                for event in [{"type": "approval", "name": "shell", "args": args}, {"type": "done"}]:
                    stream.feed_data((json.dumps(event) + "\n").encode())
                stream.feed_eof()
                writes = []
                process = SimpleNamespace(stdout=stream, returncode=0, wait=AsyncMock(return_value=0),
                                          stdin=SimpleNamespace(write=writes.append, drain=AsyncMock()))
                view = SimpleNamespace(allowed=allowed, children=[], wait=AsyncMock())
                with patch.object(asyncio, "create_subprocess_exec", AsyncMock(return_value=process)), patch.object(bot, "Approval", return_value=view):
                    await self.client.execute(self.thread, "task")
                self.assertEqual(json.loads(writes[-1]), {"approve": allowed})
                approval_call = next(call for call in reversed(self.thread.send.call_args_list) if "file" in call.kwargs)
                self.assertEqual(json.loads(approval_call.kwargs["file"].fp.getvalue()), {"tool": "shell", "arguments": args})

    async def test_worker_failure_does_not_report_success(self):
        stream = asyncio.StreamReader()
        stream.feed_eof()
        process = SimpleNamespace(stdout=stream, returncode=1, wait=AsyncMock(return_value=1),
                                  stdin=SimpleNamespace(write=lambda _: None, drain=AsyncMock()))
        with patch.object(asyncio, "create_subprocess_exec", AsyncMock(return_value=process)):
            await self.client.execute(self.thread, "task")
        self.assertIn("error", self.thread.send.call_args.args[0])

    async def test_cancellation_interrupts_worker_and_denies_approval(self):
        stream = asyncio.StreamReader()
        writes = []
        process = SimpleNamespace(stdout=stream, returncode=None, wait=AsyncMock(return_value=130),
                                  send_signal=lambda sig: writes.append(sig),
                                  stdin=SimpleNamespace(write=writes.append, drain=AsyncMock()))
        with patch.object(asyncio, "create_subprocess_exec", AsyncMock(return_value=process)):
            task = asyncio.create_task(self.client.execute(self.thread, "task"))
            await asyncio.sleep(0.01)
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
        self.assertIn(signal.SIGINT, writes)
        self.assertIn(b'{"approve":false}\n', writes)


if __name__ == "__main__":
    unittest.main()
