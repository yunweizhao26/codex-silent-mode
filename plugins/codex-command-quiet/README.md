# Codex Command Quiet

`codex-command-quiet` is a small Rust MCP plugin with one tool, `run_quiet`.
It runs a shell command with stdin, stdout, and stderr connected to null devices,
then returns only its exit status.

## Use

Call `run_quiet` explicitly:

```json
{
  "command": "cargo test",
  "cwd": "/path/to/project",
  "timeout_ms": 120000
}
```

The response is a short status such as `ok: exit code 0` or `failed: exit code 1`.
The command text is still part of the MCP tool call shown by the host.

## Limitation

Codex plugins cannot currently intercept or hide native `Bash`/`exec` command cards.
This plugin suppresses child process output only when the command is routed through
`run_quiet`.

## Development

The launcher builds the Rust server on first use and then runs the release binary.

```sh
cargo test --manifest-path Cargo.toml
cargo build --release --manifest-path Cargo.toml
```
