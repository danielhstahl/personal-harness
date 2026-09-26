#!/bin/bash
set -e

TAG="v0.2.4"

# sanitize PWD into something safe for docker names, and make it unique
# even if two dirs share a basename
SLUG="$(basename "$PWD")-$(echo -n "$PWD" | shasum -a 256 | cut -c1-8)"
VOLUME="beads-data-$SLUG"
docker volume create $VOLUME  >/dev/null
# make sure .beads exists before anything else touches the volume
docker run --rm \
  -v $VOLUME:/home/appuser/.beads \
  --entrypoint bd \
  ghcr.io/danielhstahl/pi-beads:$TAG \
  init --quiet --stealth || echo "already initialized"

GIT_USER_NAME="$(git config user.name || true)"
GIT_USER_NAME="${GIT_USER_NAME:-$USER}"

GIT_USER_EMAIL="$(git config user.email || true)"
GIT_USER_EMAIL="${GIT_USER_EMAIL:-$USER@example.com}"
# run the actual pi harness
# put models.json in $HOME/.pi/agent
docker run --rm -it \
  -v "$PWD:/workspace" \
  --add-host=host.docker.internal:host-gateway \
  -v $HOME/.pi/agent:/home/appuser/.pi/agent \
  -v $VOLUME:/home/appuser/.beads \
  -e GIT_USER_NAME="$GIT_USER_NAME" \
  -e GIT_USER_EMAIL="$GIT_USER_EMAIL" \
  -e LOOP_NTFY_URL="$NTFY_URL" \
  -e LOOP_NTFY_TOPIC="harness" \
  -e LOOP_KANBAN="board" \
  -e LOOP_MONITOR=0 \
  -e LOOP_AUDIT=0 \
  ghcr.io/danielhstahl/pi-beads:$TAG
