//! Agents as configuration: what sanduk needs to know about a CLI it runs.
//!
//! An agent is a TOML file. It names the image that carries the program (a recipe, or an image
//! and its Containerfile), the flags that drive it headlessly, the variables it reads its
//! endpoint and credential from, and the stream format it answers in. The runner knows none of
//! those answers, so a new agent is a new file: in the shipped catalogue, in
//! `~/.config/sanduk/agents/`, or anywhere, named by path.
//!
//! Templates use `{name}` placeholders (see [`template`]). Three things decide what a run gets:
//!
//! - `[check]` refuses a combination before anything starts: a flag the agent cannot honour, a
//!   required one missing, or a relayed mode it cannot be pointed at.
//! - `[vars]` looks a value up by the protocol the run speaks or by the provider's name.
//! - `argv` items are strings, or groups dropped whole when a variable they read is unset.
//!
//! See docs/agents.md.

pub mod stream;
pub mod template;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::catalog::{self, Kind, NAME_RE, Origin};
use crate::error::{Error, Result};
use crate::providers::{Provider, Scheme, protocol};
use crate::util::Dirs;
use template::{Template, Vars};

pub use stream::Outcome;

pub const DEFAULT_AGENT: &str = "codex";
pub const REPORT_NAME: &str = "REPORT.md";

pub fn report_instruction() -> String {
    format!(
        "\n\nWhen you are done, write your findings to ./{REPORT_NAME} in the working directory. \
         That file is the only output that survives; anything you print to the terminal is \
         discarded when the container is deleted."
    )
}

/// The run's own choices, as flags gave them. The names are the variables templates read.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_turns: Option<u32>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Option<String>,
    pub bare: bool,
    /// Whether the key stays on the host behind the relay: `--mode key-safe` or `sealed`.
    pub relayed: bool,
    /// `--agent-key-env` and `--agent-base-url-env`, which win over the spec's.
    pub key_env: Option<String>,
    pub base_url_env: Option<String>,
}

/// The options a spec's `require`, `refuse` and `when` may name: [`run::Options`](crate::run::Options)
/// fields.
const OPTIONS: [&str; 6] = [
    "model",
    "effort",
    "max_turns",
    "permission_mode",
    "allowed_tools",
    "bare",
];

/// Variables every template may read, before the spec's own `[vars]`.
const BASE_VARS: [&str; 13] = [
    "model",
    "effort",
    "max_turns",
    "permission_mode",
    "allowed_tools",
    "bare",
    "relayed",
    "provider",
    "provider_key_env",
    "provider_base_url_env",
    "api_prefix",
    "root",
    "endpoint",
];
/// Resolved from the provider and the spec's protocols.
const PROTOCOL_VAR: &str = "protocol";
/// Available to argv and env once the wiring is resolved.
const WIRE_VARS: [&str; 4] = ["key_env", "base_url_env", "base_url", "task"];

// --- the file ---------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecFile {
    name: String,
    #[serde(default)]
    description: String,
    recipe: Option<String>,
    image: Option<String>,
    containerfile: Option<String>,
    skills_dir: Option<String>,
    instructions_file: Option<String>,
    protocols: Vec<String>,
    stream: String,
    #[serde(default)]
    check: CheckFile,
    #[serde(default)]
    vars: BTreeMap<String, VarFile>,
    argv: Vec<ItemFile>,
    wire: WireFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckFile {
    #[serde(default)]
    require: Vec<String>,
    #[serde(default)]
    refuse: Vec<String>,
    /// Why this agent cannot run behind the relay. Present: `key-safe` and `sealed` are refused.
    relay: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VarFile {
    from: String,
    map: toml::Table,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ItemFile {
    Arg(String),
    Group {
        args: Vec<String>,
        #[serde(default)]
        when: Option<String>,
        #[serde(rename = "else", default)]
        otherwise: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFile {
    key_env: String,
    base_url_env: String,
    base_url: Option<String>,
    /// Keyed with their spans: without toml's `preserve_order`, which would leak into every crate
    /// that links this one, the span is what keeps the file's order.
    #[serde(default)]
    env: BTreeMap<toml::Spanned<String>, EnvFile>,
}

/// A `wire.env` value as written: a string, `{ json = ... }`, or anything else, refused later
/// with the variable's name.
#[derive(Debug)]
enum EnvFile {
    Text(String),
    Json(Doc),
    Other,
}

impl<'de> Deserialize<'de> for EnvFile {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = EnvFile;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string, or { json = ... }")
            }
            fn visit_str<E>(self, v: &str) -> std::result::Result<EnvFile, E> {
                Ok(EnvFile::Text(v.into()))
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<EnvFile, E> {
                Ok(EnvFile::Other)
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<EnvFile, E> {
                Ok(EnvFile::Other)
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<EnvFile, E> {
                Ok(EnvFile::Other)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<EnvFile, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(EnvFile::Other)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<EnvFile, A::Error> {
                let (mut doc, mut others) = (None, 0);
                while let Some(key) = map.next_key::<String>()? {
                    if key == "json" && doc.is_none() {
                        doc = Some(map.next_value::<Doc>()?);
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        others += 1;
                    }
                }
                Ok(match doc {
                    Some(doc) if others == 0 => EnvFile::Json(doc),
                    _ => EnvFile::Other,
                })
            }
        }
        d.deserialize_any(Visit)
    }
}

/// A JSON document in the order its TOML was written. Object keys are read with their spans and
/// sorted by them: toml's own tables are sorted maps without `preserve_order`.
#[derive(Debug, Clone, PartialEq)]
enum Doc {
    Scalar(serde_json::Value),
    Array(Vec<Doc>),
    Object(Vec<(String, Doc)>),
}

impl<'de> Deserialize<'de> for Doc {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Doc;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a TOML value")
            }
            fn visit_bool<E>(self, v: bool) -> std::result::Result<Doc, E> {
                Ok(Doc::Scalar(v.into()))
            }
            fn visit_i64<E>(self, v: i64) -> std::result::Result<Doc, E> {
                Ok(Doc::Scalar(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> std::result::Result<Doc, E> {
                Ok(Doc::Scalar(v.into()))
            }
            fn visit_f64<E>(self, v: f64) -> std::result::Result<Doc, E> {
                Ok(Doc::Scalar(v.into()))
            }
            fn visit_str<E>(self, v: &str) -> std::result::Result<Doc, E> {
                Ok(Doc::Scalar(v.into()))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Doc, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Doc::Array(items))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Doc, A::Error> {
                let mut members: Vec<(toml::Spanned<String>, Doc)> = Vec::new();
                while let Some(key) = map.next_key()? {
                    members.push((key, map.next_value()?));
                }
                members.sort_by_key(|(k, _)| k.span().start);
                Ok(Doc::Object(
                    members
                        .into_iter()
                        .map(|(k, v)| (k.into_inner(), v))
                        .collect(),
                ))
            }
        }
        d.deserialize_any(Visit)
    }
}

impl serde::Serialize for Doc {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Doc::Scalar(v) => v.serialize(s),
            Doc::Array(items) => s.collect_seq(items),
            Doc::Object(members) => {
                let mut map = s.serialize_map(Some(members.len()))?;
                for (k, v) in members {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

// --- the resolved spec ------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Item {
    Arg(Template),
    Group {
        args: Vec<Template>,
        when: Option<String>,
        otherwise: Vec<Template>,
    },
}

#[derive(Debug, Clone)]
enum EnvValue {
    Text(Template),
    /// A JSON document: keys and string leaves are templates, serialised compactly.
    Json(Doc),
}

#[derive(Debug, Clone)]
struct Var {
    by_provider: bool,
    map: BTreeMap<String, Template>,
}

/// One agent, read and checked.
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub origin: Origin,
    pub recipe: Option<String>,
    pub image: Option<String>,
    pub containerfile: Option<PathBuf>,
    pub skills_dir: Option<String>,
    pub instructions_file: Option<String>,
    /// In preference order: the run speaks the first one the provider serves.
    pub protocols: Vec<String>,
    pub stream: String,
    require: Vec<String>,
    refuse: Vec<String>,
    relay_refusal: Option<String>,
    vars: BTreeMap<String, Var>,
    argv: Vec<Item>,
    key_env: Template,
    base_url_env: Template,
    base_url: Option<Template>,
    env: Vec<(String, EnvValue)>,
}

/// How the container is pointed at the endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wiring {
    pub key_env: String,
    pub base_url_env: String,
    /// `None`: the agent's own built-in endpoint.
    pub base_url: Option<String>,
    /// Fixed settings the agent needs, as `K=V`; an explicit `-e` still wins.
    pub env: Vec<(String, String)>,
}

/// Where the run's requests go.
#[derive(Debug, Clone)]
pub struct Endpoint<'a> {
    pub provider: &'a Provider,
    /// The provider's, or `--upstream`'s.
    pub scheme: Scheme,
    pub host: String,
    /// The relay (`http://gateway:port`), `--base-url`, or `None` for the provider itself.
    pub root: Option<String>,
}

/// Every name the catalogue can find.
pub fn names(dirs: &Dirs) -> Result<Vec<String>> {
    catalog::names(dirs, Kind::Agents)
}

/// An agent by catalogue name, or by path to its file.
pub fn get(dirs: &Dirs, spec: &str) -> Result<Agent> {
    let (path, origin) = catalog::locate(dirs, Kind::Agents, spec, None)?;
    load(&path, origin)
}

fn load(path: &Path, origin: Origin) -> Result<Agent> {
    let where_ = path.display().to_string();
    let bad = |msg: String| Error::new(format!("{where_}: {msg}"));
    let text = std::fs::read_to_string(path).map_err(|e| bad(e.to_string()))?;
    let file: SpecFile = toml::from_str(&text).map_err(|e| bad(e.to_string()))?;

    if !NAME_RE.is_match(&file.name) {
        return Err(bad(format!("name {:?} is not allowed here", file.name)));
    }
    if origin != Origin::Path
        && path.file_stem().and_then(|s| s.to_str()) != Some(file.name.as_str())
    {
        return Err(bad(format!(
            "name {:?} must equal the file name",
            file.name
        )));
    }
    if file.protocols.is_empty() {
        return Err(bad("protocols must name at least one".into()));
    }
    for p in &file.protocols {
        if protocol(p).is_none() {
            return Err(bad(format!("protocol {p:?} is unknown")));
        }
    }
    if stream::reader(&file.stream).is_none() {
        return Err(bad(format!(
            "stream {:?} is unknown; known: {}",
            file.stream,
            stream::FORMATS.join(", ")
        )));
    }
    let containerfile = match (&file.recipe, &file.image, &file.containerfile) {
        (Some(_), None, None) => None,
        (None, Some(_), Some(cf)) => Some(path.parent().unwrap_or(Path::new("/")).join(cf)),
        _ => {
            return Err(bad(
                "give a recipe, or an image and its containerfile".into()
            ));
        }
    };
    for name in file.check.require.iter().chain(&file.check.refuse) {
        if !OPTIONS.contains(&name.as_str()) {
            return Err(bad(format!("check names {name:?}, which is not an option")));
        }
    }

    // What each stage may read: vars see the base; the wiring adds vars; argv and env add wiring.
    let mut known: Vec<String> = BASE_VARS
        .iter()
        .chain([&PROTOCOL_VAR])
        .map(|s| s.to_string())
        .collect();
    let parse = |text: &str, known: &[String], what: &str| -> Result<Template> {
        let t = Template::parse(text).map_err(|e| bad(format!("{what}: {e}")))?;
        if let Some(unknown) = t.names().find(|n| !known.iter().any(|k| k == n)) {
            return Err(bad(format!("{what}: {{{unknown}}} is not a variable here")));
        }
        Ok(t)
    };

    let mut vars = BTreeMap::new();
    for (name, var) in &file.vars {
        if known.contains(name) || WIRE_VARS.contains(&name.as_str()) {
            return Err(bad(format!("vars.{name} shadows a built-in variable")));
        }
        let by_provider = match var.from.as_str() {
            "provider" => true,
            "protocol" => false,
            other => {
                return Err(bad(format!(
                    "vars.{name}.from is {other:?}; use protocol or provider"
                )));
            }
        };
        let mut map = BTreeMap::new();
        for (key, value) in &var.map {
            let valid = key == "*"
                || if by_provider {
                    crate::providers::get_provider(key).is_ok()
                } else {
                    file.protocols.contains(key)
                };
            if !valid {
                return Err(bad(format!(
                    "vars.{name}.map: {key:?} is not a {}",
                    var.from
                )));
            }
            let value = value
                .as_str()
                .ok_or_else(|| bad(format!("vars.{name}.map.{key} must be a string")))?;
            map.insert(key.clone(), parse(value, &known, &format!("vars.{name}"))?);
        }
        vars.insert(name.clone(), Var { by_provider, map });
    }
    known.extend(vars.keys().cloned());

    let key_env = parse(&file.wire.key_env, &known, "wire.key_env")?;
    let base_url_env = parse(&file.wire.base_url_env, &known, "wire.base_url_env")?;
    let base_url = file
        .wire
        .base_url
        .as_deref()
        .map(|t| parse(t, &known, "wire.base_url"))
        .transpose()?;
    known.extend(WIRE_VARS.iter().map(|s| s.to_string()));

    let mut env = Vec::new();
    let mut in_order: Vec<_> = file.wire.env.iter().collect();
    in_order.sort_by_key(|(name, _)| name.span().start);
    for (name, value) in in_order {
        let name = name.get_ref();
        let what = format!("wire.env.{name}");
        let value = match value {
            EnvFile::Text(text) => EnvValue::Text(parse(text, &known, &what)?),
            EnvFile::Json(doc) => {
                check_json(doc, &known, &what, &parse)?;
                EnvValue::Json(doc.clone())
            }
            EnvFile::Other => {
                return Err(bad(format!("{what} must be a string, or {{ json = ... }}")));
            }
        };
        env.push((name.clone(), value));
    }

    let mut argv = Vec::new();
    for item in &file.argv {
        argv.push(match item {
            ItemFile::Arg(text) => Item::Arg(parse(text, &known, "argv")?),
            ItemFile::Group {
                args,
                when,
                otherwise,
            } => {
                if let Some(when) = when
                    && !known.contains(when)
                {
                    return Err(bad(format!("argv: when = {when:?} is not a variable")));
                }
                Item::Group {
                    args: args
                        .iter()
                        .map(|a| parse(a, &known, "argv"))
                        .collect::<Result<_>>()?,
                    when: when.clone(),
                    otherwise: otherwise
                        .iter()
                        .map(|a| parse(a, &known, "argv"))
                        .collect::<Result<_>>()?,
                }
            }
        });
    }
    if !argv
        .iter()
        .any(|i| matches!(i, Item::Arg(t) if t.names().any(|n| n == "task")))
    {
        return Err(bad("argv never passes {task}".into()));
    }

    Ok(Agent {
        name: file.name,
        description: file.description,
        path: path.to_path_buf(),
        origin,
        recipe: file.recipe,
        image: file.image,
        containerfile,
        skills_dir: file.skills_dir,
        instructions_file: file.instructions_file,
        protocols: file.protocols,
        stream: file.stream,
        require: file.check.require,
        refuse: file.check.refuse,
        relay_refusal: file.check.relay,
        vars,
        argv,
        key_env,
        base_url_env,
        base_url,
        env,
    })
}

/// A template parser that also checks its names against what the stage may read.
type Parse<'a> = dyn Fn(&str, &[String], &str) -> Result<Template> + 'a;

/// Parses every key and string leaf of a JSON document as a template, to check its names.
fn check_json(doc: &Doc, known: &[String], what: &str, parse: &Parse) -> Result<()> {
    match doc {
        Doc::Scalar(serde_json::Value::String(s)) => parse(s, known, what).map(|_| ()),
        Doc::Scalar(_) => Ok(()),
        Doc::Array(items) => items
            .iter()
            .try_for_each(|v| check_json(v, known, what, parse)),
        Doc::Object(members) => members.iter().try_for_each(|(k, v)| {
            parse(k, known, what)?;
            check_json(v, known, what, parse)
        }),
    }
}

/// A JSON document with its templates rendered, or `None` if one reads an unset variable.
fn render_json(doc: &Doc, vars: &Vars) -> Option<Doc> {
    let text = |s: &str| Template::parse(s).ok()?.render(vars);
    Some(match doc {
        Doc::Scalar(serde_json::Value::String(s)) => Doc::Scalar(text(s)?.into()),
        Doc::Scalar(other) => Doc::Scalar(other.clone()),
        Doc::Array(items) => Doc::Array(
            items
                .iter()
                .map(|v| render_json(v, vars))
                .collect::<Option<_>>()?,
        ),
        Doc::Object(members) => Doc::Object(
            members
                .iter()
                .map(|(k, v)| Some((text(k)?, render_json(v, vars)?)))
                .collect::<Option<_>>()?,
        ),
    })
}

/// The wire protocols this provider accepts a completion on.
pub fn completion_protocols(provider: &Provider) -> Vec<&'static str> {
    provider.routes.iter().filter_map(|(_, p)| *p).collect()
}

impl Agent {
    /// The protocol this run speaks: the first of the agent's the provider serves.
    fn protocol(&self, provider: &Provider) -> Option<&str> {
        let served = completion_protocols(provider);
        self.protocols
            .iter()
            .map(String::as_str)
            .find(|p| served.contains(p))
    }

    fn base_vars(&self, opts: &Options, at: &Endpoint) -> Vars {
        let mut vars = Vars::new();
        let mut set = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                vars.insert(k.to_string(), v);
            }
        };
        set("model", opts.model.clone());
        set("effort", opts.effort.clone());
        set("max_turns", opts.max_turns.map(|n| n.to_string()));
        set("permission_mode", opts.permission_mode.clone());
        set("allowed_tools", opts.allowed_tools.clone());
        set("bare", opts.bare.then(|| "true".into()));
        set("relayed", opts.relayed.then(|| "true".into()));
        set("provider", Some(at.provider.name.into()));
        set("provider_key_env", Some(at.provider.key_env.into()));
        set(
            "provider_base_url_env",
            Some(at.provider.base_url_env.into()),
        );
        set("api_prefix", Some(at.provider.api_prefix.into()));
        set("root", at.root.clone());
        let endpoint = at
            .root
            .clone()
            .unwrap_or_else(|| format!("{}://{}", at.scheme.as_str(), at.host));
        set("endpoint", Some(endpoint));
        set(PROTOCOL_VAR, self.protocol(at.provider).map(String::from));
        vars
    }

    /// Refuses a combination this agent cannot serve, before anything runs.
    pub fn check(&self, opts: &Options, provider: &Provider) -> Result<()> {
        if self.protocol(provider).is_none() {
            return Err(Error::new(format!(
                "{} cannot talk to the {} provider: it speaks {}. Choose another agent or provider.",
                self.name,
                provider.name,
                self.protocols.join(", ")
            )));
        }
        if opts.relayed
            && let Some(why) = &self.relay_refusal
        {
            return Err(Error::new(why.clone()));
        }
        for (name, var) in &self.vars {
            if var.by_provider && !var.map.contains_key(provider.name) && !var.map.contains_key("*")
            {
                return Err(Error::new(format!(
                    "{} has no {name} for the {} provider",
                    self.name, provider.name
                )));
            }
        }
        let set = |name: &str| match name {
            "model" => opts.model.is_some(),
            "effort" => opts.effort.is_some(),
            "max_turns" => opts.max_turns.is_some(),
            "permission_mode" => opts.permission_mode.is_some(),
            "allowed_tools" => opts.allowed_tools.is_some(),
            _ => opts.bare,
        };
        // An error names its option as a `&'static str`.
        let option = |name: &str| {
            OPTIONS
                .into_iter()
                .find(|o| *o == name)
                .expect("load refuses a name not in OPTIONS")
        };
        // Dropping one of these silently would weaken a restriction the caller asked for.
        if let Some(name) = self.refuse.iter().find(|n| set(n)) {
            return Err(Error::about(
                option(name),
                format!("has no {} equivalent", self.name),
            ));
        }
        if let Some(name) = self.require.iter().find(|n| !set(n)) {
            return Err(Error::about(
                option(name),
                format!("is required with agent {}", self.name),
            ));
        }
        Ok(())
    }

    fn with_vars(&self, opts: &Options, at: &Endpoint) -> Vars {
        let mut vars = self.base_vars(opts, at);
        let protocol = vars.get(PROTOCOL_VAR).cloned().unwrap_or_default();
        let resolved: Vec<(String, String)> = self
            .vars
            .iter()
            .filter_map(|(name, var)| {
                let key = if var.by_provider {
                    at.provider.name
                } else {
                    protocol.as_str()
                };
                let template = var.map.get(key).or_else(|| var.map.get("*"))?;
                Some((name.clone(), template.render(&vars)?))
            })
            .collect();
        vars.extend(resolved);
        vars
    }

    /// Where the agent finds its endpoint and credential.
    pub fn wire(&self, opts: &Options, at: &Endpoint) -> Wiring {
        let mut vars = self.with_vars(opts, at);
        let key_env = opts
            .key_env
            .clone()
            .or_else(|| self.key_env.render(&vars))
            .unwrap_or_default();
        let base_url_env = opts
            .base_url_env
            .clone()
            .or_else(|| self.base_url_env.render(&vars))
            .unwrap_or_default();
        let base_url = self.base_url.as_ref().and_then(|t| t.render(&vars));
        vars.insert("key_env".into(), key_env.clone());
        vars.insert("base_url_env".into(), base_url_env.clone());
        if let Some(url) = &base_url {
            vars.insert("base_url".into(), url.clone());
        }
        let env = self
            .env
            .iter()
            .filter_map(|(name, value)| {
                let rendered = match value {
                    EnvValue::Text(t) => t.render(&vars)?,
                    EnvValue::Json(doc) => serde_json::to_string(&render_json(doc, &vars)?).ok()?,
                };
                Some((name.clone(), rendered))
            })
            .collect();
        Wiring {
            key_env,
            base_url_env,
            base_url,
            env,
        }
    }

    /// The command after the image. Nothing secret belongs in it: the credential reaches the
    /// container through `Wiring::key_env`, and this argv is visible to `inspect`.
    pub fn argv(
        &self,
        opts: &Options,
        at: &Endpoint,
        task: &str,
        wiring: &Wiring,
    ) -> Result<Vec<String>> {
        let mut vars = self.with_vars(opts, at);
        vars.insert("task".into(), task.to_string());
        vars.insert("key_env".into(), wiring.key_env.clone());
        vars.insert("base_url_env".into(), wiring.base_url_env.clone());
        if let Some(url) = &wiring.base_url {
            vars.insert("base_url".into(), url.clone());
        }
        let mut argv = Vec::new();
        for item in &self.argv {
            match item {
                Item::Arg(t) => argv.push(t.render(&vars).ok_or_else(|| {
                    let unset: Vec<&str> = t.names().filter(|n| !vars.contains_key(*n)).collect();
                    Error::new(format!(
                        "agent {}: argv needs {}, which this run does not set",
                        self.name,
                        unset.join(", ")
                    ))
                })?),
                Item::Group {
                    args,
                    when,
                    otherwise,
                } => {
                    let wanted = when.as_ref().is_none_or(|w| vars.contains_key(w));
                    let rendered: Option<Vec<String>> =
                        args.iter().map(|t| t.render(&vars)).collect();
                    match rendered.filter(|_| wanted) {
                        Some(args) => argv.extend(args),
                        None => {
                            for t in otherwise {
                                argv.extend(t.render(&vars));
                            }
                        }
                    }
                }
            }
        }
        Ok(argv)
    }

    pub fn reader(&self) -> Box<dyn stream::Reader> {
        stream::reader(&self.stream).expect("checked when the spec was read")
    }
}
