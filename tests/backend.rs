#![cfg(unix)]

use codex_silent::backend::Client;
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    executable: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(mode: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let executable =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transport.py");
        fs::write(directory.path().join("mode"), mode).unwrap();
        let log = directory.path().join("stderr.log");
        Self {
            directory,
            executable,
            log,
        }
    }

    fn spawn(&self, config: &[String]) -> Client {
        Client::spawn(&self.executable, config, self.directory.path(), &self.log).unwrap()
    }
}

fn receive(client: &Client) -> Result<Value, String> {
    client
        .messages
        .recv_timeout(Duration::from_secs(5))
        .expect("reader stalled")
}

fn assert_reaped(pid: i32) {
    let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    assert_eq!(result, -1, "server was not reaped");
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

fn assert_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if unsafe { libc::kill(pid, 0) } == -1 {
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
            return;
        }
        // A killed grandchild can remain a zombie until the system's reaper runs.
        let status = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&status.stdout)
            .trim_start()
            .starts_with('Z')
        {
            return;
        }
        assert!(Instant::now() < deadline, "process {pid} survived shutdown");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn handshake_roundtrip_and_server_requests_preserve_content() {
    let fixture = Fixture::new("roundtrip");
    let config = vec![
        "model=\"example model\"".into(),
        "features.test=true".into(),
    ];
    let mut client = fixture.spawn(&config);
    let started = receive(&client).unwrap();
    assert_eq!(
        started["params"]["args"],
        json!([
            "app-server",
            "--listen",
            "stdio://",
            "-c",
            config[0],
            "-c",
            config[1]
        ])
    );
    let reported_cwd = PathBuf::from(started["params"]["cwd"].as_str().unwrap());
    assert_eq!(
        reported_cwd.canonicalize().unwrap(),
        fixture.directory.path().canonicalize().unwrap()
    );

    let params = json!({"clientInfo":{"name":"transport-test","version":"1.0"}});
    let first = client.request("initialize", params.clone()).unwrap();
    let response = receive(&client).unwrap();
    assert_eq!(response["id"], first);
    assert_eq!(
        response["result"]["received"],
        json!({"id":first,"method":"initialize","params":params})
    );
    client.notify("initialized", json!({})).unwrap();
    assert_eq!(
        receive(&client).unwrap(),
        json!({"method":"fixture/initialized","params":{"method":"initialized","params":{}}})
    );
    let request = receive(&client).unwrap();
    assert_eq!(
        request,
        json!({
            "id":"approval-λ", "method":"item/commandExecution/requestApproval",
            "params":{"command":"printf 'hello\\n'", "unknown":{"keep":[null,false,19]}}
        })
    );
    let result = json!({"decision":"accept", "extra":{"exact":"\u{001b}[31m\nλ"}});
    client.reply(request["id"].clone(), result.clone()).unwrap();
    assert_eq!(
        receive(&client).unwrap()["params"],
        json!({"id":"approval-λ","result":result})
    );
    client
        .reply_error(json!(42), "Unsupported request: λ\nreason")
        .unwrap();
    assert_eq!(
        receive(&client).unwrap()["params"],
        json!({"id":42,"error":{"code":-32000,"message":"Unsupported request: λ\nreason"}})
    );
    let second = client
        .request("arbitrary/tool", json!({"nested":[null,"\n",true]}))
        .unwrap();
    assert_eq!(second, first + 1);
    assert_eq!(
        receive(&client).unwrap()["result"],
        json!({"id":second,"method":"arbitrary/tool","params":{"nested":[null,"\n",true]}})
    );
    client.shutdown();
}

#[test]
fn malformed_lines_are_reported_and_partial_messages_survive() {
    let fixture = Fixture::new("invalid");
    let mut client = fixture.spawn(&[]);
    for _ in 0..3 {
        assert!(receive(&client).unwrap_err().contains("Invalid JSON"));
    }
    assert_eq!(
        receive(&client).unwrap(),
        json!({"method":"item/commandExecution/outputDelta","params":{"delta":"raw\u{001b}[31m\nλ"}})
    );
    assert_eq!(
        receive(&client).unwrap(),
        json!({"method":"partial/event","params":{"intact":true}})
    );
    assert_eq!(
        receive(&client).unwrap(),
        json!({"id":99,"result":"unterminated"})
    );
    assert!(receive(&client).unwrap_err().contains("EOF"));
    assert_eq!(
        client.messages.recv_timeout(Duration::from_secs(1)),
        Err(RecvTimeoutError::Disconnected)
    );
    client.shutdown();
}

#[test]
fn exit_signals_eof_and_drop_reaps_the_server() {
    let fixture = Fixture::new("exit");
    let client = fixture.spawn(&[]);
    let pid = receive(&client).unwrap()["params"]["pid"].as_i64().unwrap() as i32;
    assert!(receive(&client).unwrap_err().contains("EOF"));
    drop(client);
    assert_reaped(pid);
}

#[test]
fn stdin_eof_allows_graceful_exit_and_shutdown_is_idempotent() {
    let fixture = Fixture::new("graceful");
    let mut client = fixture.spawn(&[]);
    let pid = receive(&client).unwrap()["params"]["pid"].as_i64().unwrap() as i32;
    client.shutdown();
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("graceful-exit")).unwrap(),
        "stdin EOF"
    );
    assert_reaped(pid);
    client.shutdown();
    assert_eq!(
        client
            .request("after/shutdown", Value::Null)
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        client.reply(Value::Null, Value::Null).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(client
        .messages
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .is_err());
    assert_eq!(
        client.messages.recv_timeout(Duration::from_secs(1)),
        Err(RecvTimeoutError::Disconnected)
    );
}

#[test]
fn shutdown_kills_stubborn_descendants_even_after_server_exit() {
    for mode in ["stubborn", "orphan"] {
        let fixture = Fixture::new(mode);
        let mut client = fixture.spawn(&[]);
        let ready = receive(&client).unwrap();
        let pid = ready["params"]["pid"].as_i64().unwrap() as i32;
        let child = ready["params"]["child"].as_i64().unwrap() as i32;
        // macOS can return ESRCH for an exited, unreaped leader.
        if mode == "stubborn" {
            assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        }
        assert_eq!(unsafe { libc::getpgid(child) }, pid);
        assert_ne!(unsafe { libc::getpgrp() }, pid);
        let start = Instant::now();
        if mode == "orphan" {
            drop(client);
        } else {
            client.shutdown();
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "shutdown was unbounded"
        );
        assert_reaped(pid);
        assert_gone(child);
    }
}

#[test]
fn stderr_is_a_private_file_and_never_a_protocol_message() {
    for existing in [false, true] {
        let fixture = Fixture::new("exit");
        if existing {
            fs::write(&fixture.log, "earlier log\n").unwrap();
            fs::set_permissions(&fixture.log, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let mut client = fixture.spawn(&[]);
        assert_eq!(receive(&client).unwrap()["method"], "fixture/exiting");
        assert!(receive(&client).unwrap_err().contains("EOF"));
        client.shutdown();
        let log = fs::read_to_string(&fixture.log).unwrap();
        assert!(log.contains("private backend diagnostic"));
        if existing {
            assert!(log.starts_with("earlier log\n"));
        }
        assert_eq!(
            fs::metadata(&fixture.log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn stderr_rejects_symlinks_and_non_regular_files() {
    let fixture = Fixture::new("exit");
    let target = fixture.directory.path().join("untouched");
    fs::write(&target, "keep").unwrap();
    symlink(&target, &fixture.log).unwrap();
    assert!(Client::spawn(
        &fixture.executable,
        &[],
        fixture.directory.path(),
        &fixture.log
    )
    .is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    let result = Client::spawn(
        &fixture.executable,
        &[],
        fixture.directory.path(),
        std::path::Path::new("/dev/null"),
    );
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::InvalidInput);
    assert!(!fixture.directory.path().join("server.pid").exists());
}

#[test]
fn missing_codex_has_an_actionable_io_error() {
    let fixture = Fixture::new("exit");
    let missing = fixture.directory.path().join("missing-codex");
    let error = Client::spawn(&missing, &[], fixture.directory.path(), &fixture.log)
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(error.to_string().contains("missing-codex"));
    assert!(error.to_string().contains("installed"));
}
