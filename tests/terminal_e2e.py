#!/usr/bin/env python3
"""Offline PTY integration tests. Run: cargo build && python3 tests/terminal_e2e.py.

CODEX_SILENT_BIN may name another built binary. No Codex installation or model
access is used. The suite has a shared 50-second deadline, including failed waits.
"""

import codecs
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata
import unittest


ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("CODEX_SILENT_BIN", ROOT / "target/debug/codex-silent")).expanduser().resolve()
CTRL_O = b"\x0f"
CSI = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")


class Screen:
    """Only the VT operations emitted by the full-screen Crossterm renderer.

    Unknown operations fail instead of making screen assertions silently wrong.
    Raw bytes are checked separately so a transient disclosure cannot be erased.
    """

    def __init__(self, rows, cols):
        self.decoder = codecs.getincrementaldecoder("utf-8")("strict")
        self.pending = ""
        self.row = self.col = 0
        self.cells = []
        self.resize(rows, cols)

    def resize(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.cells = [(self.cells[r][:cols] if r < len(self.cells) else []) for r in range(rows)]
        for row in self.cells:
            row.extend([" "] * (cols - len(row)))
        self.row, self.col = min(self.row, rows - 1), min(self.col, cols - 1)

    @property
    def text(self):
        return "\n".join("".join(row) for row in self.cells)

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            ch = self.pending[0]
            if ch == "\x1b":
                match = CSI.match(self.pending)
                if not match:
                    if len(self.pending) == 1 or (self.pending.startswith("\x1b[") and
                                                not any("@" <= c <= "~" for c in self.pending[2:])):
                        return
                    raise AssertionError(f"Unsupported terminal escape: {self.pending[:60]!r}")
                self.pending = self.pending[match.end():]
                raw, intermediate, op = match.groups()
                if op in ("m", "h", "l") or (op == "q" and intermediate == " "):
                    if raw == "?1049" and op == "h":
                        self.cells = [[" "] * self.cols for _ in range(self.rows)]
                    continue
                values = [int(v or "0") for v in raw.split(";")]
                n = values[0] or 1
                if op in ("H", "f"):
                    self.row = n - 1
                    self.col = (values[1] or 1) - 1 if len(values) > 1 else 0
                elif op == "G":
                    self.col = n - 1
                elif op == "d":
                    self.row = n - 1
                elif op in "ABCD":
                    self.row += n * ((op == "B") - (op == "A"))
                    self.col += n * ((op == "C") - (op == "D"))
                elif op in ("J", "K"):
                    start, end = 0, self.rows * self.cols if op == "J" else self.cols
                    cursor = self.row * self.cols + self.col if op == "J" else self.col
                    if values[0] == 0:
                        start = cursor
                    elif values[0] == 1:
                        end = cursor + 1
                    elif values[0] not in (2, 3):
                        raise AssertionError(f"Unsupported erase: {match[0]!r}")
                    for index in range(start, end):
                        r, c = divmod(index, self.cols) if op == "J" else (self.row, index)
                        self.cells[r][c] = " "
                else:
                    raise AssertionError(f"Unsupported CSI: {match[0]!r}")
                self.row = max(0, min(self.row, self.rows - 1))
                self.col = max(0, min(self.col, self.cols - 1))
                continue
            self.pending = self.pending[1:]
            if ch == "\r":
                self.col = 0
            elif ch == "\n":
                self.row += 1
                if self.row >= self.rows:
                    self.cells.pop(0)
                    self.cells.append([" "] * self.cols)
                    self.row = self.rows - 1
            elif ch == "\b":
                self.col = max(0, self.col - 1)
            elif ch >= " ":
                if unicodedata.combining(ch):
                    self.cells[self.row][max(0, self.col - 1)] += ch
                    continue
                width = 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
                if self.col + width > self.cols:
                    self.row = min(self.row + 1, self.rows - 1)
                    self.col = 0
                self.cells[self.row][self.col] = ch
                if width == 2:
                    self.cells[self.row][self.col + 1] = ""
                self.col += width


class Terminal:
    def __init__(self, argv, cwd, env, rows=80, cols=110):
        self.master, self.slave = pty.openpty()
        self.screen = Screen(rows, cols)
        self.raw = bytearray()
        self.eof = False
        self.process = None
        try:
            self.resize(rows, cols)
            self.process = subprocess.Popen(
                argv, cwd=cwd, env=env, stdin=self.slave, stdout=self.slave, stderr=self.slave,
                start_new_session=True,
                preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
            )
        except BaseException:
            os.close(self.master)
            os.close(self.slave)
            raise

    def resize(self, rows, cols):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.screen.resize(rows, cols)
        if self.process and self.process.poll() is None:
            os.kill(self.process.pid, signal.SIGWINCH)

    def send(self, data):
        os.write(self.master, data.encode("utf-8") if isinstance(data, str) else data)

    def submit(self, text):
        # Bracketed paste submits Unicode atomically, followed by a real Enter.
        self.send("\x1b[200~" + text + "\x1b[201~\r")

    def pump(self, timeout=0.04):
        if self.eof or not select.select([self.master], [], [], timeout)[0]:
            return
        try:
            data = os.read(self.master, 65536)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            data = b""
        if not data:
            self.eof = True
            return
        self.raw.extend(data)
        self.screen.feed(data)

    def close(self):
        stop_process(self.process)
        os.close(self.master)
        os.close(self.slave)


def stop_process(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=1)


class TerminalE2E(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BIN.is_file() or not os.access(BIN, os.X_OK):
            raise AssertionError(f"Build the binary first: cargo build (missing executable {BIN})")
        cls.suite_deadline = time.monotonic() + 50

    def setUp(self):
        self.deadline = min(time.monotonic() + 8, self.suite_deadline)
        self.temp = tempfile.TemporaryDirectory(prefix="codex-silent-e2e-")
        self.addCleanup(self.temp.cleanup)
        self.cwd = Path(self.temp.name) / "work dir"
        self.cwd.mkdir()
        self.logs = Path(self.temp.name) / "logs"
        self.fake = Path(self.temp.name) / "fake-codex"
        shutil.copyfile(ROOT / "tests/fake_codex.py", self.fake)
        self.fake.chmod(0o700)
        self.env = {**os.environ, "TERM": "xterm-256color", "CODEX_SILENT_E2E": "1",
                    "PYTHONIOENCODING": "utf-8", "PYTHONDONTWRITEBYTECODE": "1",
                    "PATH": str(Path(sys.executable).parent) + os.pathsep + os.environ.get("PATH", "")}
        self.terminals = []
        self.processes = []
        self.addCleanup(self.cleanup_processes)

    def wire(self):
        path = self.cwd / "fixture-wire.jsonl"
        if not path.exists():
            return []
        # Ignore a trailing partial write while the fake is still running.
        return [json.loads(line) for line in path.read_text(encoding="utf-8").split("\n")[:-1]]

    def received(self, method=None):
        messages = [row["message"] for row in self.wire() if row["kind"] == "receive"]
        return messages if method is None else [m for m in messages if m.get("method") == method]

    def cleanup_processes(self):
        try:
            for terminal in self.terminals:
                terminal.close()
            for process in self.processes:
                stop_process(process)
        finally:
            # The backend owns a separate process group. Killing only the UI
            # would leave the fake behind after an assertion failure.
            for row in self.wire():
                if row["kind"] == "start" and row["pgid"] == row["pid"] and row["pgid"] > 1:
                    try:
                        os.killpg(row["pgid"], signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def argv(self, *extra):
        return [str(BIN), "--codex", str(self.fake), "--cwd", str(self.cwd),
                "--log-dir", str(self.logs), *map(str, extra)]

    def start(self, *extra):
        terminal = Terminal(self.argv(*extra), self.cwd, self.env)
        self.terminals.append(terminal)
        self.until(terminal, lambda: "Replay" in terminal.screen.text if "--replay" in extra
                   else "Ready" in terminal.screen.text, "initial terminal frame")
        return terminal

    def until(self, terminal, predicate, label):
        deadline = min(time.monotonic() + 3, self.deadline)
        while time.monotonic() < deadline:
            terminal.pump()
            exited = terminal.process.poll() is not None
            if predicate():
                return
            if exited:
                break
        # The child may exit between the last poll and the deadline check.
        if predicate():
            return
        self.fail(f"Timed out waiting for {label}; exit={terminal.process.poll()}\n"
                  f"SCREEN:\n{terminal.screen.text}\nRAW TAIL: {bytes(terminal.raw[-1500:])!r}")

    def shows(self, terminal, *texts):
        self.until(terminal, lambda: all(text in terminal.screen.text for text in texts), repr(texts))

    def settle(self, terminal, seconds=0.16):
        deadline = min(time.monotonic() + seconds, self.deadline)
        while time.monotonic() < deadline:
            terminal.pump(max(0, min(0.04, deadline - time.monotonic())))

    def hidden(self, terminal, markers, since=0):
        for marker in markers:
            self.assertNotIn(marker, terminal.screen.text)
            self.assertNotIn(marker.encode(), bytes(terminal.raw[since:]))

    def quit(self, terminal):
        terminal.submit("/quit")
        self.until(terminal, lambda: terminal.process.poll() is not None, "/quit exit")
        while not terminal.eof and select.select([terminal.master], [], [], 0)[0]:
            terminal.pump(0)
        self.assertEqual(terminal.process.returncode, 0)
        self.assertIn(b"\x1b[?1049l", terminal.raw)
        self.assertIn(b"\x1b[?2004l", terminal.raw)
        self.assertIn(b"\x1b[?25h", terminal.raw)
        for row in self.wire():
            if row["kind"] == "start":
                with self.assertRaises(ProcessLookupError, msg="fake app-server survived /quit"):
                    os.kill(row["pid"], 0)

    def test_check_without_tty_forwards_config_and_never_starts_a_turn(self):
        config = ['approval_policy="on-request"', 'sandbox_mode="read-only"']
        process = subprocess.Popen(self.argv("--check", "-c", config[0], "-c", config[1]),
                                   cwd=self.cwd, env=self.env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        self.processes.append(process)
        stdout, stderr = process.communicate(timeout=max(0.01, min(5, self.deadline - time.monotonic())))
        self.assertEqual(process.returncode, 0, stderr.decode())
        self.assertIn(b"handshake passed", stdout)
        self.assertNotIn(b"STDERR_SECRET", stdout + stderr)
        self.assertEqual([m["method"] for m in self.received()], ["initialize", "initialized"])
        started = self.wire()[0]
        self.assertEqual(started["argv"], ["app-server", "--listen", "stdio://", "-c", config[0], "-c", config[1]])
        self.assertEqual(Path(started["cwd"]), self.cwd.resolve())
        sessions = list(self.logs.iterdir())
        self.assertEqual(len(sessions), 1)
        self.assertEqual(stat.S_IMODE(sessions[0].stat().st_mode), 0o700)
        for name in ("events.jsonl", "stderr.log"):
            self.assertEqual(stat.S_IMODE((sessions[0] / name).stat().st_mode), 0o600)

    def test_ctrl_o_reveals_then_erases_activity_and_hides_new_output(self):
        terminal = self.start()
        terminal.submit("first question")
        self.shows(terminal, "first question", "ANSWER_1", "completed")
        markers = ["TOOL_MARKER_1", "DELTA_SECRET_1", "MCP_SECRET_1", "COMMENTARY_SECRET_1"]
        self.hidden(terminal, markers + ["STDERR_SECRET"])
        terminal.send(CTRL_O)
        self.shows(terminal, "activity shown", *markers)
        terminal.send(CTRL_O)
        self.shows(terminal, "answers only", "first question", "ANSWER_1")
        for marker in markers:
            self.assertNotIn(marker, terminal.screen.text)
        boundary = len(terminal.raw)
        terminal.submit("second question")
        self.shows(terminal, "ANSWER_2", "first question", "ANSWER_1", "second question")
        self.hidden(terminal, ["TOOL_MARKER_2", "DELTA_SECRET_2", "MCP_SECRET_2", "COMMENTARY_SECRET_2"], boundary)
        self.quit(terminal)

    def test_approval_stays_visible_and_requires_explicit_yes_enter(self):
        terminal = self.start()
        terminal.submit("approval please")
        self.shows(terminal, "Response required", "APPROVAL_VISIBLE", "APPROVAL_REQUIRED")
        markers = ["TOOL_MARKER_1", "DELTA_SECRET_1", "MCP_SECRET_1", "COMMENTARY_SECRET_1"]
        self.hidden(terminal, markers)
        replies = lambda: [m for m in self.received() if m.get("id") == "approval-1"]
        self.settle(terminal)
        self.assertEqual(replies(), [], "approval was answered without user input")
        terminal.send(b"\r")
        self.settle(terminal)
        self.assertEqual(replies(), [], "empty Enter must not approve")
        terminal.send(b"yes")
        self.settle(terminal)
        self.assertEqual(replies(), [], "typing yes without Enter must not approve")
        self.assertIn("Response required", terminal.screen.text)
        terminal.send(b"\r")
        self.shows(terminal, "APPROVAL_ACCEPTED", "completed")
        self.assertEqual(replies(), [{"id": "approval-1", "result": {"decision": "accept"}}])
        self.assertEqual(len(self.received("turn/start")), 1, "yes became a model prompt")
        self.hidden(terminal, markers)
        self.quit(terminal)

    def test_previous_answers_survive_live_noise_resize_and_interrupt(self):
        terminal = self.start()
        question = "Earlier question 中文 café"
        terminal.submit(question)
        self.shows(terminal, question, "ANSWER_1 中文 café")
        terminal.submit("noisy work")
        self.shows(terminal, "noisy work", "Working")
        terminal.resize(24, 64)
        self.shows(terminal, question, "ANSWER_1 中文 café")
        frames = 0
        deadline = min(time.monotonic() + 3, self.deadline)
        while "NOISE_DRAINED" not in terminal.screen.text and time.monotonic() < deadline:
            terminal.pump()
            self.assertIn(question, terminal.screen.text)
            self.assertIn("ANSWER_1 中文 café", terminal.screen.text)
            frames += 1
        self.assertIn("NOISE_DRAINED", terminal.screen.text)
        self.assertGreater(frames, 2, "noise must span multiple terminal frames")
        noise = [row for row in self.wire() if row["kind"] == "send"
                 and row["message"].get("params", {}).get("item", {}).get("id", "").startswith("noise-")]
        self.assertEqual(len(noise), 240)
        self.hidden(terminal, ["TOOL_MARKER_", "NOISE_SECRET_", "MCP_SECRET_", "COMMENTARY_SECRET_"])
        terminal.send(b"\x03")
        self.shows(terminal, "INTERRUPTED_ANSWER", "interrupted")
        self.assertIsNone(terminal.process.poll())
        self.assertEqual([m["params"] for m in self.received("turn/interrupt")],
                         [{"threadId": "thread-e2e", "turnId": "turn-2"}])
        self.assertEqual(self.received("turn/start")[0]["params"]["input"][0]["text"], question)
        terminal.submit("after interrupt")
        self.shows(terminal, "ANSWER_3", "completed")
        self.quit(terminal)

    def test_resume_restores_history_and_filters_it_like_live_events(self):
        terminal = self.start("--resume", "saved-thread")
        self.shows(terminal, "RESUMED_QUESTION 中文", "RESUMED_ANSWER 中文")
        markers = ["RESUME_TOOL_MARKER", "RESUME_MCP_SECRET", "RESUME_COMMENTARY_SECRET"]
        self.hidden(terminal, markers)
        self.assertEqual(self.received("thread/resume")[0]["params"]["threadId"], "saved-thread")
        self.assertEqual(self.received("thread/start"), [])
        terminal.submit("new question after resume")
        self.shows(terminal, "RESUMED_QUESTION", "RESUMED_ANSWER", "ANSWER_2")
        terminal.send(CTRL_O)
        self.shows(terminal, *markers)
        self.quit(terminal)

    def test_replay_filters_tool_requests_and_tracks_thread_switches(self):
        def item(thread, turn, value):
            return {"method": "item/completed", "params": {
                "threadId": thread, "turnId": turn, "item": value,
            }}

        events = [
            {"id": 2, "result": {"thread": {"id": "first-thread", "turns": []}}},
            item("first-thread", "first-turn", {"id": "q", "type": "userMessage",
                 "content": [{"type": "text", "text": "FIRST_QUESTION"}]}),
            item("first-thread", "first-turn", {"id": "a", "type": "agentMessage",
                 "phase": "final_answer", "text": "FIRST_ANSWER"}),
            item("first-thread", "first-turn", {"id": "cmd", "type": "commandExecution",
                 "command": "printf APPROVAL_CONTEXT", "cwd": "/safe/cwd",
                 "aggregatedOutput": "REPLAY_TOOL_SECRET"}),
            item("detached-thread", "detached-turn", {"id": "q", "type": "userMessage",
                 "content": [{"type": "text", "text": "DETACHED_QUESTION"}]}),
            item("detached-thread", "detached-turn", {"id": "a", "type": "agentMessage",
                 "phase": "final_answer", "text": "DETACHED_ANSWER"}),
            {"id": "tool-call", "method": "item/tool/call", "params": {
                "threadId": "first-thread", "turnId": "first-turn", "callId": "call",
                "tool": "fixture", "arguments": {"raw": "UNSUPPORTED_TOOL_SECRET"},
            }},
            {"id": "approval", "method": "item/commandExecution/requestApproval", "params": {
                "threadId": "first-thread", "turnId": "first-turn", "itemId": "cmd",
            }},
            {"id": 3, "result": {"thread": {"id": "second-thread", "turns": []}}},
            item("first-thread", "late-turn", {"id": "late", "type": "agentMessage",
                 "phase": "final_answer", "text": "STALE_THREAD_SECRET"}),
            item("second-thread", "second-turn", {"id": "q", "type": "userMessage",
                 "content": [{"type": "text", "text": "SECOND_QUESTION"}]}),
            item("second-thread", "second-turn", {"id": "a", "type": "agentMessage",
                 "phase": "final_answer", "text": "SECOND_ANSWER"}),
        ]
        fixture = self.cwd / "replay.jsonl"
        saved = "".join(json.dumps(event) + "\n" for event in events)
        fixture.write_text(saved, encoding="utf-8")
        terminal = self.start("--replay", fixture)
        self.shows(terminal, "FIRST_QUESTION", "FIRST_ANSWER", "SECOND_QUESTION",
                   "SECOND_ANSWER", "APPROVAL_CONTEXT", "item/tool/call")
        always_hidden = ["DETACHED_QUESTION", "DETACHED_ANSWER",
                         "STALE_THREAD_SECRET", "UNSUPPORTED_TOOL_SECRET"]
        self.hidden(terminal, always_hidden + ["REPLAY_TOOL_SECRET"])
        terminal.send(CTRL_O)
        self.shows(terminal, "REPLAY_TOOL_SECRET", "APPROVAL_CONTEXT")
        self.hidden(terminal, always_hidden)
        terminal.send(CTRL_O)
        self.shows(terminal, "answers only", "FIRST_ANSWER", "SECOND_ANSWER")
        self.assertNotIn("REPLAY_TOOL_SECRET", terminal.screen.text)
        self.quit(terminal)
        self.assertEqual(self.wire(), [], "replay launched a backend")
        self.assertEqual(fixture.read_text(encoding="utf-8"), saved)

    def test_replay_uses_saved_unfiltered_events_without_starting_backend(self):
        terminal = self.start()
        terminal.submit("question for replay")
        self.shows(terminal, "question for replay", "ANSWER_1", "completed")
        self.quit(terminal)
        event_files = list(self.logs.glob("*/events.jsonl"))
        self.assertEqual(len(event_files), 1)
        saved = event_files[0].read_text(encoding="utf-8")
        events = [json.loads(line) for line in saved.splitlines()]
        self.assertEqual(events, [row["message"] for row in self.wire() if row["kind"] == "send"])
        self.assertIn("TOOL_MARKER_1", saved)
        self.assertTrue(any(event.get("method") == "item/commandExecution/outputDelta" for event in events))
        wire_before = self.wire()
        replay = self.start("--replay", event_files[0])
        self.shows(replay, "Replay", "question for replay", "ANSWER_1")
        markers = ["TOOL_MARKER_1", "DELTA_SECRET_1", "MCP_SECRET_1", "COMMENTARY_SECRET_1"]
        self.hidden(replay, markers)
        replay.send(CTRL_O)
        self.shows(replay, *markers)
        replay.send(CTRL_O)
        self.shows(replay, "answers only", "ANSWER_1")
        for marker in markers:
            self.assertNotIn(marker, replay.screen.text)
        self.quit(replay)
        self.assertEqual(self.wire(), wire_before, "replay launched a backend or sent protocol traffic")
        self.assertEqual(event_files[0].read_text(encoding="utf-8"), saved)


if __name__ == "__main__":
    unittest.main(verbosity=2)
