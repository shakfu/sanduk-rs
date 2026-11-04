//! The key check: one cheap request before anything starts.

use sanduk_core::preflight;

/// Answers every request with `status`, once per connection.
fn answering(status: u16) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    addr
}

use sanduk_core::providers::{ANTHROPIC_PROVIDER, OPENAI_COMPAT_PROVIDER, Scheme};

/// A bad key fails here in milliseconds, not after minutes of in-container retries.
#[test]
fn a_rejected_key_stops_the_run_and_other_statuses_do_not() {
    let addr = answering(401);
    let refused = preflight::validate_key("sk-bad", Scheme::Http, &addr, "", &ANTHROPIC_PROVIDER)
        .unwrap_err();
    assert!(
        refused.message.contains("ANTHROPIC_API_KEY rejected") && refused.message.contains("401"),
        "{}",
        refused.message
    );
    // A 500 proves the endpoint answered; the agent is left to try.
    preflight::validate_key("sk", Scheme::Http, &answering(500), "", &ANTHROPIC_PROVIDER).unwrap();
}

#[test]
fn an_unreachable_endpoint_is_named() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let refused = preflight::validate_key(
        "sk",
        Scheme::Http,
        &format!("127.0.0.1:{port}"),
        "",
        &ANTHROPIC_PROVIDER,
    )
    .unwrap_err();
    assert!(
        refused
            .message
            .starts_with("cannot reach http://127.0.0.1:"),
        "{}",
        refused.message
    );
}

#[test]
fn a_keyless_provider_is_not_checked() {
    preflight::validate_key("", Scheme::Http, "127.0.0.1:1", "", &OPENAI_COMPAT_PROVIDER).unwrap();
}
