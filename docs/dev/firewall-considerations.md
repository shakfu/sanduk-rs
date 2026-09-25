# Firewall considerations

Status: 2026-09-25. The probe (step 1 of the recommendation) is implemented; firewall analysis, steps 2 and 3, is deferred while the tool is young. Claims are measured on one macOS host unless marked as inference.

## What needs a path through a firewall

In `key-safe` and `sealed` modes the relay listens on the engine's bridge gateway, and the agent connects to it from inside the container. To the host that connection is incoming. If a host firewall drops it, the agent's first API call hangs until `--timeout`, with no error from sanduk or the agent.

Everything else sanduk does is outgoing: the key check, image builds, and the relay's calls to the provider.

## Where it applies in sanduk

| Case | Listens? | Exposure | Priority |
|-|-|-|-|
| `run --mode sealed` or `key-safe` at a terminal | relay, on the bridge gateway | a person sees the macOS prompt and can answer it; on Linux a drop is silent | medium |
| `tick` and `serve` (assistants) | relay, per wakeup | unattended: nobody answers a prompt, and the wakeup times out | high |
| pma's `sanduk` worker (`run --mode key-safe`) | relay, per dispatch | unattended, as above | high |
| `run --mode open` | nothing | not applicable | -- |
| `run --mode open --base-url http://<host>:8080` (a host llama-server) | the host service, not sanduk | the firewall rule belongs to that service's binary | out of scope |
| `build`, `ps`, `clean`, `destroy`, `list` | nothing | outgoing only | -- |
| Docker Desktop, Colima, Lima | relay cannot bind: the bridge is inside a VM | already refused, with `Kind::gateway_hint` | handled |
| Tests: `tests/relay.rs`, `tests/cli.rs`, `make live` | relay on 127.0.0.1 | whether the macOS firewall prompts for a loopback-only listener is unmeasured (see below) | low |
| `sanduk-sandbox`, minima's `--sandbox` | nothing | neither Landlock nor Seatbelt rules here touch the network | not applicable |

Outgoing application firewalls (Little Snitch, LuLu on macOS; OpenSnitch on Linux) are a separate case. They prompt when the relay first connects to the provider. That is the same unattended-hang problem in the other direction, and sanduk cannot detect it without their own tools. They are named here and not analysed further.

## macOS

The built-in Application Firewall filters incoming connections per application. A listener it has no entry for triggers an Allow/Deny dialog.

Measured on 2026-09-25 (macOS, firewall on, stealth mode on):

- The first sealed run of `target/debug/sanduk` raised the dialog "Do you want the application "sanduk" to accept incoming network connections?" After Allow, the same run on Apple's `container` reached the relay from the VM and passed.
- `socketfilterfw --listapps` then listed the binary by path, as "Allow incoming connections".
- The debug binary is ad-hoc signed: `codesign -dv` reports `Signature=adhoc`, `TeamIdentifier=not set`, `Identifier=sanduk-86cf8b193bd4e115`.
- All of the state below is readable without root: `--getglobalstate`, `--getblockall`, `--getallowsigned`, `--getstealthmode`, `--listapps`.

- The binary was rebuilt many times after that Allow. `--listapps` still showed the same path as allowed, and a later sealed run passed its probe, with no dialog reported. So a rebuild at the same path is not re-prompted, on this evidence: one host, one observation, and a dialog answered quickly would not show in it.

Inferred, not measured:
- A Developer ID-signed binary is allowed without a dialog when "automatically allow downloaded signed software" is on.
- The CLI tests, which bind the relay to 127.0.0.1, may themselves have raised the first dialog. It appeared during a session that ran them and before any VM run. Whether loopback-only listeners are prompted for was not isolated.

### What `firewall_warning` checks today

`src/preflight.rs` warns only when the firewall is on and the binary has an explicit Block entry. It misses the case that happened: firewall on, and a binary with no entry.

### The proposed check

| Firewall state | This binary | Result |
|-|-|-|
| off | any | nothing |
| block-all on | any | the relay is unreachable: warn at a terminal, refuse unattended |
| on | listed, Block | as today: warn, with `--unblockapp` |
| on | listed, Allow | nothing |
| on | unlisted, Developer ID-signed, auto-allow on | nothing (inference above) |
| on | unlisted, unsigned or ad-hoc | a dialog is coming: warn at a terminal; `tick` and `serve` refuse before starting a container, naming `sudo socketfilterfw --add <path>` |

Implementation, about 60 lines beside the existing parser:

- read the four states and the listing, as the existing code does for two of them;
- classify the signature with `codesign -dv <current_exe>`: `Signature=adhoc`, a `TeamIdentifier`, or no signature;
- return a verdict; let `run` warn and let the assistant path refuse;
- tests: one per table row, with canned `socketfilterfw` and `codesign` output, reusing the listing-parser test.

Python sanduk has the same gap for its interpreter (its `TODO.md`), and the same table applies.

### The durable fix, for a user

Put a release build at a fixed path, then either sign it with a stable identity or add it once with `sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add <path>`. After that no prompt appears, whatever the check above does.

## Linux

Linux has no per-application prompt. A firewall rule matches interfaces, addresses and ports, so a blocked relay fails silently: a DROP rule makes the agent hang until `--timeout`, and a REJECT rule makes it fail at once.

Container-to-host traffic arrives on the host's INPUT path from the bridge interface. The relevant configurations, all unmeasured here:

- **ufw** with its default `deny (incoming)` is expected to drop a container's connection to the gateway. Docker's own rules cover published ports and forwarding, not a service listening on the host ([Docker and ufw](https://docs.docker.com/engine/network/packet-filtering-firewalls/#docker-and-ufw)). The usual fix is `ufw allow in on <bridge interface>`.
- **firewalld**: Docker places its bridge interfaces in a `docker` zone ([firewalld integration](https://docs.docker.com/engine/network/packet-filtering-firewalls/#integration-with-firewalld)). Whether that zone admits traffic to a host listener, for a user-defined `--internal` network like `sanduk-net`, is not verified.
- **nftables or iptables** with an INPUT policy of DROP: as ufw.
- **Rootless Docker, Docker Desktop**: the gateway is not a host address, so the relay cannot bind. `run` already refuses this case.

Reading these rules needs `CAP_NET_ADMIN`, and `ufw status` needs root. Firewall analysis on Linux is therefore not possible for an ordinary user, which is what sanduk runs as.

One change would make a Linux fix stable: name the bridge interface when creating the network (`docker network create -o com.docker.network.bridge.name=sanduk-net`). A ufw rule can then name the interface, which would otherwise be `br-<network id>` and change whenever the network is recreated. This is a proposal; the option exists in Docker's bridge driver, and its effect on `--internal` networks is untested.

## Implemented: the probe

`run` probes the relay after it binds and before the agent starts, in `key-safe` and `sealed` modes (`probe_relay` in `src/cli/run.rs`, `Engine::probe` in `sanduk-container`):

- The relay answers `GET /_sanduk/ping` with 204, with no token; it is not counted in `Stats` or logged.
- On Apple's engine, `container exec <holder> curl ...`: no new VM. On Docker, a short container on the network, written into the run record before it starts, deleted after.
- curl waits 5 seconds. A failed probe gives back the relay, holder and record, and stops with the likely cause: on macOS, the firewall prompt or `socketfilterfw --add <binary>`; on Linux, a host firewall rule for the bridge interface.

Measured: a live sealed run on Apple's `container` passed its probe through the holder. The stub engine's tests cover both forms and a dropped connection (`FAKE_DOCKER_PROBE_FAIL`), which stops the run before the agent starts and leaves nothing behind.

Every relayed case in the table above is covered, including `tick`, `serve` and pma's worker, which call `run`. What remains deferred is steps 2 and 3 below: explaining a failure from the macOS firewall's state, and a stable bridge interface name on Linux.

## Recommendation: probe the path, don't parse the firewall

A firewall table only predicts reachability, and on Linux it cannot be read at all. A probe measures it, on both platforms and whatever the firewall is (the built-in one, pf, ufw, an outgoing filter, a VPN client's rules):

1. The relay answers `GET /_sanduk/ping` with 204. No token is needed; the response exposes nothing, and it is not counted in `Stats`.
2. After the relay binds and before the agent starts, run `curl -s -o /dev/null --max-time 5 http://<gateway>:<port>/_sanduk/ping` from inside the network. Every shipped image carries curl.
   - On Apple's engine, `container exec` into the holder, which is already running: no new VM.
   - On Docker, `docker run --rm --network <net> <image> curl ...`. That costs one short container start (inference: well under a second).
3. If the probe fails, give back what is held and stop, with the platform's hint: on macOS the table's verdict for this binary; on Linux, "a host firewall dropped a connection from `<bridge interface>` to `<gateway>:<port>`", with the ufw rule.

The probe turns every case above into a failure within seconds, and it needs no platform code except the hint. The macOS table then becomes an explanation of a failure, not a prediction. It could also run before a container starts, for `tick` and `serve`, where a refusal is cheaper than a failed wakeup.

Order, when this is taken up:

1. The probe, in `run`. Small, platform-neutral, and it closes the unattended hang.
2. The macOS verdict, as the probe's failure message.
3. The named bridge interface, if Linux users report ufw.
