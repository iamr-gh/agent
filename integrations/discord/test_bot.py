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
        self.client.db.execute("INSERT INTO sessions (thread, session) VALUES (?, NULL)", (self.thread.id,))
        self.client.db.commit()

    async def asyncTearDown(self):
        self.client.db.close()
        await self.client.close()
        self.state.stop()
        self.temp.cleanup()

    def interaction(self, owner=bot.OWNER_ID):
        return SimpleNamespace(user=SimpleNamespace(id=owner), guild=object(), channel_id=self.thread.id,
                               response=SimpleNamespace(send_message=AsyncMock(), edit_message=AsyncMock()))

    async def test_only_owner_can_start_stop_or_approve(self):
        interaction = self.interaction(42)
        await self.client.start_session(interaction, "hello")
        await self.client.stop_turn(interaction)
        await self.client.settings(interaction, True, 10)
        view = bot.Approval()
        self.assertFalse(await view.interaction_check(interaction))
        self.assertEqual(interaction.response.send_message.await_count, 4)
        self.assertFalse(view.allowed)
        view.stop()

    async def test_existing_threads_default_to_manual_and_new_command_defaults_to_auto(self):
        row = self.client.db.execute("SELECT auto_approve, max_turns FROM sessions WHERE thread=?", (self.thread.id,)).fetchone()
        self.assertEqual(row, (0, 25))
        parameters = {param.name: param for param in self.client.tree.get_command("agent").parameters}
        self.assertIs(parameters["auto_approve"].default, True)
        self.assertEqual(parameters["max_turns"].default, 25)
        self.assertEqual(parameters["max_turns"].min_value, 1)
        self.assertEqual(parameters["max_turns"].max_value, 100)

    async def test_start_announces_policy_once_in_thread_parent(self):
        interaction = self.interaction()
        interaction.id = 5678
        self.thread.id = 4321  # The new thread has no saved session yet.
        parent = SimpleNamespace(create_thread=AsyncMock(return_value=self.thread))
        interaction.original_response = AsyncMock(return_value=parent)
        interaction.channel = SimpleNamespace()
        with patch.object(bot.discord, "TextChannel", SimpleNamespace), patch.object(self.client, "submit", AsyncMock()) as submit:
            await self.client.start_session(interaction, "hello", max_turns=7)
        parent.create_thread.assert_awaited_once()
        interaction.response.send_message.assert_awaited_once()
        notice = interaction.response.send_message.await_args.args[0]
        self.assertIn("full local privileges", notice)
        self.assertIn("7 model calls", notice)
        submit.assert_awaited_once_with(self.thread, "hello")
        self.thread.send.assert_not_awaited()

    async def test_owner_can_change_persistent_thread_policy(self):
        await self.client.settings(self.interaction(), True, 7)
        row = self.client.db.execute("SELECT auto_approve, max_turns FROM sessions WHERE thread=?", (self.thread.id,)).fetchone()
        self.assertEqual(row, (1, 7))
        other = bot.AgentBot()
        self.assertEqual(other.db.execute("SELECT auto_approve, max_turns FROM sessions WHERE thread=?", (self.thread.id,)).fetchone(), (1, 7))
        other.db.close()
        await other.close()
        self.client.runs.active[self.thread.id] = {"task": asyncio.current_task(), "workspace": bot.WORKSPACE, "state": "running"}
        await self.client.settings(self.interaction(), False, 3)
        self.assertEqual(self.client.db.execute("SELECT auto_approve, max_turns FROM sessions WHERE thread=?", (self.thread.id,)).fetchone(), (1, 7))

    async def test_legacy_database_migration_preserves_session_and_manual_mode(self):
        import sqlite3
        with tempfile.TemporaryDirectory() as root:
            db = sqlite3.connect(Path(root) / "state.sqlite3")
            db.execute("CREATE TABLE sessions (thread INTEGER PRIMARY KEY, session TEXT)")
            db.execute("INSERT INTO sessions VALUES (1, 'saved-context')")
            db.commit()
            db.close()
            with patch.object(bot, "STATE", Path(root)):
                migrated = bot.AgentBot()
            self.assertEqual(migrated.db.execute("SELECT session, auto_approve, max_turns FROM sessions WHERE thread=1").fetchone(), ("saved-context", 0, 25))
            migrated.db.close()
            await migrated.close()

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
        self.client.runs.active[self.thread.id] = {"task": task, "workspace": bot.WORKSPACE, "state": "running"}
        await self.client.on_disconnect()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.client.runs.active.clear()

    async def test_stop_is_scoped_to_current_thread_and_status_is_owner_only(self):
        task = asyncio.create_task(asyncio.sleep(100))
        self.client.runs.active[999] = {"task": task, "workspace": bot.WORKSPACE, "state": "running"}
        interaction = self.interaction()
        await self.client.stop_turn(interaction)
        self.assertFalse(task.cancelled())
        self.assertIn("No active", interaction.response.send_message.call_args.args[0])
        interaction = self.interaction(42)
        await self.client.status(interaction)
        self.assertEqual(interaction.response.send_message.call_args.args[0], "Not authorized.")
        interaction = self.interaction()
        await self.client.status(interaction)
        self.assertIn("idle", interaction.response.send_message.call_args.args[0])
        self.client.runs.cancel_all()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.client.runs.active.clear()

    async def test_saved_workspace_and_generic_entry_point_reach_worker(self):
        self.client.db.execute("UPDATE sessions SET workspace=?", (self.temp.name,))
        stream = asyncio.StreamReader()
        stream.feed_eof()
        process = SimpleNamespace(stdout=stream, returncode=0, wait=AsyncMock(return_value=0),
                                  stdin=SimpleNamespace(write=lambda _: None, drain=AsyncMock()))
        with patch.object(asyncio, "create_subprocess_exec", AsyncMock(return_value=process)) as spawn:
            await self.client.execute(self.thread, "task")
        self.assertEqual(spawn.call_args.args[1], "--worker")
        self.assertEqual(spawn.call_args.kwargs["cwd"], self.temp.name)

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
        self.thread.send.assert_awaited_once_with("hello")

    async def test_long_and_empty_replies_get_one_message(self):
        for answer in ("é" * 1801, " \n"):
            with self.subTest(length=len(answer)):
                self.thread.send.reset_mock()
                stream = asyncio.StreamReader()
                for event in [{"type": "answer", "text": answer}, {"type": "done"}]:
                    stream.feed_data((json.dumps(event) + "\n").encode())
                stream.feed_eof()
                process = SimpleNamespace(stdout=stream, returncode=0, wait=AsyncMock(return_value=0),
                                          stdin=SimpleNamespace(write=lambda _: None, drain=AsyncMock()))
                with patch.object(asyncio, "create_subprocess_exec", AsyncMock(return_value=process)):
                    await self.client.execute(self.thread, "task")
                if answer.strip():
                    self.assertEqual(self.thread.send.await_count, 1)
                    attachment = self.thread.send.await_args.kwargs["file"]
                    self.assertEqual(attachment.filename, "answer.txt")
                    self.assertEqual(attachment.fp.getvalue().decode(), answer)
                else:
                    self.thread.send.assert_awaited_once_with("I finished, but didn't get a reply to share.")

    async def test_auto_policy_reaches_worker_and_limit_is_not_reported_as_failure(self):
        await self.client.settings(self.interaction(), True, 2)
        stream = asyncio.StreamReader()
        for event in [{"type": "limit", "max_turns": 2}, {"type": "done"}]:
            stream.feed_data((json.dumps(event) + "\n").encode())
        stream.feed_eof()
        writes = []
        process = SimpleNamespace(stdout=stream, returncode=0, wait=AsyncMock(return_value=0),
                                  stdin=SimpleNamespace(write=writes.append, drain=AsyncMock()))
        with patch.object(asyncio, "create_subprocess_exec", AsyncMock(return_value=process)):
            await self.client.execute(self.thread, "task")
        payload = json.loads(writes[0])
        self.assertIn("Discord presentation", payload.pop("presentation"))
        self.assertEqual(payload, {"prompt": "task", "session": None, "auto_approve": True, "max_turns": 2})
        self.assertIn("2-model-call cap", self.thread.send.call_args.args[0])

    async def test_corrupt_saved_policy_never_launches_worker(self):
        self.client.db.execute("UPDATE sessions SET max_turns=0")
        self.client.db.commit()
        with patch.object(asyncio, "create_subprocess_exec", AsyncMock()) as spawn:
            await self.client.execute(self.thread, "task")
        spawn.assert_not_awaited()

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
        async def spawn(*args, **kwargs):
            diagnostic = kwargs["stderr"]
            self.assertEqual(os.stat(diagnostic.name).st_mode & 0o777, 0o600)
            diagnostic.write(b"synthetic private provider failure")
            diagnostic.flush()
            return process

        with patch.object(asyncio, "create_subprocess_exec", AsyncMock(side_effect=spawn)) as launch:
            await self.client.execute(self.thread, "task")
        diagnostic = launch.call_args.kwargs["stderr"]
        self.assertTrue(diagnostic.closed)
        self.assertEqual(Path(diagnostic.name).read_text(), "synthetic private provider failure")
        message = self.thread.send.call_args.args[0]
        self.assertIn("worker exit 1", message)
        self.assertIn(Path(diagnostic.name).name, message)
        self.assertNotIn("synthetic private provider failure", str(self.thread.send.call_args_list))

    async def test_worker_launch_failure_closes_private_diagnostic(self):
        with patch.object(asyncio, "create_subprocess_exec", AsyncMock(side_effect=OSError("synthetic"))) as launch:
            await self.client.execute(self.thread, "task")
        self.assertTrue(launch.call_args.kwargs["stderr"].closed)
        self.assertIn("Discord/worker error", self.thread.send.call_args.args[0])

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
