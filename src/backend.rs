//! Unfiltered JSON transport and ownership of a Codex app-server process.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const EOF_GRACE: Duration = Duration::from_millis(100);
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_millis(250);
const KILL_GRACE: Duration = Duration::from_secs(1);

pub struct Client {
    /// Every decoded message, including server requests, followed by an EOF/read error.
    /// Malformed lines produce errors without discarding subsequent messages.
    pub messages: mpsc::Receiver<Result<Value, String>>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<JoinHandle<()>>,
    stop_reader: Arc<AtomicBool>,
    next_id: u64,
}

impl Client {
    /// Starts the transport. The caller owns the initialize/initialized handshake.
    pub fn spawn(
        codex: &Path,
        config: &[String],
        cwd: &Path,
        stderr_path: &Path,
    ) -> io::Result<Self> {
        let mut log_options = OpenOptions::new();
        log_options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Refuse symlinks and avoid blocking on an accidentally supplied FIFO.
            log_options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let log = log_options.open(stderr_path)?;
        if !log.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Codex stderr must be redirected to a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // mode() only protects newly created files, so tighten existing logs too.
            log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }

        let mut command = Command::new(codex);
        command.args(["app-server", "--listen", "stdio://"]);
        for setting in config {
            command.arg("-c").arg(setting);
        }
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Could not start Codex executable '{}': {error}. Verify Codex is installed, \
                     its executable path is correct, and working directory '{}' exists.",
                    codex.display(),
                    cwd.display()
                ),
            )
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout was configured as piped");
        let (sender, messages) = mpsc::channel();
        // Own the child before any further fallible setup, so Drop also covers failures.
        let mut client = Self {
            messages,
            child: Some(child),
            stdin,
            reader: None,
            stop_reader: Arc::new(AtomicBool::new(false)),
            next_id: 1,
        };
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = stdout.as_raw_fd();
            // Nonblocking reads let shutdown stop even with an incomplete JSON line.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(io::Error::last_os_error());
            }
        }
        let stop = Arc::clone(&client.stop_reader);
        client.reader = Some(
            thread::Builder::new()
                .name("codex-transport".into())
                .spawn(move || read_messages(stdout, sender, stop))?,
        );
        Ok(client)
    }

    pub fn request(&mut self, method: &str, params: Value) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Codex request IDs exhausted"))?;
        self.write_message(json!({"id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    /// Sends protocol notifications, including the initialized acknowledgement.
    pub fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.write_message(json!({"method": method, "params": params}))
    }

    pub fn reply(&mut self, id: Value, result: Value) -> io::Result<()> {
        self.write_message(json!({"id": id, "result": result}))
    }

    pub fn reply_error(&mut self, id: Value, message: &str) -> io::Result<()> {
        self.write_message(json!({"id": id, "error": {"code": -32000, "message": message}}))
    }

    fn write_message(&mut self, message: Value) -> io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Codex transport is shut down")
        })?;
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        stdin.write_all(&bytes)?;
        stdin.flush()
    }

    /// Closes stdin, then terminates the owned process group with bounded grace periods.
    pub fn shutdown(&mut self) {
        self.stdin.take();
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        {
            // Do not reap the leader before the final group signal: its unreaped PID
            // reserves the group ID even when the server exits ahead of its children.
            let group = i32::try_from(child.id()).expect("Unix PID fits in pid_t");
            thread::sleep(EOF_GRACE);
            if group > 1 {
                unsafe { libc::kill(-group, libc::SIGTERM) };
                thread::sleep(TERM_GRACE);
                unsafe { libc::kill(-group, libc::SIGKILL) };
            } else {
                let _ = child.kill();
            }
        }
        #[cfg(not(unix))]
        if !wait_for_exit(&mut child, EOF_GRACE) {
            let _ = child.kill();
        }
        if !wait_for_exit(&mut child, KILL_GRACE) {
            // A process stuck in the kernel must not hold up the terminal. Keep a
            // waiter so it is still reaped if/when the operating system releases it.
            let _ = thread::Builder::new()
                .name("codex-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        self.stop_reader.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            // Never let a slow parser or an inherited pipe extend the shutdown bound.
            let deadline = Instant::now() + EOF_GRACE;
            while !reader.is_finished() && Instant::now() < deadline {
                thread::sleep(POLL_INTERVAL);
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            _ => return false,
        }
    }
}

fn read_messages(
    stdout: ChildStdout,
    sender: mpsc::Sender<Result<Value, String>>,
    stop: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let parse = |bytes: &[u8]| {
        serde_json::from_slice(bytes).map_err(|error| {
            format!(
                "Invalid JSON from Codex app-server: {error}; line: {:?}",
                String::from_utf8_lossy(bytes)
            )
        })
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = sender.send(Err("Codex app-server transport shut down".into()));
            return;
        }
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => {
                // A prior nonblocking read may have saved a final unterminated line.
                if !line.is_empty() && sender.send(parse(&line)).is_err() {
                    return;
                }
                let _ = sender.send(Err("Codex app-server stdout closed (EOF)".into()));
                return;
            }
            Ok(_) => {
                if sender.send(parse(&line)).is_err() {
                    return;
                }
                line.clear();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                let _ = sender.send(Err(format!(
                    "Could not read Codex app-server stdout: {error}"
                )));
                return;
            }
        }
    }
}
