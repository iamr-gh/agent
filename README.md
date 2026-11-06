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

`AGENT_PROVIDER` (default `openai`) and `AGENT_MODEL` can replace the corresponding flags. Keys are `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `OPENROUTER_API_KEY`. Codex uses `--provider codex`, `CODEX_ACCESS_TOKEN`, and `CODEX_ACCOUNT_ID`; token acquisition/refresh belongs outside this CLI. The CLI does not read `.env` or another tool's credentials.

### Tools and authority

- `read_file`: bounded UTF-8, numbered lines; optional `start_line`/`line_count`.
- `edit_file`: exactly one matching replacement, including rejection of overlapping matches.
- `write_file`: create or replace a file; existing parent required. Atomic sibling-file replacement preserves existing permissions, not hard-link identity, extended attributes, or inode metadata. New files are private (`0600`).
- `shell`: `/bin/sh -c` in the startup working directory. Results include exit status, stdout, and stderr.

Writes and shell commands show their full arguments and require literal `y`. Noninteractive use denies them unless **you explicitly pass `--yes`**, which grants arbitrary shell execution with your user privileges. Reads are automatic. Unknown tools/fields and incorrect argument types are rejected locally.

File tools reject resolved paths outside the startup directory. These checks and approvals are **not a sandbox**: concurrent symlink/directory swaps can race checks, reads can expose workspace secrets, and approved shell commands have unrestricted host access. Use trusted projects or external OS isolation. Root `AGENTS.md` is loaded automatically; the model is instructed to read applicable nested guides, but that is not mechanically enforced.

### Bounds and failure behavior

- Default 20 model calls per user prompt (`--max-turns N`); at most 32 tools per response.
- Files/writes: 1 MiB; each file result and each shell output stream: 32 KiB. Oversized output is marked, not silently accepted as complete.
- Shell deadline: 60 seconds; subprocess stdin is closed. Process-group cleanup also runs after normal shell completion. Detached jobs can escape cleanup, but cannot keep the output collector waiting indefinitely.
- Context: 512 KiB serialized request guard, **not a model-specific token counter**. Full history and opaque replay are kept until `/clear` or exit; nothing is silently compacted.
- Only finalized `tool_calls` responses execute tools. Incomplete responses, malformed IDs, provider errors, context exhaustion, and turn exhaustion stop with exit 1. Tool failures become matching results for the model; there are no automatic side-effect retries.
- Ctrl-C is cooperative: shell work is cleaned up at the next polling boundary; streaming is cancelled at the next event. `llm-api` currently has a 180-second request timeout and no independently interruptible idle network read. This is not yet a nonblocking TUI or a prompt-revision interface.

There is no persistence, automatic resume, background task manager, subagent system, MCP, or model catalog yet. A final model answer is not proof that its code or tests are correct. No changes to `llm-api` were needed for this first implementation.

## Verification

```sh
cargo test --locked --offline
cargo fmt --check
cargo clippy --locked --offline --all-targets -- -D warnings
```

Tests use temporary workspaces, fabricated responses, local subprocesses, and a loopback HTTP server exercising the real `llm-api::Client` tool/reasoning round trip. They require no credentials or paid model requests. Production code is in `src/main.rs`; the prompt and tests are separate.

See [design notes](docs/DESIGN.md) for the research references and intentional omissions.
