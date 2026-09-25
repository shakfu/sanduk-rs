# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0]

### Added

- A Rust rewrite of [sanduk](https://github.com/shakfu/sanduk) 0.3.1, as a workspace of three crates: `sanduk` (the CLI, agents, recipes, kits, providers, the relay and assistants), `sanduk-container` (the container engines) and `sanduk-sandbox` (a per-process write sandbox). Every Python command and flag is kept. Rust over Go because minima and pma are Rust and link the crates in-process. The port and its checks against Python are recorded in [docs/dev/rewrite.md](docs/dev/rewrite.md).

- `sanduk-sandbox` confines a child process's writes to one directory, with Landlock on Linux and Seatbelt on macOS. It is minima's `--sandbox`, moved here so minima, pma and timu share one implementation.

- Agents are TOML files, not code: eight ship, and a user agent is a file in `~/.config/sanduk/agents/` or a path given to `--agent`. Stream formats stay code, since each is a stateful reader; a new agent that speaks an existing format needs none. See [docs/agents.md](docs/agents.md).

- `run` checks that the container can reach the relay before the agent starts, in `key-safe` and `sealed` modes. A host firewall that dropped the connection used to leave the agent hung until `--timeout`, with no error; the run now stops within 5 seconds and names the likely cause. On Apple's engine the check runs in the holder container, so it costs no VM. See [docs/dev/firewall-considerations.md](docs/dev/firewall-considerations.md).

- `make diagrams` renders `docs/media/*.d2` with d2's TALA layout; the README shows the architecture diagram.

### Changed

Relative to Python sanduk 0.3.1. Built images and the assistants database are shared: either implementation uses what the other built or wrote.

- No plugin entry points, for agents, recipes or kits. A user's own is a file in the config directory or a path.

- A request body over 64 MiB is refused with 413. The relay reads a body whole, because the model allowlist and token cap read its JSON, and Python's had no bound: a container holding the run token could fill host memory.

- The shipped agents, recipes and kits are embedded in the binary and written to `~/.cache/sanduk/resources/` on first use, checked against the binary each time.

- The agent runs in its own process group, so a timeout or signal also kills whatever the engine's CLI started. Not with `--stdin`, where a background group reading the terminal would be stopped by SIGTTIN.

### Fixed

Bugs in Python sanduk 0.3.1, fixed in the port.

- opencode's and pi's config named the default key variable whatever `--agent-key-env` said, so the agent looked for its credential in a variable the container did not set.

- codex's `-c` values were spliced into TOML unquoted; they are now quoted.

- A run's record is released only once its containers are deleted. A failed delete used to release it anyway, so the next sweep never retried, and the container kept whatever environment it had.

- On Apple's engine, `image_exists` missed an image pulled by its full name: the engine lists `docker.io/library/alpine` as `alpine`, and the check matched suffixes. The same match took `xsanduk` for `sanduk`.
