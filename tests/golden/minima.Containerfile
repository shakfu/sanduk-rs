# Rendered by sanduk from recipe minima. Edit the recipe instead.
FROM docker.io/library/debian:trixie-slim

# minima: base-tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ripgrep ca-certificates curl jq python3 \
 && rm -rf /var/lib/apt/lists/*

# minima: minima
RUN set -eu; \
    arch="$(dpkg --print-architecture 2>/dev/null || uname -m)"; \
    case "$arch" in \
      amd64|x86_64) url=https://github.com/shakfu/minima/releases/download/0.3.0/minima-0.3.0-linux-x86_64.tar.gz; sum=b1efe3d406b7482001339b90254fd9a0293fee77ee40fbe0171de694e5d8d37c; member=minima ;; \
      arm64|aarch64) url=https://github.com/shakfu/minima/releases/download/0.3.0/minima-0.3.0-linux-aarch64.tar.gz; sum=a55b48969684b9dc308899aa60257dd6729fd60a0928a3adefd21f9876b30617; member=minima ;; \
      *) echo "minima: no build for $arch" >&2; exit 1 ;; \
    esac; \
    tmp="$(mktemp -d)"; \
    curl -fsSL -o "$tmp/download" "$url"; \
    echo "$sum  $tmp/download" | sha256sum -c -; \
    tar -xzf "$tmp/download" -C "$tmp" "$member"; \
    install -D -m 0755 "$tmp/$member" /usr/local/bin/minima; \
    rm -rf "$tmp"

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
ENV HOME="/home/agent"

WORKDIR /work
LABEL sanduk.recipe="minima" sanduk.kits=""
ENTRYPOINT ["minima"]
