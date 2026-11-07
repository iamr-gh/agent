# Design notes

The starting reference is the local [harness taxonomy](../../harness_test/TAXONOMY.md) and all six [reviews](../../harness_test/reviews/): core harnesses; first-party agents; orchestrators; Jcode/OpenClaw/RVM/pi; commercial products; evidence audit. These are static architectural evidence, not performance rankings. In particular, unavailable commercial internals and RVM's registry lifecycle are not treated as implementations of a coding loop.

The implementation is original code, not copied source. Useful source comparisons from the pinned local snapshots:

- [pi's agent loop](../../harness_test/sources/pi/packages/agent/src/agent-loop.ts): keep conversation state distinct from individual requests; execute finalized calls and pair results with IDs.
- [OpenClaw's loop](../../harness_test/sources/openclaw/packages/agent-core/src/agent-loop.ts): every executed or failed tool must settle to a result. Its orchestration, recovery, steering, and plugin machinery are deliberately not reproduced.
- [`llm-api` request and response types](../../llm-api/src/lib.rs) and [protocol tests](../../llm-api/tests/protocols.rs): preserve `response.message` intact, including opaque reasoning replay; never execute argument deltas or truncated responses.

The reviews' broader lessons are reflected in the boundary choices: the first-party products separate instructions from authority, Aider demonstrates that editing is its own correctness problem, orchestrators solve a larger lifecycle problem, and the commercial review explicitly limits what can be concluded from public evidence. A small line count is a readability constraint, not evidence of benchmark superiority.

## Ownership

`llm-api` owns provider selection, encoding, streaming normalization, authentication headers, usage, finish reasons, and native replay. `agent` owns the conversation, prompt, tool schemas, argument checks, approval, filesystem effects, subprocesses, and terminal rendering. Current `llm-api` providers are OpenAI, Anthropic, OpenRouter, and Codex; its older README provider roster is not authoritative.

There is one sequential loop: user message → `Client::run_cancellable` → intact assistant message → validated/authorized tools → user-role results → next request. Failed tools are observable results. Model/protocol/budget errors terminate rather than silently retrying effects. Output control characters are escaped before terminal rendering, but the model receives the raw bounded output.

### Engine / frontend boundary

- `src/engine.rs`: conversation/tool loop, budgets, validation, approval policy, workspace containment, file operations, subprocesses, and cancellation checks. It has no terminal I/O, styling, or dependency on `ui`.
- `src/skills.rs` and `src/auth.rs`: skill and credential logic; skill discovery reports raw diagnostics through the frontend port rather than printing.
- `src/ui.rs`: terminal adapter for the engine's `Frontend` port, full approval prompts, compact/verbose tool summaries, escaping, styles, and streamed model presentation.
- `src/ui/input.rs`: terminal input editing and redraw.
- `src/ui/cli.rs`: CLI configuration and interactive controller (including `/clear`, `/quit`, and displaying inference events).
- `src/main.rs`: wiring and process signal/exit handling.

`Frontend` receives raw tool-start/tool-finish events, durations, diagnostics, and approval requests. The engine retains approval policy (`--yes` versus asking), checks cancellation after an approval reply, and revalidates files before writing. Inference is an injected callback, so rendering model deltas is not part of the engine. Tests exercise the engine with a nonterminal recorder as well as the real terminal adapter. Neither Markdown nor ANSI escapes are inserted into stored messages or tool results.

These are module boundaries, not separate packages or a wire protocol yet. A later server can own the engine, request/transcript, and tool execution, forwarding events to a client and awaiting explicit approval replies. That extraction still needs serializable event/request IDs, disconnect/approval failure policy, per-session cancellation instead of the process-global flag, and transport/authentication design. No server, RPC framework, or async runtime is introduced here.

### Terminal Markdown options

Basic completed-message rendering is low complexity; correct incremental rendering is moderate complexity. Markdown delimiters and code fences can span arbitrary model chunks, and later text can change the interpretation of earlier lines. Wrapping, tables, nested lists, resizing, and syntax highlighting add more work.

- `termimad` is a convenient ready-made terminal Markdown renderer (a supported Markdown subset, not full CommonMark). It is a small integration, but adds terminal/layout dependencies; check its supported syntax and transitive dependency cost before choosing it.
- `pulldown-cmark` is a lean CommonMark parser, not a terminal renderer. Pairing its events with our ANSI styles gives more control and avoids adopting a UI framework, but wrapping/layout remain our responsibility.

For a minimal first implementation, render completed answer blocks in the UI, or buffer complete lines/blocks with explicit code-fence state. Do not parse individual network chunks independently. Keep reasoning separate, preserve raw Markdown for transcripts/pipes, respect `NO_COLOR`, and escape untrusted terminal controls before adding our own styles. Plain text remains the current behavior; no Markdown dependency or renderer is added in this refactor.

`src/auth.rs` owns Codex credential discovery: explicit environment credentials, an agent-owned version-1 profile, then a read-only fx compatibility profile. It never writes a login, refreshes a token, or moves auth discovery into `llm-api`. A native OAuth flow can later replace the compatibility bridge without changing the transport API; an agent-owned access-token file alone does not implement login/refresh.

The direct dependencies besides `llm-api` are `serde_json`, `libc`, `unicode-segmentation`, and `unicode-width`. The Unicode helpers keep input deletion grapheme-aware and wrapped input rows within terminal cell boundaries. Serialization is already in the interface layer's dependency graph. `libc` provides Unix nonblocking pipes, process-group cleanup, and signal handling without an async runtime. The application is intentionally Unix-only for now; this does not add platform assumptions to the portable `llm-api` library.

## Next boundaries, not initial features

Cancellation now uses `llm-api::Client::run_cancellable` with the signal handler's atomic flag. The library owns per-request socket shutdown and its scoped monitor; Unix signal/input/process handling stays in this application. Input uses temporarily noncanonical, no-echo mode and nonblocking byte reads from a separately opened controlling terminal, with termios restored and the descriptor closed on scope exit. This avoids stdio prefetch, shared-descriptor flag changes, and the terminal-flush race between poll and read. `src/ui/input.rs` owns editing and explicit multiline redraw, showing trailing rows when input exceeds the screen height, instead of relying on canonical erase across emulator soft wraps or OS-limited canonical input buffers. Interactive interruption keeps complete transcript messages and settled tool results, records a warning about effects, clears the flag only after request cleanup, and returns to the prompt. Batch interruption exits 130. No automatic side-effect retry or rollback is attempted.

A fully responsive editing/steering UI remains deferred. DNS/connect cancellation is bounded rather than immediate; arbitrary callbacks and filesystem operations cannot be forcibly interrupted, and detached shell jobs may escape process-group cleanup.

Other deferred work: transcript persistence with explicit no-replay semantics for effects; prompt revisions/forks; model-aware context budgeting and explicit compaction; hierarchical instruction enforcement; stronger path traversal using anchored descriptors; and external sandboxing. None is implied by the current approval prompt or path checks.
