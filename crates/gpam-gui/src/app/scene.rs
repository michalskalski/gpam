//! A scripted demo scene for the README screenshot. With `--screenshot` the
//! app arranges the demo data into a state that shows its main features,
//! saves a picture of the window and exits.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use eframe::egui;
use gpam_core::app::action::Action;
use gpam_core::app::request::RequestAction;

use super::{GuiApp, Intent, View};

/// Grants the scene requests: scope ID, entitlement, duration and
/// justification. The first is auto-approved and becomes active. The second
/// needs approval and stays pending.
const GRANTS: &[(&str, &str, &str, &str)] = &[
    ("acme-staging", "secrets-admin", "1h", ""),
    (
        "acme-prod-platform",
        "k8s-admin",
        "2h",
        "Rolling out the ingress fix",
    ),
];

/// Role label put into the search field.
const ROLE: &str = "admin";

/// Rows marked for a request: scope ID and entitlement.
const MARKED: &[(&str, &str)] = &[
    ("acme-staging", "k8s-admin"),
    ("acme-prod-platform", "storage-admin"),
];

/// Row the cursor rests on, which the details panel shows.
const CURSOR: (&str, &str) = ("300000001", "org-policy-admin");

/// The scene gives up when a step takes longer than this.
const STEP_TIMEOUT: Duration = Duration::from_secs(40);

enum Step {
    /// Wait for the demo entitlements to load.
    Load,
    /// Open the request dialog for a grant and confirm it.
    Request(usize),
    /// Wait for that request to finish.
    Requested(usize),
    /// Set the search, the marks, the cursor and the panels.
    Arrange,
    /// Wait for the grant states and for the status bar to clear.
    Settle,
    /// Wait for the picture to be written.
    Shot,
}

impl Step {
    fn name(&self) -> &'static str {
        match self {
            Step::Load => "load",
            Step::Request(_) => "request",
            Step::Requested(_) => "requested",
            Step::Arrange => "arrange",
            Step::Settle => "settle",
            Step::Shot => "shot",
        }
    }
}

pub struct Scene {
    path: PathBuf,
    step: Step,
    since: Instant,
    saved: Arc<AtomicBool>,
}

impl Scene {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            step: Step::Load,
            since: Instant::now(),
            saved: Arc::new(AtomicBool::new(false)),
        }
    }

    fn go(&mut self, step: Step) {
        self.step = step;
        self.since = Instant::now();
    }

    /// Advance the scene. Called once per frame.
    pub fn tick(&mut self, app: &mut GuiApp, ctx: &egui::Context) {
        ctx.request_repaint();
        if self.since.elapsed() > STEP_TIMEOUT {
            let state = &app.app.browse;
            eprintln!(
                "screenshot scene: step {} timed out (grants: {:?}, status: {:?}, browse view: {})",
                self.step.name(),
                state
                    .live_grants()
                    .map(|g| g.state.to_string())
                    .collect::<Vec<_>>(),
                state.status(),
                matches!(app.view, View::Browse),
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        match self.step {
            Step::Load => {
                let state = &app.app.browse;
                if !state.refresh_in_flight() && !state.entitlements().is_empty() {
                    self.go(Step::Request(0));
                }
            }
            Step::Request(i) => {
                let Some((scope, name, duration, why)) = GRANTS.get(i) else {
                    self.go(Step::Arrange);
                    return;
                };
                let Some(pos) = position(app, scope, name) else {
                    eprintln!("screenshot scene: no entitlement {scope}/{name}");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                };
                app.apply(ctx, Intent::OpenRow(pos));
                if let View::Request(r) = &mut app.view {
                    r.flow.set_duration(duration);
                    r.flow.set_justification(why);
                }
                // Confirming in the duration field moves on to a required
                // justification, and confirming there sends the request.
                for _ in 0..2 {
                    if matches!(&app.view, View::Request(r) if !r.busy) {
                        app.apply(ctx, Intent::Request(RequestAction::Confirm));
                    }
                }
                self.go(Step::Requested(i));
            }
            Step::Requested(i) => {
                if matches!(app.view, View::Browse) {
                    self.go(Step::Request(i + 1));
                }
            }
            Step::Arrange => {
                app.app.browse.add_role_filter(ROLE);
                for (scope, name) in MARKED {
                    if let Some(pos) = position(app, scope, name) {
                        app.app.browse.toggle_mark_at(pos);
                    }
                }
                if let Some(pos) = position(app, CURSOR.0, CURSOR.1) {
                    app.app.browse.set_cursor(pos);
                }
                // Opening the logs marks every warning as read, so the status
                // bar shows no warning badge.
                app.app.browse.apply(Action::ShowLogs);
                app.app.browse.close_overlays();
                app.nav_open = true;
                app.details_open = true;
                app.grants_open = true;
                app.scroll_to_cursor = true;
                self.go(Step::Settle);
            }
            Step::Settle => {
                let state = &app.app.browse;
                let active = state.live_grants().filter(|g| g.state.is_active()).count();
                let pending = state.live_grants().count() - active;
                let settled = active == 1
                    && pending == GRANTS.len() - 1
                    && state.status().is_empty()
                    && self.since.elapsed() > Duration::from_secs(1);
                if settled {
                    let path = self.path.clone();
                    let saved = self.saved.clone();
                    let repaint = ctx.clone();
                    ctx.request_screenshot(move |image| {
                        if let Err(err) = save_png(&path, &image) {
                            eprintln!("screenshot scene: {err:#}");
                        }
                        saved.store(true, Ordering::Relaxed);
                        repaint.request_repaint();
                    });
                    self.go(Step::Shot);
                }
            }
            Step::Shot => {
                if self.saved.load(Ordering::Relaxed) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }
}

/// Position in the visible list of the entitlement `name` on scope `scope`.
fn position(app: &GuiApp, scope: &str, name: &str) -> Option<usize> {
    app.app
        .browse
        .visible_rows()
        .position(|(_, ent)| ent.scope_id == scope && ent.short_name == name)
}

fn save_png(path: &Path, image: &egui::ColorImage) -> Result<()> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let [width, height] = image.size;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(image.as_raw())?;
    Ok(())
}
