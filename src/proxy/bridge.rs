//! The WebSocket bridge: one upstream connection per client connection.
//!
//! The upstream handshake runs before the client gets its 101, so a refusal
//! (401, 403, 429...) reaches Codex as the same HTTP status it would have seen
//! talking to Copilot directly, and the upstream's 101 extras (Codex reads
//! `x-reasoning-included`, `openai-model`, `x-codex-turn-state` there) can be
//! copied onto the client's 101.
//!
//! The upstream handshake is an HTTP/1.1 upgrade sent with the relay's one
//! reqwest client, so it goes through the same proxy as the HTTP transports
//! (see [`dial`]).

use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::{debug, info, warn};

use super::normalize::Normalizer;
use super::{conn_id, error_json, headers, upstream_error, Shared};

/// Bounds the upstream handshake (through a proxy also the connection to it
/// and its `CONNECT` reply, then TCP, TLS and the HTTP upgrade). Shorter than
/// Codex's own 15 s WebSocket connect timeout, so that Codex gets the
/// relay's 504 with a reason instead of a bare timeout of its own.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

/// How long closing handshakes get once one side is done.
pub(super) const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// Largest message and frame accepted on either hop, and largest refusal
/// body relayed. The libraries' defaults (64 MiB messages, 16 MiB frames)
/// are below what one `response.create` with a long history and inline
/// images, or one `response.completed` echoing it, can reach; Codex itself
/// sends each message as one frame.
pub(super) const MAX_MESSAGE: usize = 256 << 20;

/// Close code for "the other side failed" (RFC 6455 section 7.4.1).
const INTERNAL_ERROR: u16 = 1011;
/// Close code for "going away": the client vanished or the relay stops.
const GOING_AWAY: u16 = 1001;

/// The upstream WebSocket runs on the connection reqwest upgraded, directly
/// or through a proxy.
type Upstream = tokio_tungstenite::WebSocketStream<reqwest::Upgraded>;

/// Why the upstream WebSocket did not open.
#[derive(Debug)]
enum DialError {
    /// The upstream answered the handshake with something other than 101:
    /// its status, headers and body, for [`mirror_refusal`].
    Refused(StatusCode, HeaderMap, Bytes),
    /// No answer, or a broken one; the message names the cause.
    Failed(String),
}

/// Everything one bridge needs after the handshake.
struct Ctx {
    conn: String,
    /// Upstream URL, for the close log line.
    url: String,
    review_model: Option<String>,
    shutdown: watch::Receiver<bool>,
    /// Held while the bridge runs; the server waits for every clone to drop.
    _alive: mpsc::Sender<()>,
}

/// Why a bridge ended.
#[derive(Debug)]
enum End {
    /// The client sent a close frame (already forwarded upstream).
    ClientClosed,
    /// The client stream failed or ended without a close frame.
    ClientLost(String),
    /// The upstream sent a close frame (already forwarded to the client).
    UpstreamClosed,
    /// The upstream stream failed or ended without a close frame.
    UpstreamLost(String),
    /// The relay is shutting down.
    Shutdown,
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            End::ClientClosed => f.write_str("client closed"),
            End::ClientLost(why) => write!(f, "client lost: {why}"),
            End::UpstreamClosed => f.write_str("upstream closed"),
            End::UpstreamLost(why) => write!(f, "upstream lost: {why}"),
            End::Shutdown => f.write_str("relay shutting down"),
        }
    }
}

/// Handles a valid upgrade request on `/responses`: opens the upstream
/// WebSocket, then either mirrors its refusal or upgrades the client and
/// starts pumping.
pub(super) async fn open(
    shared: &Shared,
    upgrade: WebSocketUpgrade,
    uri: &Uri,
    client_headers: &HeaderMap,
) -> Response {
    let conn = conn_id("ws");
    let path_and_query = uri.path_and_query().map_or("/responses", |pq| pq.as_str());
    let url = headers::http_url(&shared.cfg.upstream, path_and_query);
    let route = shared.route.as_str();
    debug!(conn, %url, proxy = shared.proxy.as_deref().unwrap_or("none"), "connecting upstream");
    // One bound for the whole chain, the proxy's part included.
    let connect = dial(shared, &url, client_headers);
    let (upstream, handshake) = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(DialError::Refused(status, headers, body))) => {
            warn!(conn, %url, %status, "upstream refused the WebSocket handshake");
            return mirror_refusal(status, &headers, body);
        }
        Ok(Err(DialError::Failed(err))) => {
            warn!(conn, %url, error = %err, "upstream WebSocket connect failed");
            return error_json(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                format!("could not open the upstream WebSocket {url}: {err}"),
            );
        }
        Err(_) => {
            warn!(conn, %url, "upstream WebSocket handshake timed out");
            return error_json(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                format!(
                    "no WebSocket handshake from {url} within {}s{route}",
                    CONNECT_TIMEOUT.as_secs(),
                ),
            );
        }
    };
    info!(conn, %url, "bridge open");

    let extras = headers::ws_response(&handshake);
    let failed_conn = conn.clone();
    let ctx = Ctx {
        conn,
        url,
        review_model: shared.cfg.review_model.clone(),
        shutdown: shared.shutdown.subscribe(),
        _alive: shared.bridges.clone(),
    };
    let mut reply = upgrade
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_failed_upgrade(move |err| {
            warn!(conn = failed_conn, error = %err, "client WebSocket upgrade failed");
        })
        .on_upgrade(move |client| run(client, upstream, ctx));
    // Appended after axum's own 101 headers, none of which can collide:
    // `ws_response` drops connection / upgrade / sec-websocket-*.
    for (name, value) in &extras {
        reply.headers_mut().append(name.clone(), value.clone());
    }
    reply
}

/// Opens the upstream WebSocket: a `GET` with the upgrade headers through
/// the relay's reqwest client (its proxy, TLS and TCP_NODELAY included),
/// then, on a 101 with the right `Sec-WebSocket-Accept`, the upgraded
/// connection as a WebSocket. Returns it with the 101's headers.
async fn dial(
    shared: &Shared,
    url: &str,
    client_headers: &HeaderMap,
) -> Result<(Upstream, HeaderMap), DialError> {
    let key = generate_key();
    let reply = shared
        .http
        .get(url)
        .headers(handshake_headers(client_headers, &key))
        .send()
        .await
        .map_err(|err| DialError::Failed(upstream_error(&err, &shared.route)))?;
    let status = reply.status();
    if status != StatusCode::SWITCHING_PROTOCOLS {
        let headers = reply.headers().clone();
        return Err(DialError::Refused(
            status,
            headers,
            refusal_body(reply).await,
        ));
    }
    let accepted = reply
        .headers()
        .get(header::SEC_WEBSOCKET_ACCEPT)
        .is_some_and(|accept| accept.as_bytes() == derive_accept_key(key.as_bytes()).as_bytes());
    if !accepted {
        return Err(DialError::Failed(
            "the upstream's 101 has no matching Sec-WebSocket-Accept".to_string(),
        ));
    }
    let headers = reply.headers().clone();
    let upgraded = reply
        .upgrade()
        .await
        .map_err(|err| DialError::Failed(upstream_error(&err, &shared.route)))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let upstream =
        tokio_tungstenite::WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config))
            .await;
    Ok((upstream, headers))
}

/// The upstream handshake request headers: the client's end-to-end headers
/// plus the identity headers, and the upgrade headers with the relay's own
/// `key` (reqwest adds `host` from the URL).
fn handshake_headers(client_headers: &HeaderMap, key: &str) -> HeaderMap {
    let mut out = headers::ws_request(client_headers);
    headers::inject_identity(&mut out);
    let upgrade = [
        (header::CONNECTION, "Upgrade"),
        (header::UPGRADE, "websocket"),
        (header::SEC_WEBSOCKET_VERSION, "13"),
    ];
    for (name, value) in upgrade {
        out.insert(name, HeaderValue::from_static(value));
    }
    // A generated key is base64, always a valid header value.
    if let Ok(key) = HeaderValue::from_str(key) {
        out.insert(header::SEC_WEBSOCKET_KEY, key);
    }
    out
}

/// A refusal's body, up to [`MAX_MESSAGE`] bytes: as far as it arrives
/// (reqwest has already decoded any chunked framing).
async fn refusal_body(mut reply: reqwest::Response) -> Bytes {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = reply.chunk().await {
        let room = MAX_MESSAGE - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if chunk.len() >= room {
            break;
        }
    }
    body.into()
}

/// Turns a refused upstream handshake into the client's response.
fn mirror_refusal(status: StatusCode, upstream_headers: &HeaderMap, body: Bytes) -> Response {
    // 426 would make Codex give up the WebSocket for the whole session at
    // the first handshake; anything else only does after its retry budget.
    let status = if status == StatusCode::UPGRADE_REQUIRED {
        StatusCode::BAD_GATEWAY
    } else {
        status
    };
    let mut out = Response::new(Body::from(body));
    *out.status_mut() = status;
    *out.headers_mut() = headers::ws_response(upstream_headers);
    out
}

/// Pumps frames both ways until either side is done, then closes the other
/// side and lets both closing handshakes finish.
async fn run(client: WebSocket, upstream: Upstream, ctx: Ctx) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    let norm = Mutex::new(Normalizer::new());
    let conn = ctx.conn.as_str();

    // Both pumps run in this one task: when one finishes the other is
    // dropped mid-await, which is safe because neither holds a frame that
    // still needs delivering to a peer that is going away.
    let end = tokio::select! {
        end = client_to_upstream(
            &mut client_rx,
            &mut upstream_tx,
            &norm,
            ctx.review_model.as_deref(),
            conn,
        ) => end,
        end = upstream_to_client(&mut upstream_rx, &mut client_tx, &norm, conn) => end,
        () = super::stopped(ctx.shutdown.clone()) => End::Shutdown,
    };

    match &end {
        End::ClientClosed | End::UpstreamClosed => {}
        End::ClientLost(_) => {
            let frame = upstream_close(GOING_AWAY, "client went away");
            send_close(&mut upstream_tx, Message::Close(Some(frame))).await;
        }
        End::UpstreamLost(_) => {
            let frame = client_close(INTERNAL_ERROR, "upstream connection lost");
            send_close(&mut client_tx, ws::Message::Close(Some(frame))).await;
        }
        End::Shutdown => {
            let reason = "codex-copilot relay shutting down";
            let upstream = Message::Close(Some(upstream_close(GOING_AWAY, reason)));
            let client = ws::Message::Close(Some(client_close(GOING_AWAY, reason)));
            tokio::join!(
                send_close(&mut upstream_tx, upstream),
                send_close(&mut client_tx, client)
            );
        }
    }
    // Reading on flushes the automatic close replies and collects the peers'
    // close echoes, so neither side sees a reset instead of a close.
    let _ = tokio::time::timeout(CLOSE_GRACE, async {
        tokio::join!(drain(&mut client_rx), drain(&mut upstream_rx))
    })
    .await;

    let url = ctx.url.as_str();
    match &end {
        End::UpstreamLost(_) => warn!(conn, url, reason = %end, "bridge closed"),
        _ => info!(conn, url, reason = %end, "bridge closed"),
    }
}

/// Client to upstream: `response.create` frames go through
/// [`Normalizer::upstream`], everything else verbatim. Pings and pongs stay
/// on their own hop (each WebSocket library answers pings itself).
async fn client_to_upstream<R, W>(
    rx: &mut R,
    tx: &mut W,
    norm: &Mutex<Normalizer>,
    review_model: Option<&str>,
    conn: &str,
) -> End
where
    R: Stream<Item = Result<ws::Message, axum::Error>> + Unpin,
    W: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    while let Some(next) = rx.next().await {
        let out = match next {
            Ok(ws::Message::Text(text)) => {
                Message::text(prepare_upstream(text.as_str(), norm, review_model, conn))
            }
            Ok(ws::Message::Binary(data)) => {
                debug!(conn, bytes = data.len(), "client binary frame");
                Message::Binary(data)
            }
            Ok(ws::Message::Ping(_) | ws::Message::Pong(_)) => continue,
            Ok(ws::Message::Close(frame)) => {
                let code = frame.as_ref().map(|f| f.code);
                debug!(conn, ?code, "client close frame");
                let frame = frame.map(|f| upstream_close(f.code, f.reason.as_str()));
                send_close(tx, Message::Close(frame)).await;
                return End::ClientClosed;
            }
            Err(err) => return End::ClientLost(err.to_string()),
        };
        if let Err(err) = tx.send(out).await {
            return End::UpstreamLost(format!("send failed: {err}"));
        }
    }
    End::ClientLost("connection ended without a close frame".to_string())
}

/// Upstream to client: every JSON text frame goes through
/// [`Normalizer::downstream`], everything else verbatim.
async fn upstream_to_client<R, W>(
    rx: &mut R,
    tx: &mut W,
    norm: &Mutex<Normalizer>,
    conn: &str,
) -> End
where
    R: Stream<Item = Result<Message, tungstenite::Error>> + Unpin,
    W: Sink<ws::Message, Error = axum::Error> + Unpin,
{
    while let Some(next) = rx.next().await {
        let out = match next {
            Ok(Message::Text(text)) => {
                ws::Message::Text(prepare_downstream(text.as_str(), norm, conn).into())
            }
            Ok(Message::Binary(data)) => {
                debug!(conn, bytes = data.len(), "upstream binary frame");
                ws::Message::Binary(data)
            }
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
            Ok(Message::Close(frame)) => {
                let frame = frame.map(|f| client_close(u16::from(f.code), f.reason.as_str()));
                let code = frame.as_ref().map(|f| f.code);
                debug!(conn, ?code, "upstream close frame");
                send_close(tx, ws::Message::Close(frame)).await;
                return End::UpstreamClosed;
            }
            Err(err) => return End::UpstreamLost(err.to_string()),
        };
        if let Err(err) = tx.send(out).await {
            return End::ClientLost(format!("send failed: {err}"));
        }
    }
    End::UpstreamLost("connection ended without a close frame".to_string())
}

/// One client text frame, ready for the upstream. Unchanged frames (and
/// anything that is not JSON) go out byte for byte.
fn prepare_upstream(
    text: &str,
    norm: &Mutex<Normalizer>,
    review_model: Option<&str>,
    conn: &str,
) -> String {
    let Ok(mut frame) = serde_json::from_str::<Value>(text) else {
        debug!(conn, bytes = text.len(), "client text frame (not JSON)");
        return text.to_string();
    };
    let changed = lock(norm).upstream(&mut frame, review_model);
    debug!(
        conn,
        kind = frame_type(&frame),
        bytes = text.len(),
        changed,
        "client frame"
    );
    if changed {
        frame.to_string()
    } else {
        text.to_string()
    }
}

/// One upstream text frame, ready for the client.
fn prepare_downstream(text: &str, norm: &Mutex<Normalizer>, conn: &str) -> String {
    let Ok(mut event) = serde_json::from_str::<Value>(text) else {
        debug!(conn, bytes = text.len(), "upstream text frame (not JSON)");
        return text.to_string();
    };
    let changed = lock(norm).downstream(&mut event);
    debug!(
        conn,
        kind = frame_type(&event),
        bytes = text.len(),
        changed,
        "upstream frame"
    );
    if changed {
        event.to_string()
    } else {
        text.to_string()
    }
}

fn frame_type(frame: &Value) -> &str {
    frame.get("type").and_then(Value::as_str).unwrap_or("?")
}

/// The normalizer is only ever locked for one synchronous rewrite, so a
/// poisoned lock (a panic mid-rewrite) leaves nothing half-done worth refusing.
fn lock(norm: &Mutex<Normalizer>) -> MutexGuard<'_, Normalizer> {
    norm.lock().unwrap_or_else(PoisonError::into_inner)
}

fn upstream_close(code: u16, reason: &str) -> CloseFrame {
    CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    }
}

fn client_close(code: u16, reason: &str) -> ws::CloseFrame {
    ws::CloseFrame {
        code,
        reason: reason.to_string().into(),
    }
}

/// Sends a close frame, giving up after [`CLOSE_GRACE`]: a peer that stopped
/// reading (a full TCP window) must not hold the bridge, and with it a
/// stopping relay, forever. Errors are moot, the connection is ending.
async fn send_close<S, M>(tx: &mut S, close: M)
where
    S: Sink<M> + Unpin,
{
    let _ = tokio::time::timeout(CLOSE_GRACE, tx.send(close)).await;
}

/// Reads and discards until the peer is gone.
async fn drain<S, T, E>(rx: &mut S)
where
    S: Stream<Item = Result<T, E>> + Unpin,
{
    while let Some(Ok(_)) = rx.next().await {}
}

#[cfg(test)]
mod tests {
    use futures_util::stream;
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn refusals_are_mirrored_but_a_426_never_is() {
        let mut upstream = HeaderMap::new();
        for (name, value) in [
            ("content-type", "application/json"),
            ("content-length", "99"),
            ("retry-after", "7"),
            ("transfer-encoding", "chunked"),
        ] {
            upstream.insert(name, value.parse().unwrap());
        }
        let body = Bytes::from_static(b"{\"error\":\"upgrade\"}");
        let out = mirror_refusal(StatusCode::UPGRADE_REQUIRED, &upstream, body.clone());
        assert_eq!(out.status(), StatusCode::BAD_GATEWAY);
        let h = out.headers();
        assert_eq!(h["content-type"], "application/json");
        assert_eq!(h["retry-after"], "7");
        assert!(!h.contains_key("content-length"));
        assert!(!h.contains_key("transfer-encoding"));
        let got = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(got, body);

        let out = mirror_refusal(StatusCode::UNAUTHORIZED, &HeaderMap::new(), Bytes::new());
        assert_eq!(out.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn handshakes_carry_identity_the_bearer_and_the_relays_own_key() {
        let mut client = HeaderMap::new();
        client.insert("authorization", "Bearer t".parse().unwrap());
        client.insert("host", "127.0.0.1:12899".parse().unwrap());
        client.insert("sec-websocket-key", "x".parse().unwrap());
        client.insert("connection", "Upgrade, keep-alive".parse().unwrap());
        let key = generate_key();
        let h = handshake_headers(&client, &key);
        assert_eq!(h["authorization"], "Bearer t");
        assert!(!h.contains_key("host"), "reqwest sets the upstream's host");
        assert_eq!(h["sec-websocket-key"], key.as_str());
        assert_eq!(h.get_all("sec-websocket-key").iter().count(), 1);
        assert_eq!(h["sec-websocket-version"], "13");
        assert_eq!(h["connection"], "Upgrade");
        assert_eq!(h["upgrade"], "websocket");
        assert_eq!(h["copilot-integration-id"], crate::capi::INTEGRATION_ID);
    }
    #[tokio::test]
    async fn close_frames_do_not_wait_on_a_stuck_peer() {
        // A peer that never accepts the frame (its TCP window is full).
        let mut stuck = std::pin::pin!(futures_util::sink::unfold((), |(), _: Message| {
            std::future::pending::<Result<(), tungstenite::Error>>()
        }));
        let close = Message::Close(Some(upstream_close(GOING_AWAY, "bye")));
        let started = std::time::Instant::now();
        let bounded = tokio::time::timeout(CLOSE_GRACE * 2, send_close(&mut stuck, close));
        assert!(bounded.await.is_ok(), "send_close outlived CLOSE_GRACE");
        assert!(started.elapsed() >= CLOSE_GRACE);
    }

    #[tokio::test]
    async fn client_frames_are_rewritten_and_close_is_forwarded() {
        let create = json!({"type":"response.create","model":"codex-auto-review",
            "input":[{"type":"message","id":"copilot-a-0","role":"assistant","content":[]}]});
        let verbatim = "{ \"type\" : \"response.cancel\" }";
        let mut rx = stream::iter(vec![
            Ok(ws::Message::Text(create.to_string().into())),
            Ok(ws::Message::Ping(vec![1].into())),
            Ok(ws::Message::Text(verbatim.into())),
            Ok(ws::Message::Binary(vec![7, 7].into())),
            Ok(ws::Message::Close(Some(client_close(4000, "bye")))),
            Ok(ws::Message::Text("never sent".into())),
        ]);
        let mut sent: Vec<Message> = Vec::new();
        let mut tx = std::pin::pin!(futures_util::sink::unfold(
            &mut sent,
            |sent, m| async move {
                sent.push(m);
                Ok::<_, tungstenite::Error>(sent)
            }
        ));
        let norm = Mutex::new(Normalizer::new());
        let end = client_to_upstream(&mut rx, &mut tx, &norm, Some("gpt-6-luna"), "t").await;
        assert!(matches!(end, End::ClientClosed), "{end}");
        assert_eq!(sent.len(), 4, "ping dropped, nothing after close");

        let Message::Text(first) = &sent[0] else {
            panic!("{:?}", sent[0])
        };
        let first: Value = serde_json::from_str(first.as_str()).unwrap();
        assert_eq!(first["model"], "gpt-6-luna");
        assert!(first["input"][0].get("id").is_none());
        assert!(
            lock(&norm).stream().is_some(),
            "response.create opened a stream"
        );

        assert_eq!(
            sent[1],
            Message::text(verbatim),
            "unchanged frames are byte-identical"
        );
        assert_eq!(sent[2], Message::Binary(vec![7, 7].into()));
        let Message::Close(Some(frame)) = &sent[3] else {
            panic!("{:?}", sent[3])
        };
        assert_eq!(u16::from(frame.code), 4000);
        assert_eq!(frame.reason.as_str(), "bye");
    }

    #[tokio::test]
    async fn upstream_frames_are_normalized_until_the_stream_ends() {
        let mut rx = stream::iter(vec![
            Ok(Message::text(
                json!({"type":"response.output_text.delta","output_index":2,"item_id":"x+/="})
                    .to_string(),
            )),
            Ok(Message::Pong(vec![].into())),
            Ok(Message::text("not json")),
        ]);
        let mut sent: Vec<ws::Message> = Vec::new();
        let mut tx = std::pin::pin!(futures_util::sink::unfold(
            &mut sent,
            |sent, m| async move {
                sent.push(m);
                Ok::<_, axum::Error>(sent)
            }
        ));
        let norm = Mutex::new(Normalizer::with_stream("s"));
        let end = upstream_to_client(&mut rx, &mut tx, &norm, "t").await;
        assert!(matches!(end, End::UpstreamLost(_)), "{end}");
        assert_eq!(sent.len(), 2);
        let ws::Message::Text(first) = &sent[0] else {
            panic!("{:?}", sent[0])
        };
        let first: Value = serde_json::from_str(first.as_str()).unwrap();
        assert_eq!(first["item_id"], "copilot-s-2");
        assert_eq!(sent[1], ws::Message::Text("not json".into()));
    }
}
