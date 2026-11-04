# sanduk as a library

Status: proposal, 2026-11-04. Nothing here is built. pma is the main intended consumer, so its needs set the API. Claims about pma cite its source at `main`. Build-cost figures not marked as measured are inference.

## Problem

sanduk is meant to be a sandboxing library with an illustrative CLI. Today the run itself lives in the CLI: `cli::run::run` (`src/cli/run.rs:973`, about 400 lines) takes clap's `RunArgs` and does everything:

- validation;
- the image;
- the network and holder;
- the relay;
- the clone;
- the agent;
- verify;
- the clone's return;
- teardown;
- the stats file.

A caller cannot run an agent in a container without going through argv and stdout. pma's `sanduk` worker does exactly that: it builds a command line (`pma/crates/pma-core/src/worker.rs:548`) and parses the agent's stream back out of the log. Each new option, `--clone` included, needs a template change, a placeholder and a settings migration in pma.

pma tried linking `sanduk` once and reverted (`pma/docs/dev/sanduk-integration.md`, section 3). These are the blockers it found, and they set what the library must not do:

| Blocker | Where | Effect on a linking caller |
|-|-|-|
| `serde_json/preserve_order` | `Cargo.toml` | Cargo unifies features: every `serde_json::Map` in pma serialised in insertion order, and `pma project export` failed its round-trip test |
| `toml/preserve_order` | `Cargo.toml` | Same mechanism for `toml::Table`. pma uses `toml_edit`, so it is not affected today |
| Process-wide signal handlers | `src/agent/launch.rs:15`, `:29` | One global `CAUGHT`. Two runs on two threads restore handlers out of order |
| Thread-local directory overrides | `src/util.rs:27` | State depends on which thread calls |
| Notes to stderr, trace to stdout | `util::note`, 32 print sites in `cli/run.rs` | Parallel runs interleave; pma's TUI breaks |
| Reaper started as `current_exe() reap` | `src/runs.rs:248` | In a library `current_exe()` is pma, which has no `reap` subcommand: no reaper, so a killed run leaks its container |
| `rusqlite` bundled, for assistants | `Cargo.toml` | `libsqlite3-sys` declares `links = "sqlite3"`, so only one version may be in a graph. pma pins its own; the two must move in lockstep |

Measured by pma against sanduk 0.1.0, linking everything: release build 45 s -> 55 s, binary 10.9 MB -> 18.9 MB, 111 -> 181 crates.

## How pma runs an agent

This shapes the API more than anything else:

- **pma owns the agent's process.** `spawn_guarded` (`pma/crates/pma-core/src/agent.rs:163`) starts each worker under a shell watchdog, as a process group. stdout and stderr go to a log, and the group is killed when pma's end of a pipe closes. Timeouts, `pma job stop` and pma's own death all go through it.
- **pma parses the agent's output itself**, with its own parsers (`claude-json`, `text-tail` and others). sanduk's stream reader would duplicate them.
- **One process per dispatch.** A dispatch runs as a job, a separate `pma` process (`pma/crates/pma-core/src/jobs.rs:277`). A batch runs agents on threads inside it, so two runs can share one process.
- **Interactive runs live in a herdr pane.** The job process waits until the session ends, so anything the run needs on the host, such as the relay, can live in that process for as long as the session does.
- **pma checks git itself.** It snapshots hooks and command-running settings at dispatch and refuses a run whose snapshot changed (`pma/crates/pma-core/src/dispatch.rs:293`). `--clone` makes that check a second line of defence, not the only one.

So pma needs sanduk to set up and tear down everything around the agent's process, and to hand back the process to start. pma does not need sanduk to run that process.

## Recommendation: the caller owns the process

The library prepares a run and returns the argv and environment to start. The caller starts it its own way and waits for it, then hands back the exit status. The library finishes the run. `sanduk_container::Engine::run_argv` already works this way ("run it however the caller needs its stdio"); this extends it to the whole run.

```rust
let sanduk = sanduk::Sanduk::new(sanduk::Dirs::default())?;   // XDG dirs; no globals
let plan = sanduk
    .run(&task)                     // the prompt
    .agent("claude")
    .provider("anthropic")
    .mode(Mode::KeySafe)
    .workdir(&worktree)
    .at_host_path(true)
    .clone_into(home.join("clones").join(run.id.to_string()))   // pma's state, pma's run id
    .model("claude-sonnet-4-5")
    .timeout(Duration::from_secs(1800))
    .plan()?;                       // checks only; creates nothing

let live = plan.start(&mut observer)?;   // image, network, holder, relay, clone, record, reaper
let child = spawn_guarded(live.command(), None, &log)?;   // pma's watchdog, unchanged
let status = child.wait()?;
let outcome = live.finish(status, &mut observer)?;   // verify, return, teardown, relay stats
```

The three phases:

| Phase | Does | Side effects |
|-|-|-|
| `plan()` | Resolves mode, agent, recipe, kits, mounts, user; checks the key exists; for `clone_into`, checks the tree as `Clone::plan` does and that the path does not exist | None. Refusals come back as typed errors before anything is acquired |
| `start()` | Builds or finds the image; creates the network and holder; starts the relay; makes the clone; writes the run record and starts the reaper; prepares the agent's container spec | Every resource it acquires is listed in the run record |
| `finish(status)` | Runs verify and the clone's return while the network is up; deletes containers, holder and network; stops the relay; releases the record | Keeps the clone if its return failed, and says so in `Outcome` |

Dropping a `Live` without `finish` deletes its containers, holder and network and stops the relay. It does not run the clone's return, and it keeps the clone if the agent had started. The caller chose its path, so it can find it later. SIGKILL runs no `Drop`; the reaper covers that case, as it does for the CLI today.

Keeping the clone, rather than returning it from `Drop`, for three reasons:

- The return starts a container and may take up to `RETURN_SECONDS` (300 s). A destructor that blocks for minutes stalls whatever dropped it, a panicking thread included.
- `Drop` cannot report a failure. A return that failed there would lose the work silently.
- A caller that skipped `finish` has abandoned the run. Keeping the clone leaves the choice to a person.

### Where the clone lives

The caller names the clone's path; the library has no default for it. The CLI's `--clone` passes `<sanduk state>/clones/<run>`, as now. pma passes `~/.local/state/pma/clones/<pma run id>`, beside its `worktrees/`, `runs/` and `verify-trees/`. Three reasons:

- The clone is the run's work, and pma owns the run. pma's review, retention and `pma close` then cover a kept clone without reading sanduk's state.
- Keyed by pma's run id, a kept clone maps to its run with no lookup. sanduk's run id is random hex that pma never sees.
- `pma` removing its state directory removes every clone it made.

The rest of sanduk's state stays in sanduk's directory, for every caller. A caller could pass `Dirs { state: pma's }` instead, but that would move the run records too. Records describe containers and networks, which belong to the machine and not to one caller. The next `sanduk run`'s sweep, and `sanduk ps` and `sanduk clean`, must see every caller's leftovers. The split:

| What | Where | Why |
|-|-|-|
| Run records, reaper list, sweep | sanduk's state | containers and networks are machine-wide; any sanduk may reap them |
| Images, kits cache | sanduk's cache | shared between callers: one build serves all |
| Agent specs, recipes | sanduk's config | what `sanduk list agents` shows |
| The clone | the caller's `clone_into` path | the run's work, owned by whoever owns the run |

`Live::command()` returns a `std::process::Command`. Its program is the engine's CLI (`docker` or `container`), with the agent's environment set. For `.interactive(true)` it carries `-it` and expects a terminal, so pma runs it in a herdr pane. The relay lives in the process that holds `Live`, so pma's job process must outlive the pane. It already does.

### Why not a `run()` that does everything

A blocking `sanduk::run(opts) -> Outcome` that spawns the agent itself is simpler to call. Three reasons against it:

- pma would lose its watchdog, its log and its kill path for exactly the runs that most need them.
- The library would need its own timeout, signal and cancellation design. pma already has one, and it is tested.
- Interactive runs need a terminal the caller provides.

The CLI can still offer one: `sanduk-cli` calls `plan`, `start`, its own `launch`, then `finish`. That is today's `run`, minus the setup it moves into the library.

### Outcome and errors

`Outcome` replaces the stats file for a library caller:

- `exit`;
- `mode`;
- `relay`: requests, rejected, spent, unpriced;
- `verify`: command, exit, ok, seconds, timed out;
- `returned`: branch and commit count, `None` when the agent changed nothing;
- `kept`: the clone's path when its return failed;
- `notes`: each warning, such as a wildcard host listener.

The stats file stays a CLI feature, written from `Outcome`.

Errors carry a kind a caller can act on, not only a message:

| Kind | Example | pma would |
|-|-|-|
| `Refused` | dirty tree under `clone_into`, clone path exists, `-w` not a top level, unknown agent | fail the dispatch, not count an attempt |
| `Setup` | engine not running, image build failed, no key | mark it as infrastructure (`--infra`), retryable |
| `Return` | bundle missing, fetch failed | fail the run and name `kept` |

pma today can only match on message text for these.

### Observer

`trait Observer { fn note(&mut self, note: Note); }` receives what the CLI prints now: notes, image build progress, the trace. The CLI's observer writes to stderr. pma's writes to the run's log. `()` discards everything. Every print site in `cli/run.rs` becomes a call to it.

## Crates

| Crate | Holds | Dependencies | pma links |
|-|-|-|-|
| `sanduk-sandbox` | Unchanged | `landlock`, `libc` | yes, as now |
| `sanduk-container` | Engine primitives as now, plus run records, sweep and reaper (`src/runs.rs`), which depend only on the engine | `serde_json` (no features), `libc` | through `sanduk` |
| `sanduk` | The library: `Sanduk`, `Plan`, `Live`, `Outcome`; agents, recipes, kits, catalog, providers, the relay, preflight, clone and return, verify, image build | tokio, hyper, rustls (ring), flate2, serde, toml, sha2, regex | yes |
| `sanduk-cli` | Binary `sanduk`: clap, `Signals`, `launch` and the stream reader, the stats file, the prose report, assistants and `rusqlite`, `serve` and `tick` | `sanduk`, clap, rusqlite | no |

### Why the relay stays in `sanduk`

An earlier suggestion (pma, 2026-11-04) put the relay in its own async crate, so a caller could link the rest without tokio. For pma that buys nothing:

- pma's `sanduk` worker runs `key-safe`, and every keyed mode needs the relay. pma compiles tokio either way.
- `Relay::start` is already a blocking API (`src/relay/mod.rs:228`). It runs its own two-thread runtime on a thread of its own, so pma writes no async code.

Split it out when a caller appears that needs containers without a key. A container-only verify is the likely one, and pma would still link both crates.

### Why `sanduk-cli` rather than a `cli` feature

A `cli` feature on `sanduk`, default on and with `required-features` on the binary, keeps `cargo install sanduk` working. Two reasons against it:

- The library's dependency list then names clap and rusqlite, so a reader has to check the features to know they are optional.
- One crate anywhere in the graph enabling `cli` pulls them into every caller.

The cost of a separate crate is the install command: `cargo install sanduk-cli`. The README and pma's docs name it once each.

## Changes, in order

Each step leaves the CLI's tests passing and its output unchanged.

1. **Remove process-wide state.**
   - Pass a `Dirs { config, state, cache }` value instead of `override_dirs` (`src/util.rs:35`).
   - Move `Signals` to the CLI; the library takes no signal.
   - Replace `note` with the observer.
   - Tests that call `override_dirs` pass `Dirs` instead.
2. **Remove the feature leaks.**
   - Drop `serde_json/preserve_order`. `apply_policy` (`src/relay/mod.rs:640`) reads the model, caps the token field, and adds `stream_options.include_usage` to OpenAI chat requests. Read the top level into `indexmap::IndexMap<String, Box<serde_json::value::RawValue>>`, which needs serde_json's `raw_value` feature. Parse only the values the policy reads or changes, and write the rest back byte for byte. That keeps more of the body intact than `preserve_order` does, since nested values are never re-serialised. `raw_value` is additive: it changes no other crate's `Map`.
   - Drop `toml/preserve_order`. sanduk only reads TOML. The feature changes three things, none of them meaningful:
     - the order `wire.env` sets environment variables;
     - the key order inside a `{ json = ... }` env value, which is the same JSON either way;
     - which of several invalid keys an error names first.

     `vars.<name>.map` is collected into a `BTreeMap` already (`src/agent/mod.rs:315`).
   - Add a test that builds a `serde_json::Map` in a dependent crate and checks its keys serialise sorted.
3. **The reaper without `current_exe`.** A library cannot assume its host binary has a `reap` subcommand.
   - **Recommended:** start `sh -c` with a short script. It blocks on `read` from the pipe, then deletes each name in the run record with the engine's CLI. The record gains a plain list of container and network names, one per line, beside its JSON. No binary has to cooperate.
   - **Alternative:** `Reaper::start_with(program, args)`, and pma adds a hidden `pma sanduk-reap` that calls `sanduk::reap`. Each caller must then remember to wire it, and one that does not leaks silently.
4. **Extract the run.** Move `cli/run.rs`'s setup and teardown into `sanduk::run::{Plan, Live, Outcome}`. `RunArgs` maps onto the builder, and the CLI's `run` becomes `plan`, `start`, `launch`, `finish`. `--dry-run` becomes `Plan::describe()`. The CLI tests in `tests/cli.rs` are the parity check.
5. **Typed errors**, with the kinds above.
6. **Split the crates.** Move the CLI, assistants and `rusqlite` into `sanduk-cli`, and `runs.rs` into `sanduk-container`.
7. **A test engine for callers.** `sanduk-container`'s tests drive a fake `Exec` (`crates/sanduk-container/tests/engine.rs`). Expose it behind a `test-util` feature, so pma can test dispatch through `Plan`, `start` and `finish` without Docker.
8. **Publish** `sanduk-container`, `sanduk` and `sanduk-cli` together.

## What changes in pma

For reference; this is pma's work, after the steps above.

- **pma's default does not change.** `clone_into` is an opt-in on the `sanduk` worker, off by default. Without it, pma mounts the directory it mounts today:
  - under `workspace = clone`, the user's own clone, with its uncommitted work;
  - under `workspace = worktree`, pma's worktree.

  With it on, sanduk clones that directory into `~/.local/state/pma/clones/<run id>`. `plan()` refuses a clone with uncommitted changes to tracked files. pma then fails the dispatch with no attempt counted, and its message names the two ways out: commit or stash, or turn the option off for this project.

- The `sanduk` worker becomes a typed worker kind instead of an argv template: mode, agent, provider, kits and `clone` (default off), in `[agents.sanduk]`. The four flags `Worker::sanduk()` documents as mandatory (`--work-at-host-path`, `--stream-json`, `--no-report-instruction`, `--timeout`) become defaults pma cannot get wrong.
- `attempt_once` calls `plan`, `start`, `spawn_guarded(live.command())`, then `finish`, and reads cost, verify and the returned branch from `Outcome`.
- With the option on, pma passes `~/.local/state/pma/clones/<run id>`. After `finish` it writes the returned branch's tree into its worktree as uncommitted edits, then deletes the branch. A kept clone shows on the run, and `pma close` or a reject deletes it. Its review, scope and verify steps stay unchanged.
- pma pins `sanduk` with `=` until 1.0, as it pins `sanduk-sandbox` now. The test that the template parses against the installed `sanduk` (`pma/crates/pma-core/src/worker.rs:971`) is no longer needed: the compiler does that check.

## Open

- **Build cost after the split.** Expected: about the measured figures minus clap and rusqlite, which pma already has. Measure after step 6.
- **Returning a kept clone later.** `sanduk::return_clone(path, repo)` in the library, and `sanduk return <clone>` in the CLI, would run the return step on a clone that `Drop` or a failed return kept. pma would call it from a retry. Without it, a person fetches the work by hand.
- **The clone's cost, and a shallow clone.** `Clone::create` copies the repository's whole object history per run, so disk and start-up time grow with repository size. A git worktree would cost almost nothing, but it is not an option: a worktree shares the repository's `.git` (objects, refs, config, hooks), so the agent could only commit with that `.git` mounted writable, which is what `--clone` exists to prevent. Ways to copy less while keeping the separate `.git`:

  | Variant | Copies | Status |
  |-|-|-|
  | `--depth 1`, with `--no-local` | the base commit only | Unverified. git ignores `--depth` for a plain local clone; `--no-local` should make it apply, as a `file://` URL does. The bundle's prerequisite is the base, which the host has |
  | `--reference`, with the original's `.git/objects` mounted read-only | no objects | Untried. Adds a read-only mount, and the clone's `objects/info/alternates` names a host path the container must see |
  | `--filter=blob:none` | Rejected | It fetches missing blobs from the origin when needed, and a sealed container cannot reach it |

  Measure `--depth 1` with `make live` before adopting it, on a repository large enough for the difference to show. Also check that the agents cope with a shallow history: a `git log` or `git blame` past the base comes back empty.

- **Cloning pma's worktree.** Checked 2026-11-04 with git in a scratch repository. `git clone --no-local --no-hardlinks` from a detached worktree checks out the worktree's commit, not the repository's `HEAD`. Not yet run: `Clone::plan` on a worktree. It should accept one, since `rev-parse --show-toplevel` in a worktree names the worktree, as git documents. Under `workspace = worktree` with the option on, a run then holds two copies, pma's worktree and sanduk's clone. pma could instead give sanduk the repository and the base commit and skip its own worktree. That is a later change, and it would need `clone_into` to take a commit as well as a directory.

- **Two runs returning into one repository.** Each fetches into its own ref, so no lock is needed today. Check this again if `Outcome` ever offers a merge.
- **The stream reader.** pma does not need it. Does any other caller?
