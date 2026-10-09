#!/usr/bin/env bash
# The docs rot gate (looprs-00u.10 / ADR-0008) — a shell wrapper around
# scripts/docs_check.py so the step in scripts/check.sh looks like the other steps
# and can be run the same way.
#
#   ./scripts/docs_check.sh                # the gate
#   ./scripts/docs_check.sh --fix-keymap  # regenerate docs/guide/keymap.md's tables
#   ./scripts/docs_check.sh --list-knobs  # every LOOPRS_* the code reads, with file:line
#   ./scripts/docs_check.sh --list-captures  # every spike capture, its total, which is current
#   ./scripts/docs_check.sh --list-test-names  # every test name the docs quote, and what resolved it
#
#   ./scripts/docs_check.sh --list-corpus  # today's corpus line counts, each with
#         the command that produced it — the re-count that looprs-00u.27 asked to
#         be a re-run rather than a re-type
#
# Runs in well under a second, needs no network and no cargo, and is a step of
# ./scripts/check.sh. See the Python file for what the checks are and why each one
# is worth a gate.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v python3 >/dev/null 2>&1; then
    echo "docs_check.sh: python3 is not on PATH" >&2
    exit 127
fi
exec python3 "$here/docs_check.py" "$@"
