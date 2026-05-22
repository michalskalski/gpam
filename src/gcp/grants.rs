use anyhow::{Context, Result};
use google_cloud_privilegedaccessmanager_v1::client::PrivilegedAccessManager;
use google_cloud_privilegedaccessmanager_v1::model::{Grant, Justification};
use google_cloud_wkt::Duration;

/// Submit a grant request. Returns the created grant's resource name on success.
pub async fn create(
    client: &PrivilegedAccessManager,
    entitlement_name: &str,
    duration_secs: i64,
    justification: Option<&str>,
) -> Result<String> {
    let duration = Duration::new(duration_secs, 0).context("invalid duration")?;

    let mut grant = Grant::new().set_requested_duration(duration);
    if let Some(text) = justification.filter(|t| !t.is_empty()) {
        grant = grant.set_justification(Justification::new().set_unstructured_justification(text));
    }

    let resp = client
        .create_grant()
        .set_parent(entitlement_name)
        .set_grant(grant)
        .send()
        .await
        .context("create_grant")?;

    Ok(resp.name)
}

/// Fetch the current state of a grant.
pub async fn get(client: &PrivilegedAccessManager, grant_name: &str) -> Result<Grant> {
    client
        .get_grant()
        .set_name(grant_name)
        .send()
        .await
        .context("get_grant")
}

/// Terminal states from the PAM proto.
pub fn is_terminal(state: &str) -> bool {
    matches!(
        state,
        "Denied"
            | "ActivationFailed"
            | "Expired"
            | "Revoked"
            | "Ended"
            | "Withdrawn"
            | "DENIED"
            | "ACTIVATION_FAILED"
            | "EXPIRED"
            | "REVOKED"
            | "ENDED"
            | "WITHDRAWN"
    )
}

/// Parse a duration like `1h`, `30m`, `5400s`, or bare integer seconds.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, suffix) = match s.chars().last()? {
        'h' | 'H' => (&s[..s.len() - 1], 3600i64),
        'm' | 'M' => (&s[..s.len() - 1], 60),
        's' | 'S' => (&s[..s.len() - 1], 1),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: i64 = num.parse().ok()?;
    if n <= 0 {
        return None;
    }
    n.checked_mul(suffix)
}

pub const MIN_DURATION_SECS: i64 = 30 * 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duration_formats() {
        assert_eq!(parse_duration("1h"), Some(3600));
        assert_eq!(parse_duration("30m"), Some(1800));
        assert_eq!(parse_duration("5400s"), Some(5400));
        assert_eq!(parse_duration("3600"), Some(3600));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("0h"), None);
    }

    #[test]
    fn is_terminal_state() {
        assert!(is_terminal("Expired"));
        assert!(is_terminal("ACTIVATION_FAILED"));
        assert!(!is_terminal("Active"));
        assert!(!is_terminal("ApprovalAwaited"));
    }
}
