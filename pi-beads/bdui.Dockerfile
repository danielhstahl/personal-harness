FROM node:24-slim
RUN mkdir -p /app
ENV NPM_CONFIG_PREFIX=/app/.npm-global
ENV PATH=$PATH:/app/.npm-global/bin
ENV BEADS_DIR=/workspace/.beads
RUN addgroup --system appgroup && \
    adduser --system --ingroup appgroup appuser --home /home/appuser
# Set all of /app to root:root, read-only for appuser
RUN chown -R root:root /app && chmod -R 755 /app

RUN npm install -g --ignore-scripts beads-ui
# Switch to the non-root user
USER appuser
WORKDIR /workspace
CMD ["bdui", "start"]
