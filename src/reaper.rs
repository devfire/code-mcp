use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::transport::streamable_http_server::session::{
    SessionId, SessionManager, local::LocalSessionManager,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Per-session last-activity timestamps. The gate middleware bumps an
/// entry on every request that carries a session id; the reaper closes
/// any session whose last bump is older than the idle timeout.
#[derive(Default)]
pub struct ActivityTracker {
    inner: RwLock<HashMap<SessionId, Instant>>,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record activity for the given session id (called by the gate middleware
    /// on every request that carries a session id). Updates the last-seen
    /// timestamp to `Instant::now()`.
    pub async fn touch(&self, id: SessionId) {
        self.inner.write().await.insert(id, Instant::now());
    }

    #[cfg(test)]
    pub async fn set_for_test(&self, id: SessionId, when: Instant) {
        self.inner.write().await.insert(id, when);
    }
}

/// Periodically close any session whose last tracked activity is older
/// than `idle_timeout`. Sessions that exist in the manager but have no
/// tracker entry yet (just-initialized, never touched) are seeded with
/// `now()` on the first sweep so they get a full idle window.
pub async fn reap_loop(
    manager: Arc<LocalSessionManager>,
    tracker: Arc<ActivityTracker>,
    idle_timeout: Duration,
    interval: Duration,
    cancel: CancellationToken,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // first tick fires immediately; skip it
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {
                sweep(&manager, &tracker, idle_timeout).await;
            }
        }
    }
}

async fn sweep(manager: &LocalSessionManager, tracker: &ActivityTracker, idle_timeout: Duration) {
    let now = Instant::now();
    let live_ids: Vec<SessionId> = {
        let s = manager.sessions.read().await;
        s.keys().cloned().collect()
    };

    let to_close: Vec<SessionId> = {
        let mut activity = tracker.inner.write().await;
        let live_set: HashSet<SessionId> = live_ids.iter().cloned().collect();
        activity.retain(|id, _| live_set.contains(id));
        let mut to_close = Vec::new();
        for id in &live_ids {
            let last = *activity.entry(id.clone()).or_insert(now);
            if now.duration_since(last) > idle_timeout {
                to_close.push(id.clone());
            }
        }
        to_close
    };

    if to_close.is_empty() {
        return;
    }

    tracing::info!(count = to_close.len(), "reaping idle sessions");
    for id in to_close {
        if let Err(e) = manager.close_session(&id).await {
            tracing::warn!(error = %e, "failed to close idle session");
        }
        tracker.inner.write().await.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn live_count(manager: &LocalSessionManager) -> usize {
        manager.sessions.read().await.len()
    }

    #[tokio::test]
    async fn first_sweep_seeds_unknown_sessions_and_does_not_reap() {
        let manager = Arc::new(LocalSessionManager::default());
        let tracker = Arc::new(ActivityTracker::new());

        let (_id, _transport) = manager.create_session().await.unwrap();
        assert_eq!(live_count(&manager).await, 1);

        // Brand-new session, no tracker entry yet. Sweep should seed
        // its activity at "now" rather than reap it.
        sweep(&manager, &tracker, Duration::from_secs(60)).await;
        assert_eq!(live_count(&manager).await, 1);
    }

    #[tokio::test]
    async fn sweep_reaps_idle_session() {
        let manager = Arc::new(LocalSessionManager::default());
        let tracker = Arc::new(ActivityTracker::new());

        let (id, _transport) = manager.create_session().await.unwrap();
        // Backdate activity well past the idle threshold.
        tracker
            .set_for_test(id.clone(), Instant::now() - Duration::from_secs(3600))
            .await;

        sweep(&manager, &tracker, Duration::from_secs(60)).await;
        assert_eq!(live_count(&manager).await, 0);
        assert!(tracker.inner.read().await.is_empty());
    }

    #[tokio::test]
    async fn sweep_drops_tracker_entries_for_dead_sessions() {
        let manager = Arc::new(LocalSessionManager::default());
        let tracker = Arc::new(ActivityTracker::new());

        // Tracker has an entry for a session that was never registered
        // (e.g. closed via DELETE before this sweep). Stale entry must
        // be evicted so the map can't grow unboundedly.
        let ghost: SessionId = Arc::from("ghost-session");
        tracker.touch(ghost.clone()).await;

        sweep(&manager, &tracker, Duration::from_secs(60)).await;
        assert!(!tracker.inner.read().await.contains_key(&ghost));
    }

    #[tokio::test]
    async fn touch_keeps_active_session_alive() {
        let manager = Arc::new(LocalSessionManager::default());
        let tracker = Arc::new(ActivityTracker::new());

        let (id, _transport) = manager.create_session().await.unwrap();
        tracker.touch(id.clone()).await;

        // Threshold is 1h, last touch was just now → no reap.
        sweep(&manager, &tracker, Duration::from_secs(3600)).await;
        assert_eq!(live_count(&manager).await, 1);
    }
}
