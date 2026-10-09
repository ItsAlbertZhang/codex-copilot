//! The HTTP SSE transport: `POST /responses`, answered with
//! `text/event-stream`.
//!
//! Codex is configured for the WebSocket, but that is not the only transport
//! it uses. After `stream_max_retries` consecutive retryable failures (a
//! dropped network, a 502 or 504 from the relay, an upstream reset) it calls
//! `try_switch_fallback_transport` (`core/src/responses_retry.rs`), which
//! disables WebSockets for the rest of the session (`force_http_fallback`,
//! `core/src/client.rs`): every later turn is a `POST /responses`. A relay
//! that only spoke WebSocket would leave that thread dead until Codex
//! restarts, so this transport is relayed too, with the same rewrites:
//!
//! * the request body is a `response.create` frame without its `type`, and
//!   goes through [`Normalizer::upstream_http`];
//! * every event's `data` is the JSON the socket would have carried, and goes
//!   through [`Normalizer::downstream`] ([`Reframer`]).
//!
//! Codex reads the stream with `eventsource-stream` and only looks at each
//! event's `data` (`codex-api/src/sse/responses.rs`), so each event goes out
//! as its `data:` lines alone (one line when rewritten); `event:`, `id:`,
//! `retry:` and comment lines are dropped. Anything that is not an
//! uncompressed event stream (errors included) is mirrored as is. An event
//! larger than one WebSocket message ends the stream with an error.

use axum::body::{Body, Bytes};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::Response;
use axum::BoxError;
use futures_util::{stream, Stream, StreamExt};
use serde_json::Value;
use tracing::{debug, info, warn};

use super::normalize::Normalizer;
use super::{bridge, conn_id, error_json, headers, mirror, upstream_error, Shared};

/// Largest request body accepted: the same bound as one WebSocket message,
/// which is what the body would have been on the socket.
const MAX_BODY: usize = bridge::MAX_MESSAGE;

/// Largest event the reframer buffers (its data so far plus the partial
/// line), for the same reason. Past it the upstream is not sending events
/// the relay can rewrite.
const MAX_EVENT: usize = bridge::MAX_MESSAGE;

/// Relays one `POST /responses` (the route handler has checked the method).
pub(super) async fn forward(shared: &Shared, parts: Parts, body: Body) -> Response {
    let conn = conn_id("http");
    let body = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(body) => body,
        Err(err) => {
            // Mostly the limit; a body that broke on the way gets the same.
            warn!(conn, error = %err, "request body refused");
            return error_json(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                format!("request bodies are limited to {MAX_BODY} bytes"),
            );
        }
    };
    let mut norm = Normalizer::new();
    let body = prepare_body(body, &mut norm, shared.cfg.review_model.as_deref(), &conn);

    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or("/responses", |pq| pq.as_str());
    let url = headers::http_url(&shared.cfg.upstream, path_and_query);
    let mut outgoing = headers::sse_request(&parts.headers);
    headers::inject_identity(&mut outgoing);
    let reply = match shared
        .http
        .post(&url)
        .headers(outgoing)
        .body(body)
        .send()
        .await
    {
        Ok(reply) => reply,
        Err(err) => {
            let error = upstream_error(&err, &shared.route);
            warn!(conn, %url, error = %error, "upstream request failed");
            return error_json(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                format!("POST {url}: {error}"),
            );
        }
    };

    let status = reply.status();
    if !headers::is_plain_event_stream(reply.headers()) {
        info!(conn, %status, "responses over HTTP, mirrored (not an event stream)");
        return mirror(reply);
    }
    info!(conn, %status, "responses over HTTP");
    let response_headers = headers::sse_response(reply.headers());
    let events = normalized(reply.bytes_stream(), Reframer::new(norm, conn));
    let mut out = Response::new(Body::from_stream(events));
    *out.status_mut() = status;
    *out.headers_mut() = response_headers;
    out
}

/// The request body, ready for the upstream. Unchanged bodies (and anything
/// that is not JSON, a compressed body for one) go out byte for byte.
fn prepare_body(
    body: Bytes,
    norm: &mut Normalizer,
    review_model: Option<&str>,
    conn: &str,
) -> Bytes {
    let Ok(mut request) = serde_json::from_slice::<Value>(&body) else {
        debug!(conn, bytes = body.len(), "request body (not JSON)");
        return body;
    };
    let changed = norm.upstream_http(&mut request, review_model);
    debug!(conn, bytes = body.len(), changed, "request body");
    if changed {
        Bytes::from(request.to_string())
    } else {
        body
    }
}

/// The upstream event stream, re-framed event by event. An upstream read
/// error, or an event too large to buffer, ends the client's body with an
/// error, so Codex sees a broken stream (and retries) rather than an orderly
/// end or an event with Copilot's raw ids.
fn normalized<S, E>(upstream: S, reframer: Reframer) -> impl Stream<Item = Result<Bytes, BoxError>>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: Into<BoxError>,
{
    let state = (Box::pin(upstream), reframer, false);
    stream::unfold(state, |(mut upstream, mut r, done)| async move {
        if done {
            return None;
        }
        let out = match upstream.next().await {
            Some(Ok(chunk)) => r.push(&chunk),
            Some(Err(err)) => Err(err.into()),
            None => {
                debug!(conn = r.conn.as_str(), "upstream event stream ended");
                return Some((Ok(Bytes::from(r.finish())), (upstream, r, true)));
            }
        };
        if let Err(err) = &out {
            warn!(conn = r.conn.as_str(), error = %err, "upstream event stream cut off");
        }
        let done = out.is_err();
        Some((out.map(Bytes::from), (upstream, r, done)))
    })
}

/// Re-frames one event stream, normalizing each event's JSON on the way.
/// Codex only reads each event's `data`, so that is all that goes out: one
/// `data:` line per event, or the original lines when the data is not JSON
/// or has nothing to rewrite. Comments and `event:` / `id:` / `retry:` lines
/// are dropped.
pub(super) struct Reframer {
    norm: Normalizer,
    conn: String,
    /// The line being received (its LF not seen yet).
    line: Vec<u8>,
    /// The event's `data` values so far, joined with LF; `None` before its
    /// first `data:` line.
    data: Option<Vec<u8>>,
    /// Most bytes one event may buffer.
    limit: usize,
}

impl Reframer {
    pub(super) fn new(norm: Normalizer, conn: impl Into<String>) -> Self {
        Self {
            norm,
            conn: conn.into(),
            line: Vec::new(),
            data: None,
            limit: MAX_EVENT,
        }
    }

    /// Takes the next upstream chunk; returns every event it completed, or an
    /// error once an event outgrows the limit.
    pub(super) fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, BoxError> {
        let mut out = Vec::new();
        for (i, piece) in chunk.split(|&b| b == b'\n').enumerate() {
            if i > 0 {
                self.end_line(&mut out);
            }
            self.line.extend_from_slice(piece);
        }
        if self.line.len() + self.data.as_ref().map_or(0, Vec::len) > self.limit {
            let limit = self.limit;
            return Err(format!("an upstream event exceeded {limit} bytes").into());
        }
        Ok(out)
    }

    /// The end of the stream: an event the upstream never terminated goes
    /// out unterminated (whether it counts is the client's call), normalized.
    pub(super) fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut out);
        }
        let complete = out.len();
        self.dispatch(&mut out);
        if out.len() > complete {
            out.pop();
        }
        out
    }

    /// Takes the complete line in `line` (LF or CRLF): a blank one ends the
    /// event, a `data:` one adds to it, anything else is dropped.
    fn end_line(&mut self, out: &mut Vec<u8>) {
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            self.dispatch(out);
        } else if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            match &mut self.data {
                Some(data) => {
                    data.push(b'\n');
                    data.extend_from_slice(value);
                }
                None => self.data = Some(value.to_vec()),
            }
        }
    }

    /// Writes the event read so far, if it had data, with its blank line.
    fn dispatch(&mut self, out: &mut Vec<u8>) {
        let Some(data) = self.data.take() else {
            return;
        };
        let conn = self.conn.as_str();
        let rewritten = match serde_json::from_slice::<Value>(&data) {
            Ok(mut event) => {
                let changed = self.norm.downstream(&mut event);
                let kind = event.get("type").and_then(Value::as_str).unwrap_or("?");
                debug!(conn, kind, bytes = data.len(), changed, "upstream event");
                changed.then(|| event.to_string().into_bytes())
            }
            Err(_) => {
                debug!(conn, bytes = data.len(), "upstream event (not JSON)");
                None
            }
        };
        // Serialized JSON has no raw line break, so it is one line.
        for line in rewritten.unwrap_or(data).split(|&b| b == b'\n') {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(line);
            out.push(b'\n');
        }
        out.push(b'\n');
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// LF and CRLF line breaks; a comment-only block; data over several
    /// lines (one with no space after the colon, one with two); an `id:` line
    /// after the data; data that is not JSON; and an event with no ids.
    const STREAM: &str = concat!(
        ": hello\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,",
        "\"item_id\":\"a+/\",\"delta\":\"Hi\"}\n\n",
        "event: response.output_item.done\r\n",
        "data: {\"type\":\"response.output_item.done\",\r\n",
        "data:\"output_index\":0,\r\n",
        "data:  \"item\":{\"id\":\"b\",\"type\":\"message\"}}\r\n",
        "id: 7\r\n\r\n",
        "data: not\ndata: json\n\n",
        "event: response.created\r\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r+1\"}}\r\n\r\n",
    );

    const EXPECTED: &str = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,",
        "\"item_id\":\"copilot-s-1\",\"delta\":\"Hi\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
        "\"item\":{\"id\":\"copilot-s-0\",\"type\":\"message\"}}\n\n",
        "data: not\ndata: json\n\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r+1\"}}\n\n",
    );

    fn reframer() -> Reframer {
        Reframer::new(Normalizer::with_stream("s"), "t")
    }

    fn run(chunks: &[&[u8]]) -> String {
        let mut r = reframer();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(r.push(chunk).unwrap());
        }
        out.extend(r.finish());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn events_are_rewritten_whatever_the_chunking() {
        let bytes = STREAM.as_bytes();
        for split in 0..=bytes.len() {
            let (a, b) = bytes.split_at(split);
            assert_eq!(run(&[a, b]), EXPECTED, "split at {split}");
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(run(&singles), EXPECTED);
    }

    #[test]
    fn an_unterminated_last_event_is_normalized_but_left_unterminated() {
        let tail = "data: {\"output_index\":2,\"item_id\":\"q\"}";
        let expected = "data: {\"output_index\":2,\"item_id\":\"copilot-s-2\"}\n";
        assert_eq!(run(&[tail.as_bytes()]), expected);
        assert_eq!(run(&[b"event: x\nda"]), "");
        assert_eq!(run(&[]), "");
    }

    #[tokio::test]
    async fn an_upstream_error_or_an_oversized_event_ends_the_stream() {
        let event = json!({"type":"response.output_item.added","output_index":0,
                           "item":{"id":"x","type":"message"}});
        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from(format!("data: {event}\n\ndata: {{\"cut"))),
            Err("connection reset"),
            Ok(Bytes::from_static(b"never read")),
        ];
        let out: Vec<_> = normalized(stream::iter(chunks), reframer()).collect().await;
        assert_eq!(out.len(), 2, "{out:?}");
        let first = String::from_utf8(out[0].as_ref().unwrap().to_vec()).unwrap();
        assert!(first.contains("\"id\":\"copilot-s-0\""), "{first}");
        assert!(first.ends_with("\n\n"), "{first}");
        assert_eq!(out[1].as_ref().unwrap_err().to_string(), "connection reset");

        let chunks: Vec<Result<Bytes, &str>> = vec![
            Ok(Bytes::from_static(
                b"data: {\"output_index\":1,\"item_id\":\"a\"}\n\n",
            )),
            Ok(Bytes::from(vec![b'x'; 65])),
            Ok(Bytes::from_static(b"never read")),
        ];
        let mut r = reframer();
        r.limit = 64;
        let out: Vec<_> = normalized(stream::iter(chunks), r).collect().await;
        assert_eq!(out.len(), 2, "{out:?}");
        let err = out[1].as_ref().unwrap_err().to_string();
        assert_eq!(err, "an upstream event exceeded 64 bytes");
    }

    #[test]
    fn unchanged_request_bodies_are_forwarded_byte_for_byte() {
        let mut norm = Normalizer::new();
        let body = Bytes::from_static(b"{ \"model\" : \"gpt-6-astra\", \"input\": [] }");
        let out = prepare_body(body.clone(), &mut norm, Some("gpt-6-luna"), "t");
        assert_eq!(out, body);
        assert!(norm.stream().is_some(), "every body opens a namespace");

        let zstd = Bytes::from_static(&[0x28, 0xb5, 0x2f, 0xfd, 0, 1]);
        assert_eq!(prepare_body(zstd.clone(), &mut norm, None, "t"), zstd);

        let body = json!({"model":crate::CODEX_AUTO_REVIEW,"input":[
            {"type":"message","id":"copilot-a-0","role":"assistant","content":[]}]});
        let out = prepare_body(Bytes::from(body.to_string()), &mut norm, Some("m"), "t");
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out["model"], "m");
        assert!(out["input"][0].get("id").is_none());
    }
}
