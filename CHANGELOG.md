# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `run --verify CMD` runs CMD with `sh -c` after the agent finishes, in a second container with the same image, mounts and network, no credential, and the rest of `--timeout`. Its status goes to `--stats-file`, not the exit status: one status cannot tell a failed edit from a failed test. With no task, `run --verify` runs only the check, so a base tree is checked in the container a run with the same flags would use. A mode of `run` over a separate verb: the environment is identical by construction.

  ```sh
  sanduk run "fix the parser" -w repo --mode sealed --verify "make test" --stats-file s.json
  sanduk run -w base --mode sealed --verify "make test" --stats-file base.json
  ```

- Toolchain kits, so a `--verify` check can build what it tests: `build` (make, gcc, pkg-config), `rust` (Rust 1.98.1 with clippy and rustfmt through rustup 1.29.1), `go` (Go 1.27.1) and `uv` (uv and uvx 0.12.20). Each download is pinned by sha256; rustup checks the toolchain it fetches. Each links into `/usr/local/bin` rather than setting `PATH`, which a second kit's `PATH` would replace, so they stack: `--kit rust --kit uv`. Cargo's home is writable by the agent's uid, as in the official rust image, so a check can fetch crates.

- `--stats-file` also records `mode`, the relay's `requests`, `rejected`, `spent` and `unpriced` as numbers, and `verify`: `command`, `exit`, `ok`, `seconds`, `timed_out`, `error`.

### Fixed

- A killed `run` has its containers deleted when it dies, not when the next `run` starts. SIGKILL runs no teardown, and the engine's CLI runs in its own process group, so the agent's container kept running with the worktree mounted until some later run's sweep. A caller that kills sanduk's process group, as pma does at its deadline, left it running every time. Each run now starts `sanduk reap` in its own process group, blocked on a pipe the run holds; the pipe closing, at teardown or at death, starts the delete. A detached reaper over a container-side watchdog: the engine's CLI and the image stay unchanged, and every engine gets it.

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
