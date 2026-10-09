#!/usr/bin/env python3
"""A `pi` that streams one **open** paragraph slowly, for the viewport spike.

`tests/fixtures/fake_pi_chat.py` answers with `text_delta` and `message_end` back
to back, so its reply is flushed to the scrollback before a frame can be drawn
over it — there is no live tail to measure. This one keeps the assistant message
open and dribbles the text out, which is the shape a real streamed answer has and
the only shape in which "how tall is the live region?" has an answer.

The block never closes on its own while the spike is measuring: HOLD controls how
long it stays open, and there is no `settle` to touch. `abort` unwinds it, so the
harness can still be cancelled.

Reply text is `<MARK> word word ...` — one paragraph, no blank line, so it stays
one open prose block and wraps instead of flushing.
"""
import json
import os
import sys
import threading
import time

HOLD = float(os.environ.get("LOOPRS_FAKE_HOLD", "30"))
# ~190 chars per chunk; 24 chunks is ~4500 chars, ~46 wrapped rows at 100 cols.
CHUNK = "bravo charlie echo foxtrot golf hotel india juliett kilo lima mike november oscar papa quebec romeo sierra tango uniform victor "
MARK = "ALPHASHEET"
N_CHUNKS = 24
DELAY = 0.22

out_lock = threading.Lock()
log_lock = threading.Lock()
aborted = {"hit": False}


def log(msg):
    with log_lock:
        print(msg, file=sys.stderr, flush=True)


def emit(obj):
    with out_lock:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()


def stream_turn(user_text):
    emit({"type": "agent_start"})
    emit({"type": "turn_start"})
    emit({"type": "message_start", "message": {"role": "user", "content": user_text}})
    emit({"type": "message_end", "message": {"role": "user", "content": user_text}})
    emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
    body = ""
    deadline = time.time() + HOLD
    for i in range(N_CHUNKS):
        if aborted["hit"] or time.time() > deadline:
            break
        # The first chunk carries the marker; the rest are identical, so the
        # needle for "is answer text on this row" is CHUNK's own first word.
        chunk = (MARK + " " + CHUNK) if i == 0 else CHUNK
        body += chunk
        emit({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_delta",
                "contentIndex": 0,
                "delta": chunk,
            },
        })
        time.sleep(DELAY)
    emit({
        "type": "message_update",
        "assistantMessageEvent": {
            "type": "text_end",
            "contentIndex": 0,
            "content": body,
        },
    })
    emit({"type": "message_end", "message": {"role": "assistant", "content": []}})
    emit({"type": "turn_end", "message": {"role": "assistant"}, "toolResults": []})
    emit({"type": "agent_end", "messages": [], "willRetry": False})
    emit({"type": "agent_settled"})
    log("settled fake turn")


def main():
    log("fake pi slow: pid=%d" % os.getpid())
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            cmd = json.loads(line)
        except ValueError:
            continue
        kind = cmd.get("type")
        rid = cmd.get("id")
        msg = cmd.get("message", "")
        log("recv %s %s" % (kind, msg.replace("\n", "\\n")[:80]))
        if kind == "prompt":
            emit({"type": "response", "id": rid, "command": "prompt", "success": True,
                  "data": {"disposition": "started"}})
            threading.Thread(target=stream_turn, args=(msg,), daemon=True).start()
        elif kind == "abort":
            aborted["hit"] = True
            emit({"type": "response", "id": rid, "command": "abort", "success": True})
        else:
            emit({"type": "response", "id": rid, "command": kind, "success": True,
                  "data": {}})


if __name__ == "__main__":
    main()
