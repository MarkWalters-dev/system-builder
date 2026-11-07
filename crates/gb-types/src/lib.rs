//! Shared API types for the Gentoo system-builder.
//!
//! These types are the JSON contract between clients and `gb-server`.
//! They are kept in their own crate so a future client CLI can depend on
//! them without pulling in the build engine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A build target: a single package atom (`sys-apps/foo`), a versioned atom,
/// or a package set such as `@system` or `@world`.
pub type Target = String;

/// Request body for `POST /v1/builds`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBuildRequest {
    /// Logical system name from the server config (e.g. `laptop`, `desktop`, `buildhost`).
    pub system: String,
    /// What to build: package atoms and/or sets like `@system`.
    pub targets: Vec<Target>,
    /// Extra args appended to the `emerge` invocation.
    /// Defaults to `--update --newuse --deep --verbose` (up-to-date
    /// packages are skipped, not reinstalled). Keep empty vec to use
    /// the default; any explicit args replace it entirely.
    #[serde(default)]
    pub emerge_args: Vec<String>,
    /// Automatically unmerge packages blocking the merge (bounded resolve
    /// attempts; everything unmerged is recorded on the job). Defaults to
    /// true when omitted.
    #[serde(default)]
    pub auto_fix_conflicts: Option<bool>,
}

/// Lifecycle states of a build job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Resolving,
    Estimating,
    Building,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Resolving | Self::Estimating | Self::Building)
    }
}

/// Live progress of a running build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildProgress {
    /// Package currently being emerged, e.g. `sys-apps/iproute2-7.3.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_package: Option<String>,
    /// 1-based index of the current package (`x` in "package x of y").
    #[serde(default)]
    pub current_index: usize,
    /// Total packages planned (`y` in "package x of y"). 0 until resolved.
    #[serde(default)]
    pub total_packages: usize,
    /// Seconds elapsed since the build phase started.
    #[serde(default)]
    pub elapsed_secs: u64,
    /// Estimated seconds remaining for the whole build, if estimable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_remaining_secs: Option<u64>,
    /// Sum of per-package estimates for the whole plan, if estimable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_total_secs: Option<u64>,
}

/// How one package's build-time estimate was derived.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageEstimate {
    pub package: String,
    /// Median historical build time in seconds, if any history exists.
    pub estimated_secs: Option<u64>,
    /// Mean historical build time in seconds, if any history exists.
    pub average_secs: Option<u64>,
    /// Number of historical samples behind the estimate.
    pub samples: usize,
    /// Which system's log the estimate came from. Equals the requested
    /// system when local history exists, otherwise the fallback system.
    pub source_system: Option<String>,
    /// True when no history existed on the target system and another
    /// system's logs were used instead.
    pub fallback: bool,
}

/// Full build job as returned by `GET /v1/builds/:id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildJob {
    pub id: Uuid,
    pub system: String,
    pub targets: Vec<Target>,
    pub emerge_args: Vec<String>,
    /// Whether blockers were auto-unmerged during resolving.
    #[serde(default)]
    pub auto_fix_conflicts: bool,
    /// Blocking packages the server unmerged to clear the merge, oldest first.
    #[serde(default)]
    pub unmerged_blockers: Vec<String>,
    /// Set for provision jobs (see [`ProvisionPlan`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<ProvisionPlan>,
    pub state: JobState,
    /// Position in the queue (1-based) while queued, else 0.
    #[serde(default)]
    pub queue_position: usize,
    #[serde(default)]
    pub progress: BuildProgress,
    /// Ordered plan from `emerge -p`, once resolved.
    #[serde(default)]
    pub plan: Vec<String>,
    /// Per-package estimates, once estimated.
    #[serde(default)]
    pub estimates: Vec<PackageEstimate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// Short form used in list responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSummary {
    pub id: Uuid,
    pub system: String,
    pub targets: Vec<Target>,
    pub state: JobState,
    pub queue_position: usize,
    pub progress: BuildProgress,
    pub created_at: DateTime<Utc>,
}

impl From<&BuildJob> for BuildSummary {
    fn from(j: &BuildJob) -> Self {
        Self {
            id: j.id,
            system: j.system.clone(),
            targets: j.targets.clone(),
            state: j.state,
            queue_position: j.queue_position,
            progress: j.progress.clone(),
            created_at: j.created_at,
        }
    }
}

/// `GET /v1/queue` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueStatus {
    pub max_concurrent_builds: usize,
    pub running: usize,
    pub queued: Vec<BuildSummary>,
    pub active: Vec<BuildSummary>,
}

/// How a system is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemMode {
    /// Built on the host directly (no container), e.g. `buildhost`.
    Host,
    /// Built inside a systemd-nspawn container.
    Nspawn,
}

/// `GET /v1/systems` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub name: String,
    pub mode: SystemMode,
    /// Container name for nspawn systems, `None` for the host.
    pub nspawn: Option<String>,
    pub status: SystemStatus,
    pub active_build_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemStatus {
    Idle,
    Building,
}

/// `GET /v1/systems/:system/active` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemActiveBuild {
    pub system: String,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildJob>,
}

/// Client machine specs for provisioning, gathered by `gb-client` from
/// `emerge --info` (plus `cpuid2cpuflags` and profile detection). The
/// server renders these into the new image's `make.conf`, so the image
/// is compiled from source with the client's own settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSpecs {
    /// Target triplet, e.g. `x86_64-pc-linux-gnu`. Must match the
    /// server's stage3 (amd64 only for now).
    pub chost: String,
    /// Full CFLAGS, reused verbatim as the image's `COMMON_FLAGS`.
    pub cflags: String,
    /// `CPU_FLAGS_X86` entries, e.g. `["aes", "avx2", "sse4_2"]`.
    #[serde(default)]
    pub cpu_flags_x86: Vec<String>,
    /// Global `USE` flags, e.g. `["ssl", "-X"]`.
    #[serde(default)]
    pub use_flags: Vec<String>,
    /// Gentoo profile path relative to `profiles/`, e.g.
    /// `default/linux/amd64/23.0/desktop`.
    pub profile: String,
}

/// Request body for `POST /v1/systems`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionRequest {
    /// System name to create (becomes the `<prefix><name>` image).
    /// Provisioning is idempotent: when the system already exists the
    /// response carries it with no job instead of failing.
    pub name: String,
    /// Client settings the image is compiled with.
    pub specs: ClientSpecs,
}

/// `POST /v1/systems` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionResponse {
    pub system: SystemInfo,
    /// The recompile job, or `None` when the system already existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<BuildJob>,
}

/// Marks a [`BuildJob`] as a provision recompile: before resolving,
/// the runner creates the system image from scratch (stage3 + client
/// `make.conf`), then the job rebuilds `@world` inside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionPlan {
    pub specs: ClientSpecs,
}

/// Request body for `POST /v1/estimates`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimateRequest {
    pub system: String,
    /// Explicit `category/packagename` (or `category/packagename-version`)
    /// entries to estimate directly from history.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Emerge targets (`@system`, atoms, ...) to resolve via `emerge -p`
    /// inside the target before estimating. Slower; requires the machine.
    #[serde(default)]
    pub targets: Vec<Target>,
}

/// `POST /v1/estimates` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimateResponse {
    pub system: String,
    pub packages: Vec<PackageEstimate>,
    /// Resolved plan when `targets` were given.
    #[serde(default)]
    pub plan: Vec<String>,
    pub total_estimated_secs: Option<u64>,
    pub packages_with_history: usize,
    pub packages_without_history: usize,
}

/// `GET /v1/history?system=&package=` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub system: String,
    pub package: String,
    pub samples: usize,
    pub average_secs: Option<u64>,
    pub median_secs: Option<u64>,
    pub min_secs: Option<u64>,
    pub max_secs: Option<u64>,
    /// System whose logs supplied the data (fallback when requested
    /// system has no history for this package).
    pub source_system: Option<String>,
    pub fallback: bool,
}

/// `GET /v1/builds/:id/log` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildLog {
    pub id: Uuid,
    pub state: JobState,
    /// Full buffered log (may be truncated server-side for huge builds).
    pub log: String,
    pub truncated: bool,
}

/// Server-sent event payloads on `GET /v1/builds/:id/stream`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Status {
        state: JobState,
    },
    Log {
        line: String,
    },
    Progress {
        progress: BuildProgress,
    },
    Done {
        state: JobState,
        exit_code: Option<i32>,
    },
}

/// Generic error body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

/// Per-user config file locations, shared by both apps.
///
/// Each app keeps its own file so the two can never be confused on a host
/// that runs both:
///
/// * server: `$HOME/.config/gb-server/config.toml`
/// * client: `$HOME/.config/gb-client/config.toml`
///
/// `$XDG_CONFIG_HOME` is honored when set (replacing `$HOME/.config`).
pub mod config_path {
    use std::path::{Path, PathBuf};

    /// Resolve `$XDG_CONFIG_HOME/<app>/config.toml` (or
    /// `$HOME/.config/<app>/config.toml`). `None` when neither env var
    /// yields a directory.
    pub fn user_config_path(app: &str) -> Option<PathBuf> {
        let xdg = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty());
        let home = std::env::var("HOME").ok().filter(|s| !s.is_empty());
        user_config_path_with(app, xdg.as_deref(), home.as_deref())
    }

    fn user_config_path_with(
        app: &str,
        xdg_config_home: Option<&str>,
        home: Option<&str>,
    ) -> Option<PathBuf> {
        if let Some(dir) = xdg_config_home {
            return Some(PathBuf::from(dir).join(app).join("config.toml"));
        }
        home.map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join(app)
                .join("config.toml")
        })
    }

    /// First-run scaffolding: create `path` (plus parent dirs) holding
    /// `template`, but only when nothing exists there yet.
    ///
    /// Returns `true` when the file was created, `false` when it already
    /// existed (existing files are never touched).
    pub fn scaffold_file(path: &Path, template: &str) -> std::io::Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, template)?;
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn prefers_xdg_config_home() {
            assert_eq!(
                user_config_path_with("gb-server", Some("/xdg"), Some("/home/u")),
                Some(PathBuf::from("/xdg/gb-server/config.toml"))
            );
        }

        #[test]
        fn falls_back_to_home_dot_config() {
            assert_eq!(
                user_config_path_with("gb-client", None, Some("/home/u")),
                Some(PathBuf::from("/home/u/.config/gb-client/config.toml"))
            );
        }

        #[test]
        fn none_without_any_home() {
            assert_eq!(user_config_path_with("gb-client", None, None), None);
        }

        #[test]
        fn scaffold_creates_but_never_overwrites() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("sub/dir/config.toml");
            assert!(scaffold_file(&path, "template\n").unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "template\n");
            // Second call leaves the edited file alone.
            std::fs::write(&path, "edited\n").unwrap();
            assert!(!scaffold_file(&path, "template\n").unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited\n");
        }
    }
}
