# Agent

Basic coding agent, for me, by me. Distills core concepts found in other projects.

Meant to have minimal dependencies, small codebase readable by one person. Starting from basic loop, will add a minimal set of high leverage features.

UI will have forgivable complexity as needed to make it nice. UX should always feel instantaneous, and never block user interaction.

Currently, high leverage features could be:
- good subagent use/orchestration
- better support of prompt edits/revisions/forking
- swarm support (agent-agent comms)
- self-scaffolding/RLM esque features
- seamless syncing to cloud agent and back
- smarter permissions management and sandboxing of sensitive assets

## First implementation

A small blocking Rust/Unix CLI using [`../llm-api`](../llm-api) directly. Provider HTTP, SSE, authentication headers, and native reasoning replay stay in that library. No provider SDK, async runtime, TUI framework, or duplicated API adapter.

Requires Rust 1.85+, macOS or Linux, and the sibling `llm-api` checkout.

```sh
cargo build --release --locked
export OPENAI_API_KEY='...'
./target/release/agent --provider openai --model YOUR_MODEL
./target/release/agent --provider anthropic --model YOUR_MODEL 'Inspect this project'
```

Run the binary **from the directory you want it to work in**. Options precede the prompt. No prompt starts a line-oriented conversation; `/clear` resets history and `/quit` exits. Use `--` before a prompt beginning with `-`.

The defaults are provider `codex` and model `gpt-6.1-sol`. `AGENT_PROVIDER` and `AGENT_MODEL` override these defaults; the corresponding command-line flags take precedence. Keys are `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `OPENROUTER_API_KEY`. The CLI does not load `.env`.

### One-command Codex launch

From this repository, using your current fx Codex login:

```sh
cargo run --release
```

`src/auth.rs` owns authentication/profile discovery; `llm-api` only receives the access token and account ID. Codex credential precedence is:

1. Both `CODEX_ACCESS_TOKEN` and `CODEX_ACCOUNT_ID`, if supplied. Supplying only one is an error.
2. `$AGENT_HOME/codex-auth.json`, or `~/.agent/codex-auth.json` by default.
3. Compatibility fallback: `$FX_HOME/chatgpt-auth.json`, or `~/.fx/chatgpt-auth.json`.

File profiles must be private, user-owned regular files, not symlinks. The version-1 schema needs `access_token`, `account_id`, and `expires_at_ms` at least 60 seconds in the future. Existing files are read-only: nothing is copied, printed, rotated, or refreshed. An invalid agent-owned profile fails rather than silently falling back. Environment credentials or an agent-owned profile avoid any fx runtime dependency; acquiring/refreshing those credentials is still external.

A fully independent browser/device OAuth login and refresh flow belongs in this application's auth module (or a separate reusable auth crate), not the provider transport library. It is not implemented yet. If the existing fx token expires, renew the login in fx and rerun the command.

### Tools and authority

- `read_file`: bounded UTF-8, numbered lines; optional `start_line`/`line_count`.
- `edit_file`: exactly one matching replacement, including rejection of overlapping matches.
- `write_file`: create or replace a file; existing parent required. Atomic sibling-file replacement preserves existing permissions, not hard-link identity, extended attributes, or inode metadata. New files are private (`0600`).
- `shell`: `/bin/sh -c` in the startup working directory. Results include exit status, stdout, and stderr.

**All valid tool calls are allowed without confirmation by default**, including arbitrary shell execution with your full user privileges. Writes and shell commands still show their full arguments. Pass `--ask` to require literal `y` for writes and shell commands (noninteractive use then denies them); `--yes` restores automatic approval. Reads are automatic in either mode. Unknown tools/fields and incorrect argument types are rejected locally.

File tools reject resolved paths outside the startup directory. These checks and approvals are **not a sandbox**: concurrent symlink/directory swaps can race checks, reads can expose workspace secrets, and approved shell commands have unrestricted host access. Use trusted projects or external OS isolation. Root `AGENTS.md` is loaded automatically; the model is instructed to read applicable nested guides, but that is not mechanically enforced.

### Bounds and failure behavior

- Default 20 model calls per user prompt (`--max-turns N`); at most 32 tools per response.
- Files/writes: 1 MiB; each file result and each shell output stream: 32 KiB. Oversized output is marked, not silently accepted as complete.
- Shell deadline: 60 seconds; subprocess stdin is closed. Process-group cleanup also runs after normal shell completion. Detached jobs can escape cleanup, but cannot keep the output collector waiting indefinitely.
- Context: 512 KiB serialized request guard, **not a model-specific token counter**. Full history and opaque replay are kept until `/clear` or exit; nothing is silently compacted.
- Only finalized `tool_calls` responses execute tools. Incomplete responses, malformed IDs, provider errors, context exhaustion, and turn exhaustion stop with exit 1. Tool failures become matching results for the model; there are no automatic side-effect retries.
- Ctrl-C cancels the current interactive turn and returns to the prompt, including while waiting for input or approval. During a model request it uses `llm-api::Client::run_cancellable` to close idle sockets without waiting for another event. Shell process-group cleanup runs before continuing. Batch cancellation exits 130. DNS/connect stages may still take up to five seconds each; callbacks and file writes are not forcibly preempted. This is not yet a nonblocking TUI or a prompt-revision interface.
- Cancellation is not rollback: completed changes remain. Partial model output is not appended as an assistant response; advertised tool calls retain matching results, including interrupted/unstarted calls. History gains an interruption note, and later turns are told to inspect state before retrying effects. Context is retained unless you use `/clear`.

There is no persistence, automatic resume, background task manager, subagent system, MCP, or model catalog yet. A final model answer is not proof that its code or tests are correct. Provider protocols remain in `llm-api`; cancellation uses its standard-library atomic-flag API.

## Verification

```sh
cargo test --locked --offline
cargo fmt --check
cargo clippy --locked --offline --all-targets -- -D warnings
```

Tests use temporary workspaces, fabricated responses, local subprocesses, and a loopback HTTP server exercising the real `llm-api::Client` tool/reasoning round trip. They require no credentials or paid model requests. Production code is in `src/main.rs`; the prompt and tests are separate.

Opt-in live CLI checks are in `tests/live.py`; `--scenario code` exercises a real Rust bug fix with independently recompiled tests. For example, `python3 -u tests/live.py --provider codex --model gpt-6.1-sol --scenario code` uses a disposable workspace and normal approval prompts, with the existing read-only login bridge. See [live results](docs/LIVE_TESTS.md) for the provider matrix, exact commands, and the preserved OpenRouter newline failure.

See [design notes](docs/DESIGN.md) for the research references and intentional omissions.
