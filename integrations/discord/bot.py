"""Single-owner Discord frontend. Start with the private venv described in docs/DISCORD.md."""
import asyncio
from collections import deque
from contextlib import suppress
import io
import json
import os
from pathlib import Path
import sqlite3

from runner import Busy, Runs, worker

import discord
from discord import app_commands

OWNER_ID = 383714960498229250
WORKSPACE = Path(__file__).resolve().parents[2]
BINARY = WORKSPACE / "target/release/ag"
CONFIG = Path.home() / ".config/agent/discord.env"
STATE = Path.home() / ".agent/discord"


def read_token(path=CONFIG):
    stat = path.lstat()
    if not path.is_file() or path.is_symlink() or stat.st_uid != os.getuid() or stat.st_mode & 0o077:
        raise ValueError("Discord token file must be a private, user-owned regular file (chmod 600)")
    for line in path.read_text().splitlines():
        if line.startswith("DISCORD_BOT_TOKEN="):
            token = line.split("=", 1)[1].strip()
            if token:
                return token
    raise ValueError("DISCORD_BOT_TOKEN is missing or empty")


def worker_env():
    # Never pass the bot token to the model worker or its shell children.
    return {k: v for k, v in os.environ.items() if k != "DISCORD_BOT_TOKEN"}


def prompt_preview(prompt, limit):
    text = " ".join(prompt.split())
    return text if len(text) <= limit else text[:limit - 1].rstrip() + "…"


class Approval(discord.ui.View):
    def __init__(self):
        super().__init__(timeout=120)
        self.allowed = False
        self.decided = False

    async def interaction_check(self, interaction):
        if interaction.user.id != OWNER_ID:
            await interaction.response.send_message("Not authorized.", ephemeral=True)
            return False
        return True

    async def decide(self, interaction, allowed):
        if self.decided or self.is_finished():
            await interaction.response.send_message("Approval already settled or expired.", ephemeral=True)
            return
        self.decided = True
        self.allowed = allowed
        for child in self.children:
            child.disabled = True
        await interaction.response.edit_message(view=self)
        self.stop()

    @discord.ui.button(label="Approve", style=discord.ButtonStyle.danger)
    async def approve(self, interaction, button):
        await self.decide(interaction, True)

    @discord.ui.button(label="Deny", style=discord.ButtonStyle.secondary)
    async def deny(self, interaction, button):
        await self.decide(interaction, False)


class AgentBot(discord.Client):
    def __init__(self):
        intents = discord.Intents.default()
        intents.message_content = True
        super().__init__(intents=intents, allowed_mentions=discord.AllowedMentions.none())
        self.tree = app_commands.CommandTree(self)
        self.runs = Runs()
        self.drains = {}
        STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.db = sqlite3.connect(STATE / "state.sqlite3")
        self.db.execute("CREATE TABLE IF NOT EXISTS sessions (thread INTEGER PRIMARY KEY, session TEXT)")
        # Old threads remain in manual mode; only new owner-created threads default to auto.
        columns = {row[1] for row in self.db.execute("PRAGMA table_info(sessions)")}
        if "auto_approve" not in columns:
            self.db.execute("ALTER TABLE sessions ADD COLUMN auto_approve INTEGER NOT NULL DEFAULT 0")
        if "max_turns" not in columns:
            self.db.execute("ALTER TABLE sessions ADD COLUMN max_turns INTEGER NOT NULL DEFAULT 25")
        if "workspace" not in columns:
            self.db.execute("ALTER TABLE sessions ADD COLUMN workspace TEXT")
        self.db.execute("CREATE TABLE IF NOT EXISTS seen (id INTEGER PRIMARY KEY)")
        self.db.commit()
        self.tree.command(name="agent", description="Start an agent thread in an existing local workspace")(self.start_session)
        self.tree.command(name="agent_stop", description="Cancel this thread's active turn and clear queued replies")(self.stop_turn)
        self.tree.command(name="agent_settings", description="Set approval mode and per-prompt model-call cap for this thread")(self.settings)
        self.tree.command(name="agent_status", description="Show this session's run status and workspace")(self.status)

    async def setup_hook(self):
        await self.tree.sync()

    async def on_ready(self):
        print("Discord bot connected; /agent, /agent_stop, /agent_status, /agent_settings registered.", flush=True)

    def seen(self, event_id):
        inserted = self.db.execute("INSERT OR IGNORE INTO seen VALUES (?)", (event_id,)).rowcount
        self.db.commit()
        return not inserted

    @app_commands.describe(auto_approve="Allow shell and file changes without buttons (full local privileges)",
                           max_turns="Maximum model calls per prompt, not a tool-call or cost cap (1–100)")
    async def start_session(self, interaction: discord.Interaction, prompt: str,
                            auto_approve: bool = True, max_turns: app_commands.Range[int, 1, 100] = 50,
                            workspace: str = ""):
        if interaction.user.id != OWNER_ID or interaction.guild is None:
            await interaction.response.send_message("Not authorized; use your server.", ephemeral=True)
            return
        if self.seen(interaction.id):
            await interaction.response.send_message("Already received.", ephemeral=True)
            return
        if not isinstance(interaction.channel, discord.TextChannel):
            await interaction.response.send_message("Use a text channel.", ephemeral=True)
            return
        try:
            root = Path(workspace).expanduser() if workspace else WORKSPACE
            if not root.is_absolute() or not root.is_dir():
                raise ValueError("Workspace must be an existing absolute directory.")
            root = root.resolve(strict=True)
        except (OSError, ValueError) as error:
            await interaction.response.send_message(str(error), ephemeral=True)
            return
        mode = ("Tools are approved automatically with full local privileges" if auto_approve
                else "I'll ask before running shell commands or changing files")
        preview = discord.utils.escape_markdown(prompt_preview(prompt, 240), ignore_links=False)
        await interaction.response.send_message(
            f"**Request:** {preview}\n\n"
            f"Reply in this thread to keep going. {mode}. Up to {max_turns} model calls per message. Not a sandbox.",
            allowed_mentions=discord.AllowedMentions.none(), suppress_embeds=True,
        )
        message = await interaction.original_response()
        title = prompt_preview(prompt, 60)
        thread = await message.create_thread(name=f"Agent: {title}" if title else "Agent session", auto_archive_duration=1440)
        self.db.execute("INSERT INTO sessions (thread, session, auto_approve, max_turns, workspace) VALUES (?, NULL, ?, ?, ?)",
                        (thread.id, int(auto_approve), max_turns, str(root)))
        self.db.commit()
        await self.enqueue(thread, prompt, message)

    @app_commands.describe(auto_approve="Allow shell and file changes without buttons (full local privileges)",
                           max_turns="Maximum model calls per prompt (1–100); resets on each reply")
    async def settings(self, interaction: discord.Interaction, auto_approve: bool,
                       max_turns: app_commands.Range[int, 1, 100]):
        if interaction.user.id != OWNER_ID or interaction.guild is None:
            await interaction.response.send_message("Not authorized.", ephemeral=True)
            return
        thread_id = interaction.channel_id
        if not self.db.execute("SELECT 1 FROM sessions WHERE thread=?", (thread_id,)).fetchone():
            await interaction.response.send_message("Use this command inside an agent thread.", ephemeral=True)
            return
        if self.runs.status(thread_id) != "idle":
            await interaction.response.send_message("Wait for this turn to finish or cancel it before changing settings.", ephemeral=True)
            return
        self.db.execute("UPDATE sessions SET auto_approve=?, max_turns=? WHERE thread=?",
                        (int(auto_approve), max_turns, thread_id))
        self.db.commit()
        mode = "Automatic approvals (full local privileges)" if auto_approve else "Approval buttons"
        await interaction.response.send_message(f"{mode} · {max_turns} model calls per prompt. The cap resets on each reply; not a sandbox.")

    async def on_message(self, message):
        if message.author.bot or message.author.id != OWNER_ID or message.guild is None:
            return
        if not isinstance(message.channel, discord.Thread) or not message.content.strip():
            return
        if not self.db.execute("SELECT 1 FROM sessions WHERE thread=?", (message.channel.id,)).fetchone():
            return
        if self.seen(message.id):
            return
        await self.enqueue(message.channel, message.content, message)

    async def enqueue(self, thread, prompt, message):
        if thread.id in self.drains:
            task, pending = self.drains[thread.id]
            if task.cancelling():
                await thread.send("This thread is still stopping. Resend when ready.")
            else:
                pending.append((prompt, message))
            return
        # Keep the FIFO at Discord ingress; Runs still rejects session and checkout contention.
        pending = deque()
        self.drains[thread.id] = (asyncio.current_task(), pending)
        try:
            while await self.submit(thread, prompt, message) and pending:
                prompt, message = pending.popleft()
        finally:
            self.drains.pop(thread.id)
        if pending:
            await thread.send(f"Cleared {len(pending)} queued replies because work stopped. Resend them when ready.")

    async def submit(self, thread, prompt, message=None):
        row = self.db.execute("SELECT workspace FROM sessions WHERE thread=?", (thread.id,)).fetchone()
        try:
            return await self.runs.submit(thread.id, row[0] or WORKSPACE, lambda: self.run_turn(thread, prompt, message))
        except (Busy, OSError) as error:
            await thread.send(f"Not started: {error} Resend when ready.")
            return False

    async def run_turn(self, thread, prompt, message):
        try:
            if message is not None:
                with suppress(discord.HTTPException, OSError):
                    await asyncio.wait_for(message.add_reaction("⏳"), timeout=3)
            # The slash-command opener lives in the parent channel, not in this thread.
            reference = (message.to_reference(fail_if_not_exists=False)
                         if message is not None and message.channel.id == thread.id else None)
            return await self.execute(thread, prompt, reference=reference)
        finally:
            if message is not None and self.user is not None:
                with suppress(discord.HTTPException, OSError):
                    await asyncio.wait_for(message.remove_reaction("⏳", self.user), timeout=3)

    async def status(self, interaction: discord.Interaction):
        if interaction.user.id != OWNER_ID or interaction.guild is None:
            await interaction.response.send_message("Not authorized.", ephemeral=True)
            return
        row = self.db.execute("SELECT workspace FROM sessions WHERE thread=?", (interaction.channel_id,)).fetchone()
        drain = self.drains.get(interaction.channel_id)
        queued = len(drain[1]) if drain else 0
        text = (f"{self.runs.status(interaction.channel_id)} · {queued} queued · {row[0] or WORKSPACE}" if row
                else "Use this command inside an agent thread.")
        await interaction.response.send_message(text, ephemeral=True)

    async def stop_turn(self, interaction: discord.Interaction):
        if interaction.user.id != OWNER_ID or interaction.guild is None:
            await interaction.response.send_message("Not authorized.", ephemeral=True)
            return
        cancelled, cleared = self.cancel_thread(interaction.channel_id)
        if cancelled:
            await interaction.response.send_message(
                f"Cancelling this thread. Cleared {cleared} queued replies. Completed changes are not rolled back.",
                ephemeral=True)
        else:
            await interaction.response.send_message("No active turn.", ephemeral=True)

    def cancel_thread(self, thread_id):
        drain = self.drains.get(thread_id)
        if drain is None:
            return self.runs.cancel(thread_id), 0
        task, pending = drain
        cleared = len(pending)
        pending.clear()
        # Don't interrupt subprocess or reaction cleanup with a second cancellation.
        if not task.cancelling():
            task.cancel()
        return True, cleared

    async def on_disconnect(self):
        # No unobserved work or pending approval survives loss of Discord connectivity.
        for thread_id in self.drains.keys() | self.runs.active.keys():
            self.cancel_thread(thread_id)

    async def execute(self, thread, prompt, *, reference=None):
        reply = {"reference": reference, "mention_author": False} if reference is not None else {}
        view = None
        try:
            saved, auto_approve, max_turns, workspace = self.db.execute(
                "SELECT session, auto_approve, max_turns, workspace FROM sessions WHERE thread=?", (thread.id,)).fetchone()
            if auto_approve not in (0, 1) or not isinstance(max_turns, int) or not 1 <= max_turns <= 100:
                raise ValueError("Invalid saved permission policy")
            payload = {"prompt": prompt, "session": saved, "auto_approve": bool(auto_approve), "max_turns": max_turns,
                       "presentation": "\nDiscord presentation: reply directly and conversationally. For project work, mention relevant changes, checks, and blockers without a fixed template or tool-by-tool narration. Match the level of detail to the request. Never omit important failures or risks."}
            async with worker(BINARY, workspace or WORKSPACE, worker_env(), STATE, thread.id, payload) as handle:
                done = False
                limited = False
                answered = False
                async for event in handle.events():
                    self.runs.event(thread.id, event)
                    kind = event.get("type")
                    if kind == "session":
                        self.db.execute("UPDATE sessions SET session=? WHERE thread=?", (event["id"], thread.id))
                        self.db.commit()
                    elif kind == "answer":
                        answer = event.get("text", "")
                        if answer.strip():
                            if len(answer) > 1800:
                                await thread.send("The full reply is attached.", file=discord.File(io.BytesIO(answer.encode()), filename="answer.txt"), **reply)
                            else:
                                await thread.send(answer, **reply)
                            answered = True
                    elif kind == "approval":
                        view = Approval()
                        arguments = json.dumps({"tool": event["name"], "arguments": event["args"]}, indent=2, ensure_ascii=False)
                        message = await thread.send(
                            "Allow this exact action with this machine's user privileges? Review the attached complete arguments. Expires in 2 minutes.",
                            file=discord.File(io.BytesIO(arguments.encode()), filename="approval.json"), view=view,
                        )
                        await view.wait()
                        for child in view.children:
                            child.disabled = True
                        await message.edit(content="Approved." if view.allowed else "Denied or expired.", view=view)
                        await handle.send({"approve": view.allowed})
                        self.runs.event(thread.id, {"type": "running"})
                        view = None
                    elif kind in ("quota_paused", "quota_resumed", "quota_login"):
                        await thread.send(event["message"])
                    elif kind == "quota_ready":
                        await self.runs.reserve_after_quota(thread.id)
                        await handle.send({"resume": True})
                    elif kind == "limit":
                        limited = True
                    elif kind == "done":
                        done = True
                code = await handle.process.wait()
                if not done or code != 0:
                    await thread.send(
                        f"Turn stopped with an error (worker exit {code}). Saved context is retained; inspect local state before retrying. "
                        f"Private local diagnostic: ~/.agent/discord/{Path(handle.diagnostic.name).name} (not uploaded)."
                    )
                    return False
                if limited:
                    await thread.send(f"Stopped at the {max_turns}-model-call cap. Completed changes remain; reply to continue with a fresh budget.", **reply)
                elif not answered:
                    await thread.send("I finished, but didn't get a reply to share.", **reply)
            return True
        except asyncio.CancelledError:
            try:
                await thread.send("Cancelled. Effects may remain; inspect state before retrying.")
            except discord.HTTPException:
                pass
            raise
        except (OSError, ValueError, discord.HTTPException):
            try:
                await thread.send("Discord/worker error. Work stopped; saved context is retained.")
            except discord.HTTPException:
                pass
            return False
        finally:
            if view is not None:
                view.stop()


if __name__ == "__main__":
    os.umask(0o077)
    token = read_token()
    try:
        AgentBot().run(token, log_handler=None)
    except discord.PrivilegedIntentsRequired:
        raise SystemExit("Enable Message Content Intent in Developer Portal > Bot > Privileged Gateway Intents for this token's application, save, then restart.") from None
    except discord.LoginFailure:
        raise SystemExit("Discord rejected the bot token. Update the private discord.env file, then restart.") from None
