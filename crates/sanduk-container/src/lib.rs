//! Container engines, driven through their CLIs.
//!
//! Every subprocess call to an engine lives here, so another engine is a [`Kind`] variant and
//! its match arms. Apple's `container` and `docker` are implemented. An engine supplies four
//! things: the CLI name, the verb that deletes a container (`rm`, not `delete`), how `network
//! inspect` reports the gateway, and whether the host bridge needs a placeholder container.
//!
//! [`Engine::run_argv`] is shared, which is not an accident of the two engines agreeing:
//! `--name`, `--cpus`, `--memory`, `-v`, `-w`, `-e`, `--network` and `--entrypoint` are the
//! Docker CLI flags that Apple's engine adopted.
//!
//! Nothing here prints. What Python sanduk reported as it went is returned instead, for the
//! caller to report or act on.

mod daemon;
mod exec;

use std::fmt;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

pub use daemon::docker_daemon_error;
pub use exec::{Captured, Exec, System};

/// Every container sanduk starts is named from this, and every container it will stop or delete
/// is found by it. Nothing else is touched.
pub const CONTAINER_PREFIX: &str = "sanduk-";

/// Set by every shipped Containerfile to the uid its agent runs as.
pub const AGENT_UID_LABEL: &str = "sanduk.agent-uid";

/// How long the network holder sleeps when the caller does not say. `run` sizes it to the run.
pub const HOLDER_SECONDS: u64 = 86400;

/// A failure the caller can act on. The message is written for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error(message.into()))
}

/// One container as sanduk sees it, whatever the engine's columns say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub name: String,
    pub image: String,
    pub state: String,
}

/// One host directory inside the container, beside the working directory.
///
/// Rendered with `--mount` rather than `-v`: both engines spell the read-only flag the same way
/// there, where `-v host:dest:ro` is Docker's alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub dest: String,
    pub ro: bool,
}

impl Mount {
    pub fn argv(&self) -> [String; 2] {
        let spec = format!(
            "type=bind,source={},target={}",
            self.host.display(),
            self.dest
        );
        let spec = if self.ro { spec + ",readonly" } else { spec };
        ["--mount".into(), spec]
    }
}

/// One container to run. Engine-neutral; [`Engine::run_argv`] renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub cpus: u32,
    pub memory: String,
    /// The working directory: (host dir, path inside).
    pub mount: Option<(PathBuf, String)>,
    /// Every other mount.
    pub mounts: Vec<Mount>,
    /// `-e NAME`: the engine takes the value from this process's environment.
    pub inherit_env: Vec<String>,
    /// `-e K=V`.
    pub env: Vec<String>,
    pub network: Option<String>,
    pub detach: bool,
    pub entrypoint: Option<String>,
    /// What starts the container in place of runc: runsc, Kata.
    pub oci_runtime: Option<String>,
    /// `UID[:GID]` the agent process runs as.
    pub user: Option<String>,
    /// Keep stdin open, so the caller can write to the agent.
    pub stdin: bool,
}

impl ContainerSpec {
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            command: Vec::new(),
            cpus: 4,
            memory: "4G".into(),
            mount: None,
            mounts: Vec::new(),
            inherit_env: Vec::new(),
            env: Vec::new(),
            network: None,
            detach: false,
            entrypoint: None,
            oci_runtime: None,
            user: None,
            stdin: false,
        }
    }
}

/// Where a probe runs from: a container already on the network, or a short one started for it.
#[derive(Debug, Clone, Copy)]
pub enum Via<'a> {
    Holder(&'a str),
    Container {
        name: &'a str,
        network: &'a str,
        image: &'a str,
    },
}

/// A network's gateway, the address the relay binds, and its subnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub gateway: String,
    pub subnet: String,
}

/// Which engine. The per-engine facts are here; the behaviour is on [`Engine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Apple's `container`: Linux containers as lightweight VMs on macOS.
    Apple,
    /// The Docker CLI against a local daemon.
    Docker,
}

/// Flags every container gets on top of its spec. The agent runs as the image's unprivileged
/// user and only reads, writes and forks, so no capability it could keep is one it needs. Not
/// `--read-only`: every shipped agent writes under `$HOME`.
const HARDENING: &[&str] = &["--cap-drop", "ALL", "--init"];

/// Docker shares the host kernel, unlike Apple's VM per container, so the two flags Apple's CLI
/// lacks are the two that matter most there. The pid ceiling is a fork bomb's, not a workload's:
/// node plus a shell plus ripgrep is two orders of magnitude below it.
const DOCKER_HARDENING: &[&str] = &[
    "--cap-drop",
    "ALL",
    "--init",
    "--security-opt",
    "no-new-privileges",
    "--pids-limit",
    "1024",
];

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Apple, Kind::Docker];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Apple => "apple",
            Kind::Docker => "docker",
        }
    }

    pub fn cli(self) -> &'static str {
        match self {
            Kind::Apple => "container",
            Kind::Docker => "docker",
        }
    }

    /// Deletes containers, images and networks alike: `container delete` against `docker rm`.
    pub fn delete_verb(self) -> &'static str {
        match self {
            Kind::Apple => "delete",
            Kind::Docker => "rm",
        }
    }

    pub fn install_hint(self) -> &'static str {
        match self {
            Kind::Apple => "Install from github.com/apple/container.",
            Kind::Docker => "Install from docs.docker.com/get-docker/.",
        }
    }

    pub fn hardening(self) -> &'static [&'static str] {
        match self {
            Kind::Apple => HARDENING,
            Kind::Docker => DOCKER_HARDENING,
        }
    }

    /// Whether the program that starts a container can be swapped, for gVisor or Kata in place
    /// of runc.
    pub fn takes_oci_runtime(self) -> bool {
        self == Kind::Docker
    }

    /// vmnet-style engines only create the host bridge while a container is attached; Docker
    /// creates it with the network.
    pub fn needs_network_holder(self) -> bool {
        self == Kind::Apple
    }

    /// Whether a bind mount keeps host ownership inside the container, so the agent writes the
    /// workdir only as its owner. Apple's engine maps it.
    pub fn keeps_mount_owner(self) -> bool {
        self == Kind::Docker
    }

    /// Whether `run --user` picks the uid the agent runs as. Without it the image's own user is
    /// the only one there is.
    pub fn takes_user(self) -> bool {
        self == Kind::Docker
    }

    /// Appended when the relay cannot bind the gateway, where the engine knows a likely reason.
    pub fn gateway_hint(self) -> &'static str {
        match self {
            Kind::Apple => "",
            Kind::Docker => {
                " A daemon inside a VM (Docker Desktop, Colima, Lima) keeps the bridge in the VM \
                 rather than on this host; --proxy needs a daemon running on this kernel."
            }
        }
    }

    pub fn from_name(name: &str) -> Result<Kind> {
        Kind::ALL
            .into_iter()
            .find(|k| k.name() == name)
            .ok_or_else(|| {
                let known: Vec<_> = Kind::ALL.iter().map(|k| k.name()).collect();
                Error(format!(
                    "unknown runtime {name:?}; known: {}",
                    known.join(", ")
                ))
            })
    }
}

/// Engines tried in order when none is named. `os` is `std::env::consts::OS`. Apple's engine
/// exists only on macOS; elsewhere a `container` on `PATH` is some other program.
pub fn runtime_order(os: &str) -> &'static [Kind] {
    match os {
        "macos" => &[Kind::Apple, Kind::Docker],
        _ => &[Kind::Docker],
    }
}

/// The first engine in `os`'s order whose CLI is installed. Falls back to the first in the
/// order, so the not-found error names it.
pub fn default_kind(os: &str, installed: impl Fn(&str) -> bool) -> Kind {
    let order = runtime_order(os);
    order
        .iter()
        .copied()
        .find(|k| installed(k.cli()))
        .unwrap_or(order[0])
}

/// A container engine driven through its CLI.
#[derive(Clone)]
pub struct Engine {
    kind: Kind,
    exec: Arc<dyn Exec>,
    poll: Duration,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine").field("kind", &self.kind).finish()
    }
}

impl Engine {
    pub fn new(kind: Kind) -> Self {
        Self::with_exec(kind, Arc::new(System))
    }

    /// An engine whose commands `exec` answers.
    pub fn with_exec(kind: Kind, exec: Arc<dyn Exec>) -> Self {
        Self {
            kind,
            exec,
            poll: Duration::from_millis(500),
        }
    }

    /// How long to wait between polls for a network another run is creating.
    pub fn poll_interval(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// The named engine, or this platform's default.
    pub fn get(name: Option<&str>) -> Result<Self> {
        let kind = match name {
            Some(name) => Kind::from_name(name)?,
            None => default_kind(std::env::consts::OS, |cli| System.which(cli)),
        };
        Ok(Self::new(kind))
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn cli(&self) -> &'static str {
        self.kind.cli()
    }

    fn argv(&self, args: &[&str]) -> Vec<String> {
        std::iter::once(self.cli())
            .chain(args.iter().copied())
            .map(String::from)
            .collect()
    }

    /// The engine's CLI with `args`, output captured.
    fn capture(&self, args: &[&str]) -> Captured {
        self.exec.run(&self.argv(args), true)
    }

    /// The engine's CLI with `args`, output on the terminal.
    fn attached(&self, args: &[&str]) -> Captured {
        self.exec.run(&self.argv(args), false)
    }

    // --- preflight ---------------------------------------------------------------------------

    pub fn require(&self) -> Result<()> {
        if !self.exec.which(self.cli()) {
            return fail(format!(
                "`{}` not found on PATH. {}",
                self.cli(),
                self.kind.install_hint()
            ));
        }
        self.require_service()
    }

    /// Whether the engine's service answers.
    fn require_service(&self) -> Result<()> {
        match self.kind {
            Kind::Apple => {
                // It exits 1 when stopped, printing "apiserver is not running": the word alone
                // does not tell.
                let st = self.capture(&["system", "status"]);
                if !st.success()
                    || !st.stdout.contains("running")
                    || st.stdout.contains("not running")
                {
                    return fail(
                        "container system is not running. Start it with: container system start",
                    );
                }
            }
            Kind::Docker => {
                let r = self.capture(&["info", "--format", "{{.ServerVersion}}"]);
                if !r.success() {
                    let systemd = self.exec.which("systemctl");
                    return fail(docker_daemon_error(
                        &r.stderr,
                        std::env::consts::OS,
                        systemd,
                    ));
                }
            }
        }
        Ok(())
    }

    /// [`Engine::require`], plus whatever else this engine needs to start an agent. Only `run`
    /// needs it: the other verbs need the engine to answer, no more.
    pub fn require_run(&self) -> Result<()> {
        self.require()?;
        // The snap reports its base as the daemon's OS, whatever the host runs: "Ubuntu Core 24"
        // on an Ubuntu 24.04 host. DockerRootDir would also tell, but the snap lets a user move it.
        if self.kind == Kind::Docker
            && self
                .capture(&["info", "--format", "{{.OperatingSystem}}"])
                .stdout
                .starts_with("Ubuntu Core")
        {
            return fail(
                "docker is the snap package, which cannot run an agent. Its AppArmor profile \
                 blocks every exec under --security-opt no-new-privileges, and its /tmp is not \
                 this host's. Install Docker Engine from docs.docker.com/engine/install/.",
            );
        }
        Ok(())
    }

    // --- the engine's own service ------------------------------------------------------------

    /// One line: whether this engine can take a container right now. Never fails, since it
    /// exists to report a broken engine.
    pub fn service_status(&self) -> String {
        match self.require() {
            Ok(()) => format!("{} is running", self.cli()),
            Err(e) => e.0,
        }
    }

    fn unmanaged(&self) -> Error {
        Error(format!(
            "{}'s service is managed outside sanduk. Start or stop it the way your system does.",
            self.cli()
        ))
    }

    pub fn service_start(&self) -> Result<()> {
        match self.kind {
            Kind::Apple if self.attached(&["system", "start"]).success() => Ok(()),
            Kind::Apple => fail("could not start the container service"),
            Kind::Docker => Err(self.unmanaged()),
        }
    }

    /// For Apple's engine, this stops the service for everything on the machine.
    pub fn service_stop(&self) -> Result<()> {
        match self.kind {
            Kind::Apple if self.attached(&["system", "stop"]).success() => Ok(()),
            Kind::Apple => fail("could not stop the container service"),
            Kind::Docker => Err(self.unmanaged()),
        }
    }

    // --- images ------------------------------------------------------------------------------

    pub fn image_exists(&self, image: &str) -> bool {
        match self.kind {
            Kind::Apple => {
                let out = self.capture(&["image", "list"]);
                if !out.success() {
                    return false;
                }
                let (name, tag) = split_tag(image);
                let name = short_name(name);
                out.stdout.lines().skip(1).any(|line| {
                    let f: Vec<_> = line.split_whitespace().collect();
                    f.len() >= 2 && short_name(f[0]) == name && f[1] == tag
                })
            }
            // inspect rather than a parsed listing: it answers the same for a tag, a digest and
            // an id, and the exit status is the answer.
            Kind::Docker => self.capture(&["image", "inspect", image]).success(),
        }
    }

    /// Every `repository:tag` this engine holds for one repository.
    pub fn image_tags(&self, repository: &str) -> Vec<String> {
        match self.kind {
            Kind::Apple => {
                let out = self.capture(&["image", "list"]);
                if !out.success() {
                    return Vec::new();
                }
                let suffix = format!("/{repository}");
                out.stdout
                    .lines()
                    .skip(1)
                    .filter_map(|line| {
                        let f: Vec<_> = line.split_whitespace().collect();
                        (f.len() >= 2 && (f[0] == repository || f[0].ends_with(&suffix)))
                            .then(|| format!("{}:{}", f[0], f[1]))
                    })
                    .collect()
            }
            Kind::Docker => {
                let fmt = "{{.Repository}}:{{.Tag}}";
                let out = self.capture(&["image", "ls", "--format", fmt, repository]);
                if !out.success() {
                    return Vec::new();
                }
                out.stdout
                    .lines()
                    .filter(|line| !line.ends_with(":<none>"))
                    .map(String::from)
                    .collect()
            }
        }
    }

    /// Whether an image was deleted.
    pub fn delete_image(&self, image: &str) -> bool {
        self.capture(&["image", self.kind.delete_verb(), image])
            .success()
    }

    /// Flags `build_image` adds for this engine, for the calling user.
    pub fn build_args(&self) -> Vec<String> {
        let (uid, gid) = ids();
        build_args(self.kind, uid, gid)
    }

    /// The agent's uid from the image's label, or `None` if it has none.
    pub fn image_uid(&self, image: &str) -> Option<u32> {
        if self.kind != Kind::Docker {
            return None;
        }
        let fmt = "{{json .Config.Labels}}";
        let r = self.capture(&["image", "inspect", "--format", fmt, image]);
        let labels: Value = serde_json::from_str(&r.stdout).ok()?;
        match labels.get(AGENT_UID_LABEL)? {
            Value::String(s) => s.parse().ok(),
            Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
            _ => None,
        }
    }

    /// Builds `image` from `containerfile`, with its directory as the context. Output goes to
    /// the terminal.
    pub fn build_image(&self, image: &str, containerfile: &Path) -> Result<()> {
        let cf = match std::fs::canonicalize(containerfile) {
            Ok(cf) if cf.is_file() => cf,
            _ => return fail(format!("no Containerfile at {}", containerfile.display())),
        };
        let argv = build_argv(self.cli(), &self.build_args(), image, &cf);
        let r = self.exec.run(&argv, false);
        if !r.success() {
            return fail(format!("build failed ({})", exit(&r)));
        }
        Ok(())
    }

    // --- networks ----------------------------------------------------------------------------

    /// The network's addresses, or `None` if it does not exist or reports none.
    pub fn network_info(&self, name: &str) -> Option<Network> {
        let r = self.capture(&["network", "inspect", name]);
        if !r.success() {
            return None;
        }
        let json: Value = serde_json::from_str(&r.stdout).ok()?;
        let (gateway, subnet) = match self.kind {
            Kind::Apple => {
                let st = json.get(0)?.get("status")?;
                (st.get("ipv4Gateway")?, st.get("ipv4Subnet")?)
            }
            Kind::Docker => {
                let config = json.get(0)?.get("IPAM")?.get("Config")?.get(0)?;
                (config.get("Gateway")?, config.get("Subnet")?)
            }
        };
        Some(Network {
            gateway: gateway.as_str()?.to_string(),
            subnet: subnet.as_str()?.to_string(),
        })
    }

    /// Whether `name` has no route off the host, or `None` if this engine does not report it.
    /// Whether Apple's CLI reports the mode of an existing network is unconfirmed.
    pub fn network_internal(&self, name: &str) -> Option<bool> {
        if self.kind != Kind::Docker {
            return None;
        }
        let r = self.capture(&["network", "inspect", name]);
        if !r.success() {
            return None;
        }
        let json: Value = serde_json::from_str(&r.stdout).ok()?;
        json.get(0)?.get("Internal")?.as_bool()
    }

    /// Creates `name` if it is not already there. Returns its addresses, and whether this call
    /// created it.
    ///
    /// `internal` is the only difference between sanduk's two relayed modes: without a route off
    /// the host, the relay is the one address a container can reach.
    ///
    /// A network that is already there is reused, so what it was created with is what a sealed
    /// run gets. Reuse is refused where the engine reports a routable network under a name a
    /// sealed run asked for.
    pub fn ensure_network(&self, name: &str, internal: bool) -> Result<(Network, bool)> {
        if let Some(info) = self.network_info(name) {
            if internal && self.network_internal(name) == Some(false) {
                return fail(format!(
                    "network {name} already exists with a route off the host, and a sealed run \
                     must have none. Delete it (`{} network {} {name}`) or name another with \
                     --proxy-network",
                    self.cli(),
                    self.kind.delete_verb()
                ));
            }
            return Ok((info, false));
        }
        let mut args = vec!["network", "create"];
        if internal {
            args.push("--internal");
        }
        args.push(name);
        let r = self.capture(&args);
        if !r.success() {
            // Two runs that both found no network both create it, and the second create fails.
            // The network it wanted is the first run's: wait for it. Apple's engine creates one
            // in under 0.1s.
            for _ in 0..20 {
                if let Some(info) = self.network_info(name) {
                    return Ok((info, false));
                }
                std::thread::sleep(self.poll);
            }
            return fail(format!(
                "could not create network {name}: {}",
                r.stderr.trim()
            ));
        }
        match self.network_info(name) {
            Some(info) => Ok((info, true)),
            None => fail(format!("network {name} created but has no address")),
        }
    }

    /// Whether a network was deleted.
    pub fn delete_network(&self, name: &str) -> bool {
        self.capture(&["network", self.kind.delete_verb(), name])
            .success()
    }

    /// Starts a placeholder container so the host bridge exists, and returns it for teardown;
    /// `None` when the engine does not need one.
    ///
    /// Without it the relay cannot bind the gateway address. `seconds` is how long it holds; the
    /// caller sizes it to the run, since a holder that exits first takes the bridge with it.
    pub fn hold_network_up(
        &self,
        network: &str,
        image: &str,
        seconds: u64,
    ) -> Result<Option<String>> {
        if !self.kind.needs_network_holder() {
            return Ok(None);
        }
        let spec = ContainerSpec {
            cpus: 1,
            memory: "256M".into(),
            network: Some(network.into()),
            detach: true,
            entrypoint: Some("sleep".into()),
            command: vec![seconds.to_string()],
            ..ContainerSpec::new(format!("{CONTAINER_PREFIX}hold-{}", short_id()), image)
        };
        let r = self.exec.run(&self.run_argv(&spec), true);
        if !r.success() {
            return fail(format!(
                "could not start network holder: {}",
                r.stderr.trim()
            ));
        }
        Ok(Some(spec.name))
    }

    /// Whether `url` answers 204 from inside a network: through `via`, with curl, which every
    /// shipped image carries. A host firewall that drops the connection makes this false within
    /// `timeout` seconds, where the agent would have hung until its own deadline.
    pub fn probe(&self, via: &Via, url: &str, timeout: u32) -> Result<bool> {
        let curl = [
            "-s".to_string(),
            "-o".into(),
            "/dev/null".into(),
            "-w".into(),
            "%{http_code}".into(),
            "--max-time".into(),
            timeout.to_string(),
            url.to_string(),
        ];
        let argv = match via {
            Via::Holder(holder) => {
                let mut argv = self.argv(&["exec", holder, "curl"]);
                argv.extend(curl);
                argv
            }
            Via::Container {
                name,
                network,
                image,
            } => self.run_argv(&ContainerSpec {
                cpus: 1,
                memory: "256M".into(),
                network: Some(network.to_string()),
                entrypoint: Some("curl".into()),
                command: curl.to_vec(),
                ..ContainerSpec::new(*name, *image)
            }),
        };
        let r = self.exec.run(&argv, true);
        if let Via::Container { name, .. } = via {
            self.destroy(name)?;
        }
        Ok(r.stdout.trim() == "204")
    }

    // --- containers --------------------------------------------------------------------------

    /// Containers whose name starts with `prefix`, running or not.
    ///
    /// Fails when the engine cannot answer. An empty list has to mean there are none: a sweep
    /// drops a record once the engine says it no longer holds the containers that record names,
    /// and a stopped daemon answering "none" would drop the record of a container still holding
    /// a key.
    pub fn list_containers(&self, prefix: &str) -> Result<Vec<Container>> {
        let found = match self.kind {
            Kind::Apple => {
                // Columns, because this CLI has no --format. ID IMAGE OS ARCH STATE ...
                let r = self.capture(&["list", "-a"]);
                if !r.success() {
                    return fail(format!("`container list -a` failed: {}", r.stderr.trim()));
                }
                r.stdout
                    .lines()
                    .skip(1)
                    .filter_map(|line| {
                        let f: Vec<_> = line.split_whitespace().collect();
                        (f.len() >= 5).then(|| Container {
                            name: f[0].into(),
                            image: f[1].into(),
                            state: f[4].into(),
                        })
                    })
                    .collect::<Vec<_>>()
            }
            Kind::Docker => {
                // --format over columns: a named field cannot shift under a value that contains a
                // space, and an unknown field fails loudly at the template.
                let fmt = "{{.Names}}\t{{.Image}}\t{{.State}}";
                let r = self.capture(&["ps", "-a", "--format", fmt]);
                if !r.success() {
                    return fail(format!("`docker ps -a` failed: {}", r.stderr.trim()));
                }
                r.stdout
                    .lines()
                    .filter_map(|line| {
                        let f: Vec<_> = line.split('\t').collect();
                        (f.len() == 3).then(|| Container {
                            name: f[0].into(),
                            image: f[1].into(),
                            state: f[2].into(),
                        })
                    })
                    .collect()
            }
        };
        Ok(found
            .into_iter()
            .filter(|c| c.name.starts_with(prefix))
            .collect())
    }

    /// An interactive shell in `image`, mounting nothing and joining no network. Not
    /// [`Engine::run_argv`]: that builds no tty.
    pub fn shell_argv(&self, image: &str) -> Vec<String> {
        self.argv(&["run", "--rm", "-it", "--entrypoint", "sh", image])
    }

    pub fn run_argv(&self, spec: &ContainerSpec) -> Vec<String> {
        let mut argv = self.argv(&["run", "--name", &spec.name]);
        argv.extend(["--cpus".into(), spec.cpus.to_string()]);
        argv.extend(["--memory".into(), spec.memory.clone()]);
        argv.extend(self.kind.hardening().iter().map(|s| s.to_string()));
        if spec.detach {
            argv.push("-d".into());
        }
        if spec.stdin {
            // Without it the engine gives the container no stdin at all, and a pipe into the
            // agent is closed before the first turn.
            argv.push("-i".into());
        }
        if let Some(user) = &spec.user {
            argv.extend(["--user".into(), user.clone()]);
        }
        if let Some((host, dest)) = &spec.mount {
            argv.extend([
                "-v".into(),
                format!("{}:{dest}", host.display()),
                "-w".into(),
                dest.clone(),
            ]);
        }
        for mount in &spec.mounts {
            argv.extend(mount.argv());
        }
        // Bare -e NAME: the engine inherits the value from this process, so the value stays out
        // of the argv and out of the host's process list.
        for key in &spec.inherit_env {
            argv.extend(["-e".into(), key.clone()]);
        }
        for kv in &spec.env {
            argv.extend(["-e".into(), kv.clone()]);
        }
        if let Some(network) = &spec.network {
            argv.extend(["--network".into(), network.clone()]);
        }
        if let Some(entrypoint) = &spec.entrypoint {
            argv.extend(["--entrypoint".into(), entrypoint.clone()]);
        }
        if let Some(oci) = &spec.oci_runtime {
            argv.extend(["--runtime".into(), oci.clone()]);
        }
        argv.push(spec.image.clone());
        argv.extend(spec.command.iter().cloned());
        argv
    }

    /// Stops a container, leaving it on disk. Already stopped is not an error.
    pub fn stop(&self, name: &str) {
        self.capture(&["stop", name]);
    }

    /// Stops and deletes a container. Fails when the delete does, so a caller keeping a record
    /// of the container keeps it: `inspect` on a container left behind shows its environment.
    pub fn destroy(&self, name: &str) -> Result<()> {
        self.stop(name);
        let r = self.capture(&[self.kind.delete_verb(), name]);
        if !r.success() {
            return fail(format!("could not delete {name}: {}", r.stderr.trim()));
        }
        Ok(())
    }
}

/// A native Docker daemon keeps host ownership on a bind mount, so an agent at uid 1000 cannot
/// write a workdir owned by uid 1001. Root keeps the image's default: uid 0 inside would be root
/// there. Apple's mounts already let uid 1000 write a directory the host user owns.
fn build_args(kind: Kind, uid: u32, gid: u32) -> Vec<String> {
    if kind != Kind::Docker || uid == 0 {
        return Vec::new();
    }
    vec![
        "--build-arg".into(),
        format!("AGENT_UID={uid}"),
        "--build-arg".into(),
        format!("AGENT_GID={gid}"),
    ]
}

fn build_argv(cli: &str, build_args: &[String], image: &str, cf: &Path) -> Vec<String> {
    let context = cf.parent().unwrap_or(Path::new("/"));
    [cli, "build"]
        .into_iter()
        .map(String::from)
        .chain(build_args.iter().cloned())
        .chain(["-t".into(), image.into(), "-f".into()])
        .chain([cf.display().to_string(), context.display().to_string()])
        .collect()
}

#[cfg(unix)]
fn ids() -> (u32, u32) {
    // SAFETY: getuid and getgid take no arguments and cannot fail.
    unsafe { (libc::getuid(), libc::getgid()) }
}

#[cfg(not(unix))]
fn ids() -> (u32, u32) {
    (0, 0)
}

/// `name:tag`, with `latest` for a bare name. The tag is after the last colon, unless that colon
/// is a registry's port: `localhost:5000/x` has no tag.
fn split_tag(image: &str) -> (&str, &str) {
    match image.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name, tag),
        _ => (image, "latest"),
    }
}

/// A Docker Hub name as Apple's engine lists it: `docker.io/library/alpine` is `alpine`.
fn short_name(name: &str) -> &str {
    let name = name.strip_prefix("docker.io/").unwrap_or(name);
    name.strip_prefix("library/").unwrap_or(name)
}

fn exit(r: &Captured) -> String {
    r.code
        .map_or_else(|| "killed by a signal".into(), |c| format!("exit {c}"))
}

/// Six hex digits, for a holder's name. Unique enough among the holders alive at once.
fn short_id() -> String {
    use std::hash::BuildHasher;
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let hash = std::collections::hash_map::RandomState::new().hash_one((seed, std::process::id()));
    format!("{:06x}", hash & 0xff_ffff)
}

/// Polls until `gateway` is bindable on this host, as it is once the engine has the bridge up.
pub fn wait_for_gateway(gateway: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpListener::bind((gateway, 0)).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_builds_the_agent_as_the_caller() {
        let args = build_args(Kind::Docker, 1001, 121);
        let argv = build_argv(
            "docker",
            &args,
            "sanduk:latest",
            Path::new("/r/Containerfile"),
        );
        assert_eq!(argv[2..4], ["--build-arg", "AGENT_UID=1001"]);
        assert_eq!(argv[4..6], ["--build-arg", "AGENT_GID=121"]);
        assert_eq!(argv.last().unwrap(), "/r");
    }

    #[test]
    fn root_builds_the_default_agent_user() {
        assert!(build_args(Kind::Docker, 0, 0).is_empty());
    }

    #[test]
    fn apple_passes_no_build_args() {
        assert!(build_args(Kind::Apple, 1001, 121).is_empty());
    }

    #[test]
    fn holder_ids_are_six_hex_digits_and_differ() {
        let (a, b) = (short_id(), short_id());
        assert_eq!(a.len(), 6);
        assert_ne!(a, b);
    }
}
