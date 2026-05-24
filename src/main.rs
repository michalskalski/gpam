mod auth;
mod backend;
mod cache;
mod fuzzy;
mod gcp;
mod ipc;
mod logs;
mod poller;
mod refresh;
mod tui;

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::backend::{Backend, demo::DemoBackend, gcp::GcpBackend};
use crate::poller::DEFAULT_POLL_INTERVAL;
use crate::refresh::RefreshOptions;

const DEMO_POLL_INTERVAL: Duration = Duration::from_millis(800);

/// Terminal UI for Google Cloud Privileged Access Manager.
///
/// Application Default Credentials must be available. If they aren't,
/// run: gcloud auth application-default login
#[derive(Debug, Parser)]
#[command(name = "gpam", version, about, long_about = None)]
struct Args {
    /// Run with seeded fixtures instead of talking to GCP.
    #[arg(long, global = true)]
    demo: bool,
    /// Skip project-scope entitlement discovery.
    #[arg(long, global = true)]
    no_projects: bool,
    /// Skip folder-scope entitlement discovery.
    #[arg(long, global = true)]
    no_folders: bool,
    /// Skip organization-scope entitlement discovery.
    #[arg(long, global = true)]
    no_orgs: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Forward a pending grant to a running gpam TUI via its local socket.
    ///
    /// Exits non-zero if no TUI is listening, so shell fallbacks compose
    /// cleanly (e.g. `gpam send <name> || gpam approve <name>`).
    Send {
        /// Fully-qualified PAM grant resource name.
        name: String,
        /// Tag stored with the event (e.g. applescript, pubsub). Defaults to cli.
        #[arg(long)]
        source: Option<String>,
    },
}

impl Args {
    fn refresh_options(&self) -> RefreshOptions {
        RefreshOptions {
            projects: !self.no_projects,
            folders: !self.no_folders,
            organizations: !self.no_orgs,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        Some(Command::Send { name, source }) => run_send(name, source).await,
        None => run_tui(args).await,
    }
}

async fn run_send(name: String, source: Option<String>) -> Result<()> {
    if !ipc::is_valid_grant_name(&name) {
        bail!(
            "'{name}' is not a valid grant resource name; expected '<organizations|folders|projects>/<id>/locations/<loc>/entitlements/<ent>/grants/<id>'"
        );
    }
    let event = ipc::ApprovalEvent {
        name,
        source: source.or_else(|| Some("cli".into())),
    };
    ipc::send(&ipc::socket_path()?, &event).await
}

async fn run_tui(args: Args) -> Result<()> {
    let log_state = logs::init()?;

    let (account, backend): (String, Arc<dyn Backend>) = if args.demo {
        ("demo".into(), Arc::new(DemoBackend::new()))
    } else {
        let session = auth::resolve().await?;
        let backend = GcpBackend::new(session.credentials).await?;
        (session.account, Arc::new(backend))
    };

    let poll_interval = if args.demo {
        DEMO_POLL_INTERVAL
    } else {
        DEFAULT_POLL_INTERVAL
    };
    let cache = Arc::new(Mutex::new(cache::Cache::open(&account)?));
    tui::run(
        account,
        cache,
        backend,
        args.refresh_options(),
        poll_interval,
        log_state,
    )
    .await
}
