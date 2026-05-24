mod approval_queue;
mod browse;
mod keymap;
mod request;
pub mod widgets;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, cursor};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{Mutex, mpsc};

use crate::backend::DynBackend;
use crate::cache::{Cache, now_unix};
use crate::ipc;
use crate::logs::SharedLogState;
use crate::poller::{PollMode, Poller};
use crate::refresh::RefreshOptions;

pub type Term = Terminal<CrosstermBackend<io::Stdout>>;

pub async fn run(
    account: String,
    cache: Arc<Mutex<Cache>>,
    backend: DynBackend,
    refresh_options: RefreshOptions,
    poll_interval: Duration,
    log_state: SharedLogState,
) -> Result<()> {
    let mut term = init_terminal()?;
    let result = run_inner(
        &mut term,
        account,
        cache,
        backend,
        refresh_options,
        poll_interval,
        log_state,
    )
    .await;
    // always restore terminal
    let _ = restore_terminal();
    result
}

async fn run_inner(
    term: &mut Term,
    account: String,
    cache: Arc<Mutex<Cache>>,
    backend: DynBackend,
    refresh_options: RefreshOptions,
    poll_interval: Duration,
    log_state: SharedLogState,
) -> Result<()> {
    // Channel for grant-state updates from background pollers to the browse
    // strip. Buffer is generous because pollers send aggressively and the
    // browse loop drains lazily (e.g. while inside the request modal).
    let (grant_tx, grant_rx) = mpsc::channel(256);

    // Shared queue of inbound approval events, drained by the approval-queue
    // screen and surfaced by browse's status bar.
    let approval_queue = approval_queue::new_queue();

    // Socket listener for forwarded approval events. `bind_or_skip` returns
    // None (with a logged warning) when another gpam instance already owns
    // the socket, so a second TUI still runs but without IPC. The cleanup
    // guard unlinks the socket on the way out, even on panic.
    let _socket_cleanup = match ipc::bind_or_skip(&ipc::socket_path()?)? {
        Some((listener, cleanup)) => {
            let (approval_tx, mut approval_rx) = mpsc::channel(64);
            tokio::spawn(ipc::listen(listener, approval_tx));
            let queue_for_drainer = approval_queue.clone();
            tokio::spawn(async move {
                while let Some(event) = approval_rx.recv().await {
                    tracing::info!(
                        "approval received via socket: {} (source: {})",
                        event.name,
                        event.source.as_deref().unwrap_or("-")
                    );
                    let queued = approval_queue::QueuedEvent {
                        event,
                        received_at: std::time::SystemTime::now(),
                    };
                    if let Ok(mut q) = queue_for_drainer.lock() {
                        q.push_back(queued);
                    }
                }
            });
            Some(cleanup)
        }
        None => None,
    };

    let poller = Poller {
        backend: backend.clone(),
        cache: cache.clone(),
        tx: grant_tx,
        interval: poll_interval,
    };

    // Resume polling on the grants we already know about from prior runs.
    spawn_initial_pollers(&poller).await;

    let mut browse = browse::BrowseScreen::new(
        account,
        cache.clone(),
        backend.clone(),
        refresh_options,
        grant_rx,
        approval_queue.clone(),
        log_state,
    )
    .await?;

    loop {
        match browse.run(term).await? {
            browse::BrowseExit::Quit => return Ok(()),
            browse::BrowseExit::Request(selection) => {
                // request::run handles its own summary screen on errors.
                // The strip on browse will pick up successful grants via the
                // poller's channel.
                let _ = request::run(term, poller.clone(), selection).await?;
            }
            browse::BrowseExit::OpenApprovalQueue => {
                match approval_queue::run(term, approval_queue.clone()).await? {
                    approval_queue::QueueExit::Back => {}
                    approval_queue::QueueExit::Quit => return Ok(()),
                }
            }
        }
        // Always return to browse. Only q from browse is the exit.
    }
}

/// On launch, look at every grant the cache says is non-terminal (or recently
/// terminal) and spawn the appropriate poller. Active rows get a single
/// confirmation poll so we catch out-of-band revokes; everything else gets
/// the standard 3s tracker.
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

fn init_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(cursor::Show)?;
    stdout.execute(LeaveAlternateScreen)?;
    Ok(())
}
