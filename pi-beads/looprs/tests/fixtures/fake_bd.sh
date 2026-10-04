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
# table that picks the read/show/write answers):
#   LOG          -> the shared bd.log this fake appends to
#   FAIL_MARK    -> existence of this file makes every call exit 3
#   REFUSE_MARK  -> existence of this file makes every `update` exit 4
#   READ_CMD     -> the command run for `ready` / `list` (usually `cat <board>`)
#   SHOW_CMD     -> the command run for `show` (usually `cat <show>`, sometimes `[]`)
#   WRITE_CMD    -> the command run for `update` / `create` / `close`
#
# The board file itself is not a token: the substituted READ_CMD / SHOW_CMD name it,
# which is what lets a test rewrite the board *between* two reads of one pass.
set -u
echo "bd $*" >>"{{LOG}}"
# Mid-run failure lever: see `Fakes::fail_bd`. Checked per invocation, so a test
# can flip the board's health between two reads of the same pass.
if [ -e "{{FAIL_MARK}}" ]; then
  echo "fake bd: failing on request (fail marker set)" >&2
  exit 3
fi
verb="${1:-}"
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
case "$verb" in
  ready|list) {{READ_CMD}} ;;
  show) {{SHOW_CMD}} ;;
  update|create|close) {{WRITE_CMD}} ;;
  *) echo "fake bd: unsupported verb $verb" >&2; exit 2 ;;
esac
