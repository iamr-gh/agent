# Design

This document describes the current application. See [saved sessions](SESSIONS.md) for persistence and recovery, and [verification](LIVE_TESTS.md) for checks and test limitations.

## Responsibilities

`ag` is a Unix terminal application with a sequential conversation/tool loop. The `llm-api` dependency handles provider requests, streaming events, native message replay, and network cancellation. The application owns prompts, credentials, tools, approvals, workspace effects, sessions, and terminal presentation.

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | Entry point, signal handling, and exit status |
| `src/engine.rs` | Conversation/tool loop, budgets, argument validation, workspace file operations, shell execution, and cancellation checks |
| `src/auth.rs` | Read-only Codex credential discovery and profile validation |
| `src/skills.rs` | Skill discovery, metadata catalog, and on-demand instruction loading |
| `src/session.rs` | Private snapshots, writer locks, recovery notices, and session summaries |
| `src/ui.rs` | Terminal frontend, approval prompts, tool summaries, and model-output rendering |
| `src/ui/cli.rs` | CLI options, interactive commands, resume, and reload |
| `src/ui/input.rs` | Unicode-aware input editing, history, and terminal mode management |
| `src/ui/live.rs` | Live busy-turn input area, queued follow-ups, and synchronized terminal output |
| `src/ui/picker.rs` | Searchable workspace session switcher |

The engine has no terminal I/O or presentation dependency. Its `Frontend` interface receives tool activity, diagnostics, and approval requests; inference is supplied as a callback. This lets tests exercise the engine without a terminal. These are in-process module boundaries, not a client/server protocol.

Direct dependencies are `llm-api`, `serde_json`, `libc`, `unicode-segmentation`, `unicode-width`, and `termimad`. The Cargo manifest currently uses a sibling checkout for `llm-api`; this is a build requirement, not a documentation reference. No external source or design review is needed to understand these docs.

## Conversation and tools

The loop is:

1. Append the user message and checkpoint the request.
2. Call the provider with cancellation support.
3. Accept a complete assistant message, preserving its opaque provider replay data.
4. Validate and authorize tool calls, execute them sequentially, and record ordered results.
5. Continue until the model finishes, a budget is exceeded, an error occurs, or cancellation is requested.

Partial tool-call deltas and incomplete model responses are not executed. Tool failures become explicit results. Provider/protocol errors are not silently retried. The persistence checkpoints around effects are described in [saved sessions](SESSIONS.md).

The available tools are `load_skill`, `read_file`, `write_file`, `edit_file`, and `shell`. File tools resolve paths against the canonical workspace and reject escapes. Edits require exactly one matching occurrence. Writes use atomic replacement and revalidate the destination after approval. Reads are limited to 1 MiB files; tool output is bounded to 32 KiB. Shell commands run through `/bin/sh` in the workspace, with a 60-second timeout and bounded stdout/stderr capture.

**Tools are automatically allowed by default.** `--ask` requires confirmation for shell commands and file writes; `--yes` enables automatic approval. An approval prompt shows the complete proposed arguments even when ordinary tool output is compact. Nonterminal approval fails closed. These policies are not a sandbox: shell commands run with the user's full privileges and can access files outside the workspace.

The default per-prompt limits are 500 model calls and 4096 KiB of serialized context. The hard limit includes provider replay, is not a token estimate, and still applies after compaction. Provider limits may be lower.

Automatic compaction checks the normalized conversation content before each model call, including between completed tool batches. The default threshold is 128 KiB, excluding opaque replay; set `AGENT_COMPACT_KIB` to change it or `0` to disable automatic compaction. This is a local size heuristic, not the provider's context window. `/compact` requests the same operation manually, regardless of the threshold. Compaction sends an older prefix (at most 192 KiB) to the current model in a separate, tool-free request. It keeps at least eight recent messages, the latest user request, and whole tool batches. Automatic attempts count against the per-prompt model-call limit; there must still be budget for the task request. If summarization fails or does not shrink the context, the original stays active. A failed automatic attempt is not repeated in the same turn.

Set `AGENT_PAGE_TOOL_OUTPUT=1` to try historical tool-output paging. It is off by default. The inference request keeps at least the latest 16 messages intact and revisits older results at 16-message boundaries rather than rewriting the prefix every call. Old successful `read_file` and successful-exit `shell` outputs of at least 6 KiB keep roughly 2 KiB at each end. The model can recover omitted bytes in 8 KiB pages with `read_tool_output`, without rerunning any tools. User messages, skill instructions, tool errors, native replay, the saved transcript, and the local serialized context limit are unchanged. Paging edits old prefixes, which can affect provider prompt-cache hits; wire-byte savings alone do not establish token or quota savings.

To measure serialized provider-request bytes without model calls or tool execution, run `AGENT_TRACE_FILES=/path/to/first.json:/path/to/second.json cargo test --locked --offline replay_saved_traces -- --ignored --nocapture`. This reconstructs one request per saved assistant response and prints aggregate bytes, not transcript content.

## Instructions, skills, and credentials

A new session's system instructions combine the built-in prompt, workspace path, skill metadata catalog, and root `AGENTS.md` when present. Skills are loaded on demand by catalog name. Discovery searches user-level and workspace-level `.agents/skills` and agent-specific skill directories; later roots override earlier names. Skill loading can read installed skills outside the workspace without widening ordinary file-tool access. The CLI does not automatically assemble hierarchical subdirectory instructions.

Supported provider modes are OpenAI, Anthropic, OpenRouter, and Codex. API-key modes use their corresponding environment variables. Codex credential discovery checks, in order:

1. Both `CODEX_ACCESS_TOKEN` and `CODEX_ACCOUNT_ID`, if supplied.
2. The agent-owned `codex-auth.json` profile under `AGENT_HOME` (default `~/.agent`).
3. A read-only compatibility login at `$FX_HOME/chatgpt-auth.json` (default `~/.fx/chatgpt-auth.json`) when the agent-owned profile is absent.

Profiles must be private, user-owned regular files, not symlinks, and no larger than 1 MiB. The version-1 profile contains `access_token`, `account_id`, and `expires_at_ms`; tokens expiring in less than a minute are rejected. The application does not perform OAuth login or token refresh, and never writes these profiles.

## Terminal behavior

On a styled terminal, `termimad` renders buffered Markdown for answers and reasoning. Each role's text is flushed when the role changes or the request ends, including error/cancellation cleanup. Reasoning uses a distinct color. This is buffered rendering, not incremental Markdown parsing on every network chunk. Nonterminal output or `NO_COLOR` uses escaped plain streaming text, with a `Thinking:` label for reasoning. Rendering does not alter the stored request or tool results.

Tool output is compact by default; `--verbose` shows full arguments and bounded results. Untrusted control characters are escaped before presentation. The session picker additionally flattens whitespace and clips entries by display width.

Interactive chat keeps the controlling terminal in noncanonical, no-echo mode during both input and model/tool output. While the agent works, a visible input area at the bottom of the terminal supports the same editing keys as the idle prompt. Enter queues a follow-up; queued messages (including slash commands) are handled in submission order after the current turn finishes, never injected into an active model request or tool batch. An unfinished draft and its cursor survive the transition back to the idle prompt and through queued turns. Ctrl-C cancels the active turn and discards pending queued input and the draft. Queued input is in memory only, not saved until submitted to the conversation.

The busy-turn reader polls nonblocking input every 16 ms. Model and tool output share a synchronized scrolling region above the input area. Approval prompts temporarily suspend the reader and restore normal output; already queued input and the live draft cannot answer an approval. Idle input uses `select` with a 25 ms timeout for interruption and resize checks. Redirected output and terminals shorter than five rows at turn start retain the ordinary typeahead behavior without the live area. Terminal settings and the full scrolling region are restored on exit and before reload.

The editor supports grapheme-aware left/right movement, Home/End, Delete/Backspace, and Up/Down navigation through in-memory submitted-input history. Moving past the newest history entry restores the unfinished draft. Wrapped input scrolls to keep the cursor visible. Approval prompts have separate disposable history. Prompt history is not persisted across restarts or reloads; saved conversation context is.

`/resume` opens the searchable session picker; `/clear` starts a new session; `/reload` replaces the process with the executable at its captured startup path; `/quit` exits. Details are in [saved sessions](SESSIONS.md).

## Cancellation and limits

SIGINT sets a process-global flag. The engine checks it around inference, approval, and tool execution. Network requests use the provider client's cancellation path. Shell cleanup targets the child process group; completed effects are not rolled back. Interactive cancellation records a continuation warning and returns to the prompt; cancelled batch execution exits 130.

Cancellation is cooperative, not an ability to preempt arbitrary filesystem work or callbacks. Detached shell jobs may escape process-group cleanup. There is no distributed session lock, exactly-once execution guarantee, filesystem rollback, external sandbox, RPC server, or live cross-machine synchronization. A remote frontend would still need event/request identifiers, transport and authentication, disconnect/approval policy, and per-session rather than process-global cancellation.
