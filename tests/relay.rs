//! Relay tests: token, path, policy, header injection, streaming, cost.
//!
//! Every test runs against a local fake upstream, so the suite makes no API calls, costs
//! nothing, and needs no key.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use sanduk::providers::{
    ANTHROPIC_PROVIDER, OPENAI_COMPAT_PROVIDER, OPENROUTER_PROVIDER, PROVIDERS, Provider,
};
use serde_json::{Value, json};

const A: &Provider = &ANTHROPIC_PROVIDER;
const COMPAT: &Provider = &OPENAI_COMPAT_PROVIDER;

fn ok() -> Upstream {
    Upstream::json(r#"{"ok":true}"#)
}

fn models(models: &[&str]) -> Option<BTreeSet<String>> {
    Some(models.iter().map(|m| m.to_string()).collect())
}

// --- access control -----------------------------------------------------------------------------

#[test]
fn a_wrong_or_missing_token_is_rejected() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    assert_eq!(
        call(relay.port(), A, "/v1/models", "guessed", None).status,
        401
    );
    assert_eq!(call(relay.port(), A, "/v1/models", "", None).status, 401);
    assert_eq!(up.calls(), 0);
}

/// A prefix match would admit this; the allowlist is exact.
#[test]
fn a_lookalike_path_is_rejected() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    assert_eq!(
        call(relay.port(), A, "/v1/models-internal-secret", TOKEN, None).status,
        403
    );
}

/// Claude Code calls /v1/messages?beta=true. Matching the raw target would 403.
#[test]
fn a_query_string_still_matches_and_is_forwarded() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let r = call(
        relay.port(),
        A,
        "/v1/messages?beta=true",
        TOKEN,
        Some(&message("m", 64)),
    );
    assert_eq!(r.status, 200);
    assert_eq!(up.last().target, "/v1/messages?beta=true");
}

#[test]
fn rejections_are_counted() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    call(relay.port(), A, "/v1/models", "guessed", None);
    call(relay.port(), A, "/v1/nope", TOKEN, None);
    let stats = relay.stats();
    assert_eq!((stats.requests, stats.rejected), (0, 2));
}

// --- credential handling ------------------------------------------------------------------------

#[test]
fn the_real_key_is_injected_and_the_token_is_not_forwarded() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    assert_eq!(
        call(
            relay.port(),
            A,
            "/v1/messages",
            TOKEN,
            Some(&message("m", 64))
        )
        .status,
        200
    );
    let seen = up.last();
    assert_eq!(seen.header("x-api-key"), Some(REAL_KEY));
    assert!(!format!("{:?}", seen.headers).contains(TOKEN));
    assert_eq!(seen.header("host"), Some(up.addr.as_str()));
}

/// Denylist, not allowlist: provider headers must survive the hop.
#[test]
fn provider_headers_are_forwarded() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let mut headers = auth(A, TOKEN);
    headers.push(("anthropic-version".into(), "2023-06-01".into()));
    headers.push(("anthropic-beta".into(), "x-1".into()));
    call_with(
        relay.port(),
        "/v1/messages",
        &headers,
        Some(&message("m", 64)),
    );
    let seen = up.last();
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(seen.header("anthropic-beta"), Some("x-1"));
}

const SMUGGLED: [(&str, &str); 4] = [
    ("x-api-key", "smuggled-anthropic"),
    ("authorization", "Bearer smuggled-openai"),
    ("api-key", "smuggled-azure"),
    ("proxy-authorization", "Basic smuggled"),
];

/// The relay supplies the credential; whatever the container presents is dropped. Getting this
/// wrong forwards an attacker-chosen key upstream and fails silently, so every provider is
/// checked.
#[test]
fn no_container_credential_reaches_any_upstream() {
    for provider in PROVIDERS {
        let up = ok();
        let (relay, _) = relay(provider, &up, |c| c);
        let path = provider
            .routes
            .iter()
            .map(|(p, _)| *p)
            .find(|p| p.ends_with("/models"))
            .unwrap();
        let mut headers: Vec<(String, String)> = SMUGGLED
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        headers.retain(|(n, _)| n != provider.auth_header);
        // The run token has to arrive in the header this provider reads, or the request is
        // refused before any of this is exercised.
        headers.push((provider.auth_header.into(), provider.auth_value(TOKEN)));
        assert_eq!(
            call_with(relay.port(), path, &headers, None).status,
            200,
            "{}",
            provider.name
        );
        let seen = up.last();
        for (name, value) in SMUGGLED {
            assert_ne!(
                seen.header(name),
                Some(value),
                "{} forwarded {name}",
                provider.name
            );
        }
        assert_eq!(
            seen.header(provider.auth_header),
            Some(provider.auth_value(REAL_KEY).as_str()),
            "{}",
            provider.name
        );
        for name in ["x-api-key", "authorization", "api-key"] {
            if name != provider.auth_header {
                assert_eq!(seen.header(name), None, "{} leaked {name}", provider.name);
            }
        }
        assert!(
            !format!("{:?}", seen.headers).contains(TOKEN),
            "{}",
            provider.name
        );
    }
}

// --- body policy --------------------------------------------------------------------------------

#[test]
fn a_disallowed_model_is_rejected_and_an_allowed_one_passes() {
    let up = ok();
    let (relay, _) = relay(A, &up, |mut c| {
        c.allow_models = models(&["claude-opus-5"]);
        c
    });
    let port = relay.port();
    assert_eq!(
        call(
            port,
            A,
            "/v1/messages",
            TOKEN,
            Some(&message("claude-sonnet-5", 64))
        )
        .status,
        403
    );
    assert_eq!(
        call(
            port,
            A,
            "/v1/messages",
            TOKEN,
            Some(&message("claude-opus-5", 64))
        )
        .status,
        200
    );
    assert_eq!(up.calls(), 1);
}

#[test]
fn max_tokens_is_clamped_and_a_smaller_ask_is_untouched() {
    let up = ok();
    let (relay, _) = relay(A, &up, |mut c| {
        c.max_tokens_cap = Some(4000);
        c
    });
    call(
        relay.port(),
        A,
        "/v1/messages",
        TOKEN,
        Some(&message("m", 999_999)),
    );
    assert_eq!(up.last().json()["max_tokens"], 4000);
    call(
        relay.port(),
        A,
        "/v1/messages",
        TOKEN,
        Some(&message("m", 1024)),
    );
    wait_until(|| up.calls() == 2);
    assert_eq!(up.last().json()["max_tokens"], 1024);
}

#[test]
fn malformed_json_is_rejected_when_policy_is_on() {
    let up = ok();
    let (relay, _) = relay(A, &up, |mut c| {
        c.max_tokens_cap = Some(4000);
        c
    });
    assert_eq!(
        call(relay.port(), A, "/v1/messages", TOKEN, Some(b"{oops")).status,
        400
    );
    assert_eq!(
        call(relay.port(), A, "/v1/messages", TOKEN, Some(b"[1]")).status,
        400
    );
}

#[test]
fn the_body_is_untouched_when_no_policy_is_set() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let body = message("m", 999_999);
    call(relay.port(), A, "/v1/messages", TOKEN, Some(&body));
    assert_eq!(up.last().body, body);
}

/// A GET carries no body, which must not read as a policy refusal.
#[test]
fn a_bodyless_get_reaches_upstream() {
    let up = ok();
    let (relay, _) = relay(A, &up, |mut c| {
        c.max_tokens_cap = Some(10);
        c
    });
    assert_eq!(call(relay.port(), A, "/v1/models", TOKEN, None).status, 200);
    assert_eq!(up.last().header("x-api-key"), Some(REAL_KEY));
}

/// The cap is a bound on host memory: the container holds the run token and could otherwise
/// send any amount.
#[test]
fn a_body_over_the_cap_is_refused_before_it_is_sent() {
    let up = ok();
    let (relay, _) = relay(A, &up, |mut c| {
        c.max_body = 1024;
        c
    });
    let r = call(
        relay.port(),
        A,
        "/v1/messages",
        TOKEN,
        Some(&vec![b'x'; 4096]),
    );
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["error"]["type"], "request_too_large");
    assert_eq!(up.calls(), 0);
}

// --- body log -----------------------------------------------------------------------------------

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sanduk-relay-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The audit trail lands outside the bind mount, one file per call.
#[test]
fn request_bodies_are_written_to_the_log_dir() {
    let dir = scratch("log");
    let up = ok();
    let (relay, notes) = relay(A, &up, |mut c| {
        c.log_bodies = true;
        c.log_dir = Some(dir.clone());
        c
    });
    call(
        relay.port(),
        A,
        "/v1/messages",
        TOKEN,
        Some(&message("claude-opus-5", 64)),
    );
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let body: Value =
        serde_json::from_slice(&std::fs::read(dir.join("001.json")).unwrap()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(names, ["001.json"]);
    assert_eq!(body["model"], "claude-opus-5");
    let digest = notes
        .lock()
        .unwrap()
        .iter()
        .find(|n| n.starts_with("body 001"))
        .cloned()
        .unwrap();
    assert!(
        digest.contains("model=claude-opus-5 max_tokens=64"),
        "{digest}"
    );
}

/// A symlink at the next name would put the body wherever it points.
#[test]
fn a_log_file_that_already_exists_is_not_followed_or_overwritten() {
    let dir = scratch("symlink");
    let target = dir.join("elsewhere");
    std::os::unix::fs::symlink(&target, dir.join("001.json")).unwrap();
    let up = ok();
    let (relay, notes) = relay(A, &up, |mut c| {
        c.log_bodies = true;
        c.log_dir = Some(dir.clone());
        c
    });
    assert_eq!(
        call(
            relay.port(),
            A,
            "/v1/messages",
            TOKEN,
            Some(&message("m", 64))
        )
        .status,
        200
    );
    let followed = target.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!followed, "the body was written through the symlink");
    assert!(
        notes
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.contains("not written"))
    );
}

// --- streaming ----------------------------------------------------------------------------------

/// Four events, 0.3s apart, then the terminator.
fn spaced_events() -> Upstream {
    Upstream::start(|_, stream| {
        start_chunked(stream, "text/event-stream");
        for i in 0..4 {
            let _ = write_chunk(stream, format!("data: {{\"i\":{i}}}\n\n").as_bytes());
            std::thread::sleep(Duration::from_millis(300));
        }
        let _ = write_chunk(stream, b"");
    })
}

/// Each event reaches the client as it arrives, not when the response is whole.
#[test]
fn sse_is_streamed_not_buffered() {
    let up = spaced_events();
    let (relay, _) = relay(A, &up, |c| c);
    let mut conn = Conn::open(relay.port());
    let headers = auth(A, TOKEN);
    let headers: Vec<_> = headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    conn.send("POST", "/v1/messages", &headers, Some(b"{}"));
    let started = Instant::now();
    assert_eq!(conn.read_head().0, 200);
    let mut arrivals = Vec::new();
    while let Some(chunk) = conn.read_chunk() {
        if chunk.starts_with(b"data:") {
            arrivals.push(started.elapsed().as_secs_f64());
        }
    }
    assert_eq!(arrivals.len(), 4);
    assert!(
        arrivals[3] - arrivals[0] > 0.5,
        "arrived together: {arrivals:?}"
    );
}

#[test]
fn streamed_usage_reaches_the_log_line() {
    let up = Upstream::start(|_, stream| {
        start_chunked(stream, "text/event-stream");
        let _ = write_chunk(
            stream,
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\
              \"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":210330,\
              \"output_tokens\":1}}}\n\n",
        );
        let _ = write_chunk(
            stream,
            b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":16517}}\n\n",
        );
        let _ = write_chunk(stream, b"");
    });
    let (relay, notes) = relay(A, &up, |c| c);
    assert_eq!(
        call(
            relay.port(),
            A,
            "/v1/messages",
            TOKEN,
            Some(&message("m", 64))
        )
        .status,
        200
    );
    let line = relayed_line(&notes);
    assert!(
        line.contains("cache_read=210330") && line.contains("out=16517"),
        "{line}"
    );
}

/// /v1/models reports no tokens; the line must not grow four zero fields.
#[test]
fn a_response_without_usage_gets_no_suffix() {
    let up = ok();
    let (relay, notes) = relay(A, &up, |c| c);
    assert_eq!(call(relay.port(), A, "/v1/models", TOKEN, None).status, 200);
    let line = relayed_line(&notes);
    assert!(
        !line.contains("cache_read") && !line.contains("usage="),
        "{line}"
    );
}

/// The API prefers brotli when offered, and the usage reader cannot decode it.
#[test]
fn brotli_is_never_offered_and_identity_is_kept() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    for (offered, forwarded) in [("gzip, deflate, br", "gzip"), ("identity", "identity")] {
        let mut headers = auth(A, TOKEN);
        headers.push(("accept-encoding".into(), offered.into()));
        call_with(
            relay.port(),
            "/v1/messages",
            &headers,
            Some(&message("m", 64)),
        );
        let seen = up.last();
        assert_eq!(seen.header("accept-encoding"), Some(forwarded), "{offered}");
    }
}

// --- openai-compat ------------------------------------------------------------------------------

fn openai_usage() -> Upstream {
    Upstream::json(
        r#"{"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":64}}}"#,
    )
}

fn chat(model: &str, cap: Option<u64>) -> Vec<u8> {
    let mut payload = json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
    if let Some(cap) = cap {
        payload["max_completion_tokens"] = cap.into();
    }
    payload.to_string().into_bytes()
}

fn compat(
    up: &Upstream,
    api_key: &str,
    tweak: impl FnOnce(sanduk::relay::Config) -> sanduk::relay::Config,
) -> (sanduk::relay::Relay, Notes) {
    let key = api_key.to_string();
    relay(COMPAT, up, move |mut c| {
        c.api_key = key;
        tweak(c)
    })
}

/// The provider's own scheme is http, so the relay reaches a local server without TLS.
#[test]
fn openai_compat_is_reached_over_plaintext_by_default() {
    assert_eq!(COMPAT.scheme, sanduk::providers::Scheme::Http);
    let up = openai_usage();
    let (relay, _) = compat(&up, "", |c| c);
    assert_eq!(
        call(
            relay.port(),
            COMPAT,
            "/v1/chat/completions",
            TOKEN,
            Some(&chat("local", None))
        )
        .status,
        200
    );
    assert_eq!(up.last().json()["model"], "local");
}

#[test]
fn a_bearer_token_is_checked_and_a_bare_one_refused() {
    let up = openai_usage();
    let (relay, _) = compat(&up, "", |c| c);
    assert_eq!(
        call(relay.port(), COMPAT, "/v1/models", "guessed", None).status,
        401
    );
    let bare = vec![("authorization".to_string(), TOKEN.to_string())];
    assert_eq!(
        call_with(relay.port(), "/v1/models", &bare, None).status,
        401
    );
}

/// A local llama-server has no credential. Forwarding the run token would leak the relay's own
/// access control upstream.
#[test]
fn no_auth_header_is_written_when_there_is_no_key() {
    let up = openai_usage();
    let (relay, _) = compat(&up, "", |c| c);
    call(
        relay.port(),
        COMPAT,
        "/v1/chat/completions",
        TOKEN,
        Some(&chat("local", None)),
    );
    assert_eq!(up.last().header("authorization"), None);
}

#[test]
fn a_key_is_written_as_a_bearer_credential() {
    let up = openai_usage();
    let (relay, _) = compat(&up, "sk-upstream", |c| c);
    call(
        relay.port(),
        COMPAT,
        "/v1/chat/completions",
        TOKEN,
        Some(&chat("local", None)),
    );
    assert_eq!(
        up.last().header("authorization"),
        Some("Bearer sk-upstream")
    );
}

/// max_tokens is Anthropic's name. Clamping it here would leave the real limit untouched.
#[test]
fn the_cap_clamps_the_openai_field_not_max_tokens() {
    let up = openai_usage();
    let (relay, _) = compat(&up, "", |mut c| {
        c.max_tokens_cap = Some(4000);
        c
    });
    call(
        relay.port(),
        COMPAT,
        "/v1/chat/completions",
        TOKEN,
        Some(&chat("local", Some(64000))),
    );
    let body = up.last().json();
    assert_eq!(body["max_completion_tokens"], 4000);
    assert!(body.get("max_tokens").is_none());
}

#[test]
fn openai_usage_including_nested_cached_tokens_is_logged() {
    let up = openai_usage();
    let (relay, notes) = compat(&up, "", |c| c);
    call(
        relay.port(),
        COMPAT,
        "/v1/chat/completions",
        TOKEN,
        Some(&chat("local", None)),
    );
    let line = relayed_line(&notes);
    assert!(line.ends_with(" in=100 cache_read=64 out=20"), "{line}");
}

// --- stream usage injection ---------------------------------------------------------------------
//
// A streamed OpenAI response carries no usage unless the request asked for it. OpenRouter sends
// usage unasked, which is why the flag sits on the provider and not on the protocol.

fn streaming(extra: Value) -> Vec<u8> {
    let mut payload =
        json!({"model": "local", "messages": [{"role": "user", "content": "hi"}], "stream": true});
    for (k, v) in extra.as_object().unwrap() {
        payload[k] = v.clone();
    }
    payload.to_string().into_bytes()
}

/// It fires on a plain run, with no other policy set, which is every run.
#[test]
fn include_usage_is_added_for_openai_streams() {
    let up = openai_usage();
    let (relay, _) = compat(&up, "", |c| c);
    call(
        relay.port(),
        COMPAT,
        "/v1/chat/completions",
        TOKEN,
        Some(&streaming(json!({}))),
    );
    assert_eq!(
        up.last().json()["stream_options"],
        json!({"include_usage": true})
    );
}

#[test]
fn existing_stream_options_are_preserved() {
    let up = openai_usage();
    let (relay, _) = compat(&up, "", |c| c);
    let body = streaming(json!({"stream_options": {"something_else": 1}}));
    call(
        relay.port(),
        COMPAT,
        "/v1/chat/completions",
        TOKEN,
        Some(&body),
    );
    assert_eq!(
        up.last().json()["stream_options"],
        json!({"something_else": 1, "include_usage": true})
    );
}

/// OpenAI answers 400 unknown_parameter: the field is Chat Completions only.
#[test]
fn a_streamed_responses_request_is_not_given_stream_options() {
    let up = openai_usage();
    let (relay, _) = compat(&up, "", |c| c);
    let body = json!({"model": "m", "input": "hi", "stream": true}).to_string();
    call(
        relay.port(),
        COMPAT,
        "/v1/responses",
        TOKEN,
        Some(body.as_bytes()),
    );
    assert!(up.last().json().get("stream_options").is_none());
}

/// A non-streamed response carries usage already.
#[test]
fn a_non_streamed_request_is_untouched() {
    let up = openai_usage();
    let (relay, _) = compat(&up, "", |c| c);
    let body = chat("local", None);
    call(
        relay.port(),
        COMPAT,
        "/v1/chat/completions",
        TOKEN,
        Some(&body),
    );
    assert_eq!(up.last().body, body);
}

#[test]
fn openrouter_and_anthropic_are_not_given_stream_options() {
    let up = ok();
    let (or_relay, _) = relay(&OPENROUTER_PROVIDER, &up, |c| c);
    call(
        or_relay.port(),
        &OPENROUTER_PROVIDER,
        "/api/v1/chat/completions",
        TOKEN,
        Some(&streaming(json!({}))),
    );
    assert!(up.last().json().get("stream_options").is_none());

    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let body = json!({"model": "m", "max_tokens": 64, "stream": true, "messages": []}).to_string();
    call(
        relay.port(),
        A,
        "/v1/messages",
        TOKEN,
        Some(body.as_bytes()),
    );
    assert!(up.last().json().get("stream_options").is_none());
}

// --- chunked requests ---------------------------------------------------------------------------

fn chunked_head(path: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: relay\r\nx-api-key: {TOKEN}\r\ncontent-type: application/json\r\n\
         Transfer-Encoding: chunked\r\n\r\n"
    )
}

fn chunked(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend(format!("{:x}\r\n", part.len()).bytes());
        out.extend_from_slice(part);
        out.extend(b"\r\n");
    }
    out.extend(b"0\r\n\r\n");
    out
}

/// A client that streams its request sends no Content-Length. Measured with prime-agent in
/// Python sanduk, whose system prompt is large enough that its client streams the request.
#[test]
fn a_chunked_request_body_is_read_and_forwarded() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let payload = json!({"model": "m", "max_tokens": 8, "messages": [{"role": "user", "content": "x".repeat(5000)}]}).to_string();
    let mut conn = Conn::open(relay.port());
    conn.send_raw(chunked_head("/v1/messages").as_bytes());
    conn.send_raw(&chunked(&[
        &payload.as_bytes()[..2000],
        &payload.as_bytes()[2000..],
    ]));
    assert_eq!(conn.read_reply().status, 200);
    assert_eq!(up.last().body, payload.as_bytes());
}

/// The failure this guards against was a leftover body parsed as the next request, so one
/// request through a fresh connection proves nothing.
#[test]
fn a_chunked_request_survives_a_second_on_one_connection() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let mut conn = Conn::open(relay.port());
    for n in 0..2 {
        let body = json!({"model": "m", "max_tokens": 8, "n": n}).to_string();
        conn.send_raw(chunked_head("/v1/messages").as_bytes());
        conn.send_raw(&chunked(&[body.as_bytes()]));
        assert_eq!(conn.read_reply().status, 200, "{n}");
        wait_until(|| up.calls() == n + 1);
        assert_eq!(up.last().json()["n"], n);
    }
}

#[test]
fn a_malformed_chunk_size_is_refused() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let mut conn = Conn::open(relay.port());
    conn.send_raw(chunked_head("/v1/messages").as_bytes());
    conn.send_raw(b"zz\r\nnonsense\r\n0\r\n\r\n");
    assert_eq!(conn.read_reply().status, 400);
    assert_eq!(up.calls(), 0);
}

/// The body is still unread when a refusal is written. Reusing the connection would have the
/// next parse read that body as a request line.
#[test]
fn a_refused_request_closes_its_connection() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let mut conn = Conn::open(relay.port());
    conn.send(
        "POST",
        "/v1/messages",
        &[("x-api-key", "wrong"), ("content-type", "application/json")],
        Some(&message("m", 64)),
    );
    let reply = conn.read_reply();
    assert_eq!(reply.status, 401);
    assert_eq!(reply.header("connection"), Some("close"));
}

/// An agent branches on the kind, and tells its user the message.
#[test]
fn every_refusal_carries_its_own_kind() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let r = call(
        relay.port(),
        A,
        "/v1/messages",
        "wrong",
        Some(&message("m", 64)),
    );
    assert_eq!(r.json()["error"]["type"], "authentication_error");
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("run token")
    );
    let r = call(relay.port(), A, "/v1/nope", TOKEN, Some(&message("m", 64)));
    assert_eq!(r.json()["error"]["type"], "forbidden");
}

#[test]
fn an_unreachable_upstream_is_a_502() {
    // A port nothing listens on: bound, then released.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let notes: Notes = Arc::default();
    let captured = notes.clone();
    let mut cfg = sanduk::relay::Config::new(A, REAL_KEY, TOKEN);
    cfg.upstream = format!("127.0.0.1:{port}");
    cfg.scheme = sanduk::providers::Scheme::Http;
    cfg.note = Arc::new(move |m| captured.lock().unwrap().push(m.to_string()));
    let relay = sanduk::relay::Relay::start(cfg, "127.0.0.1", 0).unwrap();
    assert_eq!(call(relay.port(), A, "/v1/models", TOKEN, None).status, 502);
    assert!(
        notes
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.starts_with("upstream failed"))
    );
}

/// A keep-alive client that resets after its response. The call is already relayed and
/// counted, and the relay keeps serving.
#[test]
fn a_client_resetting_an_idle_connection_leaves_the_relay_serving() {
    let up = ok();
    let (relay, _) = relay(A, &up, |c| c);
    let mut conn = Conn::open(relay.port());
    conn.send("GET", "/v1/models", &[("x-api-key", TOKEN)], None);
    assert_eq!(conn.read_reply().status, 200);
    conn.reset();
    wait_until(|| relay.stats().requests == 1);
    assert_eq!(call(relay.port(), A, "/v1/models", TOKEN, None).status, 200);
}

// --- cost budget --------------------------------------------------------------------------------

const OR: &Provider = &OPENROUTER_PROVIDER;
const OR_PATH: &str = "/api/v1/chat/completions";

/// A fake OpenRouter that charges `cost` per call, holding each open for `dwell`: the window a
/// concurrent admission would slip through. `None` answers with no usage block at all.
fn costing(cost: Option<f64>, dwell: Duration) -> Upstream {
    Upstream::start(move |_, stream| {
        std::thread::sleep(dwell);
        let body = match cost {
            Some(cost) => {
                json!({"usage": {"prompt_tokens": 10, "completion_tokens": 2, "cost": cost}})
                    .to_string()
            }
            None => "{}".into(),
        };
        write_json(stream, 200, &body);
    })
}

fn budgeted(up: &Upstream, budget: Option<f64>) -> sanduk::relay::Relay {
    relay(OR, up, move |mut c| {
        c.budget = budget;
        c
    })
    .0
}

fn or_call(port: u16) -> Reply {
    call(port, OR, OR_PATH, TOKEN, Some(&message("m", 64)))
}

#[test]
fn cost_is_read_out_of_the_usage_block() {
    let up = costing(Some(0.4), Duration::ZERO);
    let relay = budgeted(&up, None);
    assert_eq!(or_call(relay.port()).status, 200);
    wait_until(|| relay.stats().requests == 1);
    assert!((relay.stats().spent - 0.4).abs() < 1e-9);
}

/// The call that crosses the line is paid for before the line is seen; the next is refused.
#[test]
fn a_budget_refuses_the_call_after_it_is_spent() {
    let up = costing(Some(0.4), Duration::ZERO);
    let relay = budgeted(&up, Some(1.0));
    for _ in 0..3 {
        assert_eq!(or_call(relay.port()).status, 200);
    }
    // 1.2 spent of 1.0: the next one is refused, not the one that crossed.
    let refused = or_call(relay.port());
    assert_eq!(refused.status, 402);
    let error = &refused.json()["error"];
    assert_eq!(error["type"], "budget_exceeded");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("$1.2000 spent against a $1.0000 ceiling")
    );
    assert_eq!(up.calls(), 3);
    assert_eq!(relay.stats().rejected, 1);
}

#[test]
fn no_budget_counts_but_does_not_stop() {
    let up = costing(Some(9.0), Duration::ZERO);
    let relay = budgeted(&up, None);
    for _ in 0..3 {
        assert_eq!(or_call(relay.port()).status, 200);
    }
    wait_until(|| relay.stats().requests == 3);
    assert!((relay.stats().spent - 27.0).abs() < 1e-9);
}

fn concurrently(port: u16, n: usize) -> Vec<u16> {
    let codes = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..n)
        .map(|_| {
            let codes = codes.clone();
            std::thread::spawn(move || {
                // Called before the lock is taken: inside it, the calls would queue on the mutex.
                let status = or_call(port).status;
                codes.lock().unwrap().push(status);
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let mut codes = codes.lock().unwrap().clone();
    codes.sort_unstable();
    codes
}

/// An agent holding the run token can open as many connections as it likes. In Python sanduk
/// before the gate, five concurrent calls billed $2.00 against a $1.00 ceiling.
#[test]
fn concurrent_calls_do_not_walk_past_the_budget() {
    let up = costing(Some(0.4), Duration::from_millis(300));
    let relay = budgeted(&up, Some(1.0));
    assert_eq!(concurrently(relay.port(), 5), [200, 200, 200, 402, 402]);
    assert_eq!(
        up.calls(),
        3,
        "a call was billed after the ceiling was crossed"
    );
    assert!((relay.stats().spent - 1.2).abs() < 1e-9);
}

/// Only a budget makes calls take turns.
#[test]
fn an_unbudgeted_relay_is_not_serialised() {
    let up = costing(Some(0.4), Duration::from_millis(300));
    let relay = budgeted(&up, None);
    let started = Instant::now();
    assert_eq!(concurrently(relay.port(), 5), [200; 5]);
    // Serialised, five 0.3s calls take 1.5s.
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

/// Zero and unknown are not the same number.
#[test]
fn a_response_with_no_cost_stops_a_budgeted_run() {
    let up = costing(None, Duration::ZERO);
    let relay = budgeted(&up, Some(1.0));
    assert_eq!(or_call(relay.port()).status, 200);
    wait_until(|| relay.stats().unpriced == 1);
    let refused = or_call(relay.port());
    assert_eq!(refused.status, 402);
    assert!(
        refused.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no cost")
    );
    assert_eq!(
        up.calls(),
        1,
        "a second call was billed against an unknown total"
    );
}

/// A refused call costs nothing, so it owes no usage block.
#[test]
fn an_upstream_error_is_not_counted_as_unpriced() {
    let up = Upstream::start(|_, stream| write_json(stream, 429, r#"{"error":"slow down"}"#));
    let relay = budgeted(&up, Some(1.0));
    assert_eq!(or_call(relay.port()).status, 429);
    wait_until(|| relay.stats().requests == 1);
    assert_eq!(relay.stats().unpriced, 0);
}

/// Reports its cost first, then streams a long tail, starting after `delay`.
fn slow_stream(cost: f64, delay: Duration) -> Upstream {
    Upstream::start(move |_, stream| {
        std::thread::sleep(delay);
        start_chunked(stream, "text/event-stream");
        let usage = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 2, "cost": cost}});
        let _ = write_chunk(stream, format!("data: {usage}\n\n").as_bytes());
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(20));
            if write_chunk(stream, &[b'x'; 4000]).is_err() {
                return;
            }
        }
        let _ = write_chunk(stream, b"");
    })
}

/// The call is billed whether or not the client stayed to read it.
#[test]
fn a_call_the_client_abandons_mid_stream_is_still_counted() {
    let up = slow_stream(0.4, Duration::ZERO);
    let relay = budgeted(&up, Some(10.0));
    let mut conn = Conn::open(relay.port());
    let headers = auth(OR, TOKEN);
    let headers: Vec<_> = headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    conn.send("POST", OR_PATH, &headers, Some(&message("m", 64)));
    conn.read_head();
    conn.read_chunk(); // the usage has arrived; the tail has not
    conn.reset();
    wait_until(|| relay.stats().requests == 1);
    assert!(
        (relay.stats().spent - 0.4).abs() < 1e-9,
        "billed work went uncounted"
    );
}

/// The client resets while the relay waits on upstream, before any header is written.
#[test]
fn a_call_the_client_leaves_before_its_headers_is_still_counted() {
    let up = slow_stream(0.4, Duration::from_millis(500));
    let relay = budgeted(&up, Some(10.0));
    let mut conn = Conn::open(relay.port());
    let headers = auth(OR, TOKEN);
    let headers: Vec<_> = headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    conn.send("POST", OR_PATH, &headers, Some(&message("m", 64)));
    std::thread::sleep(Duration::from_millis(200));
    conn.reset();
    wait_until(|| relay.stats().requests == 1);
    assert!(
        (relay.stats().spent - 0.4).abs() < 1e-9,
        "billed work went uncounted"
    );
}

/// `run` probes this before the agent starts. It needs no token, and a probe is neither a relayed
/// call nor a rejection.
#[test]
fn the_ping_answers_without_a_token_and_is_not_counted() {
    let up = ok();
    let (relay, notes) = relay(A, &up, |c| c);
    let mut conn = Conn::open(relay.port());
    conn.send("GET", sanduk::relay::PING, &[], None);
    let reply = conn.read_reply();
    assert_eq!(reply.status, 204);
    assert!(reply.body.is_empty());
    assert_eq!(relay.stats(), sanduk::relay::Stats::default());
    assert!(notes.lock().unwrap().is_empty());
    assert_eq!(up.calls(), 0);
    // Only a GET: anything else there is an ordinary request, refused without the token.
    let refused = call_with(relay.port(), sanduk::relay::PING, &[], Some(b"{}"));
    assert_eq!(refused.status, 401);
}
