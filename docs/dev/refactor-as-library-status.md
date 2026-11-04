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

## Verified

On macOS, against fakes:

- `make test`: 342 pass, 0 fail, 6 ignored (the live tests).
- `make lint`: clean.
- CLI output parity: 8 scenarios run through the binary before the refactor and after it, against the fake docker in `crates/sanduk-cli/tests/fixtures/bin/docker`. Normalised stdout and stderr match, except for one reworded line: `--clone needs a task: a check alone changes nothing to fetch back`.

On Linux (Ubuntu, rootful Docker 29.9.3, `/bin/sh` is dash), 2026-11-04:

- `make test`: 336 pass, 0 fail, 6 ignored. The 7 tests under `#[cfg(target_os = "macos")]` in `crates/sanduk-sandbox/tests/policy.rs` do not build here; one Linux-only test is new.
- `make lint`: clean.
- `make live`: 6 pass, real Docker and a real TLS call. No container is left after either suite.
- CI for `031bb4e`: Ubuntu passes. macOS was still queued at the time of writing.

Two tests failed on Linux. Both were test bugs; the code was right:

- `an_image_whose_agent_cannot_write_the_workdir_says_why_as_data` (`tests/library.rs`). Under umask `0002` the workdir is `0775`, and `ensure_image` rightly does not refuse a group-writable workdir. The test now sets `0755`.
- `a_container_runs_its_command_with_the_workdir_mounted` (`crates/sanduk-container/tests/live.rs`), older than the refactor. Rootful Docker keeps host ownership, and root under `--cap-drop ALL` lacks `CAP_DAC_OVERRIDE`, so it could not write a mount owned by uid 1000. The test now passes `--user`, as a run does where `keeps_mount_owner` holds.

Linux paths macOS did not exercise:

| Path | Covered by |
|-|-|
| The uid check, `Detail::AgentUid` | `an_image_whose_agent_cannot_write_the_workdir_says_why_as_data`, under `Fake` |
| The host-listener warning | `a_sealed_run_names_host_ports_listening_on_every_address` (`crates/sanduk-cli/tests/cli.rs`), new, Linux only. It binds a port on every address and one on loopback; the warning must name the first only. It fails when the LISTEN check in `wildcard_listeners` is broken. Only sealed mode reaches the warning: `key-safe` has egress |
| `REAP` under dash | the three `a_reaper_*` tests in `crates/sanduk-container/tests/runs.rs`, with a stub CLI |
| A killed run's process group | `a_killed_run_has_its_container_deleted_without_waiting_for_another_run`, with the fake docker |
| Landlock | `crates/sanduk-sandbox` tests, unchanged by the refactor; they pass |

## Not verified

- The reaper against real Docker. Its tests use a stub CLI; `make live` does not start one.
- The CLI's rebuild hint (`with_rebuild` in `crates/sanduk-cli/src/run.rs`) on Linux with a real image of another uid.
- The Apple engine's network holder path. No test covers it.
- Build time and binary size against pma. Only crate counts were measured (`cargo tree -e normal`): `sanduk` 89, `sanduk-cli` 116.
- The API from pma. Nothing calls it yet.

## Next

1. Commit the Linux test fixes; check the macOS CI job.
2. Try the API from pma on a branch.
3. Decide on publishing.

## Known gaps

- Under `Fake`, the reaper still calls `docker` from `PATH`. On a Linux host with a running daemon, the calls reach it. A record left behind by a test, after a failed fake delete or a killed process, sends `docker stop` and `docker rm` to the real CLI. The names do not exist, so this is harmless, but it is not isolated. The fix: let `Fake` name a stand-in CLI for the reaper.
- `sanduk build` and `run` lost Docker's live progress display: build output is a pipe to the observer now. To get it back for the CLI only, keep the attached `Engine::build_image` for `sanduk build`.
