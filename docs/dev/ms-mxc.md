# Microsoft MXC

Status: 2026-10-10. Spike steps 1 and 2 run on one Ubuntu 24.04 host; step 2 is blocked (see "Spike"). The rest is deferred to a host with Docker. No decision is taken. Other claims come from MXC's docs and the crates.io API unless marked as inference.

Source: <https://github.com/microsoft/mxc>, crate [`mxc-sdk`](https://crates.io/crates/mxc-sdk).

## What it is

An SDK for running untrusted code ("model output, plugins, and tools") under a policy. The core is Rust; .NET and Node packages wrap it. License MIT.

The caller names a backend, a policy and a command; MXC validates the request and launches the workload.

| Platform | Default backend | Others |
|-|-|-|
| Linux | Bubblewrap | LXC; MicroVM (Nanvix) and Hyperlight, experimental |
| macOS | Seatbelt | -- |
| Windows 11 | ProcessContainer | Windows Sandbox (experimental), IsolationSession, WSLC |

Policy has three parts: filesystem (`readonlyPaths`, `readwritePaths`, `deniedPaths`), network egress and ingress, and UI (clipboard, display).

`--audit policy.json` records the accesses a workload makes, to help write a policy. It disables the sandbox for that run.

## State of the project

Measured 2026-10-10:

- `mxc-sdk` 1.0.0, published 2026-10-06; 63 downloads. It is the only version.
- The crate declares no `rust_version`. The repo pins Rust 1.93 in `src/rust-toolchain.toml`; sanduk declares 1.88. The crate builds on 1.89 (1.88 not tried).
- Repo created 2026-02-06; about 2.4k stars.
- Telemetry is opt-in and off by default.

## The Bubblewrap backend

From [`bubblewrap-backend.md`](https://github.com/microsoft/mxc/blob/main/docs/backends/bwrap/bubblewrap-backend.md):

- **Namespaces.** User, PID, IPC and UTS are unshared. The capability bounding set is emptied before `exec`. The environment is cleared.
- **Filesystem.** An allowlist of host paths over a read-only baseline. `$HOME`, `/root`, `/opt`, `/var` and `/sys` are not visible. `deniedPaths` are masked: directories with an empty tmpfs, files with `/dev/null`. Symlinks are resolved before masking.
- **Network, deny.** `egress.default: "deny"` with no rules uses `--unshare-net`: loopback only.
- **Network, rules.** `egress.allow` and `egress.deny` take IP literals and CIDRs, not hostnames. A supervisor installs iptables rules in a private namespace backed by `slirp4netns`, then drops `CAP_NET_ADMIN`. No IPv6 connectivity.
- **Network, proxy.** `runtimeConfig.networkProxy` allows loopback and one proxy endpoint at `10.0.2.2`. DNS is closed; the proxy resolves.
- **Ingress.** `ingress.allow` is refused: slirp does not forward ports.
- **Lifecycle.** One-shot only; no persistent sandbox on this backend.
- **Requires.** Unprivileged user namespaces, `bwrap` >= 0.5. Rule and proxy modes also need `slirp4netns`, `unshare`, `nsenter`, `iptables`, `ip6tables` and `nf_conntrack`.
- **Not covered by the doc.** Seccomp, Landlock, Ubuntu's AppArmor user-namespace restriction, and running inside a container.

## Compared with `sanduk-sandbox`

| | `sanduk-sandbox` | MXC, Linux (bwrap) |
|-|-|-|
| Writes | Landlock, in-process | bind-mount allowlist |
| Reads | open | allowlist |
| Network | open | deny, CIDR rules, or proxy only |
| Deny inside an allowed tree (`.git`) | not expressible: Landlock unions rules along a path | `deniedPaths` |
| External binaries | none | `bwrap`; more for network rules |
| Windows | no | yes |

On macOS both use Seatbelt. Whether MXC's Seatbelt profile is stricter than ours is not checked.

## What it would add

- **timu's policy.** `rewrite.md` ("timu's policy") records that `Policy` cannot express timu's rules: no network, no reads under `$HOME`, no `.git` writes. The bwrap backend expresses all three, on the doc's description.
- **A sealed tier without a container engine.** Proxy mode allows one endpoint. If the relay is that endpoint, a process gets model access and nothing else. Inference: the relay admits only provider paths, so tool traffic (`cargo fetch`, `uv sync`) would fail too.
- **Windows.** sanduk has no Windows sandbox today.

## Risks

- **Maturity.** One release, four days old. API stability is unknown.
- **No in-process option on Linux.** minima links `sanduk-sandbox` and needs no binaries. A missing `bwrap` is a new way to fail.
- **Inside a sanduk container (inference).** bwrap needs `CLONE_NEWUSER`. Docker's default seccomp profile is expected to block it, and `--cap-drop ALL` gives nothing back. MXC would then fail inside sanduk's containers, as Landlock does on Apple's `container` (`rewrite.md`, "Apple's container guest kernel").
- **Ubuntu 24.04+ (measured).** Every bwrap launch fails out of the box; see "Spike".
- **Failures look like workload exits.** A bwrap setup failure returns `Ok` with `Exited(1)`, not `Err`. The caller cannot tell it from the command failing. `platform_support()` still reports `bubblewrap` available.
- **Dependency weight.** 87 crates against `sanduk-sandbox`'s 12, including `tokio` with `full`, `nix`, `chrono` and `flatbuffers`. 55 are not already in sanduk.

## Spike

Run 2026-10-10 on Ubuntu 24.04.5, kernel 7.0.0, bwrap 0.9.0. A scratch crate depended on `mxc-sdk` 1.0.0 with default features and called `v1::run` with `Containment::Bubblewrap`. The build script downloads nothing unless the `microvm` feature is on.

### Step 1: dependencies and toolchain

| | |
|-|-|
| Crates, normal edges | 87 (`sanduk-sandbox`: 12; `sanduk`: 96) |
| Not already in sanduk | 55 |
| Rust 1.89 | builds |
| Release binary, trivial `main` | 2.7 MB; 2.0 MB stripped |

### Step 2: host

All bwrap runs failed before the workload started:

```text
bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted
```

Cause: `kernel.apparmor_restrict_unprivileged_userns = 1`, Ubuntu's default since 23.10. An unconfined process may create a user namespace but gets no capabilities in it. Bare `bwrap` fails the same way without `--unshare-net` (`setting up uid map: Permission denied`), and so does `unshare -rn`. Ubuntu ships no AppArmor profile for `/usr/bin/bwrap`, and bwrap is not setuid.

| Test | Result |
|-|-|
| Write inside root, write outside, read `$HOME`, `.git` and `.env` denied inside root, TCP deny | not reached: bwrap setup fails |
| Write outside root reached the host | no: fails closed |
| Proxy mode, CIDR allow | refused at request time: `slirp4netns` not on `PATH` |
| `platform_support()` | `bubblewrap` available; proxy unsupported, with the reason |
| Latency, `/bin/true` | 4.6 ms under MXC (failing setup) against 0.35 ms direct |

The latency covers a launch that fails during setup, so it is a lower bound.

Remedies, both needing root, neither applied:

- An AppArmor profile granting `userns` to `/usr/bin/bwrap`. This also opens user namespaces to anything that runs bwrap.
- `sysctl kernel.apparmor_restrict_unprivileged_userns=0`, which lifts the restriction host-wide.

`sanduk-sandbox` needs neither: Landlock has no user-namespace requirement.

### Step 3: inside a container

Not run: this host has no container engine.

## Options

1. **Replace `sanduk-sandbox` with MXC.** Not proposed: it adds a binary dependency to minima and a four-day-old API.
2. **Add MXC as an optional backend** behind `Policy`, used when reads or network must be denied.
3. **Borrow the design, not the crate.** Add read and network denial to `sanduk-sandbox` directly: Landlock ABI 4 covers TCP bind and connect; reads can use Landlock's read rights.
4. **Do nothing** until MXC has a release history.

## Open questions

- Is Windows a target? If not, the gain is Linux-only.
- Should tier 1 stop prompt injection, or only model error? The README promises only the latter. Read and network denial would change that promise.
- Must minima stay free of external binaries? If so, option 2 is out for minima.

## Next step, if pursued

1. On a host where bwrap can create namespaces, rerun step 2: the policy tests, then proxy mode pointed at the relay (needs `slirp4netns`).
2. Run step 3 inside `docker run --cap-drop ALL --security-opt no-new-privileges`.

Step 2's result already counts against option 2 on stock Ubuntu: an install there needs a root-applied AppArmor change before MXC can run.
