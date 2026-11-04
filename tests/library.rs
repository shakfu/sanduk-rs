//! The run as a library caller drives it: plan, start, the caller's own process, finish. The
//! engine is `sanduk_container::testing::Fake`: no daemon, and every container that runs, runs
//! on this host.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sanduk_container::testing::Fake;
use sanduk_core::Sanduk;
use sanduk_core::error::{Detail, ErrorKind};
use sanduk_core::observe::{Note, Observer};
use sanduk_core::run::{Ended, Mode, Options, Returned};
use sanduk_core::util::Dirs;

/// Every note, in order.
#[derive(Default)]
struct Notes(Mutex<Vec<Note>>);

impl Observer for Notes {
    fn note(&self, note: Note) {
        self.0.lock().unwrap().push(note);
    }
}

impl Notes {
    fn infos(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|n| match n {
                Note::Info(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    fn builds(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|n| match n {
                Note::Build(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    fn output(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|n| match n {
                Note::Output(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }
}

/// One test's directories, removed when dropped.
struct World {
    root: PathBuf,
    fake: Arc<Fake>,
    notes: Arc<Notes>,
}

impl World {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("sanduk-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("work")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        World {
            root,
            fake: Fake::new(),
            notes: Arc::default(),
        }
    }

    fn sanduk(&self) -> Sanduk {
        Sanduk::new(Dirs {
            cache: Dirs::default().cache,
            ..Dirs::under(&self.root)
        })
        .engine(self.fake.engine(sanduk_core::prefix()))
        .observer(self.notes.clone())
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn options(&self) -> Options {
        Options {
            task: Some("do the task".into()),
            workdir: self.work(),
            image: sanduk_core::run::ImageSpec {
                agent: Some("pi".into()),
                ..Default::default()
            },
            provider: "openai-compat".into(),
            model: Some("m".into()),
            timeout: Duration::from_secs(60),
            skip_key_check: true,
            ..Options::default()
        }
    }

    fn records(&self) -> usize {
        std::fs::read_dir(self.root.join("state/runs"))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .count()
            })
            .unwrap_or(0)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn sh(dir: &Path, script: &str) -> Output {
    let out = Command::new("sh")
        .args(["-c", script])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{script}: {out:?}");
    out
}

/// The fake agent: a shell script, run in the directory mounted as the workdir.
fn agent(script: &'static str) -> impl Fn(&Path, &[String]) -> Command + Send + Sync {
    move |_, _| {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }
}

#[test]
fn a_run_starts_the_callers_process_and_finishes_with_nothing_left() {
    let w = World::new("open");
    w.fake.on_agent(agent("echo found > REPORT.md"));
    let plan = w.sanduk().plan(w.options()).unwrap();
    let name = plan.name().to_string();
    let mut live = plan.start().unwrap();
    assert_eq!(w.records(), 1, "a started run is recorded");
    let status = live.command().status().unwrap();
    let outcome = live.finish(status);

    assert_eq!(outcome.exit, Some(0));
    assert_eq!(outcome.mode, Mode::Open);
    assert!(outcome.relay.is_none());
    let mut report = String::new();
    std::io::Read::read_to_string(&mut outcome.report.unwrap(), &mut report).unwrap();
    assert_eq!(report, "found\n");
    assert!(!w.fake.containers().contains_key(&name));
    assert_eq!(w.records(), 0);
    assert!(
        w.notes.infos().iter().any(|n| n.starts_with(&name)),
        "{:?}",
        w.notes.infos()
    );
    // The image was missing, so the run built it, and the build's output came as notes.
    let builds = w.notes.builds();
    assert!(
        builds.len() == 1 && builds[0].starts_with("built docker build"),
        "{builds:?}"
    );
}

/// Behind the relay the agent's process holds a token, not the key, and the relay is gone once
/// the run is.
#[test]
fn a_keyed_run_hands_the_agent_a_token_and_reports_the_relay() {
    let w = World::new("keysafe");
    w.fake.on_agent(agent("env > env.txt; echo r > REPORT.md"));
    let plan = w
        .sanduk()
        .plan(Options {
            mode: Mode::KeySafe,
            verify: Some("test -s env.txt && echo checked".into()),
            ..w.options()
        })
        .unwrap();
    let mut live = plan.start().unwrap();
    // The credential's variable: the first bare `-e NAME`, its value inherited from the process.
    let key_env = live
        .argv()
        .windows(2)
        .find(|p| p[0] == "-e" && !p[1].contains('='))
        .map(|p| p[1].clone())
        .unwrap();
    let status = live.command().status().unwrap();
    let outcome = live.finish(status);

    let env = std::fs::read_to_string(w.work().join("env.txt")).unwrap();
    let token = env
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key_env}=")))
        .unwrap_or_else(|| panic!("no {key_env} in {env}"));
    assert_eq!(token.len(), 32, "{token:?}");
    let stats = outcome.relay.expect("a keyed run reports its relay");
    assert_eq!((stats.requests, stats.rejected), (0, 0));
    let verified = outcome.verify.unwrap();
    assert_eq!(verified.ok(), Some(true), "{verified:?}");
    assert_eq!(w.notes.output(), ["checked"]);
    assert!(w.fake.networks().contains(&"sanduk-open".to_string()));
    assert!(w.fake.containers().is_empty(), "{:?}", w.fake.containers());
    assert_eq!(w.records(), 0);
}

fn repo(dir: &Path) {
    sh(
        dir,
        "git init -q && echo a > a.txt && git add a.txt && \
         git -c user.name=t -c user.email=t@t commit -qm base",
    );
}

/// The caller names the clone's path. The agent's edit comes back as a branch; the clone goes.
#[test]
fn a_clone_at_the_callers_path_comes_back_as_a_branch() {
    let w = World::new("clone");
    repo(&w.work());
    w.fake.on_agent(agent("echo b > b.txt; echo r > REPORT.md"));
    let copy = w.root.join("mine/clones/run-7");
    let plan = w
        .sanduk()
        .plan(Options {
            clone_into: Some(copy.clone()),
            id: Some("run-7".into()),
            ..w.options()
        })
        .unwrap();
    assert_eq!(plan.branch(), Some("sanduk/run-7"));
    let mut live = plan.start().unwrap();
    assert_eq!(live.work(), copy);
    let status = live.command().status().unwrap();
    let outcome = live.finish(status);

    assert_eq!(
        outcome.returned,
        Some(Returned::Branch {
            branch: "sanduk/run-7".into(),
            commits: 1
        })
    );
    let shown = sh(&w.work(), "git show sanduk/run-7:b.txt");
    assert_eq!(String::from_utf8_lossy(&shown.stdout), "b\n");
    assert!(outcome.report.is_ok(), "the report outlives the clone");
    assert!(!copy.exists(), "a returned clone is removed");
}

/// Dropped without `finish`: containers and record go, the clone stays, and nothing returns.
#[test]
fn a_run_dropped_without_finishing_keeps_the_clone_and_leaks_no_container() {
    let w = World::new("dropped");
    repo(&w.work());
    w.fake.on_agent(agent("echo b > b.txt"));
    let copy = w.root.join("clones/x");
    let plan = w
        .sanduk()
        .plan(Options {
            clone_into: Some(copy.clone()),
            mode: Mode::KeySafe,
            ..w.options()
        })
        .unwrap();
    let mut live = plan.start().unwrap();
    live.command().status().unwrap();
    drop(live);

    assert!(copy.join("b.txt").exists(), "the clone was not kept");
    assert!(w.fake.containers().is_empty(), "{:?}", w.fake.containers());
    assert_eq!(w.records(), 0);
    let branches = sh(&w.work(), "git branch --list 'sanduk/*'");
    assert!(
        branches.stdout.is_empty(),
        "a dropped run returned its work"
    );
}

/// A dropped run whose agent never started holds no work: its clone goes too.
#[test]
fn a_run_dropped_before_its_agent_started_removes_its_clone() {
    let w = World::new("unstarted");
    repo(&w.work());
    let copy = w.root.join("clones/y");
    let plan = w
        .sanduk()
        .plan(Options {
            clone_into: Some(copy.clone()),
            ..w.options()
        })
        .unwrap();
    drop(plan.start().unwrap());
    assert!(!copy.exists());
    assert_eq!(w.records(), 0);
}

#[test]
fn a_refusal_comes_back_before_anything_is_acquired() {
    let w = World::new("refused");
    repo(&w.work());
    std::fs::write(w.work().join("a.txt"), "dirty").unwrap();
    let refused = w
        .sanduk()
        .plan(Options {
            clone_into: Some(w.root.join("clones/z")),
            ..w.options()
        })
        .err()
        .unwrap();
    assert_eq!(refused.kind, ErrorKind::Refused, "{refused:?}");
    assert!(
        refused.message.contains("uncommitted"),
        "{}",
        refused.message
    );

    std::fs::create_dir_all(w.root.join("taken")).unwrap();
    sh(&w.work(), "git checkout -q a.txt");
    let taken = w
        .sanduk()
        .plan(Options {
            clone_into: Some(w.root.join("taken")),
            ..w.options()
        })
        .err()
        .unwrap();
    assert_eq!(taken.kind, ErrorKind::Refused);
    assert!(
        taken.message.contains("already exists"),
        "{}",
        taken.message
    );
    assert!(w.fake.calls().is_empty(), "{:?}", w.fake.calls());
}

/// A failed check is the check's result, not the run's error. Verify runs only after an agent
/// that finished.
#[test]
fn the_check_runs_only_after_an_agent_that_finished() {
    let w = World::new("verify");
    w.fake.on_agent(agent("exit 1"));
    let plan = w
        .sanduk()
        .plan(Options {
            verify: Some("echo ran".into()),
            ..w.options()
        })
        .unwrap();
    let mut live = plan.start().unwrap();
    let status = live.command().status().unwrap();
    let outcome = live.finish(Ended {
        keep: false,
        ..Ended::from(status)
    });
    let verified = outcome.verify.unwrap();
    assert_eq!(verified.exit, None);
    assert!(verified.error.starts_with("not run"), "{verified:?}");
    assert!(w.notes.output().is_empty());
}

/// The fake's reaper never calls the host's `docker`, which may have a daemon behind it.
#[test]
fn the_fakes_reaper_reaches_no_engine_on_the_host() {
    let w = World::new("reapercli");
    assert_eq!(w.fake.engine(sanduk_core::prefix()).reaper_cli(), "false");
}

/// An image built for another uid is refused with its facts, so a caller can offer its own fix.
#[test]
fn an_image_whose_agent_cannot_write_the_workdir_says_why_as_data() {
    let w = World::new("agentuid");
    // A group-writable workdir is not a sure refusal; fix the mode so the umask cannot decide.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(w.work(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = w.sanduk().plan(w.options()).unwrap();
    let tag = plan.image().tag.clone();
    // SAFETY: getuid takes no arguments and cannot fail.
    let me = unsafe { libc::getuid() };
    w.fake.with_image(&tag, Some(me + 1));
    let refused = plan.start().err().unwrap();
    assert_eq!(refused.kind, ErrorKind::Refused);
    assert_eq!(
        refused.detail,
        Some(Detail::AgentUid {
            image: tag,
            agent_uid: me + 1,
            workdir: w.work(),
            owner: me,
            runtime: "docker",
        })
    );
    assert!(w.fake.containers().is_empty());
    assert_eq!(w.records(), 0);
}
