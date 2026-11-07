# Verification and live testing

This is a guide to the current checks, not a record of a fresh live-provider run. Historical provider matrices and intermediate test counts have been removed: they were tied to older approval defaults, output formatting, and dependency versions, and do not establish current compatibility.

See [design](DESIGN.md) for execution boundaries and [saved sessions](SESSIONS.md) for persistence behavior. All commands below run from the repository root unless stated otherwise. Documentation links stay within this folder.

## Offline checks

```sh
cargo fmt --check
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --release --locked --offline
python3 tests/sessions.py
```

Cargo requires the dependencies declared in the manifest, including the local `llm-api` checkout, and an already populated cache for offline operation. Rust tooling and Python 3 are required; terminal tests use Unix controlling PTYs. These are application checks, not a separately verified run of the dependency's own suite.

The Rust tests cover:

- Tool argument validation, workspace path containment, atomic writes, file limits, output bounds, and exact replacement behavior.
- Approval denials, provider errors, model/context budgets, and intact replay/tool-result ordering.
- Read-only credential validation and skill discovery/loading.
- Session round trips, private permissions, schema/workspace/version rejection, writer locks, interrupted-tool recovery, and failed checkpoints.
- Terminal cursor editing, Unicode graphemes, wrapped redraw, input history, response typeahead, session-picker filtering, and Markdown/plain-output rendering.
- Process/PTY cancellation, stalled loopback requests, batch exit 130, shell process-group cleanup, and successful interactive continuation.

`tests/sessions.py` uses a disposable private agent home and synthetic credentials, without model requests. It checks session clearing, picker filtering/navigation/cancellation, locked selections, direct resume, exact context preservation, and reload after executable replacement, including a failed reload retaining the writer lock. It accepts an optional binary path; the default is `target/release/agent`.

These checks do not prove model instruction following, production-provider availability, sandbox isolation, exactly-once effects, or recovery of arbitrary external processes. Record the actual commands, outcomes, and environment for a verification run rather than copying an old test count.

## Existing live runner: limitations

`tests/live.py` contains opt-in `read`, `deny`, `approve`, and `code` scenarios in disposable workspaces. It is **not currently a self-contained or up-to-date acceptance runner**:

- It imports credential and model-catalog helpers from a separate checkout; those helpers are not bundled here. The offline checks above do not require those Python helpers.
- Its child command does not pass `--ask`. The application now automatically allows tools by default, so the runner's denial and interactive-approval assumptions no longer hold. Do not rely on it to enforce approval.
- Its read assertion expects line-numbered file contents in stderr, while the default frontend now emits a compact read summary rather than the full result.
- It does not isolate `AGENT_HOME`, so saved conversations can be written to the normal session directory even though the workspace itself is temporary.

The runner needs those issues addressed before its scenarios can be treated as current acceptance checks. No credentials, external helper implementations, or third-party source snapshots are copied into this documentation. The relevant behavior and limitations are explained here instead of requiring external citations.

## Manual live smoke checks

Live checks are optional. They use real credentials, may incur API charges or consume subscription quota, and execute tools with the user's full privileges. Do not run them automatically as part of offline verification.

For a manual check:

1. Build the release executable and use its absolute path when launching from a disposable workspace.
2. Use a disposable `AGENT_HOME` to keep test sessions separate. Configure credentials through the application's supported environment variables or a private test profile; never put credential values in recorded commands or output. A disposable home does not disable the compatibility credential fallback described in [design](DESIGN.md).
3. Pass **`--ask` explicitly**, and use `--verbose` when inspecting complete bounded tool results. For a nonterminal denial check, close stdin while keeping `--ask`; do not substitute `--yes`.
4. Select the provider and model explicitly. A provider name or a `:free` suffix alone does not establish current availability or price.
5. Exercise a small scenario and independently check the resulting workspace, not just the assistant's answer or shell exit status.

Useful scenarios:

| Scenario | Independent assertion |
| --- | --- |
| Read an unpredictable fixture marker using only `read_file` | Final answer contains the marker; no workspace files changed |
| Deny a proposed write under `--ask` | Denial is observable and the destination does not exist |
| Approve create/read/edit/check operations in a terminal | Final bytes match exactly; the original fixture and file inventory are unchanged except for intended effects |
| Fix a tiny Rust function with unchanged tests | Tests fail before the edit, pass afterward, and pass again when independently recompiled; source changes are limited to the intended fix |

A previous live run exposed a useful failure mode: a model omitted a requested trailing newline, and its shell check using `$(cat file)` still passed because command substitution strips trailing newlines. The independent byte comparison caught the error. Preserve exact-byte checks; neither model claims nor a successful shell command establishes exact file contents.

Report a live run's provider, model, selected scenario, approval mode, agent/runner exit statuses, and independent assertions. Do not include secrets or treat a small smoke pass as a coding benchmark or a reliability/security certification.
