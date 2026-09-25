# Rendered by sanduk from recipe prime. Edit the recipe instead.
FROM docker.io/library/debian:trixie-slim

# prime: base-tools -- fd-find and ripgrep are the fd and rg prime-agent would otherwise download unpinned
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ripgrep fd-find ca-certificates curl jq python3 \
 && rm -rf /var/lib/apt/lists/*

# prime: agent -- the archive stays whole: prime-agent finds its assets and Python runtime beside the executable. x64 is the baseline build, for CPUs without AVX2
RUN set -eu; \
    arch="$(dpkg --print-architecture 2>/dev/null || uname -m)"; \
    case "$arch" in \
      amd64|x86_64) url=https://github.com/PrimeIntellect-ai/prime-agent/releases/download/v0.9.5/prime-agent-0.9.5-linux-x64-baseline.tar.gz; sum=7888fd9735b97a6c9865738467b00f67ecba72e9e597f1de2480fb584e74a2b0 ;; \
      arm64|aarch64) url=https://github.com/PrimeIntellect-ai/prime-agent/releases/download/v0.9.5/prime-agent-0.9.5-linux-arm64.tar.gz; sum=81303f98f31aed99aa641a66ccb87629940de1e45dd86d56ede1d88fd521971c ;; \
      *) echo "agent: no build for $arch" >&2; exit 1 ;; \
    esac; \
    tmp="$(mktemp -d)"; \
    curl -fsSL -o "$tmp/download" "$url"; \
    echo "$sum  $tmp/download" | sha256sum -c -; \
    mkdir -p /opt/prime-agent; \
    tar -xzf "$tmp/download" -C /opt/prime-agent --strip-components="${strip:-0}"; \
    ln -sf /opt/prime-agent/prime-agent /usr/local/bin/prime-agent; \
    rm -rf "$tmp"

# prime: entrypoint
COPY files/prime/entrypoint/sanduk-prime /usr/local/bin/sanduk-prime
RUN chmod 0755 /usr/local/bin/sanduk-prime

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
    PATH="/home/agent/.local/bin:$PATH" \
    PRIME_AGENT_CODING_AGENT_DIR="/home/agent/.prime/agent" \
    PI_OFFLINE="1" \
    PI_SKIP_VERSION_CHECK="1"

# prime: kernel -- the archive has no virtual environment, and a sealed run cannot fetch one: uv and Python are installed here
COPY steps/prime-kernel.sh /usr/local/share/sanduk/steps/prime-kernel.sh
RUN sh -eu /usr/local/share/sanduk/steps/prime-kernel.sh

WORKDIR /work
LABEL sanduk.recipe="prime" sanduk.kits=""
ENTRYPOINT ["sanduk-prime"]
