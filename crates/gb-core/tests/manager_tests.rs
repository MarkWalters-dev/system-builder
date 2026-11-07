//! Manager integration tests: queueing, dry-run builds, estimates.
//!
//! All tests run in `dry_run` mode: no real emerge, machines, or git access.
//! History fixtures are written to temp dirs so estimates are deterministic.

use gb_core::Manager;
use gb_core::config::{Config, ServerConfig, SystemConfig};
use gb_types::{CreateBuildRequest, EstimateRequest, JobState};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const LOG_A: &str = "\
1000:  >>> emerge (1 of 1) app-fake/pkg1-1.0 to /\n\
1010:  ::: completed emerge (1 of 1) app-fake/pkg1-1.0 to /\n\
2000:  >>> emerge (1 of 1) app-fake/pkg1-1.2 to /\n\
2030:  ::: completed emerge (1 of 1) app-fake/pkg1-1.2 to /\n\
3000:  >>> emerge (1 of 1) app-fake/pkg2-1.0 to /\n\
3100:  ::: completed emerge (1 of 1) app-fake/pkg2-1.0 to /\n";

const LOG_B: &str = "\
1000:  >>> emerge (1 of 1) app-fake/pkg9-1.0 to /\n\
1060:  ::: completed emerge (1 of 1) app-fake/pkg9-1.0 to /\n";

fn fixture_machine(root: &Path, log: &str) -> PathBuf {
    let machine = root.to_path_buf();
    std::fs::create_dir_all(machine.join("var/log")).unwrap();
    std::fs::write(machine.join("var/log/emerge.log"), log).unwrap();
    machine
}

fn test_config(a: PathBuf, b: PathBuf) -> Config {
    Config {
        discovery: Default::default(),
        provision: Default::default(),
        server: ServerConfig {
            dry_run: true,
            max_concurrent_builds: 1,
            ..Default::default()
        },
        systems: vec![
            SystemConfig {
                name: "a".into(),
                nspawn: Some("machine-a".into()),
                machine_path: a,
                emerge_log: "/var/log/emerge.log".into(),
                emerge_bin: "/usr/bin/emerge".into(),
            },
            SystemConfig {
                name: "b".into(),
                nspawn: None, // host-style system
                machine_path: b,
                emerge_log: "/var/log/emerge.log".into(),
                emerge_bin: "/usr/bin/emerge".into(),
            },
        ],
    }
}

fn setup() -> (tempfile::TempDir, Manager) {
    let tmp = tempfile::tempdir().unwrap();
    let a = fixture_machine(&tmp.path().join("mach-a"), LOG_A);
    let b = fixture_machine(&tmp.path().join("mach-b"), LOG_B);
    let mgr = Manager::new(test_config(a, b));
    (tmp, mgr)
}

fn req(system: &str, targets: &[&str]) -> CreateBuildRequest {
    CreateBuildRequest {
        system: system.to_string(),
        targets: targets.iter().map(|s| s.to_string()).collect(),
        emerge_args: Vec::new(),
        auto_fix_conflicts: None,
    }
}

async fn wait_terminal(mgr: &Manager, id: Uuid) -> gb_types::BuildJob {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let job = mgr.get(id).await.expect("job exists");
        if job.state.is_terminal() {
            return job;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {id}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn rejects_unknown_system() {
    let (_tmp, mgr) = setup();
    let err = mgr.submit(req("nope", &["@system"])).await.unwrap_err();
    assert!(matches!(err, gb_core::ManagerError::UnknownSystem(_)));
}

#[tokio::test]
async fn rejects_empty_targets() {
    let (_tmp, mgr) = setup();
    let err = mgr.submit(req("a", &[])).await.unwrap_err();
    assert!(matches!(err, gb_core::ManagerError::EmptyTargets));
}

#[tokio::test]
async fn dry_run_build_completes_with_plan_and_estimates() {
    let (_tmp, mgr) = setup();
    let job = mgr.submit(req("a", &["@system"])).await.unwrap();
    let done = wait_terminal(&mgr, job.id).await;
    assert_eq!(done.state, JobState::Done, "error: {:?}", done.error);
    // Auto-fix defaults on; dry-run pretend has no blockers to unmerge.
    assert!(done.auto_fix_conflicts);
    assert!(done.unmerged_blockers.is_empty());
    // Dry-run pretend resolves one package per target.
    assert_eq!(done.plan, vec!["app-fake/pkg1-1.0".to_string()]);
    assert_eq!(done.progress.total_packages, 1);
    // pkg1 has own-system history: median of [10, 30] = 20.
    assert_eq!(done.estimates.len(), 1);
    let est = &done.estimates[0];
    assert_eq!(est.estimated_secs, Some(20));
    assert_eq!(est.source_system.as_deref(), Some("a"));
    assert!(!est.fallback);
    // Log captured the fake emerge output.
    let log = mgr.log(job.id, None).await.unwrap();
    assert!(log.log.contains("Emerging"), "log was: {}", log.log);
}

/// A fake `emerge` that reports a blocker on its first `-p` call, is clean
/// after, and logs every invocation. Lets the real manager + runner exercise
/// the auto-fix loop with no Gentoo system involved.
fn stub_emerge_bin(dir: &Path) -> PathBuf {
    let stub = dir.join("emerge-stub");
    std::fs::write(
        &stub,
        r#"#!/bin/sh
# First pretend reports a blocker; later pretends are clean.
dir=$(dirname "$0")
echo "$@" >> "$dir/calls.log"
for a in "$@"; do
  case "$a" in
    --unmerge) echo ">>> unmerge success: $*"; exit 0;;
  esac
done
for a in "$@"; do
  case "$a" in
    -p)
      if [ -f "$dir/blocked" ]; then
        echo '[ebuild   R    ] app-fake/pkg1-1.0'
      else
        touch "$dir/blocked"
        echo '[ebuild  N     ] app-fake/pkg1-1.0'
        echo '[blocks B      ] app-fake/oldpkg ("app-fake/oldpkg" is blocking app-fake/pkg1-1.0)'
      fi
      exit 0;;
  esac
done
echo '>>> Emerging (1 of 1) app-fake/pkg1-1.0::gentoo'
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    stub
}

/// Non-dry-run manager whose "host" system uses the stub emerge binary.
fn stub_setup() -> (tempfile::TempDir, Manager, PathBuf) {
    // config.dry_run() also honors the env var; these tests need it off.
    // No other test depends on its absence, and none set it.
    // SAFETY: no other thread in this test binary touches process env.
    unsafe { std::env::remove_var("SYSTEM_BUILDER_DRY_RUN") };
    let tmp = tempfile::tempdir().unwrap();
    let stub_dir = tmp.path().join("stub");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let stub = stub_emerge_bin(&stub_dir);
    let machine = fixture_machine(&tmp.path().join("mach"), "");
    let config = Config {
        discovery: Default::default(),
        provision: Default::default(),
        server: ServerConfig {
            dry_run: false,
            max_concurrent_builds: 1,
            ..Default::default()
        },
        systems: vec![SystemConfig {
            name: "a".into(),
            nspawn: None,
            machine_path: machine,
            emerge_log: "/var/log/emerge.log".into(),
            emerge_bin: stub,
        }],
    };
    let mgr = Manager::new(config);
    (tmp, mgr, stub_dir)
}

#[tokio::test]
async fn auto_fix_unmerges_blockers_end_to_end() {
    let (_tmp, mgr, stub_dir) = stub_setup();
    let job = mgr.submit(req("a", &["@system"])).await.unwrap();
    let done = wait_terminal(&mgr, job.id).await;
    assert_eq!(done.state, JobState::Done, "error: {:?}", done.error);
    assert_eq!(done.unmerged_blockers, vec!["app-fake/oldpkg".to_string()]);
    assert_eq!(done.plan, vec!["app-fake/pkg1-1.0".to_string()]);
    // The stub really saw an unmerge invocation for the blocker.
    let calls = std::fs::read_to_string(stub_dir.join("calls.log")).unwrap();
    assert!(
        calls
            .lines()
            .any(|l| l.contains("--unmerge") && l.contains("app-fake/oldpkg")),
        "calls were:\n{calls}"
    );
    // ... and the activity is visible in the job log.
    let log = mgr.log(job.id, None).await.unwrap();
    assert!(
        log.log.contains("auto-unmerged blockers: app-fake/oldpkg"),
        "log was:\n{}",
        log.log
    );
}

#[tokio::test]
async fn default_remote_args_update_instead_of_reinstalling() {
    let (_tmp, mgr) = setup();
    let job = mgr.submit(req("a", &["@system"])).await.unwrap();
    // Update semantics by default: without these, `emerge @system` lists
    // the whole set as reinstalls and every build merges it all again.
    assert_eq!(
        job.emerge_args,
        vec!["--update", "--newuse", "--deep", "--verbose"]
    );
    let done = wait_terminal(&mgr, job.id).await;
    assert_eq!(done.state, JobState::Done);
    // Explicit args replace the default entirely.
    let mut r = req("a", &["@system"]);
    r.emerge_args = vec!["--verbose".to_string()];
    let custom = mgr.submit(r).await.unwrap();
    assert_eq!(custom.emerge_args, vec!["--verbose".to_string()]);
    let done = wait_terminal(&mgr, custom.id).await;
    assert_eq!(done.state, JobState::Done);
}

#[tokio::test]
async fn pretend_and_build_both_run_as_updates() {
    let (_tmp, mgr, stub_dir) = stub_setup();
    let job = mgr.submit(req("a", &["@system"])).await.unwrap();
    let done = wait_terminal(&mgr, job.id).await;
    assert_eq!(done.state, JobState::Done, "error: {:?}", done.error);
    let calls = std::fs::read_to_string(stub_dir.join("calls.log")).unwrap();
    let has = |line: &str, flag: &str| line.split_whitespace().any(|t| t == flag);
    // Pretend runs resolve the update plan, so they need update flags too —
    // otherwise the plan lists the whole set as reinstalls every time.
    let pretends: Vec<&str> = calls.lines().filter(|l| has(l, "-p")).collect();
    assert!(!pretends.is_empty(), "no pretend calls in:\n{calls}");
    for line in &pretends {
        for flag in ["--update", "--newuse", "--deep"] {
            assert!(
                has(line, flag),
                "pretend line missing {flag}: {line}\nall calls:\n{calls}"
            );
        }
    }
    // The real build (neither pretend nor unmerge) updates as well.
    let builds: Vec<&str> = calls
        .lines()
        .filter(|l| !has(l, "-p") && !has(l, "--unmerge"))
        .collect();
    assert!(!builds.is_empty(), "no build calls in:\n{calls}");
    for line in &builds {
        for flag in ["--update", "--newuse", "--deep"] {
            assert!(
                has(line, flag),
                "build line missing {flag}: {line}\nall calls:\n{calls}"
            );
        }
    }
}

#[tokio::test]
async fn disabled_auto_fix_fails_on_blockers() {
    let (_tmp, mgr, _stub_dir) = stub_setup();
    let mut r = req("a", &["@system"]);
    r.auto_fix_conflicts = Some(false);
    let job = mgr.submit(r).await.unwrap();
    let done = wait_terminal(&mgr, job.id).await;
    assert_eq!(done.state, JobState::Failed);
    let error = done.error.unwrap_or_default();
    assert!(error.contains("app-fake/oldpkg"), "error was: {error}");
    assert!(error.contains("auto-fix disabled"), "error was: {error}");
}

#[tokio::test]
async fn auto_fix_can_be_disabled_per_build() {
    let (_tmp, mgr) = setup();
    let mut r = req("a", &["@system"]);
    r.auto_fix_conflicts = Some(false);
    let job = mgr.submit(r).await.unwrap();
    assert!(!job.auto_fix_conflicts);
    let done = wait_terminal(&mgr, job.id).await;
    assert_eq!(done.state, JobState::Done);
    assert!(!done.auto_fix_conflicts);
    assert!(done.unmerged_blockers.is_empty());
}

#[tokio::test]
async fn builds_serialize_with_max_concurrent_1() {
    let (_tmp, mgr) = setup();
    let first = mgr.submit(req("a", &["@system"])).await.unwrap();
    let second = mgr.submit(req("b", &["@system"])).await.unwrap();
    let a = wait_terminal(&mgr, first.id).await;
    let b = wait_terminal(&mgr, second.id).await;
    assert_eq!(a.state, JobState::Done);
    assert_eq!(b.state, JobState::Done);
    // Global slot of 1: second build started no earlier than first finished.
    assert!(b.started_at >= a.finished_at);
}

#[tokio::test]
async fn same_system_never_runs_twice_at_once() {
    let (_tmp, mgr) = setup();
    let first = mgr.submit(req("a", &["@system"])).await.unwrap();
    let second = mgr.submit(req("a", &["@system"])).await.unwrap();
    let a = wait_terminal(&mgr, first.id).await;
    let b = wait_terminal(&mgr, second.id).await;
    assert_eq!(a.state, JobState::Done);
    assert_eq!(b.state, JobState::Done);
    assert!(b.started_at >= a.finished_at);
}

#[tokio::test]
async fn shutdown_stops_claims_and_ends_inflight_build() {
    let (_tmp, mgr) = setup();
    let j1 = mgr
        .submit(req("a", &["t1", "t2", "t3", "t4", "t5"]))
        .await
        .unwrap();
    let j2 = mgr.submit(req("b", &["t1"])).await.unwrap();
    mgr.shutdown().await;
    // shutdown() waited: j1 finished somehow, j2 was failed shut.
    let end = mgr.get(j1.id).await.unwrap();
    assert!(
        matches!(end.state, JobState::Cancelled | JobState::Done),
        "state: {:?}",
        end.state
    );
    assert_eq!(mgr.get(j2.id).await.unwrap().state, JobState::Cancelled);
}

#[tokio::test]
async fn cancel_unknown_and_finished() {
    let (_tmp, mgr) = setup();
    let err = mgr.cancel(Uuid::new_v4()).await.unwrap_err();
    assert!(matches!(err, gb_core::ManagerError::UnknownJob(_)));

    let job = mgr.submit(req("a", &["@system"])).await.unwrap();
    wait_terminal(&mgr, job.id).await;
    let err = mgr.cancel(job.id).await.unwrap_err();
    assert!(matches!(err, gb_core::ManagerError::AlreadyFinished));
}

#[tokio::test]
async fn cancel_queued_build() {
    let (_tmp, mgr) = setup();
    let first = mgr.submit(req("a", &["@system"])).await.unwrap();
    let second = mgr.submit(req("b", &["@system"])).await.unwrap();
    // Cancel immediately: with max_concurrent=1 the second build is still
    // queued (and even if it just started, the cancel token applies).
    let cancelled = mgr.cancel(second.id).await.unwrap();
    assert!(
        cancelled.state == JobState::Cancelled || cancelled.state.is_active(),
        "unexpected state {:?}",
        cancelled.state
    );
    let done = wait_terminal(&mgr, second.id).await;
    assert_eq!(done.state, JobState::Cancelled);
    // The first build is unaffected.
    let a = wait_terminal(&mgr, first.id).await;
    assert_eq!(a.state, JobState::Done);
    // Nothing left in the queue.
    let q = mgr.queue_status().await;
    assert!(q.queued.is_empty());
}

#[tokio::test]
async fn system_active_reports_idle_and_unknown() {
    let (_tmp, mgr) = setup();
    let idle = mgr.system_active("a").await.unwrap();
    assert!(!idle.active);
    assert!(idle.build.is_none());
    let err = mgr.system_active("nope").await.unwrap_err();
    assert!(matches!(err, gb_core::ManagerError::UnknownSystem(_)));
}

#[tokio::test]
async fn estimate_uses_fallback_system() {
    let (_tmp, mgr) = setup();
    // pkg9 only exists in b's logs; estimating for a must fall back.
    let resp = mgr
        .estimate(EstimateRequest {
            system: "a".into(),
            packages: vec!["app-fake/pkg9".into(), "app-fake/pkg-missing".into()],
            targets: vec![],
        })
        .await
        .unwrap();
    assert_eq!(resp.packages.len(), 2);
    let fb = &resp.packages[0];
    assert_eq!(fb.estimated_secs, Some(60));
    assert_eq!(fb.source_system.as_deref(), Some("b"));
    assert!(fb.fallback);
    let missing = &resp.packages[1];
    assert_eq!(missing.estimated_secs, None);
    assert_eq!(missing.source_system, None);
    assert_eq!(resp.packages_with_history, 1);
    assert_eq!(resp.packages_without_history, 1);
}

#[tokio::test]
async fn estimate_can_resolve_targets() {
    let (_tmp, mgr) = setup();
    let resp = mgr
        .estimate(EstimateRequest {
            system: "a".into(),
            packages: vec![],
            targets: vec!["@system".into()],
        })
        .await
        .unwrap();
    assert_eq!(resp.plan, vec!["app-fake/pkg1-1.0".to_string()]);
    assert_eq!(resp.total_estimated_secs, Some(20));
}

#[tokio::test]
async fn history_reports_stats_and_source() {
    let (_tmp, mgr) = setup();
    let h = mgr.history("a", "app-fake/pkg1-3.0").await.unwrap();
    assert_eq!(h.samples, 2);
    assert_eq!(h.median_secs, Some(20));
    assert_eq!(h.average_secs, Some(20));
    assert_eq!(h.min_secs, Some(10));
    assert_eq!(h.max_secs, Some(30));
    assert_eq!(h.source_system.as_deref(), Some("a"));
    assert!(!h.fallback);

    let fb = mgr.history("a", "app-fake/pkg9").await.unwrap();
    assert_eq!(fb.source_system.as_deref(), Some("b"));
    assert!(fb.fallback);
}
