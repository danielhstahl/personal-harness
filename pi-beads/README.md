Docker image that has `node`, `python`, `beads`, and pi extension `npm:pi-workgraph` preinstalled.  Does not run as root.

To run:

```sh
docker run --rm -it \
  -v "$PWD:/workspace" \
  # if on a mac
  --add-host=host.docker.internal:host-gateway \
  # optional, do this if you want to persist settings between runs
  -v $PWD/pi-agent-home:/home/appuser/.pi/agent \
  ghcr.io/danielhstahl/pi-beads
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

docker run --rm -it \
  -e LLAMA_BASE_URL=llm.home:8000 \
  -v "$PWD:/workspace" \
  --add-host=host.docker.internal:host-gateway \
  pi-sandbox

docker build -t tmp -f Dockerfile . --no-cache
