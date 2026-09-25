//! Run records: who owns a container, and what happens when that owner dies. Nothing starts a
//! container; the engine is a fake that answers for one and records what it was asked.

use std::sync::{Arc, Mutex};

use sanduk::runs::{self, claim, owner_alive, runs_dir, sweep_with};
use sanduk::{preflight, util};
use sanduk_container::{Captured, Engine, Exec, Kind};
use serde_json::Value;

/// A docker CLI holding `containers`, or one whose daemon is down.
struct Fake {
    containers: Mutex<Vec<String>>,
    reachable: bool,
    deleted: Mutex<Vec<String>>,
}

impl Exec for Fake {
    fn run(&self, argv: &[String], _capture: bool) -> Captured {
        if !self.reachable {
            return Captured {
                code: Some(1),
                stdout: String::new(),
                stderr: "Cannot connect to the daemon".into(),
            };
        }
        let ok = |stdout: String| Captured {
            code: Some(0),
            stdout,
            stderr: String::new(),
        };
        match argv[1].as_str() {
            "ps" => ok(self
                .containers
                .lock()
                .unwrap()
                .iter()
                .map(|n| format!("{n}\tsanduk:latest\trunning\n"))
                .collect()),
            "rm" => {
                self.deleted.lock().unwrap().push(argv[2].clone());
                self.containers.lock().unwrap().retain(|c| *c != argv[2]);
                ok(String::new())
            }
            _ => ok(String::new()),
        }
    }

    fn which(&self, _: &str) -> bool {
        true
    }
}

fn fake(containers: &[&str], reachable: bool) -> Arc<Fake> {
    Arc::new(Fake {
        containers: Mutex::new(containers.iter().map(|s| s.to_string()).collect()),
        reachable,
        deleted: Mutex::default(),
    })
}

fn engine_of(f: &Arc<Fake>) -> impl Fn(&str) -> sanduk::error::Result<Engine> + '_ {
    move |_| Ok(Engine::with_exec(Kind::Docker, f.clone()))
}

/// A scratch state directory for this thread.
struct State(std::path::PathBuf);

impl State {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sanduk-runs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        util::override_dirs(None, Some(dir.clone()));
        State(dir)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        util::override_dirs(None, None);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A pid that has exited and been reaped.
fn dead_pid() -> i64 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    child.wait().unwrap();
    i64::from(child.id())
}

fn rewrite(name: &str, key: &str, value: Value) {
    let path = runs_dir().join(format!("{name}.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record[key] = value;
    std::fs::write(path, record.to_string()).unwrap();
}

fn records() -> usize {
    std::fs::read_dir(runs_dir())
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn a_record_names_its_owner_and_every_container_it_starts() {
    let _s = State::new("record");
    let mut run = claim("docker", "sanduk-abcd").unwrap();
    run.add("sanduk-hold-ef").unwrap();
    let record: Value =
        serde_json::from_slice(&std::fs::read(runs_dir().join("sanduk-abcd.json")).unwrap())
            .unwrap();
    assert_eq!(
        record["containers"],
        serde_json::json!(["sanduk-abcd", "sanduk-hold-ef"])
    );
    assert_eq!(record["runtime"], "docker");
    assert!(owner_alive(record["pid"].as_i64().unwrap()));
}

#[test]
fn a_live_owner_is_left_alone() {
    let _s = State::new("live");
    let f = fake(&["sanduk-abcd"], true);
    claim("docker", "sanduk-abcd").unwrap();
    assert!(sweep_with(engine_of(&f)).is_empty());
    assert!(f.deleted.lock().unwrap().is_empty());
    assert!(runs::live_containers().contains("sanduk-abcd"));
}

/// The holder counts. A killed relayed run leaks both.
#[test]
fn a_dead_owners_containers_are_deleted() {
    let _s = State::new("dead");
    let f = fake(&["sanduk-abcd", "sanduk-hold-ef"], true);
    let mut run = claim("docker", "sanduk-abcd").unwrap();
    run.add("sanduk-hold-ef").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    assert_eq!(sweep_with(engine_of(&f)), ["sanduk-abcd", "sanduk-hold-ef"]);
    assert_eq!(
        *f.deleted.lock().unwrap(),
        ["sanduk-abcd", "sanduk-hold-ef"]
    );
    assert_eq!(records(), 0);
}

/// What --keep does: the container stays, and the next run leaves it.
#[test]
fn a_released_record_is_never_swept() {
    let _s = State::new("released");
    let f = fake(&["sanduk-abcd"], true);
    claim("docker", "sanduk-abcd").unwrap().release();
    assert!(sweep_with(engine_of(&f)).is_empty());
    assert!(f.deleted.lock().unwrap().is_empty());
}

#[test]
fn a_container_the_engine_no_longer_has_is_not_deleted() {
    let _s = State::new("gone");
    let f = fake(&[], true);
    claim("docker", "sanduk-abcd").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    assert!(sweep_with(engine_of(&f)).is_empty());
    assert!(f.deleted.lock().unwrap().is_empty());
    assert_eq!(records(), 0);
}

/// The containers are still there; the engine may be back on the next run. A stopped daemon must
/// not read as an engine holding none of them.
#[test]
fn an_unreachable_engine_keeps_the_record() {
    let _s = State::new("unreachable");
    let f = fake(&["sanduk-abcd"], false);
    claim("docker", "sanduk-abcd").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    assert!(sweep_with(engine_of(&f)).is_empty());
    assert_eq!(records(), 1);
}

#[test]
fn a_record_naming_an_unknown_engine_is_left_alone() {
    let _s = State::new("unknown");
    claim("nosuch", "sanduk-abcd").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    assert!(runs::sweep().is_empty());
    assert_eq!(records(), 1);
}

#[test]
fn an_unreadable_record_is_dropped() {
    let _s = State::new("unreadable");
    let f = fake(&[], true);
    claim("docker", "sanduk-abcd").unwrap();
    std::fs::write(runs_dir().join("sanduk-abcd.json"), "{not json").unwrap();
    assert!(sweep_with(engine_of(&f)).is_empty());
    assert_eq!(records(), 0);
}

#[test]
fn sweeping_an_empty_state_directory_is_not_an_error() {
    let _s = State::new("empty");
    assert!(sweep_with(engine_of(&fake(&[], true))).is_empty());
}

// --- the key check --------------------------------------------------------------------------------

/// Answers every request with `status`, once per connection.
fn answering(status: u16) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    addr
}

use sanduk::providers::{ANTHROPIC_PROVIDER, OPENAI_COMPAT_PROVIDER, Scheme};

/// A bad key fails here in milliseconds, not after minutes of in-container retries.
#[test]
fn a_rejected_key_stops_the_run_and_other_statuses_do_not() {
    let addr = answering(401);
    let refused = preflight::validate_key("sk-bad", Scheme::Http, &addr, "", &ANTHROPIC_PROVIDER)
        .unwrap_err();
    assert!(
        refused.message.contains("ANTHROPIC_API_KEY rejected") && refused.message.contains("401"),
        "{}",
        refused.message
    );
    // A 500 proves the endpoint answered; the agent is left to try.
    preflight::validate_key("sk", Scheme::Http, &answering(500), "", &ANTHROPIC_PROVIDER).unwrap();
}

#[test]
fn an_unreachable_endpoint_is_named() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let refused = preflight::validate_key(
        "sk",
        Scheme::Http,
        &format!("127.0.0.1:{port}"),
        "",
        &ANTHROPIC_PROVIDER,
    )
    .unwrap_err();
    assert!(
        refused
            .message
            .starts_with("cannot reach http://127.0.0.1:"),
        "{}",
        refused.message
    );
}

#[test]
fn a_keyless_provider_is_not_checked() {
    preflight::validate_key("", Scheme::Http, "127.0.0.1:1", "", &OPENAI_COMPAT_PROVIDER).unwrap();
}
