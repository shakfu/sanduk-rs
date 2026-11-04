# tsk-tsk

Status: 2026-11-04. Read from source at `main`; nothing here is measured unless marked. No decision is taken. Claims about Docker and git internals not cited are inference.

Source: <https://github.com/dtormoen/tsk-tsk>.

## What it is

A Rust CLI that queues coding-agent tasks (Claude Code, Codex) in Docker or Podman containers. Per task it:

1. copies the repo's non-ignored files (`src/git_operations.rs:403`);
2. runs the agent on a per-task internal network whose only exit is a Squid sidecar with a domain allowlist ([network-isolation.md](https://github.com/dtormoen/tsk-tsk/blob/main/docs/network-isolation.md));
3. fetches the agent's commits back as a branch (`src/git_operations.rs:189`).

## Where sanduk is already stronger

- **Credential.** tsk bind-mounts the host's `~/.claude`, OAuth credentials included (`src/agent/claude.rs:200`). sanduk `key-safe` and `sealed` keep the key on the host.
- **Egress.** Squid's `dstdomain` on a `CONNECT` sees the host name only. Any method and path to an allowed host passes, so each allowed domain is a channel out. `sealed` has no route out.
- **Capabilities.** tsk drops a list of eight (`src/docker/mod.rs:502`). sanduk drops ALL.
- **Model policy.** tsk has no model allowlist, token cap or budget.

## Ideas

### 1. Return work as a git bundle, not a writable repo mount

**Problem.** `-w` is a read-write bind mount. When it is a repo, the agent can write `.git/config` and `.git/hooks/`. Two keys run host commands from ordinary git use, before any review:

- `core.fsmonitor` runs on `git status` and `git diff`.
- `core.pager` and `diff.external` run on `git diff` and `git log`.

Hooks run on `git commit` and `git checkout`. None of this needs a route off the host, so `sealed` does not stop it.

The distinction from build-time risk: building agent code on the host runs agent code by choice, after review. These keys run it at review time.

**Design.** A `--repo PATH` option, alongside `-w`:

1. The host makes a copy: `git clone --no-local --no-hardlinks PATH COPY`. `--no-local` stops objects being hard-linked into the copy. Overlay uncommitted non-ignored files, as tsk does, or refuse a dirty tree; refusing is simpler.
2. The run mounts `COPY` at `/work`, read-write.
3. After the agent, a step inside the container runs `git bundle create /out/run.bundle <base>..HEAD`.
4. The host runs `git fetch run.bundle HEAD:refs/heads/sanduk/<run-id>`. A bundle is a pack plus refs, so no git config, hooks or `upload-pack` from the copy run on the host. `git bundle verify` comes first.

Fetching from `COPY` as a local remote also works. It runs `upload-pack` against an agent-written repo. Git honours `uploadpack.packObjectsHook` only from protected config (`git-config(1)`), but the bundle removes the question.

The copy leaves out ignored files, so `.env` and `target/` stay on the host.

**Alternatives.**

- **Read-only `.git` over a writable worktree** (`--mount repo/.git:/work/.git:ro`). Possible today, with no code. The agent cannot commit, and codex and claude both expect to. Nested mounts are untested on Apple's `container`.
- **Hash `.git/config` and `.git/hooks/` before and after; refuse on change.** Rejected. It detects rather than prevents. It must also enumerate every config key that runs a command: `.git/info/attributes` with a filter driver, `includeIf`, and so on.

**Costs.** One copy per run: disk and time scale with repo size. The fetch step needs a lock when two runs return to one repo. tsk takes a `flock(2)` (`src/git_sync.rs:81`).

**Open.** Is `-w` ever a real repo in practice, through pma or by hand? If it is only a scratch directory, this idea has no target. It is: the CHANGELOG's `--verify` example passes `-w repo`, and pma mounts a worktree. A worktree's `.git` is a file, which the agent can replace with a directory.

Done 2026-11-04 as `run --clone`, opt-in:

- The design above, with `--clone` modifying `-w` rather than a new `--repo PATH`.
- A dirty tree means uncommitted changes to tracked files only. Untracked files are not refused: the clone leaves them where they are.
- The return step disables hooks and `core.fsmonitor`, so a fsmonitor the agent set cannot hide a change from `add`.
- A failed return keeps the clone under `$XDG_STATE_HOME/sanduk/clones/`. The sweep does not reap it.
- No fetch lock. Each run fetches into its own ref.

### 2. Test what a sealed container can reach

**Problem.** `sealed` claims "no route off the host". The claim rests on `--internal` networks, tested only through a fake `Exec` (`crates/sanduk-container/tests/engine.rs`). No live test probes the network.

The host itself is reachable. The relay binds the network's gateway, so the gateway exists on the host.

Measured 2026-11-04, Docker 29.9.3 on Linux, iptables backend. From an `--internal` network with `--cap-drop ALL`:

| Target | Result |
|-|-|
| `1.1.1.1:443` | blocked |
| `169.254.169.254:80` | blocked |
| DNS lookup | blocked |
| host LAN address, port with a `0.0.0.0` listener | blocked |
| gateway `172.18.0.1`, same port | **reached** |

So any host service bound to `0.0.0.0` answers a sealed container: an `ssh` daemon, a database, a `llama-server`. A `llama-server` reached directly bypasses the relay's model allowlist and budget. Apple's `container` is unmeasured.

Fix options:

- Document it: "no route off the host; host services on all interfaces are reachable."
- Bind the relay to the gateway, and refuse a sealed run while another socket listens on `0.0.0.0` or the gateway address. That covers only what listens at start.
- Add a host `INPUT` rule per network that admits only the relay's port. Needs root and differs per firewall backend. tsk does not do this either.

Done 2026-11-04: the README qualifies the claim, and a sealed run on Linux warns about wildcard TCP listeners. A warning over a refusal: `sshd` on `0.0.0.0` is common, and a refusal would stop every run there. The `INPUT` rule stays open.

**Design.** A live test in `crates/sanduk-container/tests/live.rs`, or in `tests/` if it needs the relay. On an internal network, a container must fail to reach:

- `1.1.1.1:443` by IP;
- a name through DNS;
- `169.254.169.254:80`, the cloud metadata address;
- the host's LAN address;
- the gateway on a port where a test listener is bound to `0.0.0.0`.

It must reach the gateway on the relay's port. The last listed case fails today on Docker, so the test asserts the current result until one of the fix options lands.

tsk's equivalent: `scripts/network-isolation-test.sh`.

**Costs.** Needs a real engine and runs under `make live`. About one file.

### 3. One network per run

**Problem.** Every concurrent sealed run shares `sanduk-net`; key-safe runs share `sanduk-open` (`Mode::network` in `src/run.rs`). A container can open connections to another run's containers. Per-run tokens stop it using another run's relay. Nothing stops it reaching the other container directly. Agent containers listen on nothing by default, so the exposure is small unless an agent starts a server.

**Design.** Name the network `sanduk-net-<run-id>`. Add it to the run record and delete it after the containers. The sweep then reaps it with them. Per-run networks also remove the create race that `ensure_network` handles (`crates/sanduk-container/src/lib.rs:624`). `hold_network_up` already starts one holder per run.

**Alternative.** Docker only: create the shared network with `-o com.docker.network.bridge.enable_icc=false`. One flag, no lifecycle change. Container-to-container traffic stops; gateway traffic does not. Apple's `container` has no documented equivalent found.

**Costs.**

- Docker's default address pools hold about 31 bridge networks (`/16` from `172.17-31`, plus `192.168/16` in `/20`). Parallel runs plus other projects could exhaust them. Inference from Docker's defaults; check `daemon.json`.
- A killed run leaks a network until the next sweep.
- `clean` must delete by prefix, not by the two fixed names in `mode_networks`.

### 4. Package registries for sealed runs

**Problem.** A sealed `--verify` cannot fetch dependencies. Today the options are a kit that pre-bakes them, or `key-safe`, which opens all egress.

**Design.** The relay already filters by exact path. Extend it to forward GET-only requests to a fixed set of registry hosts:

| Tool | Client setting | Upstream |
|-|-|-|
| cargo | `[source.crates-io] replace-with`, sparse registry URL | `index.crates.io`, `static.crates.io` |
| pip, uv | `--index-url` | `pypi.org/simple`, `files.pythonhosted.org` |
| go | `GOPROXY` | `proxy.golang.org`, `sum.golang.org` |
| npm | `registry` | `registry.npmjs.org` |

Each client takes a base URL, so the run token can go in the path (`http://GATEWAY:PORT/<token>/crates/`). That avoids a per-tool auth scheme. The token is visible to the agent, as the relay token is now.

This is narrower than tsk's Squid: fixed hosts, GET only, path prefixes, no `CONNECT`. Data can still leave in a GET path, such as a crate name. It reaches only the registry's own logs.

**Alternatives.**

- **Squid sidecar, as tsk.** Rejected: any method and path to each allowed host.
- **A fetch container between agent and verify.** It has egress and no key, and mounts the workdir read-only. Rejected: the agent controls `Cargo.toml`, and a git dependency URL sends data to any host.
- **Pre-baked kits.** Already possible. The agent cannot add a dependency the kit lacks.

**Open.** Does any caller need this? pma's verify is the likely one.

## Considered, not adopted

- **Rebuild on agent upgrade** (tsk's `TSK_AGENT_VERSION`). Not needed: recipes pin the agent's version (`"@anthropic-ai/claude-code@2.1.272"` in `resources/recipes/claude.json`), kits pin by sha256, and the image tag follows the rendered Containerfile.
- **`--agent a,b` fan-out.** A shell loop over `run --stats-file` does it.
- **Prompt templates with `{{PROMPT}}`.** `--task-file` covers it.
- **Stack detection from `Cargo.toml` and the like.** Explicit kits are more predictable for unattended runs.
- **Task queue and chaining.** `tick` and `serve` cover scheduling.
- **The TUI.** Out of scope for a library-first tool.
