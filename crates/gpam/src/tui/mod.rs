mod approval_queue;
mod approve;
mod browse;
mod keymap;
mod request;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, cursor};
use gpam_core::app::App;
use gpam_core::app::browse::BrowseExit;
use gpam_core::backend::DynBackend;
use gpam_core::cache::Cache;
use gpam_core::logs::SharedLogState;
use gpam_core::refresh::RefreshOptions;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::Mutex;

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

/// One-shot standalone modal for the `gpam approve <name>` subcommand. Sets
/// up its own terminal, runs the approve modal, and always restores the
/// terminal on exit.
pub async fn run_approve(backend: DynBackend, name: String) -> Result<()> {
    let mut term = init_terminal()?;
    let result = approve::run(&mut term, backend, name).await;
    let _ = restore_terminal();
    result.map(|_| ())
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
    let mut app = App::start(
        account,
        cache,
        backend,
        refresh_options,
        poll_interval,
        log_state,
    )
    .await?;

    loop {
        match browse::run(term, &mut app.browse).await? {
            BrowseExit::Quit => return Ok(()),
            BrowseExit::Request(selection) => {
                // request::run handles its own summary screen on errors.
                // Browse picks up successful grants via the poller's channel.
                let _ = request::run(term, &app.poller, selection).await?;
            }
            BrowseExit::OpenApprovalQueue => {
                match approval_queue::run(term, app.backend.clone(), app.approval_queue.clone())
                    .await?
                {
                    approval_queue::QueueExit::Back => {}
                    approval_queue::QueueExit::Quit => return Ok(()),
                }
            }
        }
        // Always return to browse. Only q from browse is the exit.
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
