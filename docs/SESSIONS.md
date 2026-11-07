# Saved sessions and reload

Sessions are saved automatically under `$AGENT_HOME/sessions` (default `~/.agent/sessions`).

- `/resume` lists sessions for the current canonical workspace, newest first.
- `/resume ID` restores one and shows recent text. `agent --resume ID` does the same at launch.
- `/reload` saves and execs the executable at its startup path with `--resume ID`. Build a new executable first to load new code. This preserves the PID, current permission mode, verbosity, and budgets; it does not build automatically.
- `/clear` starts a new saved session without deleting the old one.

Resume/reload wait for input: they do not send a model request or replay tools. Credentials are resolved again, not saved. Cross-workspace resume and changed tool schemas are rejected. A session already open in another process cannot be resumed until that process closes it. If reload fails, the current process keeps the session lock and returns to the prompt.

## Minimal representation

Each version-1 JSON snapshot contains `workspace`, `provider`, the complete `llm_api::Request`, and an `active` boolean. The request is the single context representation: saved system instructions, tool schemas, model/options, ordered messages, raw bounded tool results, and opaque assistant replay. Terminal deltas, ANSI styles, credentials, and approval grants are not stored. Resume preserves old instructions rather than silently refreshing them from changed project files. Permission policy comes from the current invocation (`--ask`/`--yes`), not the snapshot.

The session directory is private (0700), snapshots/lock files are private (0600), and writers use an advisory lock with close-on-exec. Snapshots use a synced temporary file and atomic rename, followed by directory sync. Unlike a journal, no partial record recovery or index is needed: a crash leaves the previous or new snapshot. Corrupt snapshots are rejected. Full-context rewrites trade efficiency for simplicity; there is no append-only audit log, compaction, fork graph, or filesystem rollback.

## Effect boundaries

Before executing an assistant batch, save its intact message and one ordered result placeholder per call (`not executed`). Before each call, save `outcome unknown; effects may remain`. Immediately after each call, replace that placeholder with its actual result and save again. A checkpoint failure stops further execution. Incomplete model responses are not accepted into durable context.

Loading an active snapshot adds one recovery notice and marks it inactive. Completed results survive; unknown/not-executed results remain explicit. No saved call is automatically executed. A crash after an effect but before saving its result cannot be distinguished from a failed/unstarted effect: inspect workspace state before retrying. This is deliberately not an exactly-once guarantee. Partial streaming text is display-only until a complete model response is accepted.

These files can contain sensitive source, prompts, or command output; private permissions are not encryption. No automatic retention/deletion is implemented.

## Offline verification

```sh
cargo test --locked --offline
cargo build --release --locked --offline
python3 tests/sessions.py
```

The PTY test uses a disposable private home, synthetic credentials, and no model requests. It checks listing/restoring/clearing, writer locking, exact context preservation, and reload after replacing the executable inode. Unit tests cover native replay round trips, crash recovery between tools, failed checkpoints, schema/version/workspace rejection, corruption, and permissions.
