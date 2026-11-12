mod app;
mod approve;
mod browse;
mod keymap;
mod queue;
mod request;
mod style;

use std::sync::Arc;

use anyhow::{Result, anyhow};
use clap::Parser;
use eframe::egui;
use tokio::sync::Mutex;

use gpam_core::app::App;
use gpam_core::poller::{DEFAULT_POLL_INTERVAL, DEMO_POLL_INTERVAL};
use gpam_core::refresh::RefreshOptions;
use gpam_core::{backend, cache, logs};

/// Window size for the README screenshot, small enough to stay legible at
/// the width a README is shown.
#[cfg(feature = "screenshot")]
const SCREENSHOT_SIZE: [f32; 2] = [1200.0, 720.0];

const WINDOW_SIZE: [f32; 2] = [1200.0, 760.0];

/// Desktop UI for Google Cloud Privileged Access Manager.
///
/// Application Default Credentials must be available. If they aren't,
/// run: gcloud auth application-default login
#[derive(Debug, Parser)]
#[command(name = "gpam-gui", version, about, long_about = None)]
struct Args {
    /// Run with seeded fixtures instead of talking to GCP.
    #[arg(long)]
    demo: bool,
    /// Skip project-scope entitlement discovery.
    #[arg(long)]
    no_projects: bool,
    /// Skip folder-scope entitlement discovery.
    #[arg(long)]
    no_folders: bool,
    /// Skip organization-scope entitlement discovery.
    #[arg(long)]
    no_orgs: bool,
    /// Arrange the demo data into a showcase, save a picture of the window
    /// to this PNG file and exit. Used to make the README screenshot.
    #[cfg(feature = "screenshot")]
    #[arg(long, value_name = "PATH", requires = "demo")]
    screenshot: Option<std::path::PathBuf>,
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

fn main() -> Result<()> {
    let args = Args::parse();

    // Font lookup enumerates system fonts, so it overlaps with startup.
    let fonts = std::thread::spawn(style::load_system_fonts);

    // eframe owns the main thread. The runtime runs core's background tasks,
    // and the guard lets core call `tokio::spawn` from the UI thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = runtime.enter();

    let log_state = logs::init()?;
    let app = runtime.block_on(async {
        let (account, backend) = backend::resolve(args.demo).await?;
        let poll_interval = if args.demo {
            DEMO_POLL_INTERVAL
        } else {
            DEFAULT_POLL_INTERVAL
        };
        let cache = Arc::new(Mutex::new(cache::Cache::open(&account)?));
        App::start(
            account,
            cache,
            backend,
            args.refresh_options(),
            poll_interval,
            log_state,
        )
        .await
    })?;
    let fonts = fonts.join().unwrap_or_default();

    #[cfg(feature = "screenshot")]
    let window_size = if args.screenshot.is_some() {
        SCREENSHOT_SIZE
    } else {
        WINDOW_SIZE
    };
    #[cfg(not(feature = "screenshot"))]
    let window_size = WINDOW_SIZE;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("gpam")
            .with_app_id("gpam-gui")
            .with_inner_size(window_size)
            .with_min_inner_size([560.0, 380.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let handle = runtime.handle().clone();
    #[cfg(feature = "screenshot")]
    let screenshot = args.screenshot.clone();
    eframe::run_native(
        "gpam",
        options,
        Box::new(move |cc| {
            style::install(&cc.egui_ctx, fonts);
            let gui = app::GuiApp::new(app, handle);
            #[cfg(feature = "screenshot")]
            let gui = match screenshot {
                Some(path) => gui.with_screenshot(path),
                None => gui,
            };
            Ok(Box::new(gui))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}
