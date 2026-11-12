use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt};

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub level: Level,
    pub message: String,
    pub at: SystemTime,
}

#[derive(Debug)]
pub struct LogState {
    pub last: Option<LogEntry>,
    pub unread: usize,
    pub log_path: PathBuf,
}

impl LogState {
    pub fn clear_unread(&mut self) {
        self.unread = 0;
    }
}

pub type SharedLogState = Arc<Mutex<LogState>>;

/// Wire up tracing: append-only file under the platform cache dir plus an
/// in-memory layer that feeds the TUI footer and `L` popup.
pub fn init() -> Result<SharedLogState> {
    let log_path = log_file_path();
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating log dir {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;

    let state: SharedLogState = Arc::new(Mutex::new(LogState {
        last: None,
        unread: 0,
        log_path: log_path.clone(),
    }));

    let file_layer = fmt::layer().with_writer(Mutex::new(file)).with_ansi(false);
    let buffer_layer = BufferLayer {
        state: state.clone(),
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    Registry::default()
        .with(filter)
        .with(file_layer)
        .with(buffer_layer)
        .init();

    Ok(state)
}

struct BufferLayer {
    state: SharedLogState,
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        let level = *event.metadata().level();
        // Level ordering: ERROR < WARN < INFO < DEBUG < TRACE. Keep WARN+.
        if level > Level::WARN {
            return;
        }
        let mut v = MessageVisitor::default();
        event.record(&mut v);
        if let Ok(mut state) = self.state.lock() {
            state.last = Some(LogEntry {
                level,
                message: v.into_message(),
                at: SystemTime::now(),
            });
            state.unread = state.unread.saturating_add(1);
        }
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl MessageVisitor {
    fn into_message(self) -> String {
        let mut out = self.message;
        for (k, v) in self.fields {
            out.push(' ');
            out.push_str(&k);
            out.push('=');
            out.push_str(&v);
        }
        out
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let v = format!("{value:?}");
        if field.name() == "message" {
            self.message = v;
        } else {
            self.fields.push((field.name().to_string(), v));
        }
    }
}

fn log_file_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "gpam")
        .map(|p| p.cache_dir().join("gpam.log"))
        .unwrap_or_else(|| PathBuf::from("./gpam.log"))
}
