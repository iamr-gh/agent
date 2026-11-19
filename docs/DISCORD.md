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

- In a server text channel, run `/agent prompt:Describe this project`.
- The bot creates a public thread. Reply in that thread to continue; only text is submitted (attachments are not ingested).
- File writes/edits and every shell command require a fresh Approve/Deny decision. Review the complete `approval.json` attachment before approving. Buttons are owner-only, single-use, and expire after two minutes; timeout denies the action.
- `/agent_stop` cancels the single active turn. Already completed effects are not rolled back.
- One turn can run at a time across all threads; busy prompts are rejected, not queued. Avoid simultaneously editing the same workspace through the terminal agent.
- Long answers are sent as UTF-8 attachments. Raw tool output and model reasoning are not posted; mention generation is disabled. Answers and approval arguments can still expose repository secrets, so use only private, trusted channels.

Thread-to-session mappings and received Discord event IDs persist in `~/.agent/discord/state.sqlite3`; model context uses the existing private saved sessions under `$AGENT_HOME/sessions`. Restart the bot and reply in the same thread to resume. A crashed or cancelled turn is never automatically retried, and saved tools are never replayed. Events received while offline are not backfilled. Deduplication prevents duplicate received prompts but is not an exactly-once guarantee across crashes.

## Boundaries and limitations

`ag --discord-worker` is an internal JSON-lines subprocess entry point: the adapter supplies a prompt/session on stdin, receives session/answer/approval/done events on stdout, and returns an explicit boolean for each approval. It always disables automatic tool approval. Each turn has an isolated process and reuses the existing engine, credential discovery, and session checkpoints, with limits of 100 model calls and 4 MiB serialized context. The worker doesn't stream partial reasoning or answers to Discord.

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
