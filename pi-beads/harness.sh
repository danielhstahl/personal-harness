#!/bin/bash
set -e

TAG="v0.0.7"

# sanitize PWD into something safe for docker names, and make it unique
# even if two dirs share a basename
SLUG="$(basename "$PWD")-$(echo -n "$PWD" | shasum -a 256 | cut -c1-8)"
VOLUME="beads-data-$SLUG"
UI_NAME="beads-ui-$SLUG"

docker volume create $VOLUME  >/dev/null

# make sure .beads exists before anything else touches the volume
docker run --rm \
  -v $VOLUME:/workspace/.beads \
  --entrypoint bd \
  ghcr.io/danielhstahl/pi-beads:$TAG \
  init --quiet --stealth || echo "already initialized"

# clean up any leftover UI container from a previous ungraceful exit
docker rm -f $UI_NAME >/dev/null 2>&1 || true

# start the UI in the background
# let docker pick a free host port instead of hardcoding one
# only apply to localhost/loopback (don't expose beyond machine)
docker run -d --rm \
  --name $UI_NAME \
  -p 127.0.0.1::3000 \
  --add-host=host.docker.internal:host-gateway \
  -v $VOLUME:/workspace/.beads \
  ghcr.io/danielhstahl/bd-ui:$TAG

BD_UI_PORT=$(docker port "$UI_NAME" 3000/tcp | head -n1 | cut -d: -f2)
echo "beads-ui running at http://localhost:$BD_UI_PORT"
open "http://localhost:$BD_UI_PORT"

# make sure it's stopped whenever this script exits, however that happens
trap "docker stop $UI_NAME >/dev/null 2>&1" EXIT

GIT_USER_NAME="$(git config user.name || true)"
GIT_USER_NAME="${GIT_USER_NAME:-$USER}"

GIT_USER_EMAIL="$(git config user.email || true)"
GIT_USER_EMAIL="${GIT_USER_EMAIL:-$USER}@example.com"
# run the actual pi harness
# put models.json in $HOME/.pi/agent
docker run --rm -it \
  -v "$PWD:/workspace" \
  --add-host=host.docker.internal:host-gateway \
  -v $HOME/.pi/agent:/home/appuser/.pi/agent \
  -v $VOLUME:/workspace/.beads \
  -e GIT_USER_NAME="$GIT_USER_NAME" \
  -e GIT_USER_EMAIL="$GIT_USER_EMAIL" \
  ghcr.io/danielhstahl/pi-beads:$TAG
