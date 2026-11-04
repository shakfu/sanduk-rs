# sanduk as a library: status and next steps

State as of 2026-11-04. The plan is [refactor-as-library.md](refactor-as-library.md); its "As built" section records where the build departs from it. This file is the handoff: what is verified, and what to run next.

## Built

Steps 1-7 of the plan:

1. No process-wide state: `Dirs`, `Observer`, signals in the CLI.
2. `serde_json/preserve_order` and `toml/preserve_order` dropped.
3. The reaper is `sh` and the engine's CLI.
4. The run is `sanduk::run`: `Plan`, `Live`, `Outcome`.
5. `ErrorKind`, `Error::option`, `Error::detail`.
6. Crates split: `sanduk-cli` holds the binary and assistants; `runs` is in `sanduk-container`.
7. `sanduk_container::testing::Fake`, behind `test-util`.

Step 8, publishing, is not done: it is outward-facing and not reversible.

## Verified, on macOS only

- `make test`: 342 pass, 0 fail, 6 ignored (the live tests).
- `make lint`: clean.
- CLI output parity: 8 scenarios run through the binary before the refactor and after it, against the fake docker in `crates/sanduk-cli/tests/fixtures/bin/docker`. Normalised stdout and stderr match, except for one reworded line: `--clone needs a task: a check alone changes nothing to fetch back`.

## Not verified

- Linux. The CI run of the last commit (`6b65e6a`) passed on Ubuntu, but it predates the refactor.
- A real engine. Every test so far uses a fake: the shell-script docker for the CLI, `Fake` for the library. `make live` has not been run. The Apple engine's network holder path is covered by no test.
- Build time and binary size against pma. Only crate counts were measured (`cargo tree -e normal`): `sanduk` 89, `sanduk-cli` 116.
- The API from pma. Nothing calls it yet.

## Next, on Linux

1. Check that the work is in the checkout: `crates/sanduk-cli/`, `src/run.rs`, `src/observe.rs`, `src/clone.rs` exist, and `src/cli/` does not.
2. `make test`, then `make lint`.
3. `make live`. Needs Docker and the network.

Paths that macOS did not exercise:

- The uid check. Docker on Linux keeps host ownership on a bind mount: `Plan::ensure_image`, `Detail::AgentUid`, and the CLI's rebuild hint (`with_rebuild` in `crates/sanduk-cli/src/run.rs`).
- The host-listener warning. A sealed run reads `/proc/net/tcp`, which macOS lacks: `preflight::host_listener_warning`.
- The reaper script, under Linux's `sh`: `REAP` in `crates/sanduk-container/src/runs.rs`.
- Landlock, in `sanduk-sandbox`. That crate is unchanged, but it has not been tested since the refactor.

Then: commit, push for CI, try the API from pma on a branch, and decide on publishing.

## Known gaps

- Under `Fake`, the reaper still calls `docker` from `PATH`. A record left behind by a test, after a failed fake delete or a killed process, sends `docker stop` and `docker rm` to the real CLI. The names do not exist, so this is harmless, but it is not isolated. The fix: let `Fake` name a stand-in CLI for the reaper.
- `sanduk build` and `run` lost Docker's live progress display: build output is a pipe to the observer now. To get it back for the CLI only, keep the attached `Engine::build_image` for `sanduk build`.
