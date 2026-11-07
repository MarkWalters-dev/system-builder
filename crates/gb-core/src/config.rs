//! Server configuration: which systems map to which nspawn containers.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read {0}: {1}")]
    Read(PathBuf, #[source] std::io::Error),
    #[error("parse {0}: {1}")]
    Parse(PathBuf, #[source] toml::de::Error),
    #[error("config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Explicit systems. Optional: with discovery enabled, build VMs are
    /// found automatically. Entries here add to (or override by name) the
    /// discovered set — rarely needed.
    #[serde(default)]
    pub systems: Vec<SystemConfig>,
}

fn default_max_concurrent() -> usize {
    1
}

/// Automatic builder discovery. The default setup needs no `[[systems]]`
/// at all: build VMs are found via `machinectl list-images` by prefix.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// Discover build VMs automatically. Default true. (Also off in dry-run,
    /// which never touches the host.)
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Only images starting with this become build systems; the remainder of
    /// the image name is the system name (`gentoo-builder-laptop` -> `laptop`).
    #[serde(default = "default_image_prefix")]
    pub image_prefix: String,
    /// Machine root; a system's files live at `<root>/<image>`.
    #[serde(default = "default_machine_root")]
    pub machine_root: PathBuf,
    /// Also register the local host (named by hostname) as a build system
    /// that runs `emerge` directly instead of via nspawn. Default true.
    #[serde(default = "default_true")]
    pub include_host: bool,
}

fn default_true() -> bool {
    true
}

fn default_image_prefix() -> String {
    "gentoo-builder-".to_string()
}

fn default_machine_root() -> PathBuf {
    PathBuf::from("/var/lib/machines")
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            image_prefix: default_image_prefix(),
            machine_root: default_machine_root(),
            include_host: default_true(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Builds running at once across ALL systems (1 = strictly one at a
    /// time). A system never runs two builds at once regardless of this value.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_builds: usize,
    /// Keep at most this many finished jobs in memory (0 = unbounded).
    #[serde(default = "default_keep_finished")]
    pub keep_finished_jobs: usize,
    /// Keep at most this many log lines per job in memory.
    #[serde(default = "default_keep_log_lines")]
    pub keep_log_lines: usize,
    /// If true, run builds without touching real emerge/machines.
    /// Emits fake progress for development and tests.
    /// Also enabled per-process via env `SYSTEM_BUILDER_DRY_RUN=1`.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_keep_finished() -> usize {
    50
}

fn default_keep_log_lines() -> usize {
    20_000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            max_concurrent_builds: default_max_concurrent(),
            keep_finished_jobs: default_keep_finished(),
            keep_log_lines: default_keep_log_lines(),
            dry_run: false,
        }
    }
}

/// One buildable system: either a systemd-nspawn container or the host.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SystemConfig {
    /// Logical name clients send, e.g. `laptop`, `desktop`, `gateway`, `buildhost`.
    pub name: String,
    /// Container name for `systemd-run --machine=` / `machinectl`.
    /// `None` means: build directly on the host (e.g. `buildhost`).
    #[serde(default)]
    pub nspawn: Option<String>,
    /// Host-side path to the machine root. `/` for the host itself,
    /// `/var/lib/machines/<name>` for containers. Lets the server read
    /// `emerge.log` directly without entering the container.
    pub machine_path: PathBuf,
    /// Path of emerge.log *inside* the target root. Almost always
    /// `/var/log/emerge.log`.
    #[serde(default = "default_emerge_log")]
    pub emerge_log: PathBuf,
    /// Emerge binary inside the target.
    #[serde(default = "default_emerge_bin")]
    pub emerge_bin: PathBuf,
}

fn default_emerge_log() -> PathBuf {
    PathBuf::from("/var/log/emerge.log")
}

fn default_emerge_bin() -> PathBuf {
    PathBuf::from("/usr/bin/emerge")
}

impl SystemConfig {
    pub fn is_host(&self) -> bool {
        self.nspawn.is_none()
    }

    /// Host-visible path of this system's emerge.log.
    pub fn host_emerge_log_path(&self) -> PathBuf {
        join_root(&self.machine_path, &self.emerge_log)
    }
}

/// Join a target-absolute path onto a host-visible machine root.
fn join_root(root: &Path, target_abs: &Path) -> PathBuf {
    let rel = target_abs.strip_prefix("/").unwrap_or(target_abs);
    if root == Path::new("/") || root.as_os_str().is_empty() {
        PathBuf::from("/").join(rel)
    } else {
        root.join(rel)
    }
}

/// First-run template: every option present, all defaults commented out.
///
/// Values are rendered from the real defaults so the template cannot drift
/// from the code. Since everything is commented, a freshly scaffolded file
/// holds no active settings — the server asks the operator to add at least
/// one `[[systems]]` section and re-run.
pub fn template() -> String {
    let server = ServerConfig::default();
    let discovery = DiscoveryConfig::default();
    format!(
        r#"# gb-server configuration.
# Generated on first run with every option present and all defaults commented out.
# Uncomment and edit what you need, then (re)start gb-server.
#
# Lookup order without --config: ./config.toml, then this file
# ($HOME/.config/gb-server/config.toml), then /etc/system-builder/config.toml.

# [server]
# Network address to listen on.
# bind = "{bind}"
# Builds running at once across ALL systems (1 = strictly one at a time).
# A system never runs two builds at once regardless of this value.
# max_concurrent_builds = {max_concurrent}
# Finished jobs kept in memory (0 = unbounded).
# keep_finished_jobs = {keep_finished}
# Log lines kept in memory per job.
# keep_log_lines = {keep_log}
# Fake builds for development (also enabled via SYSTEM_BUILDER_DRY_RUN=1).
# dry_run = {dry_run}

# Discovery is on by default: build VMs are found via `machinectl list-images`
# by prefix, and the local host is registered too. No [[systems]] needed.
# [discovery]
# enabled = {discovery_enabled}
# Only images starting with this become systems (`gentoo-builder-laptop` -> `laptop`).
# image_prefix = "{image_prefix}"
# A system's files live at <machine_root>/<image>.
# machine_root = "{machine_root}"
# Also register the local host (by hostname) as a non-nspawn system.
# include_host = {include_host}

# Explicit systems are optional: they add to (or override by name) the
# discovered set. Only needed for custom paths or unusual setups.
# [[systems]]
# Logical name clients send, e.g. "laptop", "desktop", "gateway", "buildhost".
# name = "laptop"
# Container for systemd-run/machinectl. Omit for a host build (no container).
# nspawn = "gentoo-builder-laptop"
# Host-visible machine root: "/" for the host, /var/lib/machines/<name> for containers.
# machine_path = "/var/lib/machines/gentoo-builder-laptop"
# emerge.log path inside the target root.
# emerge_log = "/var/log/emerge.log"
# Emerge binary inside the target.
# emerge_bin = "/usr/bin/emerge"
"#,
        bind = server.bind,
        keep_finished = server.keep_finished_jobs,
        keep_log = server.keep_log_lines,
        dry_run = server.dry_run,
        max_concurrent = server.max_concurrent_builds,
        discovery_enabled = discovery.enabled,
        image_prefix = discovery.image_prefix,
        machine_root = discovery.machine_root.display(),
        include_host = discovery.include_host,
    )
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.to_path_buf(), e))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.max_concurrent_builds == 0 {
            return Err(ConfigError::Invalid(
                "max_concurrent_builds must be >= 1".to_string(),
            ));
        }
        if !self.discovery.enabled && self.systems.is_empty() {
            return Err(ConfigError::Invalid(
                "no [[systems]] configured and discovery is disabled".to_string(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for s in &self.systems {
            if s.name.trim().is_empty() {
                return Err(ConfigError::Invalid("system with empty name".to_string()));
            }
            if !seen.insert(s.name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate system '{}'",
                    s.name
                )));
            }
        }
        Ok(())
    }

    /// Look up an *explicitly configured* system by name. Discovered systems
    /// are resolved separately (see `crate::discover`).
    pub fn system(&self, name: &str) -> Option<&SystemConfig> {
        self.systems.iter().find(|s| s.name == name)
    }

    pub fn dry_run(&self) -> bool {
        self.server.dry_run
            || std::env::var("SYSTEM_BUILDER_DRY_RUN")
                .is_ok_and(|v| v == "1" || v.to_lowercase() == "true")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_paths_join_to_root() {
        let sys = SystemConfig {
            name: "buildhost".into(),
            nspawn: None,
            machine_path: "/".into(),
            emerge_log: "/var/log/emerge.log".into(),
            emerge_bin: "/usr/bin/emerge".into(),
        };
        assert_eq!(
            sys.host_emerge_log_path(),
            PathBuf::from("/var/log/emerge.log")
        );
        assert!(sys.is_host());
    }

    #[test]
    fn template_is_valid_toml_with_everything_commented() {
        let t = super::template();
        // Parses (to an empty table) and carries no active settings.
        let table: toml::Table = t.parse().expect("template is valid TOML");
        assert!(table.is_empty());
        for line in t.lines() {
            let line = line.trim();
            assert!(
                line.is_empty() || line.starts_with('#'),
                "uncommented line: {line}"
            );
        }
        // Every option is documented.
        for key in [
            "bind",
            "keep_finished_jobs",
            "keep_log_lines",
            "dry_run",
            "max_concurrent_builds",
            "[discovery]",
            "enabled",
            "image_prefix",
            "machine_root",
            "include_host",
            "[[systems]]",
            "name",
            "nspawn",
            "machine_path",
            "emerge_log",
            "emerge_bin",
        ] {
            assert!(t.contains(key), "template missing {key}");
        }
        // Rendered defaults match the real ones.
        let d = ServerConfig::default();
        assert!(t.contains(&format!("bind = {:?}", d.bind)));
        assert!(t.contains(&format!("keep_log_lines = {}", d.keep_log_lines)));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("bogus_key = 1\n").is_err());
        assert!(toml::from_str::<Config>("[server]\nbogus_key = 1\n").is_err());
    }

    #[test]
    fn empty_systems_ok_with_discovery_only() {
        assert!(Config::default().validate().is_ok());
        let mut cfg = Config::default();
        cfg.discovery.enabled = false;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn shipped_example_config_loads() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml");
        let cfg = Config::load(&path).expect("config.example.toml must parse and validate");
        // Discovery-first: no explicit systems, discovery on.
        assert!(cfg.systems.is_empty());
        assert!(cfg.discovery.enabled);
        assert_eq!(cfg.server.max_concurrent_builds, 1);
    }

    /// Simulate a user uncommenting every option in place: the result must be
    /// a valid config. Guards against TOML section-ordering traps (a scalar
    /// key placed after a `[table]` header would land in the wrong table).
    #[test]
    fn template_survives_uncomment_everything() {
        let uncommented: String = super::template()
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                let body = trimmed.strip_prefix("# ").unwrap_or(trimmed);
                let is_option = body.starts_with('[')
                    || body.split_once('=').is_some_and(|(k, _)| {
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
        let cfg: Config = toml::from_str(&uncommented).expect("uncommented template must parse");
        cfg.validate().expect("uncommented template must validate");
        assert_eq!(cfg.systems.len(), 1);
        assert_eq!(cfg.systems[0].name, "laptop");
        assert_eq!(cfg.server.bind, "127.0.0.1:8080");
    }

    #[test]
    fn container_paths_join_under_machine() {
        let sys = SystemConfig {
            name: "laptop".into(),
            nspawn: Some("gentoo-builder-laptop".into()),
            machine_path: "/var/lib/machines/gentoo-builder-laptop".into(),
            emerge_log: "/var/log/emerge.log".into(),
            emerge_bin: "/usr/bin/emerge".into(),
        };
        assert_eq!(
            sys.host_emerge_log_path(),
            PathBuf::from("/var/lib/machines/gentoo-builder-laptop/var/log/emerge.log")
        );
        assert!(!sys.is_host());
    }
}
