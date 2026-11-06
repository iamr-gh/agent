# Design notes

The starting reference is the local [harness taxonomy](../../harness_test/TAXONOMY.md) and all six [reviews](../../harness_test/reviews/): core harnesses; first-party agents; orchestrators; Jcode/OpenClaw/RVM/pi; commercial products; evidence audit. These are static architectural evidence, not performance rankings. In particular, unavailable commercial internals and RVM's registry lifecycle are not treated as implementations of a coding loop.

The implementation is original code, not copied source. Useful source comparisons from the pinned local snapshots:

- [pi's agent loop](../../harness_test/sources/pi/packages/agent/src/agent-loop.ts): keep conversation state distinct from individual requests; execute finalized calls and pair results with IDs.
- [OpenClaw's loop](../../harness_test/sources/openclaw/packages/agent-core/src/agent-loop.ts): every executed or failed tool must settle to a result. Its orchestration, recovery, steering, and plugin machinery are deliberately not reproduced.
- [`llm-api` request and response types](../../llm-api/src/lib.rs) and [protocol tests](../../llm-api/tests/protocols.rs): preserve `response.message` intact, including opaque reasoning replay; never execute argument deltas or truncated responses.

The reviews' broader lessons are reflected in the boundary choices: the first-party products separate instructions from authority, Aider demonstrates that editing is its own correctness problem, orchestrators solve a larger lifecycle problem, and the commercial review explicitly limits what can be concluded from public evidence. A small line count is a readability constraint, not evidence of benchmark superiority.

## Ownership

`llm-api` owns provider selection, encoding, streaming normalization, authentication headers, usage, finish reasons, and native replay. `agent` owns the conversation, prompt, tool schemas, argument checks, approval, filesystem effects, subprocesses, and terminal rendering. Current `llm-api` providers are OpenAI, Anthropic, OpenRouter, and Codex; its older README provider roster is not authoritative.

There is one sequential loop: user message → `Client::run` → intact assistant message → validated/authorized tools → user-role results → next request. Failed tools are observable results. Model/protocol/budget errors terminate rather than silently retrying effects. Output control characters are escaped before terminal rendering, but the model receives the raw bounded output.

The only direct dependencies besides `llm-api` are `serde_json` and `libc`. Serialization is already in the interface layer's dependency graph. `libc` provides Unix nonblocking pipes, process-group cleanup, and signal handling without an async runtime. The application is intentionally Unix-only for now; this does not add platform assumptions to the portable `llm-api` library.

## Next boundaries, not initial features

A genuinely responsive UI with cancellation during an idle provider read will need a portable cancellation/deadline facility in `llm-api`, or process isolation of model requests. Its current event callback alone does not provide that guarantee. This limitation is documented rather than patched into the concurrently edited dependency without coordination.

Other deferred work: transcript persistence with explicit no-replay semantics for effects; prompt revisions/forks; model-aware context budgeting and explicit compaction; hierarchical instruction enforcement; stronger path traversal using anchored descriptors; and external sandboxing. None is implied by the current approval prompt or path checks.
