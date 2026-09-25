//! Install sections: the vocabulary recipes and kits share.
//!
//! A section is checked when its file is read and rendered to Containerfile lines when an image
//! is built. Every value that reaches a shell is matched against a character set and quoted.
//! `run` is the exception by design: its lines are written to a script in the build context
//! rather than spliced into a RUN line, so no quoting rule stands between the author and the
//! shell they wrote.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::catalog::{NAME_RE, SHA256_RE};
use crate::error::{Error, Result};
use crate::util::{py_json_list, shell_quote as q};

pub const TYPES: [&str; 7] = ["apt", "npm", "pip", "binary", "archive", "copy", "run"];

/// What `dpkg --print-architecture` prints, and what `uname -m` prints where there is no dpkg.
const ARCHES: [(&str, [&str; 2]); 2] = [
    ("amd64", ["amd64", "x86_64"]),
    ("arm64", ["arm64", "aarch64"]),
];

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}

/// Package specs: nothing a shell treats specially, and no leading dash, so a spec cannot become a
/// flag.
static SPEC_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9@][A-Za-z0-9._+:@/~=<>!,-]*$"));
static NPM_PINNED: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(@[a-z0-9._-]+/)?[a-z0-9._-]+@[0-9][A-Za-z0-9._+-]*$"));
static PIP_PINNED: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[A-Za-z0-9][A-Za-z0-9._-]*==[A-Za-z0-9.+!_-]+$"));
static FLAG_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^--?[a-z][a-z0-9-]*(=[A-Za-z0-9._/:-]+)?$"));
pub static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^https://[A-Za-z0-9._~:/?#@!$&'()*+,;=%-]+$"));
static SEGMENTS_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[A-Za-z0-9._+-]+(/[A-Za-z0-9._+-]+)*$"));
static MODE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^0?[0-7]{3}$"));
static ENV_KEY_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z_][A-Za-z0-9_]*$"));
pub static USER_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[a-z_][a-z0-9_-]*$"));

fn keys(kind: &str) -> &'static [&'static str] {
    match kind {
        "apt" => &["install"],
        "npm" | "pip" => &["install", "flags"],
        "binary" => &["artifacts", "path"],
        "archive" => &["artifacts", "dest", "links"],
        "copy" => &["from", "to", "mode"],
        _ => &["lines", "user"],
    }
}

/// Where `run` scripts land in the image. Kept, not deleted: a record of what built the image.
const STEPS_DIR: &str = "/usr/local/share/sanduk/steps";

pub fn fail(where_: &str, msg: impl AsRef<str>) -> Error {
    Error::new(format!("{where_}: {}", msg.as_ref()))
}

/// A single-line string. A newline would end a Containerfile instruction.
pub fn text<'a>(value: Option<&'a Value>, where_: &str, what: &str) -> Result<&'a str> {
    match value.and_then(Value::as_str) {
        Some(s) if !s.is_empty() && !s.contains(['\n', '\r']) => Ok(s),
        _ => Err(fail(
            where_,
            format!("{what} must be a non-empty single-line string"),
        )),
    }
}

pub fn matching<'a>(
    value: Option<&'a Value>,
    pattern: &Regex,
    where_: &str,
    what: &str,
) -> Result<&'a str> {
    let found = text(value, where_, what)?;
    if !pattern.is_match(found) {
        return Err(fail(
            where_,
            format!("{what} {found:?} is not allowed here"),
        ));
    }
    Ok(found)
}

pub fn strings(value: Option<&Value>, where_: &str, what: &str) -> Result<Vec<String>> {
    match value.and_then(Value::as_array) {
        Some(items) if !items.is_empty() => items
            .iter()
            .map(|v| text(Some(v), where_, what).map(String::from))
            .collect(),
        _ => Err(fail(where_, format!("{what} must be a non-empty list"))),
    }
}

fn check_keys(data: &Map<String, Value>, allowed: &[&str], where_: &str) -> Result<()> {
    let mut unknown: Vec<&str> = data
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    Err(fail(where_, format!("unknown keys {}", unknown.join(", "))))
}

/// A path with no `.` or `..` segment, absolute or relative as asked.
pub fn path_value<'a>(
    value: Option<&'a Value>,
    where_: &str,
    what: &str,
    absolute: bool,
) -> Result<&'a str> {
    let found = text(value, where_, what)?;
    let body = if absolute {
        found.strip_prefix('/').unwrap_or("")
    } else {
        found
    };
    if (absolute && !found.starts_with('/')) || !SEGMENTS_RE.is_match(body) {
        let kind = if absolute {
            "an absolute"
        } else {
            "a relative"
        };
        return Err(fail(
            where_,
            format!("{what} {found:?} must be {kind} path of plain segments"),
        ));
    }
    if body.split('/').any(|part| part == "." || part == "..") {
        return Err(fail(
            where_,
            format!("{what} {found:?} may not contain . or .."),
        ));
    }
    Ok(found)
}

pub fn env_map(value: Option<&Value>, where_: &str) -> Result<EnvMap> {
    let mut out = EnvMap::new();
    let Some(value) = value else {
        return Ok(out);
    };
    let Value::Object(map) = value else {
        return Err(fail(where_, "env must be an object"));
    };
    for (key, v) in map {
        if !ENV_KEY_RE.is_match(key) {
            return Err(fail(where_, format!("env key {key:?} is not allowed here")));
        }
        match v.as_str() {
            Some(s) if !s.contains(['\n', '\r']) => out.insert(key.clone(), s.to_string()),
            _ => {
                return Err(fail(
                    where_,
                    format!("env {key} must be a single-line string"),
                ));
            }
        }
    }
    Ok(out)
}

/// An insertion-ordered string map: an ENV line keeps the order the file wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvMap(Vec<(String, String)>);

impl EnvMap {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Replaces in place, or appends.
    pub fn insert(&mut self, key: String, value: String) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn remove(&mut self, key: &str) {
        self.0.retain(|(k, _)| k != key);
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn to_json(&self) -> Value {
        Value::Object(
            self.0
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        )
    }
}

/// One validated install step, and the directory `copy` reads from.
#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub kind: String,
    pub raw: Map<String, Value>,
    pub base: PathBuf,
}

impl Section {
    pub fn as_agent(&self) -> bool {
        self.raw.get("user").and_then(Value::as_str) == Some("agent")
    }

    pub fn has_artifact(&self, arch: &str) -> bool {
        self.raw
            .get("artifacts")
            .and_then(|a| a.get(arch))
            .is_some()
    }
}

pub fn section(raw: &Value, where_: &str, base: &Path) -> Result<Section> {
    let Value::Object(raw) = raw else {
        return Err(fail(where_, "each section must be an object"));
    };
    let name = matching(raw.get("name"), &NAME_RE, where_, "section name")?.to_string();
    let where_ = &format!("{where_}: section {name}");
    let kind = match raw.get("type").and_then(Value::as_str) {
        Some(kind) if TYPES.contains(&kind) => kind.to_string(),
        _ => {
            return Err(fail(
                where_,
                format!("type must be one of {}", TYPES.join(", ")),
            ));
        }
    };
    let mut allowed = vec!["name", "type", "description"];
    allowed.extend(keys(&kind));
    check_keys(raw, &allowed, where_)?;
    if raw.contains_key("description") {
        text(raw.get("description"), where_, "description")?;
    }

    match kind.as_str() {
        "apt" | "npm" | "pip" => {
            let specs = strings(raw.get("install"), where_, "install")?;
            for spec in &specs {
                matching(
                    Some(&Value::String(spec.clone())),
                    &SPEC_RE,
                    where_,
                    "package",
                )?;
            }
            if kind != "apt" {
                let (pattern, example) = if kind == "npm" {
                    (&*NPM_PINNED, "pkg@1.2.3")
                } else {
                    (&*PIP_PINNED, "pkg==1.2.3")
                };
                for spec in &specs {
                    if !pattern.is_match(spec) {
                        return Err(fail(
                            where_,
                            format!("{spec:?} is unpinned; give an exact version, as {example}"),
                        ));
                    }
                }
                let flags = match raw.get("flags") {
                    None => &[][..],
                    Some(Value::Array(flags)) => flags.as_slice(),
                    Some(_) => return Err(fail(where_, "flags must be a list")),
                };
                for flag in flags {
                    matching(Some(flag), &FLAG_RE, where_, "flag")?;
                }
            }
        }
        "binary" | "archive" => {
            artifacts(raw.get("artifacts"), where_, &kind)?;
            if kind == "binary" && raw.contains_key("path") {
                path_value(raw.get("path"), where_, "path", true)?;
            }
            if kind == "archive" {
                path_value(raw.get("dest"), where_, "dest", true)?;
                let links = match raw.get("links") {
                    None => None,
                    Some(Value::Object(links)) => Some(links),
                    Some(_) => return Err(fail(where_, "links must be an object")),
                };
                for (link, target) in links.into_iter().flatten() {
                    path_value(Some(&Value::String(link.clone())), where_, "link", true)?;
                    path_value(Some(target), where_, "link target", false)?;
                }
            }
        }
        "copy" => {
            let source = path_value(raw.get("from"), where_, "from", false)?;
            let joined = base.join(source);
            let inside = std::fs::canonicalize(&joined)
                .ok()
                .zip(std::fs::canonicalize(base).ok())
                .is_some_and(|(r, b)| r.starts_with(b));
            if !inside || joined.is_symlink() {
                return Err(fail(
                    where_,
                    format!("from {source:?} leaves {}", base.display()),
                ));
            }
            if !joined.is_file() {
                return Err(fail(
                    where_,
                    format!("from {source:?} is not a file in {}", base.display()),
                ));
            }
            path_value(raw.get("to"), where_, "to", true)?;
            if raw.contains_key("mode") {
                matching(raw.get("mode"), &MODE_RE, where_, "mode")?;
            }
        }
        _ => {
            strings(raw.get("lines"), where_, "lines")?;
            if !matches!(
                raw.get("user").and_then(Value::as_str).unwrap_or("root"),
                "root" | "agent"
            ) || raw.get("user").is_some_and(|u| !u.is_string())
            {
                return Err(fail(where_, "user must be \"root\" or \"agent\""));
            }
        }
    }
    Ok(Section {
        name,
        kind,
        raw: raw.clone(),
        base: base.to_path_buf(),
    })
}

fn artifacts(value: Option<&Value>, where_: &str, kind: &str) -> Result<()> {
    let Some(Value::Object(value)) = value.filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
    else {
        return Err(fail(
            where_,
            "artifacts must be an object keyed by amd64 and/or arm64",
        ));
    };
    let extra = if kind == "binary" { "member" } else { "strip" };
    let mut members = std::collections::BTreeSet::new();
    for (arch, artifact) in value {
        if !ARCHES.iter().any(|(a, _)| a == arch) {
            return Err(fail(
                where_,
                format!("artifact architecture {arch:?} is not amd64 or arm64"),
            ));
        }
        let Value::Object(artifact) = artifact else {
            return Err(fail(where_, format!("artifact {arch} must be an object")));
        };
        check_keys(
            artifact,
            &["url", "sha256", extra],
            &format!("{where_}: {arch}"),
        )?;
        matching(artifact.get("url"), &URL_RE, where_, &format!("{arch} url"))?;
        matching(
            artifact.get("sha256"),
            &SHA256_RE,
            where_,
            &format!("{arch} sha256"),
        )?;
        if kind == "binary" {
            members.insert(artifact.contains_key("member"));
            if artifact.contains_key("member") {
                path_value(
                    artifact.get("member"),
                    where_,
                    &format!("{arch} member"),
                    false,
                )?;
            }
        } else if let Some(strip) = artifact.get("strip")
            && strip.as_u64().is_none()
        {
            return Err(fail(
                where_,
                format!("{arch} strip must be a non-negative integer"),
            ));
        }
    }
    if members.len() > 1 {
        return Err(fail(
            where_,
            "either every artifact names a member or none does",
        ));
    }
    Ok(())
}

// --- rendering ------------------------------------------------------------------------------------

/// The files a rendered Containerfile copies, by path in the build context.
#[derive(Debug, Default)]
pub struct Context {
    pub files: BTreeMap<String, Vec<u8>>,
}

impl Context {
    pub fn add(&mut self, rel: String, data: Vec<u8>) -> Result<String> {
        if self
            .files
            .get(&rel)
            .is_some_and(|existing| *existing != data)
        {
            return Err(Error::new(format!(
                "two different files would be copied as {rel}"
            )));
        }
        self.files.insert(rel.clone(), data);
        Ok(rel)
    }
}

/// One RUN instruction. Each piece carries its own trailing separator.
pub fn run_line(pieces: &[String]) -> String {
    format!("RUN {}", pieces.join(" \\\n    "))
}

/// A JSON value as the shell assignment reads it: a string bare, a number as written.
fn scalar(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), String::from)
}

/// Shell pieces that download this architecture's artifact to `"$tmp/download"` and verify it.
/// `names` maps an artifact key to the variable it sets.
fn fetch(label: &str, arts: &Map<String, Value>, names: &[(&str, &str)]) -> Vec<String> {
    let mut pieces: Vec<String> = vec![
        "set -eu;".into(),
        "arch=\"$(dpkg --print-architecture 2>/dev/null || uname -m)\";".into(),
        "case \"$arch\" in".into(),
    ];
    let mut arches: Vec<&String> = arts.keys().collect();
    arches.sort();
    for arch in arches {
        let artifact = &arts[arch];
        let mut assigns = vec![
            format!("url={}", q(&scalar(&artifact["url"]))),
            format!("sum={}", q(&scalar(&artifact["sha256"]))),
        ];
        for (key, var) in names {
            if let Some(v) = artifact.get(*key) {
                assigns.push(format!("{var}={}", q(&scalar(v))));
            }
        }
        let spellings = ARCHES
            .iter()
            .find(|(a, _)| a == arch)
            .map(|(_, s)| s.join("|"))
            .unwrap_or_default();
        pieces.push(format!("  {spellings}) {} ;;", assigns.join("; ")));
    }
    pieces.extend([
        format!("  *) echo \"{label}: no build for $arch\" >&2; exit 1 ;;"),
        "esac;".into(),
        "tmp=\"$(mktemp -d)\";".into(),
        "curl -fsSL -o \"$tmp/download\" \"$url\";".into(),
        "echo \"$sum  $tmp/download\" | sha256sum -c -;".into(),
    ]);
    pieces
}

fn str_list(raw: &Map<String, Value>, key: &str) -> Vec<String> {
    raw.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Containerfile lines for one section. `owner` names the recipe or kit.
pub fn render(s: &Section, owner: &str, ctx: &mut Context) -> Result<Vec<String>> {
    let raw = &s.raw;
    let mut head = format!("# {owner}: {}", s.name);
    if let Some(description) = raw.get("description").and_then(Value::as_str) {
        head += &format!(" -- {description}");
    }
    let flags = str_list(raw, "flags")
        .iter()
        .map(|f| q(f))
        .collect::<Vec<_>>()
        .join(" ");
    let specs = str_list(raw, "install")
        .iter()
        .map(|p| q(p))
        .collect::<Vec<_>>()
        .join(" ");
    let flags = if flags.is_empty() {
        String::new()
    } else {
        format!("{flags} ")
    };
    let lines = |text: String| text.lines().map(String::from).collect::<Vec<_>>();

    let body: Vec<String> = match s.kind.as_str() {
        "apt" => vec![
            "RUN apt-get update \\".into(),
            " && apt-get install -y --no-install-recommends \\".into(),
            format!("      {specs} \\"),
            " && rm -rf /var/lib/apt/lists/*".into(),
        ],
        "npm" => vec![
            format!("RUN npm install -g {flags}{specs} \\"),
            " && npm cache clean --force".into(),
        ],
        "pip" => vec![format!("RUN pip install --no-cache-dir {flags}{specs}")],
        "binary" => {
            let arts = raw["artifacts"].as_object().expect("checked when read");
            let default = format!("/usr/local/bin/{}", s.name);
            let target = raw.get("path").and_then(Value::as_str).unwrap_or(&default);
            let mut pieces = fetch(&s.name, arts, &[("member", "member")]);
            if arts.values().any(|a| a.get("member").is_some()) {
                pieces.push("tar -xzf \"$tmp/download\" -C \"$tmp\" \"$member\";".into());
                pieces.push(format!(
                    "install -D -m 0755 \"$tmp/$member\" {};",
                    q(target)
                ));
            } else {
                pieces.push(format!(
                    "install -D -m 0755 \"$tmp/download\" {};",
                    q(target)
                ));
            }
            pieces.push("rm -rf \"$tmp\"".into());
            lines(run_line(&pieces))
        }
        "archive" => {
            let arts = raw["artifacts"].as_object().expect("checked when read");
            let dest = raw["dest"].as_str().expect("checked when read");
            let mut pieces = fetch(&s.name, arts, &[("strip", "strip")]);
            pieces.push(format!("mkdir -p {};", q(dest)));
            pieces.push(format!(
                "tar -xzf \"$tmp/download\" -C {} --strip-components=\"${{strip:-0}}\";",
                q(dest)
            ));
            let mut links: Vec<(&String, &Value)> = raw
                .get("links")
                .and_then(Value::as_object)
                .map(|l| l.iter().collect())
                .unwrap_or_default();
            links.sort_by(|a, b| a.0.cmp(b.0));
            for (link, target) in links {
                let target = format!("{dest}/{}", target.as_str().unwrap_or(""));
                pieces.push(format!("ln -sf {} {};", q(&target), q(link)));
            }
            pieces.push("rm -rf \"$tmp\"".into());
            lines(run_line(&pieces))
        }
        "copy" => {
            let from = raw["from"].as_str().expect("checked when read");
            let file_name = Path::new(from)
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let rel = ctx.add(
                format!("files/{owner}/{}/{file_name}", s.name),
                std::fs::read(s.base.join(from))?,
            )?;
            let to = raw["to"].as_str().expect("checked when read");
            let mut body = vec![format!("COPY {rel} {to}")];
            if let Some(mode) = raw.get("mode").and_then(Value::as_str) {
                body.push(format!("RUN chmod {mode} {to}"));
            }
            body
        }
        _ => {
            let script = format!("{owner}-{}.sh", s.name);
            let rel = ctx.add(
                format!("steps/{script}"),
                (str_list(raw, "lines").join("\n") + "\n").into_bytes(),
            )?;
            vec![
                format!("COPY {rel} {STEPS_DIR}/{script}"),
                format!("RUN sh -eu {STEPS_DIR}/{script}"),
            ]
        }
    };
    Ok(std::iter::once(head).chain(body).collect())
}

/// JSON exec form, for ENTRYPOINT and agent setup: no shell parses it.
pub fn exec_form(argv: &[String]) -> String {
    py_json_list(argv)
}
