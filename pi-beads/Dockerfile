# https://pi.dev/docs/latest/containerization
FROM nikolaik/python-nodejs:python3.14-nodejs25-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends bash ca-certificates git ripgrep curl gcc \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /app
ENV NPM_CONFIG_PREFIX=/app/.npm-global
ENV PATH=$PATH:/app/.npm-global/bin
ENV BEADS_DIR=/workspace/.beads
ENV PLAYWRIGHT_BROWSERS_PATH="/app/playwright-browsers"
RUN addgroup --system appgroup && \
    adduser --system --ingroup appgroup appuser --home /home/appuser
# Set all of /app to root:root, read-only for appuser
RUN chown -R root:root /app && chmod -R 755 /app
RUN npm install -g --ignore-scripts @earendil-works/pi-coding-agent
COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh

# Install Playwright and browsers
RUN npm install -g playwright && \
    npx playwright install --with-deps chromium && \
    chmod -R o+rx /app/playwright-browsers

# install beads
RUN npm install -g @beads/bd
# Switch to the non-root user
USER appuser
# uses beads to handle work
RUN pi install npm:pi-workgraph
WORKDIR /workspace
ENTRYPOINT ["/app/entrypoint.sh"]
