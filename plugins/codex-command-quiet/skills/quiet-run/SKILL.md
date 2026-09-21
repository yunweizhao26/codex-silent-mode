---
name: quiet-run
description: Run a shell command through the Rust MCP tool while discarding child output.
---

Use the `run_quiet` MCP tool when the user explicitly wants a command to run without stdout or stderr in the response.

Provide:

- `command`: the shell command to execute.
- `cwd`: an optional working directory.
- `timeout_ms`: an optional timeout from 1 to 600000 milliseconds.

The tool returns only success or failure status. Do not reproduce output that the tool discarded.

This plugin does not intercept native Codex `Bash` or `exec` calls. Use `run_quiet` explicitly for commands that need quiet execution.
