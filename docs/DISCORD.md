# Personal Discord frontend

This optional Python adapter runs on the same Unix machine as `ag`, in the repository containing this document. The terminal CLI is unchanged. Only Discord user **383714960498229250** can start sessions, send prompts, cancel, or approve tools. This is a personal-server integration, not a multi-tenant service.

## Setup

1. Create a Discord application/bot, enable Message Content Intent, and invite it with the `bot` and `applications.commands` scopes. Grant View Channels, Send Messages, Send Messages in Threads, Create Public Threads, Read Message History, and Attach Files. Do not grant Administrator.
2. Put `DISCORD_BOT_TOKEN=...` (without quotes) in `~/.config/agent/discord.env`. Keep the directory mode 700 and file mode 600. Never commit the token. Use the bot token, not a Discord user account token.
3. Build `cargo build --release --locked --offline` from the repository root. Model authentication is the same as the CLI (`AGENT_PROVIDER`, `AGENT_MODEL`, provider keys or the private Codex profile). The adapter does not load other variables from `discord.env`.
4. Install Python 3.12+ and create a private virtual environment:

   ```sh
   python3 -m venv ~/.local/share/agent-discord-venv
   ~/.local/share/agent-discord-venv/bin/python -m pip install -r integrations/discord/requirements.txt
   ```

   On Debian/Ubuntu, creating a venv normally requires `python3-venv`. If unavailable, create it with `--without-pip` and use the official PyPA `pip.pyz` bootstrap inside that environment instead. No global Python packages are needed.
5. Start from the repository root:

   ```sh
   ~/.local/share/agent-discord-venv/bin/python integrations/discord/bot.py
   ```

Keep the process running; it does not install an automatic startup service. Global slash commands may take time to appear in Discord. Stop with Ctrl-C. The token is read only at startup, is not logged, and is excluded from worker environments.

## Use

- In a server text channel, run `/agent prompt:Describe this project`. New threads default to **automatic tool approvals** and **25 model calls per prompt**. You can explicitly choose `auto_approve:false` and `max_turns:10` (allowed range: 1–100).
- The bot creates a public thread. Reply in that thread to continue; only text is submitted (attachments are not ingested). Each reply gets a fresh model-call budget. At the cap, work stops without another inference request; completed changes and context remain, and the bot reports the limit rather than a generic error.
- Auto mode permits every shell command and file change with full local account privileges, just like the CLI's `--yes`. **The cap counts model calls, not individual tool calls, runtime, money, or changes.** Each model response can contain multiple tool calls; a cap is not a security boundary.
- Use `/agent_settings auto_approve:true max_turns:25` inside an existing agent thread to change its policy for subsequent prompts, or set `auto_approve:false` for buttons. Settings persist across restarts and can only be changed by the owner while the thread is idle. Threads created before this feature stay in manual mode until you explicitly change them.
- In manual mode, file writes/edits and every shell command require a fresh Approve/Deny decision. Review the complete `approval.json` attachment before approving. Buttons are owner-only, single-use, and expire after two minutes; timeout denies the action.
- `/agent_stop` cancels the single active turn. Already completed effects are not rolled back.
- One turn can run at a time across all threads; busy prompts are rejected, not queued. Avoid simultaneously editing the same workspace through the terminal agent.
- Discord asks the model for concise final summaries (changes, checks, blockers) rather than tool-by-tool narration. Intermediate tool-step commentary, raw tool output, and reasoning are not posted. Full context/tool results remain in the local saved session. Long final answers are sent as UTF-8 attachments; mention generation is disabled. Answers and approval arguments can still expose repository secrets, so use only private, trusted channels.

Thread-to-session mappings and received Discord event IDs persist in `~/.agent/discord/state.sqlite3`; model context uses the existing private saved sessions under `$AGENT_HOME/sessions`. Restart the bot and reply in the same thread to resume. A crashed or cancelled turn is never automatically retried, and saved tools are never replayed. Events received while offline are not backfilled. Deduplication prevents duplicate received prompts but is not an exactly-once guarantee across crashes.

## Boundaries and limitations

`ag --discord-worker` is an internal JSON-lines subprocess entry point: the adapter supplies a prompt/session and the owner-selected `auto_approve`/`max_turns` policy on stdin, receives session/answer/approval/limit/done events on stdout, and returns an explicit boolean for each manual approval. Missing policy fields default to manual mode and 25 calls; malformed fields or limits outside 1–100 fail before tools run. Permissions are never inferred from model output or loaded from saved model context. Each turn has an isolated process and reuses the same engine, tools, credential discovery, and session checkpoints, with a 4 MiB serialized context limit. Only the final complete answer is sent to Discord, not intermediate tool-step commentary or reasoning. The terminal CLI is unchanged.

A disconnect cancels active work and invalidates pending approval. Process cleanup first sends SIGINT and denies any waiting approval; after five seconds it kills an unresponsive worker. Interrupted snapshots retain recovery warnings, not automatic retries. Detached shell jobs may survive cancellation, as with the CLI. Provider failures are summarized without uploading raw errors; inspect local credentials and saved context to diagnose them.

**This is not a sandbox.** Shell tools run with the local account's full privileges and can access files outside the workspace, including the token file if explicitly commanded. Prefer a restricted OS account/container before broader use. The allowlist is fixed in `integrations/discord/bot.py`; this implementation does not restrict a particular server ID, so don't invite it to untrusted servers. No automatic startup, remote workspace selection, session import command, or file upload support is installed.

## Verification

```sh
cargo fmt --check
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --release --locked --offline
~/.local/share/agent-discord-venv/bin/python -m unittest discover -s integrations/discord -p 'test_*.py' -v
```

Python tests use synthetic credentials and mocked Discord/worker objects. They cover private token storage, token environment stripping, owner authorization, expired/single-use approvals, durable event deduplication, session mapping, cancellation/disconnect, and failure reporting. They make no network/model calls; a real Discord conversation and tool approval still need a live smoke test.

### Initial local verification

- `cargo fmt --check`: passed.
- `cargo test --locked --offline`: 53 passed, 8 failed in terminal cancellation tests with PTY `Input/output error` at `tests/unit/cancellation.rs:166`. These test files were not changed by this integration.
- `cargo test --locked --offline discord::`: passed (1 test).
- `cargo test --locked --offline -- --skip tests::cancellation`: passed (49 tests).
- `cargo clippy --locked --offline --all-targets -- -D warnings`: blocked by existing `useless_format` in `tests/unit/frontend.rs:71` and `unnecessary_mut_passed` in `tests/unit/cancellation.rs:87`.
- `cargo clippy --locked --offline --bin ag -- -D warnings`: passed.
- `cargo build --release --locked --offline`: passed; local `~/.local/bin/ag` symlink refreshed and `ag --help` verified.
- `~/.local/share/agent-discord-venv/bin/python -m unittest discover -s integrations/discord -p 'test_*.py' -v`: passed (12 tests).
- Live startup authenticated and registered commands, but the Gateway rejected Message Content Intent (`PrivilegedIntentsRequired`). Enable that intent for the application corresponding to the installed token, save, then restart. No model requests or live tool effects were tested. No bot process is left running.
- A subsequent production Clippy recheck was blocked by concurrent, unrelated edits to `src/ui/input.rs` (undefined `editor`, `escape`, and `pending` variables). The successful release build above predates those edits; no changes to that file are included in the Discord integration.

### Bounded auto-approval verification

- `cargo fmt --check`: passed.
- `cargo test --locked --offline discord::`: 3 passed (strict manual grants, validated policy, and automatic shell execution stopping at the exact model-call cap).
- `cargo test --locked --offline`: 74 passed, 1 live metadata test ignored. The earlier PTY failures above no longer occur with the current working tree.
- `cargo clippy --locked --offline --all-targets -- -D warnings`: still blocked by the same two unrelated test-code warnings listed above.
- `cargo clippy --locked --offline --bin ag -- -D warnings`: passed.
- `cargo build --release --locked --offline`: passed; local executable symlink refreshed and help checked.
- `~/.local/share/agent-discord-venv/bin/python -m unittest discover -s integrations/discord -p 'test_*.py' -v`: 17 passed, including persistent policy settings, owner checks, old-database migration, bounded protocol, and corrupt-policy rejection.
- Live worker smoke test: automatic mode executed only `printf DISCORD_AUTO_OK`, stopped at a one-model-call cap, emitted no approval prompt, and retained a successful tool result in an inactive saved session. The Discord connection and a live model response were also verified after the intent was enabled. Approval buttons and the new slash settings still need an owner-driven Discord interaction test.
