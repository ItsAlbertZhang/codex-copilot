//! Header plumbing shared by the WebSocket bridge, the HTTP SSE transport
//! and the HTTP passthrough.
//!
//! Everything here is pure so the forwarding rules can be tested without
//! sockets. The relay never adds, logs or inspects `authorization`: Codex
//! sends the bearer and the relay forwards it as one more end-to-end header.

use axum::http::{header, HeaderMap, HeaderName, HeaderValue};

use crate::capi;

/// Headers that describe one connection, never the message (RFC 9110
/// section 7.6.1), plus the legacy `proxy-connection`. Any `proxy-*` header
/// is treated the same way.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The identity headers CAPI keys model policy, rate limits and billing on,
/// with the values of the official Copilot CLI (see [`crate::capi`]). They
/// overwrite whatever the client sent, so the relay is the one place that
/// decides how Copilot sees this tool.
const IDENTITY: [(&str, &str); 7] = [
    ("copilot-integration-id", capi::INTEGRATION_ID),
    ("editor-version", capi::EDITOR_VERSION),
    ("editor-plugin-version", capi::EDITOR_PLUGIN_VERSION),
    ("x-github-api-version", capi::API_VERSION),
    ("openai-intent", capi::OPENAI_INTENT),
    ("x-interaction-type", capi::INTERACTION_TYPE),
    // Static: one WebSocket handshake carries a whole session, so there is no
    // per-turn value to compute. "user" over-reports, the safe direction.
    ("x-initiator", capi::INITIATOR),
];

/// Client request headers for a plain HTTP request to the upstream. `host`
/// belongs to the upstream URL. `content-length` is kept on purpose: the body
/// is streamed through unchanged, and keeping the length stops the upload
/// from turning into a chunked one the gateway never saw from Codex in 1.x.
pub(crate) fn http_request(src: &HeaderMap) -> HeaderMap {
    filtered(src, |name| name == "host")
}

/// Upstream response headers for the client. `content-length` stays for the
/// same reason as in [`http_request`]: the body is relayed byte for byte.
pub(crate) fn http_response(src: &HeaderMap) -> HeaderMap {
    filtered(src, |_| false)
}

/// Client request headers for `POST /responses`: as [`http_request`], minus
/// `content-length` (the body may be re-serialized; the HTTP client sets the
/// new length) and `accept-encoding` (the event stream is rewritten event by
/// event, so it has to arrive uncompressed).
pub(crate) fn sse_request(src: &HeaderMap) -> HeaderMap {
    filtered(src, |name| {
        name == "host" || name == "content-length" || name == "accept-encoding"
    })
}

/// Upstream event-stream response headers for the client: as
/// [`http_response`], minus `content-length`, which rewritten events change.
pub(crate) fn sse_response(src: &HeaderMap) -> HeaderMap {
    filtered(src, |name| name == "content-length")
}

/// Whether an upstream response is an event stream the relay can rewrite:
/// `text/event-stream` without a content coding.
pub(crate) fn is_plain_event_stream(headers: &HeaderMap) -> bool {
    let event_stream = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"));
    let encoded = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|v| !v.as_bytes().trim_ascii().eq_ignore_ascii_case(b"identity"));
    event_stream && !encoded
}

/// Client handshake headers for the upstream WebSocket handshake. The relay
/// writes its own `host`, `connection`, `upgrade` and `sec-websocket-*`
/// (with its own key), and extensions such as permessage-deflate are
/// negotiated per hop, never end to end.
pub(crate) fn ws_request(src: &HeaderMap) -> HeaderMap {
    filtered(src, |name| {
        name == "host" || name == "content-length" || name.starts_with("sec-websocket-")
    })
}

/// Upstream handshake response headers for the client: the extras on a 101
/// (Codex reads `x-reasoning-included`, `openai-model` and
/// `x-codex-turn-state` there), or the headers of a refused handshake. The
/// client's own 101 already carries `sec-websocket-accept`, and a refused
/// handshake's body is re-framed here, so its length is recomputed.
pub(crate) fn ws_response(src: &HeaderMap) -> HeaderMap {
    filtered(src, |name| {
        name == "content-length" || name.starts_with("sec-websocket-")
    })
}

/// Sets the seven Copilot identity headers, replacing any client values.
pub(crate) fn inject_identity(headers: &mut HeaderMap) {
    for (name, value) in IDENTITY {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
}

/// Whether the request asks for a WebSocket upgrade at all (as opposed to a
/// malformed one), which decides between 400 and 501 on `/responses`.
pub(crate) fn asks_for_websocket(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::UPGRADE)
        .iter()
        .any(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

/// `{upstream}{path}{?query}` for every upstream request (the WebSocket
/// handshake is an HTTP request too).
pub(crate) fn http_url(upstream: &str, path_and_query: &str) -> String {
    format!("{}{path_and_query}", upstream.trim_end_matches('/'))
}

/// Copies `src` minus hop-by-hop headers, the headers its own `Connection`
/// header nominates, and whatever `drop` names. Repeated headers stay
/// repeated.
fn filtered(src: &HeaderMap, drop: impl Fn(&str) -> bool) -> HeaderMap {
    let nominated = nominated(src);
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src {
        let n = name.as_str();
        let hop =
            HOP_BY_HOP.contains(&n) || n.starts_with("proxy-") || nominated.iter().any(|t| t == n);
        if !hop && !drop(n) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Header names listed in `Connection` (`Connection: keep-alive, x-foo`),
/// lowercased. They are hop-by-hop for this message only.
fn nominated(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn client_handshake() -> HeaderMap {
        map(&[
            ("host", "127.0.0.1:12899"),
            ("connection", "Upgrade, x-hop-only"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-extensions", "permessage-deflate"),
            ("authorization", "Bearer secret"),
            ("openai-beta", "responses_websockets=2026-02-06"),
            ("x-hop-only", "1"),
            ("keep-alive", "timeout=5"),
            ("te", "trailers"),
            ("proxy-authorization", "Basic x"),
            ("x-custom", "a"),
            ("x-custom", "b"),
            ("copilot-integration-id", "spoofed"),
        ])
    }

    #[test]
    fn ws_handshake_forwards_end_to_end_headers_only() {
        let out = ws_request(&client_handshake());
        assert_eq!(out["authorization"], "Bearer secret");
        assert_eq!(out["openai-beta"], "responses_websockets=2026-02-06");
        let custom: Vec<_> = out.get_all("x-custom").iter().collect();
        assert_eq!(custom, ["a", "b"], "repeated headers stay repeated");
        for gone in [
            "host",
            "connection",
            "upgrade",
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-extensions",
            "x-hop-only",
            "keep-alive",
            "te",
            "proxy-authorization",
        ] {
            assert!(!out.contains_key(gone), "{gone} must not be forwarded");
        }
    }

    #[test]
    fn http_requests_keep_their_length_but_not_their_host() {
        let src = map(&[
            ("host", "127.0.0.1:12899"),
            ("content-length", "42"),
            ("content-type", "application/json"),
            ("transfer-encoding", "chunked"),
            ("trailer", "x-checksum"),
            ("proxy-connection", "keep-alive"),
            ("authorization", "Bearer secret"),
        ]);
        let out = http_request(&src);
        assert_eq!(out["content-length"], "42");
        assert_eq!(out["content-type"], "application/json");
        assert_eq!(out["authorization"], "Bearer secret");
        for gone in ["host", "transfer-encoding", "trailer", "proxy-connection"] {
            assert!(!out.contains_key(gone), "{gone}");
        }
    }

    #[test]
    fn upgrade_extras_are_copied_without_handshake_internals() {
        let src = map(&[
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            ("sec-websocket-extensions", "permessage-deflate"),
            ("x-reasoning-included", "true"),
            ("openai-model", "gpt-6-astra"),
            ("x-codex-turn-state", "abc"),
            ("x-request-id", "00000-1"),
            ("content-length", "12"),
        ]);
        let out = ws_response(&src);
        let mut names: Vec<_> = out.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "openai-model",
                "x-codex-turn-state",
                "x-reasoning-included",
                "x-request-id"
            ]
        );
    }

    #[test]
    fn responses_drop_hop_by_hop_headers() {
        let src = map(&[
            ("content-type", "text/event-stream"),
            ("content-length", "7"),
            ("transfer-encoding", "chunked"),
            ("connection", "close"),
            ("keep-alive", "timeout=5"),
            ("x-request-id", "r"),
        ]);
        let out = http_response(&src);
        assert_eq!(out.len(), 3);
        assert_eq!(out["content-length"], "7");
        assert_eq!(out["x-request-id"], "r");
    }

    #[test]
    fn sse_requests_drop_length_and_compression() {
        let src = map(&[
            ("host", "127.0.0.1:12899"),
            ("content-length", "42"),
            ("accept-encoding", "gzip, br"),
            ("accept", "text/event-stream"),
            ("authorization", "Bearer secret"),
            ("x-codex-turn-state", "t"),
            ("connection", "keep-alive"),
        ]);
        let out = sse_request(&src);
        let mut names: Vec<_> = out.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["accept", "authorization", "x-codex-turn-state"]);

        let src = map(&[
            ("content-type", "text/event-stream; charset=utf-8"),
            ("content-length", "7"),
            ("transfer-encoding", "chunked"),
            ("x-request-id", "r"),
            ("openai-model", "m"),
        ]);
        let out = sse_response(&src);
        let mut names: Vec<_> = out.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["content-type", "openai-model", "x-request-id"]);
    }

    #[test]
    fn only_uncompressed_event_streams_are_rewritten() {
        assert!(is_plain_event_stream(&map(&[(
            "content-type",
            "text/event-stream"
        )])));
        assert!(is_plain_event_stream(&map(&[
            ("content-type", "Text/Event-Stream ; charset=utf-8"),
            ("content-encoding", "identity"),
        ])));
        assert!(!is_plain_event_stream(&map(&[
            ("content-type", "text/event-stream"),
            ("content-encoding", "gzip"),
        ])));
        assert!(!is_plain_event_stream(&map(&[(
            "content-type",
            "application/json"
        )])));
        assert!(!is_plain_event_stream(&HeaderMap::new()));
    }

    #[test]
    fn identity_headers_overwrite_the_client() {
        let mut h = ws_request(&client_handshake());
        inject_identity(&mut h);
        assert_eq!(h["copilot-integration-id"], capi::INTEGRATION_ID);
        assert_eq!(h.get_all("copilot-integration-id").iter().count(), 1);
        assert_eq!(h["editor-version"], capi::EDITOR_VERSION);
        assert_eq!(h["editor-plugin-version"], capi::EDITOR_PLUGIN_VERSION);
        assert_eq!(h["x-github-api-version"], capi::API_VERSION);
        assert_eq!(h["openai-intent"], capi::OPENAI_INTENT);
        assert_eq!(h["x-interaction-type"], capi::INTERACTION_TYPE);
        assert_eq!(h["x-initiator"], capi::INITIATOR);
        // The bearer is the client's and is never replaced.
        assert_eq!(h["authorization"], "Bearer secret");
    }

    #[test]
    fn websocket_requests_are_told_apart_from_plain_ones() {
        assert!(asks_for_websocket(&map(&[("upgrade", "WebSocket")])));
        assert!(!asks_for_websocket(&map(&[("upgrade", "h2c")])));
        assert!(!asks_for_websocket(&HeaderMap::new()));
    }

    #[test]
    fn upstream_urls() {
        let up = "https://api.enterprise.githubcopilot.com";
        assert_eq!(
            http_url("http://127.0.0.1:9/", "/responses?a=1"),
            "http://127.0.0.1:9/responses?a=1"
        );
        assert_eq!(
            http_url(&format!("{up}/"), "/alpha/search?q=1"),
            "https://api.enterprise.githubcopilot.com/alpha/search?q=1"
        );
    }
}
