//! How an agent's output is read: one reader per stream format.
//!
//! These are code, not configuration: each is a small state machine (a tally across events, a
//! retry that clears an earlier failure, a prose section read line by line), and a rule language
//! able to express them would be an interpreter to maintain. An agent spec names its format;
//! a new agent speaking an existing format needs no code.
//!
//! Token accounting is per format, not per provider: Claude Code reports cache reads outside
//! `input_tokens` and hax inside it, so a shared summary would miscount one of them.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

/// A finished run, in terms the CLI prints without knowing the agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    /// The final assistant message, when the agent reports one.
    pub text: String,
    pub error: String,
    /// One line: turns, tokens, cost. A line rather than a struct: there is no shared meaning
    /// across agents to normalise to.
    pub stats: String,
}

/// Where trace lines go. The caller drops them under `--quiet`.
pub type Trace<'a> = &'a mut dyn FnMut(String);

/// One run's output, consumed as it arrives.
pub trait Reader {
    /// Consumes one JSON record.
    fn event(&mut self, record: &Map<String, Value>, trace: Trace);
    /// Consumes one line that is not a JSON record. Only hermes has any.
    fn line(&mut self, _text: &str, _trace: Trace) {}
    /// The run's outcome, or `None` when no terminal record arrived.
    fn finish(&self) -> Option<Outcome>;
}

pub const FORMATS: [&str; 7] = [
    "claude", "codex", "hax", "hermes", "minima", "opencode", "pi",
];

pub fn reader(format: &str) -> Option<Box<dyn Reader>> {
    Some(match format {
        "claude" => Box::new(Claude::default()),
        "codex" => Box::new(Codex::default()),
        "hax" => Box::new(Hax::default()),
        "hermes" => Box::new(Hermes::default()),
        "minima" => Box::new(Minima::default()),
        "opencode" => Box::new(OpenCode::default()),
        "pi" => Box::new(Pi::default()),
        _ => return None,
    })
}

// --- helpers --------------------------------------------------------------------------------------

fn get<'a>(v: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    v.get(key)
}

fn obj(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

fn text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

fn int(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

fn float(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

/// A value as printed, or `?` when absent.
fn shown(v: Option<&Value>) -> String {
    match v {
        None => "?".into(),
        Some(v) => text(Some(v)),
    }
}

/// The first 160 characters of the stripped text, as the trace shows it.
fn clip(s: &str) -> String {
    s.trim().chars().take(160).collect()
}

/// `12,345`, as Python's `{:,}`.
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[derive(Debug, Default, Clone, Copy)]
struct Tokens {
    input: i64,
    output: i64,
    cached: i64,
}

impl Tokens {
    fn line(&self) -> String {
        format!(
            "{} in ({} cached) / {} out",
            thousands(self.input),
            thousands(self.cached),
            thousands(self.output)
        )
    }
}

// --- claude ---------------------------------------------------------------------------------------

/// `claude -p --output-format stream-json`: one `result` record at the end.
#[derive(Default)]
struct Claude {
    result: Option<Map<String, Value>>,
}

impl Reader for Claude {
    fn event(&mut self, record: &Map<String, Value>, trace: Trace) {
        let kind = get(record, "type").and_then(Value::as_str);
        let blocks = || {
            obj(get(record, "message"))
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        match kind {
            Some("result") => self.result = Some(record.clone()),
            Some("assistant") => {
                for b in blocks() {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") if !text(b.get("text")).trim().is_empty() => {
                            trace(format!("  . {}", clip(&text(b.get("text")))));
                        }
                        Some("tool_use") => trace(format!("  > {}", text(b.get("name")))),
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for b in blocks() {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result")
                        && b.get("is_error").is_some_and(|e| e.as_bool() == Some(true))
                    {
                        trace("  ! tool error".into());
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(&self) -> Option<Outcome> {
        let result = self.result.as_ref()?;
        let empty = Map::new();
        let usage = obj(result.get("usage")).unwrap_or(&empty);
        let cached = int(usage.get("cache_read_input_tokens"));
        // Claude Code reports cache reads and writes outside input_tokens.
        let total_in =
            int(usage.get("input_tokens")) + int(usage.get("cache_creation_input_tokens")) + cached;
        let said = text(result.get("result"));
        let failed = result
            .get("is_error")
            .is_some_and(|e| e.as_bool() == Some(true));
        Some(Outcome {
            ok: !failed,
            text: if failed { String::new() } else { said.clone() },
            error: if failed { said } else { String::new() },
            stats: format!(
                "{} turns, {} in ({} cached) / {} out, ${:.4}",
                shown(result.get("num_turns")),
                thousands(total_in),
                thousands(cached),
                thousands(int(usage.get("output_tokens"))),
                float(result.get("total_cost_usd"))
            ),
        })
    }
}

// --- codex ----------------------------------------------------------------------------------------

/// `codex exec --json`: items, then `turn.completed` or `turn.failed`.
#[derive(Default)]
struct Codex {
    result: Option<Map<String, Value>>,
    text: String,
    failed: String,
}

impl Reader for Codex {
    fn event(&mut self, record: &Map<String, Value>, trace: Trace) {
        let kind = get(record, "type").and_then(Value::as_str).unwrap_or("");
        if kind == "turn.completed" {
            self.result = Some(record.clone());
            return;
        }
        if kind == "turn.failed" || kind == "error" {
            let why = [get(record, "error"), get(record, "message")]
                .into_iter()
                .map(text)
                .find(|s| !s.is_empty() && s != "false" && s != "0")
                .unwrap_or_else(|| kind.to_string());
            self.failed = why;
            return;
        }
        if !kind.starts_with("item.") {
            return;
        }
        let empty = Map::new();
        let item = obj(get(record, "item")).unwrap_or(&empty);
        let itype = item.get("type").and_then(Value::as_str);
        // The last agent_message is the answer; kept whether tracing or not.
        if itype == Some("agent_message") {
            self.text = text(item.get("text"));
        }
        // A command arrives as item.started and item.completed; tracing on started shows it while
        // it runs, and once.
        if itype == Some("command_execution") && kind == "item.started" {
            trace(format!(
                "  > {}",
                text(item.get("command"))
                    .chars()
                    .take(160)
                    .collect::<String>()
            ));
        } else if kind != "item.completed" {
        } else if itype == Some("agent_message") && !self.text.trim().is_empty() {
            trace(format!("  . {}", clip(&self.text)));
        } else if itype == Some("error") {
            // Not fatal: a failed turn is turn.failed. Traced because a swallowed error record is
            // the run's only warning.
            trace(format!(
                "  ! {}",
                text(item.get("message"))
                    .chars()
                    .take(160)
                    .collect::<String>()
            ));
        }
    }

    fn finish(&self) -> Option<Outcome> {
        if self.result.is_none() && self.failed.is_empty() {
            return None;
        }
        let empty = Map::new();
        let usage = self
            .result
            .as_ref()
            .and_then(|r| obj(r.get("usage")))
            .unwrap_or(&empty);
        let failed = !self.failed.is_empty();
        Some(Outcome {
            ok: !failed,
            text: if failed {
                String::new()
            } else {
                self.text.clone()
            },
            error: self.failed.clone(),
            // No turn count and no cost; cached input is counted inside input_tokens.
            stats: Tokens {
                input: int(usage.get("input_tokens")),
                output: int(usage.get("output_tokens")),
                cached: int(usage.get("cached_input_tokens")),
            }
            .line(),
        })
    }
}

// --- hax ------------------------------------------------------------------------------------------

/// `hax --json`: one record per session item. The result carries turns and cost but no token
/// totals, which are summed from `turn_usage` items.
#[derive(Default)]
struct Hax {
    result: Option<Map<String, Value>>,
    tokens: Tokens,
}

impl Reader for Hax {
    fn event(&mut self, record: &Map<String, Value>, trace: Trace) {
        if get(record, "type").and_then(Value::as_str) == Some("result") {
            self.result = Some(record.clone());
            return;
        }
        match get(record, "kind").and_then(Value::as_str) {
            Some("turn_usage") => {
                let empty = Map::new();
                let usage = obj(get(record, "usage")).unwrap_or(&empty);
                self.tokens.input += int(usage.get("input"));
                self.tokens.output += int(usage.get("output"));
                self.tokens.cached += int(usage.get("cached"));
            }
            Some("assistant") if !text(get(record, "text")).trim().is_empty() => {
                trace(format!("  . {}", clip(&text(get(record, "text")))));
            }
            Some("tool_call") => trace(format!("  > {}", text(get(record, "tool_name")))),
            _ => {}
        }
    }

    fn finish(&self) -> Option<Outcome> {
        let result = self.result.as_ref()?;
        let outcome = text(result.get("outcome"));
        let ok = outcome == "complete";
        // A stopped run carries no error string, so the outcome name is what there is to report.
        let error = if ok {
            String::new()
        } else {
            Some(text(result.get("error")))
                .filter(|e| !e.is_empty())
                .unwrap_or(outcome)
        };
        Some(Outcome {
            ok,
            text: text(result.get("text")),
            error,
            // hax normalises cache reads and writes into input.
            stats: format!(
                "{} turns, {}, ${:.4}",
                shown(result.get("turns")),
                self.tokens.line(),
                float(result.get("cost"))
            ),
        })
    }
}

// --- minima ---------------------------------------------------------------------------------------

/// `minima -p --json`: one record per line, then `result`.
#[derive(Default)]
struct Minima {
    result: Option<Map<String, Value>>,
}

impl Reader for Minima {
    fn event(&mut self, record: &Map<String, Value>, trace: Trace) {
        match get(record, "type").and_then(Value::as_str) {
            Some("result") => self.result = Some(record.clone()),
            Some("turn") if !text(get(record, "text")).trim().is_empty() => {
                trace(format!("  . {}", clip(&text(get(record, "text")))));
            }
            Some("tool_call") => trace(format!("  > {}", text(get(record, "name")))),
            Some("tool_result")
                if !get(record, "ok").is_some_and(|ok| ok.as_bool() == Some(true)) =>
            {
                trace("  ! tool error".into());
            }
            Some("retry") => trace(format!("  ! retry {}", text(get(record, "attempt")))),
            _ => {}
        }
    }

    fn finish(&self) -> Option<Outcome> {
        let result = self.result.as_ref()?;
        let outcome = text(result.get("outcome"));
        let ok = outcome == "complete";
        let error = if ok {
            String::new()
        } else {
            Some(text(result.get("error")))
                .filter(|e| !e.is_empty())
                .unwrap_or(outcome)
        };
        Some(Outcome {
            ok,
            text: text(result.get("text")),
            error,
            // No cache counts and no cost.
            stats: format!(
                "{} turns, {} in / {} out",
                shown(result.get("turns")),
                thousands(int(result.get("input_tokens"))),
                thousands(int(result.get("output_tokens")))
            ),
        })
    }
}

// --- opencode -------------------------------------------------------------------------------------

/// `opencode run --format json`: `{type, part}` records. Counts arrive per step, and
/// `part.tokens.input` excludes the cache, so the input total is the sum of the three.
#[derive(Default)]
struct OpenCode {
    text: String,
    failed: String,
    stepped: bool,
    tokens: Tokens,
    cost: f64,
}

impl Reader for OpenCode {
    fn event(&mut self, record: &Map<String, Value>, trace: Trace) {
        let kind = text(get(record, "type"));
        let empty = Map::new();
        let part = obj(get(record, "part")).unwrap_or(&empty);
        if kind.contains("error") {
            let error = text(get(record, "error"));
            self.failed = if error.is_empty() { kind } else { error };
            return;
        }
        match kind.as_str() {
            "step_finish" => {
                self.stepped = true;
                let counts = obj(part.get("tokens")).unwrap_or(&empty);
                let cache = obj(counts.get("cache")).unwrap_or(&empty);
                let read = int(cache.get("read"));
                self.tokens.cached += read;
                self.tokens.input += int(counts.get("input")) + read + int(cache.get("write"));
                self.tokens.output += int(counts.get("output"));
                self.cost += float(part.get("cost"));
            }
            "text" if !text(part.get("text")).is_empty() => {
                self.text = text(part.get("text"));
                trace(format!("  . {}", clip(&self.text)));
            }
            "tool_use" => trace(format!("  > {}", text(part.get("tool")))),
            _ => {}
        }
    }

    fn finish(&self) -> Option<Outcome> {
        if !self.stepped && self.failed.is_empty() {
            return None;
        }
        let failed = !self.failed.is_empty();
        Some(Outcome {
            ok: !failed,
            text: if failed {
                String::new()
            } else {
                self.text.clone()
            },
            error: self.failed.clone(),
            stats: format!("{}, ${:.4}", self.tokens.line(), self.cost),
        })
    }
}

// --- pi -------------------------------------------------------------------------------------------

/// pi's `--mode json`, which prime-agent shares. Counts come off the assistant's own
/// `message_end`, where they are final; `message_update` carries them cumulatively, and reading
/// both would count every message twice. `input` excludes the cache.
#[derive(Default)]
struct Pi {
    text: String,
    failed: String,
    ended: bool,
    tokens: Tokens,
    cost: f64,
}

impl Pi {
    fn message(&mut self, message: &Map<String, Value>, trace: Trace) {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let empty = Map::new();
        let usage = obj(message.get("usage")).unwrap_or(&empty);
        let cached = int(usage.get("cacheRead"));
        self.tokens.cached += cached;
        self.tokens.input += int(usage.get("input")) + cached + int(usage.get("cacheWrite"));
        self.tokens.output += int(usage.get("output"));
        self.cost += float(obj(usage.get("cost")).and_then(|c| c.get("total")));
        if message.get("stopReason").and_then(Value::as_str) == Some("error") {
            let why = text(message.get("errorMessage"));
            self.failed = if why.is_empty() {
                "the provider call failed".into()
            } else {
                why
            };
            return;
        }
        // A retry that lands clears the attempt that did not.
        self.failed.clear();
        let said = text_of(message);
        if !said.is_empty() {
            trace(format!("  . {}", clip(&said)));
            self.text = said;
        }
    }
}

/// The assistant text of one message, whatever shape its content takes.
fn text_of(message: &Map<String, Value>) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| text(b.get("text")))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl Reader for Pi {
    fn event(&mut self, record: &Map<String, Value>, trace: Trace) {
        match text(get(record, "type")).as_str() {
            "message_end" => {
                let empty = Map::new();
                let message = obj(get(record, "message")).unwrap_or(&empty).clone();
                self.message(&message, trace);
            }
            "tool_execution_start" => trace(format!("  > {}", text(get(record, "toolName")))),
            "auto_retry_start" => {
                let line = format!(
                    "  ! retry {}/{}: {}",
                    text(get(record, "attempt")),
                    text(get(record, "maxAttempts")),
                    text(get(record, "errorMessage"))
                );
                trace(line.chars().take(160).collect());
            }
            // pi retries a failed call up to three times, ending an agent each time. Only the one
            // that will not retry is the end.
            "agent_end" | "agent_settled" => {
                let retrying = get(record, "willRetry").is_some_and(|r| r.as_bool() == Some(true));
                self.ended = self.ended || !retrying;
            }
            _ => {}
        }
    }

    fn finish(&self) -> Option<Outcome> {
        if !self.ended && self.failed.is_empty() {
            return None;
        }
        let failed = !self.failed.is_empty();
        Some(Outcome {
            ok: !failed,
            text: if failed {
                String::new()
            } else {
                self.text.clone()
            },
            error: self.failed.clone(),
            stats: format!("{}, ${:.4}", self.tokens.line(), self.cost),
        })
    }
}

// --- hermes ---------------------------------------------------------------------------------------

static COMPLETED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Completed:\s*(True|False)").unwrap());
static CALLS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"API Calls:\s*(\d+)").unwrap());
static WARNED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\x{26A0}\x{FE0F}\s*(.+)").unwrap());
static FAILED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\x{274C}\s*(.+)").unwrap());
const FINAL: &str = "FINAL RESPONSE:";
const SIGN_OFF: char = '\u{1F44B}';

/// hermes-agent prints prose, not a stream: emoji-headed sections ending in a summary block. It
/// reports API calls and no token counts.
#[derive(Default)]
struct Hermes {
    completed: Option<bool>,
    calls: i64,
    failed: String,
    text: String,
    in_final: bool,
}

impl Reader for Hermes {
    /// hermes emits no JSON records. A line that happens to be one is conversation content.
    fn event(&mut self, _record: &Map<String, Value>, _trace: Trace) {}

    fn line(&mut self, line: &str, trace: Trace) {
        if self.in_final {
            // Everything after the header is the answer, until the sign-off. The rules drawn
            // around it are not part of it.
            let stripped = line.trim();
            if stripped.starts_with(SIGN_OFF) {
                self.in_final = false;
                if let Some(first) = self.text.lines().next() {
                    trace(format!(
                        "  . {}",
                        first.chars().take(160).collect::<String>()
                    ));
                }
            } else if !stripped.is_empty() && stripped.chars().all(|c| c == '-' || c == '=') {
            } else if !stripped.is_empty() {
                self.text = format!("{}\n{stripped}", self.text).trim().to_string();
            }
            return;
        }
        if line.contains(FINAL) {
            self.in_final = true;
        } else if let Some(found) = COMPLETED.captures(line) {
            self.completed = Some(&found[1] == "True");
        } else if let Some(found) = CALLS.captures(line) {
            self.calls = found[1].parse().unwrap_or(0);
        } else if let Some(found) = FAILED.captures(line) {
            self.failed = found[1].trim().to_string();
        } else if let Some(found) = WARNED.captures(line) {
            trace(format!(
                "  ! {}",
                found[1].trim().chars().take(160).collect::<String>()
            ));
        }
    }

    fn finish(&self) -> Option<Outcome> {
        if self.completed.is_none() && self.failed.is_empty() {
            return None;
        }
        let failed = !self.failed.is_empty();
        Some(Outcome {
            ok: self.completed == Some(true) && !failed,
            text: if failed {
                String::new()
            } else {
                self.text.clone()
            },
            error: self.failed.clone(),
            stats: format!("{} api calls", self.calls),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(-1234), "-1,234");
    }

    #[test]
    fn every_format_has_a_reader() {
        for format in FORMATS {
            assert!(reader(format).is_some(), "{format}");
        }
        assert!(reader("nope").is_none());
    }
}
