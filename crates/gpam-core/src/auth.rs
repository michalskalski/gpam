use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use google_cloud_auth::credentials::{Builder as CredentialsBuilder, Credentials};

#[derive(Debug, Clone)]
pub struct Session {
    pub account: String,
    pub credentials: Credentials,
}

pub async fn resolve() -> Result<Session> {
    let credentials = CredentialsBuilder::default().build().map_err(|e| {
        anyhow!(
            "could not load Application Default Credentials: {e}\n\
             hint: run `gcloud auth application-default login`"
        )
    })?;

    // TODO: before falling back to "default", try the OAuth2 userinfo endpoint
    // (GET https://www.googleapis.com/oauth2/v3/userinfo with the ADC token).
    // That covers users without gcloud installed (service-account keys, WIF)
    // and keeps the per-account cache filename meaningful for them.
    let account = active_account().unwrap_or_else(|_| "default".to_string());

    Ok(Session {
        account,
        credentials,
    })
}

/// Read the active account name from gcloud's own config files. The format is
/// stable: `~/.config/gcloud/active_config` names the active configuration,
/// and `~/.config/gcloud/configurations/config_<name>` is an INI file with
/// `account = <email>` under the `[core]` section.
fn active_account() -> Result<String> {
    let gcloud_dir = gcloud_config_dir()?;

    let active = std::fs::read_to_string(gcloud_dir.join("active_config"))
        .context("reading active_config")?;
    let active = active.trim();
    if active.is_empty() {
        return Err(anyhow!("active_config is empty"));
    }

    let config_path = gcloud_dir
        .join("configurations")
        .join(format!("config_{active}"));
    let body = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;

    parse_ini_value(&body, "core", "account")
        .ok_or_else(|| anyhow!("no core.account in {}", config_path.display()))
}

fn gcloud_config_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("CLOUDSDK_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".config").join("gcloud"))
}

fn parse_ini_value(body: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_section = name.trim().eq_ignore_ascii_case(section);
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ini_account() {
        let body = "[core]\naccount = me@example.com\nproject = foo\n";
        assert_eq!(
            parse_ini_value(body, "core", "account").as_deref(),
            Some("me@example.com")
        );
    }

    #[test]
    fn ignores_other_sections() {
        let body = "[other]\naccount = wrong\n[core]\naccount = right\n";
        assert_eq!(
            parse_ini_value(body, "core", "account").as_deref(),
            Some("right")
        );
    }

    #[test]
    fn returns_none_when_missing() {
        let body = "[core]\nproject = foo\n";
        assert!(parse_ini_value(body, "core", "account").is_none());
    }
}
