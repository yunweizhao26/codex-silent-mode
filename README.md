# Codex Silent Mode

Codex Silent Mode is a public Codex CLI plugin that adds one MCP tool:
`run_quiet`. It runs a shell command with stdin, stdout, and stderr discarded,
then returns only the exit status.

The repository is distributed under the MIT license. The plugin runs locally,
so users need Codex CLI, Rust, Cargo, and a POSIX shell on macOS or Linux.

## Install from GitHub

Add this repository as a Codex marketplace and install the plugin:

```sh
codex plugin marketplace add yunweizhao26/codex-silent-mode
codex plugin add codex-command-quiet@codex-silent-mode
```

Start a new Codex session after installation. The CLI also exposes the plugin
browser with `/plugins`.

The first use builds the Rust MCP server and may download the locked Cargo
dependencies.

## Use it

Ask Codex explicitly:

> Use `run_quiet` to run `cargo test` in `/path/to/project`.

The tool accepts:

```json
{
  "command": "cargo test",
  "cwd": "/path/to/project",
  "timeout_ms": 120000
}
```

Successful output is only `ok: exit code 0`. A non-zero command returns its
exit code. Child stdout and stderr never enter the tool response.

## Direct MCP setup

If you do not want the marketplace flow, clone the repository and register the
server directly:

```sh
git clone https://github.com/yunweizhao26/codex-silent-mode.git
codex mcp add codex-command-quiet -- \
  /absolute/path/to/codex-silent-mode/plugins/codex-command-quiet/scripts/launch.sh
```

Start a new Codex session after changing MCP configuration.

## Important limits

This tool does not hide the MCP tool call or its command argument from the
host. It also cannot intercept native Codex `Bash` or `exec` cards. Commands
run with the permissions of the Codex process, so install and use the plugin
only when you trust the source and the commands being run.

This repository provides a public GitHub marketplace package. Inclusion in the
universal ChatGPT/Codex Plugins Directory is a separate submission process and
would require a hosted HTTPS MCP endpoint; this plugin intentionally runs as a
local Rust process.

## Development

```sh
cargo fmt --check --manifest-path plugins/codex-command-quiet/Cargo.toml
cargo test --manifest-path plugins/codex-command-quiet/Cargo.toml
cargo build --release --manifest-path plugins/codex-command-quiet/Cargo.toml
```
