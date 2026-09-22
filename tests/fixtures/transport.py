#!/usr/bin/env python3
"""Local fake app-server. Each test selects a mode through its private cwd."""

import json
import os
from pathlib import Path
import signal
import sys
import time


def send(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


mode = Path("mode").read_text()
print("private backend diagnostic", file=sys.stderr, flush=True)
Path("server.pid").write_text(str(os.getpid()))

if mode == "roundtrip":
    send({"method": "fixture/started", "params": {"args": sys.argv[1:], "cwd": os.getcwd()}})
    for line in sys.stdin:
        message = json.loads(line)
        if message.get("method") == "initialize":
            send({"id": message["id"], "result": {"userAgent": "fake-codex", "received": message}})
        elif message.get("method") == "initialized":
            send({"method": "fixture/initialized", "params": message})
            send({"id": "approval-λ", "method": "item/commandExecution/requestApproval",
                  "params": {"command": "printf 'hello\\n'", "unknown": {"keep": [None, False, 19]}}})
        elif "method" in message:
            send({"id": message["id"], "result": message})
        else:
            send({"method": "fixture/reply", "params": message})
elif mode == "invalid":
    sys.stdout.buffer.write(b'not json\n\xff\n\n')
    sys.stdout.buffer.flush()
    send({"method": "item/commandExecution/outputDelta", "params": {"delta": "raw\u001b[31m\nλ"}})
    sys.stdout.write('{"method":"partial')
    sys.stdout.flush()
    time.sleep(0.08)
    sys.stdout.write('/event","params":{"intact":true}}\n')
    sys.stdout.flush()
    sys.stdout.write('{"id":99,"result":"unterminated"}')
    sys.stdout.flush()
    time.sleep(0.08)
elif mode == "exit":
    send({"method": "fixture/exiting", "params": {"pid": os.getpid()}})
    sys.exit(17)
elif mode == "graceful":
    send({"method": "fixture/ready", "params": {"pid": os.getpid()}})
    sys.stdin.read()
    Path("graceful-exit").write_text("stdin EOF")
elif mode in ("stubborn", "orphan"):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    child = os.fork()
    if child == 0:
        Path("descendant.pid").write_text(str(os.getpid()))
        while True:
            signal.pause()
    while not Path("descendant.pid").exists():
        time.sleep(0.005)
    send({"method": "fixture/ready", "params": {"pid": os.getpid(), "child": child}})
    if mode == "orphan":
        os._exit(0)
    # Leave the reader in the middle of a line while both processes ignore SIGTERM.
    sys.stdout.write('{"partial":')
    sys.stdout.flush()
    while True:
        signal.pause()
else:
    raise RuntimeError(f"unknown fixture mode: {mode}")
