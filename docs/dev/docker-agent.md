# Docker Agent

Status: 2026-10-10. Desk evaluation only: nothing is built or measured. One decision: `sbx` is rejected as an engine (see Options). Claims come from Docker's docs and the GitHub API unless marked as inference or third-party.

Sources: <https://docs.docker.com/ai/docker-agent/>, <https://github.com/docker/docker-agent>.

## What it is

A framework for teams of agents, defined in YAML. A root agent delegates to `sub_agents`; each has its own model, instructions and `toolsets` (`filesystem`, `shell`, `mcp`). Formerly `cagent`. Ships with Docker Desktop 4.63+, or standalone.

- Go; Apache-2.0.
- v1.150.0, released 2026-10-09 (GitHub API). The release rate suggests pinning is required.
- Configs can be pushed to and pulled from OCI registries.

Three of its features matter to sanduk:

1. It is an agent sanduk could run.
2. Its `--sandbox` mode overlaps with sanduk's sealed mode.
3. Its `serve` modes (ACP, MCP, A2A) expose agents to other clients.

## As a sanduk agent

What fits, from the [CLI reference](https://docs.docker.com/ai/docker-agent/reference/cli/) and [models reference](https://docs.docker.com/ai/docker-agent/configuration/models/):

| sanduk needs | Docker Agent has |
|-|-|
| headless run | `run --exec [config] [message...]` |
| a stream to read | `--json`: newline-delimited JSON |
| no approval prompts | `--yolo`, or `--safety autonomous` |
| endpoint from the environment | `base_url: ${env.VAR}` |
| key under a named variable | `token_key: VAR` |
| working directory | `--working-dir` |

A custom `base_url` implies `bypass_models_gateway`: calls go straight to that URL. That is what the relay needs.

What it costs:

- **A stream reader.** Its JSON matches none of the formats in `src/agent/stream.rs`. A new reader is code, not configuration (`docs/agents.md`, "Stream formats").
- **A config file in the image.** The YAML has to ship with the recipe. Not checked: whether it can come from an environment variable, as opencode's does.
- **Anthropic endpoints.** The docs show `base_url` only for OpenAI-compatible providers. Start with `protocols = ["openai-chat"]` until Anthropic with a custom `base_url` is tested.
- **Pinning.** A recipe pin on a project that releases about daily.

Its multi-agent teams, todo lists and memory need nothing from sanduk. They run inside the container like any other agent's internals.

## `--sandbox` mode

`run --sandbox` runs the agent under Docker Sandboxes (`sbx`). `--template` picks the image; `docker agent sandbox allow|deny|list` manages the host allowlist.

Docker documents `sbx` at <https://docs.docker.com/ai/sandboxes/>; the points below come from third-party write-ups ([innoq](https://www.innoq.com/en/blog/2026/07/trust-but-sandbox/), [Vaughan](https://codex.danielvaughan.com/2026/04/13/docker-sandboxes-codex-cli-microvm-isolation/)) and are not yet checked against it.

- **Sign-in.** `sbx` requires a Docker account. `sbx login` runs a browser OAuth flow; every command except `login` and `logout` fails without a valid session ([get started](https://docs.docker.com/ai/sandboxes/get-started/), [sign-in enforcement](https://docs.docker.com/ai/sandboxes/governance/sign-in-enforcement/)). Organisation admins can restrict which accounts may sign in.
- **Isolation.** One microVM per sandbox, with its own kernel and Docker daemon. The agent is root inside it.
- **Credentials.** A host-side proxy injects the real key; the agent sees a placeholder. Registry credentials are an exception: they are written into the VM.
- **Network.** Egress is denied by default and limited to HTTP and HTTPS. DNS goes through the proxy.
- **Workspace.** Mounted live: the agent edits the host's files.
- **Hosts.** Apple Silicon macOS, Windows 11 x86_64, Ubuntu 22.04+ with KVM.
- **Status.** Beta; one source says the license is not final.

This is sanduk's sealed design with a VM as the boundary. It competes with sanduk; it does not plug into it. Running `--sandbox` inside a sanduk container would add a second sandbox and nothing else.

The sign-in requirement rules `sbx` out as a sanduk dependency. sanduk needs no account with any vendor. A browser login cannot run on a headless host, and `tick`, `serve` and pma's worker would fail once the session expires. Inference: session lifetime is not documented in the sources found.

## Comparison with sanduk sealed mode

| | sanduk `sealed` | `sbx` |
|-|-|-|
| Boundary | container on Apple `container` or Docker | microVM |
| Key | held by the relay on the host | injected by a host proxy |
| Egress | none: internal network, relay only | HTTP and HTTPS allowlist |
| Model policy | model allowlist, token cap, budget | not described |
| Workspace | per `run` options | live mount |
| Linux without KVM | yes | no |
| Vendor account | none | Docker account, browser sign-in |

The relay's model allowlist and budget have no counterpart in the sources found. Unverified.

## Options

1. **Add `docker-agent` as a ninth agent.** One TOML file, one recipe, one stream reader.
2. **Add `sbx` as an engine in `sanduk-container`.** Rejected: it requires a Docker account sign-in, which unattended runs cannot provide. Its proxy and policy would also duplicate the relay.
3. **Use `sbx` as a reference design only.** Compare its credential proxy and egress policy with the relay's.
4. **Do nothing.**

## Open questions

- Does anyone need Docker Agent behind sanduk? Its value is multi-agent teams, which no shipped agent offers.
- Does the YAML config allow every tool to be set from the environment, so one recipe serves every provider?
- Is the `--json` event schema stable across releases? At this release rate, a reader may break often.

## Next step, if pursued

1. Run `docker agent run --exec --json --yolo` against llama-server. Record the event stream.
2. Point `base_url` at the relay with `provider: openai`. Then try `provider: anthropic`.
3. Write `resources/agents/docker-agent.toml` and a recipe. Add a reader to `src/agent/stream.rs` with a recorded fixture.
