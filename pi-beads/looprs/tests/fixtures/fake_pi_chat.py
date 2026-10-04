#!/usr/bin/env python3
"""A stateful fake `pi --mode rpc`, for looprs's process-level tests.

Fixture for `src/testing.rs` (`PiFake::Chat`), copied into a scratch dir per test
and invoked as the `pi` binary. It lives here, as a real file, rather than inside
a Rust string literal so that it can be read, edited and syntax-checked as the
python it is. It takes no `{{TOKENS}}`: it finds all of its state next to itself
(`HERE`, below), which is why one fixture serves every chat test.

Three properties the Pi terminal state tests depend on, and nothing else:

* **It remembers.** Every prompt lands in this process's own memory, and the
  answer to turn N quotes turns 1..N-1. A client that spawned a child per
  message would get an empty `<memory=>` on turn 2, which is the persistence
  claim under test, falsifiable.
* **A run is held open** until the test touches `settle` next to this script, so
  the test can act *during* a run rather than after it.
* **`abort` stops the held run** and settles it, the way pi unwinds.

It logs one line per command (`recv <verb> <args>`) so command *order* is
assertable -- `clear_queue` before `abort` is not a detail.
"""
import json, os, sys, threading, time

HERE = os.path.dirname(os.path.realpath(__file__))
LOG = os.path.join(HERE, "pi.log")
SETTLE = os.path.join(HERE, "settle")
# When this marker exists, `abort` is answered but the run keeps going: the shape
# of a child that traps the cancel rather than honouring it (Fakes::stubborn_pi).
STUBBORN = os.path.join(HERE, "stubborn")
# A run never stays open forever: a test that forgets to settle is a hung test.
MAX_HOLD = float(os.environ.get("LOOPRS_FAKE_HOLD", "20"))

out_lock = threading.Lock()
log_lock = threading.Lock()
st_lock = threading.Lock()

memory = []        # prompts this process has seen == its only context
queued = []        # steering / follow-up text, in queue order
state = {"turns": 0}


def log(msg):
    with log_lock:
        with open(LOG, "a") as f:
            f.write(msg + "\n")


def one_line(s):
    """Flatten a message so one command is one log line.

    `Fakes::pi_prompts()` reads this log *by line*. A real planner prompt is a
    multi-line block of instructions, and a raw newline through this path splits
    `"prompt " + msg` into a `"prompt "` line and an orphan — which makes every
    assertion about what the harness actually sent to the planner quietly useless.
    Only newlines are escaped, so plain substring assertions still match.
    """
    return s.replace("\r", " ").replace("\n", "\\n")


def emit(obj):
    with out_lock:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()


def response(rid, command, data=None, success=True, error=None):
    rec = {"type": "response", "command": command, "success": success}
    if rid is not None:
        rec["id"] = rid
    if data is not None:
        rec["data"] = data
    if error is not None:
        rec["error"] = error
    emit(rec)


def run_turn(n, user_text, answer, aborted):
    """Stream one assistant turn the way pi does, then hold it open."""
    emit({"type": "agent_start"})
    emit({"type": "turn_start"})
    emit({"type": "message_start", "message": {"role": "user", "content": user_text}})
    emit({"type": "message_end", "message": {"role": "user", "content": user_text}})
    emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
    emit({"type": "message_update", "assistantMessageEvent":
          {"type": "text_delta", "contentIndex": 0, "delta": answer}})
    emit({"type": "message_update", "assistantMessageEvent":
          {"type": "text_end", "contentIndex": 0, "content": answer}})
    emit({"type": "message_end", "message": {"role": "assistant", "content": []}})
    emit({"type": "turn_end", "message": {"role": "assistant"}, "toolResults": []})

    deadline = time.time() + MAX_HOLD
    while time.time() < deadline:
        if aborted["hit"]:
            break
        if os.path.exists(SETTLE):
            try:
                os.remove(SETTLE)
            except OSError:
                pass
            break
        time.sleep(0.01)

    emit({"type": "agent_end", "messages": [], "willRetry": False})
    emit({"type": "agent_settled"})
    log("aborted turn %d" % n if aborted["hit"] else "settled turn %d" % n)


def main():
    log("spawn pid=%d args=%s" % (os.getpid(), " ".join(sys.argv[1:])))
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
        log("recv %s %s" % (kind, one_line(msg)))

        if kind == "prompt":
            # Also logged in the pre-existing "prompt <text>" shape so the shared
            # `pi_prompts()` reader works for both fakes.
            log("prompt " + one_line(msg))
            with st_lock:
                prior = list(memory)
                memory.append(msg)
                state["turns"] += 1
                n = state["turns"]
            # The answer's memory is the whole trick: it can only be non-empty if
            # the same child process that heard turn 1 is still here.
            answer = "reply %d <memory=%s>" % (n, "|".join(prior))
            response(rid, "prompt", {"disposition": "started"})
            aborted = {"hit": False}
            t = threading.Thread(target=run_turn, args=(n, msg, answer, aborted), daemon=True)
            t.start()
            live.append(aborted)
        elif kind == "steer":
            with st_lock:
                queued.append(msg)
            response(rid, "steer", {"disposition": "queued"})
        elif kind == "follow_up":
            with st_lock:
                queued.append(msg)
            response(rid, "follow_up", {"disposition": "queued"})
        elif kind == "clear_queue":
            with st_lock:
                taken = list(queued)
                del queued[:]
            response(rid, "clear_queue", {"steering": taken, "followUp": []})
        elif kind == "abort":
            # Tell every open run to unwind, then answer like pi does: abort waits
            # for the session to become idle. Under the `stubborn` marker the
            # answer still goes back but nothing is told to unwind, which is how a
            # cancelled-but-uncancellable run actually presents.
            if not os.path.exists(STUBBORN):
                for a in list(live):
                    a["hit"] = True
                del live[:]
            response(rid, "abort")
        elif kind == "get_state":
            response(rid, "get_state", {"isStreaming": len(live) > 0,
                                        "messageCount": len(memory)})
        else:
            response(rid, kind, success=False,
                     error="fake pi: unknown command %r" % kind)


live = []
main()
