# Rendered by sanduk from recipe claude. Edit the recipe instead.
FROM docker.io/library/node:22-slim

# claude: base-tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ripgrep ca-certificates curl jq python3 \
 && rm -rf /var/lib/apt/lists/*

# claude: agent
RUN npm install -g @anthropic-ai/claude-code@2.1.272 \
 && npm cache clean --force

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
    DISABLE_AUTOUPDATER="1" \
    DISABLE_TELEMETRY="1" \
    DISABLE_ERROR_REPORTING="1" \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1"

WORKDIR /work
LABEL sanduk.recipe="claude" sanduk.kits=""
ENTRYPOINT ["claude"]
