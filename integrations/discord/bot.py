"""Single-owner Discord frontend. Start with the private venv described in docs/DISCORD.md."""
import asyncio
import io
import json
import os
from pathlib import Path
import signal
import sqlite3

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
        self.active = None
        self.active_thread = None
        STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.db = sqlite3.connect(STATE / "state.sqlite3")
        self.db.execute("CREATE TABLE IF NOT EXISTS sessions (thread INTEGER PRIMARY KEY, session TEXT)")
        # Old threads remain in manual mode; only new owner-created threads default to auto.
        columns = {row[1] for row in self.db.execute("PRAGMA table_info(sessions)")}
        if "auto_approve" not in columns:
            self.db.execute("ALTER TABLE sessions ADD COLUMN auto_approve INTEGER NOT NULL DEFAULT 0")
        if "max_turns" not in columns:
            self.db.execute("ALTER TABLE sessions ADD COLUMN max_turns INTEGER NOT NULL DEFAULT 25")
        self.db.execute("CREATE TABLE IF NOT EXISTS seen (id INTEGER PRIMARY KEY)")
        self.db.commit()
        self.tree.command(name="agent", description="Start an agent thread in the local agent repository")(self.start_session)
        self.tree.command(name="agent_stop", description="Cancel the active agent turn")(self.stop_turn)
        self.tree.command(name="agent_settings", description="Set approval mode and per-prompt model-call cap for this thread")(self.settings)

    async def setup_hook(self):
        await self.tree.sync()

    async def on_ready(self):
        print("Discord bot connected; /agent and /agent_stop registered.", flush=True)

    def seen(self, event_id):
        inserted = self.db.execute("INSERT OR IGNORE INTO seen VALUES (?)", (event_id,)).rowcount
        self.db.commit()
        return not inserted

    @app_commands.describe(auto_approve="Allow shell and file changes without buttons (full local privileges)",
                           max_turns="Maximum model calls per prompt, not a tool-call or cost cap (1–100)")
    async def start_session(self, interaction: discord.Interaction, prompt: str,
                            auto_approve: bool = True, max_turns: app_commands.Range[int, 1, 100] = 25):
        if interaction.user.id != OWNER_ID or interaction.guild is None:
            await interaction.response.send_message("Not authorized; use your server.", ephemeral=True)
            return
        if self.seen(interaction.id):
            await interaction.response.send_message("Already received.", ephemeral=True)
            return
        if self.active is not None or not isinstance(interaction.channel, discord.TextChannel):
            await interaction.response.send_message("Agent busy, or this is not a text channel.", ephemeral=True)
            return
        # Reserve synchronously before any await so simultaneous commands cannot race.
        self.active = asyncio.current_task()
        try:
            await interaction.response.send_message("Starting a local agent session…")
            message = await interaction.original_response()
            thread = await message.create_thread(name="Agent session", auto_archive_duration=1440)
            self.db.execute("INSERT INTO sessions (thread, session, auto_approve, max_turns) VALUES (?, NULL, ?, ?)",
                            (thread.id, int(auto_approve), max_turns))
            self.db.commit()
            self.active_thread = thread.id
            await self.execute(thread, prompt)
        finally:
            self.active = None
            self.active_thread = None

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
        if self.active_thread == thread_id:
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
        if self.active is not None:
            await message.channel.send("Agent busy. Wait for completion or use /agent_stop, then resend.")
            return
        self.active = asyncio.current_task()
        self.active_thread = message.channel.id
        try:
            await self.execute(message.channel, message.content)
        finally:
            self.active = None
            self.active_thread = None

    async def stop_turn(self, interaction: discord.Interaction):
        if interaction.user.id != OWNER_ID or interaction.guild is None:
            await interaction.response.send_message("Not authorized.", ephemeral=True)
            return
        if self.active is not None:
            self.active.cancel()
            await interaction.response.send_message("Cancelling. Completed changes are not rolled back.", ephemeral=True)
        else:
            await interaction.response.send_message("No active turn.", ephemeral=True)

    async def on_disconnect(self):
        # No unobserved work or pending approval survives loss of Discord connectivity.
        if self.active is not None:
            self.active.cancel()

    async def execute(self, thread, prompt):
        process = None
        view = None
        try:
            saved, auto_approve, max_turns = self.db.execute(
                "SELECT session, auto_approve, max_turns FROM sessions WHERE thread=?", (thread.id,)).fetchone()
            if auto_approve not in (0, 1) or not isinstance(max_turns, int) or not 1 <= max_turns <= 100:
                raise ValueError("Invalid saved permission policy")
            process = await asyncio.create_subprocess_exec(
                str(BINARY), "--discord-worker", cwd=WORKSPACE, env=worker_env(),
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                # Provider errors may include sensitive details: never upload them.
                stderr=asyncio.subprocess.DEVNULL, limit=2 * 1024 * 1024,
            )
            process.stdin.write((json.dumps({"prompt": prompt, "session": saved,
                                             "auto_approve": bool(auto_approve), "max_turns": max_turns}) + "\n").encode())
            await process.stdin.drain()
            mode = "auto approvals — full local privileges" if auto_approve else "approval buttons"
            await thread.send(f"Working · {mode} · max {max_turns} model calls. Not a sandbox.")
            done = False
            limited = False
            while line := await process.stdout.readline():
                event = json.loads(line)
                kind = event.get("type")
                if kind == "session":
                    self.db.execute("UPDATE sessions SET session=? WHERE thread=?", (event["id"], thread.id))
                    self.db.commit()
                elif kind == "answer":
                    answer = event.get("text", "")
                    if answer:
                        if len(answer) > 1800:
                            await thread.send("Agent response:", file=discord.File(io.BytesIO(answer.encode()), filename="answer.txt"))
                        else:
                            await thread.send(answer)
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
                    process.stdin.write((json.dumps({"approve": view.allowed}) + "\n").encode())
                    await process.stdin.drain()
                    view = None
                elif kind == "limit":
                    limited = True
                elif kind == "done":
                    done = True
            code = await process.wait()
            if limited and done and code == 0:
                await thread.send(f"Stopped at the {max_turns}-model-call cap. Completed changes remain; reply to continue with a fresh budget.")
                return
            await thread.send("Turn complete. Reply here to continue." if done and code == 0 else
                              "Turn stopped with an error. Saved context is retained; inspect local state before retrying. Check local provider credentials if no response appeared.")
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
        finally:
            if view is not None:
                view.stop()
            if process is not None and process.returncode is None:
                process.send_signal(signal.SIGINT)
                # Unblock a worker waiting for approval, always denying it.
                try:
                    process.stdin.write(b'{"approve":false}\n')
                    await process.stdin.drain()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                try:
                    await asyncio.wait_for(process.wait(), timeout=5)
                except asyncio.TimeoutError:
                    process.kill()
                    await process.wait()


if __name__ == "__main__":
    os.umask(0o077)
    token = read_token()
    try:
        AgentBot().run(token, log_handler=None)
    except discord.PrivilegedIntentsRequired:
        raise SystemExit("Enable Message Content Intent in Developer Portal > Bot > Privileged Gateway Intents for this token's application, save, then restart.") from None
    except discord.LoginFailure:
        raise SystemExit("Discord rejected the bot token. Update the private discord.env file, then restart.") from None
