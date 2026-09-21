use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const PROTOCOL_VERSION: &str = "2024-11-05";

fn main() {
    if let Err(error) = run_server() {
        eprintln!("codex-command-quiet: {error}");
        std::process::exit(1);
    }
}

fn run_server() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let request = match serde_json::from_str::<Value>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stdout,
                    error_response(Value::Null, -32700, format!("invalid JSON: {error}")),
                )?;
                continue;
            }
        };

        if let Some(response) = handle_request(&request) {
            write_response(&mut stdout, response)?;
        }
    }

    Ok(())
}

fn write_response(stdout: &mut impl Write, response: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, &response)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn handle_request(request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str)?;

    // JSON-RPC notifications do not receive responses.
    let id = id?;
    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "instructions": "Use run_quiet only when child stdout and stderr should be discarded. The tool runs shell commands with the permissions of the Codex process.",
            "serverInfo": {
                "name": "codex-command-quiet",
                "version": "0.1.0"
            }
        }),
        "tools/list" => json!({
            "tools": [tool_definition()]
        }),
        "tools/call" => call_tool(request.get("params").unwrap_or(&Value::Null)),
        _ => return Some(error_response(id, -32601, "method not found".to_string())),
    };

    Some(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    }))
}

fn tool_definition() -> Value {
    json!({
        "name": "run_quiet",
        "description": "Run a shell command with stdin, stdout, and stderr discarded. Return only its exit status.",
        "annotations": {
            "readOnlyHint": false,
            "destructiveHint": true,
            "openWorldHint": true
        },
        "inputSchema": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory. Defaults to the MCP server working directory."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_MS,
                    "default": DEFAULT_TIMEOUT_MS,
                    "description": "Optional timeout in milliseconds."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }
    })
}

fn call_tool(params: &Value) -> Value {
    let Some(params) = params.as_object() else {
        return tool_error("tool parameters must be an object");
    };

    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return tool_error("tool name must be a string");
    };
    if name != "run_quiet" {
        return tool_error("unknown tool");
    }

    let Some(arguments) = params.get("arguments").and_then(Value::as_object) else {
        return tool_error("arguments must be an object");
    };

    let Some(command) = arguments.get("command").and_then(Value::as_str) else {
        return tool_error("command must be a string");
    };
    if command.trim().is_empty() {
        return tool_error("command must not be empty");
    }

    let timeout_ms = match parse_timeout(arguments.get("timeout_ms")) {
        Ok(timeout_ms) => timeout_ms,
        Err(message) => return tool_error(&message),
    };
    let cwd = arguments.get("cwd").and_then(Value::as_str);

    match run_command(command, cwd, Duration::from_millis(timeout_ms)) {
        Ok(outcome) if outcome.timed_out => tool_error("command timed out"),
        Ok(outcome) if outcome.exit_code == Some(0) => tool_success("ok: exit code 0"),
        Ok(outcome) => tool_error(&match outcome.exit_code {
            Some(code) => format!("failed: exit code {code}"),
            None => "failed: process ended without an exit code".to_string(),
        }),
        Err(_) => tool_error("failed to start command"),
    }
}

fn parse_timeout(value: Option<&Value>) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_TIMEOUT_MS);
    };
    let Some(timeout_ms) = value.as_u64() else {
        return Err("timeout_ms must be a positive integer".to_string());
    };
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        return Err(format!("timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"));
    }
    Ok(timeout_ms)
}

#[derive(Debug, PartialEq, Eq)]
struct RunOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
}

fn run_command(command: &str, cwd: Option<&str>, timeout: Duration) -> io::Result<RunOutcome> {
    let mut process = if cfg!(windows) {
        let mut process = Command::new("cmd");
        process.args(["/C", command]);
        process
    } else {
        let mut process = Command::new("sh");
        process.args(["-c", command]);
        process
    };

    process
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }

    let mut child = process.spawn()?;
    let started = Instant::now();

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(RunOutcome {
                exit_code: status.code(),
                timed_out: false,
            });
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(RunOutcome {
                exit_code: None,
                timed_out: true,
            });
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn tool_success(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": false
    })
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_tool_returns_status_without_child_output() {
        let command = if cfg!(windows) {
            "echo secret-output"
        } else {
            "printf secret-output"
        };
        let result = call_tool(&json!({
            "name": "run_quiet",
            "arguments": { "command": command }
        }));

        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "ok: exit code 0");
        assert!(!result.to_string().contains("secret-output"));
    }

    #[test]
    fn failed_command_returns_only_exit_status() {
        let command = if cfg!(windows) { "exit /B 7" } else { "exit 7" };
        let result = call_tool(&json!({
            "name": "run_quiet",
            "arguments": { "command": command }
        }));

        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "failed: exit code 7");
    }

    #[test]
    fn initialize_is_wrapped_in_json_rpc_response() {
        let response = handle_request(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        }))
        .expect("request should have a response");

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
    }
}
