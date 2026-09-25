//! Recipes: JSON descriptions of an agent image, rendered to one Containerfile.
//!
//! A recipe names a base image, an agent, the agent's account, install sections and kits. It may
//! inherit from other recipes by name, left to right, and drop what it inherits with `remove`.
//! Parent sections come first, because a child step may need a parent's packages.
//!
//! Parents are not pinned; kits are. A kit whose `kit.json` no longer matches the recipe's
//! `sha256` stops the build. The image tag is the recipe's name and a hash of everything the build
//! reads, so a changed recipe, parent or kit builds a new image. The rendering matches Python
//! sanduk byte for byte, so the two share images.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use sanduk_container::{AGENT_UID_LABEL, CONTAINER_PREFIX, Engine};

use crate::catalog::{self, Kind, NAME_RE, SHA256_RE};
use crate::error::{Error, Result};
use crate::kits::{self, Kit};
use crate::sections::{
    self, Context, EnvMap, Section, TYPES, USER_RE, exec_form, fail, matching, path_value, render,
    strings, text,
};
use crate::util::{note, py_json_list, random_hex};

const RECIPE_KEYS: [&str; 13] = [
    "name",
    "description",
    "inherits",
    "agent",
    "from",
    "user",
    "home",
    "sections",
    "kits",
    "remove",
    "env",
    "entrypoint",
    "instructions",
];
const REMOVE_KEYS: [&str; 4] = ["kits", "sections", "env", "section_types"];

fn is_image_ref(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "._/:@-".contains(c))
}

#[derive(Debug, Clone)]
pub struct KitUse {
    pub kit: Kit,
    /// `None`: named on the command line, unpinned.
    pub pin: Option<String>,
    pub pinned_by: String,
}

/// Standing instructions for the agent, and the recipe that set them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    pub owner: String,
    pub text: String,
}

/// A recipe with its parents merged in.
#[derive(Debug, Clone, Default)]
pub struct Recipe {
    pub name: String,
    pub path: Option<PathBuf>,
    pub description: String,
    pub agent: String,
    /// `from`.
    pub base: String,
    pub user: String,
    pub home: String,
    pub entrypoint: Vec<String>,
    pub sections: Vec<Section>,
    pub kits: Vec<KitUse>,
    pub env: EnvMap,
    /// Parents first: a child appends to what it inherits.
    pub instructions: Vec<Instructions>,
}

impl Recipe {
    /// The resolved recipe, as `build --dry-run` prints it.
    pub fn as_json(&self) -> Value {
        json!({
            "name": self.name,
            "agent": self.agent,
            "from": self.base,
            "user": self.user,
            "home": self.home,
            "sections": self.sections.iter().map(|s| Value::Object(s.raw.clone())).collect::<Vec<_>>(),
            "kits": self.kits.iter().map(|u| json!({
                "name": u.kit.name, "sha256": u.kit.sha256, "pinned": u.pin.is_some(),
            })).collect::<Vec<_>>(),
            "env": self.env.to_json(),
            "entrypoint": self.entrypoint,
            "instructions": self.instructions.iter().map(|i| json!({"recipe": i.owner, "text": i.text})).collect::<Vec<_>>(),
        })
    }
}

// --- reading --------------------------------------------------------------------------------------

/// One recipe file as written, before inheritance.
struct File {
    name: String,
    inherits: Vec<String>,
    remove: BTreeMap<String, Vec<String>>,
    recipe: Recipe,
}

fn read(path: &Path) -> Result<File> {
    let (_, data) = catalog::read_json(path, &RECIPE_KEYS)?;
    let where_ = &path.display().to_string();
    let dir = path.parent().expect("a file has a directory");
    let stem = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let name = matching(data.get("name"), &NAME_RE, where_, "name")?.to_string();
    if name != stem {
        return Err(fail(
            where_,
            format!("name {name:?} must equal the file name, {stem:?}"),
        ));
    }

    let inherits = match data.get("inherits") {
        None => Vec::new(),
        Some(Value::String(_)) => vec![text(data.get("inherits"), where_, "inherits")?.to_string()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|p| text(Some(p), where_, "inherits").map(String::from))
            .collect::<Result<_>>()?,
        Some(_) => return Err(fail(where_, "inherits must be a name or a list of names")),
    };

    let mut recipe = Recipe {
        name: name.clone(),
        path: Some(path.to_path_buf()),
        ..Recipe::default()
    };
    if data.contains_key("description") {
        recipe.description = text(data.get("description"), where_, "description")?.into();
    }
    if data.contains_key("agent") {
        recipe.agent = matching(data.get("agent"), &NAME_RE, where_, "agent")?.into();
    }
    if data.contains_key("from") {
        recipe.base = text(data.get("from"), where_, "from")?.into();
        if !is_image_ref(&recipe.base) {
            return Err(fail(
                where_,
                format!("from {:?} is not an image reference", recipe.base),
            ));
        }
    }
    if data.contains_key("user") {
        recipe.user = matching(data.get("user"), &USER_RE, where_, "user")?.into();
    }
    if data.contains_key("home") {
        recipe.home = path_value(data.get("home"), where_, "home", true)?.into();
    }
    if data.contains_key("entrypoint") {
        recipe.entrypoint = strings(data.get("entrypoint"), where_, "entrypoint")?;
    }
    recipe.sections = match data.get("sections") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|s| sections::section(s, where_, dir))
            .collect::<Result<_>>()?,
        Some(_) => return Err(fail(where_, "sections must be a list")),
    };
    kits::unique(
        recipe.sections.iter().map(|s| s.name.as_str()),
        where_,
        "section",
    )?;
    recipe.env = sections::env_map(data.get("env"), where_)?;
    if let Some(value) = data.get("instructions") {
        recipe.instructions = vec![Instructions {
            owner: name.clone(),
            text: read_instructions(value, where_, dir)?,
        }];
    }
    recipe.kits = match data.get("kits") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|e| kit_entry(e, where_, dir, &name))
            .collect::<Result<_>>()?,
        Some(_) => return Err(fail(where_, "kits must be a list")),
    };
    kits::unique(
        recipe.kits.iter().map(|u| u.kit.name.as_str()),
        where_,
        "kit",
    )?;

    let remove = match data.get("remove") {
        None => Map::new(),
        Some(Value::Object(remove)) => remove.clone(),
        Some(_) => return Err(fail(where_, "remove must be an object")),
    };
    if remove.keys().any(|k| !REMOVE_KEYS.contains(&k.as_str())) {
        let mut allowed = REMOVE_KEYS.to_vec();
        allowed.sort_unstable();
        return Err(fail(where_, format!("remove takes {}", allowed.join(", "))));
    }
    let mut removals = BTreeMap::new();
    for (key, value) in &remove {
        removals.insert(
            key.clone(),
            strings(Some(value), where_, &format!("remove.{key}"))?,
        );
    }
    for t in removals.get("section_types").into_iter().flatten() {
        if !TYPES.contains(&t.as_str()) {
            return Err(fail(
                where_,
                format!("remove.section_types: {t:?} is not a section type"),
            ));
        }
    }
    let added: [(&str, Vec<&str>); 3] = [
        (
            "sections",
            recipe.sections.iter().map(|s| s.name.as_str()).collect(),
        ),
        (
            "kits",
            recipe.kits.iter().map(|u| u.kit.name.as_str()).collect(),
        ),
        ("env", recipe.env.keys().collect()),
    ];
    for (key, added) in added {
        let mut both: Vec<&str> = removals
            .get(key)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|n| added.contains(n))
            .collect();
        both.sort_unstable();
        if !both.is_empty() {
            return Err(fail(
                where_,
                format!(
                    "remove.{key} names {}, which this recipe also adds; defining it again already \
                     replaces the inherited one",
                    both.join(", ")
                ),
            ));
        }
    }
    Ok(File {
        name,
        inherits,
        remove: removals,
        recipe,
    })
}

/// A path relative to the recipe, or `{"text": ...}`. Never guessed between.
fn read_instructions(value: &Value, where_: &str, base: &Path) -> Result<String> {
    let body = match value {
        Value::String(_) => {
            let rel = path_value(Some(value), where_, "instructions", false)?;
            let target = base.join(rel);
            let inside = std::fs::canonicalize(&target)
                .ok()
                .zip(std::fs::canonicalize(base).ok())
                .is_some_and(|(t, b)| t.starts_with(b));
            if target.is_symlink() || (target.exists() && !inside) {
                return Err(fail(
                    where_,
                    format!("instructions {rel:?} leaves {}", base.display()),
                ));
            }
            if !target.is_file() {
                return Err(fail(
                    where_,
                    format!("instructions {rel:?} is not a file in {}", base.display()),
                ));
            }
            std::fs::read_to_string(&target)?
        }
        Value::Object(map) if map.len() == 1 && map.get("text").is_some_and(Value::is_string) => {
            map["text"].as_str().unwrap_or_default().to_string()
        }
        _ => {
            return Err(fail(
                where_,
                "instructions must be a path, or {\"text\": \"...\"}",
            ));
        }
    };
    if body.trim().is_empty() {
        return Err(fail(where_, "instructions must be non-empty"));
    }
    Ok(body)
}

fn kit_entry(raw: &Value, where_: &str, dir: &Path, owner: &str) -> Result<KitUse> {
    let shape = "each kit is {\"name\": ..., \"sha256\": ...} or {\"path\": ..., \"sha256\": ...}";
    let Some(raw) = raw
        .as_object()
        .filter(|m| m.len() == 2 && m.contains_key("sha256"))
    else {
        return Err(fail(where_, shape));
    };
    let pin = matching(raw.get("sha256"), &SHA256_RE, where_, "kit sha256")?.to_string();
    let kit = if raw.contains_key("name") {
        kits::load(
            matching(raw.get("name"), &NAME_RE, where_, "kit name")?,
            None,
        )?
    } else if raw.contains_key("path") {
        let spec = text(raw.get("path"), where_, "kit path")?;
        let spec = if catalog::is_path(spec) {
            spec.to_string()
        } else {
            format!("./{spec}")
        };
        kits::load(&spec, Some(dir))?
    } else {
        return Err(fail(where_, "a kit entry needs name or path"));
    };
    Ok(KitUse {
        kit,
        pin: Some(pin),
        pinned_by: owner.to_string(),
    })
}

// --- inheritance ----------------------------------------------------------------------------------

/// Replaces an item of the same name in place, or appends.
fn replace_by_name<T: Clone>(old: &[T], new: &[T], key: impl Fn(&T) -> String) -> Vec<T> {
    let mut out = old.to_vec();
    for item in new {
        match out.iter().position(|o| key(o) == key(item)) {
            Some(i) => out[i] = item.clone(),
            None => out.push(item.clone()),
        }
    }
    out
}

/// Applies `layer` over `into`: scalars replace, named things replace in place.
fn merge(into: &mut Recipe, layer: &Recipe) {
    for (target, value) in [
        (&mut into.agent, &layer.agent),
        (&mut into.base, &layer.base),
        (&mut into.user, &layer.user),
        (&mut into.home, &layer.home),
    ] {
        if !value.is_empty() {
            *target = value.clone();
        }
    }
    if !layer.entrypoint.is_empty() {
        into.entrypoint = layer.entrypoint.clone();
    }
    into.sections = replace_by_name(&into.sections, &layer.sections, |s| s.name.clone());
    into.kits = replace_by_name(&into.kits, &layer.kits, |u| u.kit.name.clone());
    for (k, v) in layer.env.iter() {
        into.env.insert(k.to_string(), v.to_string());
    }
    // Keyed by owner, so a recipe reached twice through inheritance counts once.
    into.instructions =
        replace_by_name(&into.instructions, &layer.instructions, |i| i.owner.clone());
}

fn apply_remove(
    merged: &mut Recipe,
    remove: &BTreeMap<String, Vec<String>>,
    where_: &str,
) -> Result<()> {
    let gone = |key: &str, present: Vec<&str>| -> Result<Vec<String>> {
        let asked = remove.get(key).cloned().unwrap_or_default();
        let mut missing: Vec<&str> = asked
            .iter()
            .map(String::as_str)
            .filter(|a| !present.contains(a))
            .collect();
        missing.sort_unstable();
        if !missing.is_empty() {
            return Err(fail(
                where_,
                format!(
                    "remove.{key}: nothing inherited is named {}",
                    missing.join(", ")
                ),
            ));
        }
        Ok(asked)
    };
    let drop = gone(
        "sections",
        merged.sections.iter().map(|s| s.name.as_str()).collect(),
    )?;
    let types = remove.get("section_types").cloned().unwrap_or_default();
    let mut absent: Vec<&str> = types
        .iter()
        .map(String::as_str)
        .filter(|t| !merged.sections.iter().any(|s| s.kind == *t))
        .collect();
    absent.sort_unstable();
    absent.dedup();
    if !absent.is_empty() {
        return Err(fail(
            where_,
            format!(
                "remove.section_types: no inherited section is {}",
                absent.join(", ")
            ),
        ));
    }
    merged
        .sections
        .retain(|s| !drop.contains(&s.name) && !types.contains(&s.kind));
    let kit_drop = gone(
        "kits",
        merged.kits.iter().map(|u| u.kit.name.as_str()).collect(),
    )?;
    merged.kits.retain(|u| !kit_drop.contains(&u.kit.name));
    let env_drop = gone("env", merged.env.keys().collect())?;
    for key in env_drop {
        merged.env.remove(&key);
    }
    Ok(())
}

/// A recipe by name or path, with its parents merged and its kit pins checked. `extra_kits` are
/// added unpinned, as `--kit` on the command line.
pub fn resolve(spec: &str, extra_kits: &[String]) -> Result<Recipe> {
    let (path, _) = catalog::locate(Kind::Recipes, spec, None)?;
    let mut recipe = resolve_path(&path, &[])?;
    for kit_spec in extra_kits {
        let kit = kits::load(kit_spec, None)?;
        note(&format!(
            "kit {} is unpinned here; its sha256 is {}",
            kit.name, kit.sha256
        ));
        let used = KitUse {
            kit,
            pin: None,
            pinned_by: String::new(),
        };
        recipe.kits = replace_by_name(&recipe.kits, &[used], |u| u.kit.name.clone());
    }

    let where_ = format!("recipe {}", recipe.name);
    for (value, key) in [
        (&recipe.agent, "agent"),
        (&recipe.base, "from"),
        (&recipe.user, "user"),
        (&recipe.home, "home"),
    ] {
        if value.is_empty() {
            return Err(fail(
                &where_,
                format!("no {key}, in the recipe or anything it inherits"),
            ));
        }
    }
    if recipe.entrypoint.is_empty() {
        return Err(fail(
            &where_,
            "no entrypoint, in the recipe or anything it inherits",
        ));
    }
    for used in &recipe.kits {
        if let Some(pin) = &used.pin
            && *pin != used.kit.sha256
        {
            return Err(Error::new(format!(
                "recipe {} pins kit {} at {pin}, but {} is {}. Check what changed, then update the pin",
                used.pinned_by,
                used.kit.name,
                used.kit.path.display(),
                used.kit.sha256
            )));
        }
    }
    let mut providers: BTreeMap<&str, &str> = BTreeMap::new();
    let mut skills: BTreeMap<&str, &str> = BTreeMap::new();
    for used in &recipe.kits {
        for cap in &used.kit.provides {
            if let Some(other) = providers.insert(cap, &used.kit.name) {
                return Err(fail(
                    &where_,
                    format!("kits {other} and {} both provide {cap}", used.kit.name),
                ));
            }
        }
        for s in &used.kit.skills {
            if let Some(other) = skills.insert(&s.name, &used.kit.name) {
                return Err(fail(
                    &where_,
                    format!(
                        "kits {other} and {} both carry skill {}",
                        used.kit.name, s.name
                    ),
                ));
            }
        }
    }
    Ok(recipe)
}

fn resolve_path(path: &Path, chain: &[(PathBuf, String)]) -> Result<Recipe> {
    let file = read(path)?;
    let mut chain = chain.to_vec();
    chain.push((path.to_path_buf(), file.name.clone()));
    let mut merged = Recipe {
        name: file.name.clone(),
        path: Some(path.to_path_buf()),
        ..Recipe::default()
    };
    for parent_spec in &file.inherits {
        let (parent, _) = catalog::locate(Kind::Recipes, parent_spec, path.parent())?;
        if chain.iter().any(|(p, _)| *p == parent) {
            let mut names: Vec<String> = chain.iter().map(|(_, n)| n.clone()).collect();
            names.push(read(&parent)?.name);
            return Err(Error::new(format!(
                "recipe inheritance cycle: {}",
                names.join(" -> ")
            )));
        }
        let parent = resolve_path(&parent, &chain)?;
        merge(&mut merged, &parent);
    }
    apply_remove(&mut merged, &file.remove, &path.display().to_string())?;
    merge(&mut merged, &file.recipe);
    merged.description = file.recipe.description;
    Ok(merged)
}

// --- rendering ------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct Rendered {
    pub containerfile: String,
    pub files: BTreeMap<String, Vec<u8>>,
}

/// The architecture both engines build for on this host.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Refuses what would fail late: an agent a kit cannot serve, a missing build.
pub fn check(
    recipe: &Recipe,
    agent: &str,
    skills_dir: Option<&str>,
    arch: &str,
    instructions_file: Option<&str>,
) -> Result<()> {
    if recipe.agent != agent {
        return Err(Error::new(format!(
            "recipe {} builds {}, not {agent}",
            recipe.name, recipe.agent
        )));
    }
    if !recipe.instructions.is_empty() && instructions_file.is_none() {
        return Err(Error::new(format!(
            "recipe {} has instructions, and sanduk does not know where {agent} reads them. They \
             would be installed with nothing reading them",
            recipe.name
        )));
    }
    for used in &recipe.kits {
        kits::check(&used.kit, agent, skills_dir)?;
    }
    let owned = recipe
        .sections
        .iter()
        .map(|s| (recipe.name.clone(), s))
        .chain(recipe.kits.iter().flat_map(|u| {
            u.kit
                .tools
                .iter()
                .map(move |t| (format!("kit {}", u.kit.name), t))
        }));
    for (owner, s) in owned {
        if (s.kind == "binary" || s.kind == "archive") && !s.has_artifact(arch) {
            return Err(Error::new(format!(
                "{owner}: section {} has no {arch} artifact",
                s.name
            )));
        }
    }
    Ok(())
}

/// One Containerfile and the build-context files it copies.
pub fn render_recipe(
    recipe: &Recipe,
    skills_dir: Option<&str>,
    instructions_file: Option<&str>,
) -> Result<Rendered> {
    let mut ctx = Context::default();
    let env = merged_env(recipe)?;
    let mut lines = vec![
        format!(
            "# Rendered by sanduk from recipe {}. Edit the recipe instead.",
            recipe.name
        ),
        format!("FROM {}", recipe.base),
    ];
    let block = |lines: &mut Vec<String>, new: Vec<String>| {
        lines.push(String::new());
        lines.extend(new);
    };

    for s in recipe.sections.iter().filter(|s| !s.as_agent()) {
        block(&mut lines, render(s, &recipe.name, &mut ctx)?);
    }
    for used in &recipe.kits {
        for t in used.kit.tools.iter().filter(|t| !t.as_agent()) {
            block(
                &mut lines,
                render(t, &format!("kit-{}", used.kit.name), &mut ctx)?,
            );
        }
    }

    let (user, home) = (&recipe.user, &recipe.home);
    block(
        &mut lines,
        vec![
            "# The agent's uid and gid. Docker passes the caller's, so the agent can".into(),
            "# write a bind mount the host user owns.".into(),
            "ARG AGENT_UID=1000".into(),
            "ARG AGENT_GID=1000".into(),
            format!("LABEL {AGENT_UID_LABEL}=$AGENT_UID"),
            format!("RUN if id -u {user} >/dev/null 2>&1; then \\"),
            format!("      groupmod -o -g \"$AGENT_GID\" \"$(id -gn {user})\" \\"),
            format!("      && usermod -o -u \"$AGENT_UID\" -g \"$AGENT_GID\" {user}; \\"),
            "    else \\".into(),
            format!("      groupadd -o -g \"$AGENT_GID\" {user} \\"),
            format!("      && useradd -o --create-home --home-dir {home} \\"),
            format!("         --uid \"$AGENT_UID\" --gid \"$AGENT_GID\" {user}; \\"),
            "    fi \\".into(),
            " && mkdir -p /work && chown \"$AGENT_UID:$AGENT_GID\" /work".into(),
        ],
    );

    if let Some(skills_dir) = skills_dir
        && recipe.kits.iter().any(|u| !u.kit.skills.is_empty())
    {
        block(
            &mut lines,
            vec![format!(
                "RUN install -d -o \"$AGENT_UID\" -g \"$AGENT_GID\" {}",
                owned_dirs(home, skills_dir.split('/'))
            )],
        );
        for used in recipe.kits.iter().filter(|u| !u.kit.skills.is_empty()) {
            block(
                &mut lines,
                kits::render_skills(&used.kit, &format!("{home}/{skills_dir}"), &mut ctx)?,
            );
        }
    }

    if let Some(instructions_file) = instructions_file
        && !recipe.instructions.is_empty()
    {
        block(
            &mut lines,
            render_instructions(recipe, &format!("{home}/{instructions_file}"), &mut ctx)?,
        );
    }

    let mut final_env = EnvMap::new();
    final_env.insert("HOME".into(), home.clone());
    for (k, v) in env.iter() {
        final_env.insert(k.to_string(), v.to_string());
    }
    block(
        &mut lines,
        vec![format!("USER {user}"), env_instruction(&final_env)],
    );
    for used in &recipe.kits {
        for t in used.kit.tools.iter().filter(|t| t.as_agent()) {
            block(
                &mut lines,
                render(t, &format!("kit-{}", used.kit.name), &mut ctx)?,
            );
        }
        let setup = kits::render_setup(&used.kit, &recipe.agent);
        if !setup.is_empty() {
            let mut steps = vec![format!(
                "# kit-{}: setup for {}",
                used.kit.name, recipe.agent
            )];
            steps.extend(setup);
            block(&mut lines, steps);
        }
    }
    for s in recipe.sections.iter().filter(|s| s.as_agent()) {
        block(&mut lines, render(s, &recipe.name, &mut ctx)?);
    }

    let kit_label: Vec<String> = recipe
        .kits
        .iter()
        .map(|u| format!("{}@{}", u.kit.name, u.kit.sha256))
        .collect();
    block(
        &mut lines,
        vec![
            "WORKDIR /work".into(),
            format!(
                "LABEL sanduk.recipe=\"{}\" sanduk.kits=\"{}\"",
                recipe.name,
                kit_label.join(",")
            ),
            format!("ENTRYPOINT {}", exec_form(&recipe.entrypoint)),
        ],
    );
    Ok(Rendered {
        containerfile: lines.join("\n") + "\n",
        files: ctx.files,
    })
}

/// `home/a home/a/b ...`: each directory down to the last part, which the agent owns.
fn owned_dirs<'a>(home: &str, parts: impl Iterator<Item = &'a str>) -> String {
    let parts: Vec<&str> = parts.collect();
    (1..=parts.len())
        .map(|i| format!("{home}/{}", parts[..i].join("/")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The instructions as one read-only file at `dest`, in an agent-owned directory: the agent keeps
/// its own state beside the file, e.g. Claude Code under ~/.claude.
fn render_instructions(recipe: &Recipe, dest: &str, ctx: &mut Context) -> Result<Vec<String>> {
    let rel_dest = &dest[recipe.home.len() + 1..];
    let parents: Vec<&str> = rel_dest.split('/').collect();
    let parents = &parents[..parents.len() - 1];
    let body = recipe
        .instructions
        .iter()
        .map(|i| i.text.trim_matches('\n'))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n";
    let file_name = dest.rsplit('/').next().unwrap_or(dest);
    let rel = ctx.add(
        format!("instructions/{}/{file_name}", recipe.name),
        body.into_bytes(),
    )?;
    let owners: Vec<&str> = recipe
        .instructions
        .iter()
        .map(|i| i.owner.as_str())
        .collect();
    let mut lines = vec![format!("# instructions from {}", owners.join(", "))];
    if !parents.is_empty() {
        lines.push(format!(
            "RUN install -d -o \"$AGENT_UID\" -g \"$AGENT_GID\" {}",
            owned_dirs(&recipe.home, parents.iter().copied())
        ));
    }
    lines.push(format!("COPY {rel} {dest}"));
    lines.push(format!("RUN chmod a=r {dest}"));
    Ok(lines)
}

/// Kit env under recipe env. Two kits disagreeing on a key is refused; the recipe composes them,
/// so its own value wins.
fn merged_env(recipe: &Recipe) -> Result<EnvMap> {
    let mut env = EnvMap::new();
    let mut source: BTreeMap<String, String> = BTreeMap::new();
    for used in &recipe.kits {
        for (key, value) in used.kit.env.iter() {
            if env.get(key).is_some_and(|v| v != value) && !recipe.env.contains(key) {
                return Err(Error::new(format!(
                    "recipe {}: kits {} and {} set {key} differently; set it in the recipe to choose",
                    recipe.name, source[key], used.kit.name
                )));
            }
            env.insert(key.to_string(), value.to_string());
            source.insert(key.to_string(), used.kit.name.clone());
        }
    }
    for (k, v) in recipe.env.iter() {
        env.insert(k.to_string(), v.to_string());
    }
    Ok(env)
}

fn env_instruction(env: &EnvMap) -> String {
    let quoted = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
    let pairs: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", quoted(v)))
        .collect();
    format!("ENV {}", pairs.join(" \\\n    "))
}

/// `sanduk-<recipe>:<12 hex>`, over everything the build reads.
pub fn image_tag(recipe: &Recipe, rendered: &Rendered, build_args: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(rendered.containerfile.as_bytes());
    for (rel, data) in &rendered.files {
        h.update(b"\0");
        h.update(rel.as_bytes());
        h.update(b"\0");
        h.update(Sha256::digest(data));
    }
    h.update(b"\0");
    h.update(py_json_list(build_args).as_bytes());
    format!(
        "{CONTAINER_PREFIX}{}:{}",
        recipe.name,
        &format!("{:x}", h.finalize())[..12]
    )
}

pub fn repository(recipe_name: &str) -> String {
    format!("{CONTAINER_PREFIX}{recipe_name}")
}

/// Writes the context to a scratch directory and builds it.
pub fn build(engine: &Engine, image: &str, rendered: &Rendered) -> Result<()> {
    let scratch = std::env::temp_dir().join(format!("sanduk-build-{}", random_hex(12)?));
    let written = (|| -> Result<()> {
        std::fs::create_dir_all(&scratch)?;
        std::fs::write(scratch.join("Containerfile"), &rendered.containerfile)?;
        for (rel, data) in &rendered.files {
            let target = scratch.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, data)?;
        }
        Ok(engine.build_image(image, &scratch.join("Containerfile"))?)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    written
}
