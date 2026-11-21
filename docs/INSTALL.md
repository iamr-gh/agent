# Local installation

From the repository root, run:

```sh
./install.sh
```

This builds the current checkout in release mode using the lockfile and cached
Cargo dependencies, then symlinks `~/.local/bin/ag` to `target/release/ag`. Rust
and the sibling `../llm-api` checkout are required. If dependencies are not cached,
run `cargo fetch --locked` with network access first.

The script also refreshes existing `ag` symlinks on PATH and repairs legacy
`agent` symlinks that point to this checkout's release binary. It refuses to
replace regular files or directories. To install links in additional locations:

```sh
./install.sh /usr/local/bin/ag
```

Destination directories must be writable. Keep `~/.local/bin` on PATH; the script
checks that `ag` resolves to this build. Restart running agent processes after
rebuilding. The symlinks depend on this checkout staying in the same location.
