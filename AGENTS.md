# Repository workflow

Keep changes small, preserve unrelated work, and inspect applicable instructions before editing.

## Documentation

The root `README.md` is human-written and human-maintained. Do not edit it or update it with each change. Structural or architectural updates may be documented in `docs/` instead.

## Verify and install

After code changes, run from the repository root:

```sh
cargo fmt --check
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --release --locked --offline
```

After a successful release build, refresh the local executable symlink:

```sh
mkdir -p "$HOME/.local/bin"
ln -sfn "$PWD/target/release/agent" "$HOME/.local/bin/agent"
"$HOME/.local/bin/agent" --help
command -v agent
ls -l "$HOME/.local/bin/agent"
```

Inspect the destination before relinking. If it is an existing non-symlink file or directory, ask before replacing it. Verify that `agent` on PATH resolves to the intended installation. A running agent process must be restarted to use the rebuilt executable.

## Commits and delivery

Commit completed, verified changes with descriptive messages explaining the behavior and relevant tests. Include only task-related changes. Push only when the user requests it; never force-push without explicit approval.

Report the commit, exact checks and outcomes, build/relink status, and any blockers. Do not claim a command succeeded without tool evidence.
