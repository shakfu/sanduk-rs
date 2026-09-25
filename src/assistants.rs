//! Assistants: an identity, a schedule and a mailbox around `sanduk run`.
//!
//! `run` is one container and no memory of it. An assistant is a directory whose `assistant.toml`
//! says how to run, a `workspace/` the agent keeps its memory in, and a SQLite file holding what
//! has to outlive a run: when the next wakeup is due, which process owns one now, what came in and
//! what went out.
//!
//! Nothing here starts a container. A wakeup composes a task and calls the `run` command, so an
//! assistant can do nothing a typed `sanduk run` cannot. The schema is Python sanduk's: either
//! binary opens a database the other wrote.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::agent::DEFAULT_AGENT;
use crate::catalog::is_path;
use crate::error::{Error, Result};
use crate::providers::DEFAULT_PROVIDER;
use crate::runs::owner_alive;
use crate::util::{note, read_unfollowed, seconds, state_dir};

pub const CONFIG_NAME: &str = "assistant.toml";
const DEFAULT_EVERY: &str = "30m";
/// A failing assistant backs off per consecutive failure, to this ceiling.
const MAX_BACKOFF: i64 = 24 * 3600;
const MODES: [&str; 3] = ["open", "key-safe", "sealed"];

/// What `run` exits with when a signal reached this process: 128 + SIGHUP, SIGINT or SIGTERM. Not
/// every code above 128: both engines exit with the container's status, so an agent the kernel
/// OOM-killed arrives as 137. That is the assistant's own failure and still counts as one.
pub const TEARDOWN_EXITS: [i32; 3] = [128 + libc::SIGHUP, 128 + libc::SIGINT, 128 + libc::SIGTERM];

/// Whether a signal to this process ended the wakeup, rather than the agent.
pub fn interrupted(code: i32) -> bool {
    TEARDOWN_EXITS.contains(&code)
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS assistants (
  name TEXT PRIMARY KEY,
  dir TEXT NOT NULL,
  next_due_at INTEGER NOT NULL,
  last_run_at INTEGER,
  failures INTEGER NOT NULL DEFAULT 0,
  disabled INTEGER NOT NULL DEFAULT 0,
  claimed_by INTEGER,
  claimed_at INTEGER
);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  exit_code INTEGER,
  report_path TEXT,
  stats TEXT,
  error TEXT
);
CREATE TABLE IF NOT EXISTS inbox (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  body TEXT NOT NULL,
  consumed_at INTEGER
);
CREATE TABLE IF NOT EXISTS outbox (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  run_id INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  body TEXT NOT NULL,
  delivered_at INTEGER,
  approved_at INTEGER,
  rejected_at INTEGER
);
";

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::new(format!("database: {e}"))
    }
}

/// One assistant's configuration, as read from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Assistant {
    pub name: String,
    pub dir: PathBuf,
    pub agent: Option<String>,
    pub recipe: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub runtime: Option<String>,
    pub mode: String,
    pub timeout: u64,
    pub every: u64,
    pub brief: Option<PathBuf>,
    pub gate: Option<PathBuf>,
    pub max_failures: i64,
    pub approval: bool,
    pub mounts: Vec<String>,
    pub args: Vec<String>,
}

impl Assistant {
    pub fn workspace(&self) -> PathBuf {
        self.dir.join("workspace")
    }

    pub fn reports(&self) -> PathBuf {
        self.dir.join("reports")
    }
}

const KEYS: [&str; 16] = [
    "name",
    "agent",
    "recipe",
    "provider",
    "model",
    "runtime",
    "mode",
    "proxy",
    "timeout",
    "every",
    "brief",
    "gate",
    "max_failures",
    "approval",
    "mounts",
    "args",
];

/// Reads `<directory>/assistant.toml`.
pub fn load(directory: &Path) -> Result<Assistant> {
    let directory = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    let path = directory.join(CONFIG_NAME);
    if !path.is_file() {
        return Err(Error::new(format!(
            "no {CONFIG_NAME} in {}",
            directory.display()
        )));
    }
    let bad = |msg: String| Error::new(format!("{}: {msg}", path.display()));
    let conf: toml::Table = std::fs::read_to_string(&path)?
        .parse()
        .map_err(|e: toml::de::Error| bad(e.to_string()))?;
    let mut unknown: Vec<&str> = conf
        .keys()
        .map(String::as_str)
        .filter(|k| !KEYS.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(bad(format!("unknown keys {}", unknown.join(", "))));
    }
    let text = |key: &str| -> Result<Option<String>> {
        match conf.get(key) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(s.clone())),
            Some(toml::Value::Integer(n)) => Ok(Some(n.to_string())),
            Some(_) => Err(bad(format!("{key} must be a string"))),
        }
    };
    let list = |key: &str| -> Result<Vec<String>> {
        match conf.get(key) {
            None => Ok(Vec::new()),
            Some(toml::Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .ok_or_else(|| bad(format!("{key} must be a list of strings")))
                })
                .collect(),
            Some(_) => Err(bad(format!("{key} must be a list of strings"))),
        }
    };
    let under = |value: Option<String>| value.map(|v| directory.join(v));

    let mut recipe = text("recipe")?;
    if let Some(spec) = &recipe
        && is_path(spec)
        && !Path::new(spec).is_absolute()
        && !spec.starts_with("~/")
    {
        // Relative to the config file, like mounts: tick runs from anywhere.
        let joined = directory.join(spec);
        recipe = Some(
            std::fs::canonicalize(&joined)
                .unwrap_or(joined)
                .display()
                .to_string(),
        );
    }
    let agent = text("agent")?;
    let max_failures = match conf.get("max_failures") {
        None => 3,
        Some(toml::Value::Integer(n)) => *n,
        Some(_) => return Err(bad("max_failures must be a number".into())),
    };
    let approval = match conf.get("approval") {
        None => false,
        Some(toml::Value::Boolean(b)) => *b,
        Some(_) => return Err(bad("approval must be true or false".into())),
    };
    let assistant = Assistant {
        name: text("name")?.unwrap_or_else(|| {
            directory
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        }),
        // A recipe names its agent; without one, the default agent's recipe.
        agent: agent.or_else(|| recipe.is_none().then(|| DEFAULT_AGENT.to_string())),
        recipe,
        provider: text("provider")?.unwrap_or_else(|| DEFAULT_PROVIDER.into()),
        model: text("model")?,
        runtime: text("runtime")?,
        mode: read_mode(&conf, &path)?,
        timeout: seconds(&text("timeout")?.unwrap_or_else(|| "900".into()))?,
        every: seconds(&text("every")?.unwrap_or_else(|| DEFAULT_EVERY.into()))?,
        brief: under(text("brief")?),
        gate: under(text("gate")?),
        max_failures,
        approval,
        mounts: list("mounts")?
            .iter()
            .map(|m| anchor(&directory, m))
            .collect(),
        args: list("args")?,
        dir: directory.clone(),
    };
    if assistant.mode == "open" {
        // Unattended runs are the ones nobody watches; without the relay the container holds the
        // key and can reach anything.
        note(&format!(
            "{}: mode = open, so the container holds the key",
            assistant.name
        ));
    }
    if let Some(brief) = &assistant.brief
        && !brief.is_file()
    {
        return Err(bad(format!("brief {} does not exist", brief.display())));
    }
    Ok(assistant)
}

/// `mode`, or the `proxy` boolean it replaced. Sealed by default: an unattended run is the one
/// nobody is watching.
fn read_mode(conf: &toml::Table, path: &Path) -> Result<String> {
    match conf.get("mode") {
        None => match conf.get("proxy").and_then(toml::Value::as_bool) {
            Some(proxy) => {
                let mode = if proxy { "sealed" } else { "open" };
                note(&format!(
                    "{}: `proxy` is now `mode`; read as mode = \"{mode}\"",
                    path.display()
                ));
                Ok(mode.into())
            }
            None => Ok("sealed".into()),
        },
        Some(toml::Value::String(mode)) if MODES.contains(&mode.as_str()) => Ok(mode.clone()),
        Some(other) => Err(Error::new(format!(
            "{}: mode {other} is not one of {}",
            path.display(),
            MODES.join(", ")
        ))),
    }
}

/// Makes a `HOST:DEST[:ro]` mount's host path absolute, from the config's own directory. `run`
/// validates the rest: one place decides what a mount may be.
fn anchor(directory: &Path, spec: &str) -> String {
    let mut parts: Vec<String> = spec.split(':').map(String::from).collect();
    if (2..=3).contains(&parts.len()) && !parts[0].is_empty() && !parts[0].starts_with('/') {
        let joined = directory.join(&parts[0]);
        parts[0] = std::fs::canonicalize(&joined)
            .unwrap_or(joined)
            .display()
            .to_string();
    }
    parts.join(":")
}

// --- state ------------------------------------------------------------------------------------------

pub fn db_path() -> PathBuf {
    state_dir().join("assistants.db")
}

pub fn connect() -> Result<Connection> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Connection::open(&path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.execute_batch(SCHEMA)?;
    migrate(&db)?;
    Ok(db)
}

fn columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(names)
}

/// Columns added after a database was first written. `CREATE TABLE IF NOT EXISTS` does not add
/// them, and an assistant's history is not worth dropping to gain a column.
fn migrate(db: &Connection) -> Result<()> {
    let have = columns(db, "runs")?;
    for column in ["stats", "error"] {
        if !have.iter().any(|c| c == column) {
            db.execute(&format!("ALTER TABLE runs ADD COLUMN {column} TEXT"), [])?;
        }
    }
    let held = columns(db, "outbox")?;
    for column in ["approved_at", "rejected_at"] {
        if !held.iter().any(|c| c == column) {
            db.execute(
                &format!("ALTER TABLE outbox ADD COLUMN {column} INTEGER"),
                [],
            )?;
            // Rows written before approvals existed were delivered on sight, which is what an
            // assistant without `approval` still does.
            if column == "approved_at" {
                db.execute("UPDATE outbox SET approved_at = created_at", [])?;
            }
        }
    }
    Ok(())
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// One row of `assistants`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub dir: String,
    pub next_due_at: i64,
    pub last_run_at: Option<i64>,
    pub failures: i64,
    pub disabled: bool,
    pub claimed_by: Option<i64>,
    pub claimed_at: Option<i64>,
}

impl Row {
    fn from(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Row {
            name: r.get("name")?,
            dir: r.get("dir")?,
            next_due_at: r.get("next_due_at")?,
            last_run_at: r.get("last_run_at")?,
            failures: r.get("failures")?,
            disabled: r.get::<_, i64>("disabled")? != 0,
            claimed_by: r.get("claimed_by")?,
            claimed_at: r.get("claimed_at")?,
        })
    }

    fn to_json(&self) -> BTreeMap<&'static str, Value> {
        BTreeMap::from([
            ("name", json!(self.name)),
            ("dir", json!(self.dir)),
            ("next_due_at", json!(self.next_due_at)),
            ("last_run_at", json!(self.last_run_at)),
            ("failures", json!(self.failures)),
            ("disabled", json!(i64::from(self.disabled))),
            ("claimed_by", json!(self.claimed_by)),
            ("claimed_at", json!(self.claimed_at)),
        ])
    }
}

/// Adds an assistant, or points an existing name at a new directory. The schedule survives a
/// re-add: registering again is not a reason to run.
pub fn register(db: &Connection, assistant: &Assistant) -> Result<()> {
    db.execute(
        "INSERT INTO assistants (name, dir, next_due_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(name) DO UPDATE SET dir = excluded.dir",
        params![assistant.name, assistant.dir.display().to_string(), now()],
    )?;
    Ok(())
}

pub fn rows(db: &Connection) -> Result<Vec<Row>> {
    let mut stmt = db.prepare("SELECT * FROM assistants ORDER BY name")?;
    let found = stmt
        .query_map([], Row::from)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(found)
}

pub fn row(db: &Connection, name: &str) -> Result<Row> {
    db.query_row(
        "SELECT * FROM assistants WHERE name = ?1",
        [name],
        Row::from,
    )
    .optional()?
    .ok_or_else(|| {
        Error::new(format!(
            "no assistant named {name:?}. `sanduk assistant list`"
        ))
    })
}

pub fn set_disabled(db: &Connection, name: &str, disabled: bool) -> Result<()> {
    row(db, name)?;
    db.execute(
        "UPDATE assistants SET disabled = ?1, failures = 0 WHERE name = ?2",
        params![i64::from(disabled), name],
    )?;
    Ok(())
}

pub fn due(db: &Connection, name: Option<&str>) -> Result<Vec<Row>> {
    if let Some(name) = name {
        let found = row(db, name)?;
        return Ok(if found.disabled {
            Vec::new()
        } else {
            vec![found]
        });
    }
    let mut stmt = db.prepare(
        "SELECT * FROM assistants WHERE disabled = 0 AND next_due_at <= ?1 ORDER BY next_due_at",
    )?;
    let found = stmt
        .query_map([now()], Row::from)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(found)
}

/// Claims an assistant for this process; `false` means someone else has it. A claim whose pid is
/// gone is stale and may be taken; a live one is left alone.
pub fn take(db: &Connection, name: &str) -> Result<bool> {
    db.execute_batch("BEGIN IMMEDIATE")?;
    let claimed = (|| -> Result<bool> {
        let me = i64::from(std::process::id());
        if let Some(held) = row(db, name)?.claimed_by
            && held != me
            && owner_alive(held)
        {
            return Ok(false);
        }
        db.execute(
            "UPDATE assistants SET claimed_by = ?1, claimed_at = ?2 WHERE name = ?3",
            params![me, now(), name],
        )?;
        Ok(true)
    })();
    match claimed {
        Ok(true) => db.execute_batch("COMMIT")?,
        _ => db.execute_batch("ROLLBACK")?,
    }
    claimed
}

pub fn release(db: &Connection, name: &str) -> Result<()> {
    db.execute(
        "UPDATE assistants SET claimed_by = NULL, claimed_at = NULL WHERE name = ?1",
        [name],
    )?;
    Ok(())
}

pub fn tell(db: &Connection, name: &str, body: &str) -> Result<i64> {
    row(db, name)?;
    db.execute(
        "INSERT INTO inbox (name, created_at, body) VALUES (?1, ?2, ?3)",
        params![name, now(), body],
    )?;
    Ok(db.last_insert_rowid())
}

/// A message waiting for the next wakeup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    pub created_at: i64,
    pub body: String,
}

pub fn pending(db: &Connection, name: &str) -> Result<Vec<Message>> {
    let mut stmt = db.prepare("SELECT id, created_at, body FROM inbox WHERE name = ?1 AND consumed_at IS NULL ORDER BY id")?;
    let found = stmt
        .query_map([name], |r| {
            Ok(Message {
                id: r.get(0)?,
                created_at: r.get(1)?,
                body: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(found)
}

pub fn consume(db: &Connection, ids: &[i64]) -> Result<()> {
    for id in ids {
        db.execute(
            "UPDATE inbox SET consumed_at = ?1 WHERE id = ?2",
            params![now(), id],
        )?;
    }
    Ok(())
}

/// One entry of the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub name: String,
    pub run_id: i64,
    pub created_at: i64,
    pub body: String,
    pub delivered_at: Option<i64>,
    pub approved_at: Option<i64>,
    pub rejected_at: Option<i64>,
}

impl Entry {
    /// What the entry is waiting for, in one word.
    pub fn state(&self) -> &'static str {
        if self.rejected_at.is_some() {
            "rejected"
        } else if self.delivered_at.is_some() {
            "delivered"
        } else if self.approved_at.is_none() {
            "pending"
        } else {
            "approved"
        }
    }
}

pub fn outbox(
    db: &Connection,
    name: Option<&str>,
    undelivered: bool,
    pending: bool,
) -> Result<Vec<Entry>> {
    let mut sql = String::from("SELECT * FROM outbox WHERE 1 = 1");
    if name.is_some() {
        sql += " AND name = ?1";
    }
    if undelivered {
        sql += " AND delivered_at IS NULL AND rejected_at IS NULL";
    }
    if pending {
        sql += " AND approved_at IS NULL AND rejected_at IS NULL";
    }
    sql += " ORDER BY id";
    let mut stmt = db.prepare(&sql)?;
    let map = |r: &rusqlite::Row| {
        Ok(Entry {
            id: r.get("id")?,
            name: r.get("name")?,
            run_id: r.get("run_id")?,
            created_at: r.get("created_at")?,
            body: r.get("body")?,
            delivered_at: r.get("delivered_at")?,
            approved_at: r.get("approved_at")?,
            rejected_at: r.get("rejected_at")?,
        })
    };
    let found = match name {
        Some(name) => stmt
            .query_map([name], map)?
            .collect::<rusqlite::Result<_>>()?,
        None => stmt.query_map([], map)?.collect::<rusqlite::Result<_>>()?,
    };
    Ok(found)
}

/// Approves or rejects entries, returning how many changed. Neither undoes the other: a decision
/// recorded is a decision made.
pub fn decide(db: &Connection, ids: &[i64], approve: bool) -> Result<usize> {
    let column = if approve {
        "approved_at"
    } else {
        "rejected_at"
    };
    let sql = format!(
        "UPDATE outbox SET {column} = ?1 WHERE id = ?2 AND {column} IS NULL AND delivered_at IS NULL"
    );
    let mut changed = 0;
    for id in ids {
        changed += db.execute(&sql, params![now(), id])?;
    }
    Ok(changed)
}

fn delivered(db: &Connection, ids: &[i64]) -> Result<()> {
    for id in ids {
        db.execute(
            "UPDATE outbox SET delivered_at = ?1 WHERE id = ?2",
            params![now(), id],
        )?;
    }
    Ok(())
}

/// One recorded wakeup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wakeup {
    pub id: i64,
    pub name: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub report_path: Option<String>,
    pub stats: Option<String>,
    pub error: Option<String>,
}

pub fn history(db: &Connection, name: Option<&str>, limit: i64) -> Result<Vec<Wakeup>> {
    let sql = match name {
        Some(_) => "SELECT * FROM runs WHERE name = ?1 ORDER BY id DESC LIMIT ?2",
        None => "SELECT * FROM runs ORDER BY id DESC LIMIT ?1",
    };
    let mut stmt = db.prepare(sql)?;
    let map = |r: &rusqlite::Row| {
        Ok(Wakeup {
            id: r.get("id")?,
            name: r.get("name")?,
            started_at: r.get("started_at")?,
            ended_at: r.get("ended_at")?,
            exit_code: r.get("exit_code")?,
            report_path: r.get("report_path")?,
            stats: r.get("stats")?,
            error: r.get("error")?,
        })
    };
    let found = match name {
        Some(name) => stmt
            .query_map(params![name, limit], map)?
            .collect::<rusqlite::Result<_>>()?,
        None => stmt
            .query_map(params![limit], map)?
            .collect::<rusqlite::Result<_>>()?,
    };
    Ok(found)
}

// --- time ---------------------------------------------------------------------------------------

/// (year, month, day, hour, minute, second) of a Unix time, in UTC.
fn civil(ts: i64) -> (i64, i64, i64, i64, i64, i64) {
    let (days, secs) = (ts.div_euclid(86400), ts.rem_euclid(86400));
    // Howard Hinnant's days_from_civil, inverted.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, secs / 3600, secs % 3600 / 60, secs % 60)
}

/// `2026-09-25T06:00:00+00:00`, as Python's `datetime.isoformat` writes a UTC time.
pub fn iso(ts: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(ts);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

/// `20260925T060000Z`: a report's file name.
fn stamp() -> String {
    let (y, mo, d, h, mi, s) = civil(now());
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

// --- one wakeup ---------------------------------------------------------------------------------

/// Runs the gate script, if there is one; a non-zero exit skips the wakeup. The point is to decide
/// cheaply, before any model is paid to decide.
fn gate_allows(assistant: &Assistant) -> Result<bool> {
    let Some(gate) = &assistant.gate else {
        return Ok(true);
    };
    let executable = std::fs::metadata(gate).is_ok_and(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.is_file() && m.permissions().mode() & 0o111 != 0
    });
    if !executable {
        return Err(Error::new(format!(
            "gate {} is not executable",
            gate.display()
        )));
    }
    let result = std::process::Command::new(gate)
        .current_dir(&assistant.dir)
        .output()?;
    if !result.status.success() {
        let said = [&result.stdout, &result.stderr]
            .iter()
            .map(|b| String::from_utf8_lossy(b).trim().to_string())
            .find(|s| !s.is_empty())
            .unwrap_or_else(|| "gate said no".into());
        note(&format!(
            "{}: skipped, {}",
            assistant.name,
            said.chars().take(120).collect::<String>()
        ));
        return Ok(false);
    }
    Ok(true)
}

/// The standing brief, then anything queued since the last wakeup.
fn compose(assistant: &Assistant, items: &[Message]) -> Result<String> {
    let mut parts = Vec::new();
    if let Some(brief) = &assistant.brief {
        parts.push(std::fs::read_to_string(brief)?.trim().to_string());
    }
    for item in items {
        parts.push(format!(
            "Message received {}:\n{}",
            iso(item.created_at),
            item.body.trim()
        ));
    }
    if parts.is_empty() {
        return Err(Error::new(format!(
            "{}: nothing to do -- set `brief` in {CONFIG_NAME} or send one with `sanduk tell {} '...'`",
            assistant.name, assistant.name
        )));
    }
    Ok(parts.join("\n\n"))
}

/// The `sanduk run` command line one wakeup is.
pub fn run_argv(
    assistant: &Assistant,
    task_file: &Path,
    report: &Path,
    runtime: Option<&str>,
    stats_file: Option<&Path>,
) -> Vec<String> {
    let mut argv: Vec<String> = [
        "run",
        "--task-file",
        &task_file.display().to_string(),
        "-w",
        &assistant.workspace().display().to_string(),
        "-o",
        &report.display().to_string(),
        "--provider",
        &assistant.provider,
        "--timeout",
        &assistant.timeout.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut pair = |flag: &str, value: &str| argv.extend([flag.to_string(), value.to_string()]);
    if let Some(agent) = &assistant.agent {
        pair("--agent", agent);
    }
    if let Some(recipe) = &assistant.recipe {
        pair("--recipe", recipe);
    }
    if let Some(engine) = runtime.or(assistant.runtime.as_deref()) {
        pair("--runtime", engine);
    }
    if let Some(model) = &assistant.model {
        pair("--model", model);
    }
    pair("--mode", &assistant.mode);
    for mount in &assistant.mounts {
        pair("--mount", mount);
    }
    if let Some(stats) = stats_file {
        pair("--stats-file", &stats.display().to_string());
    }
    argv.extend(assistant.args.iter().cloned());
    argv
}

/// Runs the command `argv` names, `run` onwards, and returns its exit status.
pub type Runner<'a> = &'a dyn Fn(&[String]) -> i32;

/// The real `sanduk run`, in this process.
pub fn run_in_process(argv: &[String]) -> i32 {
    let mut full = vec![std::ffi::OsString::from("sanduk")];
    full.extend(argv.iter().map(std::ffi::OsString::from));
    crate::cli::main(full)
}

/// One wakeup: gate, compose, run, record, reschedule. Returns the run's exit code.
///
/// A wakeup that fails is scheduling news, not an error: it backs the assistant off and, past
/// `max_failures`, disables it.
pub fn wake(
    db: &Connection,
    assistant: &Assistant,
    runtime: Option<&str>,
    run: Runner,
) -> Result<i32> {
    if !gate_allows(assistant)? {
        schedule_next(db, assistant, true)?;
        return Ok(0);
    }
    let items = pending(db, &assistant.name)?;
    let task = compose(assistant, &items)?;
    std::fs::create_dir_all(assistant.workspace())?;
    std::fs::create_dir_all(assistant.reports())?;
    let when = stamp();
    let task_file = assistant.reports().join(format!("{when}.task.md"));
    let report = assistant.reports().join(format!("{when}.md"));
    std::fs::write(&task_file, task)?;

    db.execute(
        "INSERT INTO runs (name, started_at, report_path) VALUES (?1, ?2, ?3)",
        params![assistant.name, now(), report.display().to_string()],
    )?;
    let run_id = db.last_insert_rowid();
    // Keyed on the run id, which the shared database makes unique across processes.
    let stats_file = state_dir().join(format!("wakeup-{run_id}.json"));
    let _ = std::fs::remove_file(&stats_file);
    let code = run(&run_argv(
        assistant,
        &task_file,
        &report,
        runtime,
        Some(&stats_file),
    ));
    let (stats, error) = read_stats(&stats_file);

    db.execute(
        "UPDATE runs SET ended_at = ?1, exit_code = ?2, stats = ?3, error = ?4 WHERE id = ?5",
        params![now(), code, stats, error, run_id],
    )?;
    let why = error
        .as_ref()
        .map_or_else(String::new, |e| format!(": {e}"));
    // Not followed if it is a symlink: `run` refuses to write the report through one, and reading
    // one here would put back the file that refusal withheld.
    let body = read_unfollowed(&report).unwrap_or_else(|| format!("(no report, exit {code}{why})"));
    // Approval gates delivery, the only thing that leaves the box.
    let approved = (!assistant.approval).then(now);
    db.execute(
        "INSERT INTO outbox (name, run_id, created_at, body, approved_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![assistant.name, run_id, now(), body, approved],
    )?;
    if code == 0 {
        // Only a wakeup that finished consumes its messages: a failed one has not answered them.
        consume(db, &items.iter().map(|i| i.id).collect::<Vec<_>>())?;
    }
    schedule_next(db, assistant, code == 0 || interrupted(code))?;
    Ok(code)
}

/// The token line and the error `run` recorded, if it got far enough. The file is removed either
/// way: nothing accumulates in the state directory.
fn read_stats(path: &Path) -> (Option<String>, Option<String>) {
    let found: Option<Value> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let _ = std::fs::remove_file(path);
    let field = |key: &str| {
        found
            .as_ref()
            .and_then(|f| f.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    (field("stats"), field("error"))
}

pub fn schedule_next(db: &Connection, assistant: &Assistant, ok: bool) -> Result<()> {
    let current = row(db, &assistant.name)?;
    let every = assistant.every as i64;
    let failures = if ok { 0 } else { current.failures + 1 };
    let delay = if ok {
        every
    } else {
        every
            .saturating_mul(
                1i64.checked_shl(failures.clamp(0, 62) as u32)
                    .unwrap_or(i64::MAX),
            )
            .min(MAX_BACKOFF)
    };
    let mut disabled = !ok && failures >= assistant.max_failures;
    // An operator who ran `assistant disable` while this wakeup was in flight said stop; a wakeup
    // that then finished well would have answered by starting again.
    if current.disabled {
        disabled = true;
    } else if disabled {
        note(&format!(
            "{}: disabled after {failures} failures. Fix it, then `sanduk assistant enable {}`",
            assistant.name, assistant.name
        ));
    }
    db.execute(
        "UPDATE assistants SET next_due_at = ?1, last_run_at = ?2, failures = ?3, disabled = ?4 WHERE name = ?5",
        params![now() + delay, now(), failures, i64::from(disabled), assistant.name],
    )?;
    Ok(())
}

/// Every assistant that is due, once, in order, one at a time. Returns the worst exit code.
pub fn tick(
    db: &Connection,
    name: Option<&str>,
    runtime: Option<&str>,
    run: Runner,
) -> Result<i32> {
    let mut worst = 0;
    for found in due(db, name)? {
        let assistant = match load(Path::new(&found.dir)) {
            Ok(assistant) => assistant,
            Err(e) => {
                // One assistant's problem: raising here skipped every other one due in the pass.
                note(&format!(
                    "{}: cannot load {}: {}",
                    found.name, found.dir, e.message
                ));
                worst = worst.max(2);
                continue;
            }
        };
        if !take(db, &assistant.name)? {
            note(&format!(
                "{}: another process is running it",
                assistant.name
            ));
            continue;
        }
        note(&format!("{}: waking", assistant.name));
        let woke = wake(db, &assistant, runtime, run);
        release(db, &assistant.name)?;
        let code = woke?;
        worst = worst.max(code);
        if interrupted(code) {
            // The signal was meant for this process, not for that one wakeup.
            note("interrupted; the rest of this pass is skipped");
            return Ok(code);
        }
    }
    Ok(worst)
}

/// Seconds until the next wakeup is due, capped at `interval`, so an assistant registered while
/// the loop sleeps waits `interval` at worst.
pub fn nap(db: &Connection, interval: u64) -> Result<u64> {
    let soonest: Option<i64> = db.query_row(
        "SELECT MIN(next_due_at) FROM assistants WHERE disabled = 0",
        [],
        |r| r.get(0),
    )?;
    Ok(match soonest {
        None => interval,
        Some(due) => (due - now()).clamp(1, interval.max(1) as i64) as u64,
    })
}

static STOPPING: AtomicBool = AtomicBool::new(false);

extern "C" fn halt(_signum: libc::c_int) {
    STOPPING.store(true, Ordering::SeqCst);
}

/// `tick` on a loop, in the foreground, until SIGINT or SIGTERM says stop. The loop holds no
/// credential and no container between wakeups; it is a timer.
pub fn serve(db: &Connection, interval: u64, runtime: Option<&str>, run: Runner) -> Result<i32> {
    STOPPING.store(false, Ordering::SeqCst);
    let handler = halt as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only stores to an atomic.
    let previous: Vec<_> = [libc::SIGINT, libc::SIGTERM]
        .into_iter()
        .map(|s| (s, unsafe { libc::signal(s, handler) }))
        .collect();
    note(&format!(
        "serving; waking assistants as they come due, at most every {interval}s"
    ));
    let wait = |seconds: u64| {
        let until = std::time::Instant::now() + Duration::from_secs(seconds);
        while !STOPPING.load(Ordering::SeqCst) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    while !STOPPING.load(Ordering::SeqCst) {
        // A wakeup that a signal tore down took the signal with it: `run` holds its own handlers
        // while it holds a container, so this loop learns about it from the exit code.
        match tick(db, None, runtime, run) {
            Ok(code) if interrupted(code) => break,
            Ok(_) => {}
            Err(e) => {
                // A locked database, an engine that went away: the pass is lost, the daemon is
                // not. Waiting `interval` rather than the nap keeps a still-due assistant from
                // spinning.
                note(&format!(
                    "pass failed, retrying in {interval}s: {}",
                    e.message
                ));
                wait(interval);
                continue;
            }
        }
        if STOPPING.load(Ordering::SeqCst) {
            break;
        }
        wait(nap(db, interval)?);
    }
    for (signum, handler) in previous {
        // SAFETY: restoring the handler that was there before.
        unsafe { libc::signal(signum, handler) };
    }
    note("stopped");
    Ok(0)
}

/// Pipes every approved, undelivered entry to `command` on stdin. sanduk holds no messaging
/// credential and ships no platform adapter: where an entry goes is the operator's to say.
pub fn deliver(db: &Connection, command: &str, name: Option<&str>) -> Result<usize> {
    let waiting = outbox(db, name, false, true)?.len();
    if waiting > 0 {
        note(&format!(
            "{waiting} waiting for approval (`sanduk outbox --pending`)"
        ));
    }
    let mut sent = Vec::new();
    for entry in outbox(db, name, true, false)? {
        if entry.approved_at.is_none() {
            continue;
        }
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", command])
            .env("SANDUK_ASSISTANT", &entry.name)
            .env("SANDUK_RUN_ID", entry.run_id.to_string())
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(entry.body.as_bytes());
        }
        let status = child.wait()?;
        if !status.success() {
            note(&format!(
                "delivery failed ({}); {} delivered",
                status.code().unwrap_or(-1),
                sent.len()
            ));
            break;
        }
        sent.push(entry.id);
    }
    delivered(db, &sent)?;
    Ok(sent.len())
}

/// One assistant's state, as JSON: this is what a script reads.
pub fn summary(db: &Connection, name: &str) -> Result<String> {
    let mut found = row(db, name)?.to_json();
    found.insert("pending", json!(pending(db, name)?.len()));
    found.insert(
        "undelivered",
        json!(outbox(db, Some(name), true, false)?.len()),
    );
    found.insert(
        "awaiting_approval",
        json!(outbox(db, Some(name), false, true)?.len()),
    );
    Ok(serde_json::to_string_pretty(&found).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_times_are_written_as_python_writes_them() {
        assert_eq!(iso(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso(1_790_316_461), "2026-09-25T06:07:41+00:00");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00+00:00");
    }

    #[test]
    fn only_a_signal_to_this_process_is_an_interruption() {
        assert!(interrupted(130) && interrupted(143) && interrupted(129));
        assert!(!interrupted(137) && !interrupted(139) && !interrupted(1));
    }
}
