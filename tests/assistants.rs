//! Assistants: config, the state a run outlives, and one wakeup. Nothing starts a container:
//! `wake` is handed a fake run command, the seam the whole module is built on.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use sanduk::assistants::{self as a, Assistant};
use sanduk::util;

const CONFIG: &str = r#"
name = "triage"
agent = "pi"
provider = "openai-compat"
model = "local-model"
every = "30m"
brief = "brief.md"
"#;

/// A scratch state directory and one assistant's directory, for this thread.
struct Home {
    root: PathBuf,
    dir: PathBuf,
}

impl Home {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("sanduk-assist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("triage");
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let dir = root.join("triage");
        std::fs::write(dir.join("assistant.toml"), CONFIG).unwrap();
        std::fs::write(dir.join("brief.md"), "Triage the inbox.").unwrap();
        util::override_dirs(None, Some(root.join("state")));
        Home { root, dir }
    }

    fn config(&self, extra: &str) {
        std::fs::write(
            self.dir.join("assistant.toml"),
            format!("{CONFIG}\n{extra}\n"),
        )
        .unwrap();
    }

    fn load(&self) -> Assistant {
        a::load(&self.dir).unwrap()
    }

    fn db(&self) -> Connection {
        a::connect().unwrap()
    }

    /// Loaded and registered.
    fn registered(&self, db: &Connection) -> Assistant {
        let found = self.load();
        a::register(db, &found).unwrap();
        found
    }

    /// Another assistant beside this one.
    fn sibling(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("assistant.toml"),
            CONFIG.replace("\"triage\"", &format!("\"{name}\"")),
        )
        .unwrap();
        std::fs::write(dir.join("brief.md"), "Something else.").unwrap();
        dir
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        util::override_dirs(None, None);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A run command that writes a report and a stats file, exits `code`, and records its argv.
struct Ran {
    calls: RefCell<Vec<Vec<String>>>,
    code: i32,
}

impl Ran {
    fn new(code: i32) -> Self {
        Ran {
            calls: RefCell::new(Vec::new()),
            code,
        }
    }

    fn run(&self, argv: &[String]) -> i32 {
        self.calls.borrow_mut().push(argv.to_vec());
        let after = |flag: &str| argv[argv.iter().position(|x| x == flag).unwrap() + 1].clone();
        if self.code == 0 {
            std::fs::write(after("-o"), "the report").unwrap();
        }
        std::fs::write(
            after("--stats-file"),
            format!(
                "{{\"exit\": {}, \"stats\": \"900 in / 40 out\"}}",
                self.code
            ),
        )
        .unwrap();
        self.code
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.borrow().clone()
    }
}

fn after<'a>(argv: &'a [String], flag: &str) -> &'a str {
    &argv[argv.iter().position(|x| x == flag).unwrap() + 1]
}

fn wake(db: &Connection, found: &Assistant, ran: &Ran) -> i32 {
    a::wake(db, found, None, &|argv| ran.run(argv)).unwrap()
}

fn run_rows(db: &Connection) -> Vec<a::Wakeup> {
    let mut rows = a::history(db, None, 100).unwrap();
    rows.reverse();
    rows
}

// --- config ---------------------------------------------------------------------------------------

#[test]
fn a_config_is_read_with_the_directory_as_its_root() {
    let h = Home::new("config");
    let found = h.load();
    assert_eq!(found.name, "triage");
    assert_eq!(
        (found.agent.as_deref(), found.model.as_deref()),
        (Some("pi"), Some("local-model"))
    );
    assert_eq!(found.every, 1800);
    assert_eq!(
        found.brief.as_deref(),
        Some(h.dir.join("brief.md").as_path())
    );
    assert_eq!(found.workspace(), h.dir.join("workspace"));
    // The defaults that matter for an unattended run: the strictest mode, a bounded wakeup.
    assert_eq!((found.mode.as_str(), found.timeout), ("sealed", 900));
}

#[test]
fn a_timeout_reads_the_same_way_as_an_interval() {
    let h = Home::new("timeout");
    h.config("timeout = \"5m\"");
    assert_eq!(h.load().timeout, 300);
    h.config("timeout = 120");
    assert_eq!(h.load().timeout, 120);
}

#[test]
fn a_config_that_would_fail_later_fails_at_load() {
    let h = Home::new("badconfig");
    for (extra, expected) in [
        // A typo in a schedule is a wakeup that never happens.
        ("evry = \"5m\"", "unknown keys evry"),
        ("mode = \"airgapped\"", "key-safe"),
        ("timeout = \"half an hour\"", "duration"),
    ] {
        h.config(extra);
        let refused = a::load(&h.dir).unwrap_err().message;
        assert!(refused.contains(expected), "{extra}: {refused}");
    }
    h.config("");
    std::fs::remove_file(h.dir.join("brief.md")).unwrap();
    assert!(a::load(&h.dir).unwrap_err().message.contains("brief"));
    assert!(
        a::load(&h.root)
            .unwrap_err()
            .message
            .contains("assistant.toml")
    );
}

/// `proxy = false` was the old spelling of `mode = "open"`.
#[test]
fn the_boolean_mode_replaced_still_reads() {
    let h = Home::new("proxy");
    h.config("proxy = false");
    assert_eq!(h.load().mode, "open");
    h.config("proxy = true");
    assert_eq!(h.load().mode, "sealed");
}

/// `../repo:/repo:ro` means beside the config, not beside wherever the scheduler ran from.
#[test]
fn a_configured_mount_is_anchored_to_the_assistant_and_an_absolute_one_left_alone() {
    let h = Home::new("mounts");
    std::fs::create_dir_all(h.root.join("repo")).unwrap();
    h.config("mounts = [\"../repo:/repo:ro\", \"/srv/data:/data\"]");
    let found = h.load();
    assert_eq!(
        found.mounts,
        [
            format!("{}:/repo:ro", h.root.join("repo").display()),
            "/srv/data:/data".into()
        ]
    );
}

#[test]
fn a_relative_recipe_path_is_resolved_from_the_assistant() {
    let h = Home::new("recipe");
    std::fs::write(h.dir.join("recipe.json"), "{}").unwrap();
    std::fs::write(h.dir.join("assistant.toml"), "recipe = \"recipe.json\"\n").unwrap();
    let found = h.load();
    assert_eq!(found.agent, None);
    assert_eq!(
        found.recipe,
        Some(h.dir.join("recipe.json").display().to_string())
    );
    let argv = a::run_argv(&found, Path::new("/t"), Path::new("/r"), None, None);
    assert!(!argv.contains(&"--agent".to_string()));
    assert_eq!(after(&argv, "--recipe"), found.recipe.as_deref().unwrap());
}

// --- state ----------------------------------------------------------------------------------------

#[test]
fn registering_twice_keeps_the_schedule() {
    let h = Home::new("reregister");
    let db = h.db();
    let found = h.registered(&db);
    db.execute(
        "UPDATE assistants SET next_due_at = 4102444800 WHERE name = 'triage'",
        [],
    )
    .unwrap();
    a::register(&db, &found).unwrap();
    assert_eq!(a::row(&db, "triage").unwrap().next_due_at, 4102444800);
}

#[test]
fn an_unknown_assistant_names_the_command_that_lists_them() {
    let h = Home::new("unknown");
    assert!(
        a::row(&h.db(), "nope")
            .unwrap_err()
            .message
            .contains("assistant list")
    );
}

#[test]
fn a_claim_is_refused_while_a_live_process_holds_it_and_taken_from_a_dead_one() {
    let h = Home::new("claim");
    let db = h.db();
    h.registered(&db);
    // pid 1 is alive and is not us.
    db.execute(
        "UPDATE assistants SET claimed_by = 1 WHERE name = 'triage'",
        [],
    )
    .unwrap();
    assert!(!a::take(&db, "triage").unwrap());
    db.execute(
        "UPDATE assistants SET claimed_by = 2147483646 WHERE name = 'triage'",
        [],
    )
    .unwrap();
    assert!(a::take(&db, "triage").unwrap());
    assert_eq!(
        a::row(&db, "triage").unwrap().claimed_by,
        Some(i64::from(std::process::id()))
    );
    a::release(&db, "triage").unwrap();
    assert_eq!(a::row(&db, "triage").unwrap().claimed_by, None);
}

#[test]
fn only_a_due_and_enabled_assistant_is_due() {
    let h = Home::new("due");
    let db = h.db();
    h.registered(&db);
    assert_eq!(a::due(&db, None).unwrap().len(), 1);
    db.execute(
        "UPDATE assistants SET next_due_at = 4102444800 WHERE name = 'triage'",
        [],
    )
    .unwrap();
    assert!(a::due(&db, None).unwrap().is_empty());
    // --name asks for one whether or not it is due; disabled still means no.
    assert_eq!(a::due(&db, Some("triage")).unwrap().len(), 1);
    a::set_disabled(&db, "triage", true).unwrap();
    assert!(a::due(&db, Some("triage")).unwrap().is_empty());
}

#[test]
fn a_failure_backs_off_the_third_disables_and_a_success_clears_them() {
    let h = Home::new("backoff");
    let db = h.db();
    let found = h.registered(&db);
    for expected in [1, 2] {
        a::schedule_next(&db, &found, false).unwrap();
        let row = a::row(&db, "triage").unwrap();
        assert_eq!((row.failures, row.disabled), (expected, false));
        // Doubling per consecutive failure.
        let delay = row.next_due_at - a::now();
        assert!(
            (1800 * (1 << expected) - 2..=1800 * (1 << expected)).contains(&delay),
            "{delay}"
        );
    }
    a::schedule_next(&db, &found, true).unwrap();
    let row = a::row(&db, "triage").unwrap();
    assert_eq!(row.failures, 0);
    for _ in 0..3 {
        a::schedule_next(&db, &found, false).unwrap();
    }
    assert!(a::row(&db, "triage").unwrap().disabled);
}

/// `assistant disable` during a wakeup says stop; a wakeup that then finished well used to
/// answer by scheduling itself again.
#[test]
fn an_operator_disable_survives_a_wakeup_that_finishes() {
    let h = Home::new("opdisable");
    let db = h.db();
    let found = h.registered(&db);
    a::set_disabled(&db, "triage", true).unwrap();
    a::schedule_next(&db, &found, true).unwrap();
    assert!(a::row(&db, "triage").unwrap().disabled);
}

// --- one wakeup -----------------------------------------------------------------------------------

#[test]
fn a_wakeup_runs_the_run_command_records_it_and_keeps_its_task_and_report() {
    let h = Home::new("wake");
    let db = h.db();
    let found = h.registered(&db);
    a::tell(&db, "triage", "look at PR 12").unwrap();
    let ran = Ran::new(0);
    assert_eq!(wake(&db, &found, &ran), 0);
    let argv = &ran.calls()[0];
    assert_eq!(argv[0], "run");
    assert_eq!(after(argv, "--mode"), "sealed");
    assert_eq!(after(argv, "--agent"), "pi");
    assert_eq!(after(argv, "--model"), "local-model");
    assert_eq!(after(argv, "-w"), found.workspace().display().to_string());
    let task = std::fs::read_to_string(after(argv, "--task-file")).unwrap();
    assert!(
        task.contains("Triage the inbox.") && task.contains("look at PR 12"),
        "{task}"
    );
    assert!(task.contains("Message received 20"), "{task}");
    let rows = run_rows(&db);
    assert_eq!((rows.len(), rows[0].exit_code), (1, Some(0)));
    assert_eq!(rows[0].stats.as_deref(), Some("900 in / 40 out"));
    assert_eq!(rows[0].error, None);
    let outbox = a::outbox(&db, None, false, false).unwrap();
    assert_eq!(outbox[0].body, "the report");
    assert_eq!(outbox[0].state(), "approved");
    assert!(a::pending(&db, "triage").unwrap().is_empty());
    // Read back, then deleted: nothing accumulates in the state directory.
    assert!(!std::fs::read_dir(util::state_dir()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("wakeup-")
    }));
}

/// It has not answered them; consuming on failure loses the request.
#[test]
fn a_failed_wakeup_leaves_its_messages_for_the_next_one() {
    let h = Home::new("failed");
    let db = h.db();
    let found = h.registered(&db);
    a::tell(&db, "triage", "look at PR 12").unwrap();
    assert_eq!(wake(&db, &found, &Ran::new(1)), 1);
    assert_eq!(a::pending(&db, "triage").unwrap().len(), 1);
    assert_eq!(a::row(&db, "triage").unwrap().failures, 1);
    assert_eq!(
        a::outbox(&db, None, false, false).unwrap()[0].body,
        "(no report, exit 1)"
    );
}

#[test]
fn a_wakeup_records_why_it_failed() {
    let h = Home::new("why");
    let db = h.db();
    let found = h.registered(&db);
    let why = "agent exceeded --timeout 120s";
    let code = a::wake(&db, &found, None, &|argv: &[String]| {
        std::fs::write(
            after(argv, "--stats-file"),
            format!("{{\"exit\": 124, \"stats\": \"\", \"error\": \"{why}\"}}"),
        )
        .unwrap();
        124
    })
    .unwrap();
    assert_eq!(code, 124);
    let rows = run_rows(&db);
    assert_eq!(rows[0].error.as_deref(), Some(why));
    assert_eq!(rows[0].stats, None);
    assert_eq!(
        a::outbox(&db, None, false, false).unwrap()[0].body,
        format!("(no report, exit 124: {why})")
    );
}

#[test]
fn a_wakeup_with_nothing_to_do_says_how_to_give_it_something() {
    let h = Home::new("nothing");
    std::fs::write(
        h.dir.join("assistant.toml"),
        CONFIG.replace("brief = \"brief.md\"", ""),
    )
    .unwrap();
    let db = h.db();
    let found = h.registered(&db);
    let refused = a::wake(&db, &found, None, &|_: &[String]| 0).unwrap_err();
    assert!(refused.message.contains("sanduk tell"));
}

fn gate(h: &Home, script: &str, mode: u32) -> Assistant {
    use std::os::unix::fs::PermissionsExt;
    let path = h.dir.join("gate.sh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    h.config("gate = \"gate.sh\"");
    h.load()
}

#[test]
fn a_gate_decides_before_any_model_is_paid() {
    let h = Home::new("gate");
    let db = h.db();
    let ran = Ran::new(0);
    let found = gate(&h, "#!/bin/sh\necho nothing new\nexit 1\n", 0o755);
    a::register(&db, &found).unwrap();
    assert_eq!(wake(&db, &found, &ran), 0);
    assert!(ran.calls().is_empty());
    // Skipped is not failed: the schedule moves on by one interval.
    assert_eq!(a::row(&db, "triage").unwrap().failures, 0);
    let found = gate(&h, "#!/bin/sh\nexit 0\n", 0o755);
    wake(&db, &found, &ran);
    assert_eq!(ran.calls().len(), 1);
    let found = gate(&h, "#!/bin/sh\nexit 0\n", 0o644);
    assert!(
        a::wake(&db, &found, None, &|_: &[String]| 0)
            .unwrap_err()
            .message
            .contains("not executable")
    );
}

/// `run` refuses to write the report through a symlink; reading one here would put back the file
/// that refusal withheld.
#[test]
fn the_outbox_does_not_read_through_a_report_symlink() {
    let h = Home::new("symlink");
    let db = h.db();
    let found = h.registered(&db);
    let secret = h.root.join("host-only.txt");
    std::fs::write(&secret, "a key, say").unwrap();
    a::wake(&db, &found, None, &|argv: &[String]| {
        std::os::unix::fs::symlink(&secret, after(argv, "-o")).unwrap();
        0
    })
    .unwrap();
    let body = &a::outbox(&db, None, false, false).unwrap()[0].body;
    assert!(
        !body.contains("a key, say") && body.starts_with("(no report"),
        "{body}"
    );
}

/// Ctrl-C or `systemctl stop` should not disable an assistant that works. A container killed by a
/// signal inside it (137, 139) is still the assistant's own failure.
#[test]
fn only_a_signal_to_this_process_is_not_a_failure() {
    let h = Home::new("interrupted");
    let db = h.db();
    let found = h.registered(&db);
    for code in [130, 143, 129] {
        a::wake(&db, &found, None, &|_: &[String]| code).unwrap();
        let row = a::row(&db, "triage").unwrap();
        assert_eq!((row.failures, row.disabled), (0, false), "{code}");
    }
    a::tell(&db, "triage", "look at PR 12").unwrap();
    a::wake(&db, &found, None, &|_: &[String]| 130).unwrap();
    assert_eq!(
        a::pending(&db, "triage").unwrap().len(),
        1,
        "an interrupted wakeup did not answer it"
    );
    a::wake(&db, &found, None, &|_: &[String]| 137).unwrap();
    assert_eq!(a::row(&db, "triage").unwrap().failures, 1);
}

// --- tick -----------------------------------------------------------------------------------------

#[test]
fn tick_wakes_what_is_due_once() {
    let h = Home::new("tick");
    let db = h.db();
    h.registered(&db);
    let ran = Ran::new(0);
    let run = |argv: &[String]| ran.run(argv);
    assert_eq!(a::tick(&db, None, None, &run).unwrap(), 0);
    assert_eq!(a::tick(&db, None, None, &run).unwrap(), 0);
    assert_eq!(ran.calls().len(), 1, "the interval has not passed");
}

#[test]
fn tick_leaves_an_assistant_another_process_is_running() {
    let h = Home::new("tickclaimed");
    let db = h.db();
    h.registered(&db);
    db.execute(
        "UPDATE assistants SET claimed_by = 1 WHERE name = 'triage'",
        [],
    )
    .unwrap();
    let ran = Ran::new(0);
    assert_eq!(
        a::tick(&db, None, None, &|argv: &[String]| ran.run(argv)).unwrap(),
        0
    );
    assert!(ran.calls().is_empty());
}

#[test]
fn tick_releases_the_claim_when_a_wakeup_fails() {
    let h = Home::new("tickfail");
    std::fs::write(
        h.dir.join("assistant.toml"),
        CONFIG.replace("brief = \"brief.md\"", ""),
    )
    .unwrap();
    let db = h.db();
    h.registered(&db);
    assert!(a::tick(&db, None, None, &|_: &[String]| 0).is_err());
    assert_eq!(a::row(&db, "triage").unwrap().claimed_by, None);
}

/// The signal was meant for the process, not for the one wakeup that got it.
#[test]
fn an_interrupted_wakeup_skips_the_rest_of_the_pass() {
    let h = Home::new("tickinterrupt");
    let db = h.db();
    h.registered(&db);
    a::register(&db, &a::load(&h.sibling("second")).unwrap()).unwrap();
    let woken = RefCell::new(0);
    let code = a::tick(&db, None, None, &|_: &[String]| {
        *woken.borrow_mut() += 1;
        143
    })
    .unwrap();
    assert_eq!((code, *woken.borrow()), (143, 1));
}

/// A directory moved since it was registered is one assistant's problem.
#[test]
fn an_assistant_that_will_not_load_leaves_the_rest_of_the_pass() {
    let h = Home::new("tickgone");
    let db = h.db();
    let gone = h.sibling("vanished");
    a::register(&db, &a::load(&gone).unwrap()).unwrap();
    h.registered(&db);
    std::fs::remove_file(gone.join("assistant.toml")).unwrap();
    let ran = Ran::new(0);
    assert_eq!(
        a::tick(&db, None, None, &|argv: &[String]| ran.run(argv)).unwrap(),
        2
    );
    assert_eq!(ran.calls().len(), 1);
}

/// One wakeup.json was shared: two processes raced, and one unlinked the file the other read.
#[test]
fn each_wakeup_has_its_own_stats_file() {
    let h = Home::new("statsfiles");
    let db = h.db();
    h.registered(&db);
    a::register(&db, &a::load(&h.sibling("review")).unwrap()).unwrap();
    let ran = Ran::new(0);
    a::tick(&db, None, None, &|argv: &[String]| ran.run(argv)).unwrap();
    let files: Vec<String> = ran
        .calls()
        .iter()
        .map(|argv| after(argv, "--stats-file").to_string())
        .collect();
    assert_eq!(files.len(), 2);
    assert_ne!(files[0], files[1]);
    assert!(run_rows(&db).iter().all(|r| r.stats.is_some()));
}

// --- the outbox -----------------------------------------------------------------------------------

#[test]
fn delivery_pipes_each_entry_marks_it_and_stops_at_a_failure() {
    let h = Home::new("deliver");
    let db = h.db();
    let found = h.registered(&db);
    wake(&db, &found, &Ran::new(0));
    assert_eq!(a::deliver(&db, "exit 3", None).unwrap(), 0);
    assert_eq!(a::outbox(&db, None, true, false).unwrap().len(), 1);
    let sink = h.root.join("delivered.txt");
    let command = format!(
        "cat >> {} && test \"$SANDUK_ASSISTANT\" = triage",
        sink.display()
    );
    assert_eq!(a::deliver(&db, &command, None).unwrap(), 1);
    assert_eq!(std::fs::read_to_string(&sink).unwrap(), "the report");
    assert!(a::outbox(&db, None, true, false).unwrap().is_empty());
}

/// Delivery is the only thing that leaves the box, so it is what a person gets to hold.
#[test]
fn with_approval_a_result_waits_and_a_rejection_is_final() {
    let h = Home::new("approval");
    h.config("approval = true");
    let db = h.db();
    let found = h.registered(&db);
    wake(&db, &found, &Ran::new(0));
    wake(&db, &found, &Ran::new(0));
    let entries = a::outbox(&db, None, false, false).unwrap();
    assert!(entries.iter().all(|e| e.state() == "pending"));
    let sink = h.root.join("sent.txt");
    let command = format!("cat >> {}", sink.display());
    assert_eq!(a::deliver(&db, &command, None).unwrap(), 0);
    assert!(!sink.exists());
    assert_eq!(a::decide(&db, &[entries[0].id], true).unwrap(), 1);
    assert_eq!(
        a::decide(&db, &[entries[0].id], true).unwrap(),
        0,
        "decided already"
    );
    assert_eq!(a::decide(&db, &[entries[1].id], false).unwrap(), 1);
    // A rejection is not undone by approving afterwards.
    a::decide(&db, &[entries[1].id], true).unwrap();
    assert_eq!(
        a::outbox(&db, None, false, false).unwrap()[1].state(),
        "rejected"
    );
    assert_eq!(a::deliver(&db, &command, None).unwrap(), 1);
    assert_eq!(std::fs::read_to_string(&sink).unwrap(), "the report");
}

#[test]
fn the_summary_is_the_row_and_its_queue_depths_as_sorted_json() {
    let h = Home::new("summary");
    let db = h.db();
    h.registered(&db);
    a::tell(&db, "triage", "hi").unwrap();
    let summary: serde_json::Value =
        serde_json::from_str(&a::summary(&db, "triage").unwrap()).unwrap();
    assert_eq!(summary["pending"], 1);
    assert_eq!(summary["disabled"], 0);
    assert_eq!(summary["claimed_by"], serde_json::Value::Null);
    let keys: Vec<&String> = summary.as_object().unwrap().keys().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

// --- the database ---------------------------------------------------------------------------------

#[test]
fn the_database_lives_under_the_state_directory() {
    let h = Home::new("dbpath");
    assert_eq!(a::db_path(), h.root.join("state").join("assistants.db"));
    assert_eq!(a::db_path().parent().unwrap(), util::state_dir());
}

/// Written by Python sanduk before the columns existed: the history survives, and rows from
/// before approvals stay deliverable.
#[test]
fn an_old_database_gains_its_columns_and_keeps_its_rows() {
    let h = Home::new("migrate");
    {
        let old = h.db();
        old.execute_batch(
            "DROP TABLE runs; DROP TABLE outbox;
             CREATE TABLE runs (id INTEGER PRIMARY KEY, name TEXT NOT NULL, started_at INTEGER NOT NULL,
               ended_at INTEGER, exit_code INTEGER, report_path TEXT);
             INSERT INTO runs (name, started_at) VALUES ('triage', 1);
             CREATE TABLE outbox (id INTEGER PRIMARY KEY, name TEXT NOT NULL, run_id INTEGER NOT NULL,
               created_at INTEGER NOT NULL, body TEXT NOT NULL, delivered_at INTEGER);
             INSERT INTO outbox (name, run_id, created_at, body) VALUES ('t', 1, 7, 'x');",
        )
        .unwrap();
    }
    let db = h.db();
    assert_eq!(run_rows(&db)[0].name, "triage");
    let entry = &a::outbox(&db, None, false, false).unwrap()[0];
    assert_eq!((entry.state(), entry.approved_at), ("approved", Some(7)));
}

// --- serve ----------------------------------------------------------------------------------------

/// `serve` installs process-wide handlers, so its tests take turns.
static SERVING: Mutex<()> = Mutex::new(());

#[test]
fn the_nap_is_the_time_until_the_next_wakeup_capped_by_the_interval() {
    let h = Home::new("nap");
    let db = h.db();
    assert_eq!(a::nap(&db, 30).unwrap(), 30, "nothing registered");
    h.registered(&db);
    assert_eq!(a::nap(&db, 60).unwrap(), 1);
    db.execute(
        "UPDATE assistants SET next_due_at = ?1 WHERE name = 'triage'",
        [a::now() + 10],
    )
    .unwrap();
    assert!((9..=10).contains(&a::nap(&db, 60).unwrap()));
    db.execute(
        "UPDATE assistants SET next_due_at = ?1 WHERE name = 'triage'",
        [a::now() + 9999],
    )
    .unwrap();
    assert_eq!(a::nap(&db, 60).unwrap(), 60);
    a::set_disabled(&db, "triage", true).unwrap();
    assert_eq!(a::nap(&db, 45).unwrap(), 45);
}

/// SIGTERM between wakeups: the pass in flight finishes, then the loop stops, and the handlers it
/// found are back.
#[test]
fn serve_ticks_until_a_signal_says_stop() {
    let _turn = SERVING.lock().unwrap_or_else(|e| e.into_inner());
    let h = Home::new("serve");
    let db = h.db();
    h.registered(&db);
    // SAFETY: querying the current handler by installing and restoring it.
    let before = unsafe {
        let h = libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, h);
        h
    };
    let passes = RefCell::new(0);
    let code = a::serve(&db, 60, None, &|_: &[String]| {
        *passes.borrow_mut() += 1;
        // SAFETY: signalling this process, whose handler only sets a flag.
        unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
        0
    })
    .unwrap();
    assert_eq!((code, *passes.borrow()), (0, 1));
    // SAFETY: as above.
    let after = unsafe {
        let h = libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, h);
        h
    };
    assert_eq!(before, after);
}

/// `run` holds its own handlers while it holds a container, so a signal during a wakeup reaches
/// the loop as an exit code.
#[test]
fn serve_stops_when_a_wakeup_was_torn_down() {
    let _turn = SERVING.lock().unwrap_or_else(|e| e.into_inner());
    let h = Home::new("servetorn");
    let db = h.db();
    h.registered(&db);
    let passes = RefCell::new(0);
    a::serve(&db, 60, None, &|_: &[String]| {
        *passes.borrow_mut() += 1;
        143
    })
    .unwrap();
    assert_eq!(*passes.borrow(), 1);
}

/// A failed pass loses the pass, not the daemon: `serve` is what a service unit supervises.
#[test]
fn a_failed_pass_does_not_stop_serve() {
    let _turn = SERVING.lock().unwrap_or_else(|e| e.into_inner());
    let h = Home::new("serveflaky");
    std::fs::write(
        h.dir.join("assistant.toml"),
        CONFIG.replace("brief = \"brief.md\"", ""),
    )
    .unwrap();
    let db = h.db();
    h.registered(&db);
    let passes = RefCell::new(0);
    std::thread::scope(|s| {
        // The first pass fails (nothing to do); fix it and stop the loop from outside.
        let dir = h.dir.clone();
        s.spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            std::fs::write(dir.join("assistant.toml"), CONFIG).unwrap();
            // SAFETY: signalling this process, whose handler only sets a flag.
            unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
        });
        a::serve(&db, 1, None, &|_: &[String]| {
            *passes.borrow_mut() += 1;
            0
        })
        .unwrap();
    });
    assert!(*passes.borrow() <= 1);
}
