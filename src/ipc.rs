use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

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
    let mut stream = UnixStream::connect(path).await.map_err(|e| match e.kind() {
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

/// Shape check for a fully-qualified PAM grant resource name:
/// `<scope-root>/<id>/locations/<loc>/entitlements/<ent>/grants/<id>` where
/// `<scope-root>` is `organizations`, `folders`, or `projects`.
pub fn is_valid_grant_name(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 8 {
        return false;
    }
    matches!(parts[0], "organizations" | "folders" | "projects")
        && !parts[1].is_empty()
        && parts[2] == "locations"
        && !parts[3].is_empty()
        && parts[4] == "entitlements"
        && !parts[5].is_empty()
        && parts[6] == "grants"
        && !parts[7].is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORG_NAME: &str = "organizations/000000000000/locations/global/entitlements/example-org-ent/grants/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const PROJ_NAME: &str = "projects/111122223333/locations/global/entitlements/example-proj-ent/grants/11111111-2222-3333-4444-555555555555";
    const FOLDER_NAME: &str = "folders/222233334444/locations/global/entitlements/example-folder-ent/grants/55555555-6666-7777-8888-999999999999";

    #[test]
    fn validator_accepts_org_folder_project() {
        assert!(is_valid_grant_name(ORG_NAME));
        assert!(is_valid_grant_name(FOLDER_NAME));
        assert!(is_valid_grant_name(PROJ_NAME));
    }

    #[test]
    fn validator_rejects_wrong_scope_root() {
        assert!(!is_valid_grant_name(
            "users/1/locations/global/entitlements/e/grants/g"
        ));
    }

    #[test]
    fn validator_rejects_missing_segments() {
        assert!(!is_valid_grant_name("organizations/1/locations/global"));
    }

    #[test]
    fn validator_rejects_empty_id() {
        assert!(!is_valid_grant_name(
            "organizations//locations/global/entitlements/e/grants/g"
        ));
    }

    #[test]
    fn validator_rejects_url() {
        assert!(!is_valid_grant_name(
            "https://console.cloud.google.com/iam-admin/pam/grants"
        ));
    }

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
}
