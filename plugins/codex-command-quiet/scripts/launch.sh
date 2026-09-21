#!/bin/sh
set -eu

plugin_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$plugin_dir/target/release/codex-command-quiet"
needs_build=false

if [ ! -x "$binary" ] || [ "$plugin_dir/src/main.rs" -nt "$binary" ] || [ "$plugin_dir/Cargo.toml" -nt "$binary" ]; then
    needs_build=true
fi

if [ -f "$plugin_dir/Cargo.lock" ] && [ "$plugin_dir/Cargo.lock" -nt "$binary" ]; then
    needs_build=true
fi

if [ "$needs_build" = true ]; then
    cargo build --quiet --release --manifest-path "$plugin_dir/Cargo.toml"
fi

exec "$binary" "$@"
