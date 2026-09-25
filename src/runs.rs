//! Ownership records for the containers a run holds.
//!
//! SIGKILL runs no teardown, so a killed run leaves its container alive with the run token still
//! inside it. Each run writes a record naming the containers it owns and the pid that owns them,
//! and every later run deletes the containers whose owner is gone.
//!
//! The record, not the container name, marks a container reapable. `--keep` releases it, so a
//! container the caller asked to inspect is never swept by the next run.
//!
//! A pid whose number has been reused reads as alive and its record is skipped. That misses an
//! orphan until the number is free again; the opposite error would delete a container another
//! process is still using.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use sanduk_container::{CONTAINER_PREFIX, Engine};

use crate::error::Result;
use crate::util::{note, state_dir};

pub fn runs_dir() -> PathBuf {
    state_dir().join("runs")
}

/// Whether the process that claimed a record is still there. A pid owned by another user answers
/// EPERM, which is still a live process and still not ours to reap.
pub fn owner_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 checks for the process without sending anything.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// One run's claim on the containers it started.
#[derive(Debug)]
pub struct Run {
    pub path: PathBuf,
    pub runtime: String,
    pub containers: Vec<String>,
}

impl Run {
    fn write(&self) -> Result<()> {
        let record = json!({"pid": std::process::id(), "runtime": self.runtime, "containers": self.containers});
        std::fs::write(&self.path, record.to_string())?;
        Ok(())
    }

    /// Claims a container started after the record was written.
    pub fn add(&mut self, name: &str) -> Result<()> {
        self.containers.push(name.to_string());
        self.write()
    }

    /// Gives up the claim: the containers are gone, or deliberately kept.
    pub fn release(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Records that this process owns `name` on `runtime`. A state directory that cannot be written
/// stops the run: a run with no record leaks its container silently when it is killed.
pub fn claim(runtime: &str, name: &str) -> Result<Run> {
    let dir = runs_dir();
    std::fs::create_dir_all(&dir)?;
    let run = Run {
        path: dir.join(format!("{name}.json")),
        runtime: runtime.to_string(),
        containers: vec![name.to_string()],
    };
    run.write()?;
    Ok(run)
}

/// Every readable record. An unreadable one is dropped: it names nothing anyone can act on.
fn records() -> Vec<(PathBuf, Value)> {
    let Ok(entries) = std::fs::read_dir(runs_dir()) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            match std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            {
                Some(record) if record.is_object() => Some((path, record)),
                _ => {
                    let _ = std::fs::remove_file(&path);
                    None
                }
            }
        })
        .collect()
}

fn pid_of(record: &Value) -> i64 {
    record.get("pid").and_then(Value::as_i64).unwrap_or(0)
}

fn containers_of(record: &Value) -> Vec<String> {
    record
        .get("containers")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Containers a running process still claims. `stop` and `clean` act on a name prefix, which
/// cannot tell a wakeup in flight from a leftover; the records can.
pub fn live_containers() -> BTreeSet<String> {
    records()
        .into_iter()
        .filter(|(_, r)| owner_alive(pid_of(r)))
        .flat_map(|(_, r)| containers_of(&r))
        .collect()
}

/// Deletes the containers of every run whose owner is gone, and returns them.
pub fn sweep() -> Vec<String> {
    sweep_with(|name| Ok(Engine::get(Some(name))?))
}

/// [`sweep`], with the engine for a record's runtime from `engine`.
///
/// A record naming an engine this host cannot reach is kept rather than dropped: the containers
/// it names are still there, and the engine may be back on the next run. So is one whose delete
/// failed: `inspect` on a container left behind shows its environment.
pub fn sweep_with(engine: impl Fn(&str) -> Result<Engine>) -> Vec<String> {
    let mut reaped = Vec::new();
    let mut engines: BTreeMap<String, Option<(Engine, BTreeSet<String>)>> = BTreeMap::new();
    for (path, record) in records() {
        if owner_alive(pid_of(&record)) {
            continue;
        }
        let runtime = record
            .get("runtime")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let found = engines.entry(runtime.clone()).or_insert_with(|| {
            let engine = engine(&runtime).ok()?;
            let listed = engine.list_containers(CONTAINER_PREFIX).ok()?;
            Some((engine, listed.into_iter().map(|c| c.name).collect()))
        });
        let Some((engine, listed)) = found else {
            continue;
        };
        let mut kept = false;
        for container in containers_of(&record) {
            if !listed.contains(&container) {
                continue;
            }
            note(&format!(
                "reaping {container}: the run that started it was killed"
            ));
            match engine.destroy(&container) {
                Ok(()) => reaped.push(container),
                Err(e) => {
                    note(&e.0);
                    kept = true;
                }
            }
        }
        if !kept {
            remove(&path);
        }
    }
    reaped
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}
