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
use crate::gcp::grants::is_terminal;
use crate::poller::{PollMode, Poller};
use crate::refresh::RefreshOptions;

pub type Term = Terminal<CrosstermBackend<io::Stdout>>;

pub async fn run(
    account: String,
    cache: Arc<Mutex<Cache>>,
    backend: DynBackend,
    refresh_options: RefreshOptions,
    poll_interval: Duration,
) -> Result<()> {
    let mut term = init_terminal()?;
    let result = run_inner(
        &mut term,
        account,
        cache,
        backend,
        refresh_options,
        poll_interval,
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
) -> Result<()> {
    // Channel for grant-state updates from background pollers to the browse
    // strip. Buffer is generous because pollers send aggressively and the
    // browse loop drains lazily (e.g. while inside the request modal).
    let (grant_tx, grant_rx) = mpsc::channel(256);

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
    )
    .await?;

    loop {
        let Some(selection) = browse.run(term).await? else {
            return Ok(());
        };

        // request::run handles its own summary screen on errors.
        // the strip on browse will pick up successful grants via the poller's channel.
        let _ = request::run(term, poller.clone(), selection).await?;
        // always return to browse, only q from browse is the exit.
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
        cache.list_tracked_grants(now).unwrap_or_default()
    };
    for row in rows {
        if is_terminal(&row.state) {
            continue;
        }
        let mode = if row.state.contains("Active") {
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
