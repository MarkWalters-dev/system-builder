//! Parsing live `emerge` output and `emerge -p` plans.
//!
//! Live progress lines look like:
//! ```text
//! >>> Emerging (1 of 5) sys-apps/foo-1.0::gentoo
//! >>> Installing (1 of 5) sys-apps/foo-1.0::gentoo
//! ```
//! `emerge -p` (pretend) lines look like:
//! ```text
//! [ebuild   R    ] sys-apps/foo-1.0::gentoo  USE="..."
//! [binary     R  ] sys-apps/foo-1.0::gentoo
//! ```

use regex::Regex;
use std::sync::OnceLock;

fn progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r">>>\s+\S+.*\(\s*(\d+)\s+of\s+(\d+)\)\s+(\S+)").expect("progress regex")
    })
}

/// Live "package x of y" state, fed one output line at a time.
#[derive(Debug, Clone, Default)]
pub struct LiveProgress {
    pub current_package: Option<String>,
    pub current_index: usize,
    pub total_packages: usize,
}

impl LiveProgress {
    /// Feed one emerge output line. Returns true if progress changed.
    pub fn feed(&mut self, line: &str) -> bool {
        let caps = match progress_re().captures(line) {
            Some(c) => c,
            None => return false,
        };
        let index: usize = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let total: usize = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let atom = caps.get(3).map(|m| m.as_str().to_string());
        if index == 0 || total == 0 {
            return false;
        }
        let changed = self.current_index != index
            || self.total_packages != total
            || self.current_package != atom;
        self.current_index = index;
        self.total_packages = total;
        if atom.is_some() {
            self.current_package = atom;
        }
        changed
    }
}

/// Extract the ordered package plan from `emerge -p` output.
///
/// Returns versioned atoms (`cat/pkg-ver`, repo suffix stripped).
pub fn parse_pretend_plan(output: &str) -> Vec<String> {
    let mut plan = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        let is_merge = line.starts_with("[ebuild") || line.starts_with("[binary");
        if !is_merge {
            continue;
        }
        let Some(close) = line.find(']') else {
            continue;
        };
        let after = line[close + 1..].trim_start();
        let atom = after.split_whitespace().next().unwrap_or_default();
        if atom.is_empty() || atom.starts_with('[') {
            continue;
        }
        // Strip ::repo suffix; strip slot-ish trailing junk is not present here.
        let atom = atom.split("::").next().unwrap_or(atom);
        plan.push(atom.to_string());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feeds_live_progress() {
        let mut p = LiveProgress::default();
        assert!(!p.feed(" * Package:    sys-apps/foo-1.0"));
        assert!(p.feed(">>> Emerging (1 of 5) sys-apps/foo-1.0::gentoo"));
        assert_eq!(p.current_index, 1);
        assert_eq!(p.total_packages, 5);
        assert_eq!(
            p.current_package.as_deref(),
            Some("sys-apps/foo-1.0::gentoo")
        );
        assert!(p.feed(">>> Installing (2 of 5) sys-bar/bar-2.0::gentoo"));
        assert_eq!(p.current_index, 2);
        assert!(!p.feed(">>> Installing (2 of 5) sys-bar/bar-2.0::gentoo"));
    }

    #[test]
    fn parses_pretend_output() {
        let out = "\
These are the packages that would be merged, in order:\n\
\n\
Calculating dependencies... done!\n\
Dependency resolution took 1.23 s.\n\
\n\
[ebuild   R    ] sys-apps/iproute2-7.3.0::gentoo  USE=\"-minimal\" \n\
[ebuild  N     ] dev-lang/ruby-3.3.13::gentoo  USE=\"ssl\" \n\
[ebuild   R   ] sys-apps/kmod-34 \n\
[blocks b      ] <dev-lang/ruby-3.0 (is blocking dev-lang/ruby-3.3.13)\n\
";
        assert_eq!(
            parse_pretend_plan(out),
            vec![
                "sys-apps/iproute2-7.3.0",
                "dev-lang/ruby-3.3.13",
                "sys-apps/kmod-34"
            ]
        );
    }
}
