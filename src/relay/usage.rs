//! Token counts read out of a relayed response as it streams past.

use std::collections::BTreeMap;
use std::io::Write;

use flate2::write::GzDecoder;
use serde_json::{Number, Value};

use crate::providers::Protocol;

/// A JSON body larger than this is not parsed for usage. A stream is read line by line and has no
/// such limit.
pub const LIMIT: usize = 1 << 18;

/// Print order. A counter the protocol does not declare is omitted rather than printed as 0.
const ORDER: [&str; 4] = ["in", "cache_write", "cache_read", "out"];

/// Usage pulled from a response.
///
/// Two shapes: server-sent events carry usage in some events (`message_start` and
/// `message_delta` for Anthropic, the final chunk for OpenAI), and a plain JSON response carries
/// it once at the top level. Only SSE lines holding `"usage"` are parsed, so a stream is still
/// forwarded chunk by chunk; a JSON body has nothing to read until it is whole, so it is buffered
/// to [`LIMIT`] and parsed at the end.
pub struct Usage {
    /// Every numeric counter seen, nested keys joined with a dot. A later event overwrites an
    /// earlier one: `message_delta`'s output count is final, `message_start`'s is not.
    pub counts: BTreeMap<String, Number>,
    protocol: &'static Protocol,
    sse: bool,
    buf: Vec<u8>,
    unzip: Option<GzDecoder<Vec<u8>>>,
}

impl Usage {
    pub fn new(content_type: &str, content_encoding: &str, protocol: &'static Protocol) -> Self {
        Self {
            counts: BTreeMap::new(),
            protocol,
            sse: content_type.contains("text/event-stream"),
            buf: Vec::new(),
            unzip: content_encoding
                .to_ascii_lowercase()
                .contains("gzip")
                .then(|| GzDecoder::new(Vec::new())),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        let Some(unzip) = self.unzip.as_mut() else {
            self.feed_plain(chunk);
            return;
        };
        if unzip.write_all(chunk).is_err() {
            // Corrupt, or not gzip after all: give up on usage, never on the relay.
            self.unzip = None;
            self.buf.clear();
            return;
        }
        let plain = std::mem::take(unzip.get_mut());
        self.feed_plain(&plain);
    }

    fn feed_plain(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if !self.sse {
            if self.buf.len() < LIMIT {
                self.buf.extend_from_slice(chunk);
            }
            return;
        }
        self.buf.extend_from_slice(chunk);
        let Some(last) = self.buf.iter().rposition(|&b| b == b'\n') else {
            return;
        };
        let rest = self.buf.split_off(last + 1);
        let lines = std::mem::replace(&mut self.buf, rest);
        for line in lines.split(|&b| b == b'\n') {
            if let Some(data) = line.strip_prefix(b"data: ")
                && contains(data, b"\"usage\"")
            {
                self.take(data);
            }
        }
    }

    /// The end of the response: flushes what the decoder still holds, and parses a JSON body.
    pub fn close(&mut self) {
        if let Some(mut unzip) = self.unzip.take() {
            if unzip.try_finish().is_ok() {
                let plain = std::mem::take(unzip.get_mut());
                self.feed_plain(&plain);
            } else {
                self.buf.clear();
            }
        }
        if !self.sse && self.buf.len() < LIMIT {
            let buf = std::mem::take(&mut self.buf);
            self.take(&buf);
        }
    }

    fn take(&mut self, raw: &[u8]) {
        let Ok(Value::Object(event)) = serde_json::from_slice::<Value>(raw) else {
            return;
        };
        // Anthropic nests it in message_start's `message`; a Responses stream in
        // response.completed's `response`. `null` and `{}` count as absent.
        let present = |v: Option<&Value>| v.filter(|v| truthy(v)).cloned();
        let nested = present(event.get("message")).or_else(|| present(event.get("response")));
        let found =
            present(event.get("usage")).or_else(|| nested.and_then(|n| present(n.get("usage"))));
        if let Some(found) = found {
            flatten(&found, "", &mut self.counts);
        }
    }

    /// A number, by the name the protocol reports it under.
    pub fn get(&self, field: &str) -> Option<f64> {
        self.counts.get(field).and_then(Number::as_f64)
    }

    /// ` in=4 cache_write=0 cache_read=0 out=112` for the log line; ` usage=?` when a completion
    /// owed usage and carried none; empty otherwise.
    pub fn digest(&self, expected: bool) -> String {
        if self.counts.is_empty() {
            return if expected {
                " usage=?".into()
            } else {
                String::new()
            };
        }
        ORDER
            .iter()
            .filter_map(|name| {
                let field = self.protocol.usage_field(name)?;
                let value = self.counts.get(field).map_or("0".into(), Number::to_string);
                Some(format!(" {name}={value}"))
            })
            .collect()
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(m) => !m.is_empty(),
        _ => true,
    }
}

/// Integer and float counters, nested keys joined with a dot. OpenAI reports cached tokens as
/// `prompt_tokens_details.cached_tokens`; a flat scan would drop it and the log line would read
/// `cache_read=0`, indistinguishable from a real miss.
fn flatten(usage: &Value, prefix: &str, out: &mut BTreeMap<String, Number>) {
    let Value::Object(map) = usage else {
        return;
    };
    for (key, value) in map {
        match value {
            Value::Number(n) => {
                out.insert(format!("{prefix}{key}"), n.clone());
            }
            Value::Object(_) => flatten(value, &format!("{prefix}{key}."), out),
            _ => {}
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ANTHROPIC, OPENAI_CHAT_PROTOCOL, OPENAI_RESPONSES_PROTOCOL};

    fn fed(content_type: &str, protocol: &'static Protocol, chunks: &[&[u8]]) -> Usage {
        let mut u = Usage::new(content_type, "", protocol);
        for chunk in chunks {
            u.feed(chunk);
        }
        u.close();
        u
    }

    /// Chunks split wherever the socket happens to, including mid-line.
    #[test]
    fn usage_survives_chunk_boundaries() {
        let stream: &[u8] = b"event: message_start\n\
            data: {\"type\":\"message_start\",\"message\":{\"usage\":\
            {\"input_tokens\":4,\"cache_creation_input_tokens\":18234,\
            \"cache_read_input_tokens\":0,\"output_tokens\":1}}}\n\n\
            event: content_block_delta\n\
            data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n\
            event: message_delta\n\
            data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":112}}\n\n";
        let mut u = Usage::new("text/event-stream", "", &ANTHROPIC);
        for chunk in stream.chunks(7) {
            u.feed(chunk);
        }
        assert_eq!(
            u.get("output_tokens"),
            Some(112.0),
            "message_delta must win"
        );
        assert_eq!(
            u.digest(false),
            " in=4 cache_write=18234 cache_read=0 out=112"
        );
    }

    #[test]
    fn usage_is_quiet_when_there_is_none() {
        let u = fed(
            "text/event-stream",
            &ANTHROPIC,
            &[b"data: {\"type\":\"ping\"}\n\n"],
        );
        assert_eq!(u.digest(false), "");
        assert_eq!(u.digest(true), " usage=?");
    }

    /// Without `stream`, usage arrives once, whole.
    #[test]
    fn a_non_streamed_json_body_is_read_at_the_end() {
        let body = br#"{"type":"message","usage":{"input_tokens":7,"cache_creation_input_tokens":13609,"cache_read_input_tokens":150898,"output_tokens":12388}}"#;
        let mut u = Usage::new("application/json", "", &ANTHROPIC);
        for chunk in body.chunks(11) {
            u.feed(chunk);
        }
        assert_eq!(
            u.digest(false),
            "",
            "nothing is parseable until the body is whole"
        );
        u.close();
        assert_eq!(
            u.digest(false),
            " in=7 cache_write=13609 cache_read=150898 out=12388"
        );
    }

    #[test]
    fn an_oversized_json_body_is_not_parsed() {
        let mut body = br#"{"usage":{"input_tokens":1},"pad":""#.to_vec();
        body.extend(vec![b'x'; LIMIT]);
        let u = fed("application/json", &ANTHROPIC, &[&body]);
        assert_eq!(u.digest(false), "");
    }

    #[test]
    fn gzipped_usage_is_decompressed() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(br#"{"usage":{"input_tokens":7,"output_tokens":12388}}"#)
            .unwrap();
        let compressed = gz.finish().unwrap();
        let mut u = Usage::new("application/json", "gzip", &ANTHROPIC);
        for chunk in compressed.chunks(5) {
            u.feed(chunk);
        }
        u.close();
        assert_eq!(
            u.digest(false),
            " in=7 cache_write=0 cache_read=0 out=12388"
        );
    }

    #[test]
    fn corrupt_gzip_gives_up_quietly() {
        let mut u = Usage::new("application/json", "gzip", &ANTHROPIC);
        u.feed(b"not actually gzip");
        u.close();
        assert_eq!(u.digest(false), "");
    }

    /// Intermediate chunks carry `"usage": null` when include_usage is on; only the final one has
    /// counts.
    #[test]
    fn openai_style_sse_usage_is_read() {
        let u = fed(
            "text/event-stream",
            &OPENAI_CHAT_PROTOCOL,
            &[
                b"data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}],\"usage\":null}\n\n",
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\
                  \"prompt_tokens_details\":{\"cached_tokens\":8}}}\n\n",
                b"data: [DONE]\n\n",
            ],
        );
        assert_eq!(u.digest(false), " in=11 cache_read=8 out=3");
    }

    /// OpenRouter's final usage chunk keeps a non-empty choices array, unlike OpenAI's.
    #[test]
    fn openrouter_style_sse_usage_is_read() {
        let u = fed(
            "text/event-stream",
            &OPENAI_CHAT_PROTOCOL,
            &[
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\
                \"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"cost\":0.4}}\n\n",
            ],
        );
        assert_eq!(u.digest(false), " in=5 cache_read=0 out=2");
        assert_eq!(u.get("cost"), Some(0.4));
    }

    #[test]
    fn responses_usage_is_read_with_its_own_nesting() {
        let u = fed(
            "application/json",
            &OPENAI_RESPONSES_PROTOCOL,
            &[br#"{"usage":{"input_tokens":40,"output_tokens":9,"input_tokens_details":{"cached_tokens":32}}}"#],
        );
        assert_eq!(u.digest(false), " in=40 cache_read=32 out=9");
    }

    /// Earlier events carry the response object with `"usage": null`.
    #[test]
    fn responses_sse_usage_is_read_from_the_completed_event() {
        let u = fed(
            "text/event-stream",
            &OPENAI_RESPONSES_PROTOCOL,
            &[
                b"data: {\"type\":\"response.created\",\"response\":{\"usage\":null}}\n\n",
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":40,\
                  \"output_tokens\":9,\"input_tokens_details\":{\"cached_tokens\":32}}}}\n\n",
            ],
        );
        assert_eq!(u.digest(false), " in=40 cache_read=32 out=9");
    }

    #[test]
    fn crlf_lines_are_read() {
        let u = fed(
            "text/event-stream",
            &ANTHROPIC,
            &[b"data: {\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}\r\n\r\n"],
        );
        assert_eq!(u.get("output_tokens"), Some(3.0));
    }
}
