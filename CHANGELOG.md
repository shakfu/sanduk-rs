# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- The library is `sanduk-core`; the `sanduk` package is the binary alone, so `cargo install sanduk` still installs the CLI. A library named `sanduk` with the binary in `sanduk-cli` was the first split; it would have made `cargo install sanduk` fail at 0.2.0, for a library with no other user. A run is `Sanduk::plan`, `Plan::start`, the caller's own process from `Live::command`, then `Live::finish`, which returns an `Outcome`. The caller owns the process, so pma keeps its watchdog, log and kill path; a blocking `run()` would have needed its own timeout and cancellation. `clone_into` takes the clone's path from the caller. Errors carry an `ErrorKind`, and name no CLI flag: one about a single option names it in `Error::option` as its `Options` field, and the CLI prints the flag. `Error::detail` holds the facts of an error a caller can fix in its own terms: `Detail::AgentUid`, from which the CLI prints the `sanduk build` command. Image builds stream to the observer as `Note::Build`, as `--verify` output does as `Note::Output`; `sanduk build` and `run` print both to stderr, so the engine's terminal progress display is gone.

  Nothing in the library is process-wide: directories are a `Dirs` value, notes go to an `Observer`, and signals are the CLI's. `serde_json/preserve_order` and `toml/preserve_order` are gone. Cargo unified them into every crate linking sanduk, and pma's export round-trip broke on the key order. The relay rewrites a body from `RawValue`s, so untouched values go upstream byte for byte. Recipe and agent `env` keep their file's order, which image tags hash. `runs` moved to `sanduk-container`. Deviations from the plan are recorded in [docs/dev/refactor-as-library.md](docs/dev/refactor-as-library.md).

- **Breaking, `sanduk-container` 0.2.0:** the container prefix is a required `Prefix`, with no default. `Engine::new`, `Engine::with_exec`, `Engine::get`, `runs::sweep` and `testing::Fake::engine` take it, and `Reaper::start` takes the engine. A default of `sanduk-` let a second program compile with sanduk's prefix and sweep sanduk's containers. A prefix must start with a letter and end with `-`: an empty one makes a sweep list every container on the host.

- Each crate has its own version. `sanduk-core` and `sanduk` share the workspace's, 0.2.0. `sanduk-container` is 0.2.0, and `sanduk-sandbox` stays 0.1.0, unchanged since its release. One shared version would release a crate that did not change.

### Added

- `sanduk_container::testing::Fake`, behind the `test-util` feature: a Docker engine with no daemon. `Sanduk::engine` takes it, so a caller can test `plan`, `start` and `finish` without Docker.

- `run --verify CMD` runs CMD with `sh -c` after the agent finishes, in a second container with the same image, mounts and network, no credential, and the rest of `--timeout`. Its status goes to `--stats-file`, not the exit status: one status cannot tell a failed edit from a failed test. With no task, `run --verify` runs only the check, so a base tree is checked in the container a run with the same flags would use. A mode of `run` over a separate verb: the environment is identical by construction.

  ```sh
  sanduk run "fix the parser" -w repo --mode sealed --verify "make test" --stats-file s.json
  sanduk run -w base --mode sealed --verify "make test" --stats-file base.json
  ```

- Toolchain kits, so a `--verify` check can build what it tests: `build` (make, gcc, pkg-config), `rust` (Rust 1.98.1 with clippy and rustfmt through rustup 1.29.1), `go` (Go 1.27.1) and `uv` (uv and uvx 0.12.20). Each download is pinned by sha256; rustup checks the toolchain it fetches. Each links into `/usr/local/bin` rather than setting `PATH`, which a second kit's `PATH` would replace, so they stack: `--kit rust --kit uv`. Cargo's home is writable by the agent's uid, as in the official rust image, so a check can fetch crates.

- `--stats-file` also records `mode`, the relay's `requests`, `rejected`, `spent` and `unpriced` as numbers, and `verify`: `command`, `exit`, `ok`, `seconds`, `timed_out`, `error`.

- A sealed run on Linux warns when a host TCP port listens on every address. Measured on Docker 29.9.3: an internal network blocks the internet, DNS, cloud metadata and the host's LAN address, but the gateway is a host address, so such a service answers the container past the relay. A warning over a refusal: most hosts run `sshd` on `0.0.0.0`, and a refusal would stop every run. `make live` now probes all five cases. See [docs/dev/tsk-tsk.md](docs/dev/tsk-tsk.md).

- `run --clone` runs the agent on a clone of the `-w` repository and fetches its commits back as branch `sanduk/<run>`; edits it left uncommitted are committed first. With the repository bind-mounted, the agent can write `.git/config`, and a `core.fsmonitor` there runs on the host's next `git status`, before any review. The commits return as a git bundle made inside a container, so host git never reads the clone's config or hooks. A failed return keeps the clone and names it in `--stats-file` as `kept`; `branch` names the branch. Opt-in: the clone costs a copy of the repository per run, and refuses a tree with uncommitted changes to tracked files. pma does not pass it yet, so its runs still mount the worktree; making `--clone` its default needs a change in pma.

  ```sh
  sanduk run "fix the parser" -w repo --clone --mode sealed --stats-file s.json
  ```

### Fixed

- A killed `run` has its containers deleted when it dies, not when the next `run` starts. SIGKILL runs no teardown, and the engine's CLI runs in its own process group, so the agent's container kept running with the worktree mounted until some later run's sweep. A caller that kills sanduk's process group, as pma does at its deadline, left it running every time. Each run now starts a reaper in its own process group, blocked on a pipe the run holds; the pipe closing, at teardown or at death, starts the delete. The reaper is `sh` and the engine's CLI, reading a names file beside the run record, so it works under any host binary. A detached reaper over a container-side watchdog: the engine's CLI and the image stay unchanged, and every engine gets it.

- A sweep read a record naming another prefix's container as already gone, and deleted the record. It keeps it now.

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
