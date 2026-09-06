bd init --quiet || echo "already installed"

docker run --rm -it \
  -v "$PWD:/workspace" \
  --add-host=host.docker.internal:host-gateway \
  -v $HOME/.pi/agent:/home/appuser/.pi/agent \
  ghcr.io/danielhstahl/pi-beads:v0.0.1
