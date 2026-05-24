use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::gcp::grants::is_valid_grant_name;

/// An approval event delivered to a running gpam TUI over its local socket.
/// On the wire: one JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalEvent {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Sits next to the per-account cache (`~/Library/Caches/gpam/` on macOS,
/// `~/.cache/gpam/` on Linux). One well-known path per user, not per-account.
pub fn socket_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "gpam")
        .context("locating cache dir (set XDG_CACHE_HOME or HOME)")?;
    Ok(dirs.cache_dir().join("gpam.sock"))
}

/// Connect to a listening gpam TUI and forward a single event. Returns an
/// error tagged with the socket path when no listener is present, so shell
/// fallbacks (`gpam send … || gpam approve …`) can read a useful diagnostic.
pub async fn send(path: &Path, event: &ApprovalEvent) -> Result<()> {
    let mut stream = UnixStream::connect(path)
        .await
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow!(
                "no gpam TUI listening at {} (start gpam in another terminal first)",
                path.display()
            ),
            ErrorKind::ConnectionRefused => {
                anyhow!("stale socket at {} (no live listener)", path.display())
            }
            _ => anyhow!("connecting to {}: {e}", path.display()),
        })?;
    let mut line = serde_json::to_vec(event).context("encoding event")?;
    line.push(b'\n');
    stream.write_all(&line).await.context("writing to socket")?;
    stream.flush().await.context("flushing socket")?;
    // Half-close so the listener sees EOF after our single line.
    let _ = stream.shutdown().await;
    Ok(())
}

/// Removes the socket file when dropped. Pair it with `bind_or_skip` so the
/// listener and its cleanup live together. A panic anywhere above still runs
/// Drop on the way out, so we don't leave stale sockets behind.
pub struct SocketCleanup {
    path: PathBuf,
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Try-bind / probe / unlink for the TUI's listening socket.
///
/// Returns `Ok(Some(..))` when this process owns the socket (with a cleanup
/// guard that unlinks the path on drop). Returns `Ok(None)` when another
/// gpam is already listening or when we can't bind for any other reason.
/// In both cases a warning is logged and the TUI keeps running without IPC.
pub fn bind_or_skip(path: &Path) -> Result<Option<(UnixListener, SocketCleanup)>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
    }
    match UnixListener::bind(path) {
        Ok(listener) => {
            apply_owner_only_perms(path);
            Ok(Some((listener, SocketCleanup { path: path.into() })))
        }
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    tracing::warn!(
                        "another gpam is listening at {} — this instance will not receive forwarded events",
                        path.display()
                    );
                    Ok(None)
                }
                Err(_) => {
                    let _ = std::fs::remove_file(path);
                    match UnixListener::bind(path) {
                        Ok(listener) => {
                            apply_owner_only_perms(path);
                            Ok(Some((listener, SocketCleanup { path: path.into() })))
                        }
                        Err(e2) => {
                            tracing::warn!(
                                "could not rebind socket at {} after unlinking stale file: {e2}",
                                path.display()
                            );
                            Ok(None)
                        }
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!("could not bind socket at {}: {e}", path.display());
            Ok(None)
        }
    }
}

fn apply_owner_only_perms(path: &Path) {
    let perms = std::fs::Permissions::from_mode(0o600);
    if let Err(e) = std::fs::set_permissions(path, perms) {
        tracing::warn!("could not tighten permissions on {}: {e}", path.display());
    }
}

/// Accept connections forever, parse one JSON event per line from each
/// connection, and forward valid events through `tx`. Malformed lines and
/// connection errors are logged and dropped.
/// The loop never exits except when the listener itself becomes unusable.
pub async fn listen(listener: UnixListener, tx: mpsc::Sender<ApprovalEvent>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("ipc accept failed: {e}");
                return;
            }
        };
        let tx = tx.clone();
        tokio::spawn(handle_conn(stream, tx));
    }
}

async fn handle_conn(stream: UnixStream, tx: mpsc::Sender<ApprovalEvent>) {
    let reader = BufReader::new(stream);
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(raw)) => {
                let line = raw.trim();
                if line.is_empty() {
                    continue;
                }
                match parse_event(line) {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!("ipc: dropping malformed line: {e:#}"),
                }
            }
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("ipc: read error: {e}");
                return;
            }
        }
    }
}

/// Parse one JSONL event and validate the resource name shape.
pub fn parse_event(line: &str) -> Result<ApprovalEvent> {
    let event: ApprovalEvent = serde_json::from_str(line).with_context(|| "decoding event")?;
    if !is_valid_grant_name(&event.name) {
        return Err(anyhow!(
            "event name '{}' is not a valid grant resource name",
            event.name
        ));
    }
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORG_NAME: &str = "organizations/000000000000/locations/global/entitlements/example-org-ent/grants/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const PROJ_NAME: &str = "projects/111122223333/locations/global/entitlements/example-proj-ent/grants/11111111-2222-3333-4444-555555555555";

    #[test]
    fn event_round_trip_with_source() {
        let e = ApprovalEvent {
            name: ORG_NAME.into(),
            source: Some("cli".into()),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"source\":\"cli\""));
        let back: ApprovalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, e.name);
        assert_eq!(back.source.as_deref(), Some("cli"));
    }

    #[test]
    fn event_round_trip_omits_missing_source() {
        let e = ApprovalEvent {
            name: ORG_NAME.into(),
            source: None,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("source"));
        let back: ApprovalEvent = serde_json::from_str(&json).unwrap();
        assert!(back.source.is_none());
    }

    #[test]
    fn event_ignores_unknown_fields() {
        let json = r#"{"name":"organizations/1/locations/global/entitlements/e/grants/g","received_at":"2026-01-01","extra":42}"#;
        let back: ApprovalEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            back.name,
            "organizations/1/locations/global/entitlements/e/grants/g"
        );
        assert!(back.source.is_none());
    }

    #[test]
    fn event_requires_name() {
        let json = r#"{"source":"cli"}"#;
        let result: Result<ApprovalEvent, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_fails_when_no_listener() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.sock");
        let event = ApprovalEvent {
            name: ORG_NAME.into(),
            source: None,
        };
        let err = send(&path, &event).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no gpam TUI listening"), "got: {msg}");
    }

    #[tokio::test]
    async fn send_delivers_one_line_then_closes() {
        use tokio::io::AsyncBufReadExt;
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gpam.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let event = ApprovalEvent {
            name: ORG_NAME.into(),
            source: Some("test".into()),
        };
        let send_path = path.clone();
        let send_event = event.clone();
        let sender = tokio::spawn(async move { send(&send_path, &send_event).await });

        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let trimmed = line.trim_end_matches('\n');
        let back: ApprovalEvent = serde_json::from_str(trimmed).unwrap();
        assert_eq!(back.name, event.name);
        assert_eq!(back.source.as_deref(), Some("test"));

        // Sender half-closes after one line.
        let mut tail = String::new();
        reader.read_line(&mut tail).await.unwrap();
        assert!(tail.is_empty(), "expected EOF, got: {tail:?}");

        sender.await.unwrap().unwrap();
    }

    #[test]
    fn parse_event_accepts_valid_line() {
        let line =
            r#"{"name":"organizations/1/locations/global/entitlements/e/grants/g","source":"x"}"#;
        let e = parse_event(line).unwrap();
        assert_eq!(
            e.name,
            "organizations/1/locations/global/entitlements/e/grants/g"
        );
        assert_eq!(e.source.as_deref(), Some("x"));
    }

    #[test]
    fn parse_event_rejects_invalid_name() {
        let line = r#"{"name":"not-a-name"}"#;
        let err = parse_event(line).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a valid grant resource name"),
            "got: {msg}"
        );
    }

    #[test]
    fn parse_event_rejects_non_json() {
        let err = parse_event("hello").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("decoding event"), "got: {msg}");
    }

    #[tokio::test]
    async fn bind_or_skip_returns_some_on_clean_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gpam.sock");
        let bound = bind_or_skip(&path).unwrap();
        assert!(bound.is_some());
        // Cleanup drops as the binding goes out of scope.
    }

    #[tokio::test]
    async fn bind_or_skip_replaces_stale_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gpam.sock");
        // Stale file, not an actual listener.
        std::fs::write(&path, b"").unwrap();
        let bound = bind_or_skip(&path).unwrap();
        assert!(bound.is_some(), "expected stale file to be replaced");
    }

    #[tokio::test]
    async fn bind_or_skip_returns_none_when_other_listener_is_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gpam.sock");
        // Real listener that we keep alive for the duration of the test.
        let _live = UnixListener::bind(&path).unwrap();
        let bound = bind_or_skip(&path).unwrap();
        assert!(
            bound.is_none(),
            "should refuse to bind when another listener is up"
        );
    }

    #[tokio::test]
    async fn cleanup_removes_socket_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gpam.sock");
        {
            let (_listener, _cleanup) = bind_or_skip(&path).unwrap().unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "socket file should have been unlinked");
    }

    #[tokio::test]
    async fn listen_forwards_valid_event_and_drops_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gpam.sock");
        let (listener, _cleanup) = bind_or_skip(&path).unwrap().unwrap();
        let (tx, mut rx) = mpsc::channel::<ApprovalEvent>(8);
        tokio::spawn(listen(listener, tx));

        let event = ApprovalEvent {
            name: ORG_NAME.into(),
            source: Some("test".into()),
        };
        send(&path, &event).await.unwrap();

        // A second connection writes a garbage line plus a valid one — the
        // garbage is dropped, the valid one comes through.
        let mut bad = UnixStream::connect(&path).await.unwrap();
        bad.write_all(b"not-json\n").await.unwrap();
        bad.write_all(format!("{{\"name\":\"{}\"}}\n", PROJ_NAME).as_bytes())
            .await
            .unwrap();
        bad.shutdown().await.unwrap();

        let first = rx.recv().await.expect("first event");
        assert_eq!(first.name, ORG_NAME);
        let second = rx.recv().await.expect("second event");
        assert_eq!(second.name, PROJ_NAME);
    }
}
