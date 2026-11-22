"""Frontend-independent subprocess run control; no Discord dependencies."""
import asyncio
from collections import deque
from contextlib import asynccontextmanager
import json
from pathlib import Path
import signal
import tempfile


class Busy(ValueError):
    pass


class Runs:
    """One turn per session, with FIFO reservations per checkout."""
    def __init__(self):
        self.active = {}
        self.waiting = {}
        self.changed = asyncio.Event()

    def checkout_owner(self, workspace):
        return next((key for key, run in self.active.items()
                     if run["workspace"] == workspace and run["owns_checkout"]), None)

    def checkout_changed(self):
        self.changed.set()
        self.changed = asyncio.Event()

    def queue_position(self, key):
        run = self.active.get(key)
        queue = self.waiting.get(run["workspace"], ()) if run else ()
        return queue.index(key) + 1 if key in queue else None

    async def reserve(self, key, waiting_state, reserved_state, on_wait):
        run = self.active[key]
        workspace = run["workspace"]
        queue = self.waiting.setdefault(workspace, deque())
        queue.append(key)

        def blocked():
            return queue[0] != key or self.checkout_owner(workspace) is not None

        try:
            if blocked():
                run["state"] = waiting_state
                if on_wait is not None:
                    await on_wait(self.queue_position(key))
                while blocked():
                    await self.changed.wait()
            run["owns_checkout"] = True
            run["state"] = reserved_state
        finally:
            queue.remove(key)
            if not queue:
                del self.waiting[workspace]
            # A cancelled head waiter must not strand the rest of the queue.
            self.checkout_changed()

    async def submit(self, key, workspace, operation, on_wait=None):
        workspace = Path(workspace).resolve(strict=True)
        if key in self.active:
            raise Busy("This session already has an active turn.")
        self.active[key] = {"task": asyncio.current_task(), "workspace": workspace,
                            "state": "waiting for checkout", "owns_checkout": False}
        try:
            await self.reserve(key, "waiting for checkout", "running", on_wait)
            return await operation()
        finally:
            if self.active.pop(key)["owns_checkout"]:
                self.checkout_changed()

    def event(self, key, event):
        run = self.active.get(key)
        if run is None:
            return
        kind = event["type"]
        if kind == "quota_paused":
            run["owns_checkout"] = False
            run["state"] = "paused for Codex usage limit"
            self.checkout_changed()
        elif kind == "quota_login":
            run["state"] = "Codex login unavailable"
        elif kind != "quota_ready":
            run["state"] = "awaiting approval" if kind == "approval" else "running"

    async def reserve_after_quota(self, key, on_wait=None):
        await self.reserve(key, "waiting for checkout after Codex usage limit",
                           "reacquiring checkout after Codex usage limit", on_wait)

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
