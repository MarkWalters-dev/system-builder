//! Build-time history: our own genlop/qlop.
//!
//! We parse `emerge.log` files **directly from the host** (containers are
//! visible under `/var/lib/machines/<name>/...`), so no in-container agent
//! or `emlop` call is needed for estimates.
//!
//! Log lines we care about:
//! ```text
//! <ts>:  >>> emerge (N of M) cat/pkg-ver to /
//! <ts>:  ::: completed emerge (N of M) cat/pkg-ver to /
//! ```
//! Duration = completed_ts − start_ts for the matching package. Samples are
//! keyed by `category/packagename` (version stripped).

use gb_types::PackageEstimate;
use std::collections::HashMap;
use std::path::Path;

use crate::config::SystemConfig;

/// Strip `::repo` suffix and `-version[-rN]` from an atom.
///
/// `sys-apps/iproute2-7.3.0` → `sys-apps/iproute2`
/// `gui-apps/mako-1.11.0::gentoo` → `gui-apps/mako`
pub fn strip_version(atom: &str) -> String {
    let no_repo = atom.split("::").next().unwrap_or(atom);
    let (category, pkg_ver) = match no_repo.split_once('/') {
        Some((c, p)) => (Some(c), p),
        None => (None, no_repo),
    };
    // Version starts at the first '-' followed by a digit.
    let mut cut = None;
    let bytes = pkg_ver.as_bytes();
    for (i, w) in bytes.windows(2).enumerate() {
        if w[0] == b'-' && w[1].is_ascii_digit() {
            cut = Some(i);
            break;
        }
    }
    let pn = match cut {
        Some(i) => &pkg_ver[..i],
        None => pkg_ver,
    };
    match category {
        Some(c) => format!("{c}/{pn}"),
        None => pn.to_string(),
    }
}

/// Aggregate stats for one package on one system.
#[derive(Debug, Clone)]
pub struct PackageStats {
    pub samples: usize,
    pub mean_secs: u64,
    pub median_secs: u64,
    pub min_secs: u64,
    pub max_secs: u64,
}

fn stats_of(samples: &[u64]) -> Option<PackageStats> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len() as u64;
    let sum: u64 = sorted.iter().sum();
    let mid = sorted.len() / 2;
    let median = if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2
    };
    Some(PackageStats {
        samples: sorted.len(),
        mean_secs: sum / n,
        median_secs: median,
        min_secs: sorted[0],
        max_secs: sorted[sorted.len() - 1],
    })
}

/// History for a single system.
#[derive(Debug, Clone, Default)]
pub struct SystemHistory {
    pub system: String,
    samples: HashMap<String, Vec<u64>>,
}

impl SystemHistory {
    pub fn stats(&self, package_pn: &str) -> Option<PackageStats> {
        self.samples.get(package_pn).and_then(|s| stats_of(s))
    }

    pub fn packages(&self) -> usize {
        self.samples.len()
    }
}

/// Parse one emerge.log's text into `cat/pn -> [durations]`.
pub fn parse_emerge_log(text: &str) -> HashMap<String, Vec<u64>> {
    // Pending start timestamps keyed by full versioned atom.
    let mut pending: HashMap<String, Vec<u64>> = HashMap::new();
    let mut out: HashMap<String, Vec<u64>> = HashMap::new();

    for line in text.lines() {
        // "<ts>:  <rest>"
        let Some((ts_part, rest)) = line.split_once(':') else {
            continue;
        };
        let Ok(ts) = ts_part.trim().parse::<u64>() else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(atom) = rest.strip_prefix(">>> emerge (") {
            // ">>> emerge (N of M) <atom> to /"
            if let Some(atom) = atom_after_counter(atom) {
                pending.entry(atom.to_string()).or_default().push(ts);
            }
        } else if let Some(rest) = rest.strip_prefix("::: completed emerge (")
            && let Some(atom) = atom_after_counter(rest)
            && let Some(starts) = pending.get_mut(atom)
            && let Some(start) = starts.pop()
        {
            // Guard clock skew / corrupt lines; ignore absurd spans.
            if ts >= start && ts - start < 30 * 24 * 3600 {
                out.entry(strip_version(atom)).or_default().push(ts - start);
            }
        }
    }
    out
}

/// Given the text after `>>> emerge (` or `::: completed emerge (`
/// (i.e. starting with `N of M) <atom> to /`), extract `<atom>`.
fn atom_after_counter(s: &str) -> Option<&str> {
    let close = s.find(')')?;
    let after = s[close + 1..].trim_start();
    let atom = after.split_whitespace().next()?;
    if atom.is_empty() || atom == "to" {
        return None;
    }
    Some(atom)
}

/// All systems' histories with fallback-aware estimation.
#[derive(Debug, Clone, Default)]
pub struct HistoryStore {
    systems: HashMap<String, SystemHistory>,
    /// Config order, for deterministic fallback ties.
    order: Vec<String>,
}

impl HistoryStore {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load every given system's log from its host-visible path.
    /// Missing/unreadable logs are skipped (that system just has no history).
    /// Called with the freshly resolved effective systems, so newly
    /// discovered VMs are picked up without a restart.
    pub fn refresh_all(&mut self, systems: &[SystemConfig]) {
        self.order = systems.iter().map(|s| s.name.clone()).collect();
        for sys in systems {
            let path = sys.host_emerge_log_path();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let samples = if text.is_empty() {
                HashMap::new()
            } else {
                parse_emerge_log(&text)
            };
            self.systems.insert(
                sys.name.clone(),
                SystemHistory {
                    system: sys.name.clone(),
                    samples,
                },
            );
        }
    }

    pub fn stats(&self, system: &str, package: &str) -> Option<PackageStats> {
        let pn = strip_version(package.trim());
        self.systems.get(system)?.stats(&pn)
    }

    /// Estimate one package for `system`.
    ///
    /// Uses `system`'s own logs first. If the package was never built there,
    /// falls back to the other system with the most samples for it.
    pub fn estimate(&self, system: &str, package: &str) -> PackageEstimate {
        let package = package.trim().to_string();
        let pn = strip_version(&package);

        if let Some(hist) = self.systems.get(system)
            && let Some(st) = hist.stats(&pn)
        {
            return PackageEstimate {
                package,
                estimated_secs: Some(st.median_secs),
                average_secs: Some(st.mean_secs),
                samples: st.samples,
                source_system: Some(system.to_string()),
                fallback: false,
            };
        }

        // Fallback: other system with the most samples (config order on ties).
        let mut best: Option<(&str, PackageStats)> = None;
        for name in &self.order {
            if name == system {
                continue;
            }
            if let Some(hist) = self.systems.get(name)
                && let Some(st) = hist.stats(&pn)
            {
                let better = best.as_ref().is_none_or(|(_, b)| st.samples > b.samples);
                if better {
                    best = Some((name.as_str(), st));
                }
            }
        }

        match best {
            Some((src, st)) => PackageEstimate {
                package,
                estimated_secs: Some(st.median_secs),
                average_secs: Some(st.mean_secs),
                samples: st.samples,
                source_system: Some(src.to_string()),
                fallback: true,
            },
            None => PackageEstimate {
                package,
                estimated_secs: None,
                average_secs: None,
                samples: 0,
                source_system: None,
                fallback: false,
            },
        }
    }

    pub fn estimate_many(&self, system: &str, packages: &[String]) -> Vec<PackageEstimate> {
        packages.iter().map(|p| self.estimate(system, p)).collect()
    }

    /// Read a log file directly (helper for tests / debugging).
    pub fn parse_file(path: &Path) -> HashMap<String, Vec<u64>> {
        std::fs::read_to_string(path)
            .map(|t| parse_emerge_log(&t))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
1793303083:  >>> emerge (8 of 9) media-libs/libdvdnav-7.0.0 to /\n\
1793303095:  ::: completed emerge (8 of 9) media-libs/libdvdnav-7.0.0 to /\n\
1793303095:  >>> emerge (9 of 9) media-video/mplayer-1.5_p20241125-r2 to /\n\
1793303145:  ::: completed emerge (9 of 9) media-video/mplayer-1.5_p20241125-r2 to /\n\
1793305302:  >>> emerge (1 of 1) gui-apps/mako-1.11.0 to /\n\
1793305313:  ::: completed emerge (1 of 1) gui-apps/mako-1.11.0 to /\n";

    #[test]
    fn parses_durations() {
        let map = parse_emerge_log(SAMPLE);
        assert_eq!(map.get("media-libs/libdvdnav"), Some(&vec![12]));
        assert_eq!(map.get("media-video/mplayer"), Some(&vec![50]));
        assert_eq!(map.get("gui-apps/mako"), Some(&vec![11]));
    }

    #[test]
    fn ignores_unmatched_completes() {
        let map = parse_emerge_log("1793303095:  ::: completed emerge (8 of 9) a/b-1.0 to /\n");
        assert!(map.is_empty());
    }

    #[test]
    fn strips_versions() {
        assert_eq!(
            strip_version("sys-apps/iproute2-7.3.0"),
            "sys-apps/iproute2"
        );
        assert_eq!(
            strip_version("media-video/mplayer-1.5_p20241125-r2"),
            "media-video/mplayer"
        );
        assert_eq!(
            strip_version("gui-apps/mako-1.11.0::gentoo"),
            "gui-apps/mako"
        );
        assert_eq!(strip_version("sys-apps/foo"), "sys-apps/foo");
        assert_eq!(
            strip_version("dev-lang/python-3.14.10_p1"),
            "dev-lang/python"
        );
    }

    #[test]
    fn fallback_prefers_most_samples() {
        let store = HistoryStore {
            order: vec!["a".into(), "b".into(), "c".into()],
            systems: HashMap::from([
                (
                    "a".to_string(),
                    SystemHistory {
                        system: "a".into(),
                        samples: HashMap::new(),
                    },
                ),
                (
                    "b".to_string(),
                    SystemHistory {
                        system: "b".into(),
                        samples: HashMap::from([("x/y".to_string(), vec![10])]),
                    },
                ),
                (
                    "c".to_string(),
                    SystemHistory {
                        system: "c".into(),
                        samples: HashMap::from([("x/y".to_string(), vec![20, 30])]),
                    },
                ),
            ]),
        };
        let e = store.estimate("a", "x/y-1.0");
        assert!(e.fallback);
        assert_eq!(e.source_system.as_deref(), Some("c"));
        assert_eq!(e.estimated_secs, Some(25)); // median of [20,30]
    }

    #[test]
    fn own_history_wins_over_fallback() {
        let store = HistoryStore {
            order: vec!["a".into(), "b".into()],
            systems: HashMap::from([
                (
                    "a".to_string(),
                    SystemHistory {
                        system: "a".into(),
                        samples: HashMap::from([("x/y".to_string(), vec![5])]),
                    },
                ),
                (
                    "b".to_string(),
                    SystemHistory {
                        system: "b".into(),
                        samples: HashMap::from([("x/y".to_string(), vec![100, 200])]),
                    },
                ),
            ]),
        };
        let e = store.estimate("a", "x/y");
        assert!(!e.fallback);
        assert_eq!(e.source_system.as_deref(), Some("a"));
        assert_eq!(e.estimated_secs, Some(5));
    }
}
