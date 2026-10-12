Docker image that has `node`, `python`, `beads` preinstalled.  Does not run as root.

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

Quit with `Ctrl-Q`, in every mode: looprs is the container's PID 1, so the
container stops when the app does. `Ctrl-C` only quits outside the Bash view — in
Bash mode (or under a full-screen child) it is written to that shell as a cancel
and the app, and therefore the container, stays up. `docker stop <container>`
sends `SIGTERM`, which looprs treats as the same orderly exit as `Ctrl-Q`.
