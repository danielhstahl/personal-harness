#!/bin/bash
set -e
bd init --quiet --stealth || echo "already initialized"
exec pi "$@"
