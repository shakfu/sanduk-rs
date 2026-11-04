//! Run records: who owns a container, and what happens when that owner dies. Nothing starts a
//! container; the engine is a fake that answers for one and records what it was asked.

use std::sync::{Arc, Mutex};

use sanduk_container::runs::{self, Reaper, claim, owner_alive, sweep_with};
use sanduk_container::{Captured, Engine, Exec, Kind, Prefix};
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

fn sanduk() -> Prefix {
    Prefix::new("sanduk-").unwrap()
}

fn engine_of(f: &Arc<Fake>) -> impl Fn(&str) -> sanduk_container::Result<Engine> + '_ {
    move |_| Ok(Engine::with_exec(Kind::Docker, f.clone(), sanduk()))
}

/// An engine whose reaper calls the stub `cli`.
fn reaps_with(cli: &std::path::Path) -> Engine {
    let exec = Arc::new(ReapsWith(cli.to_str().unwrap().into()));
    Engine::with_exec(Kind::Docker, exec, sanduk())
}

thread_local! {
    static RUNS: std::cell::RefCell<std::path::PathBuf> = std::cell::RefCell::default();
}

/// This thread's records directory, which [`State`] sets.
fn runs_dir() -> std::path::PathBuf {
    RUNS.with(|r| r.borrow().clone())
}

/// A scratch state directory for this thread.
struct State(std::path::PathBuf);

impl State {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sanduk-runs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        RUNS.with(|r| *r.borrow_mut() = dir.join("runs"));
        State(dir)
    }
}

impl Drop for State {
    fn drop(&mut self) {
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

/// Records, and the names files beside them, in this thread's directory.
fn files(extension: &str) -> usize {
    std::fs::read_dir(runs_dir())
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == extension))
                .count()
        })
        .unwrap_or(0)
}

fn records() -> usize {
    files("json")
}

#[test]
fn a_record_names_its_owner_and_every_container_it_starts() {
    let _s = State::new("record");
    let mut run = claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
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
    claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
    assert!(sweep_with(&runs_dir(), engine_of(&f), &|_| {}).is_empty());
    assert!(f.deleted.lock().unwrap().is_empty());
    assert!(runs::live_containers(&runs_dir()).contains("sanduk-abcd"));
}

/// The holder counts. A killed relayed run leaks both.
#[test]
fn a_dead_owners_containers_are_deleted() {
    let _s = State::new("dead");
    let f = fake(&["sanduk-abcd", "sanduk-hold-ef"], true);
    let mut run = claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
    run.add("sanduk-hold-ef").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    assert_eq!(
        sweep_with(&runs_dir(), engine_of(&f), &|_| {}),
        ["sanduk-abcd", "sanduk-hold-ef"]
    );
    assert_eq!(
        *f.deleted.lock().unwrap(),
        ["sanduk-abcd", "sanduk-hold-ef"]
    );
    assert_eq!((records(), files("names")), (0, 0));
}

/// What --keep does: the container stays, and the next run leaves it.
#[test]
fn a_released_record_is_never_swept() {
    let _s = State::new("released");
    let f = fake(&["sanduk-abcd"], true);
    claim(&runs_dir(), "docker", "sanduk-abcd")
        .unwrap()
        .release();
    assert!(sweep_with(&runs_dir(), engine_of(&f), &|_| {}).is_empty());
    assert!(f.deleted.lock().unwrap().is_empty());
}

#[test]
fn a_container_the_engine_no_longer_has_is_not_deleted() {
    let _s = State::new("gone");
    let f = fake(&[], true);
    claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    assert!(sweep_with(&runs_dir(), engine_of(&f), &|_| {}).is_empty());
    assert!(f.deleted.lock().unwrap().is_empty());
    assert_eq!(records(), 0);
}

/// The containers are still there; the engine may be back on the next run. A stopped daemon must
/// not read as an engine holding none of them.
#[test]
fn an_unreachable_engine_keeps_the_record() {
    let _s = State::new("unreachable");
    let f = fake(&["sanduk-abcd"], false);
    claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    assert!(sweep_with(&runs_dir(), engine_of(&f), &|_| {}).is_empty());
    assert_eq!(records(), 1);
}

#[test]
fn a_record_naming_an_unknown_engine_is_left_alone() {
    let _s = State::new("unknown");
    claim(&runs_dir(), "nosuch", "sanduk-abcd").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    assert!(runs::sweep(&runs_dir(), &sanduk(), &|_| {}).is_empty());
    assert_eq!(records(), 1);
}

#[test]
fn an_unreadable_record_is_dropped() {
    let _s = State::new("unreadable");
    let f = fake(&[], true);
    claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
    std::fs::write(runs_dir().join("sanduk-abcd.json"), "{not json").unwrap();
    assert!(sweep_with(&runs_dir(), engine_of(&f), &|_| {}).is_empty());
    assert_eq!(records(), 0);
}

/// A stand-in engine CLI for the reaper's shell: `alive` lists the containers it holds, `log`
/// each call. A name starting `sanduk-stuck` cannot be deleted.
fn stub_cli(alive: &[&str]) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = runs_dir().parent().unwrap().join("cli");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("alive"),
        alive.iter().map(|a| format!("{a}\n")).collect::<String>(),
    )
    .unwrap();
    let script = format!(
        r#"#!/bin/sh
d={dir}
echo "$*" >> "$d/log"
case "$1" in
  stop) exit 0 ;;
  inspect) grep -qx "$2" "$d/alive" ;;
  rm)
    case "$2" in sanduk-stuck*) exit 1 ;; esac
    grep -qx "$2" "$d/alive" || exit 1
    grep -vx "$2" "$d/alive" > "$d/alive.new"; mv "$d/alive.new" "$d/alive" ;;
esac
"#,
        dir = dir.display()
    );
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn stub_read(cli: &std::path::Path, file: &str) -> String {
    std::fs::read_to_string(cli.with_file_name(file)).unwrap_or_default()
}

/// The reaper is `sh` and the engine's CLI, so it works under any host binary. It knows its run
/// is over and does not ask the pid: a killed owner not yet waited for answers as alive.
#[test]
fn a_reaper_deletes_its_record_s_containers_whoever_owns_them() {
    let _s = State::new("reap");
    let cli = stub_cli(&["sanduk-abcd", "sanduk-hold-ef", "sanduk-other"]);
    let mut run = claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
    run.add("sanduk-hold-ef").unwrap();
    assert_eq!(
        std::fs::read_to_string(run.names_path()).unwrap(),
        "sanduk-abcd\nsanduk-hold-ef\n"
    );
    let reaper = Reaper::start(&run, &reaps_with(&cli), &|_| {}).unwrap();
    assert_eq!(
        stub_read(&cli, "log"),
        "",
        "the reaper acted before its run ended"
    );
    drop(reaper);
    assert_eq!(stub_read(&cli, "alive"), "sanduk-other\n");
    assert_eq!((records(), files("names")), (0, 0));
}

/// An engine whose reaper calls the CLI it holds, and which answers nothing else.
struct ReapsWith(String);

impl Exec for ReapsWith {
    fn run(&self, argv: &[String], _capture: bool) -> Captured {
        panic!("the reaper ran {argv:?} through the engine, not its own shell")
    }

    fn which(&self, _: &str) -> bool {
        true
    }

    fn reaper_cli(&self) -> Option<&str> {
        Some(&self.0)
    }
}

/// An engine sweeps only its own prefix: a record naming another's container is not its to judge.
#[test]
fn a_sweep_leaves_another_prefixs_containers_and_records_alone() {
    let _s = State::new("prefix");
    let f = fake(&["sanduk-abcd", "pma-abcd"], true);
    claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
    claim(&runs_dir(), "docker", "pma-abcd").unwrap();
    rewrite("sanduk-abcd", "pid", dead_pid().into());
    rewrite("pma-abcd", "pid", dead_pid().into());
    let pma = |_: &str| {
        Ok(Engine::with_exec(
            Kind::Docker,
            f.clone(),
            Prefix::new("pma-")?,
        ))
    };
    assert_eq!(sweep_with(&runs_dir(), pma, &|_| {}), ["pma-abcd"]);
    assert_eq!(*f.deleted.lock().unwrap(), ["pma-abcd"]);
    assert_eq!(records(), 1);
}

/// A run that tore down released its record before the reaper woke.
#[test]
fn a_reaper_of_a_released_record_deletes_nothing() {
    let _s = State::new("reapreleased");
    let cli = stub_cli(&["sanduk-abcd"]);
    let run = claim(&runs_dir(), "docker", "sanduk-abcd").unwrap();
    let reaper = Reaper::start(&run, &reaps_with(&cli), &|_| {}).unwrap();
    run.release();
    drop(reaper);
    assert_eq!(stub_read(&cli, "log"), "");
    assert_eq!(files("names"), 0);
}

/// A delete that failed keeps the record, for the next sweep; one the engine no longer has is
/// already gone.
#[test]
fn a_reaper_keeps_the_record_of_a_container_it_could_not_delete() {
    let _s = State::new("reapstuck");
    let cli = stub_cli(&["sanduk-stuck-1"]);
    let mut run = claim(&runs_dir(), "docker", "sanduk-gone").unwrap();
    run.add("sanduk-stuck-1").unwrap();
    drop(Reaper::start(&run, &reaps_with(&cli), &|_| {}).unwrap());
    assert_eq!(stub_read(&cli, "alive"), "sanduk-stuck-1\n");
    assert_eq!((records(), files("names")), (1, 1));
}

#[test]
fn sweeping_an_empty_state_directory_is_not_an_error() {
    let _s = State::new("empty");
    assert!(sweep_with(&runs_dir(), engine_of(&fake(&[], true)), &|_| {}).is_empty());
}
