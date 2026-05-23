use std::sync::Arc;
use std::time::Duration as StdDuration;

use tokio::sync::{Mutex, mpsc};
use tokio::time::sleep;
use tracing::warn;

use crate::backend::DynBackend;
use crate::cache::{Cache, now_unix};
use crate::gcp::grants::GrantState;

/// Default cadence for real GCP. `--demo` overrides this with a smaller value.
pub const DEFAULT_POLL_INTERVAL: StdDuration = StdDuration::from_secs(3);

/// Wake-up sent by the poller after persisting one observation to the cache.
/// The receiver re-reads the grant rows from the DB on each update, so the
/// signal itself carries no payload.
#[derive(Debug, Clone)]
pub struct GrantUpdate;

#[derive(Debug, Clone, Copy)]
pub enum PollMode {
    /// Poll until the grant transitions to Active or a terminal state. Used
    /// for newly-submitted grants and for pending grants found in the cache
    /// on launch.
    Track,
    /// Do a single get_grant call to reconcile cache state with the server,
    /// then stop. Used on launch for grants the cache already knows are Active.
    ConfirmOnce,
}

/// Shared environment for grant pollers: the backend to query, the cache to
/// persist into, the channel that wakes the UI, and the poll cadence. Cheap
/// to clone.
#[derive(Clone)]
pub struct Poller {
    pub backend: DynBackend,
    pub cache: Arc<Mutex<Cache>>,
    pub tx: mpsc::Sender<GrantUpdate>,
    pub interval: StdDuration,
}

impl Poller {
    /// Poll a single grant according to `mode`. Persists every observation to
    /// the cache and forwards a [`GrantUpdate`] on `tx`. Returns when the
    /// grant reaches a terminal/active state (Track) or after one poll
    /// (ConfirmOnce).
    pub async fn poll_grant(
        self,
        grant_name: String,
        requested_duration_secs: i64,
        initial_state: GrantState,
        mode: PollMode,
    ) {
        let mut last_state = initial_state;
        let already_active = last_state.is_active();

        loop {
            match self.backend.get_grant_state(&grant_name).await {
                Ok(state) => {
                    let now = now_unix();
                    let became_active =
                        state.is_active() && !already_active && !last_state.is_active();
                    let terminal = state.is_terminal();

                    let (activated_at, expires_at) = if became_active {
                        let exp = now + requested_duration_secs;
                        (Some(now), Some(exp))
                    } else {
                        (None, None)
                    };

                    {
                        let cache = self.cache.lock().await;
                        let _ = cache.update_grant_state(
                            &grant_name,
                            &state,
                            activated_at,
                            expires_at,
                            now,
                        );
                    }

                    let stop =
                        terminal || state.is_active() || matches!(mode, PollMode::ConfirmOnce);
                    let _ = self.tx.send(GrantUpdate).await;
                    if stop {
                        return;
                    }
                    last_state = state;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    // PERMISSION_DENIED or NOT_FOUND on get_grant means we can
                    // no longer observe this grant - usually because the host
                    // project was deleted or access was revoked. Treat it as
                    // terminal locally so we stop polling forever.
                    if msg.contains("PERMISSION_DENIED") || msg.contains("NOT_FOUND") {
                        warn!(
                            grant = %grant_name,
                            error = %msg,
                            "grant unobservable, marking Revoked locally and stopping poll"
                        );
                        {
                            let cache = self.cache.lock().await;
                            let _ = cache.update_grant_state(
                                &grant_name,
                                &GrantState::Revoked,
                                None,
                                None,
                                now_unix(),
                            );
                        }
                        let _ = self.tx.send(GrantUpdate).await;
                        return;
                    }
                    warn!(grant = %grant_name, error = %msg, "grant poll failed");
                    if matches!(mode, PollMode::ConfirmOnce) {
                        return;
                    }
                }
            }
            sleep(self.interval).await;
        }
    }
}
