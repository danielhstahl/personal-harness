#!/bin/bash
set -e

TAG="v0.4.5"

# sanitize PWD into something safe for docker names, and make it unique
# even if two dirs share a basename
SLUG="$(basename "$PWD")-$(echo -n "$PWD" | shasum -a 256 | cut -c1-8)"
# docker container names are limited to [a-zA-Z0-9][a-zA-Z0-9_.-]*
NAME="$(printf '%s' "$SLUG" | tr -cs 'a-zA-Z0-9._-' '-' | sed 's/^[-_.]*//')"
VOLUME="beads-data-$SLUG"
BASENAME=$(basename "$PWD")
BD_PREFIX="${BASENAME:0:4}"
: ${EMAIL_SUFFIX:="danielhstahl.com"}

docker volume create $VOLUME  >/dev/null

# add .transcripts to gitignore if it doesn't exist in .gitignore already
grep -qxF '.transcripts/' .gitignore || echo '.transcripts/' >> .gitignore
mkdir -p .transcripts

# looprs is PID 1 in the container (the entrypoint `exec`s it), so the container
# lives exactly as long as looprs does. Belt and braces: name it, and stop it by
# name if this script is ever interrupted, killed, or the docker client detaches
# while the container is still up (closing the terminal does that).
if [ -n "$(docker ps -q -f name="^${NAME}$")" ]; then
  echo "stopping leftover container $NAME from a previous run" >&2
  docker stop -t 15 "$NAME" >/dev/null
fi

cleanup() {
  trap - EXIT INT TERM HUP
  if [ -n "$(docker ps -q -f name="^${NAME}$")" ]; then
    # SIGTERM, which looprs answers with the same orderly exit as Ctrl-Q; docker
    # SIGKILLs after the grace period if the app ever stops answering.
    docker stop -t 15 "$NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM HUP

docker run --rm -it \
  --name "$NAME" \
  --init \
  -v "$PWD:/workspace" \
  --add-host=host.docker.internal:host-gateway \
  -v "$HOME/.pi/agent/models.json:/home/appuser/.pi/agent/models.json:ro" \
  -v "$PWD/.transcripts:/home/appuser/.local/share/looprs/transcripts/" \
  -v "$VOLUME:/home/appuser/.beads" \
  -e GIT_USER_NAME="looprs" \
  -e GIT_USER_EMAIL="looprs@${EMAIL_SUFFIX}" \
  -e LOOPRS_NTFY_URL="$NTFY_URL" \
  -e LOOPRS_NTFY_TOPIC="harness" \
  -e BD_PREFIX="$BD_PREFIX" \
  ghcr.io/danielhstahl/pi-beads:$TAG
