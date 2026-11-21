#!/bin/sh
# Build this checkout and refresh its command links. Extra arguments are link paths.
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
binary="$repo/target/release/ag"
set -- "$HOME/.local/bin/ag" "$@"

# Refresh existing ag links on PATH, plus legacy agent links to this checkout.
remaining=${PATH:-}
while :; do
    directory=${remaining%%:*}
    directory=${directory:-.}
    if [ -L "$directory/ag" ]; then
        set -- "$@" "$directory/ag"
    fi
    if [ -L "$directory/agent" ]; then
        target=$(readlink "$directory/agent")
        case "$target" in
            "$repo/target/release/agent"|"$binary")
                set -- "$@" "$directory/agent"
                ;;
        esac
    fi
    case "$remaining" in
        *:*) remaining=${remaining#*:} ;;
        *) break ;;
    esac
done

# Check every destination before building or changing any links.
for destination do
    if [ ! -L "$destination" ] && [ -e "$destination" ]; then
        printf 'Refusing to replace non-symlink: %s\n' "$destination" >&2
        exit 1
    fi
done

cd "$repo"
cargo build --release --locked --offline
for destination do
    mkdir -p -- "$(dirname -- "$destination")"
    ln -sfn -- "$binary" "$destination"
    printf '%s -> %s\n' "$destination" "$binary"
done

"$HOME/.local/bin/ag" --help
resolved=$(command -v ag || :)
if [ -z "$resolved" ] || [ ! "$resolved" -ef "$binary" ]; then
    printf 'ag is not resolving to this build. Put %s first on PATH.\n' "$HOME/.local/bin" >&2
    exit 1
fi
printf 'ag on PATH: %s\n' "$resolved"
