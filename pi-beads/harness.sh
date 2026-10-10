#!/bin/bash
set -e

TAG="v0.3.1"

# sanitize PWD into something safe for docker names, and make it unique
# even if two dirs share a basename
SLUG="$(basename "$PWD")-$(echo -n "$PWD" | shasum -a 256 | cut -c1-8)"
VOLUME="beads-data-$SLUG"
BD_PREFIX="${BASENAME:0:2}"
docker volume create $VOLUME  >/dev/null
BASENAME=$(basename "$PWD")
GIT_USER_NAME="$(git config user.name || true)"
GIT_USER_NAME="${GIT_USER_NAME:-$USER}"

GIT_USER_EMAIL="$(git config user.email || true)"
GIT_USER_EMAIL="${GIT_USER_EMAIL:-$USER@example.com}"

docker run --rm -it \
  -v "$PWD:/workspace" \
  --add-host=host.docker.internal:host-gateway \
  -v $HOME/.pi/agent:/home/appuser/.pi/agent \
  -v $VOLUME:/home/appuser/.beads \
  -e GIT_USER_NAME="$GIT_USER_NAME" \
  -e GIT_USER_EMAIL="$GIT_USER_EMAIL" \
  -e LOOPRS_NTFY_URL="$NTFY_URL" \
  -e LOOPRS_NTFY_TOPIC="harness" \
  -e BD_PREFIX="$BD_PREFIX" \
  ghcr.io/danielhstahl/pi-beads:$TAG
