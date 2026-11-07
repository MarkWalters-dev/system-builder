//! Client config file: `$HOME/.config/gb-client/config.toml`.
//!
//! Every key is optional except `server`, which must come from the flag,
//! the environment, or this file (it has no built-in default). Effective
//! values resolve as flag > environment > config file > built-in default.
//!
//! ```toml
//! server = "http://builder-host:8080"
//! timeout = 300
//! system = "laptop"
//! notify_topic = "my-topic"
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Example server URL rendered into the generated template. Unlike the
/// others this is not a live fallback: a missing server is a usage error.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8080";
pub const DEFAULT_TIMEOUT: u64 = 300;
pub const DEFAULT_NOTIFY_SERVER: &str = "https://ntfy.sh";

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Default `--system` for build/estimate/history.
    #[serde(default)]
    pub system: Option<String>,
    /// Ntfy topic for completion/failure pings. Unset (or blank) disables
    /// notifications.
    #[serde(default)]
    pub notify_topic: Option<String>,
    /// Ntfy server base URL (only needed for self-hosted ntfy).
    #[serde(default)]
    pub notify_server: Option<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read {0}: {1}")]
    Read(PathBuf, #[source] std::io::Error),
    #[error("parse {0}: {1}")]
    Parse(PathBuf, #[source] toml::de::Error),
    #[error("create {0}: {1}")]
    Create(PathBuf, #[source] std::io::Error),
}

/// Default location: `$XDG_CONFIG_HOME/gb-client/config.toml`, else
/// `$HOME/.config/gb-client/config.toml`. `None` when no home is known.
pub fn default_path() -> Option<PathBuf> {
    gb_types::config_path::user_config_path("gb-client")
}

/// First-run template: every option present, all defaults commented out.
/// Values are rendered from the real defaults so the template cannot drift.
pub fn template() -> String {
    format!(
        r#"# gb-client configuration.
# Generated on first run with every option present and all defaults commented out.
# Uncomment and edit what you need.
#
# Values resolve as: flag > environment > this file > built-in default.

# API base URL (required; or --server / GB_SERVER).
# server = "{server}"
# Timeout in seconds for single API calls; streaming is unbounded (or --timeout / GB_TIMEOUT).
# timeout = {timeout}
# Default system for build/estimate/history, e.g. "laptop" (or --system / GB_SYSTEM).
# No default; shown here as an example.
# system = "laptop"
# Ntfy topic to ping when the local emerge finishes or the remote build fails.
# Unset disables notifications; shown here as an example.
# notify_topic = "my-topic"
# Ntfy server base URL (only needed for self-hosted ntfy).
# notify_server = "{notify_server}"
"#,
        server = DEFAULT_SERVER,
        timeout = DEFAULT_TIMEOUT,
        notify_server = DEFAULT_NOTIFY_SERVER,
    )
}

/// Load `path`, scaffolding a commented template when nothing exists there
/// yet. Returns the config plus whether the file was just created.
pub fn load_or_scaffold(path: &Path) -> Result<(FileConfig, bool), ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map(|cfg| (cfg, false))
            .map_err(|e| ConfigError::Parse(path.to_path_buf(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let created = gb_types::config_path::scaffold_file(path, &template())
                .map_err(|e| ConfigError::Create(path.to_path_buf(), e))?;
            if created {
                Ok((FileConfig::default(), true))
            } else {
                // Appeared between the read and the scaffold; load it for real.
                load_or_scaffold(path)
            }
        }
        Err(e) => Err(ConfigError::Read(path.to_path_buf(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let cfg: FileConfig = toml::from_str(
            "server = \"http://b:8080\"\ntimeout = 60\nsystem = \"laptop\"\nnotify_topic = \"t\"\nnotify_server = \"https://n.example\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg,
            FileConfig {
                server: Some("http://b:8080".into()),
                timeout: Some(60),
                system: Some("laptop".into()),
                notify_topic: Some("t".into()),
                notify_server: Some("https://n.example".into()),
            }
        );
    }

    #[test]
    fn partial_and_empty_configs_default_the_rest() {
        let cfg: FileConfig = toml::from_str("system = \"gateway\"\n").unwrap();
        assert_eq!(cfg.system.as_deref(), Some("gateway"));
        assert_eq!(cfg.server, None);
        let empty: FileConfig = toml::from_str("").unwrap();
        assert_eq!(empty, FileConfig::default());
    }

    #[test]
    fn rejects_bad_types() {
        assert!(toml::from_str::<FileConfig>("timeout = \"soon\"\n").is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<FileConfig>("servre = \"x\"\n").is_err());
    }

    #[test]
    fn template_is_valid_toml_with_everything_commented() {
        let t = super::template();
        let table: toml::Table = t.parse().expect("template is valid TOML");
        assert!(table.is_empty());
        for line in t.lines() {
            let line = line.trim();
            assert!(
                line.is_empty() || line.starts_with('#'),
                "uncommented line: {line}"
            );
        }
        for key in [
            "server",
            "timeout",
            "system",
            "notify_topic",
            "notify_server",
        ] {
            assert!(t.contains(key), "template missing {key}");
        }
        assert!(t.contains(&format!("server = {DEFAULT_SERVER:?}")));
        assert!(t.contains(&format!("timeout = {DEFAULT_TIMEOUT}")));
        assert!(t.contains(&format!("notify_server = {DEFAULT_NOTIFY_SERVER:?}")));
    }

    #[test]
    fn template_survives_uncomment_everything() {
        let uncommented: String = super::template()
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                let body = trimmed.strip_prefix("# ").unwrap_or(trimmed);
                let is_option = body.split_once('=').is_some_and(|(k, _)| {
                    let k = k.trim();
                    !k.is_empty()
                        && k.chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                });
                if trimmed.starts_with("# ") && is_option {
                    body.to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let cfg: FileConfig =
            toml::from_str(&uncommented).expect("uncommented template must parse");
        assert_eq!(cfg.server.as_deref(), Some(super::DEFAULT_SERVER));
        assert_eq!(cfg.timeout, Some(super::DEFAULT_TIMEOUT));
        assert_eq!(cfg.system.as_deref(), Some("laptop"));
        assert_eq!(cfg.notify_topic.as_deref(), Some("my-topic"));
        assert_eq!(
            cfg.notify_server.as_deref(),
            Some(super::DEFAULT_NOTIFY_SERVER)
        );
    }

    #[test]
    fn load_scaffolds_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gb-client/config.toml");
        let (cfg, created) = super::load_or_scaffold(&path).unwrap();
        assert!(created);
        assert_eq!(cfg, FileConfig::default());
        assert!(path.exists());
        let (cfg2, created2) = super::load_or_scaffold(&path).unwrap();
        assert!(!created2);
        assert_eq!(cfg2, FileConfig::default());
    }
}
