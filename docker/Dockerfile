FROM rust:latest

# Set working directory
WORKDIR /app

# Copy manifest files first to leverage Docker cache
COPY looprs/Cargo.toml ./
COPY looprs/Cargo.lock ./
# Copy actual source code
COPY looprs/src ./src

# Build the application.
#
# `--features notify` keeps the capability this artifact has always had: the ntfy
# completion notice. The crate's default build is now without it — that half is
# 75 of the 218 crates in a cold build and 45% of the binary (measured in
# looprs/spikes/results/dependency-cost.log, looprs-00u.15) — so the deployment
# that wants the doorbell says so here rather than inheriting it by accident. A
# looprs built without it still starts and still reads LOOPRS_NTFY_*; it just logs
# one line saying it cannot post.
RUN cargo build --release --features notify

# https://pi.dev/docs/latest/containerization
FROM nikolaik/python-nodejs:python3.14-nodejs25-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends bash ca-certificates git ripgrep curl gcc unzip xz-utils zip libglu1-mesa libc6-dev \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /app
ENV NPM_CONFIG_PREFIX=/app/.npm-global
ENV PATH=$PATH:/app/.npm-global/bin
#
COPY --from=0 /app/target/release/looprs /app/looprs

RUN addgroup --system appgroup && \
    adduser --system --ingroup appgroup appuser --home /home/appuser

# put playwright path in somewhere the user can write
ENV PLAYWRIGHT_BROWSERS_PATH=/home/appuser/playwright
ENV BEADS_DIR=/home/appuser/.beads

# two hours timeout
#ENV LOOP_WORK_TIMEOUT_MS=7200000

# Set all of /app to root:root, read-only for appuser
RUN chown -R root:root /app && chmod -R 755 /app
RUN npm install -g --ignore-scripts @earendil-works/pi-coding-agent
COPY docker/entrypoint.sh /app/entrypoint.sh
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
ENV FLUTTER_VERSION=stable
RUN git clone https://github.com/flutter/flutter.git -b ${FLUTTER_VERSION} --depth 1 /home/appuser/flutter

## Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/home/appuser/.cargo/bin:${PATH}"

# make sure bash times out if it waits too long
RUN pi install npm:@piotr-oles/pi-bash-timeout
WORKDIR /workspace
ENTRYPOINT ["/app/entrypoint.sh"]
