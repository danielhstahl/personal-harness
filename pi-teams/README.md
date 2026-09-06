Not recommended for local models for performance reasons.

To run:

```sh
docker run --rm -it \
  -v "$PWD:/workspace" \
  # if on a mac
  --add-host=host.docker.internal:host-gateway \
  # optional, do this if you want to persist settings between runs
  -v $PWD/pi-agent-home:/home/appuser/.pi/agent \
  ghcr.io/danielhstahl/pi-teams
```

Build/run locally:

```sh
docker build -t pi-sandbox -f Dockerfile . --no-cache

docker run --rm -it \
  -v "$PWD:/workspace" \
  # if on a mac
  --add-host=host.docker.internal:host-gateway \
  # optional, do this if you want to persist settings between runs
  -v $PWD/pi-agent-home:/home/appuser/.pi/agent \
  pi-sandbox
```
