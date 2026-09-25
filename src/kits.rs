//! Kits: named bundles of tools and skills that recipes include.
//!
//! A kit is a directory holding `kit.json`. Its identity is the SHA-256 of that file's bytes,
//! which a recipe pins. `kit.json` in turn pins downloads by `sha256`, and every file of a
//! vendored skill by its own hash, checked here when the kit is read. A file a `copy` tool takes
//! from the kit directory is not pinned; see sanduk's TODO.md.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::{self, Kind, NAME_RE, SHA256_RE};
use crate::error::{Error, Result};
use crate::sections::{
    self, Context, EnvMap, Section, URL_RE, fail, matching, path_value, run_line, strings, text,
};
use crate::util::shell_quote as q;

const KIT_KEYS: [&str; 9] = [
    "name",
    "description",
    "tools",
    "skills",
    "env",
    "agents",
    "egress",
    "provides",
    "hook",
];
pub const SKILL_FILE: &str = "SKILL.md";

/// A vendored skill directory (`path`, `files`) or one fetched `SKILL.md`.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub path: Option<PathBuf>,
    /// (relative path, sha256), sorted.
    pub files: Vec<(String, String)>,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct Kit {
    pub name: String,
    /// kit.json.
    pub path: PathBuf,
    pub sha256: String,
    pub description: String,
    pub tools: Vec<Section>,
    pub skills: Vec<Skill>,
    pub env: EnvMap,
    /// Agent name to the setup argvs run as the agent user. Empty: any agent.
    pub agents: BTreeMap<String, Vec<Vec<String>>>,
    pub egress: bool,
    pub provides: Vec<String>,
    pub hook: bool,
}

pub fn digest(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Reads and checks a kit by catalogue name or path.
pub fn load(spec: &str, relative_to: Option<&Path>) -> Result<Kit> {
    let (path, _) = catalog::locate(Kind::Kits, spec, relative_to)?;
    let (raw, data) = catalog::read_json(&path, &KIT_KEYS)?;
    let where_ = &path.display().to_string();
    let dir = path.parent().expect("kit.json has a directory");
    let dir_name = dir
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let name = matching(data.get("name"), &NAME_RE, where_, "name")?.to_string();
    if name != dir_name {
        return Err(fail(
            where_,
            format!("name {name:?} must equal its directory, {dir_name:?}"),
        ));
    }

    let list = |key: &str| -> Result<Vec<Value>> {
        match data.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => Ok(items.clone()),
            Some(_) => Err(fail(where_, format!("{key} must be a list"))),
        }
    };
    let tools = list("tools")?
        .iter()
        .map(|s| sections::section(s, where_, dir))
        .collect::<Result<Vec<_>>>()?;
    unique(tools.iter().map(|t| t.name.as_str()), where_, "tool")?;
    let skills = list("skills")?
        .iter()
        .map(|s| skill(s, where_, dir))
        .collect::<Result<Vec<_>>>()?;
    unique(skills.iter().map(|s| s.name.as_str()), where_, "skill")?;

    let mut agents = BTreeMap::new();
    match data.get("agents") {
        None => {}
        Some(Value::Object(entries)) => {
            for (agent, entry) in entries {
                matching(
                    Some(&Value::String(agent.clone())),
                    &NAME_RE,
                    where_,
                    "agent name",
                )?;
                let Value::Object(entry) = entry else {
                    return Err(fail(where_, format!("agents.{agent} takes only setup")));
                };
                if entry.keys().any(|k| k != "setup") {
                    return Err(fail(where_, format!("agents.{agent} takes only setup")));
                }
                let setup = match entry.get("setup") {
                    None => Vec::new(),
                    Some(Value::Array(argvs)) => argvs
                        .iter()
                        .map(|argv| strings(Some(argv), where_, "setup argv"))
                        .collect::<Result<Vec<_>>>()?,
                    Some(_) => {
                        return Err(fail(
                            where_,
                            format!("agents.{agent}.setup must be a list of argv lists"),
                        ));
                    }
                };
                agents.insert(agent.clone(), setup);
            }
        }
        Some(_) => return Err(fail(where_, "agents must be an object keyed by agent name")),
    }

    let provides = list("provides")?
        .iter()
        .map(|p| matching(Some(p), &NAME_RE, where_, "provides").map(String::from))
        .collect::<Result<Vec<_>>>()?;
    let flag = |key: &str| -> Result<bool> {
        match data.get(key) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(fail(where_, format!("{key} must be true or false"))),
        }
    };
    let description = match data.get("description") {
        None => String::new(),
        Some(v) => text(Some(v), where_, "description")?.to_string(),
    };

    Ok(Kit {
        name,
        path: path.clone(),
        sha256: digest(&raw),
        description,
        tools,
        skills,
        env: sections::env_map(data.get("env"), where_)?,
        agents,
        egress: flag("egress")?,
        provides,
        hook: flag("hook")?,
    })
}

pub fn unique<'a>(names: impl Iterator<Item = &'a str>, where_: &str, what: &str) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(fail(where_, format!("two {what}s named {name:?}")));
        }
    }
    Ok(())
}

fn skill(raw: &Value, where_: &str, base: &Path) -> Result<Skill> {
    let Value::Object(raw) = raw else {
        return Err(fail(where_, "each skill must be an object"));
    };
    let mut keys: Vec<&str> = raw.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if raw.contains_key("path") {
        if keys != ["files", "path"] {
            return Err(fail(
                where_,
                "a vendored skill takes exactly path and files",
            ));
        }
        let rel = path_value(raw.get("path"), where_, "skill path", false)?;
        let Some(Value::Object(files)) = raw.get("files") else {
            return Err(fail(
                where_,
                format!("skill {rel}: files must map each file to its sha256"),
            ));
        };
        let root = base.join(rel);
        verify_dir(&root, base, files, &format!("{where_}: skill {rel}"))?;
        let mut listed: Vec<(String, String)> = files
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
            .collect();
        listed.sort();
        let name = root
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        return Ok(Skill {
            name,
            path: Some(root),
            files: listed,
            url: String::new(),
            sha256: String::new(),
        });
    }
    if keys != ["name", "sha256", "url"] {
        return Err(fail(
            where_,
            "a skill takes path and files, or name, url and sha256",
        ));
    }
    let name = matching(raw.get("name"), &NAME_RE, where_, "skill name")?.to_string();
    Ok(Skill {
        url: matching(
            raw.get("url"),
            &URL_RE,
            where_,
            &format!("skill {name} url"),
        )?
        .to_string(),
        sha256: matching(
            raw.get("sha256"),
            &SHA256_RE,
            where_,
            &format!("skill {name} sha256"),
        )?
        .to_string(),
        name,
        path: None,
        files: Vec::new(),
    })
}

/// Every file under `root` is listed with a matching hash, and nothing more. An unlisted file
/// would ride along uncovered by the recipe's pin.
fn verify_dir(
    root: &Path,
    base: &Path,
    files: &serde_json::Map<String, Value>,
    where_: &str,
) -> Result<()> {
    let inside = std::fs::canonicalize(root)
        .ok()
        .zip(std::fs::canonicalize(base).ok())
        .is_some_and(|(r, b)| r.starts_with(b));
    if root.is_symlink() || !inside {
        return Err(fail(where_, "must be a directory inside the kit"));
    }
    if !root.is_dir() {
        return Err(fail(where_, "is not a directory"));
    }
    let mut found = BTreeMap::new();
    walk(root, root, &mut found, where_)?;
    let unlisted: Vec<&str> = found
        .keys()
        .filter(|k| !files.contains_key(*k))
        .map(String::as_str)
        .collect();
    let missing: Vec<&str> = files
        .keys()
        .filter(|k| !found.contains_key(*k))
        .map(String::as_str)
        .collect();
    if !unlisted.is_empty() {
        return Err(fail(
            where_,
            format!("files not listed in kit.json: {}", unlisted.join(", ")),
        ));
    }
    if !missing.is_empty() {
        return Err(fail(
            where_,
            format!("listed files that are not there: {}", missing.join(", ")),
        ));
    }
    let mut listed: Vec<(&String, &Value)> = files.iter().collect();
    listed.sort_by(|a, b| a.0.cmp(b.0));
    for (rel, expected) in listed {
        let expected = matching(Some(expected), &SHA256_RE, where_, &format!("{rel} sha256"))?;
        let actual = digest(&std::fs::read(&found[rel])?);
        if actual != expected {
            return Err(fail(
                where_,
                format!("{rel} is {actual}, but kit.json lists {expected}"),
            ));
        }
    }
    let Some(skill_md) = found.get(SKILL_FILE) else {
        return Err(fail(where_, format!("has no {SKILL_FILE}")));
    };
    let meta = frontmatter(&String::from_utf8_lossy(&std::fs::read(skill_md)?));
    let dir_name = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    if meta.get("name").map(String::as_str) != Some(dir_name.as_str())
        || meta.get("description").is_none_or(|d| d.is_empty())
    {
        return Err(fail(
            where_,
            format!("{SKILL_FILE} needs frontmatter with name: {dir_name} and a description"),
        ));
    }
    Ok(())
}

fn walk(
    dir: &Path,
    root: &Path,
    found: &mut BTreeMap<String, PathBuf>,
    where_: &str,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if path.is_symlink() {
            return Err(fail(
                where_,
                format!("{name} is a symlink; a skill holds plain files"),
            ));
        }
        if path.is_dir() {
            walk(&path, root, found, where_)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(rel, path);
        }
    }
    Ok(())
}

/// The top-level keys of a `---` block at the start, as plain strings.
pub fn frontmatter(body: &str) -> BTreeMap<String, String> {
    let mut lines = body.lines();
    if lines.next().map(str::trim) != Some("---") {
        return BTreeMap::new();
    }
    let mut found = BTreeMap::new();
    for line in lines {
        if line.trim() == "---" {
            return found;
        }
        if let Some((key, value)) = line.split_once(':')
            && !line.starts_with(char::is_whitespace)
        {
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            found.insert(key.trim().to_string(), value.to_string());
        }
    }
    BTreeMap::new()
}

// --- rendering ------------------------------------------------------------------------------------

/// Root-owned, read-only skill directories under `skills_root`. A run should not be able to
/// rewrite its own instructions.
pub fn render_skills(kit: &Kit, skills_root: &str, ctx: &mut Context) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for s in &kit.skills {
        let dest = format!("{skills_root}/{}", s.name);
        lines.push(format!("# kit-{}: skill {}", kit.name, s.name));
        if let Some(path) = &s.path {
            // One COPY per file. A directory COPY left the directory empty on Apple's builder
            // (container 1.2.0) unless a file was also named.
            for (rel, _) in &s.files {
                let source = ctx.add(
                    format!("skills/{}/{}/{rel}", kit.name, s.name),
                    std::fs::read(path.join(rel))?,
                )?;
                lines.push(format!("COPY {source} {dest}/{rel}"));
            }
            lines.push(format!("RUN chmod -R a=rX {}", q(&dest)));
        } else {
            let pieces: Vec<String> = vec![
                "set -eu;".into(),
                "tmp=\"$(mktemp)\";".into(),
                format!("curl -fsSL -o \"$tmp\" {};", q(&s.url)),
                format!("echo \"{}  $tmp\" | sha256sum -c -;", s.sha256),
                format!(
                    "install -D -m 0444 \"$tmp\" {};",
                    q(&format!("{dest}/{SKILL_FILE}"))
                ),
                format!("chmod 0555 {};", q(&dest)),
                "rm -f \"$tmp\"".into(),
            ];
            lines.extend(run_line(&pieces).lines().map(String::from));
        }
    }
    Ok(lines)
}

pub fn render_setup(kit: &Kit, agent: &str) -> Vec<String> {
    kit.agents
        .get(agent)
        .map(|setup| {
            setup
                .iter()
                .map(|argv| format!("RUN {}", sections::exec_form(argv)))
                .collect()
        })
        .unwrap_or_default()
}

/// Refuses an agent this kit cannot serve.
pub fn check(kit: &Kit, agent: &str, skills_dir: Option<&str>) -> Result<()> {
    if !kit.agents.is_empty() && !kit.agents.contains_key(agent) {
        let names: Vec<&str> = kit.agents.keys().map(String::as_str).collect();
        return Err(Error::new(format!(
            "kit {} supports {}, not {agent}",
            kit.name,
            names.join(", ")
        )));
    }
    if !kit.skills.is_empty() && skills_dir.is_none() && !kit.agents.contains_key(agent) {
        return Err(Error::new(format!(
            "kit {} carries skills, and sanduk does not know where {agent} reads them. The tools \
             would be installed with nothing telling the agent they exist",
            kit.name
        )));
    }
    Ok(())
}
