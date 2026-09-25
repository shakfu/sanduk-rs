//! The command line: parse flags, wire the pieces, tear everything down.

mod run;

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use sanduk_container::{CONTAINER_PREFIX, Container, Engine, Kind, default_kind};

use crate::agent;
use crate::assistants;
use crate::catalog;
use crate::error::{Error, Result};
use crate::kits;
use crate::providers::PROVIDERS;
use crate::recipes;
use crate::runs::live_containers;
use crate::util::note;

pub use run::{RunArgs, agent_stats};

#[derive(Parser)]
#[command(
    name = "sanduk",
    version,
    about = "Run an agent in a disposable container.",
    max_term_width = 100
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an agent in a disposable container
    Run(Box<RunArgs>),
    /// Register and inspect assistants
    Assistant(AssistantArgs),
    /// Queue a message for the next wakeup
    Tell(TellArgs),
    /// Run every assistant that is due, once
    Tick(TickArgs),
    /// Tick on a loop, in the foreground
    Serve(ServeArgs),
    /// What assistants have produced
    Outbox(OutboxArgs),
    /// Let outbox entries be delivered
    Approve(DecideArgs),
    /// Keep outbox entries from ever being delivered
    Reject(DecideArgs),
    /// Wakeup history
    Runs(RunsArgs),
    /// Build the agent's image
    Build(BuildArgs),
    /// Interactive shell in the agent's image
    Shell(ShellArgs),
    /// List sanduk-* containers
    Ps(EngineArgs),
    /// Stop running sanduk containers, leaving them on disk
    Stop(EngineArgs),
    /// Stop and delete sanduk containers
    Clean(CleanArgs),
    /// Clean, plus the image and the network
    Destroy(DestroyArgs),
    /// Show or change the engine's own service
    System(SystemArgs),
    /// Show registered agents, providers, runtimes, recipes, kits
    List(ListArgs),
}

/// Flags every command that talks to a container engine takes.
#[derive(Args, Clone, Debug)]
pub struct EngineArgs {
    /// Container engine (default: the first installed of this platform's, apple then docker on macOS)
    #[arg(long, value_parser = ["apple", "docker"])]
    pub runtime: Option<String>,
    /// Suppress the per-event trace
    #[arg(short, long)]
    pub quiet: bool,
}

impl EngineArgs {
    pub fn engine(&self) -> Result<Engine> {
        Ok(Engine::get(self.runtime.as_deref())?)
    }
}

/// What the image is built from. See docs/dev/kits.md.
#[derive(Args, Clone, Debug, Default)]
pub struct ImageArgs {
    /// Agent: a catalogue name (`sanduk list agents`) or a path to an agent file (default: codex,
    /// or the recipe's)
    #[arg(long, value_name = "NAME|PATH")]
    pub agent: Option<String>,
    /// Recipe that builds the image (default: the agent's). `sanduk list recipes` shows the catalogue
    #[arg(short, long, value_name = "NAME|PATH")]
    pub recipe: Option<String>,
    /// Add a kit of tools and skills to the recipe, unpinned (repeatable)
    #[arg(long, value_name = "NAME|PATH")]
    pub kit: Vec<String>,
    /// Image to use (default: built from the agent's recipe)
    #[arg(short, long)]
    pub image: Option<String>,
    /// Build the image from this Containerfile instead of a recipe
    #[arg(long)]
    pub containerfile: Option<PathBuf>,
}

#[derive(Args)]
struct BuildArgs {
    #[command(flatten)]
    engine: EngineArgs,
    #[command(flatten)]
    image: ImageArgs,
    /// Rebuild even if the image exists
    #[arg(long)]
    force: bool,
    /// Print the resolved recipe and the Containerfile, and build nothing
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct ShellArgs {
    #[command(flatten)]
    engine: EngineArgs,
    #[command(flatten)]
    image: ImageArgs,
}

#[derive(Args)]
struct CleanArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// Include a container a live run is using (a wedged run, say)
    #[arg(long)]
    all: bool,
}

#[derive(Args)]
struct DestroyArgs {
    #[command(flatten)]
    engine: EngineArgs,
    #[command(flatten)]
    image: ImageArgs,
    /// Also delete this network (default: every network a mode creates)
    #[arg(long)]
    proxy_network: Option<String>,
}

#[derive(Args)]
struct SystemArgs {
    #[command(flatten)]
    engine: EngineArgs,
    #[arg(value_parser = ["status", "start", "stop"])]
    action: String,
}

#[derive(Args)]
struct AssistantArgs {
    #[command(subcommand)]
    action: AssistantAction,
}

#[derive(Subcommand)]
enum AssistantAction {
    /// Register a directory holding assistant.toml
    Add { dir: PathBuf },
    /// One row per registered assistant
    List,
    /// One assistant's state, as JSON
    Show { name: String },
    /// Let it run again, and clear its failure count
    Enable { name: String },
    /// Stop scheduling it
    Disable { name: String },
}

#[derive(Args)]
struct TellArgs {
    name: String,
    /// The message text
    #[arg(required = true)]
    message: Vec<String>,
}

#[derive(Args)]
struct TickArgs {
    /// This one only, whether or not it is due
    #[arg(long)]
    name: Option<String>,
    /// Override the engine named in assistant.toml
    #[arg(long, value_parser = ["apple", "docker"])]
    runtime: Option<String>,
}

#[derive(Args)]
struct ServeArgs {
    /// Seconds between passes at most
    #[arg(long, default_value_t = 60)]
    interval: u64,
    #[arg(long, value_parser = ["apple", "docker"])]
    runtime: Option<String>,
}

#[derive(Args)]
struct OutboxArgs {
    #[arg(long)]
    name: Option<String>,
    /// Only what --deliver has not sent
    #[arg(long)]
    undelivered: bool,
    /// Only what is waiting for approval
    #[arg(long)]
    pending: bool,
    /// Pipe each undelivered entry to CMD on stdin, then mark it delivered
    #[arg(long, value_name = "CMD")]
    deliver: Option<String>,
}

#[derive(Args)]
struct DecideArgs {
    /// Outbox entry ids
    #[arg(required = true)]
    id: Vec<i64>,
}

#[derive(Args)]
struct RunsArgs {
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value_t = 20)]
    limit: i64,
}

#[derive(Args)]
struct ListArgs {
    #[arg(value_parser = ["agents", "providers", "runtimes", "recipes", "kits"])]
    axis: String,
}

/// Every command's one exit. A failure the caller can act on becomes a line and a status.
pub fn main(argv: Vec<OsString>) -> i32 {
    // The verbs clap knows, so this list cannot drift from them.
    let command = <Cli as clap::CommandFactory>::command();
    let verbs: Vec<&str> = command
        .get_subcommands()
        .map(|c| c.get_name())
        .chain(["help"])
        .collect();
    if let Some(first) = argv.get(1).and_then(|a| a.to_str())
        && !first.starts_with('-')
        && !verbs.contains(&first)
    {
        // The first argument used to be the task, so this is the mistake to expect.
        eprintln!(
            "sanduk: {first:?} is not a command. Did you mean: sanduk run {}\ncommands: {}",
            crate::util::shell_quote(first),
            verbs.join(", ")
        );
        return 2;
    }
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return e.exit_code();
        }
    };
    let result = match cli.command {
        Command::Run(args) => run::run(*args),
        Command::Assistant(args) => assistant(args),
        Command::Tell(args) => tell(args),
        Command::Tick(args) => assistants::connect().and_then(|db| {
            assistants::tick(
                &db,
                args.name.as_deref(),
                args.runtime.as_deref(),
                &assistants::run_in_process,
            )
        }),
        Command::Serve(args) => assistants::connect().and_then(|db| {
            assistants::serve(
                &db,
                args.interval,
                args.runtime.as_deref(),
                &assistants::run_in_process,
            )
        }),
        Command::Outbox(args) => outbox(args),
        Command::Approve(args) => decide(args, true),
        Command::Reject(args) => decide(args, false),
        Command::Runs(args) => runs(args),
        Command::Build(args) => build(args),
        Command::Shell(args) => shell(args),
        Command::Ps(args) => ps(args),
        Command::Stop(args) => stop(args),
        Command::Clean(args) => clean(&args.engine, args.all),
        Command::Destroy(args) => destroy(args),
        Command::System(args) => system(args),
        Command::List(args) => list(args),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sanduk: {}", e.message);
            e.code
        }
    }
}

fn build(args: BuildArgs) -> Result<i32> {
    let engine = args.engine.engine()?;
    let (_, image) = run::resolve_image(&args.image, &engine)?;
    if args.dry_run {
        print_build(&image)?;
        return Ok(0);
    }
    engine.require()?;
    if engine.image_exists(&image.tag) && !args.force {
        note(&format!(
            "{} is already built (--force to rebuild)",
            image.tag
        ));
        return Ok(0);
    }
    run::build_image(&engine, &image)?;
    Ok(0)
}

/// What `build` would do: the resolved recipe, then the Containerfile.
fn print_build(image: &run::Image) -> Result<()> {
    println!("# image: {}", image.tag);
    match (&image.recipe, &image.rendered) {
        (Some(recipe), Some(rendered)) => {
            println!("# recipe, resolved:");
            println!(
                "{}",
                serde_json::to_string_pretty(&recipe.as_json()).unwrap_or_default()
            );
            for rel in rendered.files.keys() {
                println!("# build context: {rel}");
            }
            println!("# Containerfile:");
            print!("{}", rendered.containerfile);
        }
        _ => {
            let cf = image
                .containerfile
                .as_ref()
                .ok_or_else(|| Error::new("no Containerfile"))?;
            println!("# containerfile: {}", cf.display());
            print!("{}", std::fs::read_to_string(cf)?);
        }
    }
    Ok(())
}

fn shell(args: ShellArgs) -> Result<i32> {
    let engine = args.engine.engine()?;
    let (_, image) = run::resolve_image(&args.image, &engine)?;
    engine.require()?;
    if !engine.image_exists(&image.tag) {
        return Err(Error::new(format!(
            "{} is not built. Run: sanduk build {}",
            image.tag,
            run::selectors(&args.image)
        )));
    }
    // Handed straight to the terminal: this one inherits the tty.
    let argv = engine.shell_argv(&image.tag);
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()?;
    Ok(status.code().unwrap_or(1))
}

fn ps(args: EngineArgs) -> Result<i32> {
    let engine = args.engine()?;
    engine.require()?;
    let found = engine.list_containers(CONTAINER_PREFIX)?;
    for c in &found {
        println!("{:22}  {:24}  {}", c.name, c.image, c.state);
    }
    if found.is_empty() {
        note("no sanduk containers");
    }
    Ok(0)
}

/// Those no live run claims. A wakeup on a schedule is nobody's to stop.
fn unowned(containers: Vec<Container>) -> Vec<Container> {
    let held = live_containers();
    let (keep, rest): (Vec<Container>, Vec<Container>) =
        containers.into_iter().partition(|c| held.contains(&c.name));
    if !keep.is_empty() {
        let mut names: Vec<&str> = keep.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        note(&format!(
            "leaving {} to their running owner: {}",
            keep.len(),
            names.join(", ")
        ));
    }
    rest
}

fn stop(args: EngineArgs) -> Result<i32> {
    let engine = args.engine()?;
    engine.require()?;
    let running = engine
        .list_containers(CONTAINER_PREFIX)?
        .into_iter()
        .filter(|c| c.state == "running")
        .collect();
    let stoppable = unowned(running);
    for c in &stoppable {
        engine.stop(&c.name);
        note(&format!("stopped {}", c.name));
    }
    if stoppable.is_empty() {
        note("no running sanduk containers to stop");
    }
    Ok(0)
}

/// Deletes every container sanduk named, except one a live run is using. `--all` overrides the
/// exception for a run whose process is wedged rather than working.
fn clean(args: &EngineArgs, all: bool) -> Result<i32> {
    let engine = args.engine()?;
    engine.require()?;
    let found = engine.list_containers(CONTAINER_PREFIX)?;
    let deletable = if all { found } else { unowned(found) };
    for c in &deletable {
        match engine.destroy(&c.name) {
            Ok(()) => note(&format!("deleted {}", c.name)),
            Err(e) => note(&e.0),
        }
    }
    if deletable.is_empty() {
        note("no sanduk containers to delete");
    }
    Ok(0)
}

/// Everything sanduk made on this engine, except the request-body log: that is written outside
/// the bind mount so the agent cannot edit its own audit trail, and a cleanup verb deleting it
/// would undo the point.
fn destroy(args: DestroyArgs) -> Result<i32> {
    let engine = args.engine.engine()?;
    let (_, image) = run::resolve_image(&args.image, &engine)?;
    clean(&args.engine, false)?;
    match &image.recipe {
        Some(recipe) if args.image.image.is_none() => {
            // Every build of the recipe: each edit to it, or to a kit, is a new tag.
            let tags = engine.image_tags(&recipes::repository(&recipe.name));
            for tag in &tags {
                deleted(engine.delete_image(tag), "image", tag);
            }
            if tags.is_empty() {
                note(&format!("no images of recipe {}", recipe.name));
            }
        }
        _ => deleted(engine.delete_image(&image.tag), "image", &image.tag),
    }
    for network in run::mode_networks(args.proxy_network.as_deref()) {
        deleted(engine.delete_network(&network), "network", &network);
    }
    Ok(0)
}

fn deleted(done: bool, what: &str, name: &str) {
    note(&if done {
        format!("deleted {what} {name}")
    } else {
        format!("no {what} {name}")
    });
}

fn system(args: SystemArgs) -> Result<i32> {
    // No require() first: whether the engine is usable is what status reports.
    let engine = args.engine.engine()?;
    match args.action.as_str() {
        "status" => println!("{}", engine.service_status()),
        "start" => engine.service_start()?,
        _ => {
            if engine.kind() == Kind::Apple {
                note("this stops the service for everything on the machine, not just sanduk");
            }
            engine.service_stop()?;
        }
    }
    Ok(0)
}

/// One row per registered thing, on stdout.
fn list(args: ListArgs) -> Result<i32> {
    match args.axis.as_str() {
        "agents" => {
            for name in agent::names()? {
                match agent::get(&name) {
                    Ok(a) => {
                        let built = a.recipe.clone().or(a.image.clone()).unwrap_or_default();
                        let mut protocols = a.protocols.clone();
                        protocols.sort();
                        println!("{name:9}  {built:24}  {}", protocols.join(", "));
                    }
                    Err(e) => println!("{name:9}  error: {}", e.message),
                }
            }
        }
        "recipes" => {
            for name in catalog::names(catalog::Kind::Recipes)? {
                match recipes::resolve(&name, &[]) {
                    Ok(recipe) => {
                        let used: Vec<&str> =
                            recipe.kits.iter().map(|u| u.kit.name.as_str()).collect();
                        let used = if used.is_empty() {
                            "-".into()
                        } else {
                            used.join(", ")
                        };
                        println!("{name:16}  {:9}  kits: {used}", recipe.agent);
                    }
                    Err(e) => println!("{name:16}  error: {}", e.message),
                }
            }
        }
        "kits" => {
            for name in catalog::names(catalog::Kind::Kits)? {
                match kits::load(&name, None) {
                    Ok(kit) => {
                        let tools: Vec<&str> = kit.tools.iter().map(|t| t.name.as_str()).collect();
                        let tools = if tools.is_empty() {
                            "-".into()
                        } else {
                            tools.join(", ")
                        };
                        let mut marks: Vec<String> = [("hook", kit.hook), ("egress", kit.egress)]
                            .iter()
                            .filter(|(_, on)| *on)
                            .map(|(m, _)| m.to_string())
                            .collect();
                        let mut kinds: Vec<&str> = kit
                            .tools
                            .iter()
                            .map(|t| t.kind.as_str())
                            .filter(|k| *k == "run" || *k == "apt")
                            .collect();
                        kinds.sort_unstable();
                        kinds.dedup();
                        marks.extend(kinds.into_iter().map(String::from));
                        if !kit.agents.is_empty() {
                            marks.push(format!(
                                "agents: {}",
                                kit.agents.keys().cloned().collect::<Vec<_>>().join(", ")
                            ));
                        }
                        let row = format!(
                            "{name:16}  {}  tools: {tools}  {}",
                            kit.sha256,
                            marks.join("  ")
                        );
                        println!("{}", row.trim_end());
                    }
                    Err(e) => println!("{name:16}  error: {}", e.message),
                }
            }
        }
        "providers" => {
            let mut providers = PROVIDERS.to_vec();
            providers.sort_by_key(|p| p.name);
            for p in providers {
                let url = format!("{}://{}{}", p.scheme.as_str(), p.host, p.api_prefix);
                let key = if p.has_auth {
                    p.key_env
                } else {
                    "(no key needed)"
                };
                println!("{:14}  {url:34}  {key}", p.name);
            }
        }
        _ => {
            let system = sanduk_container::System;
            let picked = default_kind(std::env::consts::OS, |cli| {
                sanduk_container::Exec::which(&system, cli)
            });
            for kind in Kind::ALL {
                let found = if sanduk_container::Exec::which(&system, kind.cli()) {
                    "installed"
                } else {
                    "not installed"
                };
                let mark = if kind == picked { "default" } else { "" };
                println!(
                    "{}",
                    format!("{:8}  {:12}  {found:13}  {mark}", kind.name(), kind.cli()).trim_end()
                );
            }
        }
    }
    Ok(0)
}

fn assistant(args: AssistantArgs) -> Result<i32> {
    let db = assistants::connect()?;
    match args.action {
        AssistantAction::Add { dir } => {
            let added = assistants::load(&dir)?;
            assistants::register(&db, &added)?;
            note(&format!(
                "registered {} -> {}",
                added.name,
                added.dir.display()
            ));
        }
        AssistantAction::List => {
            let registered = assistants::rows(&db)?;
            for r in &registered {
                let left = r.next_due_at - assistants::now();
                let when = if r.disabled {
                    "disabled".to_string()
                } else if left <= 0 {
                    "due".to_string()
                } else {
                    format!("{left}s")
                };
                println!("{:16}  {when:10}  {}", r.name, r.dir);
            }
            if registered.is_empty() {
                note("no assistants registered (`sanduk assistant add <dir>`)");
            }
        }
        AssistantAction::Show { name } => println!("{}", assistants::summary(&db, &name)?),
        AssistantAction::Enable { name } => {
            assistants::set_disabled(&db, &name, false)?;
            note(&format!("{name} is enabled"));
        }
        AssistantAction::Disable { name } => {
            assistants::set_disabled(&db, &name, true)?;
            note(&format!("{name} is disabled"));
        }
    }
    Ok(0)
}

fn tell(args: TellArgs) -> Result<i32> {
    let db = assistants::connect()?;
    assistants::tell(&db, &args.name, &args.message.join(" "))?;
    note(&format!(
        "queued for {}; it arrives on the next wakeup",
        args.name
    ));
    Ok(0)
}

fn outbox(args: OutboxArgs) -> Result<i32> {
    let db = assistants::connect()?;
    if let Some(command) = &args.deliver {
        let sent = assistants::deliver(&db, command, args.name.as_deref())?;
        note(&format!("delivered {sent}"));
        return Ok(0);
    }
    let entries = assistants::outbox(&db, args.name.as_deref(), args.undelivered, args.pending)?;
    for entry in &entries {
        println!(
            "--- [{}] {} run {} {} {}",
            entry.id,
            entry.name,
            entry.run_id,
            assistants::iso(entry.created_at),
            entry.state()
        );
        println!("{}", entry.body.trim_end());
    }
    if entries.is_empty() {
        note("nothing in the outbox");
    }
    Ok(0)
}

/// approve and reject: one verb, two words for the same record.
fn decide(args: DecideArgs, approve: bool) -> Result<i32> {
    let db = assistants::connect()?;
    let changed = assistants::decide(&db, &args.id, approve)?;
    note(&format!(
        "{changed} {}",
        if approve { "approved" } else { "rejected" }
    ));
    if changed < args.id.len() {
        note("the rest were decided or delivered already");
    }
    Ok(0)
}

fn runs(args: RunsArgs) -> Result<i32> {
    let db = assistants::connect()?;
    let found = assistants::history(&db, args.name.as_deref(), args.limit)?;
    for r in &found {
        let took = r.ended_at.map_or_else(
            || "running".to_string(),
            |end| format!("{}s", end - r.started_at),
        );
        let code = r
            .exit_code
            .map_or_else(|| "-".to_string(), |c| c.to_string());
        let cost = r
            .stats
            .as_ref()
            .map_or_else(String::new, |s| format!("  {s}"));
        let why = r
            .error
            .as_ref()
            .map_or_else(String::new, |e| format!("  {e}"));
        println!(
            "{:5}  {:16}  {}  {took:>8}  exit {code}{cost}{why}",
            r.id,
            r.name,
            assistants::iso(r.started_at)
        );
    }
    if found.is_empty() {
        note("no wakeups recorded");
    }
    Ok(0)
}
