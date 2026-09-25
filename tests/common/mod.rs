//! A fake upstream and a raw HTTP/1.1 client, both blocking, so a test controls every byte: chunk
//! boundaries, keep-alive, and when a connection goes away.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sanduk::providers::{Provider, Scheme};
use sanduk::relay::{Config, Relay};
use serde_json::Value;

pub const REAL_KEY: &str = "sk-ant-api03-REAL-KEY-STAYS-ON-HOST";
pub const TOKEN: &str = "run-token-for-tests";

/// A request as the upstream received it. Header names are lowercase.
#[derive(Debug, Clone)]
pub struct Received {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Received {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

type Respond = dyn Fn(&Received, &mut TcpStream) + Send + Sync;

/// A plaintext upstream on 127.0.0.1. One request per connection, as the relay makes them.
pub struct Upstream {
    pub addr: String,
    seen: Arc<Mutex<Vec<Received>>>,
    calls: Arc<AtomicUsize>,
}

impl Upstream {
    pub fn start(respond: impl Fn(&Received, &mut TcpStream) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let respond: Arc<Respond> = Arc::new(respond);
        let (s, c) = (seen.clone(), calls.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (s, c, respond) = (s.clone(), c.clone(), respond.clone());
                std::thread::spawn(move || {
                    let Some(req) = read_request(&mut BufReader::new(stream.try_clone().unwrap()))
                    else {
                        return;
                    };
                    s.lock().unwrap().push(req.clone());
                    c.fetch_add(1, Ordering::SeqCst);
                    respond(&req, &mut stream);
                });
            }
        });
        Upstream { addr, seen, calls }
    }

    /// Answers every request 200 with `body` as JSON.
    pub fn json(body: &'static str) -> Self {
        Self::start(move |_, stream| write_json(stream, 200, body))
    }

    pub fn last(&self) -> Received {
        wait_until(|| !self.seen.lock().unwrap().is_empty());
        self.seen.lock().unwrap().last().unwrap().clone()
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

pub fn write_json(stream: &mut TcpStream, status: u16, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
}

/// Starts a chunked response; send the parts with [`write_chunk`] and end with an empty one.
pub fn start_chunked(stream: &mut TcpStream, content_type: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
}

pub fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    stream.write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Received> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let headers = read_headers(reader);
    let length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Received {
        method,
        target,
        headers,
        body,
    })
}

fn read_headers(reader: &mut impl BufRead) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    headers
}

/// A response as the client read it.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// One client connection, kept open across requests.
pub struct Conn {
    pub reader: BufReader<TcpStream>,
    pub stream: TcpStream,
}

impl Conn {
    pub fn open(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        Conn {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
        }
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    /// A request with a Content-Length.
    pub fn send(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: relay\r\n");
        for (name, value) in headers {
            head += &format!("{name}: {value}\r\n");
        }
        if let Some(body) = body {
            head += &format!("Content-Length: {}\r\n", body.len());
        }
        head += "\r\n";
        self.send_raw(head.as_bytes());
        if let Some(body) = body {
            self.send_raw(body);
        }
    }

    pub fn read_head(&mut self) -> (u16, Vec<(String, String)>) {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no status line: {line:?}"));
        (status, read_headers(&mut self.reader))
    }

    /// The next chunk of a chunked body; `None` at the terminator or the end of the connection.
    pub fn read_chunk(&mut self) -> Option<Vec<u8>> {
        let mut size = String::new();
        if self.reader.read_line(&mut size).ok()? == 0 {
            return None;
        }
        let size = usize::from_str_radix(size.trim(), 16).ok()?;
        if size == 0 {
            let mut end = String::new();
            let _ = self.reader.read_line(&mut end);
            return None;
        }
        let mut data = vec![0; size + 2];
        self.reader.read_exact(&mut data).ok()?;
        data.truncate(size);
        Some(data)
    }

    pub fn read_reply(&mut self) -> Reply {
        let (status, headers) = self.read_head();
        let find = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        let body = if find("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
            let mut body = Vec::new();
            while let Some(chunk) = self.read_chunk() {
                body.extend(chunk);
            }
            body
        } else if let Some(length) = find("content-length").and_then(|v| v.parse().ok()) {
            let mut body = vec![0; length];
            self.reader.read_exact(&mut body).unwrap();
            body
        } else {
            let mut body = Vec::new();
            let _ = self.reader.read_to_end(&mut body);
            body
        };
        Reply {
            status,
            headers,
            body,
        }
    }

    /// Closes with RST rather than FIN, as a client that resets does.
    pub fn reset(self) {
        use std::os::fd::AsRawFd;
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a valid descriptor and a correctly sized option value.
        unsafe {
            libc::setsockopt(
                self.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&raw const linger).cast(),
                size_of::<libc::linger>() as libc::socklen_t,
            );
        }
    }
}

/// The run token in the header `provider` reads, and a JSON content type.
pub fn auth(provider: &Provider, token: &str) -> Vec<(String, String)> {
    vec![
        (provider.auth_header.to_string(), provider.auth_value(token)),
        ("content-type".into(), "application/json".into()),
    ]
}

/// One request on a fresh connection.
pub fn call(port: u16, provider: &Provider, path: &str, token: &str, body: Option<&[u8]>) -> Reply {
    call_with(port, path, &auth(provider, token), body)
}

pub fn call_with(
    port: u16,
    path: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Reply {
    let headers: Vec<(&str, &str)> = headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    let mut conn = Conn::open(port);
    let method = if body.is_some() { "POST" } else { "GET" };
    conn.send(method, path, &headers, body);
    conn.read_reply()
}

pub type Notes = Arc<Mutex<Vec<String>>>;

/// A relay in front of `upstream`, over plaintext, with its notes captured.
pub fn relay(
    provider: &'static Provider,
    upstream: &Upstream,
    tweak: impl FnOnce(Config) -> Config,
) -> (Relay, Notes) {
    let notes: Notes = Arc::default();
    let mut cfg = Config::new(provider, REAL_KEY, TOKEN);
    cfg.upstream = upstream.addr.clone();
    cfg.scheme = Scheme::Http;
    let captured = notes.clone();
    cfg.note = Arc::new(move |msg| captured.lock().unwrap().push(msg.to_string()));
    let relay = Relay::start(tweak(cfg), "127.0.0.1", 0).unwrap();
    (relay, notes)
}

pub fn message(model: &str, max_tokens: u64) -> Vec<u8> {
    serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": [{"role": "user", "content": "hi"}],
    })
    .to_string()
    .into_bytes()
}

pub fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The last line the relay noted for a relayed call, once there is one.
pub fn relayed_line(notes: &Notes) -> String {
    wait_until(|| notes.lock().unwrap().iter().any(|n| n.contains(" -> ")));
    notes
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|n| n.contains(" -> "))
        .unwrap()
        .clone()
}
