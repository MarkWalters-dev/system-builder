//! Resolving a build plan, with automatic blocker (conflict) fixing.
//!
//! `emerge -p` lists blockers as:
//! ```text
//! [blocks B      ] app-text/dos2unix ("app-text/dos2unix" is blocking ...)
//! [blocks b      ] <sys-libs/zlib-1.2 ("..." is blocking sys-libs/zlib-1.3)
//! ```
//! When auto-fix is on, each resolve attempt unmerges the listed blockers
//! and re-resolves, until the plan is clean or the attempt budget is spent.

use thiserror::Error;

use crate::progress::parse_pretend_plan;

/// Blockers named by an `emerge -p` run, in order, deduplicated.
pub fn parse_blocks(output: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with("[blocks") {
            continue;
        }
        let Some(close) = line.find(']') else {
            continue;
        };
        let atom = line[close + 1..]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches('"');
        if atom.is_empty() || atom.starts_with('[') {
            continue;
        }
        let atom = atom.to_string();
        if !blocks.contains(&atom) {
            blocks.push(atom);
        }
    }
    blocks
}

/// What `resolve_with_retry` needs from the outside world. The manager wires
/// this to the real runner; tests use a scripted fake.
#[async_trait::async_trait]
pub trait BlockResolver {
    /// Run `emerge -p` and return its output.
    async fn pretend(&mut self) -> Result<String, ResolveError>;
    /// Unmerge blockers; returns the combined command output for the job log.
    async fn unmerge(&mut self, blockers: &[String]) -> Result<String, ResolveError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlan {
    pub plan: Vec<String>,
    /// Blockers unmerged across all attempts, oldest first.
    pub unmerged: Vec<String>,
    /// Combined output of each unmerge invocation, oldest first.
    pub unmerge_logs: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("blocked by: {0}")]
    Blocked(String),
    #[error("resolve backend failed: {0}")]
    Backend(String),
    /// The target machine could not be started; emerge never ran.
    #[error("machine failed to start: {0}")]
    Machine(String),
}

/// Resolve the plan, unmerging blockers and retrying while `auto_fix` allows.
///
/// At most `max_attempts` pretend runs happen (minimum 1). Stops early with
/// [`ResolveError::Blocked`] when auto-fix is off, when an attempt surfaces
/// no *new* blockers (further unmerges would be futile), or when the budget
/// is spent with blocks remaining.
pub async fn resolve_with_retry(
    backend: &mut impl BlockResolver,
    auto_fix: bool,
    max_attempts: usize,
) -> Result<ResolvedPlan, ResolveError> {
    let max_attempts = max_attempts.max(1);
    let mut unmerged: Vec<String> = Vec::new();
    let mut unmerge_logs: Vec<String> = Vec::new();
    let mut last_blocks: Vec<String> = Vec::new();
    for _ in 0..max_attempts {
        let output = backend.pretend().await?;
        let plan = parse_pretend_plan(&output);
        let blocks = parse_blocks(&output);
        if blocks.is_empty() {
            return Ok(ResolvedPlan {
                plan,
                unmerged,
                unmerge_logs,
            });
        }
        last_blocks = blocks;
        if !auto_fix {
            return Err(ResolveError::Blocked(last_blocks.join(", ")));
        }
        let fresh: Vec<String> = last_blocks
            .iter()
            .filter(|b| !unmerged.contains(b))
            .cloned()
            .collect();
        if fresh.is_empty() {
            return Err(ResolveError::Blocked(last_blocks.join(", ")));
        }
        unmerge_logs.push(backend.unmerge(&fresh).await?);
        unmerged.extend(fresh);
    }
    Err(ResolveError::Blocked(last_blocks.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_BLOCKS: &str = "\
[ebuild  N     ] dev-lang/ruby-3.3.13::gentoo\n\
[blocks B      ] app-text/dos2unix (\"app-text/dos2unix\" is blocking app-text/hd2u-1.0.5)\n\
[blocks b      ] <sys-libs/zlib-1.2 (\"<sys-libs/zlib-1.2\" is blocking sys-libs/zlib-1.3)\n\
[blocks B      ] app-text/dos2unix (\"app-text/dos2unix\" is blocking app-text/hd2u-1.0.5)\n";

    const CLEAN: &str = "[ebuild   R    ] sys-apps/iproute2-7.3.0\n";

    #[test]
    fn parses_and_dedups_blocks() {
        assert_eq!(
            parse_blocks(WITH_BLOCKS),
            vec!["app-text/dos2unix", "<sys-libs/zlib-1.2"]
        );
        assert!(parse_blocks(CLEAN).is_empty());
        assert!(parse_blocks("").is_empty());
    }

    /// Scripted backend: yields queued pretend outputs, records unmerges.
    struct Script {
        pretends: std::collections::VecDeque<Result<String, ResolveError>>,
        unmerged: Vec<Vec<String>>,
        unmerge_output: String,
    }

    impl Script {
        fn new(outputs: Vec<&str>) -> Self {
            Self {
                pretends: outputs.into_iter().map(|s| Ok(s.to_string())).collect(),
                unmerged: Vec::new(),
                unmerge_output: ">>> unmerge success".to_string(),
            }
        }
    }

    #[async_trait::async_trait]
    impl BlockResolver for Script {
        async fn pretend(&mut self) -> Result<String, ResolveError> {
            self.pretends.pop_front().unwrap_or(Ok(String::new()))
        }

        async fn unmerge(&mut self, blockers: &[String]) -> Result<String, ResolveError> {
            self.unmerged.push(blockers.to_vec());
            Ok(self.unmerge_output.clone())
        }
    }

    #[tokio::test]
    async fn clean_plan_needs_no_unmerge() {
        let mut backend = Script::new(vec![CLEAN]);
        let resolved = resolve_with_retry(&mut backend, true, 3).await.unwrap();
        assert_eq!(resolved.plan, vec!["sys-apps/iproute2-7.3.0"]);
        assert!(resolved.unmerged.is_empty());
        assert!(backend.unmerged.is_empty());
    }

    #[tokio::test]
    async fn blocks_then_clean_records_unmerged() {
        let mut backend = Script::new(vec![WITH_BLOCKS, CLEAN]);
        let resolved = resolve_with_retry(&mut backend, true, 3).await.unwrap();
        assert_eq!(resolved.plan, vec!["sys-apps/iproute2-7.3.0"]);
        assert_eq!(
            resolved.unmerged,
            vec!["app-text/dos2unix", "<sys-libs/zlib-1.2"]
        );
        assert_eq!(backend.unmerged.len(), 1);
        assert_eq!(
            resolved.unmerge_logs,
            vec![">>> unmerge success".to_string()]
        );
    }

    #[tokio::test]
    async fn no_autofix_reports_blocks() {
        let mut backend = Script::new(vec![WITH_BLOCKS]);
        let err = resolve_with_retry(&mut backend, false, 3)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolveError::Blocked(_)));
        assert!(backend.unmerged.is_empty());
    }

    #[tokio::test]
    async fn persistent_blocks_stop_when_nothing_new() {
        let mut backend = Script::new(vec![WITH_BLOCKS, WITH_BLOCKS, WITH_BLOCKS]);
        let err = resolve_with_retry(&mut backend, true, 3).await.unwrap_err();
        assert!(matches!(err, ResolveError::Blocked(_)));
        // Second attempt found no new blockers, so only one unmerge ran.
        assert_eq!(backend.unmerged.len(), 1);
    }

    #[tokio::test]
    async fn attempt_budget_caps_unmerges() {
        let b1 = "[blocks B ] x/a-1.0\n";
        let b2 = "[blocks B ] x/b-1.0\n";
        let b3 = "[blocks B ] x/c-1.0\n";
        let b4 = "[blocks B ] x/d-1.0\n";
        let mut backend = Script::new(vec![b1, b2, b3, b4]);
        let err = resolve_with_retry(&mut backend, true, 3).await.unwrap_err();
        assert_eq!(err, ResolveError::Blocked("x/c-1.0".to_string()));
        assert_eq!(backend.unmerged.len(), 3);
    }

    #[tokio::test]
    async fn backend_errors_propagate() {
        let mut backend = Script {
            pretends: [Err(ResolveError::Backend("boom".into()))]
                .into_iter()
                .collect(),
            unmerged: Vec::new(),
            unmerge_output: String::new(),
        };
        let err = resolve_with_retry(&mut backend, true, 3).await.unwrap_err();
        assert_eq!(err, ResolveError::Backend("boom".to_string()));
    }
}
