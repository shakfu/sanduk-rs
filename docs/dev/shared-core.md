# Shared core: what sanduk-rs keeps stable for other programs

Decision record, 2026-11-04. Reverted for pma 2026-11-05: pma deleted `pma-box` before anything called it. It links `sanduk-sandbox` and runs the `sanduk` binary (`pma/docs/dev/sanduk-integration.md`). No program forks the policy layer now, so the rules under "What the shared crates owe a caller" are for future callers.

## Decision

pma copies sanduk-rs's library into `pma/crates/pma-box` and maintains it apart (`pma/docs/dev/pma-box.md`). sanduk-rs keeps the crates with little policy stable and publishable, so pma can link them from crates.io in place of its copies, as it already links `sanduk-sandbox`. Both projects fork the policy layer.

The alternatives were:

- Independent forks. They leave two copies of security-relevant code: the reaper and the sweep, which delete containers. A fix to either must then be made twice.
- Vendoring sanduk-rs into pma by script. Every sync then carries sanduk's policy into pma.

## Layers

| Crate | Layer | pma |
|-|-|-|
| `sanduk-sandbox` | shared | links from crates.io |
| `sanduk-container` | shared: engines, run records, sweep, reaper, `testing::Fake` | no |
| `sanduk-core` | policy: agents, recipes, kits, providers, the relay, the run | no |
| `sanduk` | sanduk's binary, in `crates/sanduk-cli/` | runs it as the `sanduk` worker |

## What the shared crates owe a caller

- No sanduk-specific names: no path, no message and no default prefix. Every engine, sweep and reaper takes the caller's `Prefix`, so a missing one is a compile error.
- One fixed format string: `AGENT_UID_LABEL` (`sanduk.agent-uid`). It is part of the image format the crate reads, not a namespace. The recipe writes it and `Engine::image_uid` reads it from one constant, so they cannot disagree, and an image one program built reports its uid to the other. It selects nothing to delete: the prefixed repository name keeps images apart. Renaming it would change every existing image and every golden Containerfile.
- No process-wide state, no printing, no signal handlers.
- No Cargo feature that changes another crate's behaviour (see `tests/features.rs`).
- Semver: a breaking change bumps the minor version while at 0.x.

## Open

- **The relay.** One copy, in `sanduk`. Revisit only if a caller forks the policy layer.
- **Releasing.** `sanduk-container` has its own version, apart from the workspace's. 0.1.0 is on crates.io; the required prefix makes the next release 0.2.0. The `sanduk` binary depends on it and on `sanduk-core` 0.2.0, so publishing `sanduk` 0.2.0 for pma's worker publishes all three. Its only consumer is still `sanduk-core`.
- **The sweep guard.** A sweep keeps a record naming another prefix's container. It only matters when two prefixes share a runs directory.
