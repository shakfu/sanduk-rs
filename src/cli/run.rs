//! `sanduk run`: validate the key, build the image if absent, run one container, read the agent's
//! report off the bind mount, delete the container.
//!
//! The API key is read from the provider's variable and passed with the bare-name `-e` form, so
//! the engine inherits the value from this process and it never appears in an argv. Behind the
//! relay the container gets a per-run token instead, and the key stays in this process.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use clap::Args;
use regex::Regex;

use sanduk_container::{CONTAINER_PREFIX, ContainerSpec, Engine, Mount, Via, wait_for_gateway};

use super::{EngineArgs, ImageArgs};
use crate::agent::launch::{Signals, launch};
use crate::agent::{self, Agent, DEFAULT_AGENT, Endpoint, Options, Outcome, REPORT_NAME, Wiring};
use crate::catalog::Origin;
use crate::error::{Error, Result};
use crate::preflight::{firewall_warning, validate_key};
use crate::providers::{DEFAULT_PROVIDER, Provider, Scheme, get_provider, parse_upstream};
use crate::recipes::{self, Recipe, Rendered};
use crate::relay::{Config, PING, Relay};
use crate::runs::{self, Run, claim};
use crate::util::{copy_unfollowed, note, open_unfollowed, random_hex, seconds, shell_join, token};

/// Where -w lands inside the container.
const WORKDIR_DEST: &str = "/work";
/// Where --socket lands, and the variable naming it. Outside the work mount: the agent writes the
/// work mount, and a socket it can replace is a socket it can put a listener of its own at.
const SOCKET_DEST: &str = "/run/sanduk/agent.sock";
const SOCKET_ENV: &str = "SANDUK_SOCKET";
/// The network holder outlives the run by this much, so teardown happens while the bridge is up.
const HOLDER_MARGIN: u64 = 300;
/// A week. The guard against `--timeout 9000000` being a typo that parks a holder for months.
const MAX_TIMEOUT: u64 = 7 * 86400;

/// A mode is two properties: whether the host keeps the key, and whether the container has a
/// route off the host. The fourth combination, the key inside and no route to any provider, is a
/// run that cannot call anything.
struct Mode {
    name: &'static str,
    relayed: bool,
    egress: bool,
    network: Option<&'static str>,
}

const MODES: [Mode; 3] = [
    Mode {
        name: "open",
        relayed: false,
        egress: true,
        network: None,
    },
    Mode {
        name: "key-safe",
        relayed: true,
        egress: true,
        network: Some("sanduk-open"),
    },
    Mode {
        name: "sealed",
        relayed: true,
        egress: false,
        network: Some("sanduk-net"),
    },
];

/// Every network a mode creates, plus one the caller named.
pub fn mode_networks(named: Option<&str>) -> Vec<String> {
    let mut found: Vec<String> = named.into_iter().map(String::from).collect();
    for mode in &MODES {
        if let Some(n) = mode.network
            && !found.iter().any(|f| f == n)
        {
            found.push(n.into());
        }
    }
    found
}

#[derive(Args, Debug)]
#[command(
    after_help = "The API key comes from the provider's environment variable only.

  export OPENAI_API_KEY=sk-...
  sanduk run 'Summarise every .py file here.' -w ./work
  sanduk run --task-file brief.md -w ./repo --keep
  sanduk run 'Review this.' --agent hax --provider openai-compat \\
      --upstream http://127.0.0.1:8080 --mode sealed"
)]
pub struct RunArgs {
    /// The task prompt (or use --task-file)
    pub task: Option<String>,
    /// Read the task prompt from a file
    #[arg(long)]
    pub task_file: Option<PathBuf>,
    /// Host directory bind-mounted at /work
    #[arg(short, long, default_value = "./work")]
    pub workdir: PathBuf,
    /// Mount the workdir at its own host path instead of /work, so an absolute path in a diff, a
    /// report or a stack trace resolves on the host
    #[arg(long)]
    pub work_at_host_path: bool,
    /// Copy the agent's REPORT.md here after the run
    #[arg(short = 'o', long)]
    pub report: Option<PathBuf>,
    /// Another host directory in the container, e.g. ../repo:/repo:ro. Repeatable. Read-write unless :ro
    #[arg(long, value_name = "HOST:DEST[:ro]")]
    pub mount: Vec<String>,
    /// Write the run's outcome here as JSON: exit, ok, stats, error, report
    #[arg(long)]
    pub stats_file: Option<PathBuf>,

    #[command(flatten)]
    pub engine: EngineArgs,
    #[command(flatten)]
    pub image: ImageArgs,
    /// Rebuild the image first
    #[arg(short = 'b', long, help_heading = "Image")]
    pub rebuild: bool,

    /// Model id, e.g. claude-opus-5
    #[arg(long, help_heading = "Agent")]
    pub model: Option<String>,
    #[arg(long, help_heading = "Agent", value_parser = ["low", "medium", "high", "xhigh", "max"])]
    pub effort: Option<String>,
    #[arg(long, help_heading = "Agent")]
    pub max_turns: Option<u32>,
    /// claude only; e.g. "Read Edit Bash(git *)"
    #[arg(long, help_heading = "Agent")]
    pub allowed_tools: Option<String>,
    /// claude only; default: --dangerously-skip-permissions (nobody is there to answer a prompt)
    #[arg(long, help_heading = "Agent", value_parser = ["acceptEdits", "auto", "bypassPermissions", "manual", "dontAsk", "plan"])]
    pub permission_mode: Option<String>,
    /// Drop project context: no hooks, LSP, plugins, or CLAUDE.md / AGENTS.md discovery
    #[arg(long, help_heading = "Agent")]
    pub bare: bool,
    /// Do not append the write-a-REPORT.md instruction
    #[arg(long, help_heading = "Agent")]
    pub no_report_instruction: bool,

    #[arg(long, default_value_t = 4, help_heading = "Container")]
    pub cpus: u32,
    #[arg(long, default_value = "4G", help_heading = "Container")]
    pub memory: String,
    /// How long one run may take: seconds, or a suffix of s, m, h, d
    #[arg(
        long,
        default_value = "900",
        value_name = "DURATION",
        help_heading = "Container"
    )]
    pub timeout: String,
    /// Extra environment variable (repeatable)
    #[arg(short, long, value_name = "K=V", help_heading = "Container")]
    pub env: Vec<String>,
    /// Point the agent at this endpoint directly, without a relay
    #[arg(long, help_heading = "Container")]
    pub base_url: Option<String>,
    /// Attach to this container network
    #[arg(long, help_heading = "Container")]
    pub network: Option<String>,
    /// Run the agent as this uid, or `host` for the caller's own. Docker only
    #[arg(long, value_name = "UID[:GID]", help_heading = "Container")]
    pub user: Option<String>,
    /// Bind-mount one unix socket into the container (default dest: /run/sanduk/agent.sock). Its
    /// path is exported as SANDUK_SOCKET
    #[arg(long, value_name = "HOST[:DEST]", help_heading = "Container")]
    pub socket: Option<String>,
    /// Export the socket's container path under this name too (repeatable)
    #[arg(long, value_name = "NAME", help_heading = "Container")]
    pub socket_env: Vec<String>,
    /// Give the agent this process's stdin, so something can write to a turn already under way
    #[arg(long, help_heading = "Container")]
    pub stdin: bool,
    /// Copy the agent's output stream to stdout, verbatim and unparsed. Implies --quiet
    #[arg(long, help_heading = "Container")]
    pub stream_json: bool,
    /// docker only: the OCI runtime that starts the container, e.g. runsc (gVisor)
    #[arg(long, value_name = "NAME", help_heading = "Container")]
    pub oci_runtime: Option<String>,

    /// Upstream API and its wire protocol
    #[arg(long, default_value = DEFAULT_PROVIDER, value_parser = ["anthropic", "openai", "openai-compat", "openrouter"], help_heading = "Provider")]
    pub provider: String,
    /// Where the relay forwards, as scheme://host:port with no path. Defaults to the provider's own
    #[arg(long, help_heading = "Provider")]
    pub upstream: Option<String>,
    /// Permit a plaintext http upstream that is not loopback. The API key is then sent in clear
    #[arg(long, help_heading = "Provider")]
    pub insecure_upstream: bool,
    /// Variable the agent reads its credential from inside the container (default: the agent's)
    #[arg(long, help_heading = "Provider")]
    pub agent_key_env: Option<String>,
    /// Variable the agent reads its base URL from inside the container (default: the agent's)
    #[arg(long, help_heading = "Provider")]
    pub agent_base_url_env: Option<String>,

    /// open: the container holds the key and reaches anything. key-safe: the key stays on the
    /// host, the container still reaches anything. sealed: the key stays on the host and the
    /// container has no route off it (default: open)
    #[arg(long, value_parser = ["open", "key-safe", "sealed"], help_heading = "Containment")]
    pub mode: Option<String>,
    /// The old spelling of --mode sealed
    #[arg(long, hide = true)]
    pub proxy: bool,
    /// Network to create or use (default: sanduk-net sealed, sanduk-open key-safe)
    #[arg(long, help_heading = "Containment")]
    pub proxy_network: Option<String>,
    /// Stop the run once its calls have cost more than this. Only openrouter reports cost
    #[arg(long, value_name = "USD", help_heading = "Containment")]
    pub budget: Option<f64>,
    /// Host port for the relay (default: an ephemeral one)
    #[arg(long, default_value_t = 0, help_heading = "Containment")]
    pub proxy_port: u16,
    /// Allowed upstream path, matched exactly (repeatable)
    #[arg(long, help_heading = "Containment")]
    pub proxy_allow_path: Vec<String>,
    /// Restrict the agent to these model ids (repeatable); enforced on the host
    #[arg(long, help_heading = "Containment")]
    pub allow_model: Vec<String>,
    /// Clamp max_tokens on every request the agent sends
    #[arg(long, help_heading = "Containment")]
    pub max_tokens_cap: Option<u64>,
    /// Record every request body: a digest line per call, full JSON under --log-dir
    #[arg(long, help_heading = "Containment")]
    pub log_bodies: bool,
    /// Where --log-bodies writes full request JSON. A path inside -w or a --mount is refused
    #[arg(long, default_value = "./sanduk-logs", help_heading = "Containment")]
    pub log_dir: PathBuf,

    /// Do not delete the container when the run ends
    #[arg(long, help_heading = "Lifecycle")]
    pub keep: bool,
    /// Print the container command and exit
    #[arg(long, help_heading = "Lifecycle")]
    pub dry_run: bool,
    #[arg(long, help_heading = "Lifecycle")]
    pub skip_key_check: bool,
}

/// The mode, resolved into what the run path reads.
struct Resolved {
    relayed: bool,
    egress: bool,
    network: String,
    timeout: u64,
}

fn resolve_mode(args: &RunArgs) -> Result<Resolved> {
    let name = match (&args.mode, args.proxy) {
        (None, proxy) => if proxy { "sealed" } else { "open" }.to_string(),
        (Some(mode), true) if mode != "sealed" => {
            return Err(Error::new(format!(
                "--proxy is the old spelling of --mode sealed; it cannot be combined with --mode {mode}"
            )));
        }
        (Some(mode), _) => mode.clone(),
    };
    let timeout = seconds(&args.timeout)?;
    if timeout > MAX_TIMEOUT {
        return Err(Error::new(format!(
            "--timeout {timeout}s is longer than a week. The network holder is started for the run's \
             length, so a typo here parks a container for that long. Pass at most {MAX_TIMEOUT}s"
        )));
    }
    if args.budget.is_some_and(|b| b <= 0.0 || b.is_nan()) {
        return Err(Error::new("--budget must be a positive number of dollars"));
    }
    let mode = MODES
        .iter()
        .find(|m| m.name == name)
        .expect("clap checked the name");
    let network = match &args.proxy_network {
        None => mode.network.unwrap_or("sanduk-net").to_string(),
        Some(named) => {
            // An existing network is reused as it was created, and only Docker reports whether
            // that was with a route off the host. Naming one mode's default under the other is how
            // a sealed run silently kept its egress.
            if let Some(crossed) = MODES
                .iter()
                .find(|m| m.network == Some(named.as_str()) && m.egress != mode.egress)
            {
                return Err(Error::new(format!(
                    "--proxy-network {named} is the default network for --mode {}, which {} route off \
                     the host. --mode {name} needs the opposite; name another network",
                    crossed.name,
                    if crossed.egress { "has a" } else { "has no" }
                )));
            }
            named.clone()
        }
    };
    Ok(Resolved {
        relayed: mode.relayed,
        egress: mode.egress,
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
pub fn resolve_image(args: &ImageArgs, engine: &Engine) -> Result<(Agent, Image)> {
    if (args.recipe.is_some() || !args.kit.is_empty())
        && (args.image.is_some() || args.containerfile.is_some())
    {
        return Err(Error::new(
            "--recipe and --kit build an image from a recipe; they cannot be combined with --image or --containerfile",
        ));
    }
    let mut recipe = args
        .recipe
        .as_deref()
        .map(|spec| recipes::resolve(spec, &args.kit))
        .transpose()?;
    if let (Some(recipe), Some(agent)) = (&recipe, &args.agent)
        && *agent != recipe.agent
    {
        return Err(Error::new(format!(
            "recipe {} builds {}, not {agent}; drop --agent or choose another recipe",
            recipe.name, recipe.agent
        )));
    }
    let spec = match &recipe {
        Some(recipe) => recipe.agent.clone(),
        None => args.agent.clone().unwrap_or_else(|| DEFAULT_AGENT.into()),
    };
    let agent = agent::get(&spec)?;

    if let Some(cf) = &args.containerfile {
        let tag = args
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
        recipe = Some(recipes::resolve(own, &args.kit)?);
    }
    let Some(recipe) = recipe else {
        if !args.kit.is_empty() {
            return Err(Error::new(format!(
                "agent {:?} has no recipe, so it takes no kits",
                agent.name
            )));
        }
        let (Some(image), Some(cf)) = (&agent.image, &agent.containerfile) else {
            return Err(Error::new(format!(
                "agent {:?} names no recipe, image or Containerfile; pass --recipe, or --image and --containerfile",
                agent.name
            )));
        };
        let tag = args.image.clone().unwrap_or_else(|| image.clone());
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
    let tag = args
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

pub fn build_image(engine: &Engine, image: &Image) -> Result<()> {
    match (&image.rendered, &image.containerfile) {
        (Some(rendered), _) => recipes::build(engine, &image.tag, rendered),
        (None, Some(cf)) => Ok(engine.build_image(&image.tag, cf)?),
        _ => Err(Error::new("nothing to build the image from")),
    }
}

/// The flags that chose this image, for a command the user is told to run.
pub fn selectors(args: &ImageArgs) -> String {
    let mut parts: Vec<String> = args
        .recipe
        .iter()
        .map(|r| format!("--recipe {r}"))
        .collect();
    if parts.is_empty() || args.image.is_some() {
        parts.push(format!(
            "--agent {}",
            args.agent.as_deref().unwrap_or(DEFAULT_AGENT)
        ));
    }
    parts.extend(args.kit.iter().map(|k| format!("--kit {k}")));
    if let Some(image) = &args.image {
        parts.push(format!("--image {image}"));
    }
    parts.join(" ")
}

/// What the flags select. None of it depends on the relay being up yet.
struct Selection {
    agent: Agent,
    provider: &'static Provider,
    scheme: Scheme,
    host: String,
    image: Image,
    opts: Options,
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
fn select(args: &RunArgs, mode: &Resolved, engine: &Engine) -> Result<Selection> {
    let (agent, image) = resolve_image(&args.image, engine)?;
    let provider = get_provider(&args.provider)?;
    let (scheme, host) = match &args.upstream {
        Some(url) => parse_upstream(url, args.insecure_upstream)?,
        None => (provider.scheme, provider.host.to_string()),
    };
    if args.budget.is_some() {
        if provider.cost_field.is_none() {
            return Err(Error::new(format!(
                "--budget cannot be enforced against {}: it reports tokens, not cost. Only openrouter reports cost",
                provider.name
            )));
        }
        if !mode.relayed {
            return Err(Error::new(
                "--budget is counted by the relay, which --mode open does not start. Use --mode key-safe or sealed",
            ));
        }
    }
    let opts = Options {
        model: args
            .model
            .clone()
            .or(provider.default_model.map(String::from)),
        effort: args.effort.clone(),
        max_turns: args.max_turns,
        permission_mode: args.permission_mode.clone(),
        allowed_tools: args.allowed_tools.clone(),
        bare: args.bare,
        relayed: mode.relayed,
        key_env: args.agent_key_env.clone(),
        base_url_env: args.agent_base_url_env.clone(),
    };
    agent.check(&opts, provider)?;
    if let Some(recipe) = &image.recipe {
        check_kits(args, mode, recipe)?;
    }
    Ok(Selection {
        agent,
        provider,
        scheme,
        host,
        image,
        opts,
    })
}

/// Refuses a kit, or instructions, the run's own flags would defeat or break.
fn check_kits(args: &RunArgs, mode: &Resolved, recipe: &Recipe) -> Result<()> {
    if !recipe.instructions.is_empty() && args.bare {
        return Err(Error::new(format!(
            "recipe {} has instructions, and --bare stops the agent reading them",
            recipe.name
        )));
    }
    for used in &recipe.kits {
        let kit = &used.kit;
        if kit.egress && !mode.egress {
            return Err(Error::new(format!(
                "kit {} needs the network at run time, which --mode sealed has no route for. Use --mode key-safe, or drop the kit",
                kit.name
            )));
        }
        if kit.hook && args.allowed_tools.is_some() {
            return Err(Error::new(format!(
                "kit {} installs a hook that rewrites commands, which may pass one --allowed-tools would refuse. Untested, so refused",
                kit.name
            )));
        }
        if kit.hook && args.bare {
            return Err(Error::new(format!(
                "kit {} works through a hook, and --bare drops hooks",
                kit.name
            )));
        }
    }
    Ok(())
}

/// A path made absolute, with the symlinks of the part that exists resolved.
fn resolve_path(path: &Path) -> Result<PathBuf> {
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

fn work_dest(args: &RunArgs, workdir: &Path) -> String {
    if args.work_at_host_path {
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

/// `HOST:DEST[:ro]` per `--mount`. Refused: a host directory that is not there, a destination that
/// is not absolute, and anything at or under the working directory, which would shadow part of
/// what the agent was pointed at.
pub fn parse_mounts(values: &[String], dest_of_work: &str) -> Result<Vec<Mount>> {
    let mut mounts: Vec<Mount> = Vec::new();
    for value in values {
        let parts: Vec<&str> = value.split(':').collect();
        if !(2..=3).contains(&parts.len()) || parts[0].is_empty() || parts[1].is_empty() {
            return Err(Error::new(format!(
                "--mount {value:?}: expected HOST:DEST[:ro]"
            )));
        }
        let mode = parts.get(2).copied().unwrap_or("rw");
        if mode != "ro" && mode != "rw" {
            return Err(Error::new(format!(
                "--mount {value:?}: mode is ro or rw, not {mode:?}"
            )));
        }
        let host = resolve_path(&expand_home(parts[0]))?;
        let dest = parts[1];
        if !host.is_dir() {
            return Err(Error::new(format!(
                "--mount {value:?}: {} is not a directory",
                host.display()
            )));
        }
        if !dest.starts_with('/') || dest == "/" {
            return Err(Error::new(format!(
                "--mount {value:?}: {dest} is not an absolute path"
            )));
        }
        if dest == dest_of_work || dest.starts_with(&format!("{dest_of_work}/")) {
            return Err(Error::new(format!(
                "--mount {value:?}: {dest_of_work} is where -w lands; a mount there would shadow the directory the agent was given"
            )));
        }
        if let Some(other) = mounts.iter().find(|m| m.dest == dest) {
            return Err(Error::new(format!(
                "--mount {value:?}: {dest} already holds {}",
                other.host.display()
            )));
        }
        mounts.push(Mount {
            host,
            dest: dest.into(),
            ro: mode == "ro",
        });
    }
    Ok(mounts)
}

/// `HOST[:DEST]` per `--socket`: the one thing in the container that reaches the host, so what it
/// is and where it lands are both checked. It may not land in a mount the agent writes.
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
        return Err(Error::new(format!(
            "--socket {value:?}: {dest} is not an absolute path"
        )));
    }
    let Ok(meta) = std::fs::metadata(&host) else {
        return Err(Error::new(format!(
            "--socket {value:?}: {} is not there",
            host.display()
        )));
    };
    if !meta.file_type().is_socket() {
        return Err(Error::new(format!(
            "--socket {value:?}: {} is not a unix socket",
            host.display()
        )));
    }
    let work = dest_of_work.trim_end_matches('/');
    if dest == dest_of_work || dest.starts_with(&format!("{work}/")) {
        return Err(Error::new(format!(
            "--socket {value:?}: {dest} is inside {dest_of_work}, which the agent writes. Mount the socket outside the work mount."
        )));
    }
    for (other, flag) in
        std::iter::once((workdir, "-w")).chain(mounts.iter().map(|m| (m.host.as_path(), "--mount")))
    {
        if host.starts_with(other) {
            return Err(Error::new(format!(
                "--socket {value:?}: {} is inside {}, which {flag} puts in the container: the agent could replace the socket with a listener of its own. Keep it outside.",
                host.display(),
                other.display()
            )));
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

/// `--user`, as the engine spells it, or `None` for the image's own user.
fn container_user(args: &RunArgs, engine: &Engine) -> Result<Option<String>> {
    let Some(value) = args.user.as_deref().filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if !engine.kind().takes_user() {
        return Err(Error::new(format!(
            "--user needs --runtime docker: {} runs the image's own user. Rebuild the image for that uid instead.",
            engine.kind().name()
        )));
    }
    if value == "host" {
        return Ok(Some(format!("{}:{}", uid(), gid())));
    }
    static UID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+(:\d+)?$").unwrap());
    if !UID.is_match(value) {
        return Err(Error::new(format!(
            "--user {value:?}: expected UID[:GID], or `host`"
        )));
    }
    if value.split(':').next() == Some("0") {
        return Err(Error::new(
            "--user 0: sanduk runs no agent as root. The socket's file mode is the gate on it, and root ignores a file mode.",
        ));
    }
    Ok(Some(value.into()))
}

fn requested_uid(args: &RunArgs) -> Option<u32> {
    match args.user.as_deref()? {
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
    Err(Error::new(format!(
        "--socket {} is owned by uid {} and its mode admits only its owner, but the agent runs as uid {uid}. Create the socket as that uid, or run as its owner.",
        socket.host.display(),
        meta.uid()
    )))
}

/// Refuses a request-body log the agent could reach: the bodies are the audit trail of what it
/// sent, and the default is relative to the caller's directory, so `-w .` put it inside /work.
fn check_log_dir(log_dir: &Path, workdir: &Path, mounts: &[Mount]) -> Result<()> {
    let resolved = resolve_path(log_dir)?;
    for (host, flag) in
        std::iter::once((workdir, "-w")).chain(mounts.iter().map(|m| (m.host.as_path(), "--mount")))
    {
        if resolved.starts_with(host) {
            return Err(Error::new(format!(
                "--log-dir {} is inside {}, which {flag} puts in the container: the agent could read and edit the bodies it sent. Point --log-dir outside it.",
                resolved.display(),
                host.display()
            )));
        }
    }
    Ok(())
}

fn read_task(args: &RunArgs) -> Result<String> {
    let mut task = match (&args.task, &args.task_file) {
        (Some(task), None) => task.clone(),
        (None, Some(file)) => std::fs::read_to_string(file)?,
        _ => {
            return Err(Error::new(
                "give exactly one of: a task argument, or --task-file",
            ));
        }
    };
    if task.trim().is_empty() {
        return Err(Error::new("task is empty"));
    }
    if !args.no_report_instruction {
        task += &agent::report_instruction();
    }
    Ok(task)
}

/// Builds the image if it is missing, then refuses one whose agent cannot write `workdir` or open
/// `socket`. Before any container starts: an agent that cannot write its report still spends the
/// whole run's tokens.
fn ensure_image(
    engine: &Engine,
    args: &RunArgs,
    sel: &Selection,
    workdir: &Path,
    socket: Option<&Mount>,
) -> Result<()> {
    if args.rebuild || !engine.image_exists(&sel.image.tag) {
        build_image(engine, &sel.image)?;
    }
    if !engine.kind().keeps_mount_owner() {
        return Ok(());
    }
    let mut uid = requested_uid(args);
    if let Some(asked) = uid
        && let Some(built_as) = engine.image_uid(&sel.image.tag)
        && built_as != asked
    {
        // Not a refusal: this is the only way to run an image somebody else built. It is the
        // failure a mid-run "permission denied" would not explain.
        note(&format!(
            "--user {asked} is not the uid {} was built for ({built_as}); anything the agent writes under its home may be refused",
            sel.image.tag
        ));
    }
    if uid.is_none() {
        uid = engine.image_uid(&sel.image.tag);
    }
    let Some(uid) = uid.or_else(|| {
        // A shipped image built before the label ran as 1000. Anything else may run as anyone.
        let shipped = sel.agent.origin == Origin::Shipped
            && args.image.image.is_none()
            && args.image.containerfile.is_none();
        shipped.then_some(1000)
    }) else {
        return check_socket_reachable(socket, None);
    };
    let owner = match std::fs::metadata(workdir) {
        Err(_) => crate::cli::run::uid(),
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
        format!(
            "rebuild it as uid {owner}: sanduk build --force {} --runtime {}",
            selectors(&args.image),
            engine.kind().name()
        )
    };
    Err(Error::new(format!(
        "{} runs its agent as uid {uid}, which cannot write {} (owned by uid {owner}): {fix}",
        sel.image.tag,
        workdir.display()
    )))
}

/// The container: the agent's argv, the workdir, mounts, and variables named but not valued.
#[allow(clippy::too_many_arguments)]
fn build_spec(
    args: &RunArgs,
    sel: &Selection,
    wiring: &Wiring,
    command: Vec<String>,
    name: &str,
    workdir: &Path,
    dest: &str,
    network: Option<String>,
    mounts: &[Mount],
    socket: Option<&Mount>,
    user: Option<String>,
) -> ContainerSpec {
    // The key's value, and the base URL's, come from this process's environment: neither appears
    // in the argv or in `ps`.
    let mut inherit = vec![wiring.key_env.clone()];
    if wiring.base_url.is_some() {
        inherit.push(wiring.base_url_env.clone());
    }
    let mut all_mounts = mounts.to_vec();
    let mut env: Vec<String> = wiring.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    if let Some(socket) = socket {
        all_mounts.push(socket.clone());
        env.extend(
            std::iter::once(SOCKET_ENV)
                .chain(args.socket_env.iter().map(String::as_str))
                .map(|n| format!("{n}={}", socket.dest)),
        );
    }
    // The agent's own settings first, so an explicit -e can override one.
    env.extend(args.env.iter().cloned());
    ContainerSpec {
        command,
        cpus: args.cpus,
        memory: args.memory.clone(),
        mount: Some((workdir.to_path_buf(), dest.into())),
        mounts: all_mounts,
        inherit_env: inherit,
        env,
        network,
        oci_runtime: args.oci_runtime.clone(),
        user,
        stdin: args.stdin,
        ..ContainerSpec::new(name, sel.image.tag.clone())
    }
}

/// The agent's own tally, without a zero cost it had no way to know. Through the relay the model is
/// named by sanduk's provider id, which no agent's price table has, so an agent reports $0.0000.
/// Beside the relay's real figure the zero is dropped; with no figure it reads as unknown.
pub fn agent_stats(stats: &str, relay_prices: bool, spent: f64) -> String {
    static ZERO_AFTER_COMMA: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r",?\s*\$0\.0+\b").unwrap());
    static ZERO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$0\.0+\b").unwrap());
    if relay_prices {
        if spent <= 0.0 {
            return stats.to_string();
        }
        return ZERO_AFTER_COMMA.replace_all(stats, "").trim().to_string();
    }
    ZERO.replace_all(stats, "cost unknown").into_owned()
}

/// `scheme://host` and the path prefix of a `--base-url`, for the key check.
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

/// The engine command, and the variables it inherits from this process.
type Prepared = (Vec<String>, Vec<(String, String)>);

/// What a run holds before its container starts, given back if the start fails.
#[derive(Default)]
struct Held {
    relay: Option<Relay>,
    holder: Option<String>,
    record: Option<Run>,
}

impl Held {
    fn give_back(&mut self, engine: &Engine) {
        if let Some(relay) = self.relay.take() {
            relay.shutdown();
        }
        if let Some(holder) = self.holder.take()
            && let Err(e) = engine.destroy(&holder)
        {
            note(&e.0);
        }
        if let Some(record) = self.record.take() {
            record.release();
        }
    }
}

pub fn run(args: RunArgs) -> Result<i32> {
    let mode = resolve_mode(&args)?;
    let engine = args.engine.engine()?;
    if args.oci_runtime.is_some() && !engine.kind().takes_oci_runtime() {
        return Err(Error::new(format!(
            "--oci-runtime needs --runtime docker: {} has no OCI runtime to swap",
            engine.kind().name()
        )));
    }
    if let Some(oci) = &args.oci_runtime {
        // Hardening is rendered whatever starts the container, and a runtime that ignores a flag
        // says nothing. Kata drops the pid ceiling (kata-containers#13824).
        note(&format!(
            "--pids-limit is enforced by runc, not by every OCI runtime: {oci} may ignore it, and Kata's runtimes do"
        ));
    }
    let sel = select(&args, &mode, &engine)?;
    let provider = sel.provider;
    let api_url = format!("{}://{}", sel.scheme.as_str(), sel.host);
    let task = read_task(&args)?;

    let key = std::env::var(provider.key_env)
        .unwrap_or_default()
        .trim()
        .to_string();
    if key.is_empty() && provider.has_auth {
        return Err(Error::new(format!(
            "{} is not set. export it, then re-run.",
            provider.key_env
        )));
    }

    let workdir = resolve_path(&args.workdir)?;
    // Parsed before anything is acquired: a refused --mount after the holder was up left it behind.
    let dest = work_dest(&args, &workdir);
    let mounts = parse_mounts(&args.mount, &dest)?;
    let socket = parse_socket(args.socket.as_deref(), &dest, &workdir, &mounts)?;
    let user = container_user(&args, &engine)?;
    if args.log_bodies {
        check_log_dir(&args.log_dir, &workdir, &mounts)?;
    }

    if !args.dry_run && !args.skip_key_check {
        // Before anything is started, so a bad key cannot leak a container.
        match (&args.base_url, mode.relayed) {
            (Some(base), false) => {
                let (scheme, host, prefix) = split_url(base);
                validate_key(&key, scheme, &host, &prefix, provider)?;
            }
            _ => validate_key(&key, sel.scheme, &sel.host, "", provider)?,
        }
    }

    let name = format!("{CONTAINER_PREFIX}{}", random_hex(8)?);
    let signals = Signals::catch();
    let mut held = Held::default();
    let mut network = args.network.clone();
    let (mut gateway, mut port) = (String::new(), 0u16);
    let mut run_token = String::new();

    let prepared = (|| -> Result<Prepared> {
        if mode.relayed {
            engine.require_run()?;
            firewall_warning();
            network = Some(mode.network.clone());
            let (net, created) = engine.ensure_network(&mode.network, !mode.egress)?;
            if created {
                note(&format!(
                    "created {} network {}",
                    if mode.egress { "routable" } else { "internal" },
                    mode.network
                ));
            }
            gateway = net.gateway;
            run_token = token(24)?;
            if !args.dry_run {
                ensure_image(&engine, &args, &sel, &workdir, socket.as_ref())?;
                held.record = Some(claim(engine.kind().name(), &name)?);
                // Outlive the run: the holder going first takes the bridge, and the relay's
                // address, with it.
                held.holder = engine.hold_network_up(
                    &mode.network,
                    &sel.image.tag,
                    mode.timeout + HOLDER_MARGIN,
                )?;
                if let (Some(holder), Some(record)) = (&held.holder, held.record.as_mut()) {
                    record.add(holder)?;
                }
                if !wait_for_gateway(&gateway, Duration::from_secs(30)) {
                    return Err(Error::new(format!(
                        "{gateway} never became bindable on this host.{}",
                        engine.kind().gateway_hint()
                    )));
                }
                let relay = start_relay(&args, &sel, &key, &run_token, &gateway, &name)?;
                port = relay.port();
                held.relay = Some(relay);
                probe_relay(
                    &engine,
                    &mut held,
                    &mode.network,
                    &sel.image.tag,
                    &gateway,
                    port,
                )?;
            }
        }
        let root = if mode.relayed {
            Some(format!("http://{gateway}:{port}"))
        } else {
            args.base_url.clone()
        };
        let at = sel.endpoint(root);
        let wiring = sel.agent.wire(&sel.opts, &at);
        // Behind the relay the container gets the run token; the key stays in this process.
        let secret = if mode.relayed {
            run_token.clone()
        } else {
            key.clone()
        };
        let mut child_env = Vec::new();
        if !secret.is_empty() {
            child_env.push((wiring.key_env.clone(), secret));
        }
        if let Some(url) = &wiring.base_url {
            child_env.push((wiring.base_url_env.clone(), url.clone()));
        }
        let command = sel.agent.argv(&sel.opts, &at, &task, &wiring)?;
        let spec = build_spec(
            &args,
            &sel,
            &wiring,
            command,
            &name,
            &workdir,
            &dest,
            network.clone(),
            &mounts,
            socket.as_ref(),
            user.clone(),
        );
        let cmd = engine.run_argv(&spec);

        if args.dry_run {
            println!("{}", shell_join(&cmd));
            if mode.relayed {
                println!("# proxy: {gateway} -> {api_url} ({})", provider.name);
            }
            println!(
                "# container env: {}=<credential> {}={}",
                wiring.key_env,
                wiring.base_url_env,
                wiring.base_url.as_deref().unwrap_or("<agent default>")
            );
            return Ok((Vec::new(), Vec::new()));
        }
        engine.require_run()?;
        if !mode.relayed {
            ensure_image(&engine, &args, &sel, &workdir, socket.as_ref())?;
        }
        // Not before the dry-run return, and not before the key check: until a run is about to
        // start, the previous report is still the only result there is.
        std::fs::create_dir_all(&workdir)?;
        // Removed, not tested and removed: a test follows a symlink the last agent left.
        match std::fs::remove_file(workdir.join(REPORT_NAME)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        runs::sweep();
        if held.record.is_none() {
            held.record = Some(claim(engine.kind().name(), &name)?);
        }
        if let Some(signum) = Signals::caught() {
            return Err(Error::with_code("interrupted", 128 + signum));
        }
        Ok((cmd, child_env))
    })();
    let (cmd, child_env) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            held.give_back(&engine);
            return Err(e);
        }
    };
    if args.dry_run {
        return Ok(0);
    }

    if mode.relayed && mode.egress {
        note(&format!(
            "relay bound to {gateway}:{port} (bridge only); the key stays here, and {} reaches the internet: what the agent sends anywhere else is neither relayed nor recorded",
            mode.network
        ));
    } else if mode.relayed {
        note(&format!(
            "relay bound to {gateway}:{port} (bridge only); {} has no route off the host",
            mode.network
        ));
    }
    note(&format!("{name} -> {}", workdir.display()));
    let started = Instant::now();
    let launched = launch(
        &sel.agent,
        &cmd,
        Duration::from_secs(mode.timeout),
        // The trace and the stream both go to stdout; a reader parsing one out of the other is
        // what --stream-json exists to avoid.
        args.engine.quiet || args.stream_json,
        &child_env,
        args.stdin,
        args.stream_json,
    );

    // The container before the record: a kill in between still leaves it owned and reapable.
    let keep = args.keep && launched.is_ok();
    let gone = if keep {
        note(&format!(
            "keeping container {name} (`{cli} inspect {name}` exposes the API key; `{cli} {verb} {name}` when done)",
            cli = engine.cli(),
            verb = engine.kind().delete_verb()
        ));
        true
    } else {
        match engine.destroy(&name) {
            Ok(()) => true,
            Err(e) => {
                note(&e.0);
                false
            }
        }
    };
    let mut relay_prices = false;
    let mut spent = 0.0;
    if let Some(relay) = held.relay.take() {
        let stats = relay.stats();
        relay_prices = provider.cost_field.is_some();
        spent = stats.spent;
        let spent_note = if relay_prices {
            format!(", ${:.4} spent", stats.spent)
        } else {
            String::new()
        };
        relay.shutdown();
        note(&format!(
            "proxy relayed {}, rejected {}{spent_note}",
            stats.requests, stats.rejected
        ));
    }
    let mut holder_gone = true;
    if let Some(holder) = held.holder.take()
        && let Err(e) = engine.destroy(&holder)
    {
        note(&e.0);
        holder_gone = false;
    }
    // Released only once nothing it names is left, so the next run's sweep retries a failed delete.
    if let Some(record) = held.record.take()
        && gone
        && holder_gone
    {
        record.release();
    }
    drop(signals);

    let (outcome, rc, failure) = match launched {
        Ok(ran) => (ran.outcome, ran.code, String::new()),
        Err(e) if e.message == "interrupted" => return Err(e),
        Err(e) => (None, e.code, e.message),
    };
    note(&format!("{:.1}s wall", started.elapsed().as_secs_f64()));
    let outcome = outcome.map(|o| Outcome {
        stats: agent_stats(&o.stats, relay_prices, spent),
        ..o
    });
    if let Some(o) = &outcome {
        note(&o.stats);
    }
    collect_report(&args, &workdir, outcome.as_ref(), rc, &failure)
}

/// How long the probe waits for the relay. A reachable relay answers in milliseconds.
const PROBE_SECONDS: u32 = 5;

/// Refuses a run whose container could not reach the relay: its first call would hang until
/// `--timeout`. Through the holder where there is one; otherwise a short container, recorded first
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
    Err(Error::new(format!(
        "the relay at {gateway}:{port} is not reachable from {network}, so the agent's first call would \
         hang until --timeout. {likely}"
    )))
}

/// Binds the relay to the bridge address only: unreachable from Wi-Fi or the LAN.
fn start_relay(
    args: &RunArgs,
    sel: &Selection,
    key: &str,
    run_token: &str,
    gateway: &str,
    name: &str,
) -> Result<Relay> {
    let mut cfg = Config::new(sel.provider, key, run_token);
    cfg.upstream = sel.host.clone();
    cfg.scheme = sel.scheme;
    if !args.proxy_allow_path.is_empty() {
        cfg = cfg.allow_paths(args.proxy_allow_path.clone());
    }
    cfg.log_bodies = args.log_bodies;
    if args.log_bodies {
        let dir = resolve_path(&args.log_dir.join(name))?;
        std::fs::create_dir_all(&dir)?;
        note(&format!("request bodies -> {}", dir.display()));
        cfg.log_dir = Some(dir);
    }
    if !args.allow_model.is_empty() {
        cfg.allow_models = Some(args.allow_model.iter().cloned().collect());
    }
    cfg.max_tokens_cap = args.max_tokens_cap;
    cfg.budget = args.budget;
    Ok(Relay::start(cfg, gateway, args.proxy_port)?)
}

/// Copies the report out and records the run, failed or not; returns its status.
fn collect_report(
    args: &RunArgs,
    workdir: &Path,
    outcome: Option<&Outcome>,
    rc: i32,
    failure: &str,
) -> Result<i32> {
    let (error, code) = match outcome {
        // No terminal record: the agent never finished, whatever its status says.
        None => {
            let error = if failure.is_empty() {
                "the agent exited without a final result".to_string()
            } else {
                failure.to_string()
            };
            note(&error);
            (error, if rc == 0 { 1 } else { rc })
        }
        Some(o) if !o.ok => {
            note(&format!("agent reported an error: {}", o.error));
            (o.error.clone(), 1)
        }
        Some(_) => (String::new(), rc),
    };
    let report = workdir.join(REPORT_NAME);
    let file = open_unfollowed(&report);
    if let Some(stats_file) = &args.stats_file {
        // The exit code is all a caller gets, and the token line is printed rather than returned.
        let named = file.as_ref().map(|_| {
            args.report
                .as_ref()
                .unwrap_or(&report)
                .display()
                .to_string()
        });
        let record = serde_json::json!({
            "exit": code,
            "ok": outcome.is_some_and(|o| o.ok),
            "stats": outcome.map_or("", |o| o.stats.as_str()),
            "error": error,
            "report": named,
        });
        std::fs::write(stats_file, record.to_string())?;
    }
    match (&file, &args.report) {
        (None, _) => {
            note(&format!("the agent wrote no {REPORT_NAME}"));
            if let Some(o) = outcome.filter(|o| !o.text.is_empty()) {
                println!("\n{}", o.text);
            }
        }
        (Some(f), Some(dest)) => {
            copy_unfollowed(f, dest)?;
            note(&format!("report -> {}", dest.display()));
        }
        (Some(_), None) => note(&format!("report -> {}", report.display())),
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agents_own_cost_goes_only_beside_a_real_one() {
        let stats = "4 turns, 60 in (30 cached) / 5 out, $0.0000";
        assert_eq!(
            agent_stats(stats, true, 0.4),
            "4 turns, 60 in (30 cached) / 5 out"
        );
        assert_eq!(agent_stats(stats, true, 0.0), stats);
        assert_eq!(
            agent_stats(stats, false, 0.0),
            "4 turns, 60 in (30 cached) / 5 out, cost unknown"
        );
        assert_eq!(
            agent_stats("1 turns, $0.2500", false, 0.0),
            "1 turns, $0.2500"
        );
    }

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
}
