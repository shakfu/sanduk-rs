//! The binary, end to end, against a stand-in docker (`tests/fixtures/bin/docker`) whose `run`
//! executes a stand-in agent script. Nothing is built and no engine is contacted; every process
//! boundary is real.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;

const KEY: &str = "sk-ant-api03-REAL-KEY-STAYS-ON-HOST";

/// One test's world: a docker state directory, a workdir, sanduk's state and config, and the agent.
struct World {
    root: PathBuf,
}

impl World {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("sanduk-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["docker", "work", "state", "config", "out"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = std::fs::canonicalize(root).unwrap();
        World { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn work(&self) -> String {
        self.path("work").display().to_string()
    }

    /// The stand-in agent: a shell script given the workdir as $FAKE_WORK.
    fn agent(&self, body: &str) {
        let path = self.path("agent.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let stub = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sanduk"));
        cmd.args(args)
            .env(
                "PATH",
                format!("{}:{}", stub.display(), std::env::var("PATH").unwrap()),
            )
            .env("FAKE_DOCKER_STATE", self.path("docker"))
            .env("FAKE_AGENT", self.path("agent.sh"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("ANTHROPIC_API_KEY", KEY)
            .env_remove("OPENAI_API_KEY")
            .current_dir(&self.root);
        cmd
    }

    fn sanduk(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// `run` with the flags every test here shares.
    fn run(&self, extra: &[&str]) -> Output {
        let work = self.work();
        let mut args = vec![
            "run",
            "task",
            "-w",
            &work,
            "--agent",
            "claude",
            "--provider",
            "anthropic",
            "--runtime",
            "docker",
            "--skip-key-check",
        ];
        args.extend_from_slice(extra);
        self.sanduk(&args)
    }

    /// Every docker call so far, each as its arguments.
    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.path("docker/log"))
            .unwrap_or_default()
            .split('\x1e')
            .filter(|call| !call.is_empty())
            .map(|call| {
                call.split('\x1f')
                    .filter(|a| !a.is_empty())
                    .map(String::from)
                    .collect()
            })
            .collect()
    }

    fn containers(&self) -> String {
        std::fs::read_to_string(self.path("docker/containers")).unwrap_or_default()
    }

    fn records(&self) -> usize {
        std::fs::read_dir(self.path("state/sanduk/runs"))
            .map(|d| d.count())
            .unwrap_or(0)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Claude Code's terminal record.
const RESULT: &str = r#"{"type":"result","result":"done","is_error":false,"num_turns":1,"total_cost_usd":0.25,"usage":{"input_tokens":10,"output_tokens":2}}"#;
const TOOL_USE: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;

fn say(record: &str) -> String {
    format!("echo '{record}'")
}

// --- a run ------------------------------------------------------------------------------------------

#[test]
fn a_run_builds_the_image_runs_the_agent_copies_the_report_and_deletes_the_container() {
    let w = World::new("happy");
    w.agent(&format!(
        "{}\necho 'findings' > \"$FAKE_WORK/REPORT.md\"\n{}",
        say(TOOL_USE),
        say(RESULT)
    ));
    let report = w.path("out/REPORT.md");
    let stats = w.path("out/stats.json");
    let out = w.run(&[
        "-o",
        report.to_str().unwrap(),
        "--stats-file",
        stats.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("  > Bash"),
        "the trace: {}",
        stdout(&out)
    );
    assert_eq!(std::fs::read_to_string(&report).unwrap(), "findings\n");
    let stats: Value = serde_json::from_slice(&std::fs::read(&stats).unwrap()).unwrap();
    assert_eq!(stats["exit"], 0);
    assert_eq!(stats["ok"], true);
    assert_eq!(stats["stats"], "1 turns, 10 in (0 cached) / 2 out, $0.2500");
    let calls = w.calls();
    assert!(
        calls.iter().any(|c| c[0] == "build"),
        "the missing image was built"
    );
    let run = calls.iter().find(|c| c[0] == "run").unwrap();
    assert!(
        run.contains(&"ANTHROPIC_API_KEY".to_string()),
        "the key is named: {run:?}"
    );
    assert!(
        !run.iter().any(|a| a.contains(KEY)),
        "and never valued in the argv: {run:?}"
    );
    assert!(
        calls.iter().any(|c| c[0] == "rm"),
        "the container was deleted"
    );
    assert_eq!(w.containers(), "");
    assert_eq!(w.records(), 0);
}

/// Open mode: the container holds the key, so the stand-in agent sees it.
#[test]
fn in_open_mode_the_agent_is_handed_the_key_through_the_environment() {
    let w = World::new("open-key");
    w.agent(&format!(
        "echo \"$ANTHROPIC_API_KEY\" > \"$FAKE_WORK/seen\"\n{}",
        say(RESULT)
    ));
    assert!(w.run(&[]).status.success());
    assert_eq!(
        std::fs::read_to_string(w.path("work/seen")).unwrap().trim(),
        KEY
    );
}

#[test]
fn an_agent_that_exits_without_a_result_is_a_failure_with_its_own_status() {
    let w = World::new("noresult");
    w.agent(&format!("{}\nexit 3", say(TOOL_USE)));
    let stats = w.path("out/stats.json");
    let out = w.run(&["--stats-file", stats.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    let stats: Value = serde_json::from_slice(&std::fs::read(&stats).unwrap()).unwrap();
    assert_eq!(stats["error"], "the agent exited without a final result");
    assert_eq!(stats["report"], Value::Null);
    assert_eq!(w.containers(), "");
}

#[test]
fn an_error_result_is_exit_one_and_names_the_error() {
    let w = World::new("errresult");
    w.agent(&say(
        r#"{"type":"result","result":"rate limited","is_error":true,"usage":{}}"#,
    ));
    let out = w.run(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("agent reported an error: rate limited"));
}

/// The deadline is a watchdog: a silent agent never returns to the read loop.
#[test]
fn an_agent_that_hangs_is_killed_at_the_timeout_and_its_container_deleted() {
    let w = World::new("timeout");
    w.agent("sleep 60");
    let started = std::time::Instant::now();
    let out = w.run(&["--timeout", "1"]);
    assert_eq!(out.status.code(), Some(124), "{}", stderr(&out));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(stderr(&out).contains("agent exceeded --timeout 1s"));
    assert_eq!(w.containers(), "");
    assert_eq!(w.records(), 0);
}

/// SIGTERM would otherwise kill the process with its container alive, holding the key.
#[test]
fn a_terminated_run_deletes_its_container_first() {
    let w = World::new("sigterm");
    w.agent("echo started > \"$FAKE_WORK/started\"\nsleep 60");
    let work = w.work();
    let mut child = w
        .command(&[
            "run",
            "task",
            "-w",
            &work,
            "--agent",
            "claude",
            "--provider",
            "anthropic",
            "--runtime",
            "docker",
            "--skip-key-check",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = w.path("work/started");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the agent never started"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = child.wait().unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{err}");
    assert!(err.contains("interrupted"), "{err}");
    assert_eq!(w.containers(), "", "the container outlived the run");
    assert_eq!(w.records(), 0);
}

#[test]
fn stream_json_passes_every_line_through_verbatim_and_quietly() {
    let w = World::new("stream");
    w.agent(&format!(
        "echo 'not json'\n{}\n{}",
        say(TOOL_USE),
        say(RESULT)
    ));
    let out = w.run(&["--stream-json"]);
    assert!(out.status.success());
    let lines: Vec<String> = stdout(&out).lines().map(String::from).collect();
    assert_eq!(
        lines[..3],
        [
            "not json".to_string(),
            TOOL_USE.to_string(),
            RESULT.to_string()
        ]
    );
    assert!(
        !stdout(&out).contains("  > Bash"),
        "the trace would interleave with the stream"
    );
}

// --- the report -------------------------------------------------------------------------------------

#[test]
fn a_stale_report_is_removed_before_the_run_and_a_dry_run_leaves_it() {
    let w = World::new("stale");
    std::fs::write(w.path("work/REPORT.md"), "last week's").unwrap();
    w.agent(&say(RESULT));
    assert!(w.run(&["--dry-run"]).status.success());
    assert_eq!(
        std::fs::read_to_string(w.path("work/REPORT.md")).unwrap(),
        "last week's"
    );
    let out = w.run(&[]);
    assert!(out.status.success());
    assert!(!w.path("work/REPORT.md").exists());
    assert!(stderr(&out).contains("the agent wrote no REPORT.md"));
    assert!(
        stdout(&out).contains("\ndone"),
        "the final text stands in for the report"
    );
}

/// The agent owns the mount: a symlink there names a host path.
#[test]
fn a_report_symlink_the_agent_leaves_is_not_followed() {
    let w = World::new("symlink");
    std::fs::write(w.path("secret"), "host secret").unwrap();
    w.agent(&format!(
        "ln -s {} \"$FAKE_WORK/REPORT.md\"\n{}",
        w.path("secret").display(),
        say(RESULT)
    ));
    let copy = w.path("out/copy.md");
    let out = w.run(&["-o", copy.to_str().unwrap()]);
    assert!(
        stderr(&out).contains("refusing REPORT.md: a symlink"),
        "{}",
        stderr(&out)
    );
    assert!(!copy.exists());
}

// --- relayed ----------------------------------------------------------------------------------------

/// A plaintext upstream on 127.0.0.1 recording the headers of every request, answering a result.
fn upstream() -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap_or(0))
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            let _ = stream.read_exact(&mut body);
            let _ = tx.send(head);
            let reply = r#"{"usage":{"input_tokens":1,"output_tokens":1}}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (addr, rx)
}

/// The whole point of sealed mode: the container gets a run token, the relay swaps it for the key,
/// and the key never enters the container.
#[test]
fn a_sealed_run_relays_with_a_token_and_keeps_the_key_on_the_host() {
    let w = World::new("sealed");
    let (addr, seen) = upstream();
    w.agent(&format!(
        "echo \"$ANTHROPIC_API_KEY\" > \"$FAKE_WORK/token\"\n\
         curl -s -o \"$FAKE_WORK/answer\" -H \"x-api-key: $ANTHROPIC_API_KEY\" -H 'content-type: application/json' \
         -d '{{\"model\":\"m\",\"max_tokens\":8,\"messages\":[]}}' \"$ANTHROPIC_BASE_URL/v1/messages\"\n{}",
        say(RESULT)
    ));
    let upstream_url = format!("http://{addr}");
    let out = w.run(&["--mode", "sealed", "--upstream", &upstream_url]);
    assert!(out.status.success(), "{}", stderr(&out));
    let token = std::fs::read_to_string(w.path("work/token")).unwrap();
    assert!(
        !token.trim().is_empty() && !token.contains(KEY),
        "the container held {token:?}"
    );
    let head = seen
        .recv_timeout(Duration::from_secs(5))
        .expect("the relay reached the upstream");
    assert!(
        head.contains(&format!("x-api-key: {}", KEY.to_lowercase())),
        "{head}"
    );
    assert!(
        !head.contains(&token.trim().to_lowercase()),
        "the run token was forwarded: {head}"
    );
    assert!(
        std::fs::read_to_string(w.path("work/answer"))
            .unwrap()
            .contains("usage")
    );
    let err = stderr(&out);
    assert!(
        err.contains("relay bound to 127.0.0.1:") && err.contains("has no route off the host"),
        "{err}"
    );
    assert!(err.contains("proxy relayed 1, rejected 0"), "{err}");
    // The network was created internal. Docker needs no holder: it creates the bridge with the
    // network, where Apple's engine creates it only while a container is attached.
    let calls = w.calls();
    assert!(
        calls
            .iter()
            .any(|c| c[..3] == ["network", "create", "--internal"])
    );
    assert!(
        !calls
            .iter()
            .any(|c| c[0] == "run" && c.contains(&"-d".to_string())),
        "{calls:?}"
    );
    assert_eq!(w.containers(), "");
    assert_eq!(w.records(), 0);
}

/// A --mount that is refused is refused before the holder starts and the relay binds.
#[test]
fn a_bad_mount_is_refused_before_anything_is_started() {
    let w = World::new("badmount");
    w.agent(&say(RESULT));
    let out = w.run(&["--mode", "sealed", "--mount", "/nonexistent-dir:/data"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("is not a directory"));
    assert!(
        !w.calls().iter().any(|c| c[0] == "run" || c[0] == "network"),
        "{:?}",
        w.calls()
    );
}

// --- teardown ---------------------------------------------------------------------------------------

/// --keep leaves the container and releases the record, so the next run does not sweep it.
#[test]
fn keep_leaves_the_container_and_says_what_it_exposes() {
    let w = World::new("keep");
    w.agent(&say(RESULT));
    let out = w.run(&["--keep"]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("inspect") && stderr(&out).contains("exposes the API key"));
    assert!(w.containers().contains("\texited"));
    assert_eq!(w.records(), 0);
}

/// A container whose delete failed stays owned, so the next run's sweep can try again.
#[test]
fn a_failed_delete_keeps_the_record_for_the_next_sweep() {
    let w = World::new("faileddelete");
    w.agent(&say(RESULT));
    let work = w.work();
    let out = w
        .command(&[
            "run",
            "task",
            "-w",
            &work,
            "--agent",
            "claude",
            "--provider",
            "anthropic",
            "--runtime",
            "docker",
            "--skip-key-check",
        ])
        .env("FAKE_DOCKER_RM_EXIT", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("could not delete"));
    assert_eq!(
        w.records(),
        1,
        "the record was released with the container still there"
    );
}

// --- refusals before anything runs ------------------------------------------------------------------

#[test]
fn a_missing_key_exits_before_the_engine_is_asked_anything() {
    let w = World::new("nokey");
    let work = w.work();
    let out = w
        .command(&[
            "run",
            "task",
            "-w",
            &work,
            "--agent",
            "claude",
            "--provider",
            "anthropic",
            "--runtime",
            "docker",
        ])
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("ANTHROPIC_API_KEY is not set"));
    assert!(w.calls().is_empty());
}

/// An agent that could not write its report would still spend the run's tokens.
#[test]
fn an_image_whose_agent_cannot_write_the_workdir_is_refused() {
    let w = World::new("uid");
    w.agent(&say(RESULT));
    // Owner-only, whatever the umask: a group-writable workdir is let through on purpose.
    std::fs::set_permissions(w.path("work"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let work = w.work();
    let out = w
        .command(&[
            "run",
            "task",
            "-w",
            &work,
            "--agent",
            "claude",
            "--provider",
            "anthropic",
            "--runtime",
            "docker",
            "--skip-key-check",
        ])
        .env("FAKE_DOCKER_LABELS", r#"{"sanduk.agent-uid": "4242"}"#)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("runs its agent as uid 4242, which cannot write"),
        "{}",
        stderr(&out)
    );
    assert!(!w.calls().iter().any(|c| c[0] == "run"));
}

#[test]
fn a_bare_task_names_the_run_command() {
    let w = World::new("baretask");
    let out = w.sanduk(&["summarise this"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Did you mean: sanduk run 'summarise this'"));
}

// --- the engine verbs -------------------------------------------------------------------------------

fn seed(w: &World, rows: &[(&str, &str)]) {
    let body: String = rows
        .iter()
        .map(|(n, s)| format!("{n}\tsanduk:latest\t{s}\n"))
        .collect();
    std::fs::write(w.path("docker/containers"), body).unwrap();
}

#[test]
fn ps_lists_only_sanduk_containers() {
    let w = World::new("ps");
    seed(
        &w,
        &[("sanduk-aaaa1111", "running"), ("buildkit", "running")],
    );
    let out = w.sanduk(&["ps", "--runtime", "docker"]);
    assert!(stdout(&out).contains("sanduk-aaaa1111") && !stdout(&out).contains("buildkit"));
}

#[test]
fn stop_and_clean_act_on_sanduk_containers() {
    let w = World::new("clean");
    seed(
        &w,
        &[
            ("sanduk-aaaa1111", "running"),
            ("sanduk-bbbb2222", "exited"),
            ("buildkit", "running"),
        ],
    );
    assert!(w.sanduk(&["stop", "--runtime", "docker"]).status.success());
    assert!(
        w.calls()
            .iter()
            .any(|c| c[..] == ["stop", "sanduk-aaaa1111"])
    );
    assert!(
        !w.calls()
            .iter()
            .any(|c| c[..] == ["stop", "sanduk-bbbb2222"]),
        "a stopped container is left alone"
    );
    assert!(w.sanduk(&["clean", "--runtime", "docker"]).status.success());
    assert_eq!(w.containers(), "buildkit\tsanduk:latest\trunning\n");
}

/// A container a live process claims is a run in flight, not a leftover.
#[test]
fn clean_leaves_a_container_a_live_run_is_using_unless_told_all() {
    let w = World::new("owned");
    seed(&w, &[("sanduk-aaaa1111", "running")]);
    let runs = w.path("state/sanduk/runs");
    std::fs::create_dir_all(&runs).unwrap();
    // This test process is alive, so its pid owns the record.
    let record = serde_json::json!({"pid": std::process::id(), "runtime": "docker", "containers": ["sanduk-aaaa1111"]});
    std::fs::write(runs.join("sanduk-aaaa1111.json"), record.to_string()).unwrap();
    let out = w.sanduk(&["clean", "--runtime", "docker"]);
    assert!(stderr(&out).contains("leaving 1 to their running owner"));
    assert!(w.containers().contains("sanduk-aaaa1111"));
    w.sanduk(&["clean", "--runtime", "docker", "--all"]);
    assert_eq!(w.containers(), "");
}

#[test]
fn build_is_a_no_op_when_the_image_exists_and_force_rebuilds() {
    let w = World::new("build");
    assert!(
        w.sanduk(&["build", "--agent", "claude", "--runtime", "docker"])
            .status
            .success()
    );
    let builds = || w.calls().iter().filter(|c| c[0] == "build").count();
    assert_eq!(builds(), 1);
    let out = w.sanduk(&["build", "--agent", "claude", "--runtime", "docker"]);
    assert!(stderr(&out).contains("is already built"));
    assert_eq!(builds(), 1);
    w.sanduk(&[
        "build",
        "--agent",
        "claude",
        "--runtime",
        "docker",
        "--force",
    ]);
    assert_eq!(builds(), 2);
}

#[test]
fn destroy_removes_the_containers_every_build_of_the_recipe_and_every_mode_network() {
    let w = World::new("destroy");
    seed(&w, &[("sanduk-aaaa1111", "exited")]);
    std::fs::write(
        w.path("docker/tags"),
        "sanduk-claude:aaa\nsanduk-claude:bbb\n",
    )
    .unwrap();
    std::fs::write(
        w.path("docker/images"),
        "sanduk-claude:aaa\nsanduk-claude:bbb\n",
    )
    .unwrap();
    std::fs::create_dir_all(w.path("docker/nets")).unwrap();
    std::fs::write(w.path("docker/nets/sanduk-net"), "true").unwrap();
    std::fs::write(w.path("docker/nets/sanduk-open"), "false").unwrap();
    assert!(
        w.sanduk(&["destroy", "--agent", "claude", "--runtime", "docker"])
            .status
            .success()
    );
    assert_eq!(w.containers(), "");
    assert_eq!(
        std::fs::read_to_string(w.path("docker/images")).unwrap(),
        ""
    );
    assert!(
        std::fs::read_dir(w.path("docker/nets"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn an_unreachable_daemon_is_named_with_its_fix() {
    let w = World::new("daemon");
    let out = w
        .command(&["ps", "--runtime", "docker"])
        .env("FAKE_DOCKER_DOWN", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("the docker daemon at unix:///var/run/docker.sock is not reachable"),
        "{}",
        stderr(&out)
    );
}

// --- assistants -------------------------------------------------------------------------------------

/// An assistant directory in `w`, run by the stand-in agent through the stub engine.
fn assistant(w: &World, extra: &str) -> String {
    let dir = w.path("triage");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("brief.md"), "Triage the inbox.").unwrap();
    std::fs::write(
        dir.join("assistant.toml"),
        format!(
            "agent = \"claude\"\nprovider = \"anthropic\"\nruntime = \"docker\"\nbrief = \"brief.md\"\n\
             args = [\"--skip-key-check\"]\n{extra}\n"
        ),
    )
    .unwrap();
    dir.display().to_string()
}

/// A wakeup is one `sanduk run`: registered, told, ticked, and its report lands in the outbox.
#[test]
fn a_tick_runs_a_wakeup_end_to_end_and_its_report_reaches_the_outbox() {
    let w = World::new("tick");
    w.agent(&format!("cat \"$FAKE_WORK\"/../reports/*.task.md > /dev/null 2>&1; echo 'triaged' > \"$FAKE_WORK/REPORT.md\"\n{}", say(RESULT)));
    let dir = assistant(&w, "approval = true");
    assert!(w.sanduk(&["assistant", "add", &dir]).status.success());
    let listed = stdout(&w.sanduk(&["assistant", "list"]));
    assert!(
        listed.starts_with("triage") && listed.contains("due"),
        "{listed}"
    );
    assert!(
        w.sanduk(&["tell", "triage", "look", "at", "PR", "12"])
            .status
            .success()
    );
    assert!(stdout(&w.sanduk(&["assistant", "show", "triage"])).contains("\"pending\": 1"));

    let out = w.sanduk(&["tick"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let task = std::fs::read_dir(w.path("triage/reports"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".task.md"))
        .unwrap();
    let task = std::fs::read_to_string(task).unwrap();
    assert!(
        task.contains("Triage the inbox.") && task.contains("look at PR 12"),
        "{task}"
    );
    // Sealed by default: the wakeup went through the relay's network.
    assert!(
        w.calls()
            .iter()
            .any(|c| c[..3] == ["network", "create", "--internal"])
    );

    let outbox = stdout(&w.sanduk(&["outbox"]));
    assert!(
        outbox.contains("[1] triage run 1")
            && outbox.contains("pending")
            && outbox.contains("triaged"),
        "{outbox}"
    );
    let runs = stdout(&w.sanduk(&["runs"]));
    assert!(
        runs.contains("exit 0") && runs.contains("1 turns, 10 in"),
        "{runs}"
    );
    assert!(stdout(&w.sanduk(&["assistant", "show", "triage"])).contains("\"pending\": 0"));

    // Approval gates delivery.
    let sink = w.path("out/sent.txt");
    let deliver = format!("cat >> {}", sink.display());
    w.sanduk(&["outbox", "--deliver", &deliver]);
    assert!(!sink.exists());
    assert!(w.sanduk(&["approve", "1"]).status.success());
    assert!(stderr(&w.sanduk(&["approve", "1"])).contains("already"));
    w.sanduk(&["outbox", "--deliver", &deliver]);
    assert_eq!(std::fs::read_to_string(&sink).unwrap(), "triaged\n");
}

#[test]
fn telling_an_unknown_assistant_is_an_error_and_empty_listings_are_not() {
    let w = World::new("assistant-empty");
    let out = w.sanduk(&["tell", "nope", "hello"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("assistant list"));
    assert!(stderr(&w.sanduk(&["outbox"])).contains("nothing in the outbox"));
    assert!(stderr(&w.sanduk(&["runs"])).contains("no wakeups recorded"));
    assert!(stderr(&w.sanduk(&["assistant", "list"])).contains("no assistants registered"));
}

// --- the relay probe --------------------------------------------------------------------------------

/// A sealed run probes the relay from inside the network before the agent starts, and the probe's
/// container goes with the run.
#[test]
fn a_sealed_run_probes_the_relay_first() {
    let w = World::new("probe-ok");
    w.agent(&say(RESULT));
    let out = w.run(&["--mode", "sealed"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let calls = w.calls();
    let probe = calls
        .iter()
        .position(|c| c[0] == "run" && c.contains(&"curl".to_string()))
        .expect("a probe");
    let agent = calls
        .iter()
        .position(|c| c[0] == "run" && !c.contains(&"curl".to_string()))
        .expect("the agent");
    assert!(probe < agent, "the probe runs before the agent");
    assert!(calls[probe].last().unwrap().ends_with("/_sanduk/ping"));
    assert_eq!(w.containers(), "");
}

/// A firewall that drops the container's connection: the run stops in seconds with the likely
/// cause, where the agent would have hung until --timeout, and gives back everything it held.
#[test]
fn a_run_whose_container_cannot_reach_the_relay_stops_before_the_agent() {
    let w = World::new("probe-fail");
    w.agent(&say(RESULT));
    let work = w.work();
    let out = w
        .command(&[
            "run",
            "task",
            "-w",
            &work,
            "--agent",
            "claude",
            "--provider",
            "anthropic",
            "--runtime",
            "docker",
            "--skip-key-check",
            "--mode",
            "sealed",
        ])
        .env("FAKE_DOCKER_PROBE_FAIL", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(
        err.contains("is not reachable from sanduk-net") && err.contains("likely cause"),
        "{err}"
    );
    assert!(
        !w.calls()
            .iter()
            .any(|c| c[0] == "run" && !c.contains(&"curl".to_string())),
        "the agent ran"
    );
    assert_eq!(w.containers(), "");
    assert_eq!(w.records(), 0);
}
