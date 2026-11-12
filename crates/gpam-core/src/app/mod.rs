//! Frontend-agnostic application state. Each view is a plain struct that
//! takes small action enums and exposes read accessors for drawing. A
//! frontend maps its input events to those actions, drives the async loop,
//! and renders the state.

pub mod action;
pub mod approval_queue;
pub mod approve;
pub mod browse;
pub mod format;
pub mod help;
pub mod request;
pub mod scope_tree;
pub mod text_input;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, mpsc};

use crate::backend::DynBackend;
use crate::cache::{Cache, now_unix};
use crate::ipc::{self, SocketCleanup};
use crate::logs::SharedLogState;
use crate::poller::{PollMode, Poller};
use crate::refresh::RefreshOptions;

use approval_queue::Queue;
use browse::BrowseState;

/// Long-lived state for an interactive session: the browse view, the grant
/// poller environment, and the approval queue fed by the local socket.
pub struct App {
    pub browse: BrowseState,
    pub poller: Poller,
    pub backend: DynBackend,
    pub approval_queue: Queue,
    /// Unlinks the socket when the app is dropped, even on panic.
    _socket_cleanup: Option<SocketCleanup>,
}

impl App {
    /// Bind the approval socket, resume polling on grants known from prior
    /// runs, and load the browse view from the cache. Must be called from
    /// inside a tokio runtime.
    pub async fn start(
        account: String,
        cache: Arc<Mutex<Cache>>,
        backend: DynBackend,
        refresh_options: RefreshOptions,
        poll_interval: Duration,
        log_state: SharedLogState,
    ) -> Result<Self> {
        // Channel for grant-state updates from background pollers to the
        // browse view. The buffer is generous because pollers send
        // aggressively and the browse view drains lazily (e.g. while the
        // request flow is open).
        let (grant_tx, grant_rx) = mpsc::channel(256);

        let approval_queue = approval_queue::new_queue();
        let socket_cleanup = listen_for_approvals(&approval_queue)?;

        let poller = Poller {
            backend: backend.clone(),
            cache: cache.clone(),
            tx: grant_tx,
            interval: poll_interval,
        };
        spawn_initial_pollers(&poller).await;

        let browse = BrowseState::new(
            account,
            cache,
            backend.clone(),
            refresh_options,
            grant_rx,
            approval_queue.clone(),
            log_state,
        )
        .await;

        Ok(Self {
            browse,
            poller,
            backend,
            approval_queue,
            _socket_cleanup: socket_cleanup,
        })
    }
}

/// Socket listener for forwarded approval events. `bind_or_skip` returns
/// None (with a logged warning) when another gpam instance already owns the
/// socket, so a second instance still runs but without IPC.
fn listen_for_approvals(queue: &Queue) -> Result<Option<SocketCleanup>> {
    let Some((listener, cleanup)) = ipc::bind_or_skip(&ipc::socket_path()?)? else {
        return Ok(None);
    };
    let (approval_tx, mut approval_rx) = mpsc::channel(64);
    tokio::spawn(ipc::listen(listener, approval_tx));
    let queue = queue.clone();
    tokio::spawn(async move {
        while let Some(event) = approval_rx.recv().await {
            tracing::info!(
                "approval received via socket: {} (source: {})",
                event.name,
                event.source.as_deref().unwrap_or("-")
            );
            approval_queue::push_event(&queue, event);
        }
    });
    Ok(Some(cleanup))
}

/// On launch, look at every grant the cache says is non-terminal (or recently
/// terminal) and spawn the appropriate poller. Active rows get a single
/// confirmation poll so we catch out-of-band revokes. Everything else gets
/// the standard tracker.
async fn spawn_initial_pollers(poller: &Poller) {
    let now = now_unix();
    let rows = {
        let cache = poller.cache.lock().await;
        // Locally expire any Active grant whose expires_at has passed, so we
        // don't waste a poll (and surface a confusing error) on a grant that
        // has already ended.
        let _ = cache.expire_stale_active_grants(now);
        cache.list_tracked_grants(now).unwrap_or_default()
    };
    for row in rows {
        if row.state.is_terminal() {
            continue;
        }
        let mode = if row.state.is_active() {
            PollMode::ConfirmOnce
        } else {
            PollMode::Track
        };
        tokio::spawn(poller.clone().poll_grant(
            row.name,
            row.requested_duration_secs,
            row.state,
            mode,
        ));
    }
}
