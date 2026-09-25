//! One HTTP/1.1 connection per request, over TLS or not: what the relay forwards on, and what the
//! key check sends. rustls with ring and the system's trust store.

use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, pki_types::ServerName};

use crate::providers::Scheme;

/// Built once per process, from the system's trust store.
pub fn tls() -> Result<TlsConnector, String> {
    static CONNECTOR: OnceLock<Result<TlsConnector, String>> = OnceLock::new();
    CONNECTOR
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            let found = rustls_native_certs::load_native_certs();
            for cert in found.certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err(format!(
                    "no trusted root certificates found on this host: {:?}",
                    found.errors
                ));
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Ok(TlsConnector::from(Arc::new(config)))
        })
        .clone()
}

/// Sends `req` to `host[:port]` and returns the response head, its body still streaming.
pub async fn send(
    scheme: Scheme,
    host: &str,
    req: Request<Full<Bytes>>,
) -> Result<Response<Incoming>, String> {
    let tcp = TcpStream::connect(connect_addr(host, scheme))
        .await
        .map_err(|e| format!("connecting to {host}: {e}"))?;
    match scheme {
        Scheme::Http => request(TokioIo::new(tcp), req).await,
        Scheme::Https => {
            let name = ServerName::try_from(host_only(host).to_string())
                .map_err(|e| format!("{host}: {e}"))?;
            let stream = tls()?
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS to {host}: {e}"))?;
            request(TokioIo::new(stream), req).await
        }
    }
}

async fn request<T>(io: T, req: Request<Full<Bytes>>) -> Result<Response<Incoming>, String>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender.send_request(req).await.map_err(|e| e.to_string())
}

/// `host` from `host:port` or `[v6]:port`.
pub fn host_only(upstream: &str) -> &str {
    if let Some(rest) = upstream.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match upstream.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => upstream,
    }
}

/// `host:port`, with the scheme's port where the upstream names none: a provider's host is a bare
/// name, and a socket address needs a port.
pub fn connect_addr(upstream: &str, scheme: Scheme) -> String {
    let has_port = match upstream.strip_prefix('[') {
        Some(rest) => rest
            .split_once(']')
            .is_some_and(|(_, after)| after.starts_with(':')),
        None => upstream
            .rsplit_once(':')
            .is_some_and(|(_, port)| port.bytes().all(|b| b.is_ascii_digit())),
    };
    if has_port {
        return upstream.to_string();
    }
    let port = match scheme {
        Scheme::Http => 80,
        Scheme::Https => 443,
    };
    format!("{upstream}:{port}")
}

/// A GET's status, on a runtime of its own: for a caller that is not async.
pub fn get_status(
    scheme: Scheme,
    host: &str,
    path: &str,
    headers: &[(&str, String)],
    timeout: std::time::Duration,
) -> Result<u16, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let mut req = Request::get(path).header(hyper::header::HOST, host);
        for (name, value) in headers {
            req = req.header(*name, value);
        }
        let req = req
            .body(Full::new(Bytes::new()))
            .map_err(|e| e.to_string())?;
        match tokio::time::timeout(timeout, send(scheme, host, req)).await {
            Ok(Ok(response)) => Ok(response.status().as_u16()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!("no answer in {}s", timeout.as_secs())),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_is_given_its_schemes_port() {
        assert_eq!(
            connect_addr("api.anthropic.com", Scheme::Https),
            "api.anthropic.com:443"
        );
        assert_eq!(connect_addr("example.com", Scheme::Http), "example.com:80");
        assert_eq!(
            connect_addr("127.0.0.1:8080", Scheme::Http),
            "127.0.0.1:8080"
        );
        assert_eq!(connect_addr("[::1]:8080", Scheme::Http), "[::1]:8080");
        assert_eq!(connect_addr("[::1]", Scheme::Https), "[::1]:443");
    }

    #[test]
    fn the_host_is_taken_from_every_upstream_spelling() {
        assert_eq!(host_only("api.openai.com"), "api.openai.com");
        assert_eq!(host_only("openrouter.ai:443"), "openrouter.ai");
        assert_eq!(host_only("[::1]:8080"), "::1");
    }
}
