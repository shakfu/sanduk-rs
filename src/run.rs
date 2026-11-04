//! One run: an agent in a disposable container, with the API key held on the host.
//!
//! The caller owns the agent's process. [`Sanduk::plan`](crate::Sanduk::plan) checks a request
//! and acquires nothing. [`Plan::start`] builds the image, brings up the network, the holder and
//! the relay, makes the clone, and records what it acquired. The caller starts
//! [`Live::command`] its own way and waits for it. [`Live::finish`] runs the check and the clone's
//! return, and tears everything down.
//!
//! The API key is read from the provider's variable and passed with the bare-name `-e` form, so
//! the engine inherits the value from the agent's process and it never appears in an argv. Behind
//! the relay the container gets a per-run token instead, and the key stays in this process.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, LazyLock, mpsc};
use std::time::{Duration, Instant};

use regex::Regex;

use sanduk_container::{CONTAINER_PREFIX, ContainerSpec, Engine, Mount, Via, wait_for_gateway};

use crate::agent::{self, Agent, DEFAULT_AGENT, Endpoint, Options as AgentOptions, REPORT_NAME};
use crate::catalog::Origin;
use crate::clone::{self, Clone};
use crate::error::{Detail, Error, ErrorKind, Result};
use crate::observe::{Note, Observer, info};
use crate::preflight::{firewall_warning, host_listener_warning, validate_key};
use crate::providers::{DEFAULT_PROVIDER, Provider, Scheme, get_provider, parse_upstream};
use crate::recipes::{self, Recipe, Rendered};
use crate::relay::{Config, PING, Relay, Stats};
use crate::util::{Dirs, open_unfollowed, random_hex, shell_join, token};
use sanduk_container::runs::{self, Reaper, Run, claim};

/// Where the workdir lands inside the container.
const WORKDIR_DEST: &str = "/work";
/// Where the socket lands, and the variable naming it. Outside the work mount: the agent writes
/// the work mount, and a socket it can replace is a socket it can put a listener of its own at.
const SOCKET_DEST: &str = "/run/sanduk/agent.sock";
const SOCKET_ENV: &str = "SANDUK_SOCKET";
/// The network holder outlives the run by this much, so teardown happens while the bridge is up.
const HOLDER_MARGIN: u64 = 300;
/// A week. The guard against `--timeout 9000000` being a typo that parks a holder for months.
const MAX_TIMEOUT: u64 = 7 * 86400;
/// How long the return step may take. Outside the timeout: the agent's work is at stake.
pub const RETURN_SECONDS: u64 = 300;
/// How long the probe waits for the relay. A reachable relay answers in milliseconds.
const PROBE_SECONDS: u32 = 5;

/// Whether the host keeps the key, and whether the container has a route off the host. The
/// fourth combination, the key inside and no route to any provider, is a run that cannot call
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// The container holds the key and reaches anything.
    #[default]
    Open,
    /// The key stays on the host; the container still reaches anything.
    KeySafe,
    /// The key stays on the host, and the container has no route off it.
    Sealed,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Open, Mode::KeySafe, Mode::Sealed];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Open => "open",
            Mode::KeySafe => "key-safe",
            Mode::Sealed => "sealed",
        }
    }

    pub fn from_name(name: &str) -> Result<Mode> {
        Mode::ALL
            .into_iter()
            .find(|m| m.name() == name)
            .ok_or_else(|| {
                Error::of_kind(
                    ErrorKind::Refused,
                    format!("no mode {name:?}: open, key-safe or sealed"),
                )
            })
    }

    pub fn relayed(self) -> bool {
        self != Mode::Open
    }

    pub fn egress(self) -> bool {
        self != Mode::Sealed
    }

    /// The network the mode creates, if any.
    pub fn network(self) -> Option<&'static str> {
        match self {
            Mode::Open => None,
            Mode::KeySafe => Some("sanduk-open"),
            Mode::Sealed => Some("sanduk-net"),
        }
    }
}

/// Every network a mode creates, plus one the caller named.
pub fn mode_networks(named: Option<&str>) -> Vec<String> {
    let mut found: Vec<String> = named.into_iter().map(String::from).collect();
    for mode in Mode::ALL {
        if let Some(n) = mode.network()
            && !found.iter().any(|f| f == n)
        {
            found.push(n.into());
        }
    }
    found
}

/// What the image is built from. See docs/dev/kits.md.
#[derive(Debug, Clone, Default)]
pub struct ImageSpec {
    /// A catalogue name or a path to an agent file. Default: [`DEFAULT_AGENT`], or the recipe's.
    pub agent: Option<String>,
    /// The recipe that builds the image. Default: the agent's.
    pub recipe: Option<String>,
    /// Kits added to the recipe, unpinned.
    pub kits: Vec<String>,
    /// The image to use. Default: built from the agent's recipe.
    pub image: Option<String>,
    /// Build from this Containerfile instead of a recipe.
    pub containerfile: Option<PathBuf>,
}

/// One run's request. Field names follow `sanduk run`'s flags, which the errors name.
#[derive(Debug, Clone)]
pub struct Options {
    /// The prompt. `None` with `verify` set runs only the check: [`Plan::check`].
    pub task: Option<String>,
    /// Append the write-a-REPORT.md instruction to the task.
    pub report_instruction: bool,
    /// The host directory the agent works in.
    pub workdir: PathBuf,
    /// Mount the workdir at its own host path instead of /work.
    pub at_host_path: bool,
    /// Run on a clone of the workdir's repository, made at this path, which must not exist. The
    /// agent's commits come back as branch `sanduk/<id>`.
    pub clone_into: Option<PathBuf>,
    /// The run's id: it names the containers and the clone's branch. Default: 8 random hex digits.
    pub id: Option<String>,
    /// More host directories, as `HOST:DEST[:ro]`.
    pub mounts: Vec<String>,
    /// After the agent, run this with `sh -c` in a second container: same image, mounts and
    /// network, no credential.
    pub verify: Option<String>,

    /// `apple` or `docker`. Default: this platform's first installed.
    pub runtime: Option<String>,
    pub image: ImageSpec,
    /// Rebuild the image first.
    pub rebuild: bool,

    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_turns: Option<u32>,
    pub allowed_tools: Option<String>,
    pub permission_mode: Option<String>,
    pub bare: bool,

    pub cpus: u32,
    pub memory: String,
    /// How long the agent and the check may take, together.
    pub timeout: Duration,
    /// More variables, as `K=V`.
    pub env: Vec<String>,
    /// Point the agent at this endpoint directly, without a relay.
    pub base_url: Option<String>,
    /// Attach to this container network.
    pub network: Option<String>,
    /// `UID[:GID]`, or `host`. Docker only.
    pub user: Option<String>,
    /// One unix socket into the container, as `HOST[:DEST]`.
    pub socket: Option<String>,
    /// More names for the socket's container path.
    pub socket_env: Vec<String>,
    /// Give the agent the caller's stdin (`-i`).
    pub stdin: bool,
    /// Docker only: the OCI runtime, e.g. runsc.
    pub oci_runtime: Option<String>,

    pub provider: String,
    pub upstream: Option<String>,
    pub insecure_upstream: bool,
    pub agent_key_env: Option<String>,
    pub agent_base_url_env: Option<String>,

    pub mode: Mode,
    pub proxy_network: Option<String>,
    pub budget: Option<f64>,
    pub proxy_port: u16,
    pub proxy_allow_path: Vec<String>,
    pub allow_model: Vec<String>,
    pub max_tokens_cap: Option<u64>,
    pub log_bodies: bool,
    pub log_dir: PathBuf,
    /// Skip the one cheap request that checks the key.
    pub skip_key_check: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            task: None,
            report_instruction: true,
            workdir: PathBuf::from("./work"),
            at_host_path: false,
            clone_into: None,
            id: None,
            mounts: Vec::new(),
            verify: None,
            runtime: None,
            image: ImageSpec::default(),
            rebuild: false,
            model: None,
            effort: None,
            max_turns: None,
            allowed_tools: None,
            permission_mode: None,
            bare: false,
            cpus: 4,
            memory: "4G".into(),
            timeout: Duration::from_secs(900),
            env: Vec::new(),
            base_url: None,
            network: None,
            user: None,
            socket: None,
            socket_env: Vec::new(),
            stdin: false,
            oci_runtime: None,
            provider: DEFAULT_PROVIDER.into(),
            upstream: None,
            insecure_upstream: false,
            agent_key_env: None,
            agent_base_url_env: None,
            mode: Mode::Open,
            proxy_network: None,
            budget: None,
            proxy_port: 0,
            proxy_allow_path: Vec::new(),
            allow_model: Vec::new(),
            max_tokens_cap: None,
            log_bodies: false,
            log_dir: PathBuf::from("./sanduk-logs"),
            skip_key_check: false,
        }
    }
}

/// The mode, with the network it uses and the timeout checked.
struct Resolved {
    mode: Mode,
    network: String,
    timeout: u64,
}

fn resolve_mode(opts: &Options) -> Result<Resolved> {
    let timeout = opts.timeout.as_secs();
    if timeout == 0 {
        return Err(Error::about("timeout", "must be at least a second"));
    }
    if timeout > MAX_TIMEOUT {
        return Err(Error::about(
            "timeout",
            format!(
                "{timeout}s is longer than a week. The network holder is started for the run's \
                 length, so a typo here parks a container for that long. Give at most {MAX_TIMEOUT}s"
            ),
        ));
    }
    if opts.budget.is_some_and(|b| b <= 0.0 || b.is_nan()) {
        return Err(Error::about(
            "budget",
            "must be a positive number of dollars",
        ));
    }
    let mode = opts.mode;
    let network = match &opts.proxy_network {
        None => mode.network().unwrap_or("sanduk-net").to_string(),
        Some(named) => {
            // An existing network is reused as it was created, and only Docker reports whether
            // that was with a route off the host. Naming one mode's default under the other is how
            // a sealed run silently kept its egress.
            if let Some(crossed) = Mode::ALL
                .into_iter()
                .find(|m| m.network() == Some(named.as_str()) && m.egress() != mode.egress())
            {
                return Err(Error::about(
                    "proxy_network",
                    format!(
                        "{named} is the default network for mode {}, which {} route off the host. \
                         Mode {} needs the opposite; name another network",
                        crossed.name(),
                        if crossed.egress() { "has a" } else { "has no" },
                        mode.name()
                    ),
                ));
            }
            named.clone()
        }
    };
    Ok(Resolved {
        mode,
        network,
        timeout,
    })
}

/// The image a command uses, and what builds it: a rendered recipe, or a Containerfile.
pub struct Image {
    pub tag: String,
    pub containerfile: Option<PathBuf>,
    pub recipe: Option<Recipe>,
    pub rendered: Option<Rendered>,
}

/// The agent and its image, before any provider is known.
pub fn resolve_image(
    dirs: &Dirs,
    spec: &ImageSpec,
    engine: &Engine,
    obs: &dyn Observer,
) -> Result<(Agent, Image)> {
    if (spec.recipe.is_some() || !spec.kits.is_empty())
        && (spec.image.is_some() || spec.containerfile.is_some())
    {
        let named = if spec.image.is_some() {
            "image.image"
        } else {
            "image.containerfile"
        };
        return Err(Error::about(
            named,
            "cannot be combined with a recipe or kits, which build an image of their own",
        ));
    }
    let mut recipe = spec
        .recipe
        .as_deref()
        .map(|name| recipes::resolve(dirs, name, &spec.kits, obs))
        .transpose()?;
    if let (Some(recipe), Some(agent)) = (&recipe, &spec.agent)
        && *agent != recipe.agent
    {
        return Err(Error::about(
            "image.agent",
            format!(
                "{agent} is not what recipe {} builds ({}); drop the agent or choose another recipe",
                recipe.name, recipe.agent
            ),
        ));
    }
    let name = match &recipe {
        Some(recipe) => recipe.agent.clone(),
        None => spec.agent.clone().unwrap_or_else(|| DEFAULT_AGENT.into()),
    };
    let agent = agent::get(dirs, &name)?;

    if let Some(cf) = &spec.containerfile {
        let tag = spec
            .image
            .clone()
            .or_else(|| agent.image.clone())
            .unwrap_or_else(|| format!("{CONTAINER_PREFIX}{}:custom", agent.name));
        let image = Image {
            tag,
            containerfile: Some(cf.clone()),
            recipe: None,
            rendered: None,
        };
        return Ok((agent, image));
    }
    if recipe.is_none()
        && let Some(own) = &agent.recipe
    {
        recipe = Some(recipes::resolve(dirs, own, &spec.kits, obs)?);
    }
    let Some(recipe) = recipe else {
        if !spec.kits.is_empty() {
            return Err(Error::about(
                "image.kits",
                format!("cannot be added: agent {:?} has no recipe", agent.name),
            ));
        }
        let (Some(image), Some(cf)) = (&agent.image, &agent.containerfile) else {
            return Err(Error::about(
                "image.agent",
                format!(
                    "{:?} names no recipe, image or Containerfile; give a recipe, or an image and a Containerfile",
                    agent.name
                ),
            ));
        };
        let tag = spec.image.clone().unwrap_or_else(|| image.clone());
        let image = Image {
            tag,
            containerfile: Some(cf.clone()),
            recipe: None,
            rendered: None,
        };
        return Ok((agent, image));
    };
    recipes::check(
        &recipe,
        &agent.name,
        agent.skills_dir.as_deref(),
        recipes::host_arch(),
        agent.instructions_file.as_deref(),
    )?;
    let rendered = recipes::render_recipe(
        &recipe,
        agent.skills_dir.as_deref(),
        agent.instructions_file.as_deref(),
    )?;
    let tag = spec
        .image
        .clone()
        .unwrap_or_else(|| recipes::image_tag(&recipe, &rendered, &engine.build_args()));
    let image = Image {
        tag,
        containerfile: None,
        recipe: Some(recipe),
        rendered: Some(rendered),
    };
    Ok((agent, image))
}

/// Builds `image`, with each line of the engine's output sent to `obs` as [`Note::Build`].
pub fn build_image(engine: &Engine, image: &Image, obs: &dyn Observer) -> Result<()> {
    let mut line = |l: &str| obs.note(Note::Build(l.into()));
    match (&image.rendered, &image.containerfile) {
        (Some(rendered), _) => recipes::build(engine, &image.tag, rendered, &mut line),
        (None, Some(cf)) => Ok(engine.build_image_lines(&image.tag, cf, &mut line)?),
        _ => Err(Error::new("nothing to build the image from")),
    }
}

/// What the request selects. None of it depends on the relay being up yet.
struct Selection {
    agent: Agent,
    provider: &'static Provider,
    scheme: Scheme,
    host: String,
    image: Image,
    opts: AgentOptions,
}

impl Selection {
    fn endpoint(&self, root: Option<String>) -> Endpoint<'_> {
        Endpoint {
            provider: self.provider,
            scheme: self.scheme,
            host: self.host.clone(),
            root,
        }
    }
}

/// Resolves the agent and provider together, and refuses an unusable pair.
fn select(
    dirs: &Dirs,
    opts: &Options,
    mode: Mode,
    engine: &Engine,
    obs: &dyn Observer,
) -> Result<Selection> {
    let (agent, image) = resolve_image(dirs, &opts.image, engine, obs)?;
    let provider = get_provider(&opts.provider)?;
    let (scheme, host) = match &opts.upstream {
        Some(url) => parse_upstream(url, opts.insecure_upstream)
            .map_err(|e| Error::about("upstream", e.0))?,
        None => (provider.scheme, provider.host.to_string()),
    };
    if opts.budget.is_some() {
        if provider.cost_field.is_none() {
            return Err(Error::about(
                "budget",
                format!(
                    "cannot be enforced against {}: it reports tokens, not cost. Only openrouter reports cost",
                    provider.name
                ),
            ));
        }
        if !mode.relayed() {
            return Err(Error::about(
                "budget",
                "is counted by the relay, which open mode does not start. Use key-safe or sealed",
            ));
        }
    }
    let agent_opts = AgentOptions {
        model: opts
            .model
            .clone()
            .or(provider.default_model.map(String::from)),
        effort: opts.effort.clone(),
        max_turns: opts.max_turns,
        permission_mode: opts.permission_mode.clone(),
        allowed_tools: opts.allowed_tools.clone(),
        bare: opts.bare,
        relayed: mode.relayed(),
        key_env: opts.agent_key_env.clone(),
        base_url_env: opts.agent_base_url_env.clone(),
    };
    agent.check(&agent_opts, provider)?;
    if let Some(recipe) = &image.recipe {
        check_kits(opts, mode, recipe)?;
    }
    Ok(Selection {
        agent,
        provider,
        scheme,
        host,
        image,
        opts: agent_opts,
    })
}

/// Refuses a kit, or instructions, the run's own options would defeat or break.
fn check_kits(opts: &Options, mode: Mode, recipe: &Recipe) -> Result<()> {
    if !recipe.instructions.is_empty() && opts.bare {
        return Err(Error::about(
            "bare",
            format!(
                "stops the agent reading recipe {}'s instructions",
                recipe.name
            ),
        ));
    }
    for used in &recipe.kits {
        let kit = &used.kit;
        if kit.egress && !mode.egress() {
            return Err(Error::about(
                "mode",
                format!(
                    "sealed has no route for kit {}, which needs the network at run time. Use key-safe, or drop the kit",
                    kit.name
                ),
            ));
        }
        if kit.hook && opts.allowed_tools.is_some() {
            return Err(Error::about(
                "allowed_tools",
                format!(
                    "cannot be combined with kit {}: its hook rewrites commands, and may pass one the allowlist would refuse. Untested, so refused",
                    kit.name
                ),
            ));
        }
        if kit.hook && opts.bare {
            return Err(Error::about(
                "bare",
                format!("drops hooks, and kit {} works through one", kit.name),
            ));
        }
    }
    Ok(())
}

/// A path made absolute, with the symlinks of the part that exists resolved.
pub fn resolve_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut existing = absolute.clone();
    let mut rest = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => rest.push(name.to_os_string()),
            None => break,
        }
        existing.pop();
    }
    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
    for part in rest.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn work_dest(opts: &Options, workdir: &Path) -> String {
    if opts.at_host_path {
        workdir.display().to_string()
    } else {
        WORKDIR_DEST.into()
    }
}

fn expand_home(value: &str) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(value),
    }
}

/// `HOST:DEST[:ro]` per mount. Refused: a host directory that is not there, a destination that
/// is not absolute, and anything at or under the working directory, which would shadow part of
/// what the agent was pointed at.
pub fn parse_mounts(values: &[String], dest_of_work: &str) -> Result<Vec<Mount>> {
    let mut mounts: Vec<Mount> = Vec::new();
    for value in values {
        let parts: Vec<&str> = value.split(':').collect();
        if !(2..=3).contains(&parts.len()) || parts[0].is_empty() || parts[1].is_empty() {
            return Err(Error::about(
                "mounts",
                format!("{value:?}: expected HOST:DEST[:ro]"),
            ));
        }
        let mode = parts.get(2).copied().unwrap_or("rw");
        if mode != "ro" && mode != "rw" {
            return Err(Error::about(
                "mounts",
                format!("{value:?}: mode is ro or rw, not {mode:?}"),
            ));
        }
        let host = resolve_path(&expand_home(parts[0]))?;
        let dest = parts[1];
        if !host.is_dir() {
            return Err(Error::about(
                "mounts",
                format!("{value:?}: {} is not a directory", host.display()),
            ));
        }
        if !dest.starts_with('/') || dest == "/" {
            return Err(Error::about(
                "mounts",
                format!("{value:?}: {dest} is not an absolute path"),
            ));
        }
        if dest == dest_of_work || dest.starts_with(&format!("{dest_of_work}/")) {
            return Err(Error::about(
                "mounts",
                format!(
                    "{value:?}: {dest_of_work} is where the workdir lands; a mount there would shadow the directory the agent was given"
                ),
            ));
        }
        if let Some(other) = mounts.iter().find(|m| m.dest == dest) {
            return Err(Error::about(
                "mounts",
                format!("{value:?}: {dest} already holds {}", other.host.display()),
            ));
        }
        mounts.push(Mount {
            host,
            dest: dest.into(),
            ro: mode == "ro",
        });
    }
    Ok(mounts)
}

/// `HOST[:DEST]`: the one thing in the container that reaches the host, so what it is and where
/// it lands are both checked. It may not land in a mount the agent writes.
fn parse_socket(
    value: Option<&str>,
    dest_of_work: &str,
    workdir: &Path,
    mounts: &[Mount],
) -> Result<Option<Mount>> {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let (host_text, dest) = value.split_once(':').unwrap_or((value, ""));
    let dest = if dest.is_empty() { SOCKET_DEST } else { dest };
    let host = resolve_path(&expand_home(host_text))?;
    if !dest.starts_with('/') || dest == "/" {
        return Err(Error::about(
            "socket",
            format!("{value:?}: {dest} is not an absolute path"),
        ));
    }
    let Ok(meta) = std::fs::metadata(&host) else {
        return Err(Error::about(
            "socket",
            format!("{value:?}: {} is not there", host.display()),
        ));
    };
    if !meta.file_type().is_socket() {
        return Err(Error::about(
            "socket",
            format!("{value:?}: {} is not a unix socket", host.display()),
        ));
    }
    let work = dest_of_work.trim_end_matches('/');
    if dest == dest_of_work || dest.starts_with(&format!("{work}/")) {
        return Err(Error::about(
            "socket",
            format!(
                "{value:?}: {dest} is inside {dest_of_work}, which the agent writes. Mount the socket outside the work mount."
            ),
        ));
    }
    for (other, what) in std::iter::once((workdir, "the workdir"))
        .chain(mounts.iter().map(|m| (m.host.as_path(), "a mount")))
    {
        if host.starts_with(other) {
            return Err(Error::about(
                "socket",
                format!(
                    "{value:?}: {} is inside {}, which {what} puts in the container: the agent could replace the socket with a listener of its own. Keep it outside.",
                    host.display(),
                    other.display()
                ),
            ));
        }
    }
    Ok(Some(Mount {
        host,
        dest: dest.into(),
        ro: false,
    }))
}

// SAFETY: getuid and getgid take no arguments and cannot fail.
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn gid() -> u32 {
    unsafe { libc::getgid() }
}

/// The user, as the engine spells it, or `None` for the image's own user.
fn container_user(opts: &Options, engine: &Engine) -> Result<Option<String>> {
    let Some(value) = opts.user.as_deref().filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if !engine.kind().takes_user() {
        return Err(Error::about(
            "user",
            format!(
                "needs the docker runtime: {} runs the image's own user. Rebuild the image for that uid instead.",
                engine.kind().name()
            ),
        ));
    }
    if value == "host" {
        return Ok(Some(format!("{}:{}", uid(), gid())));
    }
    static UID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+(:\d+)?$").unwrap());
    if !UID.is_match(value) {
        return Err(Error::about(
            "user",
            format!("{value:?}: expected UID[:GID], or `host`"),
        ));
    }
    if value.split(':').next() == Some("0") {
        return Err(Error::about(
            "user",
            "0: sanduk runs no agent as root. The socket's file mode is the gate on it, and root ignores a file mode.",
        ));
    }
    Ok(Some(value.into()))
}

fn requested_uid(opts: &Options) -> Option<u32> {
    match opts.user.as_deref()? {
        "host" => Some(uid()),
        value => value.split(':').next()?.parse().ok(),
    }
}

/// Refuses a socket the agent's uid cannot open. Only a sure refusal stops a run: a mode that
/// grants the group or the world lets in a uid this cannot check.
fn check_socket_reachable(socket: Option<&Mount>, uid: Option<u32>) -> Result<()> {
    let (Some(socket), Some(uid)) = (socket, uid) else {
        return Ok(());
    };
    let meta = std::fs::metadata(&socket.host)?;
    if meta.permissions().mode() & 0o066 != 0 || meta.uid() == uid {
        return Ok(());
    }
    Err(Error {
        kind: ErrorKind::Refused,
        ..Error::about(
            "socket",
            format!(
                "{} is owned by uid {} and its mode admits only its owner, but the agent runs as uid {uid}. Create the socket as that uid, or run as its owner.",
                socket.host.display(),
                meta.uid()
            ),
        )
    })
}

/// Refuses a request-body log the agent could reach: the bodies are the audit trail of what it
/// sent, and the default is relative to the caller's directory, so `-w .` put it inside /work.
fn check_log_dir(log_dir: &Path, workdir: &Path, mounts: &[Mount]) -> Result<()> {
    let resolved = resolve_path(log_dir)?;
    for (host, what) in std::iter::once((workdir, "the workdir"))
        .chain(mounts.iter().map(|m| (m.host.as_path(), "a mount")))
    {
        if resolved.starts_with(host) {
            return Err(Error::about(
                "log_dir",
                format!(
                    "{} is inside {}, which {what} puts in the container: the agent could read and edit the bodies it sent. Point it outside.",
                    resolved.display(),
                    host.display()
                ),
            ));
        }
    }
    Ok(())
}

/// `scheme://host` and the path prefix of a base URL, for the key check.
fn split_url(url: &str) -> (Scheme, String, String) {
    let (scheme, rest) = match url.split_once("://") {
        Some(("http", rest)) => (Scheme::Http, rest),
        Some((_, rest)) => (Scheme::Https, rest),
        None => (Scheme::Https, url),
    };
    let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    (
        scheme,
        host.into(),
        format!("/{path}").trim_end_matches('/').to_string(),
    )
}

/// A checked request. Nothing is acquired until [`start`](Plan::start).
pub struct Plan {
    dirs: Dirs,
    obs: Arc<dyn Observer>,
    opts: Options,
    mode: Resolved,
    engine: Engine,
    sel: Selection,
    /// `None` for a check-only plan.
    task: Option<String>,
    key: String,
    workdir: PathBuf,
    name: String,
    cloned: Option<Clone>,
    work: PathBuf,
    dest: String,
    mounts: Vec<Mount>,
    socket: Option<Mount>,
    user: Option<String>,
    warnings: Vec<String>,
}

impl Plan {
    /// Checks `opts` and acquires nothing. Refusals are [`ErrorKind::Refused`]; a missing key is
    /// [`ErrorKind::Setup`].
    pub(crate) fn new(
        dirs: &Dirs,
        obs: Arc<dyn Observer>,
        engine: Option<Engine>,
        opts: Options,
    ) -> Result<Plan> {
        Self::checked(dirs, obs, engine, &opts)
            .map(|p| Plan { opts, ..p })
            .map_err(|e| e.or_kind(ErrorKind::Refused))
    }

    fn checked(
        dirs: &Dirs,
        obs: Arc<dyn Observer>,
        engine: Option<Engine>,
        opts: &Options,
    ) -> Result<Plan> {
        let mode = resolve_mode(opts)?;
        let engine = match engine {
            Some(engine) => engine,
            None => Engine::get(opts.runtime.as_deref())?,
        };
        if opts.oci_runtime.is_some() && !engine.kind().takes_oci_runtime() {
            return Err(Error::about(
                "oci_runtime",
                format!(
                    "needs the docker runtime: {} has no OCI runtime to swap",
                    engine.kind().name()
                ),
            ));
        }
        let mut warnings = Vec::new();
        if let Some(oci) = &opts.oci_runtime {
            // Hardening is rendered whatever starts the container, and a runtime that ignores a
            // flag says nothing. Kata drops the pid ceiling (kata-containers#13824).
            warnings.push(format!(
                "--pids-limit is enforced by runc, not by every OCI runtime: {oci} may ignore it, and Kata's runtimes do"
            ));
            info(&*obs, warnings[0].clone());
        }
        let sel = select(dirs, opts, mode.mode, &engine, &*obs)?;
        let id = match &opts.id {
            Some(id) => id.clone(),
            None => random_hex(8)?,
        };
        let name = format!("{CONTAINER_PREFIX}{id}");
        let workdir = resolve_path(&opts.workdir)?;
        let dest = work_dest(opts, &workdir);
        let check_only = opts.task.is_none() && opts.verify.is_some();
        let mut plan = Plan {
            dirs: dirs.clone(),
            obs,
            opts: Options::default(),
            mode,
            engine,
            sel,
            task: None,
            key: String::new(),
            workdir: workdir.clone(),
            name,
            cloned: None,
            work: workdir.clone(),
            dest,
            mounts: Vec::new(),
            socket: None,
            user: None,
            warnings,
        };
        if check_only {
            if opts.clone_into.is_some() {
                return Err(Error::about(
                    "clone_into",
                    "needs a task: a check alone changes nothing to fetch back",
                ));
            }
            plan.mounts = parse_mounts(&opts.mounts, &plan.dest)?;
            plan.user = container_user(opts, &plan.engine)?;
            return Ok(plan);
        }
        let mut task = opts.task.clone().ok_or_else(|| {
            Error::about("task", "is missing, and there is no check to run instead")
        })?;
        if task.trim().is_empty() {
            return Err(Error::new("task is empty"));
        }
        if opts.report_instruction {
            task += &agent::report_instruction();
        }
        plan.task = Some(task);

        let provider = plan.sel.provider;
        plan.key = std::env::var(provider.key_env)
            .unwrap_or_default()
            .trim()
            .to_string();
        if plan.key.is_empty() && provider.has_auth {
            return Err(Error::of_kind(
                ErrorKind::Setup,
                format!("{} is not set. export it, then re-run.", provider.key_env),
            ));
        }

        if let Some(copy) = &opts.clone_into {
            let id = plan
                .name
                .strip_prefix(CONTAINER_PREFIX)
                .unwrap_or(&plan.name);
            let c = Clone::plan(&workdir, id, resolve_path(copy)?)?;
            plan.work = c.copy.clone();
            plan.cloned = Some(c);
        }
        // Parsed before anything is acquired: a refused mount after the holder was up left it
        // behind.
        plan.mounts = parse_mounts(&opts.mounts, &plan.dest)?;
        plan.socket = parse_socket(opts.socket.as_deref(), &plan.dest, &workdir, &plan.mounts)?;
        plan.user = container_user(opts, &plan.engine)?;
        if opts.log_bodies {
            check_log_dir(&opts.log_dir, &workdir, &plan.mounts)?;
        }
        if !opts.skip_key_check {
            // Before anything is started, so a bad key cannot leak a container.
            match (&opts.base_url, mode_relayed(&plan)) {
                (Some(base), false) => {
                    let (scheme, host, prefix) = split_url(base);
                    validate_key(&plan.key, scheme, &host, &prefix, provider)
                }
                _ => validate_key(&plan.key, plan.sel.scheme, &plan.sel.host, "", provider),
            }
            .map_err(|e| e.or_kind(ErrorKind::Setup))?;
        }
        Ok(plan)
    }

    /// Whether this plan runs only the check: no task, and `verify` set.
    pub fn is_check_only(&self) -> bool {
        self.task.is_none()
    }

    /// The container's name: `sanduk-<id>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The agent the run starts.
    pub fn agent(&self) -> &Agent {
        &self.sel.agent
    }

    /// The image the run uses.
    pub fn image(&self) -> &Image {
        &self.sel.image
    }

    /// The clone's branch, for a plan with `clone_into`.
    pub fn branch(&self) -> Option<&str> {
        self.cloned.as_ref().map(|c| c.branch.as_str())
    }

    /// What the run would start, as shell lines: the container's command, then `#` comments.
    /// Under a keyed mode it creates the mode's network if it is missing, to learn its gateway.
    pub fn describe(&self) -> Result<String> {
        let mode = &self.mode;
        let network = if mode.mode.relayed() {
            Some(mode.network.clone())
        } else {
            self.opts.network.clone()
        };
        if self.is_check_only() {
            let command = self.opts.verify.as_deref().unwrap_or_default();
            let spec = self.verify_spec(&self.verify_name(), network, command);
            return Ok(shell_join(&self.engine.run_argv(&spec)));
        }
        let mut gateway = String::new();
        if mode.mode.relayed() {
            self.engine.require_run()?;
            self.warn_firewall();
            let (net, _) = self.ensure_network()?;
            gateway = net.gateway;
        }
        let root = if mode.mode.relayed() {
            Some(format!("http://{gateway}:0"))
        } else {
            self.opts.base_url.clone()
        };
        let at = self.sel.endpoint(root);
        let wiring = self.sel.agent.wire(&self.sel.opts, &at);
        let command =
            self.sel
                .agent
                .argv(&self.sel.opts, &at, self.task.as_deref().unwrap(), &wiring)?;
        let spec = self.agent_spec(&wiring, command, network.clone());
        let mut lines = vec![shell_join(&self.engine.run_argv(&spec))];
        if let Some(command) = &self.opts.verify {
            let spec = self.verify_spec(&self.verify_name(), network, command);
            lines.push(format!(
                "# verify: {}",
                shell_join(&self.engine.run_argv(&spec))
            ));
        }
        if mode.mode.relayed() {
            lines.push(format!(
                "# proxy: {gateway} -> {}://{} ({})",
                self.sel.scheme.as_str(),
                self.sel.host,
                self.sel.provider.name
            ));
        }
        if let Some(c) = &self.cloned {
            lines.push(format!(
                "# clone: {} -> {}, returned as branch {}",
                c.origin.display(),
                c.copy.display(),
                c.branch
            ));
        }
        lines.push(format!(
            "# container env: {}=<credential> {}={}",
            wiring.key_env,
            wiring.base_url_env,
            wiring.base_url.as_deref().unwrap_or("<agent default>")
        ));
        Ok(lines.join("\n"))
    }

    fn verify_name(&self) -> String {
        format!("{}-verify", self.name)
    }

    fn warn_firewall(&self) -> Option<String> {
        let warning = firewall_warning()?;
        info(&*self.obs, warning.clone());
        Some(warning)
    }

    fn ensure_network(&self) -> Result<(sanduk_container::Network, bool)> {
        let mode = &self.mode;
        let (net, created) = self
            .engine
            .ensure_network(&mode.network, !mode.mode.egress())?;
        if created {
            info(
                &*self.obs,
                format!(
                    "created {} network {}",
                    if mode.mode.egress() {
                        "routable"
                    } else {
                        "internal"
                    },
                    mode.network
                ),
            );
        }
        Ok((net, created))
    }

    /// The container: the agent's argv, the workdir, mounts, and variables named but not valued.
    fn agent_spec(
        &self,
        wiring: &agent::Wiring,
        command: Vec<String>,
        network: Option<String>,
    ) -> ContainerSpec {
        let opts = &self.opts;
        // The key's value, and the base URL's, come from the agent process's environment: neither
        // appears in the argv or in `ps`.
        let mut inherit = vec![wiring.key_env.clone()];
        if wiring.base_url.is_some() {
            inherit.push(wiring.base_url_env.clone());
        }
        let mut all_mounts = self.mounts.clone();
        let mut env: Vec<String> = wiring.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        if let Some(socket) = &self.socket {
            all_mounts.push(socket.clone());
            env.extend(
                std::iter::once(SOCKET_ENV)
                    .chain(opts.socket_env.iter().map(String::as_str))
                    .map(|n| format!("{n}={}", socket.dest)),
            );
        }
        // The agent's own settings first, so an explicit variable can override one.
        env.extend(opts.env.iter().cloned());
        ContainerSpec {
            command,
            cpus: opts.cpus,
            memory: opts.memory.clone(),
            mount: Some((self.work.clone(), self.dest.clone())),
            mounts: all_mounts,
            inherit_env: inherit,
            env,
            network,
            oci_runtime: opts.oci_runtime.clone(),
            user: self.user.clone(),
            stdin: opts.stdin,
            ..ContainerSpec::new(&self.name, self.sel.image.tag.clone())
        }
    }

    /// The check's container: the agent's image, mounts, network and user, running `sh -c CMD` in
    /// place of the agent. No key, token or agent wiring: the check calls no provider.
    fn verify_spec(&self, name: &str, network: Option<String>, command: &str) -> ContainerSpec {
        let opts = &self.opts;
        ContainerSpec {
            command: vec!["-c".into(), command.into()],
            entrypoint: Some("sh".into()),
            cpus: opts.cpus,
            memory: opts.memory.clone(),
            mount: Some((self.work.clone(), self.dest.clone())),
            mounts: self.mounts.clone(),
            env: opts.env.clone(),
            network,
            oci_runtime: opts.oci_runtime.clone(),
            user: self.user.clone(),
            ..ContainerSpec::new(name, self.sel.image.tag.clone())
        }
    }

    /// Builds the image if it is missing, then refuses one whose agent cannot write `workdir` or
    /// open the socket. Before any container starts: an agent that cannot write its report still
    /// spends the whole run's tokens.
    fn ensure_image(&self, socket: Option<&Mount>) -> Result<()> {
        let (engine, sel, opts, workdir) = (&self.engine, &self.sel, &self.opts, &self.work);
        if opts.rebuild || !engine.image_exists(&sel.image.tag) {
            build_image(engine, &sel.image, &*self.obs)?;
        }
        if !engine.kind().keeps_mount_owner() {
            return Ok(());
        }
        let mut uid = requested_uid(opts);
        if let Some(asked) = uid
            && let Some(built_as) = engine.image_uid(&sel.image.tag)
            && built_as != asked
        {
            // Not a refusal: this is the only way to run an image somebody else built. It is the
            // failure a mid-run "permission denied" would not explain.
            info(
                &*self.obs,
                format!(
                    "user {asked} is not the uid {} was built for ({built_as}); anything the agent writes under its home may be refused",
                    sel.image.tag
                ),
            );
        }
        if uid.is_none() {
            uid = engine.image_uid(&sel.image.tag);
        }
        let Some(uid) = uid.or_else(|| {
            // A shipped image built before the label ran as 1000. Anything else may run as anyone.
            let shipped = sel.agent.origin == Origin::Shipped
                && opts.image.image.is_none()
                && opts.image.containerfile.is_none();
            shipped.then_some(1000)
        }) else {
            return check_socket_reachable(socket, None);
        };
        let owner = match std::fs::metadata(workdir) {
            Err(_) => self::uid(),
            // Group or other write may let the agent in; only a sure refusal stops a run.
            Ok(meta) if meta.permissions().mode() & 0o022 != 0 => return Ok(()),
            Ok(meta) => meta.uid(),
        };
        check_socket_reachable(socket, Some(uid))?;
        if uid == owner {
            return Ok(());
        }
        let fix = if owner == 0 {
            "sanduk builds no agent as uid 0; use a workdir a non-root user owns".to_string()
        } else {
            format!("rebuild the image as uid {owner}")
        };
        Err(Error {
            detail: Some(Detail::AgentUid {
                image: sel.image.tag.clone(),
                agent_uid: uid,
                workdir: workdir.clone(),
                owner,
                runtime: engine.kind().name(),
            }),
            ..Error::of_kind(
                ErrorKind::Refused,
                format!(
                    "{} runs its agent as uid {uid}, which cannot write {} (owned by uid {owner}): {fix}",
                    sel.image.tag,
                    workdir.display()
                ),
            )
        })
    }

    fn cancelled(&self) -> Result<()> {
        if self.obs.cancelled() {
            return Err(Error::of_kind(ErrorKind::Interrupted, "interrupted"));
        }
        Ok(())
    }

    fn note(&self, msg: impl Into<String>) {
        info(&*self.obs, msg);
    }

    /// Deletes what runs whose owner is gone left behind. A record for this plan's runtime uses
    /// this plan's engine.
    fn sweep(&self) {
        let ours = self.engine.kind().name();
        runs::sweep_with(
            &self.dirs.runs(),
            |runtime| {
                if runtime == ours {
                    Ok(self.engine.clone())
                } else {
                    Engine::get(Some(runtime))
                }
            },
            &|m| self.note(m),
        );
    }

    /// Runs only the check, in the container a run with the same options would use, with no
    /// agent, no relay and no key. A base tree checked this way and a head checked after a run are
    /// compared in one environment.
    pub fn check(self) -> Result<Verified> {
        let command = self
            .opts
            .verify
            .clone()
            .ok_or_else(|| Error::about("verify", "is not set"))?;
        let mode = &self.mode;
        let network = if mode.mode.relayed() {
            Some(mode.network.clone())
        } else {
            self.opts.network.clone()
        };
        let name = self.verify_name();
        let spec = self.verify_spec(&name, network, &command);
        let setup = || -> Result<()> {
            self.engine.require_run()?;
            if mode.mode.relayed() {
                self.engine
                    .ensure_network(&mode.network, !mode.mode.egress())?;
            }
            self.ensure_image(None)
        };
        setup().map_err(|e| e.or_kind(ErrorKind::Setup))?;
        self.sweep();
        let mut held = Held::default();
        held.claim(&self.dirs, self.engine.kind().name(), &name, &*self.obs)
            .map_err(|e| e.or_kind(ErrorKind::Setup))?;
        let timeout = Duration::from_secs(mode.timeout);
        let (verified, gone) = verify(&self.engine, None, &spec, &command, timeout, &*self.obs);
        if let Some(record) = held.record.take()
            && gone
        {
            record.release();
        }
        drop(held.reaper.take());
        self.cancelled()?;
        Ok(verified)
    }

    /// Acquires what the run needs: the image, the network, the holder and the relay, the clone,
    /// the run record and its reaper. Gives back whatever it acquired if a step fails.
    pub fn start(mut self) -> Result<Live> {
        if self.is_check_only() {
            return Err(Error::of_kind(
                ErrorKind::Refused,
                "a plan with no task runs only its check: call check",
            ));
        }
        let mut held = Held::default();
        let mut network = self.opts.network.clone();
        let (mut gateway, mut port) = (String::new(), 0u16);
        let mut run_token = String::new();
        let relayed = self.mode.mode.relayed();
        let egress = self.mode.mode.egress();

        let prepared = (|| -> Result<Prepared> {
            if relayed {
                self.engine.require_run()?;
                if let Some(w) = self.warn_firewall() {
                    self.warnings.push(w);
                }
                network = Some(self.mode.network.clone());
                let (net, _) = self.ensure_network()?;
                gateway = net.gateway;
                run_token = token(24)?;
                self.ensure_image(self.socket.as_ref())?;
                held.claim(
                    &self.dirs,
                    self.engine.kind().name(),
                    &self.name,
                    &*self.obs,
                )?;
                // Outlive the run: the holder going first takes the bridge, and the relay's
                // address, with it.
                held.holder = self.engine.hold_network_up(
                    &self.mode.network,
                    &self.sel.image.tag,
                    self.mode.timeout + HOLDER_MARGIN,
                )?;
                if let (Some(holder), Some(record)) = (&held.holder, held.record.as_mut()) {
                    record.add(holder)?;
                }
                if !wait_for_gateway(&gateway, Duration::from_secs(30)) {
                    return Err(Error::new(format!(
                        "{gateway} never became bindable on this host.{}",
                        self.engine.kind().gateway_hint()
                    )));
                }
                if !egress && let Some(warning) = host_listener_warning(&gateway) {
                    self.note(warning.clone());
                    self.warnings.push(warning);
                }
                let relay = self.start_relay(&run_token, &gateway)?;
                port = relay.port();
                held.relay = Some(relay);
                probe_relay(
                    &self.engine,
                    &mut held,
                    &self.mode.network,
                    &self.sel.image.tag,
                    &gateway,
                    port,
                )?;
            }
            let root = if relayed {
                Some(format!("http://{gateway}:{port}"))
            } else {
                self.opts.base_url.clone()
            };
            let at = self.sel.endpoint(root);
            let wiring = self.sel.agent.wire(&self.sel.opts, &at);
            // Behind the relay the container gets the run token; the key stays in this process.
            let secret = if relayed {
                run_token.clone()
            } else {
                self.key.clone()
            };
            let mut child_env = Vec::new();
            if !secret.is_empty() {
                child_env.push((wiring.key_env.clone(), secret));
            }
            if let Some(url) = &wiring.base_url {
                child_env.push((wiring.base_url_env.clone(), url.clone()));
            }
            let task = self.task.as_deref().unwrap_or_default();
            let command = self.sel.agent.argv(&self.sel.opts, &at, task, &wiring)?;
            let spec = self.agent_spec(&wiring, command, network.clone());
            let argv = self.engine.run_argv(&spec);

            self.engine.require_run()?;
            if !relayed {
                self.ensure_image(self.socket.as_ref())?;
            }
            // Not before the key check: until a run is about to start, the previous report is
            // still the only result there is.
            match &self.cloned {
                Some(c) => c.create()?,
                None => std::fs::create_dir_all(&self.workdir)?,
            }
            // Removed, not tested and removed: a test follows a symlink the last agent left.
            match std::fs::remove_file(self.work.join(REPORT_NAME)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            self.sweep();
            if held.record.is_none() {
                held.claim(
                    &self.dirs,
                    self.engine.kind().name(),
                    &self.name,
                    &*self.obs,
                )?;
            }
            self.cancelled()?;
            Ok((argv, child_env))
        })();
        let (argv, env) = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                held.give_back(&self.engine, &*self.obs);
                // The agent has not run: the clone holds nothing of its.
                if let Some(c) = &self.cloned {
                    c.remove();
                }
                return Err(e.or_kind(ErrorKind::Setup));
            }
        };

        if relayed && egress {
            self.note(format!(
                "relay bound to {gateway}:{port} (bridge only); the key stays here, and {} reaches the internet: what the agent sends anywhere else is neither relayed nor recorded",
                self.mode.network
            ));
        } else if relayed {
            self.note(format!(
                "relay bound to {gateway}:{port} (bridge only); {} has no route off the host",
                self.mode.network
            ));
        }
        self.note(format!("{} -> {}", self.name, self.work.display()));
        Ok(Live {
            plan: self,
            held,
            network,
            argv,
            env,
            started: Instant::now(),
            handed_out: false,
            finished: false,
        })
    }

    /// Binds the relay to the bridge address only: unreachable from Wi-Fi or the LAN.
    fn start_relay(&self, run_token: &str, gateway: &str) -> Result<Relay> {
        let opts = &self.opts;
        let mut cfg = Config::new(self.sel.provider, &self.key, run_token);
        let obs = self.obs.clone();
        cfg.note = Arc::new(move |msg| obs.note(Note::Relay(msg.into())));
        cfg.upstream = self.sel.host.clone();
        cfg.scheme = self.sel.scheme;
        if !opts.proxy_allow_path.is_empty() {
            cfg = cfg.allow_paths(opts.proxy_allow_path.clone());
        }
        cfg.log_bodies = opts.log_bodies;
        if opts.log_bodies {
            let dir = resolve_path(&opts.log_dir.join(&self.name))?;
            std::fs::create_dir_all(&dir)?;
            self.note(format!("request bodies -> {}", dir.display()));
            cfg.log_dir = Some(dir);
        }
        if !opts.allow_model.is_empty() {
            cfg.allow_models = Some(opts.allow_model.iter().cloned().collect());
        }
        cfg.max_tokens_cap = opts.max_tokens_cap;
        cfg.budget = opts.budget;
        Ok(Relay::start(cfg, gateway, opts.proxy_port)?)
    }
}

/// The plan's mode is keyed, before the plan is whole.
fn mode_relayed(plan: &Plan) -> bool {
    plan.mode.mode.relayed()
}

/// The engine command, and the variables it inherits from the agent's process.
type Prepared = (Vec<String>, Vec<(String, String)>);

/// What a run holds before its container starts, given back if the start fails.
#[derive(Default)]
struct Held {
    relay: Option<Relay>,
    holder: Option<String>,
    record: Option<Run>,
    /// Last, so it is dropped after the record is released or kept.
    reaper: Option<Reaper>,
}

impl Held {
    /// Claims `name` and starts the reaper that deletes it if this process dies holding it.
    fn claim(&mut self, dirs: &Dirs, runtime: &str, name: &str, obs: &dyn Observer) -> Result<()> {
        let record = claim(&dirs.runs(), runtime, name)?;
        self.reaper = Reaper::start(&record, &|m| info(obs, m));
        self.record = Some(record);
        Ok(())
    }

    fn give_back(&mut self, engine: &Engine, obs: &dyn Observer) {
        if let Some(relay) = self.relay.take() {
            relay.shutdown();
        }
        if let Some(holder) = self.holder.take()
            && let Err(e) = engine.destroy(&holder)
        {
            info(obs, e.0);
        }
        if let Some(record) = self.record.take() {
            record.release();
        }
        drop(self.reaper.take());
    }
}

/// How the agent's process ended, as the caller saw it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ended {
    /// Its exit status, or `None` if it was killed or never started.
    pub exit: Option<i32>,
    /// Whether it finished its task. The check runs only if so.
    pub ok: bool,
    /// Keep the agent's container rather than delete it, for inspection.
    pub keep: bool,
}

impl From<ExitStatus> for Ended {
    fn from(status: ExitStatus) -> Self {
        Ended {
            exit: status.code(),
            ok: status.success(),
            keep: false,
        }
    }
}

/// A started run. Start [`command`](Live::command), wait for it, then [`finish`](Live::finish).
///
/// Dropped without `finish`, it deletes its containers and holder and stops the relay. It runs no
/// return, and keeps the clone once `command` was handed out: the caller chose its path.
pub struct Live {
    plan: Plan,
    held: Held,
    network: Option<String>,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    started: Instant,
    handed_out: bool,
    finished: bool,
}

impl Live {
    /// The agent's process: the engine's CLI, with the key or token and the base URL in its
    /// environment. The caller sets its stdio and starts it.
    pub fn command(&mut self) -> Command {
        self.handed_out = true;
        let mut command = self.plan.engine.command(&self.argv);
        command.envs(self.env.iter().map(|(k, v)| (k, v)));
        command
    }

    /// The argv [`command`](Live::command) runs.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn agent(&self) -> &Agent {
        &self.plan.sel.agent
    }

    pub fn name(&self) -> &str {
        &self.plan.name
    }

    /// Where the agent works: the workdir, or the clone.
    pub fn work(&self) -> &Path {
        &self.plan.work
    }

    /// How long the agent and the check may take, together.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.plan.mode.timeout)
    }

    /// Runs the check and the clone's return while the network is up, deletes the containers,
    /// the holder, stops the relay, and releases the record.
    pub fn finish(mut self, ended: impl Into<Ended>) -> Outcome {
        self.finished = true;
        let ended = ended.into();
        let (engine, obs) = (self.plan.engine.clone(), self.plan.obs.clone());
        let name = self.plan.name.clone();

        // The container before the record: a kill in between still leaves it owned and reapable.
        let gone = if ended.keep {
            info(
                &*obs,
                format!(
                    "keeping container {name} (`{cli} inspect {name}` exposes the API key; `{cli} {verb} {name}` when done)",
                    cli = engine.cli(),
                    verb = engine.kind().delete_verb()
                ),
            );
            true
        } else {
            destroyed(&engine, &name, &*obs)
        };
        let priced = self.plan.sel.provider.cost_field.is_some();
        let relay = self.held.relay.take().map(|relay| {
            let stats = relay.stats();
            relay.shutdown();
            let spent = if priced {
                format!(", ${:.4} spent", stats.spent)
            } else {
                String::new()
            };
            info(
                &*obs,
                format!(
                    "proxy relayed {}, rejected {}{spent}",
                    stats.requests, stats.rejected
                ),
            );
            stats
        });
        // After the relay is gone: the check calls no provider, and holds no token for one.
        let left = self.timeout().saturating_sub(self.started.elapsed());
        let mut verify_gone = true;
        let verified = self.plan.opts.verify.clone().map(|command| {
            if !ended.ok || obs.cancelled() {
                return Verified::not_run(&command, "the agent did not finish");
            }
            let spec =
                self.plan
                    .verify_spec(&self.plan.verify_name(), self.network.clone(), &command);
            let (verified, gone) = verify(
                &engine,
                self.held.record.as_mut(),
                &spec,
                &command,
                left,
                &*obs,
            );
            verify_gone = gone;
            verified
        });
        // Before the holder goes: the return step uses the run's network.
        let mut return_gone = true;
        let returned = self.plan.cloned.as_ref().map(|c| {
            let spec =
                self.plan
                    .verify_spec(&format!("{name}-return"), self.network.clone(), &c.script());
            let (returned, gone) =
                return_clone(&engine, self.held.record.as_mut(), &spec, c, &*obs);
            return_gone = gone;
            returned
        });
        let holder_gone = self
            .held
            .holder
            .take()
            .is_none_or(|holder| destroyed(&engine, &holder, &*obs));
        // Released only once nothing it names is left, so the next run's sweep retries a failed
        // delete.
        if let Some(record) = self.held.record.take()
            && gone
            && holder_gone
            && verify_gone
            && return_gone
        {
            record.release();
        }
        drop(self.held.reaper.take());

        // Opened before the clone is removed: the handle outlives the directory.
        let report = open_unfollowed(&self.plan.work.join(REPORT_NAME));
        // Kept when the return failed: the clone is then the only copy of the agent's work.
        if let (Some(c), Some(Returned::Unchanged | Returned::Branch { .. })) =
            (&self.plan.cloned, &returned)
        {
            c.remove();
        }
        Outcome {
            exit: ended.exit,
            mode: self.plan.mode.mode,
            relay,
            priced,
            verify: verified,
            returned,
            report,
            warnings: std::mem::take(&mut self.plan.warnings),
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let (engine, obs) = (&self.plan.engine, &*self.plan.obs);
        let gone = !self.handed_out || destroyed(engine, &self.plan.name, obs);
        if let Some(relay) = self.held.relay.take() {
            relay.shutdown();
        }
        let holder_gone = self
            .held
            .holder
            .take()
            .is_none_or(|holder| destroyed(engine, &holder, obs));
        if let Some(record) = self.held.record.take()
            && gone
            && holder_gone
        {
            record.release();
        }
        drop(self.held.reaper.take());
        match &self.plan.cloned {
            Some(c) if self.handed_out => info(
                obs,
                format!(
                    "the run ended without finishing; the agent's work is kept in {}",
                    c.copy.display()
                ),
            ),
            Some(c) => c.remove(),
            None => {}
        }
    }
}

/// Deletes `name`; whether it is gone.
fn destroyed(engine: &Engine, name: &str, obs: &dyn Observer) -> bool {
    match engine.destroy(name) {
        Ok(()) => true,
        Err(e) => {
            info(obs, e.0);
            false
        }
    }
}

/// What a finished run left. Replaces the CLI's stats file for a library caller.
#[derive(Debug)]
pub struct Outcome {
    /// What the caller passed to [`Live::finish`].
    pub exit: Option<i32>,
    pub mode: Mode,
    /// What the relay did, under a keyed mode.
    pub relay: Option<Stats>,
    /// Whether the provider reports cost, so `relay.spent` means something.
    pub priced: bool,
    pub verify: Option<Verified>,
    /// The clone's return, for a run with `clone_into`.
    pub returned: Option<Returned>,
    /// The agent's report, opened without following a symlink. `Err(None)` when there is none;
    /// `Err(Some(why))` when what is there was refused. Opened before the clone is removed.
    pub report: std::result::Result<File, Option<String>>,
    /// Each warning the run gave, also sent to the observer.
    pub warnings: Vec<String>,
}

/// What became of the clone's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Returned {
    /// The agent changed nothing; the clone is deleted.
    Unchanged,
    /// The agent's commits, uncommitted edits included, as `branch` in the repository.
    Branch { branch: String, commits: u64 },
    /// The return failed; the work is in the clone at `path`, which is kept.
    Kept { path: PathBuf, error: Error },
}

/// What the check found. `exit` is `None` when the check did not end on its own.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub command: String,
    pub exit: Option<i32>,
    pub seconds: f64,
    pub timed_out: bool,
    pub error: String,
}

impl Verified {
    fn not_run(command: &str, why: &str) -> Self {
        Verified {
            command: command.into(),
            exit: None,
            seconds: 0.0,
            timed_out: false,
            error: format!("not run: {why}"),
        }
    }

    /// The exit status a check alone ends with: the check's, 124 at the deadline, else 1.
    pub fn code(&self) -> i32 {
        self.exit.unwrap_or(if self.timed_out { 124 } else { 1 })
    }

    pub fn ok(&self) -> Option<bool> {
        self.exit.map(|c| c == 0)
    }
}

/// Runs the check's container and deletes it, which no `--rm` does. Claimed in `record` first, so
/// a kill mid-check leaves it reapable. Returns the result, and whether the container is gone.
fn verify(
    engine: &Engine,
    record: Option<&mut Run>,
    spec: &ContainerSpec,
    command: &str,
    left: Duration,
    obs: &dyn Observer,
) -> (Verified, bool) {
    if left.is_zero() {
        return (Verified::not_run(command, "the timeout was spent"), true);
    }
    if let Some(record) = record
        && let Err(e) = record.add(&spec.name)
    {
        return (Verified::not_run(command, &e.0), true);
    }
    let started = Instant::now();
    let checked = check(engine.command(&engine.run_argv(spec)), left, obs);
    let seconds = started.elapsed().as_secs_f64();
    let gone = destroyed(engine, &spec.name, obs);
    let verified = match checked {
        Ok(c) => Verified {
            command: command.into(),
            exit: c.code,
            seconds,
            timed_out: c.timed_out,
            error: if c.timed_out {
                format!("exceeded the timeout after {seconds:.1}s")
            } else {
                String::new()
            },
        },
        Err(e) => Verified {
            error: e.message,
            ..Verified::not_run(command, "")
        },
    };
    info(
        obs,
        format!(
            "verify `{command}`: {} in {seconds:.1}s",
            match (verified.exit, verified.timed_out) {
                (Some(0), _) => "passed".to_string(),
                (Some(c), _) => format!("failed (exit {c})"),
                (None, true) => "timed out".into(),
                (None, false) => format!("did not finish: {}", verified.error),
            }
        ),
    );
    (verified, gone)
}

/// Bundles the clone's commits in a container, then fetches them on the host. Returns what
/// became of the work, and whether the container is gone.
fn return_clone(
    engine: &Engine,
    record: Option<&mut Run>,
    spec: &ContainerSpec,
    c: &Clone,
    obs: &dyn Observer,
) -> (Returned, bool) {
    let keep = |why: String| {
        info(
            obs,
            format!(
                "clone: {why}; the agent's work is kept in {}",
                c.copy.display()
            ),
        );
        Returned::Kept {
            path: c.copy.clone(),
            error: Error::of_kind(ErrorKind::Return, why),
        }
    };
    if obs.cancelled() {
        return (keep("interrupted before the return step".into()), true);
    }
    if let Some(record) = record
        && let Err(e) = record.add(&spec.name)
    {
        return (keep(e.0), true);
    }
    let checked = check(
        engine.command(&engine.run_argv(spec)),
        Duration::from_secs(RETURN_SECONDS),
        obs,
    );
    let gone = destroyed(engine, &spec.name, obs);
    let returned = match checked.map(|c| c.code) {
        Ok(Some(0)) => match c.fetch() {
            Ok(count) => {
                info(
                    obs,
                    format!(
                        "clone: {count} commit(s) -> branch {} in {}",
                        c.branch,
                        c.origin.display()
                    ),
                );
                Returned::Branch {
                    branch: c.branch.clone(),
                    commits: count.parse().unwrap_or(0),
                }
            }
            Err(e) => keep(e.message),
        },
        Ok(Some(clone::UNCHANGED)) => {
            info(obs, "clone: the agent changed nothing; no branch");
            Returned::Unchanged
        }
        Ok(Some(code)) => keep(format!(
            "the return step failed (exit {code}); is git in the image?"
        )),
        Ok(None) => keep("the return step did not finish".into()),
        Err(e) => keep(e.message),
    };
    (returned, gone)
}

/// How a check ended. `code` is `None` when it was killed: at the deadline, or on cancel.
pub struct Checked {
    pub code: Option<i32>,
    pub timed_out: bool,
}

/// Runs a check, such as the verify step, with each line of its output, stdout and stderr, sent
/// to `obs`. Its own process group, killed whole at `timeout` or when `obs` cancels.
pub fn check(mut command: Command, timeout: Duration, obs: &dyn Observer) -> Result<Checked> {
    use std::os::unix::process::CommandExt;

    let program = command.get_program().to_string_lossy().into_owned();
    let (reader, writer) = std::io::pipe()?;
    let mut child = {
        // Dropped once spawned, so the pipe ends when the child's copies do.
        command
            .stdin(Stdio::null())
            .stdout(writer.try_clone()?)
            .stderr(writer)
            .process_group(0);
        command
            .spawn()
            .map_err(|e| Error::new(format!("{program}: {e}")))?
    };
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).split(b'\n').map_while(|l| l.ok()) {
            let line = String::from_utf8_lossy(&line);
            if send
                .send(line.strip_suffix('\r').unwrap_or(&line).to_string())
                .is_err()
            {
                return;
            }
        }
    });
    let pass_on = |wait: Duration| {
        while let Ok(line) = lines.recv_timeout(wait) {
            obs.note(Note::Output(line));
        }
    };
    let deadline = Instant::now() + timeout;
    loop {
        pass_on(Duration::ZERO);
        if let Some(status) = child.try_wait()? {
            // What is still in the pipe. A process the check left behind holding it is not waited
            // for past a moment.
            pass_on(Duration::from_millis(200));
            return Ok(Checked {
                code: status.code(),
                timed_out: false,
            });
        }
        let late = Instant::now() >= deadline;
        if late || obs.cancelled() {
            // SAFETY: kill takes no pointers, and the child is not yet reaped, so its pid, which
            // is also its group's id, is still ours.
            unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
            child.wait()?;
            pass_on(Duration::from_millis(200));
            return Ok(Checked {
                code: None,
                timed_out: late,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Refuses a run whose container could not reach the relay: its first call would hang until the
/// timeout. Through the holder where there is one; otherwise a short container, recorded first
/// so a killed run's sweep deletes it. See docs/dev/firewall-considerations.md.
fn probe_relay(
    engine: &Engine,
    held: &mut Held,
    network: &str,
    image: &str,
    gateway: &str,
    port: u16,
) -> Result<()> {
    let url = format!("http://{gateway}:{port}{PING}");
    let reached = match &held.holder {
        Some(holder) => engine.probe(&Via::Holder(holder), &url, PROBE_SECONDS)?,
        None => {
            let name = format!("{CONTAINER_PREFIX}probe-{}", random_hex(6)?);
            if let Some(record) = held.record.as_mut() {
                record.add(&name)?;
            }
            engine.probe(
                &Via::Container {
                    name: &name,
                    network,
                    image,
                },
                &url,
                PROBE_SECONDS,
            )?
        }
    };
    if reached {
        return Ok(());
    }
    let likely = match std::env::consts::OS {
        "macos" => {
            let exe = std::env::current_exe()
                .map_or_else(|_| "sanduk".into(), |p| p.display().to_string());
            format!(
                "The macOS firewall is the likely cause. Answer its prompt, or allow this binary once: \
                 sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add {exe}"
            )
        }
        _ => format!(
            "A host firewall is the likely cause: allow incoming connections from the container network's \
             bridge interface to port {port}, e.g. `sudo ufw allow in on <bridge>`"
        ),
    };
    Err(Error::of_kind(
        ErrorKind::Setup,
        format!(
            "the relay at {gateway}:{port} is not reachable from {network}, so the agent's first call would \
             hang until the timeout. {likely}"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_url_is_split_for_the_key_check() {
        assert_eq!(
            split_url("http://127.0.0.1:8080"),
            (Scheme::Http, "127.0.0.1:8080".into(), String::new())
        );
        assert_eq!(
            split_url("https://example.com/api/v1/"),
            (Scheme::Https, "example.com".into(), "/api/v1".into())
        );
    }

    #[test]
    fn every_network_a_mode_creates_is_destroyed() {
        assert_eq!(mode_networks(None), ["sanduk-open", "sanduk-net"]);
        assert_eq!(
            mode_networks(Some("mine")),
            ["mine", "sanduk-open", "sanduk-net"]
        );
        assert_eq!(
            mode_networks(Some("sanduk-net")),
            ["sanduk-net", "sanduk-open"]
        );
    }

    /// A check's output goes to the observer, stdout and stderr both, not to this process's own.
    #[test]
    fn a_checks_output_reaches_the_observer() {
        struct Lines(std::sync::Mutex<Vec<String>>);
        impl Observer for Lines {
            fn note(&self, note: Note) {
                if let Note::Output(line) = note {
                    self.0.lock().unwrap().push(line);
                }
            }
        }
        let lines = Lines(Default::default());
        let mut sh = Command::new("sh");
        sh.args(["-c", "echo out; echo err >&2; exit 3"]);
        let checked = check(sh, Duration::from_secs(10), &lines).unwrap();
        assert_eq!((checked.code, checked.timed_out), (Some(3), false));
        let mut seen = lines.0.into_inner().unwrap();
        seen.sort();
        assert_eq!(seen, ["err", "out"]);
    }

    #[test]
    fn a_check_past_its_deadline_is_killed() {
        let mut sh = Command::new("sh");
        sh.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let checked = check(sh, Duration::from_millis(300), &()).unwrap();
        assert_eq!((checked.code, checked.timed_out), (None, true));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
