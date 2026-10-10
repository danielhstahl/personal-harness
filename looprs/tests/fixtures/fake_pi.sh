#!/usr/bin/env bash
# Fake `pi --mode rpc`, for looprs's process-level tests (looprs-6ol).
#
# Copied into a per-test scratch dir and invoked as the `pi` binary by
# `src/testing.rs` (`PiFake::Started` / `Handled` / `Rejects`). It answers a
# `prompt` command with one canned response line and keeps every command it saw on
# the log, because the assertions that matter are about what the *harness* sent:
# which verbs, in which order, against which id.
#
# Tokens substituted by `src/testing.rs`:
#   LOG    -> the shared pi.log this fake appends to
#   REPLY  -> the exact `printf` that answers a prompt (the personality)
#
# It is deliberately *not* a stateful chat child — that is
# `fake_pi_chat.py`, which needs threads to be steerable mid-run. This one is the
# cheap fake for the tests that only ever need "the prompt was asked".
set -u
LOG="{{LOG}}"
echo "spawn pid=$$ args=$*" >>"$LOG"
while IFS= read -r line; do
  case "$line" in
    *'"type":"prompt"'*)
      echo "prompt $line" >>"$LOG"
      id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
      {{REPLY}}
      ;;
    *)
      echo "cmd $line" >>"$LOG"
      ;;
  esac
done
