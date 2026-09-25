# Rendered by sanduk from recipe hax. Edit the recipe instead.
FROM docker.io/library/debian:trixie-slim

# hax: base-tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ripgrep ca-certificates curl jq python3 \
 && rm -rf /var/lib/apt/lists/*

# hax: hax -- installed as hax: it spawns subagents by invoking that name
RUN set -eu; \
    arch="$(dpkg --print-architecture 2>/dev/null || uname -m)"; \
    case "$arch" in \
      amd64|x86_64) url=https://github.com/OleksandrChekhovskyi/hax/releases/download/v0.5.0/hax-0.5.0-linux-x86_64.tar.gz; sum=0b8480b7d7377ffeabbdc674499f4f2b9ac10f90b6ddac49bd622b04d044fc7e; member=hax ;; \
      arm64|aarch64) url=https://github.com/OleksandrChekhovskyi/hax/releases/download/v0.5.0/hax-0.5.0-linux-aarch64.tar.gz; sum=ead513f65eaf4684ffcca6f0355ff0c3ad6dc214d387029dd07734e06d26c3b0; member=hax ;; \
      *) echo "hax: no build for $arch" >&2; exit 1 ;; \
    esac; \
    tmp="$(mktemp -d)"; \
    curl -fsSL -o "$tmp/download" "$url"; \
    echo "$sum  $tmp/download" | sha256sum -c -; \
    tar -xzf "$tmp/download" -C "$tmp" "$member"; \
    install -D -m 0755 "$tmp/$member" /usr/local/bin/hax; \
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
LABEL sanduk.recipe="hax" sanduk.kits=""
ENTRYPOINT ["hax"]
