use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::{SessionId, local::LocalSessionManager};

use crate::limiter::PeerLimiter;
use crate::reaper::ActivityTracker;

/// Header name (case-insensitive) carrying the MCP session id.
/// rmcp defines this internally; we use the literal to avoid coupling
/// to a private module path.
const SESSION_ID_HEADER: &str = "mcp-session-id";

/// Shared state for the [`gate`] middleware.
///
/// Held in an `Arc` and passed to the middleware via `from_fn_with_state`.
/// Fields are `pub` because the middleware closure reads them directly; the
/// type is constructed once in `main` and never mutated thereafter.
pub struct GateCtx {
    /// The rmcp session manager — used to read the live session count for the
    /// `--max-sessions` cap.
    pub sessions: Arc<LocalSessionManager>,
    /// Hard cap on concurrent stateful sessions.
    pub max_sessions: usize,
    /// Per-peer token-bucket limiter for new initialize POSTs.
    pub limiter: PeerLimiter,
    /// Whether to trust `X-Forwarded-For` (rightmost entry) as the peer IP.
    pub trust_forwarded_for: bool,
    /// Per-session last-activity timestamps, bumped by the gate and read by
    /// the reaper.
    pub activity: Arc<ActivityTracker>,
}

/// axum middleware that gates **new-session** POSTs (no `Mcp-Session-Id`
/// header, or a header naming no live session). It enforces:
///   1. A hard cap on concurrent sessions held by the [`LocalSessionManager`].
///   2. A per-peer rate limit on initialize requests.
///
/// All other traffic (GET, DELETE, POSTs that carry a known session id) is passed
/// through untouched. An unknown session id never touches the activity
/// tracker, so forged ids cannot grow it or bypass the checks above.
///
/// # Approximate cap under burst
///
/// The cap is a `len()` check-then-create sequence: two concurrent
/// initializes can both observe `in_use < max_sessions` before either session
/// is inserted downstream, overshooting the cap by the burst size. This is
/// accepted as approximate (matching the pre-existing behaviour): the reaper
/// bounds idle-session lifetime and the per-peer limiter bounds sustained
/// creation rate, so a burst only overshoots transiently. Making it exact
/// would require holding a semaphore across session creation and release on
/// session close — i.e. owning the session lifecycle, which lives inside
/// rmcp's `StreamableHttpService`, not here.
pub async fn gate(
    State(ctx): State<Arc<GateCtx>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(id) = session_id_from_headers(req.headers()) {
        // Only a live session bypasses the gate. A forged/unknown id falls
        // through to the new-session checks below and must not create an
        // activity-tracker entry (unbounded growth from minted ids).
        let known = ctx.sessions.sessions.read().await.contains_key(&id);
        if known {
            ctx.activity.touch(id).await;
            return next.run(req).await;
        }
    }
    if req.method() != Method::POST {
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

fn session_id_from_headers(headers: &HeaderMap) -> Option<SessionId> {
    headers
        .get(SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(SessionId::from)
}

fn peer_ip(headers: &HeaderMap, addr: SocketAddr, trust_xff: bool) -> IpAddr {
    if trust_xff
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit(',').next())
            .map(str::trim)
            .and_then(|s| s.parse::<IpAddr>().ok())
    {
        tracing::debug!(peer = %ip, socket = %addr.ip(), "resolved peer IP from X-Forwarded-For");
        return ip;
    }
    addr.ip()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::any;
    use rmcp::transport::streamable_http_server::session::SessionManager;
    use std::net::Ipv4Addr;
    use std::num::NonZeroU32;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

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

    fn req(method: Method, addr: SocketAddr, session: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri("/");
        if let Some(id) = session {
            b = b.header(SESSION_ID_HEADER, id);
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
            limiter: PeerLimiter::per_minute(nz(1)),
            trust_forwarded_for: false,
            activity: Arc::new(ActivityTracker::new()),
        });
        let app = build_app(ctx);
        let res = app
            .oneshot(req(Method::GET, dummy_addr(), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn passes_through_post_with_known_session_id() {
        // Session cap of 0 would normally block; a live session bypasses the gate.
        let sessions = Arc::new(LocalSessionManager::default());
        let (id, _transport) = sessions.create_session().await.unwrap();
        let activity = Arc::new(ActivityTracker::new());
        let ctx = Arc::new(GateCtx {
            sessions,
            max_sessions: 0,
            limiter: PeerLimiter::per_minute(nz(1)),
            trust_forwarded_for: false,
            activity: activity.clone(),
        });
        let app = build_app(ctx);
        let res = app
            .oneshot(req(Method::POST, dummy_addr(), Some(&id)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(activity.contains_for_test(&id).await);
    }

    #[tokio::test]
    async fn rejects_post_with_forged_session_id_when_cap_reached() {
        let activity = Arc::new(ActivityTracker::new());
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 0,
            limiter: PeerLimiter::per_minute(nz(100)),
            trust_forwarded_for: false,
            activity: activity.clone(),
        });
        let app = build_app(ctx);
        let forged: SessionId = Arc::from("forged-session-id");
        let res = app
            .oneshot(req(Method::POST, dummy_addr(), Some(&forged)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!activity.contains_for_test(&forged).await);
    }

    #[tokio::test]
    async fn forged_session_id_post_consumes_initialize_rate_limit() {
        let activity = Arc::new(ActivityTracker::new());
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 1000,
            limiter: PeerLimiter::per_minute(nz(1)),
            trust_forwarded_for: false,
            activity: activity.clone(),
        });
        let app = build_app(ctx);
        let forged: SessionId = Arc::from("forged-session-id");
        let res = app
            .clone()
            .oneshot(req(Method::POST, dummy_addr(), Some(&forged)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .oneshot(req(Method::POST, dummy_addr(), Some(&forged)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!activity.contains_for_test(&forged).await);
    }

    #[tokio::test]
    async fn forged_session_id_get_passes_without_tracking() {
        let activity = Arc::new(ActivityTracker::new());
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 0,
            limiter: PeerLimiter::per_minute(nz(1)),
            trust_forwarded_for: false,
            activity: activity.clone(),
        });
        let app = build_app(ctx);
        let forged: SessionId = Arc::from("forged-session-id");
        let res = app
            .oneshot(req(Method::GET, dummy_addr(), Some(&forged)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!activity.contains_for_test(&forged).await);
    }

    #[tokio::test]
    async fn rejects_initialize_when_session_cap_reached() {
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 0,
            limiter: PeerLimiter::per_minute(nz(100)),
            trust_forwarded_for: false,
            activity: Arc::new(ActivityTracker::new()),
        });
        let app = build_app(ctx);
        let res = app
            .oneshot(req(Method::POST, dummy_addr(), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "5");
    }

    #[tokio::test]
    async fn rate_limits_initialize_per_peer() {
        let ctx = Arc::new(GateCtx {
            sessions: Arc::new(LocalSessionManager::default()),
            max_sessions: 1000,
            limiter: PeerLimiter::new(2.0, 0.001, 1024).unwrap(), // ~no refill
            trust_forwarded_for: false,
            activity: Arc::new(ActivityTracker::new()),
        });
        let app = build_app(ctx);

        for _ in 0..2 {
            let res = app
                .clone()
                .oneshot(req(Method::POST, dummy_addr(), None))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }
        let res = app
            .clone()
            .oneshot(req(Method::POST, dummy_addr(), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(res.headers().get(header::RETRY_AFTER).is_some());

        // A different peer is unaffected.
        let res = app
            .oneshot(req(Method::POST, other_addr(), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn peer_ip_uses_socket_addr_by_default() {
        let h = HeaderMap::new();
        let addr: SocketAddr = "10.0.0.5:1234".parse().unwrap();
        assert_eq!(
            peer_ip(&h, addr, false),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))
        );
    }

    #[test]
    fn peer_ip_ignores_xff_when_untrusted() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let addr: SocketAddr = "10.0.0.5:1234".parse().unwrap();
        assert_eq!(
            peer_ip(&h, addr, false),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))
        );
    }

    #[test]
    fn peer_ip_uses_xff_last_hop_when_trusted() {
        // Behind AWS ALB the trusted proxy appends the real client IP at the
        // rightmost position; everything to its left is client-supplied and
        // forgeable.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());
        let addr: SocketAddr = "10.0.0.5:1234".parse().unwrap();
        assert_eq!(
            peer_ip(&h, addr, true),
            IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8))
        );
    }

    #[test]
    fn peer_ip_falls_back_to_socket_when_xff_unparseable() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "garbage".parse().unwrap());
        let addr: SocketAddr = "10.0.0.5:1234".parse().unwrap();
        assert_eq!(
            peer_ip(&h, addr, true),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))
        );
    }
}
