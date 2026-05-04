use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use crate::limiter::PeerLimiter;

/// Header name (case-insensitive) carrying the MCP session id.
/// rmcp defines this internally; we use the literal to avoid coupling
/// to a private module path.
const SESSION_ID_HEADER: &str = "mcp-session-id";

/// Shared state for the [`gate`] middleware.
pub struct GateCtx {
    pub sessions: Arc<LocalSessionManager>,
    pub max_sessions: usize,
    pub limiter: PeerLimiter,
    pub trust_forwarded_for: bool,
}

/// axum middleware that gates **new-session** POSTs (no `Mcp-Session-Id`
/// header). It enforces:
///   1. A hard cap on concurrent sessions held by the [`LocalSessionManager`].
///   2. A per-peer rate limit on initialize requests.
///
/// All other traffic (GET, DELETE, POSTs that carry a session id) is passed
/// through untouched.
pub async fn gate(
    State(ctx): State<Arc<GateCtx>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if req.method() != Method::POST || req.headers().contains_key(SESSION_ID_HEADER) {
        return next.run(req).await;
    }

    let peer = peer_ip(req.headers(), addr, ctx.trust_forwarded_for);

    let in_use = ctx.sessions.sessions.read().await.len();
    if in_use >= ctx.max_sessions {
        tracing::warn!(
            in_use,
            cap = ctx.max_sessions,
            peer = %peer,
            "rejected initialize: session cap reached"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            "session cap reached",
        )
            .into_response();
    }

    if let Err(retry) = ctx.limiter.try_consume(peer) {
        let secs = retry.as_secs().max(1);
        tracing::warn!(peer = %peer, retry_secs = secs, "rate-limited initialize");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, secs.to_string())],
            "initialize rate limit",
        )
            .into_response();
    }

    next.run(req).await
}

fn peer_ip(headers: &HeaderMap, addr: SocketAddr, trust_xff: bool) -> IpAddr {
    if trust_xff
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .and_then(|s| s.parse::<IpAddr>().ok())
    {
        return ip;
    }
    addr.ip()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::any;
    use std::net::Ipv4Addr;
    use tower::ServiceExt;

    fn dummy_addr() -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], 1234))
    }

    fn other_addr() -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 2], 1234))
    }

    fn build_app(ctx: Arc<GateCtx>) -> Router {
        Router::new()
            .fallback(any(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(ctx, gate))
    }

    fn req(method: Method, addr: SocketAddr, with_session: bool) -> Request<Body> {
        let mut b = Request::builder().method(method).uri("/");
        if with_session {
            b = b.header(SESSION_ID_HEADER, "fake-session-id");
        }
        let mut req = b.body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    #[tokio::test]
    async fn passes_through_get_requests() {
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 0, // would block POSTs
            limiter: PeerLimiter::per_minute(1),
            trust_forwarded_for: false,
        });
        let app = build_app(ctx);
        let res = app.oneshot(req(Method::GET, dummy_addr(), false)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn passes_through_post_with_session_id() {
        // Session cap of 0 would normally block; the session-id header bypasses the gate.
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 0,
            limiter: PeerLimiter::per_minute(1),
            trust_forwarded_for: false,
        });
        let app = build_app(ctx);
        let res = app.oneshot(req(Method::POST, dummy_addr(), true)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_initialize_when_session_cap_reached() {
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 0,
            limiter: PeerLimiter::per_minute(100),
            trust_forwarded_for: false,
        });
        let app = build_app(ctx);
        let res = app.oneshot(req(Method::POST, dummy_addr(), false)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "5");
    }

    #[tokio::test]
    async fn rate_limits_initialize_per_peer() {
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 1000,
            limiter: PeerLimiter::new(2.0, 0.001, 1024), // ~no refill
            trust_forwarded_for: false,
        });
        let app = build_app(ctx);

        for _ in 0..2 {
            let res = app.clone().oneshot(req(Method::POST, dummy_addr(), false)).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }
        let res = app.clone().oneshot(req(Method::POST, dummy_addr(), false)).await.unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(res.headers().get(header::RETRY_AFTER).is_some());

        // A different peer is unaffected.
        let res = app.oneshot(req(Method::POST, other_addr(), false)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn peer_ip_uses_socket_addr_by_default() {
        let h = HeaderMap::new();
        let addr: SocketAddr = "10.0.0.5:1234".parse().unwrap();
        assert_eq!(peer_ip(&h, addr, false), IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)));
    }

    #[test]
    fn peer_ip_ignores_xff_when_untrusted() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let addr: SocketAddr = "10.0.0.5:1234".parse().unwrap();
        assert_eq!(peer_ip(&h, addr, false), IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)));
    }

    #[test]
    fn peer_ip_uses_xff_first_hop_when_trusted() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());
        let addr: SocketAddr = "10.0.0.5:1234".parse().unwrap();
        assert_eq!(peer_ip(&h, addr, true), IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
    }

    #[test]
    fn peer_ip_falls_back_to_socket_when_xff_unparseable() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "garbage".parse().unwrap());
        let addr: SocketAddr = "10.0.0.5:1234".parse().unwrap();
        assert_eq!(peer_ip(&h, addr, true), IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)));
    }
}
