"""Frontend-independent subprocess run control; no Discord dependencies."""
import asyncio
from contextlib import asynccontextmanager
import json
from pathlib import Path
import signal
import tempfile


class Busy(ValueError):
    pass


class Runs:
    """One writer per session and checkout, reserved before the first await."""
    def __init__(self):
        self.active = {}

    async def submit(self, key, workspace, operation):
        workspace = Path(workspace).resolve(strict=True)
        if key in self.active:
            raise Busy("This session already has an active turn.")
        if any(run["workspace"] == workspace for run in self.active.values()):
            raise Busy("Workspace busy. Wait or select a separate directory/Git worktree.")
        self.active[key] = {"task": asyncio.current_task(), "workspace": workspace, "state": "running"}
        try:
            return await operation()
        finally:
            self.active.pop(key, None)

    def event(self, key, event):
        if key in self.active:
            self.active[key]["state"] = "awaiting approval" if event["type"] == "approval" else "running"

    def status(self, key):
        run = self.active.get(key)
        return run["state"] if run else "idle"

    def cancel(self, key):
        run = self.active.get(key)
        if run:
            run["task"].cancel()
        return run is not None

    def cancel_all(self):
        for key in list(self.active):
            self.cancel(key)


class Worker:
    def __init__(self, process, diagnostic):
        self.process = process
        self.diagnostic = diagnostic

    async def send(self, value):
        self.process.stdin.write((json.dumps(value) + "\n").encode())
        await self.process.stdin.drain()

    async def events(self):
        while line := await self.process.stdout.readline():
            yield json.loads(line)


@asynccontextmanager
async def worker(binary, workspace, env, state, key, payload):
    """Own process/diagnostic lifetime, including failed launch and cancellation."""
    process = None
    diagnostic = tempfile.NamedTemporaryFile(prefix=f"worker-{key}-", suffix=".log", dir=state, delete=False)
    try:
        process = await asyncio.create_subprocess_exec(
            str(binary), "--worker", cwd=workspace, env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=diagnostic, limit=2 * 1024 * 1024,
        )
        handle = Worker(process, diagnostic)
        await handle.send(payload)
        yield handle
    finally:
        try:
            if process is not None and process.returncode is None:
                try:
                    process.send_signal(signal.SIGINT)
                except ProcessLookupError:
                    pass
                try:
                    process.stdin.write(b'{"approve":false}\n')
                    await process.stdin.drain()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                try:
                    await asyncio.wait_for(process.wait(), timeout=5)
                except asyncio.TimeoutError:
                    try:
                        process.kill()
                    except ProcessLookupError:
                        pass
                    await process.wait()
        finally:
            diagnostic.close()
