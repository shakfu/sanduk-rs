//! Finding agents, recipes and kits by name, and reading their files.
//!
//! A name is looked up in two places: what ships inside the binary, and the user's config
//! directory. A name found in both is refused: a user file claiming a shipped name would change
//! what runs without changing the command line.
//!
//! Lookup by name never reads the working directory. A cloned repository could otherwise supply
//! a kit that runs as root, with network access, at build time. A path given explicitly is read as
//! given.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::resources;
use crate::util::config_dir;

pub static NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]*$").unwrap());
pub static SHA256_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Agents,
    Recipes,
    Kits,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Agents => "agents",
            Kind::Recipes => "recipes",
            Kind::Kits => "kits",
        }
    }

    fn singular(self) -> &'static str {
        match self {
            Kind::Agents => "agent",
            Kind::Recipes => "recipe",
            Kind::Kits => "kit",
        }
    }
}

/// Where a named entry lives under one catalogue root.
fn entry_path(kind: Kind, root: &Path, name: &str) -> PathBuf {
    match kind {
        Kind::Agents => root.join("agents").join(format!("{name}.toml")),
        Kind::Recipes => root.join("recipes").join(format!("{name}.json")),
        Kind::Kits => root.join("kits").join(name).join("kit.json"),
    }
}

/// A spec with a slash, or a file suffix, names a file rather than a catalogue entry.
pub fn is_path(spec: &str) -> bool {
    spec.contains('/') || spec.ends_with(".json") || spec.ends_with(".toml")
}

/// Where an entry was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Shipped,
    User,
    /// Named by path on the command line or in another file.
    Path,
}

fn roots() -> Result<[(Origin, PathBuf); 2]> {
    Ok([
        (Origin::Shipped, resources::root()?),
        (Origin::User, config_dir()),
    ])
}

/// Every name of this kind the catalogue can find.
pub fn names(kind: Kind) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for (_, root) in roots()? {
        let Ok(entries) = std::fs::read_dir(root.join(kind.dir())) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match kind {
                Kind::Kits => path.file_name(),
                _ => path.file_stem(),
            };
            let Some(name) = name.and_then(|n| n.to_str()) else {
                continue;
            };
            if NAME_RE.is_match(name) && entry_path(kind, &root, name).is_file() {
                found.push(name.to_string());
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// The file a spec names, and where it came from: a catalogue name, or a path.
pub fn locate(kind: Kind, spec: &str, relative_to: Option<&Path>) -> Result<(PathBuf, Origin)> {
    let singular = kind.singular();
    if is_path(spec) {
        let expanded = match spec.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
            None => PathBuf::from(spec),
        };
        let mut path = if expanded.is_absolute() {
            expanded
        } else {
            match relative_to {
                Some(base) => base.join(expanded),
                None => std::env::current_dir()?.join(expanded),
            }
        };
        if kind == Kind::Kits && path.is_dir() {
            path = path.join("kit.json");
        }
        if !path.is_file() {
            return Err(Error::new(format!("no {singular} at {}", path.display())));
        }
        return Ok((std::fs::canonicalize(path)?, Origin::Path));
    }
    if !NAME_RE.is_match(spec) {
        return Err(Error::new(format!(
            "{spec:?} is not a {singular} name ([a-z0-9-]) or a path (containing /, or ending \
             .json or .toml)"
        )));
    }
    let hits: Vec<(Origin, PathBuf)> = roots()?
        .into_iter()
        .map(|(origin, root)| (origin, entry_path(kind, &root, spec)))
        .filter(|(_, path)| path.is_file())
        .collect();
    match hits.as_slice() {
        [] => {
            let known = names(kind)?;
            let known = if known.is_empty() {
                "none".into()
            } else {
                known.join(", ")
            };
            Err(Error::new(format!(
                "no {singular} named {spec:?}; known: {known}"
            )))
        }
        [(origin, path)] => Ok((std::fs::canonicalize(path)?, *origin)),
        [_, (_, user)] => Err(Error::new(format!(
            "{singular} {spec:?} is shipped with sanduk and cannot be replaced ({}). Rename yours",
            user.display()
        ))),
        _ => unreachable!("two roots"),
    }
}

/// The file's bytes and its top-level object, refusing keys not in `allowed`.
pub fn read_json(path: &Path, allowed: &[&str]) -> Result<(Vec<u8>, Map<String, Value>)> {
    let raw = std::fs::read(path).map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
    let data: Value =
        serde_json::from_slice(&raw).map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
    let Value::Object(data) = data else {
        return Err(Error::new(format!(
            "{}: expected a JSON object",
            path.display()
        )));
    };
    let mut unknown: Vec<&str> = data
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "$schema" && !allowed.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(Error::new(format!(
            "{}: unknown keys {}",
            path.display(),
            unknown.join(", ")
        )));
    }
    Ok((raw, data))
}
