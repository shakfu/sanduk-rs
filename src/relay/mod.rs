//! Host-side relay between the container and the model provider.
//!
//! In `sealed` mode the container has no route off the host, so this relay is its only path to
//! the API. Since it has to exist anyway, it also holds the key: the container presents a
//! per-run token, and the relay swaps it for the real credential.
//!
//! Each accepted request is read whole, up to [`Config::max_body`], because the body policy and
//! the body log need the JSON. The upstream exchange then runs in its own task, which drains the
//! response and charges its cost whether or not the client stays to read it.

mod usage;

pub use usage::Usage;

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::server::conn::http1 as server;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value};
use tokio::net::TcpListener;
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};

use crate::providers::{ANTHROPIC, OPENAI_CHAT, Protocol, Provider, Scheme};

/// The largest request body the relay reads. Agent requests carry the whole conversation, which
/// is megabytes at most; the cap stops a container holding the run token from filling the host's
/// memory.
pub const DEFAULT_MAX_BODY: usize = 64 << 20;

/// How long an upstream may take to answer, and to send each part of its answer. A long
/// completion streams; one that goes silent this long is not coming back.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(900);

/// Headers that describe one hop and must not be relayed to the next.
const HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Every credential header known, not just the one the active provider reads. A header inert for
/// one provider is the credential for another: `api-key` means nothing to Anthropic and is the
/// key for Azure OpenAI.
const CREDENTIAL_HEADERS: [&str; 4] = ["x-api-key", "authorization", "api-key", "x-goog-api-key"];

/// Dropped from a request: hop headers, credentials (the relay supplies its own), and what the
/// relay sets itself. `accept-encoding` is re-offered as gzip, the one encoding the usage reader
/// decodes.
fn strip_request(name: &str) -> bool {
    HOP.contains(&name)
        || CREDENTIAL_HEADERS.contains(&name)
        || ["host", "content-length", "accept-encoding"].contains(&name)
}

fn strip_response(name: &str) -> bool {
    HOP.contains(&name) || name == "content-length"
}

/// What to call a refusal, by status. An agent branches on these strings.
fn refusal_kind(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        402 => "budget_exceeded",
        413 => "request_too_large",
        _ => "forbidden",
    }
}

/// Where the relay reports what it does, one line at a time.
pub type Note = Arc<dyn Fn(&str) + Send + Sync>;

/// One run's relay policy.
pub struct Config {
    /// Written upstream in the provider's auth header. Empty for a keyless local server: no
    /// header at all is sent.
    pub api_key: String,
    /// What the container presents. Never forwarded.
    pub token: String,
    pub provider: &'static Provider,
    /// `host[:port]` requests go to; the provider's host unless `--upstream` named another.
    pub upstream: String,
    pub scheme: Scheme,
    routes: Vec<(String, Option<&'static Protocol>)>,
    pub log_bodies: bool,
    /// Where request bodies go, one file each, when `log_bodies` is set. Outside the bind mount:
    /// they hold the system prompt and every file the agent has read.
    pub log_dir: Option<PathBuf>,
    pub allow_models: Option<BTreeSet<String>>,
    pub max_tokens_cap: Option<u64>,
    /// Dollars. Enforced between calls: a streamed response reports its cost at the end, so the
    /// call that crosses the line is paid for before the line is seen.
    pub budget: Option<f64>,
    pub max_body: usize,
    pub note: Note,
}

impl Config {
    /// The provider's routes, host and scheme; no body policy; notes to stderr.
    pub fn new(
        provider: &'static Provider,
        api_key: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            token: token.into(),
            provider,
            upstream: provider.host.into(),
            scheme: provider.scheme,
            routes: provider
                .routes
                .iter()
                .map(|(path, _)| (path.to_string(), provider.protocol(path)))
                .collect(),
            log_bodies: false,
            log_dir: None,
            allow_models: None,
            max_tokens_cap: None,
            budget: None,
            max_body: DEFAULT_MAX_BODY,
            note: Arc::new(|msg| eprintln!("proxy: {msg}")),
        }
    }

    /// Replaces the allowlist. A path the provider declares keeps its protocol; one it does not
    /// (`--proxy-allow-path`) is admitted with none, so no body policy fires on it.
    pub fn allow_paths<S: Into<String>>(mut self, paths: impl IntoIterator<Item = S>) -> Self {
        self.routes = paths
            .into_iter()
            .map(|path| {
                let path = path.into();
                let protocol = self.provider.protocol(&path);
                (path, protocol)
            })
            .collect();
        self
    }

    /// The admitted paths, sorted.
    pub fn allowed(&self) -> Vec<&str> {
        let mut paths: Vec<_> = self.routes.iter().map(|(p, _)| p.as_str()).collect();
        paths.sort_unstable();
        paths.dedup();
        paths
    }

    fn admits(&self, path: &str) -> bool {
        self.routes.iter().any(|(p, _)| p == path)
    }

    /// The wire protocol of `path`, or `None` if it carries none.
    pub fn protocol(&self, path: &str) -> Option<&'static Protocol> {
        self.routes
            .iter()
            .find(|(p, _)| p == path)
            .and_then(|(_, protocol)| *protocol)
    }
}

/// What a relay has done so far.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub requests: u64,
    pub rejected: u64,
    /// Dollars, from the provider's own usage blocks.
    pub spent: f64,
    /// Completions that owed a cost and reported none. Any at all stops a budgeted run: spending
    /// against a total known to be short is what a budget exists to prevent.
    pub unpriced: u64,
}

struct State {
    cfg: Config,
    stats: Mutex<Stats>,
    body_seq: AtomicU64,
    /// Held across a budgeted call from the check to the charge, so the ceiling is crossed by one
    /// call rather than by every call in flight.
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl State {
    fn note(&self, msg: &str) {
        (self.cfg.note)(msg);
    }

    fn stats(&self) -> std::sync::MutexGuard<'_, Stats> {
        self.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A running relay. Dropping it stops it.
pub struct Relay {
    port: u16,
    state: Arc<State>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    /// Binds `host:port` (0 picks a port) and serves on a background thread.
    ///
    /// `host` is required: binding the right interface is the access control. The bridge gateway
    /// is reachable from the container and not from Wi-Fi or the LAN.
    pub fn start(cfg: Config, host: &str, port: u16) -> io::Result<Relay> {
        let state = Arc::new(State {
            cfg,
            stats: Mutex::new(Stats::default()),
            body_seq: AtomicU64::new(0),
            gate: Arc::new(tokio::sync::Mutex::new(())),
        });
        if state.cfg.scheme == Scheme::Https {
            crate::http::tls().map_err(io::Error::other)?;
        }
        HeaderValue::from_str(&state.cfg.upstream).map_err(|_| {
            io::Error::other(format!("upstream {:?} is not a host", state.cfg.upstream))
        })?;
        let listener = std::net::TcpListener::bind((host, port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sanduk-relay")
            .enable_all()
            .build()?;
        let (stop, stopped) = oneshot::channel();
        let serving = state.clone();
        let thread = std::thread::Builder::new()
            .name("sanduk-relay".into())
            .spawn(move || runtime.block_on(serve(listener, serving, stopped)))?;
        Ok(Relay {
            port,
            state,
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn config(&self) -> &Config {
        &self.state.cfg
    }

    pub fn stats(&self) -> Stats {
        *self.state.stats()
    }

    /// Stops accepting and waits for the serving thread. Calls in flight are cut off.
    pub fn shutdown(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.halt();
    }
}

async fn serve(
    listener: std::net::TcpListener,
    state: Arc<State>,
    mut stopped: oneshot::Receiver<()>,
) {
    let listener = match TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(e) => return state.note(&format!("could not listen: {e}")),
    };
    loop {
        let accepted = tokio::select! {
            _ = &mut stopped => return,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                // Out of descriptors, most likely: back off rather than spin.
                state.note(&format!("accept failed: {e}"));
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(relay(state, peer, req).await) }
            });
            // A client that resets its connection ends this with an error. The call it made is
            // already relayed and charged, so the reset is not worth a line.
            let _ = server::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// The request as the log line names it.
#[derive(Clone)]
struct Line {
    peer: IpAddr,
    method: Method,
    target: String,
}

async fn relay(state: Arc<State>, peer: SocketAddr, req: Request<Incoming>) -> Response<RelayBody> {
    let cfg = &state.cfg;
    let line = Line {
        peer: peer.ip(),
        method: req.method().clone(),
        target: req
            .uri()
            .path_and_query()
            .map_or_else(|| "/".into(), |pq| pq.to_string()),
    };
    let presented = req
        .headers()
        .get(cfg.provider.auth_header)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !token_matches(cfg.provider.presented(presented), &cfg.token) {
        let why = format!("wrong or missing run token in {}", cfg.provider.auth_header);
        return refuse(&state, &line, 401, &why);
    }
    if !cfg.admits(req.uri().path()) {
        return refuse(
            &state,
            &line,
            403,
            &format!("path not in {:?}", cfg.allowed()),
        );
    }
    if ![Method::GET, Method::POST, Method::PUT, Method::DELETE].contains(req.method()) {
        return refuse(
            &state,
            &line,
            403,
            &format!("method {} not relayed", req.method()),
        );
    }
    let gate = match cfg.budget {
        None => None,
        // One budgeted call at a time, from the check through the charge. Checking `spent` and
        // updating it after the response bounds nothing on its own: every call in flight has
        // already passed the check. Reserving credit up front would need a price for a call not
        // yet made, and the relay keeps no price table.
        Some(budget) => {
            let guard = state.gate.clone().lock_owned().await;
            let stats = *state.stats();
            if stats.unpriced > 0 {
                let why = format!(
                    "{} call(s) reported no cost, so ${:.4} is a floor rather than a total and a \
                     ${budget:.4} ceiling cannot be enforced. Re-run without --budget to continue",
                    stats.unpriced, stats.spent
                );
                return refuse(&state, &line, 402, &why);
            }
            if stats.spent >= budget {
                // Stop after crossing, not before: what the next call will cost is not knowable.
                let why = format!(
                    "over budget: ${:.4} spent against a ${budget:.4} ceiling. The call that \
                     crossed it was already paid for",
                    stats.spent
                );
                return refuse(&state, &line, 402, &why);
            }
            Some(guard)
        }
    };
    forward(&state, line, req, gate).await
}

/// Equal, in time that does not depend on where the first difference is. An empty token never
/// matches, whatever the configuration says.
fn token_matches(presented: &str, token: &str) -> bool {
    let (a, b) = (presented.as_bytes(), token.as_bytes());
    !b.is_empty()
        && a.len() == b.len()
        && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A JSON error the agent can read. `Connection: close`, because the request body may still be
/// unread in the socket: a refusal comes before it is worth reading, and a reused connection
/// would parse that body as the next request line.
fn refuse(state: &State, line: &Line, status: u16, why: &str) -> Response<RelayBody> {
    state.stats().rejected += 1;
    state.note(&format!(
        "REJECT {} {} {}: {why}",
        line.peer, line.method, line.target
    ));
    error_response(status, refusal_kind(status), why)
}

fn error_response(status: u16, kind: &str, message: &str) -> Response<RelayBody> {
    let body = serde_json::json!({"type": "error", "error": {"type": kind, "message": message}});
    let mut response = Response::new(RelayBody::Full(Some(Bytes::from(body.to_string()))));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn forward(
    state: &Arc<State>,
    line: Line,
    req: Request<Incoming>,
    gate: Option<OwnedMutexGuard<()>>,
) -> Response<RelayBody> {
    let cfg = &state.cfg;
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let body = match Limited::new(body, cfg.max_body).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            let why = format!("request body over {} bytes", cfg.max_body);
            return refuse(state, &line, 413, &why);
        }
        Err(_) => return refuse(state, &line, 400, "malformed request body"),
    };
    let protocol = cfg.protocol(parts.uri.path());
    let body = match apply_policy(cfg, protocol, body) {
        Ok(body) => body,
        Err((status, why)) => return refuse(state, &line, status, &why),
    };
    if cfg.log_bodies && !body.is_empty() {
        log_body(state, &body);
    }

    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if !strip_request(name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }
    // Checked when the relay started.
    if let Ok(host) = HeaderValue::from_str(&cfg.upstream) {
        headers.insert(header::HOST, host);
    }
    // The only place the key appears. A provider with no key (a local llama-server) gets no
    // header at all; the container's own credentials were stripped either way.
    if !cfg.api_key.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cfg.provider.auth_value(&cfg.api_key))
    {
        headers.insert(HeaderName::from_static(cfg.provider.auth_header), value);
    }
    // Narrow the offer only for a client that already accepts gzip; anything else is forwarded
    // verbatim, so `identity` stays `identity`.
    if let Some(accepted) = parts.headers.get(header::ACCEPT_ENCODING) {
        let gzip = accepted
            .to_str()
            .is_ok_and(|a| a.to_ascii_lowercase().contains("gzip"));
        let offer = if gzip {
            HeaderValue::from_static("gzip")
        } else {
            accepted.clone()
        };
        headers.insert(header::ACCEPT_ENCODING, offer);
    }
    let mut upstream = Request::new(Full::new(body));
    *upstream.method_mut() = parts.method.clone();
    *upstream.headers_mut() = headers;
    match line.target.parse() {
        Ok(uri) => *upstream.uri_mut() = uri,
        Err(_) => return refuse(state, &line, 400, "unparseable request target"),
    }

    let (head_tx, head_rx) = oneshot::channel();
    tokio::spawn(exchange(
        state.clone(),
        line,
        upstream,
        protocol,
        gate,
        started,
        head_tx,
    ));
    match head_rx.await {
        Ok(Ok((status, headers, rx))) => {
            let mut response = Response::new(RelayBody::Stream(rx));
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            response
        }
        Ok(Err(e)) => {
            state.note(&format!("upstream failed: {e}"));
            error_response(502, "api_error", "upstream unreachable")
        }
        Err(_) => error_response(502, "api_error", "upstream unreachable"),
    }
}

type Head = (StatusCode, HeaderMap, mpsc::Receiver<io::Result<Bytes>>);

/// One upstream call, from connect to charge. Runs as its own task, so a client that leaves at any
/// point cannot cut it short: the call is billed whether or not the client stays, and what it
/// cost arrives at the end of the stream.
async fn exchange(
    state: Arc<State>,
    line: Line,
    req: Request<Full<Bytes>>,
    protocol: Option<&'static Protocol>,
    gate: Option<OwnedMutexGuard<()>>,
    started: Instant,
    head_tx: oneshot::Sender<Result<Head, String>>,
) {
    let response = match tokio::time::timeout(UPSTREAM_TIMEOUT, send(&state, req)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            let _ = head_tx.send(Err(e));
            return;
        }
        Err(_) => {
            let _ = head_tx.send(Err("no answer in 900s".into()));
            return;
        }
    };
    let status = response.status();
    let text = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let mut usage = Usage::new(
        &text(header::CONTENT_TYPE),
        &text(header::CONTENT_ENCODING),
        protocol.unwrap_or(&ANTHROPIC),
    );
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers() {
        if !strip_response(name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }
    let (tx, rx) = mpsc::channel(16);
    let mut gone = head_tx.send(Ok((status, headers, rx))).is_err();
    if gone {
        state.note("client hung up before the response; still reading the cost");
    }
    let mut body = response.into_body();
    let mut sent = 0usize;
    loop {
        let failed = match tokio::time::timeout(UPSTREAM_TIMEOUT, body.frame()).await {
            Ok(None) => break,
            Ok(Some(Ok(frame))) => {
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                sent += data.len();
                usage.feed(&data);
                if !gone && tx.send(Ok(data)).await.is_err() {
                    state.note("client hung up mid-stream; still reading the cost");
                    gone = true;
                }
                continue;
            }
            Ok(Some(Err(e))) => format!("upstream broke off: {e}"),
            Err(_) => "upstream went silent for 900s".into(),
        };
        state.note(&failed);
        // An error rather than an end, so the client cannot mistake a cut stream for a whole one.
        if !gone {
            let _ = tx.send(Err(io::Error::other(failed))).await;
        }
        break;
    }
    usage.close();
    // Before the client sees the end of the response: it may send the next request the moment it
    // does, and a budget checked against a total that has not caught up would let that through.
    charge(&state, &usage, status, protocol);
    drop(tx);
    drop(gate);
    state.stats().requests += 1;
    state.note(&format!(
        "{} {} {} -> {} {sent}B {:.1}s{}",
        line.peer,
        line.method,
        line.target,
        status.as_u16(),
        started.elapsed().as_secs_f64(),
        usage.digest(protocol.is_some())
    ));
}

/// One connection per call, as the upstream is a public API and the calls are minutes apart.
async fn send(state: &State, req: Request<Full<Bytes>>) -> Result<Response<Incoming>, String> {
    crate::http::send(state.cfg.scheme, &state.cfg.upstream, req).await
}

/// Enforces model and token limits where the container cannot edit them, and adds what the
/// provider needs to report streamed usage. `Err` is a refusal: status and reason.
fn apply_policy(
    cfg: &Config,
    protocol: Option<&'static Protocol>,
    body: Bytes,
) -> Result<Bytes, (u16, String)> {
    let Some(protocol) = protocol else {
        return Ok(body);
    };
    let policed = cfg.allow_models.is_some() || cfg.max_tokens_cap.is_some();
    if body.is_empty() || (!policed && !cfg.provider.stream_usage_option) {
        return Ok(body);
    }
    let Ok(mut payload) = serde_json::from_slice::<Value>(&body) else {
        return Err((400, "body is not JSON".into()));
    };
    let Some(obj) = payload.as_object_mut() else {
        return Err((400, "body is not a JSON object".into()));
    };
    if let Some(models) = &cfg.allow_models {
        let model = obj.get("model");
        if !model
            .and_then(Value::as_str)
            .is_some_and(|m| models.contains(m))
        {
            let named = model.map_or_else(|| "none".into(), Value::to_string);
            return Err((403, format!("model {named} not allowed")));
        }
    }
    let mut edited = false;
    if let Some(cap) = cfg.max_tokens_cap {
        let asked = obj.get(protocol.cap_field).and_then(Value::as_u64);
        if asked.is_none_or(|asked| asked > cap) {
            obj.insert(protocol.cap_field.into(), cap.into());
            edited = true;
        }
    }
    // Chat Completions only: Responses rejects the field as unknown and reports usage in its
    // final event unasked.
    if cfg.provider.stream_usage_option
        && protocol.name == OPENAI_CHAT
        && obj.get("stream").is_some_and(truthy)
    {
        let mut options = match obj.get("stream_options") {
            Some(Value::Object(options)) => options.clone(),
            _ => Map::new(),
        };
        if options.get("include_usage") != Some(&Value::Bool(true)) {
            options.insert("include_usage".into(), Value::Bool(true));
            obj.insert("stream_options".into(), Value::Object(options));
            edited = true;
        }
    }
    if !edited {
        return Ok(body);
    }
    serde_json::to_vec(&payload)
        .map(Bytes::from)
        .map_err(|e| (400, e.to_string()))
}

/// JSON truthiness as the request's author meant it: `"stream": 1` streams.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// One digest line; the full body to a file when `log_dir` is set. Bodies hold the system prompt,
/// every tool schema and every file the agent has read, so they belong in a file the agent cannot
/// reach, not in terminal scrollback.
fn log_body(state: &State, body: &[u8]) {
    let seq = state.body_seq.fetch_add(1, Ordering::Relaxed) + 1;
    let payload: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let show = |v: Option<&Value>, absent: &str| match v {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => absent.into(),
    };
    let count = |key| {
        payload
            .get(key)
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };
    let mut digest = format!(
        "body {seq:03} {:.1}KB model={} max_tokens={} effort={} msgs={} tools={} stream={}",
        body.len() as f64 / 1024.0,
        show(payload.get("model"), "?"),
        show(payload.get("max_tokens"), "?"),
        show(payload.pointer("/output_config/effort"), "-"),
        count("messages"),
        count("tools"),
        show(payload.get("stream"), "false"),
    );
    if let Some(dir) = &state.cfg.log_dir {
        let path = dir.join(format!("{seq:03}.json"));
        match write_new(&path, body) {
            Ok(()) => digest += &format!(" -> {}", path.display()),
            Err(e) => digest += &format!(" -> not written: {e}"),
        }
    }
    state.note(&digest);
}

/// Creates `path`, refusing one that exists or is a symlink: the sequence is this relay's own, so
/// a name already taken is something else's, and a symlink there would put the body wherever it
/// points.
fn write_new(path: &std::path::Path, body: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)?.write_all(body)
}

/// What a call cost, or that it could not be read.
///
/// A response that owes a usage block and carries none used to count as zero, which is
/// indistinguishable from a free call. It is counted as unpriced instead, and a budgeted run
/// refuses the next call.
fn charge(state: &State, usage: &Usage, status: StatusCode, protocol: Option<&'static Protocol>) {
    let Some(field) = state.cfg.provider.cost_field else {
        return;
    };
    let mut stats = state.stats();
    match usage.get(field) {
        Some(cost) => stats.spent += cost,
        // Only where one was owed: an error costs nothing, and a path admitted by
        // --proxy-allow-path declares no protocol at all.
        None if status.as_u16() < 400 && protocol.is_some() => stats.unpriced += 1,
        None => {}
    }
}

/// A response body: a refusal, whole, or the upstream's, as the forwarding task sends it.
enum RelayBody {
    Full(Option<Bytes>),
    Stream(mpsc::Receiver<io::Result<Bytes>>),
}

impl Body for RelayBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Frame<Bytes>>>> {
        match self.get_mut() {
            RelayBody::Full(body) => Poll::Ready(body.take().map(|b| Ok(Frame::data(b)))),
            RelayBody::Stream(rx) => rx
                .poll_recv(cx)
                .map(|next| next.map(|r| r.map(Frame::data))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, RelayBody::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            RelayBody::Full(body) => {
                SizeHint::with_exact(body.as_ref().map_or(0, |b| b.len() as u64))
            }
            RelayBody::Stream(_) => SizeHint::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ANTHROPIC_PROVIDER, OPENROUTER_PROVIDER, PROVIDERS};

    /// `--proxy-allow-path` widens egress; it must not silently gain a protocol and start
    /// rewriting bodies the provider never declared.
    #[test]
    fn added_paths_are_admitted_without_policy() {
        let cfg = Config::new(&ANTHROPIC_PROVIDER, "key", "tok")
            .allow_paths(["/v1/messages", "/v1/custom"]);
        assert_eq!(cfg.allowed(), ["/v1/custom", "/v1/messages"]);
        assert_eq!(cfg.protocol("/v1/messages"), Some(&ANTHROPIC));
        assert_eq!(cfg.protocol("/v1/custom"), None);
    }

    /// Exact matching, per provider. A path is never admitted because another provider serves it.
    #[test]
    fn no_provider_admits_a_path_outside_its_own_routes() {
        for p in PROVIDERS {
            let cfg = Config::new(p, "k", "t");
            for other in PROVIDERS {
                for (path, _) in other.routes {
                    assert_eq!(cfg.admits(path), p.has_route(path), "{} {path}", p.name);
                }
            }
            assert!(!cfg.admits("/v1/models-internal-secret"));
        }
    }

    #[test]
    fn the_token_must_match_exactly_and_never_be_empty() {
        assert!(token_matches("run-token", "run-token"));
        assert!(!token_matches("run-toke", "run-token"));
        assert!(!token_matches("run-tokeN", "run-token"));
        assert!(!token_matches("", ""));
    }

    #[test]
    fn only_openrouter_is_priced() {
        let priced: Vec<_> = PROVIDERS
            .iter()
            .filter(|p| p.cost_field.is_some())
            .map(|p| p.name)
            .collect();
        assert_eq!(priced, [OPENROUTER_PROVIDER.name]);
    }
}
