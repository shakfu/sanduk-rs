//! `{name}` placeholders in an agent spec, with `|upper` and `|json` filters and `{{`/`}}` for a
//! literal brace. Parsed when the spec is read, so a misspelt name fails at load, not mid-run.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Var {
        name: String,
        filter: Option<Filter>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    Upper,
    /// A JSON string literal, quotes included. Also a valid TOML basic string, which is what
    /// codex's `-c key=value` takes.
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

/// Values by name. A name that is absent is unset.
pub type Vars = BTreeMap<String, String>;

impl Template {
    pub fn parse(text: &str) -> Result<Self> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    literal.push('}');
                }
                '{' => {
                    let mut inner = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) => inner.push(c),
                            None => return Err(Error::new(format!("unclosed {{ in {text:?}"))),
                        }
                    }
                    let (name, filter) = match inner.split_once('|') {
                        None => (inner.as_str(), None),
                        Some((name, "upper")) => (name, Some(Filter::Upper)),
                        Some((name, "json")) => (name, Some(Filter::Json)),
                        Some((_, other)) => {
                            return Err(Error::new(format!(
                                "unknown filter {other:?} in {text:?}; known: upper, json"
                            )));
                        }
                    };
                    let valid = !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                    if !valid {
                        return Err(Error::new(format!(
                            "{{{inner}}} in {text:?} is not a variable"
                        )));
                    }
                    if !literal.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut literal)));
                    }
                    parts.push(Part::Var {
                        name: name.to_string(),
                        filter,
                    });
                }
                '}' => {
                    return Err(Error::new(format!(
                        "a lone }} in {text:?}; write }}}} for a literal one"
                    )));
                }
                c => literal.push(c),
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Text(literal));
        }
        Ok(Template { parts })
    }

    /// Every variable this template reads.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|p| match p {
            Part::Var { name, .. } => Some(name.as_str()),
            Part::Text(_) => None,
        })
    }

    /// The text, or `None` when a variable it reads is unset.
    pub fn render(&self, vars: &Vars) -> Option<String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Var { name, filter } => {
                    let value = vars.get(name)?;
                    match filter {
                        None => out.push_str(value),
                        Some(Filter::Upper) => out.push_str(&value.to_uppercase()),
                        Some(Filter::Json) => {
                            out.push_str(&serde_json::Value::String(value.clone()).to_string())
                        }
                    }
                }
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn placeholders_filters_and_literal_braces() {
        let t = Template::parse("HAX_{family|upper}_KEY {{env:{key}}} {v|json}").unwrap();
        let v = vars(&[("family", "openai"), ("key", "K"), ("v", "a\"b")]);
        assert_eq!(
            t.render(&v).as_deref(),
            Some("HAX_OPENAI_KEY {env:K} \"a\\\"b\"")
        );
        assert_eq!(t.names().collect::<Vec<_>>(), ["family", "key", "v"]);
    }

    #[test]
    fn an_unset_variable_leaves_nothing_to_render() {
        let t = Template::parse("--model={model}").unwrap();
        assert_eq!(t.render(&Vars::new()), None);
        assert_eq!(
            Template::parse("plain")
                .unwrap()
                .render(&Vars::new())
                .as_deref(),
            Some("plain")
        );
    }

    #[test]
    fn malformed_templates_are_refused() {
        for bad in ["{unclosed", "lone }", "{Model}", "{x|lower}", "{}"] {
            assert!(Template::parse(bad).is_err(), "{bad}");
        }
    }
}
