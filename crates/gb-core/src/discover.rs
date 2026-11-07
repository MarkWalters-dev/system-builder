//! Automatic builder discovery: no per-system config needed.
//!
//! Build VMs are found via `machinectl list-images`: images starting with
//! the configured prefix (`gentoo-builder-`) become systems named by the
//! remainder (`gentoo-builder-laptop` -> `laptop`), with constant paths
//! (`/var/lib/machines/<image>`) and default emerge log/binary. The local
//! host (named by hostname) is registered as a non-nspawn system too.
//!
//! Explicit `[[systems]]` entries overlay the discovered set by name, so a
//! custom path or binary can still be pinned when needed.

use std::path::PathBuf;
use std::process::Stdio;

use crate::config::{DiscoveryConfig, SystemConfig};

/// Parse `machinectl list-images` output into image names (first column).
/// Tolerates the header/legend lines with or without `--no-legend`.
pub fn parse_list_images(output: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in output.lines() {
        let first = line.split_whitespace().next().unwrap_or_default();
        if first.is_empty() || first == "NAME" {
            continue;
        }
        // Footer looks like "16 images listed." — first field is a number.
        if first.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        names.push(first.to_string());
    }
    names
}

/// System name for an image (`gentoo-builder-laptop` -> `Some("laptop")`),
/// or `None` when the image doesn't match the prefix (or has no suffix).
pub fn system_name_for_image(image: &str, prefix: &str) -> Option<String> {
    let suffix = image.strip_prefix(prefix)?;
    if suffix.is_empty() || suffix.contains('/') {
        return None;
    }
    Some(suffix.to_string())
}

/// Build the [`SystemConfig`] for a discovered image. Paths are constant.
pub fn system_for_image(image: &str, system: &str, machine_root: &std::path::Path) -> SystemConfig {
    SystemConfig {
        name: system.to_string(),
        nspawn: Some(image.to_string()),
        machine_path: machine_root.join(image),
        emerge_log: PathBuf::from("/var/log/emerge.log"),
        emerge_bin: PathBuf::from("/usr/bin/emerge"),
    }
}

/// Merge explicit systems over discovered ones. Order: discovered (machinectl
/// order), then explicit-only extras appended; same-name explicit entries
/// replace in place.
pub fn merge(explicit: &[SystemConfig], discovered: Vec<SystemConfig>) -> Vec<SystemConfig> {
    let mut out = discovered;
    for sys in explicit {
        if let Some(pos) = out.iter().position(|s| s.name == sys.name) {
            out[pos] = sys.clone();
        } else {
            out.push(sys.clone());
        }
    }
    out
}

/// Run discovery: list images, filter by prefix, add the host.
/// Never fails hard — problems are logged and yield fewer systems.
pub async fn discover(cfg: &DiscoveryConfig) -> Vec<SystemConfig> {
    let mut systems = Vec::new();
    if cfg.enabled {
        match list_images().await {
            Ok(images) => {
                for image in &images {
                    if let Some(name) = system_name_for_image(image, &cfg.image_prefix) {
                        tracing::debug!(image, name, "discovered builder");
                        systems.push(system_for_image(image, &name, &cfg.machine_root));
                    }
                }
            }
            Err(e) => {
                tracing::warn!("machinectl list-images failed ({e}); no VMs discovered");
            }
        }
        if cfg.include_host {
            match local_hostname().await {
                Some(host) => {
                    tracing::debug!(host, "registering local host");
                    systems.push(SystemConfig {
                        name: host,
                        nspawn: None,
                        machine_path: PathBuf::from("/"),
                        emerge_log: PathBuf::from("/var/log/emerge.log"),
                        emerge_bin: PathBuf::from("/usr/bin/emerge"),
                    });
                }
                None => tracing::warn!("cannot determine hostname; host build unavailable"),
            }
        }
    }
    systems
}

async fn list_images() -> Result<Vec<String>, String> {
    let run = tokio::process::Command::new("machinectl")
        .args(["list-images", "--full", "--no-legend", "--no-pager"])
        .stdin(Stdio::null())
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .map_err(|_| "machinectl list-images timed out".to_string())?
        .map_err(|e| format!("spawn machinectl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_list_images(&String::from_utf8_lossy(&out.stdout)))
}

async fn local_hostname() -> Option<String> {
    let run = tokio::process::Command::new("hostname")
        .stdin(Stdio::null())
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        return None;
    }
    // `hostname` may return an FQDN; the short name matches server convention.
    Some(name.split('.').next().unwrap_or(&name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_IMAGES: &str = "\
NAME                TYPE      RO USAGE CREATED                     MODIFIED
alpine              directory no       Fri 2026-10-02 03:40:17 MDT -
gentoo              directory no       Fri 2026-10-02 03:40:17 MDT -
gentoo-builder-desktop directory no       Sun 2026-08-30 03:17:08 MDT -
gentoo-builder-laptop  directory no       Thu 2026-08-20 23:03:49 MDT -

16 images listed.
";

    #[test]
    fn parses_names_skipping_header_and_legend() {
        assert_eq!(
            parse_list_images(LIST_IMAGES),
            vec![
                "alpine",
                "gentoo",
                "gentoo-builder-desktop",
                "gentoo-builder-laptop"
            ]
        );
        assert!(parse_list_images("").is_empty());
        assert!(parse_list_images("0 images listed.\n").is_empty());
    }

    #[test]
    fn prefix_filtering() {
        let prefix = "gentoo-builder-";
        assert_eq!(
            system_name_for_image("gentoo-builder-laptop", prefix).as_deref(),
            Some("laptop")
        );
        assert_eq!(system_name_for_image("gentoo", prefix), None);
        assert_eq!(system_name_for_image("gentoo-builder-", prefix), None);
        assert_eq!(system_name_for_image("arch", prefix), None);
    }

    #[test]
    fn discovered_system_has_constant_paths() {
        let sys = system_for_image(
            "gentoo-builder-laptop",
            "laptop",
            std::path::Path::new("/var/lib/machines"),
        );
        assert_eq!(sys.name, "laptop");
        assert_eq!(sys.nspawn.as_deref(), Some("gentoo-builder-laptop"));
        assert_eq!(
            sys.machine_path,
            PathBuf::from("/var/lib/machines/gentoo-builder-laptop")
        );
        assert_eq!(sys.emerge_log, PathBuf::from("/var/log/emerge.log"));
        assert_eq!(sys.emerge_bin, PathBuf::from("/usr/bin/emerge"));
    }

    fn explicit(name: &str) -> SystemConfig {
        SystemConfig {
            name: name.to_string(),
            nspawn: None,
            machine_path: PathBuf::from("/custom"),
            emerge_log: PathBuf::from("/var/log/emerge.log"),
            emerge_bin: PathBuf::from("/usr/bin/emerge"),
        }
    }

    #[test]
    fn merge_prefers_explicit_on_name_clash() {
        let discovered = vec![
            system_for_image("gentoo-builder-a", "a", std::path::Path::new("/m")),
            system_for_image("gentoo-builder-b", "b", std::path::Path::new("/m")),
        ];
        let merged = merge(&[explicit("b"), explicit("z")], discovered);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].name, "a");
        // "b" replaced in place by the explicit entry.
        assert_eq!(merged[1].name, "b");
        assert_eq!(merged[1].machine_path, PathBuf::from("/custom"));
        // Explicit-only "z" appended.
        assert_eq!(merged[2].name, "z");
    }
}
