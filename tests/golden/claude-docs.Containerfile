# Rendered by sanduk from recipe claude-docs. Edit the recipe instead.
FROM docker.io/library/node:22-slim

# claude-docs: base-tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ripgrep ca-certificates curl jq python3 \
 && rm -rf /var/lib/apt/lists/*

# claude-docs: agent
RUN npm install -g @anthropic-ai/claude-code@2.1.272 \
 && npm cache clean --force

# kit-docs: d2 -- d2 0.9.0, MPL-2.0; renders PNG, PDF and PPTX without a browser
RUN set -eu; \
    arch="$(dpkg --print-architecture 2>/dev/null || uname -m)"; \
    case "$arch" in \
      amd64|x86_64) url=https://github.com/d2lang/d2/releases/download/v0.9.0/d2-v0.9.0-linux-amd64.tar.gz; sum=5669ddc46b99e942cc96078f4a4e36d5e62103348f4c05179ede27802fdd87a9; member=d2-v0.9.0/bin/d2 ;; \
      arm64|aarch64) url=https://github.com/d2lang/d2/releases/download/v0.9.0/d2-v0.9.0-linux-arm64.tar.gz; sum=ac2c028697199479acb321db1e3d68caee9f2ba492ed73caa3cd13f3829bf913; member=d2-v0.9.0/bin/d2 ;; \
      *) echo "d2: no build for $arch" >&2; exit 1 ;; \
    esac; \
    tmp="$(mktemp -d)"; \
    curl -fsSL -o "$tmp/download" "$url"; \
    echo "$sum  $tmp/download" | sha256sum -c -; \
    tar -xzf "$tmp/download" -C "$tmp" "$member"; \
    install -D -m 0755 "$tmp/$member" /usr/local/bin/d2; \
    rm -rf "$tmp"

# kit-docs: officecli -- officecli 1.0.150, Apache-2.0; a self-contained .NET binary
RUN set -eu; \
    arch="$(dpkg --print-architecture 2>/dev/null || uname -m)"; \
    case "$arch" in \
      amd64|x86_64) url=https://github.com/iOfficeAI/OfficeCLI/releases/download/v1.0.150/officecli-linux-x64; sum=faceb42654004f1fa5c40fb0ce641c42b7dc5a2beb270f25971ea6265b7dc227 ;; \
      arm64|aarch64) url=https://github.com/iOfficeAI/OfficeCLI/releases/download/v1.0.150/officecli-linux-arm64; sum=93bea35aaa8f153a56a4f6e1a7d22d82aec379e42ce2d45b24b0941dcfd50968 ;; \
      *) echo "officecli: no build for $arch" >&2; exit 1 ;; \
    esac; \
    tmp="$(mktemp -d)"; \
    curl -fsSL -o "$tmp/download" "$url"; \
    echo "$sum  $tmp/download" | sha256sum -c -; \
    install -D -m 0755 "$tmp/download" /usr/local/bin/officecli; \
    rm -rf "$tmp"

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

RUN install -d -o "$AGENT_UID" -g "$AGENT_GID" /home/node/.claude /home/node/.claude/skills

# kit-docs: skill d2
COPY skills/docs/d2/SKILL.md /home/node/.claude/skills/d2/SKILL.md
RUN chmod -R a=rX /home/node/.claude/skills/d2
# kit-docs: skill officecli
RUN set -eu; \
    tmp="$(mktemp)"; \
    curl -fsSL -o "$tmp" https://raw.githubusercontent.com/iOfficeAI/OfficeCLI/0c713e5f5a8b226a49b1cfec3343346c973a57a0/SKILL.md; \
    echo "c950d285ce60021712b4753fb2d9f592308d5622bab776229061dfecb1ce55d4  $tmp" | sha256sum -c -; \
    install -D -m 0444 "$tmp" /home/node/.claude/skills/officecli/SKILL.md; \
    chmod 0555 /home/node/.claude/skills/officecli; \
    rm -f "$tmp"

USER node
ENV HOME="/home/node" \
    OFFICECLI_SKIP_UPDATE="1" \
    DOTNET_SYSTEM_GLOBALIZATION_INVARIANT="1" \
    DISABLE_AUTOUPDATER="1" \
    DISABLE_TELEMETRY="1" \
    DISABLE_ERROR_REPORTING="1" \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1"

WORKDIR /work
LABEL sanduk.recipe="claude-docs" sanduk.kits="docs@7ad96f7cbe10f0824cce2b84aeb820ffb7eb3f496873e2dc22969456eeb79d67"
ENTRYPOINT ["claude"]
