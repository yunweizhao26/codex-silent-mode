# Codex Silent Mode

`codex-silent` is a terminal client for [Codex app-server](https://developers.openai.com/codex/app-server/). It shows your questions and Codex's answers by default. Press **Ctrl+O** to show or hide command output, tool calls, and commentary.

Launch `codex-silent` and ask normally. You do not need to ask the agent to use a special tool. Codex still receives tool output through its normal backend flow; hiding activity changes only the display. Approvals and questions that need your response stay visible.

This is a separate terminal client, not a hook or plugin that changes the native `codex` interface. It uses your installed Codex binary and inherits its configuration, authentication, and permission policy. It does not grant extra permissions or automatically approve requests.

## Install

Supported platforms: Linux and macOS. Install and authenticate [Codex CLI](https://developers.openai.com/codex/cli/) on the same host first. Windows is not currently supported.

### From source

Install Rust 1.88 or newer and Cargo, then run:

```sh
cargo install --git https://github.com/yunweizhao26/codex-silent-mode.git --locked
codex-silent --check
codex-silent
```

The root package is `codex-silent` v0.2.0. Cargo installs it into its usual bin directory, normally `~/.cargo/bin`. The legacy MCP package under `plugins/` is not installed by this command.

### Prebuilt releases

Releases provide Linux x86_64 MUSL, macOS Apple Silicon, and macOS Intel binaries. These instructions require the corresponding release to have been published. Download the installer to a file and review it before running it:

```sh
curl --fail --location --proto '=https' --tlsv1.2 \
  https://raw.githubusercontent.com/yunweizhao26/codex-silent-mode/main/scripts/install.sh \
  --output codex-silent-install.sh
less codex-silent-install.sh
bash codex-silent-install.sh --version v0.2.0
"$HOME/.local/bin/codex-silent" --check
```

Omit `--version` to select the latest release. Use `--bin-dir /path/to/bin` to choose a destination; the default is `~/.local/bin`. The installer downloads the matching archive and `SHA256SUMS` from the same GitHub release and verifies the archive before installing it. It never runs the downloaded binary, overwrites `codex`, or modifies shell profiles. Add the destination to your `PATH` yourself if needed, or launch the binary by its full path.

The checksum detects a corrupt or mismatched download; it is not a separate signature from the release publisher. Linux ARM is currently a source-build option, not a prebuilt release target.

## Use

```sh
codex-silent
codex-silent --cwd /path/to/project
codex-silent --codex /path/to/codex --model MODEL_NAME
codex-silent --resume THREAD_ID
codex-silent --show
codex-silent -c 'model_reasoning_effort="high"'
```

| Key or command | Action |
| --- | --- |
| Ctrl+O | Show or hide activity |
| `/show`, `/hide`, `/toggle` | Set or toggle activity visibility |
| PgUp / PgDn | Scroll the conversation |
| Ctrl+Home / Ctrl+End | Jump to the beginning or end of the conversation |
| Home / End | Jump through the conversation when the input is empty; otherwise move the input cursor |
| Enter | Send the prompt or answer a visible request |
| Alt+Enter | Insert a newline |
| Ctrl+C | Interrupt the current turn, or quit when idle |
| `/quit` | Quit |
| `/resume THREAD_ID` | Resume a saved thread while idle |
| `/new` | Start a new thread while idle |
| `/logs` | Show the session log location |
| `/help` | Show help |

Activity remains available when hidden. Toggling does not restart a turn, remove backend history, change permissions, or alter what the agent receives. The client retains conversation content and scrolls it in its own terminal viewport.

`--codex PATH` selects the Codex executable. `--cwd PATH` selects the working directory. `--resume ID` resumes a thread at startup. `--show` starts with activity visible. `--model NAME` and repeatable `-c key=value` pass explicit configuration choices to the backend. With no overrides, existing backend configuration applies.

`codex-silent --check` checks the app-server handshake without opening the interactive screen or starting a model turn. The installed Codex version must support the app-server protocol used by this client. A successful handshake does not exercise every tool or approval flow. Requests that the client does not support are rejected with a visible notice; resume that thread in the native Codex CLI if necessary.

## SSH and HPC

Run both programs on the cluster or remote host:

```sh
ssh -t your-cluster
cd /path/to/project
codex-silent --check
codex-silent
```

Install and authenticate Codex on that host too. The client launches `codex app-server` over local standard input/output; it does not need a browser, desktop app, or listening network port. The Codex backend still needs its normal network access and must work under the host's execution policies. You can run the client inside `tmux` to keep the session available across SSH disconnects.

Shell aliases and functions are not executable backend commands. If your usual `codex` command is a shell wrapper, use `--codex /path/to/codex` to select the native binary or an executable wrapper script. When bypassing a wrapper, export any environment it normally sets, such as `CODEX_HOME`, and pass its required configuration overrides with `-c key=value`. The child process inherits exported environment variables; invoking the binary does not run your shell function. Prebuilt client installation does not require Cargo on the remote host.

## Logs and saved conversations

The client writes private JSONL event logs and backend stderr logs under `$XDG_STATE_HOME/codex-silent`, or `~/.local/state/codex-silent` when `XDG_STATE_HOME` is unset. Each session has its own directory containing `events.jsonl` and `stderr.log`. Set `--log-dir PATH` to choose a different parent directory, or use `/logs` to see the current location.

These logs retain received tool activity even while it is hidden. They can contain prompts, source code, command output, and other private content. Review them before sharing.

Open an event log without starting Codex:

```sh
codex-silent --replay /path/to/session/events.jsonl
```

Replay is read-only: scroll and toggle activity, or use `/quit` to exit. Use `--resume THREAD_ID` or `/resume THREAD_ID` to continue a saved backend conversation.

Display filtering does not discard backend tool content. Codex's own output limits, persistence, and context compaction still apply.

## Legacy MCP plugin

[`plugins/codex-command-quiet`](plugins/codex-command-quiet/README.md) contains the original `run_quiet` MCP tool. It discards child output before returning a result to the agent and requires that particular tool to be called. It cannot hide normal Codex tool activity while preserving the agent's access to the output. It remains available for legacy users, but is not the terminal client described here.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 tests/terminal_e2e.py
bash -n scripts/install.sh tests/install.sh
bash tests/install.sh

cargo fmt --manifest-path plugins/codex-command-quiet/Cargo.toml --check
cargo clippy --manifest-path plugins/codex-command-quiet/Cargo.toml --locked --all-targets -- -D warnings
cargo test --manifest-path plugins/codex-command-quiet/Cargo.toml --locked
```

CI runs the root client and legacy package checks on Linux and macOS. Installer tests use local fixtures and a mocked downloader. The PTY tests use a test backend rather than a live account.

Every push also builds the `codex-silent-linux-x86_64` Actions artifact for testing before a release. It contains the Linux MUSL archive and `SHA256SUMS`, and is retained for seven days. Download it from the matching Actions run, verify the archive with `sha256sum -c SHA256SUMS`, then extract the binary for remote testing. This job runs independently of the test matrix; check the other jobs before treating that commit as validated. Pull requests run the tests but do not publish this artifact.

Pushing a version tag such as `v0.2.0` runs the release workflow. The tag must match the root package version, and the root `Cargo.lock` must be committed. The workflow builds the three supported release binaries and publishes only their archives and `SHA256SUMS`. It does not build or release the legacy plugin.

MIT licensed. See [LICENSE](LICENSE).
