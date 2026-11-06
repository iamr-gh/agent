# End-to-end verification

Date: 2026-11-06. Tested the release `agent` binary against the current sibling `llm-api` working tree. No production source or dependency source changes were required. This is a small smoke matrix, not a coding benchmark or a reliability/security certification.

## Live results

| Provider | Model | Read | Denied write | Approved file workflow | Rust bug fix |
| --- | --- | --- | --- | --- | --- |
| OpenAI | `gpt-5.4-nano` | PASS | PASS | PASS | PASS |
| Anthropic | `claude-haiku-5-5` | PASS | PASS | PASS | Not run |
| OpenRouter | `nvidia/nemotron-3.5-super-vl:free` | PASS | PASS | FAIL: missing trailing newline | Not run |
| Codex subscription | `gpt-6.1-sol` | PASS | PASS | PASS | PASS |

13 of 14 scenarios passed. Every passing test command and its agent child exited 0. The OpenRouter approved-workflow runner exited 1 (`wrong edit result`), although the agent exited 0. Do not interpret this matrix as 14 successes.

### What was exercised

- **read:** the model used `read_file` to discover an unpredictable synthetic marker, returned it, and left the temporary workspace unchanged.
- **deny:** with stdin closed and without `--yes`, a requested `write_file` was rejected. The model acknowledged denial and no file was created.
- **approve:** actual terminal `y` responses approved creation of `smoke.txt`, an exact replacement, and a shell check. The runner checked final bytes equal `beta\n`, the original fixture remained unchanged, and no unexpected files existed. No unrestricted `--yes` was used.
- **code:** a standalone Rust `add` implementation initially subtracted; its two unchanged tests failed (exit 101). Each tested model read the source, ran `rustc --test calc.rs -o calc_tests && ./calc_tests`, made the small `-` to `+` correction, and reran the tests (exit 0, two passed). The runner checked the exact source change and independently recompiled/retested, rather than trusting the model's answer or test binary.

### Preserved failure

OpenRouter supplied `{"path":"smoke.txt","content":"alpha"}` rather than the requested trailing-newline content. The edit therefore produced `beta`, not `beta\n`. Its shell check passed because `$(cat smoke.txt)` strips trailing newlines and cannot distinguish these files. The model incorrectly claimed it had written the newline; the runner's independent byte check caught it.

This is observed model instruction-following/verification failure, not evidence of a transport or filesystem corruption bug. The test was not weakened or rerun until green. A successful shell command or final answer is insufficient evidence of exact file contents.

Codex also attempted an absent file read while following project-instruction guidance and recovered from the ordinary error result. Its final fixtures and tests passed.

## Reproduction

From `agent`:

```sh
cargo build --release --locked --offline
python3 -u tests/live.py --provider openai --model gpt-5.4-nano --scenario read
python3 -u tests/live.py --provider openai --model gpt-5.4-nano --scenario deny
python3 -u tests/live.py --provider openai --model gpt-5.4-nano --scenario approve
python3 -u tests/live.py --provider openai --model gpt-5.4-nano --scenario code
python3 -u tests/live.py --provider anthropic --model claude-haiku-5-5 --scenario read
python3 -u tests/live.py --provider anthropic --model claude-haiku-5-5 --scenario deny
python3 -u tests/live.py --provider anthropic --model claude-haiku-5-5 --scenario approve
python3 -u tests/live.py --provider openrouter --model nvidia/nemotron-3.5-super-vl:free --scenario read
python3 -u tests/live.py --provider openrouter --model nvidia/nemotron-3.5-super-vl:free --scenario deny
python3 -u tests/live.py --provider openrouter --model nvidia/nemotron-3.5-super-vl:free --scenario approve
python3 -u tests/live.py --provider codex --model gpt-6.1-sol --scenario read
python3 -u tests/live.py --provider codex --model gpt-6.1-sol --scenario deny
python3 -u tests/live.py --provider codex --model gpt-6.1-sol --scenario approve
python3 -u tests/live.py --provider codex --model gpt-6.1-sol --scenario code
```

`approve` and `code` require a real terminal. Inspect each proposal and approve only the temporary fixture operations. API-key modes load keys through the sibling test helper; Codex uses its read-only existing-login bridge and never prints or persists credentials. OpenRouter is restricted to a catalog-verified zero-priced `:free` model. OpenAI/Anthropic requests may incur charges; Codex consumes subscription quota. Agent-level usage/cost was not measured.

The code scenario is new in this verification pass; existing read/deny/approve assertions were preserved. All model work used disposable temporary directories, not this project's source tree.

## Deterministic checks

The following commands also exited 0:

```sh
# agent
cargo test --locked --offline
cargo fmt --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline
cargo build --release --locked --offline

# llm-api
cargo test --locked --offline
```

Original end-to-end pass: agent 12 tests passed; `llm-api` 30 tests passed. The new Python scenario was syntax-checked and exercised by the two successful real-provider coding runs. That initial pass did not cover long sessions, crash recovery, arbitrary projects, every model, or idle-network cancellation.

## Cancellation and direct-login follow-up

After adding cancellation and the application's Rust auth module, the same commands (`cargo test --locked --offline`, `cargo fmt --check`, `cargo clippy --locked --offline --all-targets -- -D warnings`, and `cargo build --release --locked --offline`) exited 0 in both repositories: **agent 23 tests; llm-api 36 tests**.

New loopback tests cover pre-cancelled calls, blocked headers/body/partial SSE/TLS handshake for every provider mode, cancellation after a delta, callback panic cleanup, concurrent request isolation, and successful client reuse. Subprocess/PTY tests cover real terminal Ctrl-C flushing input, cancelled approvals without writes, batch exit 130, idle-network cancellation followed by a successful turn, and a three-tool batch retaining its completed effect exactly once while cancelling a shell and preventing a pending mutation. All matching results and native replay are verified on continuation. Auth tests cover private read-only profiles, expiry/schema errors, unsafe permissions/symlinks, oversized files, and nonregular files including nonblocking FIFO rejection.

An earlier expanded test run stalled in macOS controlling-terminal shutdown and was explicitly terminated (exit 101 / runner SIGTERM). The test harness was corrected to drain terminal output while waiting; the subsequent full 23-test suite passed. This was a test cleanup issue, not a successful run.

A live Codex read scenario passed after the transport change. The new one-command login path was also exercised directly, without a Python auth wrapper:

```sh
cargo run --release --locked --offline -- --provider codex --model gpt-6.1-sol --max-turns 1 'Do not use tools or modify files. Reply with exactly CODEX_AUTH_OK.'
```

It exited 0 and printed `CODEX_AUTH_OK`. The ordinary interactive launch is `cargo run --release -- --provider codex --model gpt-6.1-sol`. No credential values are recorded here. Native OAuth login/refresh is still external; agent-owned credential files or environment variables avoid depending on fx, with a read-only fx login fallback for compatibility. DNS/connect cancellation remains bounded by five-second stage timeouts rather than immediate preemption. The previous OpenRouter newline failure remains preserved above.
