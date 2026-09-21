#!/bin/bash
set -e
bd init --quiet --stealth || echo "already initialized"
if [ -n "$GIT_USER_NAME" ]; then
    git config --global user.name "$GIT_USER_NAME"
fi
if [ -n "$GIT_USER_EMAIL" ]; then
    git config --global user.email "$GIT_USER_EMAIL"
fi
git config --global --add safe.directory /workspace
# needs a git repo, if already exists running this doesn't harm
git init
exec node /app/loop/src/main.ts
