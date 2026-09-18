# https://pi.dev/docs/latest/containerization
FROM nikolaik/python-nodejs:python3.14-nodejs25-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends bash ca-certificates git ripgrep curl gcc unzip xz-utils zip libglu1-mesa libc6-dev \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /app
ENV NPM_CONFIG_PREFIX=/app/.npm-global
ENV PATH=$PATH:/app/.npm-global/bin

RUN addgroup --system appgroup && \
    adduser --system --ingroup appgroup appuser --home /home/appuser

# put playwright path in somewhere the user can write
ENV PLAYWRIGHT_BROWSERS_PATH=/home/appuser/playwright
ENV BEADS_DIR=/home/appuser/.beads

# Set all of /app to root:root, read-only for appuser
RUN chown -R root:root /app && chmod -R 755 /app
RUN npm install -g --ignore-scripts @earendil-works/pi-coding-agent
COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh

# install beads
RUN npm install -g @beads/bd

# set workspace
RUN mkdir -p /workspace && chown -R appuser:appgroup /workspace

# Switch to the non-root user
USER appuser
RUN mkdir -p $PLAYWRIGHT_BROWSERS_PATH
RUN mkdir -p $BEADS_DIR

## Flutter
ENV PATH="$PATH:/home/appuser/flutter/bin"
ENV FLUTTER_VERSION=3.47.4
RUN git clone https://github.com/flutter/flutter.git -b ${FLUTTER_VERSION} --depth 1 /home/appuser/flutter

## Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/home/appuser/.cargo/bin:${PATH}"

# uses beads to handle work
RUN pi install npm:pi-workgraph
WORKDIR /workspace
ENTRYPOINT ["/app/entrypoint.sh"]
