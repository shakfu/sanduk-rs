# Rendered by sanduk from recipe hermes. Edit the recipe instead.
FROM docker.io/library/python:3.12-slim

# hermes: base-tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ripgrep ca-certificates curl jq \
 && rm -rf /var/lib/apt/lists/*

# hermes: agent
RUN pip install --no-cache-dir hermes-agent==0.19.0

# The agent's uid and gid. Docker passes the caller's, so the agent can
# write a bind mount the host user owns.
ARG AGENT_UID=1000
ARG AGENT_GID=1000
LABEL sanduk.agent-uid=$AGENT_UID
RUN if id -u agent >/dev/null 2>&1; then \
      groupmod -o -g "$AGENT_GID" "$(id -gn agent)" \
      && usermod -o -u "$AGENT_UID" -g "$AGENT_GID" agent; \
    else \
      groupadd -o -g "$AGENT_GID" agent \
      && useradd -o --create-home --home-dir /home/agent \
         --uid "$AGENT_UID" --gid "$AGENT_GID" agent; \
    fi \
 && mkdir -p /work && chown "$AGENT_UID:$AGENT_GID" /work

USER agent
ENV HOME="/home/agent" \
    PYTHONUNBUFFERED="1"

WORKDIR /work
LABEL sanduk.recipe="hermes" sanduk.kits=""
ENTRYPOINT ["python", "-m", "run_agent"]
