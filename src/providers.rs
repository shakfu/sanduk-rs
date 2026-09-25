//! Model providers and the wire protocols they speak.
//!
//! A provider is where requests go and how they are authenticated. A protocol is how a request
//! body and a usage block are shaped. The two are not 1:1: OpenAI serves Responses and Chat
//! Completions on different paths of the same host, and those two name their token counts
//! differently.
//!
//! The protocol cannot be recovered from a response body. Anthropic Messages and OpenAI Responses
//! both report `input_tokens` and `output_tokens`, so [`Provider::routes`] declares it per path.
//!
//! A new provider is a [`Provider`] constant and a [`PROVIDERS`] entry.

use std::fmt;
use std::net::IpAddr;

/// Why a provider or upstream was refused. The message is written for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// How one wire format names the things the relay reads or rewrites.
#[derive(Debug, PartialEq, Eq)]
pub struct Protocol {
    pub name: &'static str,
    /// The field `--max-tokens-cap` clamps.
    pub cap_field: &'static str,
    /// Canonical counter to the key it arrives under in this protocol's `usage`, dotted for a
    /// nested key. A counter the protocol does not report is left out, so a log line says 0 only
    /// where 0 is the truth.
    pub usage_fields: &'static [(&'static str, &'static str)],
}

impl Protocol {
    pub fn usage_field(&self, counter: &str) -> Option<&'static str> {
        self.usage_fields
            .iter()
            .find(|(c, _)| *c == counter)
            .map(|(_, f)| *f)
    }
}

pub const ANTHROPIC_MESSAGES: &str = "anthropic-messages";
pub const OPENAI_CHAT: &str = "openai-chat";
pub const OPENAI_RESPONSES: &str = "openai-responses";

pub static ANTHROPIC: Protocol = Protocol {
    name: ANTHROPIC_MESSAGES,
    cap_field: "max_tokens",
    usage_fields: &[
        ("in", "input_tokens"),
        ("cache_write", "cache_creation_input_tokens"),
        ("cache_read", "cache_read_input_tokens"),
        ("out", "output_tokens"),
    ],
};

/// No `cache_write`: OpenAI-shaped responses do not report one.
pub static OPENAI_CHAT_PROTOCOL: Protocol = Protocol {
    name: OPENAI_CHAT,
    cap_field: "max_completion_tokens",
    usage_fields: &[
        ("in", "prompt_tokens"),
        ("cache_read", "prompt_tokens_details.cached_tokens"),
        ("out", "completion_tokens"),
    ],
};

/// Reuses Anthropic's top-level counter names, which is why the protocol has to be declared.
pub static OPENAI_RESPONSES_PROTOCOL: Protocol = Protocol {
    name: OPENAI_RESPONSES,
    cap_field: "max_output_tokens",
    usage_fields: &[
        ("in", "input_tokens"),
        ("cache_read", "input_tokens_details.cached_tokens"),
        ("out", "output_tokens"),
    ],
};

pub static PROTOCOLS: [&Protocol; 3] = [
    &ANTHROPIC,
    &OPENAI_CHAT_PROTOCOL,
    &OPENAI_RESPONSES_PROTOCOL,
];

pub fn protocol(name: &str) -> Option<&'static Protocol> {
    PROTOCOLS.iter().copied().find(|p| p.name == name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// One upstream API: where it is, how it authenticates, what it accepts.
#[derive(Debug, PartialEq, Eq)]
pub struct Provider {
    pub name: &'static str,
    /// `host[:port]`, no scheme and no path.
    pub host: &'static str,
    /// Exact path to the protocol of the completion request sent there, or `None` for a path
    /// with no body policy and no usage. Exact, not prefixes: `/v1/models` as a prefix also
    /// admits `/v1/models-internal-secret`.
    pub routes: &'static [(&'static str, Option<&'static str>)],
    pub key_env: &'static str,
    pub base_url_env: &'static str,
    /// The prefix every route shares, for an agent handed a base URL.
    pub api_prefix: &'static str,
    pub auth_header: &'static str,
    /// `Bearer` for OpenAI-shaped providers, empty when the header holds the bare credential.
    /// Applies to the token the container presents and to the key written upstream.
    pub auth_scheme: &'static str,
    pub scheme: Scheme,
    pub has_auth: bool,
    /// The usage field carrying what a call cost. Only OpenRouter reports one; the others leave
    /// pricing to you, so a dollar budget is refused for them rather than guessed.
    pub cost_field: Option<&'static str>,
    /// Whether a streamed Chat Completions request needs `stream_options.include_usage` for the
    /// response to report tokens. A provider property: OpenRouter sends usage unasked.
    pub stream_usage_option: bool,
    /// The model a run selects when none is named. Per provider, since a model id is only valid
    /// on its own.
    pub default_model: Option<&'static str>,
    pub validate_path: &'static str,
    pub validate_headers: &'static [(&'static str, &'static str)],
}

impl Provider {
    /// The header value carrying `credential`.
    pub fn auth_value(&self, credential: &str) -> String {
        if self.auth_scheme.is_empty() {
            credential.to_string()
        } else {
            format!("{} {credential}", self.auth_scheme)
        }
    }

    /// The credential inside an incoming header value, scheme removed; empty when the scheme is
    /// missing.
    pub fn presented<'a>(&self, header_value: &'a str) -> &'a str {
        if self.auth_scheme.is_empty() {
            return header_value;
        }
        let n = self.auth_scheme.len();
        match header_value.get(..n + 1) {
            Some(prefix)
                if prefix[..n].eq_ignore_ascii_case(self.auth_scheme) && prefix.ends_with(' ') =>
            {
                header_value[n + 1..].trim()
            }
            _ => "",
        }
    }

    pub fn protocol(&self, path: &str) -> Option<&'static Protocol> {
        self.routes
            .iter()
            .find(|(p, _)| *p == path)
            .and_then(|(_, name)| name.and_then(protocol))
    }

    pub fn has_route(&self, path: &str) -> bool {
        self.routes.iter().any(|(p, _)| *p == path)
    }
}

pub static ANTHROPIC_PROVIDER: Provider = Provider {
    name: "anthropic",
    host: "api.anthropic.com",
    routes: &[
        ("/v1/messages", Some(ANTHROPIC_MESSAGES)),
        // count_tokens carries a body but is not a completion: clamping its max_tokens is
        // meaningless, so it is deliberately policy-exempt.
        ("/v1/messages/count_tokens", None),
        ("/v1/models", None),
    ],
    key_env: "ANTHROPIC_API_KEY",
    base_url_env: "ANTHROPIC_BASE_URL",
    api_prefix: "/v1",
    auth_header: "x-api-key",
    auth_scheme: "",
    scheme: Scheme::Https,
    has_auth: true,
    cost_field: None,
    stream_usage_option: false,
    default_model: None,
    validate_path: "/v1/models",
    validate_headers: &[("anthropic-version", "2023-06-01")],
};

/// Any server speaking the OpenAI API: llama-server, Ollama, LM Studio, vLLM. The host is a
/// placeholder for `--upstream`. Keyless, because a local server usually has none; a key in the
/// environment is still used.
pub static OPENAI_COMPAT_PROVIDER: Provider = Provider {
    name: "openai-compat",
    host: "127.0.0.1:8080",
    routes: &[
        ("/v1/chat/completions", Some(OPENAI_CHAT)),
        // Measured against llama-server build 10850: it answers /v1/responses too. Declared
        // because the route table is the egress allowlist.
        ("/v1/responses", Some(OPENAI_RESPONSES)),
        ("/v1/models", None),
    ],
    key_env: "OPENAI_API_KEY",
    base_url_env: "OPENAI_BASE_URL",
    api_prefix: "/v1",
    auth_header: "authorization",
    auth_scheme: "Bearer",
    scheme: Scheme::Http,
    has_auth: false,
    cost_field: None,
    // include_usage is part of the OpenAI API; a server that ignores it loses nothing.
    stream_usage_option: true,
    default_model: None,
    validate_path: "/v1/models",
    validate_headers: &[],
};

/// Two protocols on two paths, which is why routes carry a protocol each.
pub static OPENAI_PROVIDER: Provider = Provider {
    name: "openai",
    host: "api.openai.com",
    routes: &[
        ("/v1/responses", Some(OPENAI_RESPONSES)),
        ("/v1/chat/completions", Some(OPENAI_CHAT)),
        ("/v1/models", None),
    ],
    key_env: "OPENAI_API_KEY",
    base_url_env: "OPENAI_BASE_URL",
    api_prefix: "/v1",
    auth_header: "authorization",
    auth_scheme: "Bearer",
    scheme: Scheme::Https,
    has_auth: true,
    cost_field: None,
    stream_usage_option: true,
    default_model: Some("gpt-5.6-luna"),
    validate_path: "/v1/models",
    validate_headers: &[],
};

/// Every path carries the `/api/v1` prefix. Validation goes to `/api/v1/key`: `/api/v1/models`
/// answers 200 with no credential at all, so it would pass any key.
pub static OPENROUTER_PROVIDER: Provider = Provider {
    name: "openrouter",
    host: "openrouter.ai",
    routes: &[
        ("/api/v1/chat/completions", Some(OPENAI_CHAT)),
        ("/api/v1/models", None),
    ],
    key_env: "OPENROUTER_API_KEY",
    base_url_env: "OPENROUTER_BASE_URL",
    api_prefix: "/api/v1",
    auth_header: "authorization",
    auth_scheme: "Bearer",
    scheme: Scheme::Https,
    has_auth: true,
    // Every response carries `usage.cost`, in credits, unasked.
    cost_field: Some("cost"),
    stream_usage_option: false,
    default_model: None,
    validate_path: "/api/v1/key",
    validate_headers: &[],
};

pub static PROVIDERS: [&Provider; 4] = [
    &ANTHROPIC_PROVIDER,
    &OPENAI_PROVIDER,
    &OPENROUTER_PROVIDER,
    &OPENAI_COMPAT_PROVIDER,
];

pub const DEFAULT_PROVIDER: &str = "openai";

pub fn get_provider(name: &str) -> Result<&'static Provider, Error> {
    PROVIDERS
        .iter()
        .copied()
        .find(|p| p.name == name)
        .ok_or_else(|| {
            let mut known: Vec<_> = PROVIDERS.iter().map(|p| p.name).collect();
            known.sort_unstable();
            Error(format!(
                "unknown provider {name:?}; known: {}",
                known.join(", ")
            ))
        })
}

/// True when `host` (`name[:port]`) cannot leave this machine.
pub fn is_loopback(host: &str) -> bool {
    // A bare IPv6 address has colons of its own, so it is tried whole before a port is removed.
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        return ip.is_loopback();
    }
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    let name = name.trim_start_matches('[').trim_end_matches(']');
    name == "localhost" || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Splits `--upstream` into scheme and `host[:port]`.
///
/// Plaintext is refused off-machine: the relay writes the real key into every forwarded request,
/// so an `http://` upstream that is not loopback puts the credential on the wire in clear.
pub fn parse_upstream(url: &str, insecure: bool) -> Result<(Scheme, String), Error> {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("https", url.strip_prefix("//").unwrap_or(url)),
    };
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        other => {
            return Err(Error(format!(
                "--upstream scheme must be http or https, not {other:?}"
            )));
        }
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (netloc, path) = rest.split_at(end);
    if netloc.is_empty() {
        return Err(Error(format!("--upstream has no host: {url:?}")));
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    if !path.trim_end_matches('/').is_empty() {
        return Err(Error(format!(
            "--upstream carries a path ({path:?}). Give scheme://host:port only; path prefixes \
             belong in --proxy-allow-path and the agent's base URL."
        )));
    }
    if scheme == Scheme::Http && !insecure && !is_loopback(netloc) {
        return Err(Error(format!(
            "refusing plaintext http to {netloc}: the API key would be sent in clear. Use \
             https, a loopback address, or --insecure-upstream."
        )));
    }
    Ok((scheme, netloc.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_is_the_default_provider() {
        assert_eq!(get_provider(DEFAULT_PROVIDER).unwrap().name, "openai");
    }

    #[test]
    fn only_openai_names_a_default_model() {
        for p in PROVIDERS {
            let expected = (p.name == "openai").then_some("gpt-5.6-luna");
            assert_eq!(p.default_model, expected, "{}", p.name);
        }
    }

    #[test]
    fn unknown_provider_names_the_known_ones() {
        let err = get_provider("gemini").unwrap_err();
        assert!(
            err.0
                .contains("anthropic, openai, openai-compat, openrouter"),
            "{err}"
        );
    }

    #[test]
    fn every_route_names_a_real_protocol() {
        for p in PROVIDERS {
            for (path, name) in p.routes {
                assert!(
                    name.is_none_or(|n| protocol(n).is_some()),
                    "{} {path}",
                    p.name
                );
            }
        }
    }

    /// count_tokens has a body but is not a completion; /v1/models has no body at all.
    #[test]
    fn only_messages_carries_a_protocol() {
        let p = &ANTHROPIC_PROVIDER;
        assert_eq!(p.protocol("/v1/messages"), Some(&ANTHROPIC));
        assert_eq!(p.protocol("/v1/messages/count_tokens"), None);
        assert_eq!(p.protocol("/v1/models"), None);
        assert_eq!(p.protocol("/v1/nope"), None);
    }

    #[test]
    fn a_bare_credential_header_round_trips() {
        let p = &ANTHROPIC_PROVIDER;
        assert_eq!(p.auth_header, "x-api-key");
        assert_eq!(p.auth_value("KEY"), "KEY");
        assert_eq!(p.presented("KEY"), "KEY");
    }

    #[test]
    fn openai_compat_speaks_bearer() {
        let p = &OPENAI_COMPAT_PROVIDER;
        assert_eq!(p.auth_header, "authorization");
        assert_eq!(p.auth_value("KEY"), "Bearer KEY");
        assert_eq!(p.presented("Bearer KEY"), "KEY");
        assert_eq!(p.presented("bearer KEY"), "KEY");
    }

    /// Without the scheme the value is not a Bearer credential, and admitting it would leave the
    /// header format unenforced.
    #[test]
    fn a_bearer_provider_rejects_a_bare_credential() {
        let p = &OPENAI_COMPAT_PROVIDER;
        assert_eq!(p.presented("KEY"), "");
        assert_eq!(p.presented("Bearer"), "");
        assert_eq!(p.presented("BearerKEY"), "");
    }

    #[test]
    fn openai_chat_declares_no_cache_write() {
        let p = &OPENAI_CHAT_PROTOCOL;
        assert_eq!(p.usage_field("cache_write"), None);
        assert_eq!(
            p.usage_field("cache_read"),
            Some("prompt_tokens_details.cached_tokens")
        );
    }

    #[test]
    fn openai_serves_two_protocols_on_two_paths() {
        let p = &OPENAI_PROVIDER;
        assert_eq!(
            p.protocol("/v1/responses"),
            Some(&OPENAI_RESPONSES_PROTOCOL)
        );
        assert_eq!(
            p.protocol("/v1/chat/completions"),
            Some(&OPENAI_CHAT_PROTOCOL)
        );
        assert_eq!(p.protocol("/v1/models"), None);
    }

    /// Neither can be told from a body, which is why the route table declares the protocol.
    #[test]
    fn responses_and_anthropic_collide_on_counter_names() {
        let (r, a) = (&OPENAI_RESPONSES_PROTOCOL, &ANTHROPIC);
        assert_eq!(r.usage_field("in"), a.usage_field("in"));
        assert_eq!(r.usage_field("out"), a.usage_field("out"));
        assert_ne!(r.usage_field("cache_read"), a.usage_field("cache_read"));
    }

    #[test]
    fn each_protocol_clamps_its_own_field() {
        assert_eq!(ANTHROPIC.cap_field, "max_tokens");
        assert_eq!(OPENAI_CHAT_PROTOCOL.cap_field, "max_completion_tokens");
        assert_eq!(OPENAI_RESPONSES_PROTOCOL.cap_field, "max_output_tokens");
    }

    /// A path copied from OpenAI's docs would 403 here, silently, since the allowlist is exact.
    #[test]
    fn openrouter_paths_carry_the_api_prefix() {
        let p = &OPENROUTER_PROVIDER;
        assert_eq!(
            p.protocol("/api/v1/chat/completions"),
            Some(&OPENAI_CHAT_PROTOCOL)
        );
        assert!(!p.has_route("/v1/chat/completions"));
        assert_eq!(p.validate_path, "/api/v1/key");
    }

    #[test]
    fn every_provider_declares_where_its_key_comes_from() {
        for p in PROVIDERS {
            assert!(
                !p.key_env.is_empty() && !p.base_url_env.is_empty(),
                "{}",
                p.name
            );
        }
        let named = |env: &str| -> Vec<_> {
            PROVIDERS
                .iter()
                .filter(|p| p.key_env == env)
                .map(|p| p.name)
                .collect()
        };
        assert_eq!(named("ANTHROPIC_API_KEY"), ["anthropic"]);
        assert_eq!(named("OPENROUTER_API_KEY"), ["openrouter"]);
    }

    #[test]
    fn only_openai_compat_runs_keyless() {
        let keyless: Vec<_> = PROVIDERS
            .iter()
            .filter(|p| !p.has_auth)
            .map(|p| p.name)
            .collect();
        assert_eq!(keyless, ["openai-compat"]);
    }

    #[test]
    fn upstream_parsing() {
        for (given, scheme, host) in [
            ("http://127.0.0.1:8080", Scheme::Http, "127.0.0.1:8080"),
            ("http://localhost:1234", Scheme::Http, "localhost:1234"),
            ("http://[::1]:8080", Scheme::Http, "[::1]:8080"),
            ("https://api.openai.com", Scheme::Https, "api.openai.com"),
            ("openrouter.ai", Scheme::Https, "openrouter.ai"),
            ("https://api.openai.com/", Scheme::Https, "api.openai.com"),
        ] {
            assert_eq!(
                parse_upstream(given, false),
                Ok((scheme, host.into())),
                "{given}"
            );
        }
    }

    /// The relay writes the real key into every forwarded request.
    #[test]
    fn plaintext_off_machine_is_refused() {
        for given in [
            "http://example.com",
            "http://192.168.1.9:8080",
            "http://10.0.0.2",
        ] {
            let err = parse_upstream(given, false).unwrap_err();
            assert!(err.0.contains("plaintext"), "{given}: {err}");
        }
        assert_eq!(
            parse_upstream("http://example.com", true),
            Ok((Scheme::Http, "example.com".into()))
        );
    }

    /// OpenRouter's /api/v1 belongs in the allowlist. Dropping it would send every request to
    /// the wrong path.
    #[test]
    fn a_path_in_the_upstream_is_refused() {
        let err = parse_upstream("https://openrouter.ai/api/v1", false).unwrap_err();
        assert!(err.0.contains("path"), "{err}");
    }

    #[test]
    fn a_nonsense_scheme_is_refused() {
        let err = parse_upstream("ftp://example.com", false).unwrap_err();
        assert!(err.0.contains("scheme"), "{err}");
    }

    #[test]
    fn loopback_is_recognised_in_every_spelling() {
        for host in [
            "localhost",
            "localhost:1",
            "127.0.0.1",
            "127.0.0.1:8080",
            "[::1]:8080",
            "::1",
            "0:0:0:0:0:0:0:1",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["example.com", "10.0.0.2:80", "[2001:db8::1]:443"] {
            assert!(!is_loopback(host), "{host}");
        }
    }
}
