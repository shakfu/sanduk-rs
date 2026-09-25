# Agents

An agent is a TOML file. It tells sanduk which image carries the program, how to drive it headlessly, where it reads its endpoint and credential, and which stream format it answers in. Eight ship: `claude`, `codex`, `hax`, `hermes`, `minima`, `opencode`, `pi` and `prime` (`resources/agents/`). `sanduk list agents` shows them.

A new agent needs no code if it speaks one of the stream formats below. Put the file in `~/.config/sanduk/agents/NAME.toml` to use it by name, or pass its path: `--agent ./mine.toml`. A user file cannot take a shipped agent's name.

## A minimal agent

```toml
name = "mine"
image = "my-agent:latest"
containerfile = "Containerfile.mine"   # relative to this file
protocols = ["openai-chat"]
stream = "minima"

argv = ["--json", { args = ["--model={model}"] }, "{task}"]

[wire]
key_env = "MY_API_KEY"
base_url_env = "MY_BASE_URL"
base_url = "{endpoint}{api_prefix}"
```

## Keys

| Key | |
|-|-|
| `name` | the `--agent` value; must equal the file name in a catalogue |
| `recipe` | the recipe that builds the image. Or `image` and `containerfile`, which take no kits |
| `skills_dir` | where the agent reads skills, relative to its home. Absent: kits carrying skills are refused |
| `instructions_file` | where it reads user-level instructions. Absent: a recipe's `instructions` are refused |
| `protocols` | wire protocols it speaks, in preference order: `anthropic-messages`, `openai-chat`, `openai-responses`. A run speaks the first one the provider serves |
| `stream` | `claude`, `codex`, `hax`, `hermes`, `minima`, `opencode` or `pi` |
| `argv` | the command after the image |
| `[wire]` | `key_env`, `base_url_env`, `base_url`, and `[wire.env]` for fixed settings |
| `[check]` | `require` and `refuse` name options; `relay` is the reason the agent cannot run behind the relay |
| `[vars.NAME]` | a value looked up by the run's `protocol` or its `provider` name; `"*"` is the default |

## Templates

`{name}` is replaced by a variable; `{name|upper}` upper-cases it and `{name|json}` writes it as a JSON string literal, which is also a TOML string. `{{` and `}}` are literal braces. A misspelt name is refused when the file is read.

| Variable | |
|-|-|
| `task` | the prompt, with the report instruction appended |
| `model`, `effort`, `max_turns`, `permission_mode`, `allowed_tools` | the run's flags; unset when not given |
| `bare`, `relayed` | set when `--bare` is given, or the mode keeps the key on the host |
| `provider`, `provider_key_env`, `provider_base_url_env`, `api_prefix` | the provider's |
| `root` | the relay, or `--base-url`; unset otherwise |
| `endpoint` | `root`, or the provider's own `scheme://host` |
| `protocol` | the protocol this run speaks |
| `key_env`, `base_url_env`, `base_url` | the wiring, for `argv` and `[wire.env]` |

In `argv`, a plain string must render. A group is dropped whole when a variable it reads is unset:

```toml
argv = [
  { args = ["--model", "{model}"] },                       # dropped without --model
  { when = "bare", args = ["--bare"] },                    # only with --bare
  { args = ["--permission-mode", "{permission_mode}"], else = ["--dangerously-skip-permissions"] },
  "{task}",
]
```

An entry of `[wire.env]` whose template reads an unset variable is left out. A value can also be a JSON document, written as TOML and serialised compactly; keys and strings in it are templates, and escaping is the serialiser's:

```toml
[wire.env.OPENCODE_CONFIG_CONTENT.json.provider.sanduk]
npm = "{driver}"
options = { baseURL = "{base_url}", apiKey = "{{env:{key_env}}}" }
```

The credential reaches the container under `key_env`, by name only. Nothing secret belongs in `argv`: it is visible to `inspect`.

## Stream formats

Each format is a reader in `src/agent/stream.rs`: a small state machine that traces the run and reports its outcome. A format is code rather than configuration because each one tallies across events, and some clear an earlier failure on a retry or read prose line by line. A new format is a new reader there.
