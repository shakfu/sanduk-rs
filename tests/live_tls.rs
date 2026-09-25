//! Against the real endpoint, over TLS. Ignored by default: it needs the network.
//!
//! ```text
//! cargo test -p sanduk --test live_tls -- --ignored
//! ```
//!
//! No API call is billed: the key is invalid, and the 401 the provider answers with is itself
//! proof that the request got there through TLS, with the relay's credential in its header.

mod common;

use common::*;
use sanduk::providers::ANTHROPIC_PROVIDER;
use sanduk::relay::{Config, Relay};

#[test]
#[ignore]
fn an_invalid_key_earns_the_providers_own_401_over_tls() {
    let cfg = Config::new(
        &ANTHROPIC_PROVIDER,
        "sk-ant-invalid-key-for-a-relay-test",
        TOKEN,
    );
    let relay = Relay::start(cfg, "127.0.0.1", 0).unwrap();
    let mut headers = auth(&ANTHROPIC_PROVIDER, TOKEN);
    headers.push(("anthropic-version".into(), "2023-06-01".into()));
    let reply = call_with(relay.port(), "/v1/models", &headers, None);
    assert_eq!(
        reply.status,
        401,
        "{:?}",
        String::from_utf8_lossy(&reply.body)
    );
    // The provider's error shape, not the relay's: the relay's 401 names the run token.
    let message = reply.json()["error"]["message"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(!message.contains("run token"), "{message}");
    assert_eq!(reply.json()["error"]["type"], "authentication_error");
}
