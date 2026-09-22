use codex_silent::{
    backend::Client,
    conversation::{error_text, string, Conversation},
    requests,
    screen::View,
};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::mpsc::TryRecvError,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HELP: &str = "Codex Silent — answers first, activity on demand

Usage: codex-silent [OPTIONS]

  --cwd PATH        Work in this directory (default: current directory)
  --codex PATH      Codex executable (default: codex from PATH)
  --resume ID       Resume an existing Codex thread
  --model NAME      Override the configured model
  -c KEY=VALUE      Forward a Codex config override (repeatable)
  --show            Start with activity expanded
  --check           Check the real Codex app-server handshake, without a model turn
  --replay PATH     View a saved events.jsonl without starting Codex
  --log-dir PATH    Parent directory for private session logs
  -h, --help        Show this help
  -V, --version     Show version

Ctrl+O toggles activity. PgUp/PgDn scroll. Ctrl+Home/Ctrl+End jump.
Enter sends; Alt+Enter inserts a newline. Ctrl+C interrupts, or exits when idle.
Commands: /show /hide /toggle /new /resume ID /logs /help /quit

Run this launcher on the HPC host after SSH login. The Codex engine keeps its
full tool output and existing permissions. No special prompt or MCP tool needed.";

#[derive(Default)]
struct Options {
    codex: PathBuf,
    cwd: PathBuf,
    config: Vec<String>,
    resume: Option<String>,
    model: Option<String>,
    show: bool,
    check: bool,
    replay: Option<PathBuf>,
    log_dir: Option<PathBuf>,
}

fn options() -> Result<Option<Options>, String> {
    let mut o = Options {
        codex: "codex".into(),
        cwd: env::current_dir().map_err(|e| e.to_string())?,
        ..Default::default()
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            println!("{HELP}");
            return Ok(None);
        }
        if arg == "-V" || arg == "--version" {
            println!("codex-silent {}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        }
        match arg.as_str() {
            "--show" => o.show = true,
            "--check" => o.check = true,
            "--codex" | "--cwd" | "--resume" | "--model" | "-c" | "--config" | "--replay"
            | "--log-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                match arg.as_str() {
                    "--codex" => o.codex = value.into(),
                    "--cwd" => o.cwd = value.into(),
                    "--resume" => o.resume = Some(value),
                    "--model" => o.model = Some(value),
                    "--replay" => o.replay = Some(value.into()),
                    "--log-dir" => o.log_dir = Some(value.into()),
                    _ => o.config.push(value),
                }
            }
            _ => return Err(format!("Unknown option: {arg}. Use --help.")),
        }
    }
    o.cwd = o
        .cwd
        .canonicalize()
        .map_err(|e| format!("Working directory: {e}"))?;
    if !o.cwd.is_dir() {
        return Err("--cwd must be a directory".into());
    }
    if o.codex.components().count() > 1 {
        o.codex = o
            .codex
            .canonicalize()
            .map_err(|e| format!("Codex executable: {e}"))?;
    }
    Ok(Some(o))
}

fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn log_directory(o: &Options) -> io::Result<PathBuf> {
    let base = o
        .log_dir
        .clone()
        .or_else(|| env::var_os("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("codex-silent")))
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state/codex-silent")))
        .ok_or_else(|| {
            io::Error::other("Set --log-dir or XDG_STATE_HOME to store session history")
        })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = base.join(format!("session-{now}-{}", std::process::id()));
    private_dir(&path)?;
    Ok(path)
}

fn initialize(client: &mut Client) -> io::Result<u64> {
    client.request("initialize", json!({
        "clientInfo":{"name":"codex_silent","title":"Codex Silent","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":true}
    }))
}

#[derive(Clone, Copy)]
enum Waiting {
    Initialize,
    Thread,
    Turn,
    Interrupt,
}

fn supported_request(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
    )
}

struct App {
    conversation: Conversation,
    view: View,
    input: String,
    cursor: usize,
    pending: VecDeque<Value>,
    draft: Option<(String, usize)>,
    finished_turn: Option<String>,
    waiting: HashMap<u64, Waiting>,
    thread: Option<String>,
    turn: Option<String>,
    starting_turn: bool,
    status: String,
    replay: bool,
    startup: Instant,
}

impl App {
    fn new(show: bool, replay: bool) -> Self {
        let mut view = View::default();
        view.expanded = show;
        Self {
            conversation: Conversation::default(),
            view,
            input: String::new(),
            cursor: 0,
            pending: VecDeque::new(),
            draft: None,
            finished_turn: None,
            waiting: HashMap::new(),
            thread: None,
            turn: None,
            starting_turn: false,
            status: if replay {
                "Replay".into()
            } else {
                "Connecting".into()
            },
            replay,
            startup: Instant::now(),
        }
    }

    fn busy(&self) -> bool {
        self.starting_turn || self.turn.is_some()
    }

    fn enrich_request(&self, message: &mut Value) {
        if let Some(details) = self.conversation.details(
            string(&message["params"]["turnId"]),
            string(&message["params"]["itemId"]),
        ) {
            message["params"]["itemDetails"] = json!(details);
        }
    }

    fn current_thread_event(&self, message: &Value) -> bool {
        match (
            self.thread.as_deref(),
            message["params"]["threadId"].as_str(),
        ) {
            (Some(current), Some(event_thread)) => current == event_thread,
            _ => true,
        }
    }

    fn replay_message(&mut self, mut message: Value) {
        if let Some(id) = message["result"]["thread"]["id"].as_str() {
            self.thread = Some(id.to_owned());
            self.conversation.restore(&message["result"]["thread"]);
        } else if message.get("method").is_some() && message.get("id").is_some() {
            let method = string(&message["method"]);
            if supported_request(method) {
                self.enrich_request(&mut message);
                self.conversation.notice(requests::describe(&message));
            } else {
                self.conversation.notice(format!("Unsupported request: {method}. It was rejected. Update codex-silent or resume this thread in Codex."));
            }
        } else if self.current_thread_event(&message) {
            self.conversation.event(&message);
        }
    }

    fn present_request(&mut self) {
        self.input.clear();
        self.cursor = 0;
        if let Some(request) = self.pending.front() {
            self.conversation.notice(requests::describe(request));
            self.view.end();
            self.status = "Your response is required".into();
        } else {
            if let Some((text, cursor)) = self.draft.take() {
                self.input = text;
                self.cursor = cursor;
            }
            self.status = if self.busy() { "Working" } else { "Ready" }.into();
        }
    }

    fn open_thread(
        &mut self,
        client: &mut Client,
        options: &Options,
        resume: Option<&str>,
    ) -> io::Result<()> {
        let mut params = json!({"cwd":options.cwd});
        if let Some(model) = &options.model {
            params["model"] = json!(model);
        }
        if let Some(thread) = resume {
            params["threadId"] = json!(thread);
        }
        let id = client.request(
            if resume.is_some() {
                "thread/resume"
            } else {
                "thread/start"
            },
            params,
        )?;
        self.waiting.insert(id, Waiting::Thread);
        self.thread = None;
        self.status = "Opening conversation".into();
        self.startup = Instant::now();
        Ok(())
    }

    fn message(
        &mut self,
        mut message: Value,
        client: &mut Client,
        options: &Options,
    ) -> io::Result<()> {
        if message.get("method").is_some() && message.get("id").is_some() {
            let method = string(&message["method"]);
            if supported_request(method) {
                self.enrich_request(&mut message);
                let first = self.pending.is_empty();
                if first {
                    self.draft = Some((std::mem::take(&mut self.input), self.cursor));
                }
                self.pending.push_back(message);
                if first {
                    self.present_request();
                }
            } else {
                client.reply_error(
                    message["id"].clone(),
                    "Request is not supported by this version of codex-silent",
                )?;
                self.conversation.notice(format!("Unsupported request: {method}. It was rejected. Update codex-silent or resume this thread in Codex."));
            }
            return Ok(());
        }
        if let Some(id) = message["id"].as_u64() {
            let waiting = self.waiting.remove(&id);
            if let Some(error) = message.get("error") {
                self.conversation.notice(error_text(error));
                self.status = "Request failed".into();
                if matches!(waiting, Some(Waiting::Turn)) {
                    self.starting_turn = false;
                }
                return Ok(());
            }
            match waiting {
                Some(Waiting::Initialize) => {
                    // The server accepts requests after initialize. The notification acknowledges it.
                    client.notify("initialized", json!({}))?;
                    self.open_thread(client, options, options.resume.as_deref())?;
                }
                Some(Waiting::Thread) => {
                    let thread = &message["result"]["thread"];
                    let id = string(&thread["id"]);
                    if id.is_empty() {
                        return Err(io::Error::other("Codex returned a thread without an id"));
                    }
                    self.thread = Some(id.to_owned());
                    self.conversation.restore(thread);
                    self.status = "Ready".into();
                }
                Some(Waiting::Turn) => {
                    self.starting_turn = false;
                    let turn = &message["result"]["turn"];
                    if turn["status"] == "inProgress"
                        && self.finished_turn.as_deref() != turn["id"].as_str()
                    {
                        self.turn = Some(string(&turn["id"]).to_owned());
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        let p = &message["params"];
        // Ignore events belonging to another thread, e.g. detached subagents.
        if !self.current_thread_event(&message) {
            return Ok(());
        }
        self.conversation.event(&message);
        match string(&message["method"]) {
            "turn/started" => {
                self.turn = Some(string(&p["turn"]["id"]).to_owned());
                self.starting_turn = false;
                self.status = "Working · Ctrl+C interrupt".into();
            }
            "turn/completed" => {
                self.finished_turn = p["turn"]["id"].as_str().map(str::to_owned);
                self.turn = None;
                self.starting_turn = false;
                if !self.pending.is_empty() {
                    self.pending.clear();
                    self.present_request();
                }
                self.status = string(&p["turn"]["status"]).to_owned();
                if !p["turn"]["error"].is_null() {
                    self.conversation.notice(error_text(&p["turn"]["error"]));
                }
            }
            "serverRequest/resolved" => {
                let front_resolved = self
                    .pending
                    .front()
                    .is_some_and(|request| request["id"] == p["requestId"]);
                self.pending
                    .retain(|request| request["id"] != p["requestId"]);
                if front_resolved {
                    self.present_request();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn submit(
        &mut self,
        mut client: Option<&mut Client>,
        options: &Options,
        logs: &Path,
    ) -> io::Result<bool> {
        let input = self.input.trim().to_owned();
        if input.is_empty() {
            return Ok(false);
        }
        match input.as_str() {
            "/show" => self.view.expanded = true,
            "/hide" => self.view.expanded = false,
            "/toggle" => self.view.toggle(),
            "/logs" => self.conversation.notice(format!("Session logs: {}", logs.display())),
            "/help" => self.conversation.notice(HELP),
            "/quit" | "/exit" => return Ok(true),
            _ if self.replay => { self.conversation.notice("Replay is read-only. Use /show, /hide, or /quit."); }
            _ if !self.pending.is_empty() => {
                let request = self.pending.front().unwrap();
                match requests::answer(request, &input) {
                    Ok(result) => {
                        client.as_mut().unwrap().reply(request["id"].clone(), result)?;
                        self.pending.pop_front();
                        self.conversation.notice("Response sent.");
                        self.present_request();
                        return Ok(false);
                    }
                    Err(error) => { self.conversation.notice(error); return Ok(false); }
                }
            }
            "/new" if !self.busy() => {
                self.open_thread(client.as_mut().unwrap(), options, None)?;
                self.conversation.notice("Starting a new conversation. Earlier answers remain visible here.");
            }
            _ if input.starts_with("/resume ") && !self.busy() => {
                self.open_thread(client.as_mut().unwrap(), options, Some(input.trim_start_matches("/resume ").trim()))?;
            }
            _ if input.starts_with('/') => self.conversation.notice("Unknown or unavailable command. Use /help. Interrupt an active turn before /new or /resume."),
            _ if self.busy() => { self.conversation.notice("A turn is running. Ctrl+C interrupts it; your next message stays in the input box."); return Ok(false); }
            _ if self.thread.is_none() => { self.conversation.notice("Still connecting to Codex. Your message stays in the input box."); return Ok(false); }
            _ => {
                let id = client.as_mut().unwrap().request("turn/start", json!({"threadId":self.thread,"input":[{"type":"text","text":input}]}))?;
                self.waiting.insert(id, Waiting::Turn);
                self.starting_turn = true;
                self.status = "Working · Ctrl+C interrupt".into();
                self.view.end();
            }
        }
        self.input.clear();
        self.cursor = 0;
        Ok(false)
    }

    fn interrupt(&mut self, client: &mut Client) -> io::Result<()> {
        if let (Some(thread), Some(turn)) = (&self.thread, &self.turn) {
            let id = client.request("turn/interrupt", json!({"threadId":thread,"turnId":turn}))?;
            self.waiting.insert(id, Waiting::Interrupt);
            self.status = "Interrupting".into();
        } else {
            self.status = "Waiting for turn to start; Ctrl+Q exits".into();
        }
        Ok(())
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

fn disconnected(app: &mut App, client: &mut Client, logs: &Path, reason: &str) {
    client.shutdown();
    app.turn = None;
    app.starting_turn = false;
    app.pending.clear();
    app.draft = None;
    app.waiting.clear();
    app.status = "Disconnected · /quit to exit".into();
    app.conversation.notice(format!(
        "{reason}\nCodex stderr: {}\nUse /logs to locate this session or /quit to exit.",
        logs.join("stderr.log").display()
    ));
    app.view.end();
}

fn terminal(
    app: &mut App,
    mut client: Option<&mut Client>,
    options: &Options,
    logs: &Path,
    mut journal: Option<&mut File>,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("Interactive mode needs a terminal. Use ssh -t for remote execution, or --check for diagnostics."));
    }
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut connected = client.is_some();
    let mut redraw = true;
    loop {
        if connected {
            let transport = client.as_mut().unwrap();
            // Bound each batch so even a noisy tool cannot starve keyboard events.
            for _ in 0..256 {
                match transport.messages.try_recv() {
                    Ok(Ok(message)) => {
                        redraw = true;
                        if let Some(log) = journal.as_mut() {
                            serde_json::to_writer(&mut **log, &message)?;
                            log.write_all(b"\n")?;
                        }
                        app.message(message, transport, options)?;
                    }
                    Ok(Err(error)) => {
                        redraw = true;
                        connected = false;
                        disconnected(app, transport, logs, &error);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        redraw = true;
                        connected = false;
                        disconnected(app, transport, logs, "Codex message channel closed.");
                        break;
                    }
                }
            }
            if connected && app.thread.is_none() && app.startup.elapsed() > Duration::from_secs(60)
            {
                redraw = true;
                app.conversation.notice(format!(
                    "Codex is still connecting. Check {}. Ctrl+Q exits.",
                    logs.join("stderr.log").display()
                ));
                app.startup = Instant::now();
            }
        }
        if redraw {
            let pending = app.pending.front().map(requests::describe);
            terminal.draw(|frame| {
                app.view.draw(
                    frame,
                    &app.conversation,
                    &app.status,
                    &app.input,
                    app.cursor,
                    pending.as_deref(),
                )
            })?;
            redraw = false;
        }
        if !event::poll(Duration::from_millis(40))? {
            continue;
        }
        redraw = true;
        match event::read()? {
            Event::Paste(text) => {
                app.input.insert_str(app.cursor, &text);
                app.cursor += text.len();
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('o') if ctrl => app.view.toggle(),
                    KeyCode::Char('q') if ctrl => break,
                    KeyCode::Char('c') if ctrl => {
                        if app.busy() && connected {
                            let transport = client.as_deref_mut().unwrap();
                            if let Err(error) = app.interrupt(transport) {
                                connected = false;
                                disconnected(app, transport, logs, &format!(
                                    "Could not send interrupt: {error}. Delivery is unknown; it will not be retried."
                                ));
                            }
                        } else {
                            break;
                        }
                    }
                    KeyCode::PageUp => app.view.scroll(-(app.view.height.max(1) as isize)),
                    KeyCode::PageDown => app.view.scroll(app.view.height.max(1) as isize),
                    KeyCode::Home if ctrl || app.input.is_empty() => app.view.home(),
                    KeyCode::End if ctrl || app.input.is_empty() => app.view.end(),
                    KeyCode::Home => app.cursor = 0,
                    KeyCode::End => app.cursor = app.input.len(),
                    KeyCode::Left => {
                        app.cursor = app.input[..app.cursor]
                            .char_indices()
                            .next_back()
                            .map(|(i, _)| i)
                            .unwrap_or(0)
                    }
                    KeyCode::Right => {
                        if let Some(c) = app.input[app.cursor..].chars().next() {
                            app.cursor += c.len_utf8();
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some((previous, _)) =
                            app.input[..app.cursor].char_indices().next_back()
                        {
                            app.input.drain(previous..app.cursor);
                            app.cursor = previous;
                        }
                    }
                    KeyCode::Delete => {
                        if let Some(c) = app.input[app.cursor..].chars().next() {
                            app.input.drain(app.cursor..app.cursor + c.len_utf8());
                        }
                    }
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                        app.input.insert(app.cursor, '\n');
                        app.cursor += 1;
                    }
                    KeyCode::Enter => {
                        let local_command = matches!(
                            app.input.trim(),
                            "" | "/show"
                                | "/hide"
                                | "/toggle"
                                | "/logs"
                                | "/help"
                                | "/quit"
                                | "/exit"
                        );
                        if !connected && !app.replay && !local_command {
                            app.conversation.notice(
                                "Codex is disconnected. Nothing was sent. Use /logs or /quit, then restart to open or resume a thread."
                            );
                            app.view.end();
                        } else {
                            match app.submit(client.as_deref_mut(), options, logs) {
                                Ok(true) => break,
                                Ok(false) => {}
                                Err(error) => {
                                    connected = false;
                                    disconnected(app, client.as_deref_mut().unwrap(), logs, &format!(
                                        "Could not send request: {error}. Delivery is unknown; it will not be retried."
                                    ));
                                }
                            }
                        }
                    }
                    KeyCode::Char(c) if !ctrl => {
                        app.input.insert(app.cursor, c);
                        app.cursor += c.len_utf8();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = options().map_err(io::Error::other)? else {
        return Ok(());
    };
    if let Some(path) = &options.replay {
        let mut app = App::new(options.show, true);
        for line in io::BufReader::new(File::open(path)?).lines() {
            let value: Value = serde_json::from_str(&line?)?;
            app.replay_message(value);
        }
        terminal(
            &mut app,
            None,
            &options,
            path.parent().unwrap_or(Path::new(".")),
            None,
        )?;
        return Ok(());
    }
    let logs = log_directory(&options)?;
    let mut journal = private_file(&logs.join("events.jsonl"))?;
    let mut client = Client::spawn(
        &options.codex,
        &options.config,
        &options.cwd,
        &logs.join("stderr.log"),
    )?;
    let id = initialize(&mut client)?;
    if options.check {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let value = client
                .messages
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|e| {
                    io::Error::other(format!(
                        "Codex did not complete initialization: {e}. Check {}",
                        logs.join("stderr.log").display()
                    ))
                })?
                .map_err(|e| {
                    io::Error::other(format!("{e}. Check {}", logs.join("stderr.log").display()))
                })?;
            if value["id"] == id {
                if let Some(error) = value.get("error") {
                    return Err(io::Error::other(error_text(error)).into());
                }
                client.notify("initialized", json!({}))?;
                println!("Codex app-server handshake passed.\nBackend: {}\nWorking directory: {}\nLogs: {}", options.codex.display(), options.cwd.display(), logs.display());
                return Ok(());
            }
        }
    }
    let mut app = App::new(options.show, false);
    app.waiting.insert(id, Waiting::Initialize);
    app.conversation
        .notice("Tool activity is hidden. Ctrl+O shows or hides it. /help lists commands.");
    let result = terminal(
        &mut app,
        Some(&mut client),
        &options,
        &logs,
        Some(&mut journal),
    );
    client.shutdown();
    if let Some(thread) = app.thread {
        println!("Resume: codex-silent --resume {thread}");
    }
    println!("Session logs: {}", logs.display());
    result?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("codex-silent: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_options(directory: &Path) -> Options {
        Options {
            codex: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transport.py"),
            cwd: directory.to_owned(),
            ..Options::default()
        }
    }

    fn fixture() -> (TempDir, Options, Client, App) {
        let directory = tempfile::tempdir().unwrap();
        let options = test_options(directory.path());
        fs::write(directory.path().join("mode"), "roundtrip").unwrap();
        let client = Client::spawn(
            &options.codex,
            &[],
            &options.cwd,
            &directory.path().join("stderr.log"),
        )
        .unwrap();
        assert_eq!(receive(&client)["method"], "fixture/started");
        let mut app = App::new(false, false);
        app.thread = Some("thread-test".into());
        (directory, options, client, app)
    }

    fn receive(client: &Client) -> Value {
        client
            .messages
            .recv_timeout(Duration::from_secs(5))
            .expect("fake backend did not respond")
            .expect("fake backend transport failed")
    }

    fn approval(id: &str, command: &str) -> Value {
        json!({
            "id": id,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": "thread-test", "turnId": "turn-test",
                "itemId": id, "startedAtMs": 0,
                "command": command, "cwd": "/work",
                "availableDecisions": ["accept", "decline", "cancel"]
            }
        })
    }

    fn resolved(id: &str) -> Value {
        json!({
            "method": "serverRequest/resolved",
            "params": {"threadId": "thread-test", "requestId": id}
        })
    }

    fn assert_no_replies(client: &mut Client) {
        // A round trip orders all previous pipe writes, avoiding timing-based
        // assertions that could miss an approval still being processed.
        let id = client.request("fixture/barrier", json!({})).unwrap();
        assert_eq!(
            receive(client),
            json!({"id": id, "result": {
                "id": id, "method": "fixture/barrier", "params": {}
            }})
        );
    }

    fn assert_reply(client: &Client, id: &str, decision: &str) {
        assert_eq!(
            receive(client),
            json!({"method": "fixture/reply", "params": {
                "id": id, "result": {"decision": decision}
            }})
        );
    }

    #[test]
    fn approval_enrichment_keeps_command_context_without_revealing_output() {
        let (directory, options, mut client, mut app) = fixture();
        app.message(
            json!({"method":"item/started", "params":{
                "threadId":"thread-test", "turnId":"turn-test", "item":{
                    "id":"cmd", "type":"commandExecution", "status":"inProgress",
                    "command":"printf APPROVAL_COMMAND", "cwd":"/safe/cwd",
                    "aggregatedOutput":"PRIVATE_AGGREGATED_OUTPUT"
                }
            }}),
            &mut client,
            &options,
        )
        .unwrap();
        app.message(
            json!({"method":"item/commandExecution/outputDelta", "params":{
                "threadId":"thread-test", "turnId":"turn-test", "itemId":"cmd",
                "delta":"PRIVATE_STREAMED_OUTPUT"
            }}),
            &mut client,
            &options,
        )
        .unwrap();
        let mut request = approval("first", "unused");
        request["params"]["itemId"] = json!("cmd");
        request["params"].as_object_mut().unwrap().remove("command");
        app.message(request, &mut client, &options).unwrap();
        let visible = app
            .conversation
            .visible(false)
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("APPROVAL_COMMAND"));
        assert!(visible.contains("/safe/cwd"));
        assert!(!visible.contains("PRIVATE_AGGREGATED_OUTPUT"));
        assert!(!visible.contains("PRIVATE_STREAMED_OUTPUT"));
        assert!(app
            .conversation
            .visible(true)
            .any(|entry| entry.text.contains("PRIVATE_AGGREGATED_OUTPUT")
                && entry.text.contains("PRIVATE_STREAMED_OUTPUT")));
        assert_no_replies(&mut client);
        app.input = "yes".into();
        app.cursor = 3;
        app.submit(Some(&mut client), &options, directory.path())
            .unwrap();
        assert_reply(&client, "first", "accept");
    }

    #[test]
    fn queued_approvals_answer_only_the_displayed_front_and_restore_prompt() {
        let (directory, options, mut client, mut app) = fixture();
        app.turn = Some("turn-test".into());
        let draft = "Follow up on λ next.\nKeep this detail.";
        let cursor = "Follow up on λ".len();
        app.input = draft.into();
        app.cursor = cursor;
        app.message(
            approval("first", "FIRST_VISIBLE_COMMAND"),
            &mut client,
            &options,
        )
        .unwrap();
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
        assert!(app
            .conversation
            .entries
            .last()
            .unwrap()
            .text
            .contains("FIRST_VISIBLE_COMMAND"));

        app.input = "yes".into();
        app.cursor = 3;
        let displayed = app.conversation.entries.len();
        app.message(
            approval("second", "SECOND_QUEUED_COMMAND"),
            &mut client,
            &options,
        )
        .unwrap();
        assert_eq!(app.pending.len(), 2);
        assert_eq!(app.pending.front().unwrap()["id"], "first");
        assert_eq!(app.conversation.entries.len(), displayed);
        assert!(!app
            .conversation
            .entries
            .iter()
            .any(|entry| entry.text.contains("SECOND_QUEUED_COMMAND")));
        assert_eq!(app.input, "yes");
        assert_eq!(app.cursor, 3);
        assert_no_replies(&mut client);

        assert!(!app
            .submit(Some(&mut client), &options, directory.path())
            .unwrap());
        assert_reply(&client, "first", "accept");
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending.front().unwrap()["id"], "second");
        assert!(app
            .conversation
            .entries
            .last()
            .unwrap()
            .text
            .contains("SECOND_QUEUED_COMMAND"));
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
        assert_no_replies(&mut client);

        app.input = "no".into();
        app.cursor = 2;
        assert!(!app
            .submit(Some(&mut client), &options, directory.path())
            .unwrap());
        assert_reply(&client, "second", "decline");
        assert!(app.pending.is_empty());
        assert_eq!(app.input, draft);
        assert_eq!(app.cursor, cursor);
        assert_no_replies(&mut client);
    }

    #[test]
    fn resolving_front_clears_stale_approval_before_presenting_next() {
        let (directory, options, mut client, mut app) = fixture();
        app.turn = Some("turn-test".into());
        app.input = "Saved follow-up".into();
        app.cursor = 5;
        app.message(approval("first", "FIRST_COMMAND"), &mut client, &options)
            .unwrap();
        app.message(approval("second", "SECOND_COMMAND"), &mut client, &options)
            .unwrap();
        app.input = "yes".into();
        app.cursor = 3;

        app.message(resolved("first"), &mut client, &options)
            .unwrap();
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending.front().unwrap()["id"], "second");
        assert!(app
            .conversation
            .entries
            .last()
            .unwrap()
            .text
            .contains("SECOND_COMMAND"));
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
        assert!(!app
            .submit(Some(&mut client), &options, directory.path())
            .unwrap());
        assert_eq!(app.pending.len(), 1);
        assert_no_replies(&mut client);

        app.input = "no".into();
        app.cursor = 2;
        app.message(resolved("second"), &mut client, &options)
            .unwrap();
        assert!(app.pending.is_empty());
        assert_eq!(app.input, "Saved follow-up");
        assert_eq!(app.cursor, 5);
        assert_no_replies(&mut client);
    }

    #[test]
    fn resolving_queued_request_preserves_active_approval_input() {
        let (_directory, options, mut client, mut app) = fixture();
        app.turn = Some("turn-test".into());
        app.message(approval("first", "FIRST_COMMAND"), &mut client, &options)
            .unwrap();
        app.message(approval("second", "SECOND_COMMAND"), &mut client, &options)
            .unwrap();
        app.input = "yes".into();
        app.cursor = 2;
        let displayed = app.conversation.entries.len();

        app.message(resolved("second"), &mut client, &options)
            .unwrap();
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending.front().unwrap()["id"], "first");
        assert_eq!(app.conversation.entries.len(), displayed);
        assert_eq!(app.input, "yes");
        assert_eq!(app.cursor, 2);
        assert_no_replies(&mut client);
    }

    #[test]
    fn late_turn_start_reply_cannot_resurrect_a_completed_turn() {
        let (directory, options, mut client, mut app) = fixture();
        app.input = "First prompt".into();
        app.cursor = app.input.len();
        assert!(!app
            .submit(Some(&mut client), &options, directory.path())
            .unwrap());
        assert!(app.busy());
        let echoed = receive(&client);
        assert_eq!(echoed["result"]["method"], "turn/start");
        let request_id = echoed["id"].as_u64().unwrap();

        app.message(
            json!({"method": "turn/started", "params": {
                "threadId": "thread-test", "turn": {"id": "first-turn", "status": "inProgress"}
            }}),
            &mut client,
            &options,
        )
        .unwrap();
        assert!(app.busy());
        app.message(
            json!({"method": "turn/completed", "params": {
                "threadId": "thread-test", "turn": {"id": "first-turn", "status": "completed"}
            }}),
            &mut client,
            &options,
        )
        .unwrap();
        assert!(!app.busy());

        app.message(
            json!({"id": request_id, "result": {
                "turn": {"id": "first-turn", "status": "inProgress"}
            }}),
            &mut client,
            &options,
        )
        .unwrap();
        assert!(!app.busy());
        assert!(app.turn.is_none());
        assert!(!app.waiting.contains_key(&request_id));

        // The completion guard must still allow a subsequent turn to start.
        app.input = "Next prompt".into();
        app.cursor = app.input.len();
        assert!(!app
            .submit(Some(&mut client), &options, directory.path())
            .unwrap());
        let echoed = receive(&client);
        assert_eq!(echoed["result"]["method"], "turn/start");
        assert_eq!(
            echoed["result"]["params"]["input"][0]["text"],
            "Next prompt"
        );
        app.message(
            json!({"id": echoed["id"], "result": {
                "turn": {"id": "next-turn", "status": "inProgress"}
            }}),
            &mut client,
            &options,
        )
        .unwrap();
        assert!(app.busy());
        assert_eq!(app.turn.as_deref(), Some("next-turn"));
    }
}
