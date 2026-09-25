//! Agents as configuration: the shipped specs against Python sanduk's handlers, spec loading and
//! its refusals, and the stream readers.

use std::path::Path;

use sanduk::agent::{self, Endpoint, Options, stream};
use sanduk::providers::{Scheme, get_provider};
use sanduk::{recipes, util};
use serde_json::{Map, Value};

fn golden() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/agents.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn options(case: &Value) -> Options {
    let o = &case["options"];
    let s = |k: &str| o.get(k).and_then(Value::as_str).map(String::from);
    Options {
        model: s("model"),
        effort: s("effort"),
        max_turns: o.get("max_turns").and_then(Value::as_u64).map(|n| n as u32),
        permission_mode: s("permission_mode"),
        allowed_tools: s("allowed_tools"),
        bare: o.get("bare").and_then(Value::as_bool).unwrap_or(false),
        relayed: case["relayed"].as_bool().unwrap(),
        key_env: case["key_env_override"].as_str().map(String::from),
        base_url_env: None,
    }
}

/// Every shipped agent, against every provider, option set and endpoint Python sanduk 0.3.1's
/// handlers were run with (`tests/golden/agents.json`): the same verdict, wiring and argv.
#[test]
fn every_shipped_agent_wires_and_drives_as_python_sanduk_does() {
    let golden = golden();
    let cases = golden["cases"].as_array().unwrap();
    assert!(cases.len() > 900);
    for case in cases {
        let agent = agent::get(case["agent"].as_str().unwrap()).unwrap();
        let provider = get_provider(case["provider"].as_str().unwrap()).unwrap();
        let opts = options(case);
        let at = Endpoint {
            provider,
            scheme: if case["scheme"] == "http" {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: case["host"].as_str().unwrap().into(),
            root: case["root"].as_str().map(String::from),
        };
        let label = format!(
            "{} {} {} root={} opts={}",
            case["agent"], case["provider"], case["host"], case["root"], case["options"]
        );
        let checked = agent.check(&opts, provider);
        assert_eq!(
            checked.is_ok(),
            case["check"].is_null(),
            "{label}: {checked:?} vs {}",
            case["check"]
        );
        if checked.is_err() {
            continue;
        }
        let wiring = agent.wire(&opts, &at);
        let expected = &case["wiring"];
        assert_eq!(wiring.key_env, expected["key_env"], "{label}");
        assert_eq!(wiring.base_url_env, expected["base_url_env"], "{label}");
        assert_eq!(
            wiring.base_url.as_deref(),
            expected["base_url"].as_str(),
            "{label}"
        );
        let env: Vec<Value> = wiring
            .env
            .iter()
            .map(|(k, v)| Value::from(vec![k.clone(), v.clone()]))
            .collect();
        assert_eq!(Value::from(env), expected["env"], "{label}");
        let argv = agent.argv(&opts, &at, "do the task", &wiring).unwrap();
        assert_eq!(Value::from(argv), case["argv"], "{label}");
    }
}

/// Fed the same records, each reader traces the same lines and reaches the same outcome.
#[test]
fn every_stream_reads_as_python_sanduk_reads_it() {
    let golden = golden();
    for case in golden["streams"].as_array().unwrap() {
        let format = case["format"].as_str().unwrap();
        let mut reader = stream::reader(format).unwrap();
        let mut traced = Vec::new();
        let mut trace = |line: String| traced.push(line);
        for record in case
            .get("records")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            reader.event(record.as_object().unwrap(), &mut trace);
        }
        for line in case
            .get("lines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let line = line.as_str().unwrap();
            match serde_json::from_str::<Map<String, Value>>(line) {
                Ok(record) => reader.event(&record, &mut trace),
                Err(_) => reader.line(line, &mut trace),
            }
        }
        assert_eq!(Value::from(traced), case["trace"], "{format}: {case}");
        let outcome = reader.finish().map(
            |o| serde_json::json!({"ok": o.ok, "text": o.text, "error": o.error, "stats": o.stats}),
        );
        assert_eq!(
            outcome.unwrap_or(Value::Null),
            case["outcome"],
            "{format}: {case}"
        );
    }
}

// --- the catalogue ---------------------------------------------------------------------------------

#[test]
fn the_shipped_agents_are_found_and_each_names_a_recipe_that_builds_it() {
    let names = agent::names().unwrap();
    assert_eq!(
        names,
        [
            "claude", "codex", "hax", "hermes", "minima", "opencode", "pi", "prime"
        ]
    );
    for name in names {
        let agent = agent::get(&name).unwrap();
        let recipe = recipes::resolve(agent.recipe.as_deref().unwrap(), &[]).unwrap();
        assert_eq!(recipe.agent, name);
    }
}

#[test]
fn an_unknown_agent_names_the_known_ones() {
    let refused = agent::get("gemini").unwrap_err();
    assert!(
        refused.message.contains("known: claude, codex"),
        "{}",
        refused.message
    );
}

/// A scratch file for one spec, under a directory removed when dropped.
struct Spec(std::path::PathBuf);

impl Spec {
    fn write(name: &str, body: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("sanduk-agent-{name}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, body).unwrap();
        Spec(path)
    }

    fn load(&self) -> Result<agent::Agent, String> {
        agent::get(self.0.to_str().unwrap()).map_err(|e| e.message)
    }
}

impl Drop for Spec {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

const MINIMAL: &str = r#"
name = "mine"
image = "mine:1"
containerfile = "Containerfile"
protocols = ["openai-chat"]
stream = "minima"
argv = ["--json", { args = ["--model={model}"] }, "{task}"]
[wire]
key_env = "MY_KEY"
base_url_env = "MY_BASE_URL"
base_url = "{endpoint}{api_prefix}"
"#;

/// A new agent speaking an existing stream format is a file and nothing else.
#[test]
fn an_agent_outside_the_catalogue_loads_by_path() {
    let spec = Spec::write("mine", MINIMAL);
    let mine = spec.load().unwrap();
    assert_eq!(mine.image.as_deref(), Some("mine:1"));
    let dir = std::fs::canonicalize(spec.0.parent().unwrap()).unwrap();
    assert_eq!(mine.containerfile, Some(dir.join("Containerfile")));
    let provider = get_provider("openai").unwrap();
    let at = Endpoint {
        provider,
        scheme: Scheme::Https,
        host: provider.host.into(),
        root: None,
    };
    let opts = Options::default();
    let wiring = mine.wire(&opts, &at);
    assert_eq!(
        wiring.base_url.as_deref(),
        Some("https://api.openai.com/v1")
    );
    assert_eq!(
        mine.argv(&opts, &at, "go", &wiring).unwrap(),
        ["--json", "go"]
    );
}

#[test]
fn a_spec_that_would_fail_mid_run_is_refused_when_read() {
    let cases = [
        (
            MINIMAL.replace("{task}", "{taks}"),
            "{taks} is not a variable",
        ),
        (
            MINIMAL.replace("\"minima\"", "\"gemini\""),
            "stream \"gemini\" is unknown",
        ),
        (
            MINIMAL.replace("openai-chat", "openai-chatty"),
            "protocol \"openai-chatty\" is unknown",
        ),
        (
            MINIMAL.replace("\"{task}\"", "\"--x\""),
            "never passes {task}",
        ),
        (
            MINIMAL.replace("image = \"mine:1\"\n", ""),
            "a recipe, or an image and its containerfile",
        ),
        (
            MINIMAL.replace("--model={model}", "--model={model|lower}"),
            "unknown filter",
        ),
        (
            format!("{MINIMAL}\n[check]\nrequire = [\"modle\"]\n"),
            "not an option",
        ),
        (
            MINIMAL.replace("stream = \"minima\"", "stream = \"minima\"\nsurprise = 1"),
            "unknown field",
        ),
        (
            format!(
                "{MINIMAL}\n[vars.x]\nfrom = \"protocol\"\nmap = {{ \"anthropic-messages\" = \"a\" }}\n"
            ),
            "is not a protocol",
        ),
        (
            format!("{MINIMAL}\n[vars.model]\nfrom = \"provider\"\nmap = {{}}\n"),
            "shadows a built-in",
        ),
    ];
    for (body, expected) in cases {
        let refused = Spec::write("mine", &body).load().unwrap_err();
        assert!(refused.contains(expected), "{expected}: {refused}");
    }
}

#[test]
fn a_user_agent_cannot_take_a_shipped_name() {
    let dir = std::env::temp_dir().join(format!("sanduk-agent-shadow-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(
        dir.join("agents/claude.toml"),
        MINIMAL.replace("\"mine\"", "\"claude\""),
    )
    .unwrap();
    util::override_dirs(Some(dir.clone()), None);
    let refused = agent::get("claude").unwrap_err();
    util::override_dirs(None, None);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        refused
            .message
            .contains("shipped with sanduk and cannot be replaced")
    );
}

// --- behaviour Python got wrong, and messages ---------------------------------------------------

fn at_relay(provider: &'static sanduk::providers::Provider) -> Endpoint<'static> {
    Endpoint {
        provider,
        scheme: provider.scheme,
        host: provider.host.into(),
        root: Some("http://10.0.0.1:9".into()),
    }
}

/// Python wrote the default variable name into the config blob whatever `--agent-key-env` said, so
/// the agent looked for its credential under a name the container did not set.
#[test]
fn an_overridden_key_variable_is_the_one_the_config_blob_names() {
    let provider = get_provider("openai").unwrap();
    let opts = Options {
        model: Some("m".into()),
        key_env: Some("MY_KEY".into()),
        relayed: true,
        ..Options::default()
    };
    for (name, var, expected) in [
        ("opencode", "OPENCODE_CONFIG_CONTENT", "{env:MY_KEY}"),
        ("pi", "SANDUK_MODELS_JSON", "$MY_KEY"),
        ("prime", "SANDUK_MODELS_JSON", "\"MY_KEY\""),
    ] {
        let wiring = agent::get(name).unwrap().wire(&opts, &at_relay(provider));
        assert_eq!(wiring.key_env, "MY_KEY");
        let blob = &wiring.env.iter().find(|(k, _)| k == var).unwrap().1;
        assert!(blob.contains(expected), "{name}: {blob}");
    }
}

/// codex takes its endpoint as TOML; a value that needs quoting is quoted, not spliced.
#[test]
fn codex_config_values_are_quoted_as_toml() {
    let provider = get_provider("openai").unwrap();
    let opts = Options {
        key_env: Some("K\"EY".into()),
        ..Options::default()
    };
    let codex = agent::get("codex").unwrap();
    let at = at_relay(provider);
    let wiring = codex.wire(&opts, &at);
    let argv = codex.argv(&opts, &at, "go", &wiring).unwrap();
    assert!(
        argv.contains(&"model_providers.sanduk.env_key=\"K\\\"EY\"".to_string()),
        "{argv:?}"
    );
}

#[test]
fn refusals_name_the_flag_and_the_agent() {
    let provider = get_provider("openai").unwrap();
    let hax = agent::get("hax").unwrap();
    let refused = hax
        .check(
            &Options {
                allowed_tools: Some("Read".into()),
                ..Options::default()
            },
            provider,
        )
        .unwrap_err();
    assert_eq!(refused.message, "--allowed-tools has no hax equivalent");
    let pi = agent::get("pi").unwrap();
    assert_eq!(
        pi.check(&Options::default(), provider).unwrap_err().message,
        "--model is required with --agent pi"
    );
    let claude = agent::get("claude").unwrap();
    assert!(
        claude
            .check(&Options::default(), provider)
            .unwrap_err()
            .message
            .contains("cannot talk to the openai provider")
    );
    let hermes = agent::get("hermes").unwrap();
    let relayed = Options {
        relayed: true,
        model: Some("m".into()),
        ..Options::default()
    };
    assert!(
        hermes
            .check(&relayed, provider)
            .unwrap_err()
            .message
            .contains("Use --mode open")
    );
}
