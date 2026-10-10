#!/usr/bin/env bash
# Fake `bd`, for looprs's process-level tests (looprs-6ol).
#
# Copied into a per-test scratch dir and invoked as the `bd` binary by
# `src/testing.rs`. It records every command line it is given and then answers
# each verb from a file the test can rewrite mid-run. That is the whole reason it
# is a process and not a mock: "the board changed underneath the loop", "the
# worker settled but never closed the bead" and "the claim was refused while reads
# still work" are all *time* properties of the real CLI, and a stub with one fixed
# answer cannot express any of them.
#
# Tokens substituted by `src/testing.rs` (see `bd_script` for the personality
# table that picks the read/show/write/events answers):
#   LOG              -> the shared bd.log this fake appends to
#   FAIL_MARK        -> existence of this file makes every call exit 3
#   REFUSE_MARK      -> existence of this file makes every `update` exit 4
#   READ_CMD         -> the command run for `ready` / `list` (usually `cat <board>`)
#   SHOW_CMD         -> the command run for `show` (usually `cat <show>`, sometimes `[]`)
#   WRITE_CMD        -> the command run for `update` / `create` / `close`
#   EVENTS_CMD       -> the command run for `events tail` (usually `journal_tail …`)
#   JOURNAL          -> the fake events journal: one JSON record per line
#   JOURNAL_TRUNC    -> two ints, "<floor> <head>": makes `--since` below floor a refusal
#   JOURNAL_DISABLED -> existence means the journal records nothing (note on stderr)
#   JOURNAL_FAIL     -> existence makes the *probe* fail (exit 5) while reads work
#
# The board file itself is not a token: the substituted READ_CMD / SHOW_CMD name it,
# which is what lets a test rewrite the board *between* two reads of one pass.
#
# The journal is deliberately a *separate* file from the board rather than a
# function of it, because in real `bd` the two are separate too: the journal is an
# append-only record of what this replica mutated, not a mirror of what the board
# currently holds. A test that rewrites the board without moving the journal is
# modelling `bd dolt pull` — a change the journal legitimately never saw — which is
# the case the board's periodic full re-read exists to catch (ADR-0007 §7).
set -u
echo "bd $*" >>"{{LOG}}"
# Every invocation records its own pid on the way in and on the way out, so a
# test can tell "a `bd` was alive at this moment" from "a `bd` has ever run" —
# the difference the board poller's *no two reads in flight* and *no orphan left
# behind* assertions are made of (looprs-5o4.2). The trap covers every exit
# path, including the fail marker and the unsupported verb below.
echo "start $$" >>"{{LOG}}"
trap 'echo "stop $$" >>"{{LOG}}"' EXIT
# Mid-run failure lever: see `Fakes::fail_bd`. Checked per invocation, so a test
# can flip the board's health between two reads of the same pass.
if [ -e "{{FAIL_MARK}}" ]; then
  echo "fake bd: failing on request (fail marker set)" >&2
  exit 3
fi
# The verb is the first argument that is not a *global* flag: the board read is
# `bd --readonly list --all --limit 0 --json`, and `--readonly` sits in front of
# the subcommand the same way it does for the real `bd` (ADR-0007 S3).
verb=""
for arg in "$@"; do
  case "$arg" in
    --*) ;;
    *) verb="$arg"; break ;;
  esac
done
# `bd events tail --since N --limit M --json`: the two numbers the change
# detector asks with. Defaults mirror the real CLI — 0 is "from the beginning" and
# a 0 limit is "no cap".
ev_since=0
ev_limit=0
ev_prev=""
for arg in "$@"; do
  case "$ev_prev" in
    --since) ev_since="$arg" ;;
    --limit) ev_limit="$arg" ;;
  esac
  ev_prev="$arg"
done
# The fake journal reader, with the real CLI's three behaviours modelled:
# records above the checkpoint, a typed refusal when the checkpoint was pruned,
# and a note-with-nothing when the journal is off.
journal_tail() {
  local since="$1" limit="$2" floor head
  if [ -e "{{JOURNAL_TRUNC}}" ]; then
    read -r floor head <"{{JOURNAL_TRUNC}}" || true
    if [ -n "${floor:-}" ] && [ "$since" -lt "$floor" ]; then
      printf '{"code":"events_journal_truncated","error":"fake bd: checkpoint %s is below the retained window [%s..%s]","floor":%s,"head":%s,"schema_version":1,"since":%s}\n' \
        "$since" "$floor" "$head" "$floor" "$head" "$since"
      exit 1
    fi
  fi
  if [ -e "{{JOURNAL_DISABLED}}" ]; then
    echo "note: the events journal is disabled for this workspace (fake)" >&2
    return 0
  fi
  [ -f "{{JOURNAL}}" ] || return 0
  # One `awk` for the whole file, deliberately: the fake is a shell script, and
  # the per-record `sed` this replaces made "drain a thousand records" cost a
  # thousand process spawns — which is a property of the fake, not of `bd`, and
  # a test that is slow for that reason quietly stops proving anything.
  awk -v since="$since" -v limit="$limit" '
    {
      line = $0
      if (sub(/^\{"seq"[ \t]*:[ \t]*/, "", line)) {
        if (match(line, /^[0-9]+/)) {
          s = substr(line, 1, RLENGTH) + 0
          if (s > since + 0) {
            print $0
            n++
            if (limit + 0 > 0 && n >= limit + 0) exit
          }
        }
      }
    }
  ' "{{JOURNAL}}"
}
# A refused claim is its own failure mode, distinct from "bd is down": reads still
# work, only `--claim` says no. Exit 4 so the two are not confusable.
case "$verb" in
  update)
    if [ -e "{{REFUSE_MARK}}" ]; then
      echo "fake bd: cannot claim: already claimed by another owner" >&2
      exit 4
    fi
    ;;
esac
# The journal probe failing while every board read still works is its own failure
# mode too (exit 5): it has to be distinguishable from "bd is down", because the
# poller's answer to the two is different — one means "keep sweeping", the other
# means "there is no board to read either".
if [ "$verb" = "events" ] && [ -e "{{JOURNAL_FAIL}}" ]; then
  echo "fake bd: events journal probe refused (journal fail marker set)" >&2
  exit 5
fi
case "$verb" in
  ready|list) {{READ_CMD}} ;;
  show) {{SHOW_CMD}} ;;
  update|create|close) {{WRITE_CMD}} ;;
  events) {{EVENTS_CMD}} ;;
  *) echo "fake bd: unsupported verb $verb" >&2; exit 2 ;;
esac
