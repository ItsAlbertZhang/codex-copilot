//! End-to-end relay tests against an in-process fake Copilot gateway. Nothing
//! leaves 127.0.0.1.
//!
//! The fake answers every `response.create` frame, and every HTTP
//! `POST /responses`, the way Copilot does: a fresh opaque id on every event
//! of the same output item, and a fresh `response.id` on `created` /
//! `in_progress` / `completed`. Over HTTP the events come as an event stream
//! cut into awkward chunks (mid-line, mid-event, mid-CRLF).
//!
//! A fake HTTP proxy stands in for a corporate one. It sends every tunnel and
//! every forwarded request to the fake gateway, whatever host it names, so a
//! relay whose upstream is `copilot.test` (a name that resolves nowhere) can
//! only reach the gateway through it. The relay gets the proxy through
//! [`ProxyConfig::proxy`]; the process environment is never touched.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame as FakeCloseFrame, Message as FakeMessage};
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use axum::serve::ListenerExt;
use axum::{Json, Router};
use codex_copilot::capi;
use codex_copilot::proxy::{bind, ProxyConfig, ProxyHandle, ProxyOverride};
use futures_util::{stream, SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Upper bound for any single await; the whole file runs in well under 10s.
const STEP: Duration = Duration::from_secs(5);
const REVIEW_MODEL: &str = "test-review-model";
/// Frame and message limit of the fake and of the large-frame client.
const LARGE: usize = 64 << 20;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What the fake gateway saw.
#[derive(Default)]
struct Seen {
    handshake: Option<HeaderMap>,
    /// The query string of the last WebSocket handshake.
    handshake_query: Option<String>,
    creates: Vec<Value>,
    /// Every HTTP `POST /responses` the fake answered with events.
    http: Vec<HttpSeen>,
    /// The `response.id` values the fake sent, one list per request.
    response_ids: Vec<Vec<String>>,
    closes: Vec<(u16, String)>,
    search: Option<(HeaderMap, String)>,
}

struct HttpSeen {
    headers: HeaderMap,
    query: Option<String>,
    /// Body length as received.
    len: usize,
    body: Value,
}

type Record = Arc<Mutex<Seen>>;

async fn fake_upstream() -> (SocketAddr, Record) {
    let seen = Record::default();
    let app = Router::new()
        .route("/responses", any(fake_responses))
        .route("/alpha/search", post(fake_search))
        .with_state(seen.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

fn bearer_of(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// Copilot's refusal, with headers a relay has to pass on.
fn unauthorized() -> Response {
    let body = json!({"error": {"message": "Bad credentials"}});
    let headers = [
        ("x-request-id", "fake-401"),
        ("www-authenticate", "Bearer realm=\"fake\""),
    ];
    (StatusCode::UNAUTHORIZED, headers, Json(body)).into_response()
}

async fn fake_responses(
    State(seen): State<Record>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(upgrade) = upgrade else {
        return fake_http(seen, parts, body).await;
    };
    let bearer = bearer_of(&parts.headers);
    {
        let mut seen = seen.lock().unwrap();
        seen.handshake = Some(parts.headers);
        seen.handshake_query = parts.uri.query().map(str::to_string);
    }
    match bearer.as_str() {
        "Bearer rejected" => return unauthorized(),
        "Bearer wants-426" => return StatusCode::UPGRADE_REQUIRED.into_response(),
        _ => {}
    }
    let mut reply = upgrade
        .max_message_size(LARGE)
        .max_frame_size(LARGE)
        .on_upgrade(move |socket| fake_session(socket, seen));
    let h = reply.headers_mut();
    h.insert("x-reasoning-included", "true".parse().unwrap());
    h.insert("openai-model", "fake-model".parse().unwrap());
    h.insert("x-codex-turn-state", "fake-turn-state".parse().unwrap());
    reply
}

/// HTTP `POST /responses`: the scripted events as an event stream, chunked
/// awkwardly, or in one piece with a content-length for `Bearer sized`.
async fn fake_http(seen: Record, parts: axum::http::request::Parts, body: Body) -> Response {
    let bearer = bearer_of(&parts.headers);
    if bearer == "Bearer rejected" {
        return unauthorized();
    }
    let raw = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let request: Value = serde_json::from_slice(&raw).unwrap();
    let (events, ids) = scripted_response();
    {
        let mut seen = seen.lock().unwrap();
        seen.http.push(HttpSeen {
            headers: parts.headers,
            query: parts.uri.query().map(str::to_string),
            len: raw.len(),
            body: request,
        });
        seen.response_ids.push(ids);
    }
    let sse = sse_body(&events);
    let mut reply = if bearer == "Bearer sized" {
        Response::new(Body::from(sse))
    } else {
        let chunks = awkward_chunks(&sse);
        Response::new(Body::from_stream(stream::iter(
            chunks.into_iter().map(Ok::<_, std::io::Error>),
        )))
    };
    let h = reply.headers_mut();
    h.insert("content-type", "text/event-stream".parse().unwrap());
    h.insert("x-codex-turn-state", "fake-http-turn".parse().unwrap());
    h.insert("openai-model", "fake-model".parse().unwrap());
    reply
}

/// The events as SSE in three styles, in turn: one `data:` line with LF;
/// pretty-printed JSON over several `data:` lines with CRLF and an `id:`
/// line after the data; a comment and a `retry:` line before `data:` with no
/// space after the colon.
fn sse_body(events: &[Value]) -> Vec<u8> {
    let mut out = String::from(": fake upstream\n\n");
    for (i, event) in events.iter().enumerate() {
        let kind = event["type"].as_str().unwrap();
        match i % 3 {
            0 => out += &format!("event: {kind}\ndata: {event}\n\n"),
            1 => {
                out += &format!("event: {kind}\r\n");
                for line in serde_json::to_string_pretty(event).unwrap().lines() {
                    out += &format!("data: {line}\r\n");
                }
                out += &format!("id: {i}\r\n\r\n");
            }
            _ => out += &format!(": keep-alive\nevent: {kind}\nretry: 1000\ndata:{event}\n\n"),
        }
    }
    out.into_bytes()
}

/// Chunks of 1, 2, 3, 5, ... bytes, so boundaries land everywhere: inside
/// field names, JSON, CRLFs and blank lines. Each one is its own HTTP chunk.
fn awkward_chunks(bytes: &[u8]) -> Vec<Bytes> {
    let sizes = [1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 7];
    let mut chunks = Vec::new();
    let mut rest = bytes;
    for size in sizes.iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let (chunk, tail) = rest.split_at((*size).min(rest.len()));
        chunks.push(Bytes::copy_from_slice(chunk));
        rest = tail;
    }
    chunks
}

async fn fake_session(mut socket: WebSocket, seen: Record) {
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            FakeMessage::Text(text) => {
                let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else {
                    // Not JSON: echo it back (the large-frame test).
                    if socket.send(FakeMessage::Text(text)).await.is_err() {
                        return;
                    }
                    continue;
                };
                if frame["type"] == "test.close" {
                    let close = FakeCloseFrame {
                        code: 4000,
                        reason: "upstream says bye".into(),
                    };
                    if socket.send(FakeMessage::Close(Some(close))).await.is_err() {
                        return;
                    }
                    continue;
                }
                if frame["type"] != "response.create" {
                    continue;
                }
                let (events, ids) = scripted_response();
                {
                    let mut seen = seen.lock().unwrap();
                    seen.creates.push(frame);
                    seen.response_ids.push(ids);
                }
                for event in events {
                    let text = FakeMessage::Text(event.to_string().into());
                    if socket.send(text).await.is_err() {
                        return;
                    }
                }
            }
            FakeMessage::Close(Some(frame)) => {
                let reason = frame.reason.as_str().to_string();
                seen.lock().unwrap().closes.push((frame.code, reason));
            }
            _ => {}
        }
    }
}

async fn fake_search(State(seen): State<Record>, headers: HeaderMap, body: String) -> Json<Value> {
    let integration = headers
        .get("copilot-integration-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    seen.lock().unwrap().search = Some((headers, body.clone()));
    Json(json!({"echo": body, "integration": integration}))
}

/// The native id Copilot puts on custom tool input deltas.
const CTC_ID: &str = "ctc_02665d66bce2fa59016ac5ff4a80ac87d2880af5e58c6b3e5e";

/// A Copilot-shaped opaque id: base64-ish, never the same twice.
fn opaque() -> String {
    format!(
        "{}+/{}=",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    )
}

/// A reasoning item at output_index 0, a message at 1 and a custom tool call
/// at 2, with a different id on every event, as in the WS captures (the tool
/// input deltas share a native `ctc_` id that matches nothing else). Returns
/// the events and the response ids used.
fn scripted_response() -> (Vec<Value>, Vec<String>) {
    let ids = vec![opaque(), opaque(), opaque()];
    let events = vec![
        json!({"type":"response.created","sequence_number":0,
               "response":{"id":ids[0],"status":"in_progress","output":[]}}),
        json!({"type":"response.in_progress","sequence_number":1,
               "response":{"id":ids[1],"status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,
               "item":{"id":opaque(),"type":"reasoning","summary":[],
                       "encrypted_content":"gAAAA+blob/1"}}),
        json!({"type":"response.reasoning_summary_text.delta","sequence_number":3,
               "output_index":0,"summary_index":0,"item_id":opaque(),"delta":"Hmm"}),
        json!({"type":"response.output_item.done","sequence_number":4,"output_index":0,
               "item":{"id":opaque(),"type":"reasoning","summary":[],
                       "encrypted_content":"gAAAA+blob/2"}}),
        json!({"type":"response.output_item.added","sequence_number":5,"output_index":1,
               "item":{"id":opaque(),"type":"message","role":"assistant",
                       "status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","sequence_number":6,"output_index":1,
               "content_index":0,"item_id":opaque(),
               "part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","sequence_number":7,"output_index":1,
               "content_index":0,"item_id":opaque(),"delta":"Hel"}),
        json!({"type":"response.output_text.delta","sequence_number":8,"output_index":1,
               "content_index":0,"item_id":opaque(),"delta":"lo"}),
        json!({"type":"response.output_text.done","sequence_number":9,"output_index":1,
               "content_index":0,"item_id":opaque(),"text":"Hello"}),
        json!({"type":"response.output_item.done","sequence_number":10,"output_index":1,
               "item":{"id":opaque(),"type":"message","role":"assistant","status":"completed",
                       "content":[{"type":"output_text","text":"Hello","annotations":[]}]}}),
        json!({"type":"response.output_item.added","sequence_number":11,"output_index":2,
               "item":{"id":opaque(),"type":"custom_tool_call","status":"in_progress",
                       "call_id":"call_fake1","name":"apply_patch","input":""}}),
        json!({"type":"response.custom_tool_call_input.delta","sequence_number":12,
               "output_index":2,"item_id":CTC_ID,"delta":"*** Begin"}),
        json!({"type":"response.custom_tool_call_input.delta","sequence_number":13,
               "output_index":2,"item_id":CTC_ID,"delta":" Patch"}),
        json!({"type":"response.custom_tool_call_input.done","sequence_number":14,
               "output_index":2,"item_id":CTC_ID,"input":"*** Begin Patch"}),
        json!({"type":"response.output_item.done","sequence_number":15,"output_index":2,
               "item":{"id":opaque(),"type":"custom_tool_call","status":"completed",
                       "call_id":"call_fake1","name":"apply_patch","input":"*** Begin Patch"}}),
        json!({"type":"response.completed","sequence_number":16,
               "response":{"id":ids[2],"status":"completed","output":[
                   {"id":opaque(),"type":"reasoning","summary":[],
                    "encrypted_content":"gAAAA+blob/2"},
                   {"id":opaque(),"type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"Hello","annotations":[]}]},
                   {"id":opaque(),"type":"custom_tool_call","status":"completed",
                    "call_id":"call_fake1","name":"apply_patch","input":"*** Begin Patch"}]}}),
    ];
    (events, ids)
}

async fn start_relay(upstream: SocketAddr) -> ProxyHandle {
    start_relay_with(&format!("http://{upstream}"), None).await
}

/// A relay for the `upstream` origin, with this proxy.
async fn start_relay_with(upstream: &str, proxy: Option<ProxyOverride>) -> ProxyHandle {
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: upstream.to_string(),
        review_model: Some(REVIEW_MODEL.to_string()),
        proxy,
    };
    timeout(STEP, bind(&cfg)).await.unwrap().unwrap()
}

/// Connects like Codex does, plus some headers a relay must not forward.
async fn connect(
    relay: SocketAddr,
    bearer: &str,
) -> Result<(Client, tungstenite::handshake::client::Response), tungstenite::Error> {
    connect_to(relay, "/responses", bearer).await
}

/// [`connect`] with a path and query of the caller's choice.
async fn connect_to(
    relay: SocketAddr,
    path_and_query: &str,
    bearer: &str,
) -> Result<(Client, tungstenite::handshake::client::Response), tungstenite::Error> {
    let mut request = format!("ws://{relay}{path_and_query}")
        .into_client_request()
        .unwrap();
    let h = request.headers_mut();
    h.insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    h.insert("x-test-custom", "kept".parse().unwrap());
    h.insert("copilot-integration-id", "spoofed".parse().unwrap());
    // Hop-by-hop: by definition, or nominated by Connection.
    h.insert("connection", "Upgrade, x-hop-test".parse().unwrap());
    h.insert("x-hop-test", "1".parse().unwrap());
    h.insert("keep-alive", "timeout=5".parse().unwrap());
    h.insert("proxy-authorization", "Basic c2VjcmV0".parse().unwrap());
    h.insert(
        "sec-websocket-extensions",
        "permessage-deflate".parse().unwrap(),
    );
    timeout(STEP, tokio_tungstenite::connect_async(request))
        .await
        .expect("handshake timed out")
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// Closes the client side and waits for the relay to finish the handshake, so
/// that the relay does not have to wait out its close grace on shutdown.
async fn close(mut ws: Client) {
    ws.close(None).await.unwrap();
    timeout(STEP, async { while let Some(Ok(_)) = ws.next().await {} })
        .await
        .expect("close handshake timed out");
}

fn create_frame() -> Value {
    json!({"type":"response.create","model":"codex-auto-review","stream":true,
    "input":[
        {"type":"message","id":"copilot-x-0","role":"assistant",
         "content":[{"type":"output_text","text":"earlier"}]},
        {"type":"message","id":"msg_keep","role":"user",
         "content":[{"type":"input_text","text":"hi"}]}
    ]})
}

/// Sends one `response.create` and collects events up to `response.completed`.
async fn turn(ws: &mut Client) -> Vec<Value> {
    ws.send(Message::text(create_frame().to_string()))
        .await
        .unwrap();
    let mut events = Vec::new();
    loop {
        let next = timeout(STEP, ws.next()).await.expect("event timed out");
        let Some(Ok(Message::Text(text))) = next else {
            panic!("unexpected frame: {next:?}");
        };
        let event: Value = serde_json::from_str(text.as_str()).unwrap();
        let done = event["type"] == "response.completed";
        events.push(event);
        if done {
            return events;
        }
    }
}

/// Every item id per output_index, from item.id, item_id and
/// response.output[i].id alike.
fn ids_by_index(events: &[Value]) -> BTreeMap<u64, BTreeSet<String>> {
    let mut out: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for e in events {
        if let Some(i) = e["output_index"].as_u64() {
            for id in [&e["item"]["id"], &e["item_id"]] {
                if let Some(id) = id.as_str() {
                    out.entry(i).or_default().insert(id.to_string());
                }
            }
        }
        if let Some(output) = e["response"]["output"].as_array() {
            for (i, item) in output.iter().enumerate() {
                let id = item["id"].as_str().expect("output item id").to_string();
                out.entry(i as u64).or_default().insert(id);
            }
        }
    }
    out
}

/// Checks one consistent relay id per output_index and returns the stream
/// segment they share.
fn stream_of(events: &[Value]) -> String {
    let ids = ids_by_index(events);
    assert_eq!(ids.keys().copied().collect::<Vec<_>>(), [0, 1, 2]);
    let mut streams = BTreeSet::new();
    for (index, set) in &ids {
        assert_eq!(set.len(), 1, "output_index {index} has ids {set:?}");
        let id = set.iter().next().unwrap();
        assert!(!id.contains('_'), "{id} would survive Codex's id filter");
        let rest = id.strip_prefix("copilot-").expect("relay id prefix");
        let (stream, suffix) = rest.rsplit_once('-').unwrap();
        assert_eq!(suffix, index.to_string());
        streams.insert(stream.to_string());
    }
    assert_eq!(streams.len(), 1, "{streams:?}");
    streams.into_iter().next().unwrap()
}

fn response_ids(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| e["response"]["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn the_bridge_normalizes_ids_and_forwards_identity() {
    let (upstream, seen) = fake_upstream().await;
    let relay = start_relay(upstream).await;
    let query = "api-version=2026-08-01&probe=a%20b";
    let path = format!("/responses?{query}");
    let (mut ws, handshake) = connect_to(relay.addr, &path, "test-token").await.unwrap();

    // The upstream's 101 extras reach the client's 101.
    assert_eq!(handshake.headers()["x-reasoning-included"], "true");
    assert_eq!(handshake.headers()["openai-model"], "fake-model");
    assert_eq!(handshake.headers()["x-codex-turn-state"], "fake-turn-state");

    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.handshake_query.as_deref(), Some(query));
        let h = seen.handshake.as_ref().unwrap();
        assert_eq!(h["authorization"], "Bearer test-token");
        assert_eq!(h["x-test-custom"], "kept");
        for (name, value) in [
            ("copilot-integration-id", capi::INTEGRATION_ID),
            ("editor-version", capi::EDITOR_VERSION),
            ("editor-plugin-version", capi::EDITOR_PLUGIN_VERSION),
            ("x-github-api-version", capi::API_VERSION),
            ("openai-intent", capi::OPENAI_INTENT),
            ("x-interaction-type", capi::INTERACTION_TYPE),
            ("x-initiator", capi::INITIATOR),
        ] {
            let got: Vec<_> = h.get_all(name).iter().collect();
            assert_eq!(got, [value], "{name}");
        }
        for hop in [
            "x-hop-test",
            "keep-alive",
            "proxy-authorization",
            "sec-websocket-extensions",
        ] {
            assert!(!h.contains_key(hop), "{hop} leaked upstream");
        }
        // The relay's own handshake, not the client's.
        assert_eq!(h["connection"], "Upgrade");
    }

    let first = turn(&mut ws).await;
    {
        let seen = seen.lock().unwrap();
        let sent = &seen.creates[0];
        assert_eq!(sent["model"], REVIEW_MODEL);
        assert!(
            sent["input"][0].get("id").is_none(),
            "relay id reached upstream"
        );
        assert_eq!(sent["input"][0]["content"][0]["text"], "earlier");
        assert_eq!(sent["input"][1]["id"], "msg_keep");
        assert_eq!(response_ids(&first), seen.response_ids[0]);
    }
    let stream1 = stream_of(&first);
    // Content is untouched.
    let deltas: String = first
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, "Hello");
    assert_eq!(first[4]["item"]["encrypted_content"], "gAAAA+blob/2");

    let second = turn(&mut ws).await;
    let stream2 = stream_of(&second);
    assert_ne!(
        stream1, stream2,
        "each response.create gets a new namespace"
    );
    assert_eq!(response_ids(&second), seen.lock().unwrap().response_ids[1]);

    // A client close frame reaches the upstream with its code and reason.
    ws.send(Message::Close(Some(CloseFrame {
        code: CloseCode::Normal,
        reason: "bye".into(),
    })))
    .await
    .unwrap();
    timeout(STEP, async {
        while seen.lock().unwrap().closes.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("close never reached the upstream");
    assert_eq!(seen.lock().unwrap().closes[0], (1000, "bye".to_string()));

    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}

#[tokio::test]
async fn refused_handshakes_are_mirrored_but_never_as_426() {
    let (upstream, _seen) = fake_upstream().await;
    let relay = start_relay(upstream).await;

    let Err(tungstenite::Error::Http(refusal)) = connect(relay.addr, "rejected").await else {
        panic!("a 401 upstream must not upgrade");
    };
    assert_eq!(refusal.status(), 401);
    let body: Value = serde_json::from_slice(refusal.body().as_deref().unwrap()).unwrap();
    assert_eq!(body["error"]["message"], "Bad credentials");
    // Its headers come along (Codex reads x-request-id into its error).
    assert_eq!(refusal.headers()["x-request-id"], "fake-401");
    assert_eq!(
        refusal.headers()["www-authenticate"],
        "Bearer realm=\"fake\""
    );
    assert_eq!(refusal.headers()["content-type"], "application/json");

    // 426 would send Codex to the HTTP transport the relay refuses.
    let Err(tungstenite::Error::Http(refusal)) = connect(relay.addr, "wants-426").await else {
        panic!("a 426 upstream must not upgrade");
    };
    assert_eq!(refusal.status(), 502);

    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_closes_open_bridges_on_both_sides() {
    let (upstream, seen) = fake_upstream().await;
    let relay = start_relay(upstream).await;
    let (mut ws, _) = connect(relay.addr, "test-token").await.unwrap();

    // The relay waits for close echoes, so keep reading like Codex would.
    let stopping = tokio::spawn(relay.shutdown());
    let next = timeout(STEP, ws.next()).await.expect("no close frame");
    let Some(Ok(Message::Close(Some(frame)))) = next else {
        panic!("expected a close frame, got {next:?}");
    };
    assert_eq!(frame.code, CloseCode::Away);
    let after = timeout(STEP, ws.next()).await.expect("socket stayed open");
    assert!(after.is_none(), "{after:?}");
    timeout(STEP, stopping).await.unwrap().unwrap().unwrap();

    let closes = &seen.lock().unwrap().closes;
    assert_eq!(closes.first().map(|c| c.0), Some(1001), "{closes:?}");
}

#[tokio::test]
async fn http_endpoints() {
    let (upstream, seen) = fake_upstream().await;
    let relay = start_relay(upstream).await;
    let base = format!("http://{}", relay.addr);
    let client = http();

    let health: Value = client
        .get(format!("{base}/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["name"], "codex-copilot");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(health["upstream"], format!("http://{upstream}"));
    assert_eq!(health["review_model"], REVIEW_MODEL);
    assert_eq!(health["pid"], std::process::id());
    // Present, and null: no proxy.
    assert_eq!(health.get("proxy"), Some(&Value::Null));

    // /responses is a WebSocket or a POST; anything else is refused, and
    // never with a 426.
    for request in [
        client.get(format!("{base}/responses")),
        client.put(format!("{base}/responses")).body("{}"),
    ] {
        let reply = request.send().await.unwrap();
        assert_eq!(reply.status(), 501);
        let body: Value = reply.json().await.unwrap();
        assert_eq!(body["error"]["type"], "unsupported_transport");
    }

    // Anything else is passed through with the identity headers added.
    let query = r#"{"query":"rust websockets"}"#;
    let reply = client
        .post(format!("{base}/alpha/search?source=test"))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(query)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let body: Value = reply.json().await.unwrap();
    assert_eq!(body["echo"], query);
    assert_eq!(body["integration"], capi::INTEGRATION_ID);
    {
        let seen = seen.lock().unwrap();
        let (headers, received) = seen.search.as_ref().unwrap();
        assert_eq!(received, query);
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert_eq!(headers["x-initiator"], capi::INITIATOR);
        assert_eq!(headers["content-length"], query.len().to_string());
    }

    // An upstream 404 comes back as is.
    let reply = client.get(format!("{base}/nowhere")).send().await.unwrap();
    assert_eq!(reply.status(), 404);

    // A browser cannot stop the service; the CLI can.
    let reply = client
        .post(format!("{base}/shutdown"))
        .header("origin", "https://example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 403);
    let reply = client
        .post(format!("{base}/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let body: Value = reply.json().await.unwrap();
    assert_eq!(body, json!({"ok": true}));
    timeout(STEP, relay.wait())
        .await
        .expect("relay did not stop")
        .unwrap();
}

/// One dispatched SSE event as a client sees it.
struct SseEvent {
    data: Value,
    /// How many `data:` lines carried it.
    data_lines: usize,
}

/// Parses an event stream the way an SSE client does: LF, CRLF and CR end
/// lines, a blank line dispatches, `data:` values are joined with LF.
fn parse_sse(raw: &str) -> Vec<SseEvent> {
    let text = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut events = Vec::new();
    let mut data = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            if !data.is_empty() {
                events.push(SseEvent {
                    data: serde_json::from_str(&data.join("\n")).unwrap(),
                    data_lines: data.len(),
                });
            }
            data.clear();
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
        }
    }
    events
}

/// The HTTP request body Codex sends: a `response.create` frame minus `type`.
fn http_body() -> Value {
    let mut body = create_frame();
    body.as_object_mut().unwrap().remove("type");
    body
}

#[tokio::test]
async fn the_http_fallback_normalizes_the_event_stream() {
    let (upstream, seen) = fake_upstream().await;
    let relay = start_relay(upstream).await;
    let client = http();
    let url = format!("http://{}/responses?api-version=2026-08-01", relay.addr);

    let mut streams = BTreeSet::new();
    // Chunked awkwardly, then in one piece with a content-length.
    for (turn, bearer) in ["test-token", "sized"].into_iter().enumerate() {
        let reply = client
            .post(&url)
            .header("authorization", format!("Bearer {bearer}"))
            .header("accept", "text/event-stream")
            .header("accept-encoding", "gzip")
            .header("x-test-custom", "kept")
            .header("copilot-integration-id", "spoofed")
            .json(&http_body())
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        let h = reply.headers();
        assert_eq!(h["content-type"], "text/event-stream");
        assert_eq!(h["x-codex-turn-state"], "fake-http-turn");
        assert_eq!(h["openai-model"], "fake-model");
        assert!(
            !h.contains_key("content-length"),
            "rewritten bodies change length"
        );
        let raw = timeout(STEP, reply.bytes()).await.unwrap().unwrap();
        let raw = String::from_utf8(raw.to_vec()).unwrap();

        // Only data goes out: Codex ignores comments and the other fields.
        assert!(raw.starts_with("data: "), "{raw}");
        assert!(!raw.contains("keep-alive") && !raw.contains("event:"));

        let parsed = parse_sse(&raw);
        let events: Vec<Value> = parsed.iter().map(|e| e.data.clone()).collect();
        assert_eq!(events.len(), 17);
        for (i, e) in parsed.iter().enumerate() {
            // In order.
            assert_eq!(e.data["sequence_number"], i);
            // Rewritten events are one data line; others are left alone.
            let rewritten = e.data.get("output_index").is_some()
                || e.data["response"]["output"]
                    .as_array()
                    .is_some_and(|o| !o.is_empty());
            if rewritten {
                assert_eq!(e.data_lines, 1, "{}", e.data);
            }
        }
        // response.in_progress was pretty-printed and had nothing to rewrite.
        assert!(parsed[1].data_lines > 1, "in_progress kept its data lines");

        streams.insert(stream_of(&events));
        let deltas: String = events
            .iter()
            .filter(|e| e["type"] == "response.custom_tool_call_input.delta")
            .map(|e| e["delta"].as_str().unwrap())
            .collect();
        assert_eq!(deltas, "*** Begin Patch");
        assert_eq!(events[15]["item"]["call_id"], "call_fake1");

        let seen = seen.lock().unwrap();
        // response.id is Copilot's, untouched.
        assert_eq!(response_ids(&events), seen.response_ids[turn]);
        let sent = &seen.http[turn];
        assert_eq!(sent.query.as_deref(), Some("api-version=2026-08-01"));
        assert_eq!(sent.body["model"], REVIEW_MODEL);
        assert!(sent.body.get("type").is_none());
        assert!(
            sent.body["input"][0].get("id").is_none(),
            "relay id reached upstream"
        );
        assert_eq!(sent.body["input"][0]["content"][0]["text"], "earlier");
        assert_eq!(sent.body["input"][1]["id"], "msg_keep");
        assert_eq!(sent.headers["authorization"], format!("Bearer {bearer}"));
        assert_eq!(sent.headers["x-test-custom"], "kept");
        let integration: Vec<_> = sent
            .headers
            .get_all("copilot-integration-id")
            .iter()
            .collect();
        assert_eq!(integration, [capi::INTEGRATION_ID]);
        assert_eq!(sent.headers["x-initiator"], capi::INITIATOR);
        assert_eq!(sent.headers["content-length"], sent.len.to_string());
        assert!(
            !sent.headers.contains_key("accept-encoding"),
            "the stream must come back uncompressed"
        );
    }
    assert_eq!(streams.len(), 2, "each request gets its own namespace");

    // A refusal that is not an event stream is mirrored as is.
    let reply = client
        .post(&url)
        .header("authorization", "Bearer rejected")
        .json(&http_body())
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 401);
    assert_eq!(reply.headers()["x-request-id"], "fake-401");
    assert_eq!(reply.headers()["www-authenticate"], "Bearer realm=\"fake\"");
    assert_eq!(reply.headers()["content-type"], "application/json");
    let body: Value = reply.json().await.unwrap();
    assert_eq!(body, json!({"error": {"message": "Bad credentials"}}));

    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}

#[tokio::test]
async fn an_upstream_close_reaches_the_client() {
    let (upstream, _seen) = fake_upstream().await;
    let relay = start_relay(upstream).await;
    let (mut ws, _) = connect(relay.addr, "test-token").await.unwrap();

    // The fake closes with its own code and reason when asked.
    ws.send(Message::text(json!({"type":"test.close"}).to_string()))
        .await
        .unwrap();
    let next = timeout(STEP, ws.next()).await.expect("no close frame");
    let Some(Ok(Message::Close(Some(frame)))) = next else {
        panic!("expected a close frame, got {next:?}");
    };
    assert_eq!(u16::from(frame.code), 4000);
    assert_eq!(frame.reason.as_str(), "upstream says bye");
    let after = timeout(STEP, ws.next()).await.expect("socket stayed open");
    assert!(after.is_none(), "{after:?}");

    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}

#[tokio::test]
async fn frames_above_the_library_defaults_cross_both_hops() {
    let (upstream, _seen) = fake_upstream().await;
    let relay = start_relay(upstream).await;
    let mut request = format!("ws://{}/responses", relay.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer test-token".parse().unwrap());
    let config = WebSocketConfig::default()
        .max_message_size(Some(LARGE))
        .max_frame_size(Some(LARGE));
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), true);
    let (mut ws, _) = timeout(STEP, connect).await.unwrap().unwrap();

    // One frame just past the 16 MiB default; the fake echoes it back.
    let big = "x".repeat((16 << 20) + 1);
    ws.send(Message::text(big.as_str())).await.unwrap();
    let next = timeout(STEP, ws.next()).await.expect("no echo");
    let Some(Ok(Message::Text(echo))) = next else {
        panic!(
            "expected the echo, got {:?}",
            next.map(|m| m.map(|m| m.len()))
        );
    };
    assert_eq!(echo.len(), big.len());

    close(ws).await;
    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// Through an HTTP proxy

/// RFC 7617's example credential, `Aladdin:open sesame`, as the proxy URL
/// carries it and as the proxy must receive it.
const USERINFO: &str = "Aladdin:open%20sesame@";
const BASIC: &str = "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==";

/// What the fake proxy does.
#[derive(Clone, Copy)]
enum ProxyMode {
    /// Tunnels `CONNECT` and forwards plain HTTP, always to this address.
    Forward(SocketAddr),
    /// Answers every request with this status (code and reason).
    Refuse(&'static str),
}

/// What the fake proxy saw: the first request of each connection (the rest
/// of a connection is piped through untouched).
#[derive(Default)]
struct ProxySeen {
    /// `CONNECT` targets.
    connects: Vec<String>,
    /// Absolute-form targets of plain HTTP requests.
    forwards: Vec<String>,
    /// `Proxy-Authorization` of each request, if any.
    auth: Vec<Option<String>>,
}

type ProxyRecord = Arc<Mutex<ProxySeen>>;

impl ProxySeen {
    fn untouched(&self) -> bool {
        self.connects.is_empty() && self.forwards.is_empty()
    }
}

async fn fake_proxy(mode: ProxyMode) -> (SocketAddr, ProxyRecord) {
    let seen = ProxyRecord::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let record = seen.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            tokio::spawn(proxy_connection(client, mode, record.clone()));
        }
    });
    (addr, seen)
}

async fn proxy_connection(mut client: TcpStream, mode: ProxyMode, seen: ProxyRecord) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        if let Some(at) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match client.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    };
    let text = String::from_utf8_lossy(&head[..end]).into_owned();
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default();
    let target = request_line.next().unwrap_or_default().to_string();
    let auth = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("proxy-authorization")
            .then(|| value.trim().to_string())
    });
    {
        let mut seen = seen.lock().unwrap();
        if method == "CONNECT" {
            seen.connects.push(target);
        } else {
            seen.forwards.push(target);
        }
        seen.auth.push(auth);
    }
    let upstream = match mode {
        ProxyMode::Forward(upstream) => upstream,
        ProxyMode::Refuse(status) => {
            let reply = format!(
                "HTTP/1.1 {status}\r\nproxy-authenticate: Basic realm=\"fake\"\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = client.write_all(reply.as_bytes()).await;
            return;
        }
    };
    let Ok(mut server) = TcpStream::connect(upstream).await else {
        return;
    };
    let early = if method == "CONNECT" {
        let established = b"HTTP/1.1 200 Connection established\r\n\r\n";
        if client.write_all(established).await.is_err() {
            return;
        }
        &head[end..]
    } else {
        // The fake gateway (hyper) takes an absolute-form request as it is.
        &head[..]
    };
    if server.write_all(early).await.is_ok() {
        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
    }
}

/// The fake proxy at `url` for every upstream, except the hosts `no_proxy`
/// names (`NO_PROXY` syntax).
fn via(url: String, no_proxy: &str) -> Option<ProxyOverride> {
    Some(ProxyOverride {
        url,
        no_proxy: no_proxy.to_string(),
    })
}

async fn health_of(relay: &ProxyHandle) -> Value {
    let url = format!("http://{}/healthz", relay.addr);
    http().get(url).send().await.unwrap().json().await.unwrap()
}

/// One `POST /responses` with the fallback body.
async fn post_responses(relay: &ProxyHandle) -> reqwest::Response {
    let url = format!("http://{}/responses", relay.addr);
    let request = http()
        .post(url)
        .header("authorization", "Bearer test-token")
        .json(&http_body());
    timeout(STEP, request.send()).await.unwrap().unwrap()
}

/// The `{"error": {"type", "message"}}` of a relay error.
fn error_of(body: &[u8]) -> (String, String) {
    let body: Value = serde_json::from_slice(body).unwrap();
    let field = |name: &str| body["error"][name].as_str().unwrap().to_string();
    (field("type"), field("message"))
}

#[tokio::test]
async fn both_transports_go_through_the_proxy() {
    let (upstream, seen) = fake_upstream().await;
    let (proxy, through) = fake_proxy(ProxyMode::Forward(upstream)).await;
    // `.test` resolves nowhere: only the proxy can reach this upstream.
    let authority = format!("copilot.test:{}", upstream.port());
    let proxy_url = format!("http://{USERINFO}{proxy}");
    let relay = start_relay_with(&format!("http://{authority}"), via(proxy_url, "")).await;
    assert_eq!(
        health_of(&relay).await["proxy"],
        format!("http://***@{proxy}")
    );

    // The WebSocket: an http:// upstream's handshake is a forwarded request,
    // and the frames follow on the upgraded connection.
    let (mut ws, handshake) = connect(relay.addr, "test-token").await.unwrap();
    assert_eq!(handshake.headers()["x-codex-turn-state"], "fake-turn-state");
    let events = turn(&mut ws).await;
    stream_of(&events);
    close(ws).await;
    {
        let through = through.lock().unwrap();
        assert_eq!(through.forwards, [format!("http://{authority}/responses")]);
        assert_eq!(through.auth, [Some(BASIC.to_string())]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.handshake.as_ref().unwrap()["host"], authority.as_str());
    }

    // The SSE transport and the passthrough: plain HTTP to the proxy.
    let reply = post_responses(&relay).await;
    assert_eq!(reply.status(), 200);
    let raw = timeout(STEP, reply.text()).await.unwrap().unwrap();
    stream_of(
        &parse_sse(&raw)
            .into_iter()
            .map(|e| e.data)
            .collect::<Vec<_>>(),
    );
    let search = http()
        .post(format!("http://{}/alpha/search", relay.addr))
        .header("authorization", "Bearer test-token")
        .body("{}")
        .send();
    assert_eq!(timeout(STEP, search).await.unwrap().unwrap().status(), 200);
    {
        let through = through.lock().unwrap();
        assert!(through.connects.is_empty(), "{:?}", through.connects);
        assert!(through.forwards.len() >= 2, "{:?}", through.forwards);
        for target in &through.forwards {
            assert!(
                target.starts_with(&format!("http://{authority}/")),
                "{target}"
            );
        }
        for auth in &through.auth {
            assert_eq!(auth.as_deref(), Some(BASIC));
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.http[0].headers["host"], authority.as_str());
        assert_eq!(seen.search.as_ref().unwrap().0["host"], authority.as_str());
    }
    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_refused_connect_is_a_502_that_names_the_proxy() {
    for status in ["407 Proxy Authentication Required", "403 Forbidden"] {
        let (proxy, through) = fake_proxy(ProxyMode::Refuse(status)).await;
        let proxy_url = format!("http://{USERINFO}{proxy}");
        // https: both transports CONNECT, and the refusal comes before TLS.
        let relay = start_relay_with("https://copilot.test", via(proxy_url, "")).await;
        let shown = format!("through the proxy http://***@{proxy}");

        let Err(tungstenite::Error::Http(refusal)) = connect(relay.addr, "test-token").await else {
            panic!("a refused tunnel must not upgrade");
        };
        assert_eq!(refusal.status(), 502);
        let (kind, ws_message) = error_of(refusal.body().as_deref().unwrap());
        assert_eq!(kind, "upstream_unreachable");
        let start = "could not open the upstream WebSocket https://copilot.test/responses: ";
        assert!(ws_message.starts_with(start), "{ws_message}");
        assert!(ws_message.contains(&shown), "{ws_message}");

        let reply = post_responses(&relay).await;
        assert_eq!(reply.status(), 502);
        let (kind, http_message) = error_of(&reply.bytes().await.unwrap());
        assert_eq!(kind, "upstream_unreachable");
        assert!(http_message.contains(&shown), "{http_message}");

        for message in [&ws_message, &http_message] {
            // reqwest's own reason, from the proxy's status line.
            if status.starts_with("407") {
                assert!(
                    message.contains("proxy authorization required"),
                    "{message}"
                );
            }
            for secret in ["Aladdin", "sesame", "QWxh"] {
                assert!(!message.contains(secret), "{message}");
            }
        }
        {
            let through = through.lock().unwrap();
            assert!(through.connects.len() >= 2, "{:?}", through.connects);
            assert!(through.connects.iter().all(|t| t == "copilot.test:443"));
            assert!(through.auth.iter().all(|a| a.as_deref() == Some(BASIC)));
        }
        timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn no_proxy_keeps_the_upstream_off_the_proxy() {
    let (upstream, _seen) = fake_upstream().await;
    let (proxy, through) = fake_proxy(ProxyMode::Forward(upstream)).await;
    // `0.0.0.0` is not a loopback name, so only NO_PROXY keeps it off the
    // proxy (which would reach the fake gateway). Dialed directly it fails
    // at once and locally on every OS: a port that is bound but not
    // listening (refused), or no connection to `0.0.0.0` at all (Windows).
    let closed = TcpSocket::new_v4().unwrap();
    closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let origin = format!("http://0.0.0.0:{}", closed.local_addr().unwrap().port());
    let proxy = via(format!("http://{proxy}"), "localhost, 0.0.0.0/8");
    let relay = start_relay_with(&origin, proxy).await;
    assert_eq!(health_of(&relay).await["proxy"], Value::Null);

    let direct = "(no proxy: ";
    let Err(tungstenite::Error::Http(refusal)) = connect(relay.addr, "test-token").await else {
        panic!("an unreachable upstream must not upgrade");
    };
    assert_eq!(refusal.status(), 502);
    let (kind, message) = error_of(refusal.body().as_deref().unwrap());
    assert_eq!(kind, "upstream_unreachable");
    assert!(message.contains(direct), "{message}");

    let reply = post_responses(&relay).await;
    assert_eq!(reply.status(), 502);
    let (_, message) = error_of(&reply.bytes().await.unwrap());
    assert!(message.contains(direct), "{message}");

    assert!(through.lock().unwrap().untouched());
    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_loopback_upstream_never_goes_through_the_proxy() {
    let (upstream, seen) = fake_upstream().await;
    let (proxy, through) = fake_proxy(ProxyMode::Forward(upstream)).await;
    // No NO_PROXY: being on this machine is enough.
    let relay = start_relay_with(
        &format!("http://{upstream}"),
        via(format!("http://{proxy}"), ""),
    )
    .await;
    assert_eq!(health_of(&relay).await["proxy"], Value::Null);

    let (mut ws, _) = connect(relay.addr, "test-token").await.unwrap();
    stream_of(&turn(&mut ws).await);
    close(ws).await;
    let reply = post_responses(&relay).await;
    assert_eq!(reply.status(), 200);
    timeout(STEP, reply.bytes()).await.unwrap().unwrap();

    assert_eq!(seen.lock().unwrap().http.len(), 1);
    assert!(through.lock().unwrap().untouched());
    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_socks_proxy_for_the_upstream_stops_the_relay() {
    // reqwest is built without SOCKS: refused at start, not at every request.
    let socks = "socks5://user:hunter2@127.0.0.1:7891".to_string();
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: "https://copilot.test".to_string(),
        review_model: None,
        proxy: via(socks.clone(), ""),
    };
    let Err(err) = timeout(STEP, bind(&cfg)).await.unwrap() else {
        panic!("a SOCKS proxy for the upstream must stop the relay");
    };
    let err = format!("{err:#}");
    assert!(
        err.contains("socks5://***@127.0.0.1:7891, a SOCKS proxy"),
        "{err}"
    );
    assert!(!err.contains("hunter2"), "{err}");

    // One the upstream does not go through is never in the way.
    let relay = start_relay_with("https://copilot.test", via(socks, "copilot.test")).await;
    assert_eq!(health_of(&relay).await["proxy"], Value::Null);
    timeout(STEP, relay.shutdown()).await.unwrap().unwrap();
}
