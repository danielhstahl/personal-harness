#!/usr/bin/env python3
"""Spike for ADR-0001 Q4: can Bash mode reuse pi's RPC `bash` command instead of our own shell?

Sends two dependent bash commands through `pi --mode rpc` and checks whether shell
state (cwd, exported env, background jobs) survives from one command to the next.
No LLM turn is involved, so this runs offline.

    python3 spikes/pi_bash_rpc.py
"""
import json
import subprocess
import sys
import threading
import time
import queue

PROBES = [
    ("pwd_first", "pwd"),
    ("cd", "cd /tmp"),
    ("pwd_second", "pwd"),
    ("env_first", 'echo "LOOPRS_SPIKE=${LOOPRS_SPIKE:-unset}"'),
    ("env_set", "export LOOPRS_SPIKE=42"),
    ("env_second", 'echo "LOOPRS_SPIKE=${LOOPRS_SPIKE:-unset}"'),
    ("bg", "(sleep 5 &) ; echo started"),
    ("bg_visible", "jobs | wc -l"),
]


def main() -> int:
    proc = subprocess.Popen(
        ["pi", "--mode", "rpc"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, bufsize=1,
    )
    q: "queue.Queue[dict]" = queue.Queue()

    def reader():
        for line in proc.stdout:
            try:
                q.put(json.loads(line))
            except json.JSONDecodeError:
                pass

    threading.Thread(target=reader, daemon=True).start()
    time.sleep(3.0)  # let pi come up

    for name, cmd in PROBES:
        rid = f"spike-{name}"
        proc.stdin.write(json.dumps({"id": rid, "type": "bash", "command": cmd}) + "\n")
        proc.stdin.flush()
        deadline = time.time() + 15
        got = None
        while time.time() < deadline:
            try:
                msg = q.get(timeout=0.5)
            except queue.Empty:
                continue
            if msg.get("type") == "response" and msg.get("id") == rid:
                got = msg
                break
        if got is None:
            print(f"PROBE {name}=TIMEOUT cmd={cmd!r}")
        else:
            data = got.get("data") or {}
            out = (data.get("output") or "").strip().replace("\n", " | ")
            print(f"PROBE {name}={out!r} exit={data.get('exitCode')} ok={got.get('success')}")

    proc.stdin.close()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    return 0


if __name__ == "__main__":
    sys.exit(main())
