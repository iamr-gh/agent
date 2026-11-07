"""Offline PTY smoke test: python3 tests/sessions.py [path/to/agent]. No API calls."""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shutil
import subprocess
import sys
import tempfile
import termios
import time

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/agent").resolve()


class Terminal:
    def __init__(self, binary, workspace, env, *args):
        self.master, slave = pty.openpty()

        def setup():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(
            [str(binary), "--provider", "openai", "--model", "test", "--ask", *args],
            cwd=workspace, env=env, stdin=slave, stdout=slave, stderr=slave,
            preexec_fn=setup,
        )
        os.close(slave)
        self.output = b""
        self.expect(b"> ")

    def expect(self, text):
        deadline = time.monotonic() + 8
        while text not in self.output:
            assert time.monotonic() < deadline, (text, self.output)
            if select.select([self.master], [], [], 0.1)[0]:
                self.output += os.read(self.master, 65536)
        before, self.output = self.output.split(text, 1)
        return before

    def send(self, text):
        os.write(self.master, text.encode() + b"\r")

    def command(self, text):
        self.send(text)
        self.expect(text.encode())
        self.expect(b"\n")
        return self.expect(b"\r\x1b[J> ")

    def close(self):
        try:
            if self.process.poll() is None:
                self.send("/quit")
                self.expect(b"/quit")
                # Drain output while the controlling-terminal leader exits.
                deadline = time.monotonic() + 8
                while self.process.poll() is None and time.monotonic() < deadline:
                    if select.select([self.master], [], [], 0.05)[0]:
                        try:
                            os.read(self.master, 65536)
                        except OSError:
                            break
                assert self.process.wait(timeout=2) == 0
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
            os.close(self.master)


with tempfile.TemporaryDirectory(prefix="agent-sessions-") as temporary:
    root = Path(temporary)
    binary = root / "agent"
    shutil.copy2(BINARY, binary)
    workspace = root / "workspace"
    workspace.mkdir()
    env = {**os.environ, "AGENT_HOME": str(root / "home"),
           "OPENAI_API_KEY": "offline-test-not-a-real-key", "NO_COLOR": "1"}
    terminal = Terminal(binary, workspace, env)
    terminal.close()
    sessions = root / "home/sessions"
    path = next(sessions.glob("*.json"))
    saved = json.loads(path.read_text())
    saved["request"]["messages"] = [
        {"role": "user", "content": [{"type": "text", "text": "saved question"}]},
        {"role": "assistant", "content": [{"type": "text", "text": "saved answer"}],
         "replay": {"opaque": "must-survive-reload"}},
    ]
    path.write_text(json.dumps(saved))
    terminal = Terminal(binary, workspace, env, "--resume", path.stem)
    try:
        locked = subprocess.run([str(binary), "--resume", path.stem], cwd=workspace,
                                env=env, capture_output=True, timeout=5)
        assert locked.returncode == 1 and b"already open" in locked.stderr
        terminal.command("/clear")
        assert len(list(sessions.glob("*.json"))) == 2
        # Search without IDs; no match, Enter, Escape, and Ctrl-C are safe.
        terminal.send("/resume")
        terminal.expect(b"Resume > ")
        terminal.expect(b"Esc cancel")
        os.write(terminal.master, b"no such session")
        terminal.expect(b"No matching sessions")
        os.write(terminal.master, b"\r\x15saved")
        terminal.expect(b"saved question")
        os.write(terminal.master, b"\x1b")
        terminal.expect(b"\r\x1b[J> ")
        assert json.loads(path.read_text()) == saved
        terminal.send("/resume")
        terminal.expect(b"Esc cancel")
        os.write(terminal.master, b"\x03")
        terminal.expect(b"\r\x1b[J> ")
        # A locked selection reports the failure without losing this session.
        other = Terminal(binary, workspace, env, "--resume", path.stem)
        try:
            terminal.send("/resume")
            terminal.expect(b"Esc cancel")
            os.write(terminal.master, b"saved\r")
            terminal.expect(b"already open")
            terminal.expect(b"\r\x1b[J> ")
        finally:
            other.close()
        # The chat just opened by the other process is now most recent.
        # Browse down and back up before selecting it without an ID.
        terminal.send("/resume")
        terminal.expect(b"(current)")
        terminal.expect(b"Esc cancel")
        os.write(terminal.master, b"\x1b[B\x1b[A\r")
        terminal.expect(b"saved answer")
        terminal.expect(b"\r\x1b[J> ")
        assert b"saved answer" in terminal.command("/resume " + path.stem)
        # A failed exec must return to the prompt without losing the writer lock.
        parked = root / "parked"
        os.replace(binary, parked)
        assert b"reload failed" in terminal.command("/reload")
        locked = subprocess.run([str(parked), "--resume", path.stem], cwd=workspace,
                                env=env, capture_output=True, timeout=5)
        assert locked.returncode == 1 and b"already open" in locked.stderr
        os.replace(parked, binary)
        # Simulate a build replacing the running executable's inode.
        replacement = root / "replacement"
        shutil.copy2(BINARY, replacement)
        os.replace(replacement, binary)
        pid = terminal.process.pid
        assert b"saved answer" in terminal.command("/reload")
        assert terminal.process.pid == pid
        assert json.loads(path.read_text()) == saved
        assert len(list(sessions.glob("*.json"))) == 2
        # The reloaded process still holds the writer lock.
        locked = subprocess.run([str(binary), "--resume", path.stem], cwd=workspace,
                                env=env, capture_output=True, timeout=5)
        assert locked.returncode == 1 and b"already open" in locked.stderr
    finally:
        terminal.close()
    assert not list(sessions.glob(".agent-*.tmp"))
    assert "offline-test-not-a-real-key" not in path.read_text()
print("PASS: saved context, /clear, /resume, writer lock, and /reload after executable replacement")
