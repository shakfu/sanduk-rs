# Rendered by sanduk from recipe pi. Edit the recipe instead.
FROM docker.io/library/node:22-slim

# pi: base-tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ripgrep ca-certificates curl jq python3 \
 && rm -rf /var/lib/apt/lists/*

# pi: agent -- --ignore-scripts is what pi's own containerization doc installs with
RUN npm install -g --ignore-scripts @earendil-works/pi-coding-agent@0.85.1 \
 && npm cache clean --force

# pi: entrypoint
COPY files/pi/entrypoint/sanduk-pi /usr/local/bin/sanduk-pi
RUN chmod 0755 /usr/local/bin/sanduk-pi

# The agent's uid and gid. Docker passes the caller's, so the agent can
# write a bind mount the host user owns.
ARG AGENT_UID=1000
ARG AGENT_GID=1000
LABEL sanduk.agent-uid=$AGENT_UID
RUN if id -u node >/dev/null 2>&1; then \
      groupmod -o -g "$AGENT_GID" "$(id -gn node)" \
      && usermod -o -u "$AGENT_UID" -g "$AGENT_GID" node; \
    else \
      groupadd -o -g "$AGENT_GID" node \
      && useradd -o --create-home --home-dir /home/node \
         --uid "$AGENT_UID" --gid "$AGENT_GID" node; \
    fi \
 && mkdir -p /work && chown "$AGENT_UID:$AGENT_GID" /work

USER node
ENV HOME="/home/node" \
    PI_CODING_AGENT_DIR="/home/node/.pi/agent" \
    PI_OFFLINE="1" \
    PI_SKIP_VERSION_CHECK="1" \
    PI_TELEMETRY="0"

WORKDIR /work
LABEL sanduk.recipe="pi" sanduk.kits=""
ENTRYPOINT ["sanduk-pi"]
