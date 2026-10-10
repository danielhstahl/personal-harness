#!/usr/bin/env bash
# Fake `pi` that dies immediately, for looprs's process-level tests (looprs-6ol).
#
# `PiFake::DiesImmediately`: the pipes close before anything is answered, which is
# the shape of a spawn that fails late — a missing binary found too late, a runtime
# that crashes on boot, an OOM at startup. Every "the child went away and nobody
# got a settle" path in the harness is tested against this, so it has to be a real
# process that really exits non-zero rather than a mock that merely says so.
#
# Token substituted by `src/testing.rs`:
#   LOG  -> the shared pi.log this fake appends to
set -u
LOG="{{LOG}}"
echo "spawn pid=$$" >>"$LOG"
exit 1
