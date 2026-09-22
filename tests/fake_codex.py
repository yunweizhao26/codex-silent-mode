#!/usr/bin/env python3
"""Offline app-server fixture; all protocol traffic is recorded in its test cwd."""

from collections import deque
import json
import os
from pathlib import Path
import select
import sys
import time


def main():
    if os.environ.get("CODEX_SILENT_E2E") != "1":
        sys.exit("This fixture must be launched by terminal_e2e.py")
    args = sys.argv[1:]
    if args[:3] != ["app-server", "--listen", "stdio://"]:
        sys.exit(f"Unexpected app-server arguments: {args!r}")
    if len(args[3:]) % 2 or any(arg != "-c" for arg in args[3::2]):
        sys.exit(f"Unexpected config arguments: {args!r}")

    with Path("fixture-wire.jsonl").open("a", buffering=1, encoding="utf-8") as log:
        def record(kind, **fields):
            log.write(json.dumps({"kind": kind, **fields}, ensure_ascii=False) + "\n")

        def send(message):
            record("send", message=message)
            print(json.dumps(message, ensure_ascii=False), flush=True)

        def result(request, value):
            send({"id": request["id"], "result": value})

        thread_id = "thread-e2e"
        turn_id = None
        turn_number = 0
        initialized = False
        handshake = False
        approval = False
        scheduled = deque()

        def event(method, **params):
            return {"method": method, "params": {
                "threadId": thread_id, "turnId": turn_id, **params,
            }}

        def item(value):
            send(event("item/started", item=value))
            send(event("item/completed", item=value))

        def command(marker, item_id="command"):
            return {"id": item_id, "type": "commandExecution", "status": "completed",
                    "command": "printf fixture", "cwd": os.getcwd(),
                    "aggregatedOutput": marker, "exitCode": 0, "durationMs": 1}

        def agent(text, phase="final_answer", item_id="answer"):
            return {"id": item_id, "type": "agentMessage", "phase": phase, "text": text}

        def complete(text, status="completed"):
            nonlocal turn_id
            item(agent(text))
            send(event("turn/completed", turn={
                "id": turn_id, "status": status, "items": [], "error": None,
            }))
            turn_id = None

        def activity():
            value = command(f"TOOL_MARKER_{turn_number}\nDELTA_SECRET_{turn_number}")
            send(event("item/started", item={**value, "status": "inProgress", "aggregatedOutput": ""}))
            send(event("item/commandExecution/outputDelta", itemId="command",
                       delta=f"TOOL_MARKER_{turn_number}\n"))
            send(event("item/commandExecution/outputDelta", itemId="command",
                       delta=f"DELTA_SECRET_{turn_number}"))
            send(event("item/completed", item=value))
            item({"id": "mcp", "type": "mcpToolCall", "server": "fixture", "tool": "lookup",
                  "arguments": {}, "status": "completed", "error": None,
                  "result": {"content": [{"type": "text", "text": f"MCP_SECRET_{turn_number}"}]}})
            send(event("item/started", item=agent("", "commentary", "commentary")))
            send(event("item/agentMessage/delta", itemId="commentary",
                       delta=f"COMMENTARY_SECRET_{turn_number}"))
            send(event("item/completed", item=agent(
                f"COMMENTARY_SECRET_{turn_number}", "commentary", "commentary")))

        record("start", pid=os.getpid(), pgid=os.getpgrp(), argv=args, cwd=os.getcwd())
        print("STDERR_SECRET: local fixture diagnostic", file=sys.stderr, flush=True)
        pending = b""
        while True:
            now = time.monotonic()
            while scheduled and scheduled[0][0] <= now:
                _, message = scheduled.popleft()
                send(message)
            timeout = max(0, scheduled[0][0] - now) if scheduled else None
            if not select.select([sys.stdin.fileno()], [], [], timeout)[0]:
                continue
            chunk = os.read(sys.stdin.fileno(), 65536)
            if not chunk:
                record("eof")
                return
            pending += chunk
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                request = json.loads(line)
                record("receive", message=request)
                method = request.get("method")
                params = request.get("params", {})
                if method == "initialize":
                    assert not handshake
                    handshake = True
                    result(request, {"userAgent": "fake-codex-e2e/1.0"})
                elif method == "initialized":
                    assert handshake and "id" not in request
                    initialized = True
                elif method in ("thread/start", "thread/resume"):
                    assert initialized and turn_id is None
                    assert Path(params["cwd"]).resolve() == Path.cwd()
                    turns = []
                    if method == "thread/resume":
                        thread_id = params["threadId"]
                        turn_number = 1
                        turns = [{"id": "turn-1", "status": "completed", "items": [
                            {"id": "user", "type": "userMessage", "content": [
                                {"type": "text", "text": "RESUMED_QUESTION 中文"}]},
                            command("RESUME_TOOL_MARKER"),
                            {"id": "mcp", "type": "mcpToolCall", "result": {"text": "RESUME_MCP_SECRET"}},
                            agent("RESUME_COMMENTARY_SECRET", "commentary", "commentary"),
                            agent("RESUMED_ANSWER 中文"),
                        ]}]
                    result(request, {"thread": {"id": thread_id, "turns": turns}})
                elif method == "turn/start":
                    assert initialized and turn_id is None
                    assert params["threadId"] == thread_id
                    turn_number += 1
                    turn_id = f"turn-{turn_number}"
                    turn = {"id": turn_id, "status": "inProgress", "items": [], "error": None}
                    result(request, {"turn": turn})
                    send(event("turn/started", turn=turn))
                    item({"id": "user", "type": "userMessage", "content": params["input"]})
                    activity()
                    question = "\n".join(value.get("text", "") for value in params["input"])
                    if question == "approval please":
                        approval = True
                        send({"id": "approval-1", "method": "item/commandExecution/requestApproval",
                              "params": {"threadId": thread_id, "turnId": turn_id, "itemId": "approval-command",
                                         "command": "printf APPROVAL_VISIBLE", "cwd": os.getcwd(),
                                         "reason": "APPROVAL_REQUIRED", "availableDecisions": ["accept", "decline", "cancel"]}})
                    elif question == "noisy work":
                        # Stream across many UI frames; stay active until Ctrl+C.
                        for n in range(240):
                            value = command((f"NOISE_SECRET_{n}\n" * 8), f"noise-{n}")
                            scheduled.append((time.monotonic() + 0.015 * (n // 5 + 1),
                                              event("item/completed", item=value)))
                        scheduled.append((scheduled[-1][0] + 0.03,
                                          event("warning", message="NOISE_DRAINED")))
                    else:
                        complete(f"ANSWER_{turn_number} 中文 café")
                elif method == "turn/interrupt":
                    assert params == {"threadId": thread_id, "turnId": turn_id}
                    assert turn_id is not None
                    result(request, {})
                    scheduled.clear()
                    approval = False
                    complete("INTERRUPTED_ANSWER", "interrupted")
                elif method is None and request.get("id") == "approval-1":
                    assert approval
                    approval = False
                    decision = request["result"]["decision"]
                    assert decision in ("accept", "decline", "cancel")
                    send(event("serverRequest/resolved", requestId="approval-1"))
                    complete("APPROVAL_ACCEPTED" if decision == "accept" else "APPROVAL_DECLINED")
                else:
                    raise AssertionError(f"Unexpected client message: {request!r}")


if __name__ == "__main__":
    main()
