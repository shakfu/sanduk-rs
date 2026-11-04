//! Ownership records for the containers a run holds.
//!
//! SIGKILL runs no teardown, so a killed run leaves its container alive with the run token still
//! inside it. Each run writes a record naming the containers it owns and the pid that owns them.
//! The run's [`Reaper`] deletes them as soon as the run is gone, and every later run deletes the
//! containers whose owner is gone, for a reaper that was killed too.
//!
//! The record, not the container name, marks a container reapable. `--keep` releases it, so a
//! container the caller asked to inspect is never swept by the next run.
//!
//! A pid whose number has been reused reads as alive and its record is skipped. That misses an
//! orphan until the number is free again; the opposite error would delete a container another
//! process is still using.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use serde_json::{Value, json};

use crate::{Engine, Prefix, Result};

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
    /// The record's names, one per line, beside it: what the [`Reaper`]'s shell reads.
    pub fn names_path(&self) -> PathBuf {
        names_of(&self.path)
    }

    fn write(&self) -> Result<()> {
        let mut names = self.containers.join("\n");
        names.push('\n');
        std::fs::write(self.names_path(), names)?;
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
        remove(&self.path);
    }
}

/// Records that this process owns `name` on `runtime`. A state directory that cannot be written
/// stops the run: a run with no record leaks its container silently when it is killed.
pub fn claim(dir: &Path, runtime: &str, name: &str) -> Result<Run> {
    std::fs::create_dir_all(dir)?;
    let run = Run {
        path: dir.join(format!("{name}.json")),
        runtime: runtime.to_string(),
        containers: vec![name.to_string()],
    };
    run.write()?;
    Ok(run)
}

/// Every readable record. An unreadable one is dropped: it names nothing anyone can act on.
fn records(dir: &Path) -> Vec<(PathBuf, Value)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
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
                    remove(&path);
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
pub fn live_containers(dir: &Path) -> BTreeSet<String> {
    records(dir)
        .into_iter()
        .filter(|(_, r)| owner_alive(pid_of(r)))
        .flat_map(|(_, r)| containers_of(&r))
        .collect()
}

/// Deletes the containers of every run in `dir` whose owner is gone, and returns them.
pub fn sweep(dir: &Path, prefix: &Prefix, note: &dyn Fn(&str)) -> Vec<String> {
    sweep_with(dir, |name| Engine::get(Some(name), prefix.clone()), note)
}

/// [`sweep`], with the engine for a record's runtime from `engine`.
///
/// A record naming an engine this host cannot reach is kept rather than dropped: the containers
/// it names are still there, and the engine may be back on the next run. So is one whose delete
/// failed: `inspect` on a container left behind shows its environment.
pub fn sweep_with(
    dir: &Path,
    engine: impl Fn(&str) -> Result<Engine>,
    note: &dyn Fn(&str),
) -> Vec<String> {
    let mut reaped = Vec::new();
    let mut engines: BTreeMap<String, Option<(Engine, BTreeSet<String>)>> = BTreeMap::new();
    for (path, record) in records(dir) {
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
            let listed = engine.list_containers(engine.prefix()).ok()?;
            Some((engine, listed.into_iter().map(|c| c.name).collect()))
        });
        let Some((engine, listed)) = found else {
            continue;
        };
        delete(&path, &record, engine, listed, &mut reaped, note);
    }
    reaped
}

/// Deletes the record's containers that `listed` still has, then the record, unless a delete
/// failed: `inspect` on a container left behind shows its environment.
fn delete(
    path: &Path,
    record: &Value,
    engine: &Engine,
    listed: &BTreeSet<String>,
    reaped: &mut Vec<String>,
    note: &dyn Fn(&str),
) {
    let mut kept = false;
    for container in containers_of(record) {
        // Another prefix's container is not in `listed`, which would read as already gone.
        if !container.starts_with(engine.prefix()) {
            kept = true;
            continue;
        }
        if !listed.contains(&container) {
            continue;
        }
        note(&format!(
            "reaping {container}: the run that started it ended without deleting it"
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
        remove(path);
    }
}

/// The shell a [`Reaper`] runs: wait for end of file, then delete each name the record still
/// lists. A container the engine no longer has is not a failure; one it still has after the
/// delete keeps the record, for the next sweep. `$1` record, `$2` names, `$3` CLI, `$4` delete verb.
const REAP: &str = r#"cat >/dev/null
[ -f "$1" ] || exit 0
kept=0
while IFS= read -r name; do
  [ -n "$name" ] || continue
  "$3" stop "$name" >/dev/null 2>&1
  "$3" "$4" "$name" >/dev/null 2>&1 || ! "$3" inspect "$name" >/dev/null 2>&1 || kept=1
done < "$2"
[ "$kept" = 1 ] || rm -f "$1" "$2""#;

/// A process that deletes one record's containers when the run holding it ends, however it ends.
///
/// It reads a pipe this process holds the other end of. Dropping the reaper closes the pipe, and
/// so does this process dying, SIGKILL included, which runs no teardown. It leads its own process
/// group, so a caller that kills this one's group, as pma does at its deadline, does not reach it.
/// A run that tore down cleanly released its record first, and the reaper finds nothing to do.
///
/// It knows its run is over, so it does not ask the pid: a killed owner not yet waited for is a
/// zombie, and a zombie answers as alive. It is `sh` and the engine's CLI, not this executable,
/// which in a library is the caller's and has no command for it.
///
/// Tied to the run rather than the process: `serve` runs many in one process.
pub struct Reaper {
    child: Child,
    pipe: Option<ChildStdin>,
}

impl Reaper {
    /// Starts the reaper for `run`, with the CLI `engine` names for its reaper. `None` when it
    /// cannot start; the next run's sweep is still there.
    pub fn start(run: &Run, engine: &Engine, note: &dyn Fn(&str)) -> Option<Reaper> {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("sh")
            .args(["-c", REAP, &format!("{}reap", engine.prefix())])
            .arg(&run.path)
            .arg(run.names_path())
            .args([engine.reaper_cli(), engine.kind().delete_verb()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| note(&format!("no reaper for this run: {e}")))
            .ok()?;
        let pipe = child.stdin.take();
        Some(Reaper { child, pipe })
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        drop(self.pipe.take());
        let _ = self.child.wait();
    }
}

fn names_of(record: &Path) -> PathBuf {
    record.with_extension("names")
}

/// The record and its names.
fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(names_of(path));
}
