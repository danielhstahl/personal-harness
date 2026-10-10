#!/bin/bash
set -e
bd init --quiet --stealth --prefix "$BD_PREFIX" || echo "already initialized"
bd config set events-journal true
if [ -n "$GIT_USER_NAME" ]; then
    git config --global user.name "$GIT_USER_NAME"
fi
if [ -n "$GIT_USER_EMAIL" ]; then
    git config --global user.email "$GIT_USER_EMAIL"
fi
git config --global --add safe.directory /workspace
# needs a git repo, if already exists running this doesn't harm
git init
exec /app/looprs
