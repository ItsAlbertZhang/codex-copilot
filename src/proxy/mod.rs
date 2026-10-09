//! The relay: a loopback HTTP server that upgrades `GET /responses` to a
//! WebSocket, opens a matching `wss://<upstream>/responses` connection, and
//! pumps frames both ways while rewriting item ids (see [`normalize`]).
//!
//! Routes:
//!
//! * `GET /healthz` - who is listening: name, version, upstream, pid, and
//!   the proxy it reaches the upstream through.
//! * `POST /shutdown` - stop gracefully.
//! * `GET /responses` with `Upgrade: websocket` - the bridge (`bridge`).
//! * `POST /responses` - the HTTP SSE transport (`sse`), with the same
//!   rewrites. Codex switches a session to it for good after repeated
//!   WebSocket failures, so it has to work too.
//! * any other request on `/responses` gets 501.
//! * anything else - HTTP passthrough to the upstream with the identity
//!   headers added (Codex calls `POST /alpha/search` for web search, and
//!   `/responses/compact` and friends go the same way).
//!
//! The relay never answers 426 on the WebSocket route: that status makes
//! Codex drop the WebSocket for the whole session at the first handshake,
//! where other failures only do so once its retry budget is spent.
//!
//! The relay never adds a bearer: Codex sends its own `Authorization` and the
//! relay forwards it, so a local process (or a web page) that reaches the
//! port gets nothing it could not already do without the relay.
//!
//! Every upstream connection, on either transport, goes through one reqwest
//! client (the WebSocket as an HTTP/1.1 upgrade), so both follow reqwest's
//! proxy policy: `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY`,
//! else the OS proxy settings, never for a loopback upstream (see
//! [`http_client`]).

mod bridge;
mod headers;
pub mod normalize;
mod sse;

use std::fmt;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::body::{Body, HttpBody};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::serve::ListenerExt;
use axum::{Json, Router};
use hyper_util::client::proxy::matcher::{Intercept, Matcher};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// `name` reported by `GET /healthz`, so a client can tell this relay from
/// whatever else might hold the port.
pub const SERVICE_NAME: &str = "codex-copilot";

/// The 501 message for requests to `/responses` that are neither transport.
const UNSUPPORTED_TRANSPORT: &str = "codex-copilot relays /responses as a WebSocket \
     (GET with Upgrade: websocket) or as HTTP SSE (POST), nothing else";

/// Connect timeout for passthrough requests. There is deliberately no overall
/// timeout: response bodies may be long-lived streams.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long in-flight HTTP requests get after a stop request before the
/// server stops anyway (a streaming passthrough must not pin the service).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Everything the relay needs. Built from CLI args by `daemon` / `cli`.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Loopback address to bind. Port 0 picks a free port (tests).
    pub listen: SocketAddr,
    /// Upstream gateway origin, e.g. `https://api.enterprise.githubcopilot.com`
    /// (no trailing slash, no path).
    pub upstream: String,
    /// Model substituted for `codex-auto-review` in `response.create` frames.
    pub review_model: Option<String>,
    /// A proxy that replaces reqwest's own policy (the proxy variables and
    /// the OS proxy settings, read when the relay starts) for every upstream
    /// connection. `None` in normal use; tests set it instead of touching the
    /// process environment.
    pub proxy: Option<ProxyOverride>,
}

/// An explicit proxy for [`ProxyConfig::proxy`].
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyOverride {
    /// `http://[user:password@]host:port`, or `https://` for TLS to the proxy.
    pub url: String,
    /// Hosts that go directly, in `NO_PROXY` syntax (may be empty).
    pub no_proxy: String,
}

/// Never the URL, which may hold a password.
impl fmt::Debug for ProxyOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyOverride")
            .field("no_proxy", &self.no_proxy)
            .finish_non_exhaustive()
    }
}

/// A running relay.
pub struct ProxyHandle {
    /// The address actually bound (differs from `listen` when port 0).
    pub addr: SocketAddr,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl ProxyHandle {
    /// Ask the server to stop and wait for it.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.shutdown.send(true);
        self.task.await?
    }

    /// Wait until the server stops on its own (`POST /shutdown`). Does not
    /// watch for signals; see [`ProxyHandle::wait_or_signal`].
    pub async fn wait(self) -> Result<()> {
        self.task.await?
    }

    /// Wait until the server stops on its own, or until ctrl-c (SIGTERM too
    /// on Unix) arrives; in that case stop it gracefully (close frames to
    /// both peers of every bridge) and wait for it. This is what the service
    /// and the foreground `serve` must use, so that `systemctl stop`,
    /// `launchctl bootout` and ctrl-c end cleanly and with exit code 0.
    pub async fn wait_or_signal(self) -> Result<()> {
        let stop = self.shutdown.clone();
        let mut task = self.task;
        tokio::select! {
            joined = &mut task => return joined?,
            () = termination_signal() => {
                info!("termination signal received");
                let _ = stop.send(true);
            }
        }
        task.await?
    }
}

/// Source of the short per-request ids that tie log lines together.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// `ws-1`, `http-2`, ...: one id per bridge or relayed `POST /responses`.
fn conn_id(kind: &str) -> String {
    format!("{kind}-{}", NEXT_CONN.fetch_add(1, Ordering::Relaxed))
}

/// State shared by every request handler.
struct Shared {
    cfg: ProxyConfig,
    /// The proxy every upstream connection goes through, credentials
    /// masked; `None` when the relay connects directly.
    proxy: Option<String>,
    /// How the upstream is reached, appended to upstream errors
    /// ([`route_note`]).
    route: String,
    /// One client for every upstream request on both transports, so
    /// connections are pooled and all follow the same proxy policy.
    http: reqwest::Client,
    /// `true` means stop. Bridges subscribe to close their sockets cleanly.
    shutdown: watch::Sender<bool>,
    /// Cloned into every bridge; the server waits until all clones are gone.
    bridges: mpsc::Sender<()>,
}

/// Bind `cfg.listen`, start serving in a background task, return immediately.
pub async fn bind(cfg: &ProxyConfig) -> Result<ProxyHandle> {
    // Before the port is taken: a proxy the upstream would go through but
    // reqwest cannot use is an error now, not at every request.
    let proxy = upstream_proxy(cfg)?;
    let http = http_client(cfg)?;
    let listener = TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("could not listen on {}", cfg.listen))?;
    let addr = listener
        .local_addr()
        .context("could not read the address the relay is bound to")?;
    let (shutdown, stop) = watch::channel(false);
    let (bridges, bridges_done) = mpsc::channel(1);
    let shared = Arc::new(Shared {
        cfg: cfg.clone(),
        route: route_note(proxy.as_deref(), &cfg.upstream),
        proxy,
        http,
        shutdown: shutdown.clone(),
        bridges,
    });
    let proxy = shared.proxy.as_deref().unwrap_or("none");
    info!(%addr, upstream = %cfg.upstream, proxy, "relay listening");
    let task = tokio::spawn(serve(listener, router(shared), stop, bridges_done));
    Ok(ProxyHandle {
        addr,
        shutdown,
        task,
    })
}

/// Bind and serve until shutdown (`POST /shutdown`, ctrl-c, or SIGTERM on
/// Unix). Prints nothing; progress goes to `tracing`.
pub async fn run(cfg: ProxyConfig) -> Result<()> {
    bind(&cfg).await?.wait_or_signal().await
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/shutdown", post(shutdown))
        .route("/responses", any(responses))
        .fallback(passthrough)
        .with_state(shared)
}

/// Serves until a stop request, then gives in-flight requests and open
/// bridges a bounded time to finish.
async fn serve(
    listener: TcpListener,
    app: Router,
    stop: watch::Receiver<bool>,
    mut bridges_done: mpsc::Receiver<()>,
) -> Result<()> {
    // Events are small and latency is the point: no Nagle delay on the
    // client hop (the upstream hops disable it too).
    let listener = listener.tap_io(|tcp| {
        if let Err(err) = tcp.set_nodelay(true) {
            debug!(error = %err, "could not set TCP_NODELAY");
        }
    });
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(stopped(stop.clone()))
        .into_future();
    let deadline = async {
        stopped(stop).await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    };
    tokio::select! {
        result = server => {
            result.context("the relay server failed")?;
            // The router (and its sender) is gone; open bridges hold the
            // remaining senders while they send their close frames and
            // collect the echoes, each step bounded by CLOSE_GRACE.
            let wait = bridges_done.recv();
            let _ = tokio::time::timeout(bridge::CLOSE_GRACE * 2, wait).await;
        }
        () = deadline => warn!("in-flight requests outlived the shutdown grace period"),
    }
    info!("relay stopped");
    Ok(())
}

/// Resolves once a stop is requested.
pub(super) async fn stopped(mut stop: watch::Receiver<bool>) {
    if stop.wait_for(|stop| *stop).await.is_err() {
        // Every sender is gone, so no stop can ever be requested.
        std::future::pending::<()>().await;
    }
}

/// ctrl-c everywhere, plus SIGTERM on Unix (systemd / launchd stop).
async fn termination_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // No console to listen on (the Windows service binary).
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// The client for every upstream connection: the WebSocket handshake, the
/// SSE transport and the passthrough. Its proxy policy is reqwest's own (the
/// variables, else the OS proxy settings, read once here), replaced by
/// [`ProxyConfig::proxy`] when that is set; a loopback upstream never goes
/// through a proxy.
fn http_client(cfg: &ProxyConfig) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        // Event frames are small and latency is the point: no Nagle delay.
        .tcp_nodelay(true)
        // Redirects are the client's business; pass them through as is.
        .redirect(reqwest::redirect::Policy::none())
        // A WebSocket upgrade needs HTTP/1.1, whatever ALPN would pick.
        .http1_only();
    let builder = if is_loopback(&cfg.upstream) {
        builder.no_proxy()
    } else if let Some(explicit) = &cfg.proxy {
        // The URL is never echoed: it may hold a password.
        let proxy = reqwest::Proxy::all(&explicit.url)
            .context("the configured proxy is not a proxy URL")?
            .no_proxy(reqwest::NoProxy::from_string(&explicit.no_proxy));
        builder.proxy(proxy)
    } else {
        builder
    };
    builder
        .build()
        .context("could not build the upstream HTTP client")
}

/// The proxy [`http_client`] takes to `cfg.upstream`, as shown (credentials
/// masked), or `None` when it connects directly. Built from the same matcher
/// reqwest builds, so it says what reqwest does. A SOCKS proxy is an error:
/// reqwest is built without SOCKS support and would fail every request
/// through it with "unsupported scheme".
fn upstream_proxy(cfg: &ProxyConfig) -> Result<Option<String>> {
    let matcher = match &cfg.proxy {
        Some(explicit) => Matcher::builder()
            .all(explicit.url.clone())
            .no(explicit.no_proxy.clone())
            .build(),
        None => Matcher::from_system(),
    };
    let Some(proxy) = proxy_for(&matcher, &cfg.upstream) else {
        return Ok(None);
    };
    let proxy_shown = shown(&proxy);
    if !matches!(proxy.uri().scheme_str(), Some("http" | "https")) {
        bail!(
            "{} would go through {proxy_shown}, a SOCKS proxy, and the relay supports http:// and \
             https:// proxies only. Point HTTPS_PROXY (HTTP_PROXY for an http:// upstream) at an \
             HTTP proxy, or exclude the upstream with NO_PROXY",
            cfg.upstream
        );
    }
    Ok(Some(proxy_shown))
}

/// The proxy a relay started from this process's environment (and the OS
/// proxy settings) would reach `upstream` through, credentials masked;
/// `None` for a direct connection. For `status --probe`.
pub fn system_proxy(upstream: &str) -> Option<String> {
    proxy_for(&Matcher::from_system(), upstream).map(|proxy| shown(&proxy))
}

/// The proxy `matcher` picks for the upstream origin, which decides for
/// every path under it; never one for a loopback upstream.
fn proxy_for(matcher: &Matcher, upstream: &str) -> Option<Intercept> {
    if is_loopback(upstream) {
        return None;
    }
    matcher.intercept(&upstream.parse().ok()?)
}

/// `scheme://host:port`, with `***@` in place of any credentials.
fn shown(proxy: &Intercept) -> String {
    let uri = proxy.uri();
    let credentials = if proxy.basic_auth().is_some() || proxy.raw_auth().is_some() {
        "***@"
    } else {
        ""
    };
    let scheme = uri.scheme_str().unwrap_or("http");
    let authority = uri.authority().map_or("", |authority| authority.as_str());
    format!("{scheme}://{credentials}{authority}")
}

/// What to add to an upstream error so that it says how the relay tried to
/// get there, since the proxy is the usual suspect on a corporate network:
/// ` (through the proxy ...)`, ` (no proxy: ...)`, or nothing for a loopback
/// upstream.
fn route_note(proxy: Option<&str>, upstream: &str) -> String {
    match proxy {
        Some(proxy) => format!(" (through the proxy {proxy})"),
        None if is_loopback(upstream) => String::new(),
        None => " (no proxy: none of the relay's proxy variables or system proxy settings \
                 applied to the upstream when it started)"
            .to_string(),
    }
}

/// The host of an upstream origin, without the brackets of an IPv6 literal.
fn upstream_host(upstream: &str) -> &str {
    let rest = upstream
        .split_once("://")
        .map_or(upstream, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    }
}

/// Whether the upstream origin points at this machine.
fn is_loopback(upstream: &str) -> bool {
    let host = upstream_host(upstream);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `{"error":{"type":..,"message":..}}`, the shape Codex surfaces verbatim.
fn error_json(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    let body = json!({"error": {"type": kind, "message": message.into()}});
    (status, Json(body)).into_response()
}

/// A failed upstream request, for the client: the error with its causes
/// (reqwest's own text is only "error sending request for url (...)"), and
/// how the relay tried to reach the upstream ([`route_note`]).
fn upstream_error(err: &reqwest::Error, route: &str) -> String {
    let mut message = err.to_string();
    let mut cause = std::error::Error::source(err);
    while let Some(err) = cause {
        message.push_str(": ");
        message.push_str(&err.to_string());
        cause = err.source();
    }
    message.push_str(route);
    message
}

async fn healthz(State(shared): State<Arc<Shared>>) -> Json<Value> {
    Json(json!({
        "name": SERVICE_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "upstream": shared.cfg.upstream,
        "review_model": shared.cfg.review_model,
        "pid": std::process::id(),
        // Credentials masked; null when the relay connects directly.
        "proxy": shared.proxy,
    }))
}

async fn shutdown(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    // Browsers attach Origin to every cross-site POST; the CLI never does.
    // Without this check any web page could stop the service.
    if headers.contains_key(header::ORIGIN) {
        return error_json(
            StatusCode::FORBIDDEN,
            "forbidden",
            "shutdown is not accepted from a browser",
        );
    }
    info!("shutdown requested");
    shared.shutdown.send_replace(true);
    Json(json!({"ok": true})).into_response()
}

async fn responses(
    State(shared): State<Arc<Shared>>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    match upgrade {
        Ok(upgrade) => bridge::open(&shared, upgrade, &parts.uri, &parts.headers).await,
        // A broken upgrade gets 400 with axum's reason, never axum's own 426.
        Err(rejection) if headers::asks_for_websocket(&parts.headers) => error_json(
            StatusCode::BAD_REQUEST,
            "bad_websocket_handshake",
            rejection.body_text(),
        ),
        Err(_) if parts.method == Method::POST => sse::forward(&shared, parts, body).await,
        Err(_) => error_json(
            StatusCode::NOT_IMPLEMENTED,
            "unsupported_transport",
            UNSUPPORTED_TRANSPORT,
        ),
    }
}

/// Relays one plain HTTP request to the upstream, streaming both bodies.
async fn passthrough(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path_and_query = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
    let url = headers::http_url(&shared.cfg.upstream, path_and_query);
    let mut outgoing = headers::http_request(&parts.headers);
    headers::inject_identity(&mut outgoing);

    let mut upstream = shared
        .http
        .request(parts.method.clone(), &url)
        .headers(outgoing);
    // An empty body stays absent, so a GET does not turn into a chunked one.
    if !body.is_end_stream() {
        upstream = upstream.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let path = parts.uri.path();
    match upstream.send().await {
        Ok(reply) => {
            info!(method = %parts.method, path, status = %reply.status(), "passthrough");
            mirror(reply)
        }
        Err(err) => {
            let error = upstream_error(&err, &shared.route);
            warn!(method = %parts.method, path, error = %error, "passthrough failed");
            error_json(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                format!("{} {url}: {error}", parts.method),
            )
        }
    }
}

/// An upstream response as is: status, end-to-end headers, streamed body.
fn mirror(reply: reqwest::Response) -> Response {
    let status = reply.status();
    let headers = headers::http_response(reply.headers());
    let mut out = Response::new(Body::from_stream(reply.bytes_stream()));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_upstreams_are_recognised() {
        assert!(is_loopback("http://127.0.0.1:9000"));
        assert!(is_loopback("http://localhost"));
        assert!(is_loopback("http://[::1]:8080/"));
        assert!(!is_loopback("https://api.enterprise.githubcopilot.com"));
        assert!(is_loopback("127.0.0.2:1"));
        assert!(!is_loopback("https://127.example.com"));
        assert!(!is_loopback("http://10.0.0.1"));
    }

    fn cfg(upstream: &str, url: &str, no_proxy: &str) -> ProxyConfig {
        ProxyConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream: upstream.to_string(),
            review_model: None,
            proxy: Some(ProxyOverride {
                url: url.to_string(),
                no_proxy: no_proxy.to_string(),
            }),
        }
    }

    const GATEWAY: &str = "https://api.enterprise.githubcopilot.com";

    #[test]
    fn the_upstream_proxy_is_shown_without_credentials() {
        let proxied = |url: &str| upstream_proxy(&cfg(GATEWAY, url, "")).unwrap();
        assert_eq!(
            proxied("http://Aladdin:open%20sesame@proxy.example:3128").as_deref(),
            Some("http://***@proxy.example:3128")
        );
        assert_eq!(
            proxied("https://proxy.example").as_deref(),
            Some("https://proxy.example")
        );
        // NO_PROXY and the loopback rule keep the upstream off the proxy.
        let url = "http://proxy.example:3128";
        assert_eq!(
            upstream_proxy(&cfg(GATEWAY, url, ".githubcopilot.com")).unwrap(),
            None
        );
        assert_eq!(
            upstream_proxy(&cfg("http://127.0.0.1:9", url, "")).unwrap(),
            None
        );
        // A Debug print of the config never shows the URL either.
        let debug = format!("{:?}", cfg(GATEWAY, "http://u:hunter2@p.example:1", ""));
        assert!(!debug.contains("hunter2"), "{debug}");
    }

    #[test]
    fn a_socks_proxy_is_refused_without_its_credentials() {
        let err = upstream_proxy(&cfg(GATEWAY, "socks5h://user:hunter2@127.0.0.1:1080", ""))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("socks5h://***@127.0.0.1:1080, a SOCKS proxy"),
            "{err}"
        );
        assert!(!err.contains("hunter2") && !err.contains("user"), "{err}");
        // Not when the upstream does not go through it.
        let exempt = cfg(GATEWAY, "socks5://127.0.0.1:1080", "githubcopilot.com");
        assert_eq!(upstream_proxy(&exempt).unwrap(), None);
    }

    #[test]
    fn upstream_errors_say_how_the_upstream_was_reached() {
        assert_eq!(route_note(None, "http://127.0.0.1:9"), "");
        assert!(route_note(None, GATEWAY).starts_with(" (no proxy: "));
        assert_eq!(
            route_note(Some("http://***@p.example:1"), GATEWAY),
            " (through the proxy http://***@p.example:1)"
        );
    }
}
