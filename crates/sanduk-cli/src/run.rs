//! `sanduk run`: the flags, the agent's process and its stream, the report and the stats file.
//! Everything around the agent's process is [`sanduk_core::run`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use clap::Args;
use regex::Regex;

use crate::launch::{Signals, launch};
use crate::{EngineArgs, ImageArgs, Stderr, note};
use sanduk_core::agent::{Outcome, REPORT_NAME};
use sanduk_core::error::{Detail, Error, ErrorKind, Result};
use sanduk_core::providers::DEFAULT_PROVIDER;
use sanduk_core::run::{Ended, Mode, Options, Returned, Verified, resolve_path};
use sanduk_core::util::{Dirs, copy_unfollowed, random_hex, seconds};

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
    /// Run on a clone of the -w repository, and fetch the agent's commits back into it as branch
    /// sanduk/<run>, uncommitted edits included. The agent never writes the repository's .git.
    /// -w must be a repository's top level with no uncommitted change to a tracked file
    #[arg(long)]
    pub clone: bool,
    /// Copy the agent's REPORT.md here after the run
    #[arg(short = 'o', long)]
    pub report: Option<PathBuf>,
    /// Another host directory in the container, e.g. ../repo:/repo:ro. Repeatable. Read-write unless :ro
    #[arg(long, value_name = "HOST:DEST[:ro]")]
    pub mount: Vec<String>,
    /// Write the run's outcome here as JSON: exit, ok, stats, error, report, mode, relay, verify
    #[arg(long)]
    pub stats_file: Option<PathBuf>,
    /// After the agent finishes, run CMD with `sh -c` in a second container: same image, mounts and
    /// network, no credential, within the same --timeout. Its result goes to --stats-file, not the
    /// exit status. With no task, only CMD runs
    #[arg(long, value_name = "CMD")]
    pub verify: Option<String>,

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

impl RunArgs {
    /// `--mode`, or the old `--proxy` spelling of sealed.
    fn mode(&self) -> Result<Mode> {
        match (&self.mode, self.proxy) {
            (None, proxy) => Ok(if proxy { Mode::Sealed } else { Mode::Open }),
            (Some(mode), true) if mode != "sealed" => Err(Error::new(format!(
                "--proxy is the old spelling of --mode sealed; it cannot be combined with --mode {mode}"
            ))),
            (Some(mode), _) => Mode::from_name(mode),
        }
    }

    fn task(&self) -> Result<Option<String>> {
        match (&self.task, &self.task_file) {
            (None, None) if self.verify.is_some() => Ok(None),
            (Some(task), None) => Ok(Some(task.clone())),
            (None, Some(file)) => Ok(Some(std::fs::read_to_string(file)?)),
            _ => Err(Error::new(
                "give exactly one of: a task argument, or --task-file",
            )),
        }
    }

    fn options(&self, dirs: &Dirs, id: &str) -> Result<Options> {
        let task = self.task()?;
        Ok(Options {
            report_instruction: !self.no_report_instruction,
            workdir: self.workdir.clone(),
            at_host_path: self.work_at_host_path,
            clone_into: self.clone.then(|| dirs.state.join("clones").join(id)),
            id: Some(id.into()),
            mounts: self.mount.clone(),
            verify: self.verify.clone(),
            runtime: self.engine.runtime.clone(),
            image: self.image.spec(),
            rebuild: self.rebuild,
            model: self.model.clone(),
            effort: self.effort.clone(),
            max_turns: self.max_turns,
            allowed_tools: self.allowed_tools.clone(),
            permission_mode: self.permission_mode.clone(),
            bare: self.bare,
            cpus: self.cpus,
            memory: self.memory.clone(),
            timeout: Duration::from_secs(seconds(&self.timeout)?),
            env: self.env.clone(),
            base_url: self.base_url.clone(),
            network: self.network.clone(),
            user: self.user.clone(),
            socket: self.socket.clone(),
            socket_env: self.socket_env.clone(),
            stdin: self.stdin,
            oci_runtime: self.oci_runtime.clone(),
            provider: self.provider.clone(),
            upstream: self.upstream.clone(),
            insecure_upstream: self.insecure_upstream,
            agent_key_env: self.agent_key_env.clone(),
            agent_base_url_env: self.agent_base_url_env.clone(),
            mode: self.mode()?,
            proxy_network: self.proxy_network.clone(),
            budget: self.budget,
            proxy_port: self.proxy_port,
            proxy_allow_path: self.proxy_allow_path.clone(),
            allow_model: self.allow_model.clone(),
            max_tokens_cap: self.max_tokens_cap,
            log_bodies: self.log_bodies,
            log_dir: self.log_dir.clone(),
            skip_key_check: self.skip_key_check || self.dry_run,
            task,
        })
    }
}

/// An interruption as the CLI ends with it: 128 plus the signal.
fn interrupted() -> Error {
    Error::with_code(
        "interrupted",
        128 + Signals::caught().unwrap_or(libc::SIGINT),
    )
}

fn or_interrupted(e: Error) -> Error {
    if e.kind == ErrorKind::Interrupted {
        interrupted()
    } else {
        e
    }
}

/// An image whose agent cannot write the workdir, with the command that rebuilds it. A build
/// runs as the calling user's uid, so the command is this user's to run only when the workdir is
/// theirs; otherwise it is the owner's.
fn with_rebuild(mut e: Error, image: &ImageArgs) -> Error {
    let Some(Detail::AgentUid {
        image: tag,
        agent_uid,
        workdir,
        owner,
        runtime,
    }) = &e.detail
    else {
        return e;
    };
    if *owner == 0 {
        return e;
    }
    let command = format!(
        "sanduk build --force {} --runtime {runtime}",
        crate::selectors(&image.spec())
    );
    // SAFETY: getuid takes no arguments and cannot fail.
    let fix = if *owner == unsafe { libc::getuid() } {
        command
    } else {
        format!("run `{command}` as that user")
    };
    e.message = format!(
        "{tag} runs its agent as uid {agent_uid}, which cannot write {} (owned by uid {owner}): \
         rebuild it as uid {owner}: {fix}",
        workdir.display()
    );
    e
}

pub fn run(dirs: &Dirs, mut args: RunArgs) -> Result<i32> {
    // Chosen here: the clone's path is named by it.
    let id = random_hex(8)?;
    let opts = args.options(dirs, &id)?;
    let sanduk = sanduk_core::Sanduk::new(dirs.clone()).observer(Arc::new(Stderr));
    let plan = sanduk.plan(opts)?;
    if args.dry_run {
        println!("{}", plan.describe()?);
        return Ok(0);
    }
    // Caught before anything is acquired: from here a signal tears down what the run started.
    let signals = Signals::catch();
    if plan.is_check_only() {
        let mode = args.mode()?;
        let verified = plan
            .check()
            .map_err(|e| with_rebuild(or_interrupted(e), &args.image))?;
        drop(signals);
        let code = verified.code();
        if let Some(stats_file) = &args.stats_file {
            let mut record = serde_json::json!({
                "exit": code,
                "ok": code == 0,
                "stats": "",
                "error": verified.error,
                "report": null,
            });
            merge(&mut record, extra(mode, None, Some(&verified)));
            std::fs::write(stats_file, record.to_string())?;
        }
        return Ok(code);
    }
    if args.clone && args.report.is_none() {
        // The container's workdir is the clone. The report is still delivered to the repository.
        args.report = Some(resolve_path(&args.workdir)?.join(REPORT_NAME));
    }
    let mut live = plan
        .start()
        .map_err(|e| with_rebuild(or_interrupted(e), &args.image))?;

    let started = Instant::now();
    let command = live.command();
    let launched = launch(
        live.agent(),
        command,
        live.timeout(),
        // The trace and the stream both go to stdout; a reader parsing one out of the other is
        // what --stream-json exists to avoid.
        args.engine.quiet || args.stream_json,
        args.stdin,
        args.stream_json,
    );
    let ended = Ended {
        exit: launched.as_ref().ok().map(|r| r.code),
        ok: matches!(&launched, Ok(r) if r.outcome.as_ref().is_some_and(|o| o.ok)),
        keep: args.keep && launched.is_ok(),
    };
    let finished = live.finish(ended);
    let caught = Signals::caught();
    drop(signals);

    let (outcome, rc, failure) = match launched {
        Ok(ran) => (ran.outcome, ran.code, String::new()),
        Err(e) if e.message == "interrupted" => return Err(e),
        Err(e) => (None, e.code, e.message),
    };
    // A signal during --verify, after the agent had finished.
    if caught.is_some() {
        return Err(interrupted());
    }
    note(&format!("{:.1}s wall", started.elapsed().as_secs_f64()));
    let spent = finished.relay.map_or(0.0, |s| s.spent);
    let outcome = outcome.map(|o| Outcome {
        stats: agent_stats(&o.stats, finished.priced, spent),
        ..o
    });
    if let Some(o) = &outcome {
        note(&o.stats);
    }
    let mut extra = extra(finished.mode, finished.relay, finished.verify.as_ref());
    if let Some(returned) = &finished.returned {
        let (branch, kept) = match returned {
            Returned::Unchanged => (None, None),
            Returned::Branch { branch, .. } => (Some(branch.clone()), None),
            Returned::Kept { path, .. } => (None, Some(path.display().to_string())),
        };
        extra["branch"] = branch.into();
        extra["kept"] = kept.into();
    }
    let report = finished
        .report
        .map_err(|refused| {
            if let Some(why) = refused {
                note(&why);
            }
        })
        .ok();
    // Where the report lies when -o is not given. With --clone, -o is always set.
    let at = resolve_path(&args.workdir)?.join(REPORT_NAME);
    collect_report(&args, &at, report, outcome.as_ref(), rc, &failure, extra)
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

fn verified_json(v: &Verified) -> serde_json::Value {
    serde_json::json!({
        "command": v.command,
        "exit": v.exit,
        "ok": v.ok(),
        "seconds": (v.seconds * 10.0).round() / 10.0,
        "timed_out": v.timed_out,
        "error": v.error,
    })
}

/// The stats file's fields beyond the agent's own.
fn extra(
    mode: Mode,
    relay: Option<sanduk_core::relay::Stats>,
    verified: Option<&Verified>,
) -> serde_json::Value {
    serde_json::json!({
        "mode": mode.name(),
        "relay": relay.map(|s| serde_json::json!({
            "requests": s.requests,
            "rejected": s.rejected,
            "spent": s.spent,
            "unpriced": s.unpriced,
        })),
        "verify": verified.map(verified_json),
    })
}

fn merge(record: &mut serde_json::Value, extra: serde_json::Value) {
    if let (Some(record), serde_json::Value::Object(extra)) = (record.as_object_mut(), extra) {
        record.extend(extra);
    }
}

/// Copies the report out and records the run, failed or not; returns its status. `report` is the
/// file the run opened, and `at` where it lies when `-o` is not given.
fn collect_report(
    args: &RunArgs,
    at: &Path,
    file: Option<std::fs::File>,
    outcome: Option<&Outcome>,
    rc: i32,
    failure: &str,
    extra: serde_json::Value,
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
    if let Some(stats_file) = &args.stats_file {
        // The exit code is all a caller gets, and the token line is printed rather than returned.
        let named = file
            .as_ref()
            .map(|_| args.report.as_deref().unwrap_or(at).display().to_string());
        let mut record = serde_json::json!({
            "exit": code,
            "ok": outcome.is_some_and(|o| o.ok),
            "stats": outcome.map_or("", |o| o.stats.as_str()),
            "error": error,
            "report": named,
        });
        merge(&mut record, extra);
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
        (Some(_), None) => note(&format!("report -> {}", at.display())),
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
}
