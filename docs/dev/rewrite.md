# Rewrite plan

Status: 2026-09-25. Steps 1-5 done: sanduk-rs does everything Python sanduk 0.3.1 does.

## Why Rust

Three projects duplicated or needed the same containment code:

- minima (Rust): Landlock and Seatbelt around its `bash` tool.
- timu (Python): Seatbelt only, in `src/timu/sandbox.py`, with a stricter policy.
- pma (Rust): runs `verify` on the host, uncontained, after an agent has edited the build files (`pma/docs/dev/using-containers.md`).

Two of the three are Rust and link a crate in-process. A Python sanduk could serve them only as a subprocess, at 10-150 ms per call and a Python install wherever they run. Go was the earlier choice in sanduk's `TODO.md`, but a Go library cannot be linked into minima or pma.

## Layout

```
sanduk-rs/
  Cargo.toml                   # workspace; becomes the `sanduk` package in step 4
  crates/sanduk-sandbox/       # kernel policy per process
  crates/sanduk-container/     # container engines (step 2)
```

Only code with a second consumer gets its own crate. The relay, agents, providers, recipes, kits and scheduler stay in the `sanduk` binary crate.

## Order

1. **`sanduk-sandbox`.** Done. Moved from `minima/src/tools/bash.rs` and `src/tools/mod.rs`; minima depends on it by path. Tests: 24 in the crate. minima's 171 and `scripts/test_sandbox.py` (22/22) pass on macOS. Linux compiles and lints clean; its tests have not run here (see below).
2. **`sanduk-container`.** Done. Ported from `runtime.py`, with its engine-level tests from `test_runtime.py`; the tests that go through `cli.parse_args` wait for step 4. 58 tests with a fake `Exec`, and 4 live tests (`make live`) that pass against Apple's `container` 1.2. Docker is not installed here, so its live run is unmeasured. Changes from Python:
   - Nothing prints. `ensure_network` returns whether it created the network; `delete_image` and `delete_network` return whether they deleted.
   - `destroy` fails when the delete fails, and takes no `keep`. A caller that keeps a run's record until teardown succeeds closes the sanduk `TODO.md` item on teardown releasing records unconfirmed.
   - Apple's `image_exists` compares whole names. Python's suffix match misses a pulled `docker.io/library/alpine:3.20`, which the engine lists as `alpine`, and matches `xsanduk` for `sanduk`.
   - `container system status` printing "not running" is refused even at exit 0. Measured: it exits 1 when stopped, so this is defensive.
3. **Relay.** Done. `proxy.py` and `providers.py` are `sanduk::relay` and `sanduk::providers`, in the root `sanduk` package, which gains its binary in step 4. tokio and hyper, rustls with `ring` (no cmake) and the system trust store. 37 unit tests; 46 relay tests against a fake upstream, ported from `test_proxy.py` and `test_providers.py`; 1 live test (`make live`) that relays to `api.anthropic.com` over TLS with an invalid key and gets Anthropic's own 401. Changes from Python:
   - Request bodies are not streamed, as this plan first said. The policy and the body log need the whole JSON. They are buffered up to `max_body` (64 MiB) and refused with 413 above it. Python read any size, so a container holding the run token could fill host memory.
   - The upstream call runs in a spawned task. A client that leaves before the headers, mid-stream or at the end cannot stop the charge; Python needed three separate fixes for those three cases.
   - An upstream that breaks off mid-stream ends the client's response with an error, not a clean terminator, so a cut stream cannot pass for a whole one.
   - An empty run token never matches. Python's `compare_digest("", "")` would admit an empty header under an empty token; the CLI never configures one.
   - A body that is JSON but not an object is refused with 400. Python raised on `payload.get` and answered 500.
   - Methods other than GET, POST, PUT and DELETE are refused as `forbidden`. Python answered 501.
   - `is_loopback` accepts a bare IPv6 loopback in any spelling (`0:0:0:0:0:0:0:1`); Python split it at the last colon.
   - Found by the live test: the provider's host has no port, and a socket address needs one. Python's `HTTPSConnection` defaulted to 443.

   Found while porting: the budget concurrency test's helper took the result lock before making the call, so its five calls ran one at a time and would have passed without the gate. Fixed here, and checked by removing the gate: the test then fails. The Python helper appends after the call, so it does not have this bug.
4. **CLI, agents, recipes, kits.** Done. `run`, `build`, `shell`, `ps`, `stop`, `clean`, `destroy`, `system` and `list`; the assistant verbs are step 5. Checked three ways against Python sanduk 0.3.1:
   - Recipes: every shipped recipe renders a byte-identical Containerfile, so image tags match and the two share built images (`tests/golden/*.Containerfile`).
   - Agents: 936 agent x provider x option x endpoint cases give the same check verdict, wiring and argv, and 31 streams the same trace and outcome (`tests/golden/agents.json`).
   - The CLI: 32 command lines, dry runs, refusals, `build --dry-run` and `list`, print the same stdout and exit alike.

   22 end-to-end tests run the binary against a stand-in docker (`tests/fixtures/bin/docker`). A real sealed run passed on Apple's `container`: minima built from its recipe, reached llama-server (Qwen3-4B) only through the relay, and wrote its report; the sweep reaped an interrupted run's containers first.

   Agents are configuration (`resources/agents/*.toml`, `docs/agents.md`); stream formats stay code. Changes from Python:
   - No plugin entry points. A user agent is a file in `~/.config/sanduk/agents/` or a path. Recipes and kits lose their entry-point lookup too.
   - opencode's and pi's config blobs name the key variable the run uses. Python wrote the default whatever `--agent-key-env` said, so the agent read an unset variable.
   - codex's `-c` values are quoted as TOML; Python spliced them.
   - hax's refusal reads `--allowed-tools has no hax equivalent`.
   - A run's record is released only once its containers are deleted, so a failed delete is retried by the next sweep. Closes the TODO.md item on teardown releasing records unconfirmed.
   - SIGINT, SIGTERM and SIGHUP are caught for the whole run, not only while the agent runs. The agent runs in its own process group, so a timeout or signal kills anything the engine's CLI started (not with `--stdin`, where a background group would be stopped by SIGTTIN).
   - The shipped agents, recipes and kits are embedded in the binary and written to `~/.cache/sanduk/resources/<hash>/` on first use, checked against the binary each time.

   Found on the live run: the macOS firewall prompts for the unsigned binary the first time the relay listens, and `firewall_warning` detects only an explicit Block entry. Deferred; see [firewall-considerations.md](firewall-considerations.md).
5. **Assistants and scheduler.** Done. `assistant`, `tell`, `tick`, `serve`, `outbox`, `approve`, `reject` and `runs`, on rusqlite with its bundled SQLite. The schema and its migrations are Python's: on one `assistants.db`, with both binaries writing in turn, all eight listings (`assistant list` and `show`, `outbox` and its filters, `runs`) print identically. 34 tests ported from `test_assistants.py`, with the run command passed to `wake` rather than patched in; 2 end-to-end CLI tests, one of which ticks a real wakeup through `run` and the stub engine, then approves and delivers its report.

   The CLI tests that start a run refuse to run as root, by design: an image built for uid 0 would run its agent as root, which `run` never does. CI runs them as its ordinary user.

Python sanduk got fixes only while the port was under way. Its Critical items in `TODO.md` were fixed in the port: `--log-dir` inside a mount is refused, and log files are opened `O_EXCL|O_NOFOLLOW`.

## Open decisions

- **timu's policy.** timu denies network, reads under `$HOME`, and `.git` writes; minima allows all three. `Policy` expresses only minima's today. On Linux:
  - "no network" needs a network namespace, since Landlock ABI 4 limits TCP by port only. Docker's default seccomp profile blocks an unprivileged `unshare` ([moby#42441](https://github.com/moby/moby/issues/42441)).
  - A `.git` hole in the root rule cannot be expressed: Landlock unions the rules along a path (`minima/docs/dev/root-sandbox.md`, "Protected paths").
- **A binary for non-Rust callers.** timu needs `sanduk exec [policy] -- cmd`. It belongs to the `sanduk` CLI in step 4, or earlier as a small binary in `sanduk-sandbox`.
- **Distribution.** minima depends on `sanduk-sandbox` by path, so its CI checkout and `cargo publish` fail until the crate is on crates.io or minima uses a git dependency.
- **Agent plugins.** Python sanduk finds agents through entry points. Rust has no runtime equivalent: either compile agents in, or make them records with a fixed set of stream parsers, as pma does.

## Measured

- Apple's `container` guest kernel, 6.18.15, has `# CONFIG_SECURITY_LANDLOCK is not set`; `landlock_create_ruleset` returns `ENOSYS`. `sanduk-sandbox` refuses there, so minima's `--sandbox` cannot start inside a sanduk container on macOS. The container is the boundary in that case.
