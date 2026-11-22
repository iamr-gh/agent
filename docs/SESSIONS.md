# Saved sessions and reload

Sessions are saved automatically under `$AGENT_HOME/sessions` (default `~/.agent/sessions`).

- `/resume` opens a quick switcher for the current canonical workspace, most recently updated first. Type to filter by the first prompt, model, or ID; use ↑/↓ to browse, Enter to resume, and Esc or Ctrl-C to cancel. Entries show the first prompt, model, relative update time, and a `(current)` marker. Choosing the current session is a no-op.
- `/resume ID` restores one and prints the complete saved conversation in order, without message-count or text-length clipping. `ag --resume ID` does the same at launch. History uses the live UI: colored user prompts, Markdown answers, distinct reasoning, and compact tool summaries (`--verbose` shows tool arguments and bounded results). `NO_COLOR` uses escaped plain text. Every saved turn is included; tool details follow the same compact/verbose policy as live operation. Opaque provider replay is retained for the model, not printed. Elapsed tool times are not saved and are omitted on restore.
- `/reload` saves and execs the executable at its startup path with `--resume ID`. Build a new executable first to load new code. This preserves the PID, current permission mode, verbosity, and budgets; it does not build automatically.
- `/clear` starts a new saved session without deleting the old one.
- `/compact` summarizes older context in place. Successful manual or automatic compaction saves the original as a separate private session before updating the active one. The terminal prints the backup ID, usable with `/resume ID`; the original workspace state is not rolled back.
- `/model` opens a searchable model picker for the current provider; `/model ID` also accepts custom IDs. Switching starts a new saved session because provider replay belongs to the original model. New sessions default to `gpt-6.2-sol` unless overridden by `--model` or `AGENT_MODEL`; saved sessions keep their model. Codex discovery runs on a detached background thread, using the existing subscription login. Startup and the picker never wait for catalog disk/network work. The worker loads an account-scoped private `codex-models-*.cache` under the sessions directory and refreshes it if older than one hour, with a five-second catalog HTTP timeout. Like fx, it first resolves the current Codex client version from the public npm package metadata (three-second timeout, no subscription credentials sent). At most one refresh runs per process; restarting/reloading refreshes again when the cache is stale. On failure, stale cache or the built-in suggestions remain usable. Reopen `/model` to see a completed refresh; an open picker keeps a stable snapshot. No prompts, tools, or credentials are stored in the catalog cache, and no generation request is made. Other providers remain built-in lists; fx is not required. `AGENT_MODEL_DISCOVERY=off` disables discovery for offline operation/tests.

Interactive resume/reload wait for input: they do not send a model request or replay tools. Supplying a prompt with `ag --resume ID 'PROMPT'` instead appends it and starts a new turn. Authentication credentials are resolved again, not deliberately saved in the snapshot; prompts and tool output may still contain secrets. Cross-workspace resume and changed tool schemas are rejected. A session already open in another process cannot be resumed until that process closes it. Selecting the current session does not reacquire its lock. If switching fails, the current session remains selected; if reload fails, the current process keeps its lock and returns to the prompt.

The picker title comes from the first user message (or `(empty)`), and recency comes from the snapshot file's modification time. Filtering is a case-insensitive substring search over that title, model, and ID, not a full-transcript search. The picker only browses sessions bound to the current canonical workspace; it does not synchronize or move them between machines.

## Minimal representation

Each version-1 JSON snapshot contains `workspace`, `provider`, the complete `llm_api::Request`, and an `active` boolean. The request is the single context representation: saved system instructions, tool schemas, model/options, ordered messages, raw bounded tool results, and opaque assistant replay. Terminal deltas, ANSI styles, credentials, and approval grants are not stored. Resume preserves old instructions rather than silently refreshing them from changed project files, and uses the saved provider/model request. Permission policy comes from the current invocation (`--ask`/`--yes`), not the snapshot; tools are automatically allowed unless `--ask` is supplied. Input-editor history is in-memory UI state and is not restored from the saved conversation.

The session directory is private (0700), snapshots/lock files are private (0600), and writers use an advisory lock with close-on-exec. Snapshots use a synced temporary file and atomic rename, followed by directory sync. Unlike a journal, no partial record recovery or index is needed: a crash leaves the previous or new snapshot. Corrupt snapshots are rejected. Full-context rewrites trade efficiency for simplicity; there is no append-only audit log, fork graph, or filesystem rollback.

## Effect boundaries

Before executing an assistant batch, save its intact message and one ordered result placeholder per call (`not executed`). Before each call, save `outcome unknown; effects may remain`. Immediately after each call, replace that placeholder with its actual result and save again. A checkpoint failure stops further execution. Incomplete model responses are not accepted into durable context.

Loading an active snapshot adds one recovery notice and marks it inactive. Completed results survive; unknown/not-executed results remain explicit. No saved call is automatically executed. A crash after an effect but before saving its result cannot be distinguished from a failed/unstarted effect: inspect workspace state before retrying. This is deliberately not an exactly-once guarantee. Partial streaming text is display-only until a complete model response is accepted.

If a model request fails, `ag` retries it once after a short delay for recognized transient failures, but only if no stream event arrived. If the request still fails, the interactive prompt stays open and `/retry` sends the same saved context without adding a user message or replaying completed tools. Noninteractive runs exit with an error. Error diagnostics omit raw provider payloads.

These files can contain sensitive source, prompts, or command output; private permissions are not encryption. No automatic retention/deletion is implemented.

## Offline verification

```sh
cargo test --locked --offline
cargo build --release --locked --offline
python3 tests/sessions.py
```

The PTY test uses a disposable private home, synthetic credentials, and no model requests. It checks picker filtering/navigation/cancellation, locked selections, direct resume, clearing, exact context preservation, and reload after replacing the executable inode. Unit tests cover native replay round trips, crash recovery between tools, failed checkpoints, schema/version/workspace rejection, corruption, and permissions. See [verification](LIVE_TESTS.md) for the full offline workflow and live-test limitations, and [design](DESIGN.md) for module boundaries.
