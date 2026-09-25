# Rendered by sanduk from recipe opencode. Edit the recipe instead.
FROM docker.io/library/node:22-slim

# opencode: base-tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ripgrep ca-certificates curl jq python3 \
 && rm -rf /var/lib/apt/lists/*

# opencode: agent
RUN npm install -g opencode-ai@1.18.29 \
 && npm cache clean --force

# opencode: drivers -- fetched at run time otherwise, which sealed has no route for
RUN npm install -g @ai-sdk/openai-compatible@3.0.48 @ai-sdk/anthropic@4.0.53 \
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
ENV HOME="/home/node"

WORKDIR /work
LABEL sanduk.recipe="opencode" sanduk.kits=""
ENTRYPOINT ["opencode"]
