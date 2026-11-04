# Shared core: what sanduk-rs keeps stable for other programs

Decision record, 2026-11-04.

## Decision

pma copies sanduk-rs's library into `pma/crates/pma-box` and maintains it apart (`pma/docs/dev/pma-box.md`). sanduk-rs keeps the crates with little policy stable and publishable, so pma can link them from crates.io in place of its copies, as it already links `sanduk-sandbox`. Both projects fork the policy layer.

The alternatives were:

- Independent forks. They leave two copies of security-relevant code: the reaper and the sweep, which delete containers. A fix to either must then be made twice.
- Vendoring sanduk-rs into pma by script. Every sync then carries sanduk's policy into pma.

## Layers

| Crate | Layer | pma |
|-|-|-|
| `sanduk-sandbox` | shared | links from crates.io |
| `sanduk-container` | shared: engines, run records, sweep, reaper, `testing::Fake` | candidate: keeps its copy for now |
| `sanduk` | policy: agents, recipes, kits, providers, the relay, the run | forks it as `pma-box` |
| `sanduk-cli` | sanduk's binary | no |

pma-box's copy of `sanduk-container` differs only in names (`pma-` for `sanduk-`). A required `Prefix` takes over those names. pma measured about 120 lines of difference from the current crate, after mapping module paths: the prefix work plus its renames.

## What the shared crates owe a caller

- No sanduk-specific names: no path, no message and no default prefix. Every engine, sweep and reaper takes the caller's `Prefix`, so a missing one is a compile error.
- One fixed format string: `AGENT_UID_LABEL` (`sanduk.agent-uid`). It is part of the image format the crate reads, not a namespace. The recipe writes it and `Engine::image_uid` reads it from one constant, so they cannot disagree, and an image one program built reports its uid to the other. It selects nothing to delete: the prefixed repository name keeps images apart. Renaming it would change every existing image and every golden Containerfile.
- No process-wide state, no printing, no signal handlers.
- No Cargo feature that changes another crate's behaviour (see `tests/features.rs`).
- Semver: a breaking change bumps the minor version while at 0.x.

## Open

- **The relay.** It holds the key, so a fix there matters in both forks. It sits in `sanduk` and is tied to providers and the run's policy. pma-box dropped its planned blocking rewrite, so the relay stays comparable with sanduk-rs's (`pma/docs/dev/pma-box.md`, "Sharing with sanduk-rs"). Revisit once pma links `sanduk-container` and one relay fix has been ported by hand between the forks: that port shows what keeping two copies costs.
- **Releasing.** `sanduk-container` has its own version, apart from the workspace's. 0.1.0 is on crates.io; the required prefix makes the next release 0.2.0. It stays unpublished until pma decides to link it. Today its only consumer is `sanduk`, through a path dependency, so publishing now would fix the API before a second user has tried it. pma's test for switching is how often its `src/container/` changes.
- **The sweep guard.** A sweep keeps a record naming another prefix's container. It only matters when two prefixes share a runs directory. pma-box keeps its records in its own directory, under one prefix, so it does not need the guard while it maintains its own copy.
