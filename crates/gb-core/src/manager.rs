//! Job queue, dispatcher, and build execution.
//!
//! Concurrency policy:
//!
//! * at most `max_concurrent_builds` jobs run at once **globally**
//!   (default 1: one build at a time across all systems);
//! * a system never runs two builds at once, regardless of the global limit;
//! * anything else waits FIFO in the queue.

use gb_types::{
    BuildJob, BuildLog, BuildProgress, BuildSummary, CreateBuildRequest, EstimateRequest,
    EstimateResponse, HistoryResponse, JobState, PackageEstimate, QueueStatus, StreamEvent,
    SystemActiveBuild, SystemInfo, SystemMode, SystemStatus,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use tokio::sync::{Notify, RwLock, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{Config, SystemConfig};
use crate::discover;
use crate::history::HistoryStore;
use crate::progress::{LiveProgress, parse_pretend_plan};
use crate::resolve::{BlockResolver, ResolveError, resolve_with_retry};
use crate::runner::{Runner, RunnerError, UPDATE_FLAGS};

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("unknown system '{0}'")]
    UnknownSystem(String),
    #[error("no targets to build")]
    EmptyTargets,
    #[error("unknown build '{0}'")]
    UnknownJob(Uuid),
    #[error("build is already finished")]
    AlreadyFinished,
    #[error("internal: {0}")]
    Internal(String),
}

struct JobRecord {
    job: BuildJob,
    log: VecDeque<String>,
    log_truncated: bool,
    bus: broadcast::Sender<StreamEvent>,
    cancel: CancellationToken,
}

impl JobRecord {
    fn push_log(&mut self, line: String, keep: usize) {
        if keep == 0 {
            return;
        }
        while self.log.len() >= keep {
            self.log.pop_front();
            self.log_truncated = true;
        }
        self.log.push_back(line);
    }
}

struct Inner {
    config: Config,
    jobs: RwLock<HashMap<Uuid, JobRecord>>,
    queue: tokio::sync::Mutex<VecDeque<Uuid>>,
    history: RwLock<HistoryStore>,
    runner: Runner,
    notify: Notify,
    /// Set by [`Manager::shutdown`]: the dispatcher stops claiming and
    /// running jobs bail at the next phase boundary, so no new emerge
    /// is spawned once shutdown starts.
    draining: AtomicBool,
}

/// Upper bound on pretend/unmerge cycles per build when auto-fix is on.
const MAX_RESOLVE_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

impl Manager {
    pub fn new(config: Config) -> Self {
        let dry_run = config.dry_run();
        // History starts empty and fills lazily: every estimate/build
        // refreshes from the freshly resolved systems.
        let inner = Arc::new(Inner {
            config,
            jobs: RwLock::new(HashMap::new()),
            queue: tokio::sync::Mutex::new(VecDeque::new()),
            history: RwLock::new(HistoryStore::empty()),
            runner: Runner::new(dry_run),
            notify: Notify::new(),
            draining: AtomicBool::new(false),
        });
        let mgr = Self { inner };
        // Dispatcher owns a clone and runs for the process lifetime.
        let worker = mgr.clone();
        tokio::spawn(async move { worker.dispatch_loop().await });
        mgr
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Effective systems: explicit config overlaid on fresh discovery.
    /// Resolved on every call so newly added VMs work without a restart.
    /// Discovery is skipped in dry-run (hermetic) and when disabled.
    async fn effective_systems(&self) -> Vec<SystemConfig> {
        let cfg = &self.inner.config;
        if cfg.dry_run() || !cfg.discovery.enabled {
            return cfg.systems.clone();
        }
        let discovered = discover::discover(&cfg.discovery).await;
        discover::merge(&cfg.systems, discovered)
    }

    async fn resolve_system(&self, name: &str) -> Option<SystemConfig> {
        self.effective_systems()
            .await
            .into_iter()
            .find(|s| s.name == name)
    }

    // ------------------------------------------------------------------ API

    pub async fn submit(&self, req: CreateBuildRequest) -> Result<BuildJob, ManagerError> {
        if self.resolve_system(&req.system).await.is_none() {
            return Err(ManagerError::UnknownSystem(req.system.clone()));
        }
        if req.targets.is_empty() || req.targets.iter().all(|t| t.trim().is_empty()) {
            return Err(ManagerError::EmptyTargets);
        }
        let now = chrono::Utc::now();
        let mut emerge_args = req.emerge_args.clone();
        if emerge_args.is_empty() {
            // Update semantics (same flags as pretend): up-to-date
            // packages are skipped instead of reinstalled every run.
            emerge_args.extend(UPDATE_FLAGS.iter().map(|s| s.to_string()));
            emerge_args.push("--verbose".to_string());
        }
        let job = BuildJob {
            id: Uuid::new_v4(),
            system: req.system,
            targets: req.targets,
            emerge_args,
            auto_fix_conflicts: req.auto_fix_conflicts.unwrap_or(true),
            unmerged_blockers: Vec::new(),
            state: JobState::Queued,
            queue_position: 0,
            progress: BuildProgress::default(),
            plan: Vec::new(),
            estimates: Vec::new(),
            error: None,
            exit_code: None,
            created_at: now,
            started_at: None,
            finished_at: None,
        };
        let (bus, _) = broadcast::channel(2048);
        {
            // Lock order is always queue -> jobs (see refresh/next_runnable).
            let mut queue = self.inner.queue.lock().await;
            let mut jobs = self.inner.jobs.write().await;
            jobs.insert(
                job.id,
                JobRecord {
                    job: job.clone(),
                    log: VecDeque::new(),
                    log_truncated: false,
                    bus,
                    cancel: CancellationToken::new(),
                },
            );
            queue.push_back(job.id);
        }
        self.refresh_queue_positions().await;
        self.inner.notify.notify_one();
        Ok(self.get(job.id).await.expect("just inserted"))
    }

    pub async fn get(&self, id: Uuid) -> Option<BuildJob> {
        let jobs = self.inner.jobs.read().await;
        let rec = jobs.get(&id)?;
        let mut job = rec.job.clone();
        // Fill in live elapsed time for running builds.
        if job.state == JobState::Building
            && let Some(started) = job.started_at
        {
            let elapsed = (chrono::Utc::now() - started).num_seconds().max(0) as u64;
            job.progress.elapsed_secs = elapsed;
            if let Some(total) = job.progress.estimated_total_secs {
                job.progress.estimated_remaining_secs = Some(total.saturating_sub(elapsed));
            }
        }
        Some(job)
    }

    pub async fn list(&self, state: Option<JobState>, system: Option<&str>) -> Vec<BuildSummary> {
        let jobs = self.inner.jobs.read().await;
        let mut out: Vec<BuildSummary> = jobs
            .values()
            .map(|r| &r.job)
            .filter(|j| state.is_none_or(|s| j.state == s))
            .filter(|j| system.is_none_or(|s| j.system == s))
            .map(BuildSummary::from)
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        out
    }

    pub async fn queue_status(&self) -> QueueStatus {
        let jobs = self.inner.jobs.read().await;
        let mut queued: Vec<BuildSummary> = Vec::new();
        let mut active: Vec<BuildSummary> = Vec::new();
        for r in jobs.values() {
            match r.job.state {
                JobState::Queued => queued.push(BuildSummary::from(&r.job)),
                s if s.is_active() => active.push(BuildSummary::from(&r.job)),
                _ => {}
            }
        }
        queued.sort_by_key(|s| s.queue_position);
        active.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        QueueStatus {
            max_concurrent_builds: self.inner.config.server.max_concurrent_builds,
            running: active.len(),
            queued,
            active,
        }
    }

    pub async fn systems(&self) -> Vec<SystemInfo> {
        let systems = self.effective_systems().await;
        let jobs = self.inner.jobs.read().await;
        systems
            .iter()
            .map(|s| {
                let active_build_id = jobs
                    .values()
                    .map(|r| &r.job)
                    .find(|j| j.system == s.name && j.state.is_active())
                    .map(|j| j.id);
                SystemInfo {
                    name: s.name.clone(),
                    mode: if s.is_host() {
                        SystemMode::Host
                    } else {
                        SystemMode::Nspawn
                    },
                    nspawn: s.nspawn.clone(),
                    status: if active_build_id.is_some() {
                        SystemStatus::Building
                    } else {
                        SystemStatus::Idle
                    },
                    active_build_id,
                }
            })
            .collect()
    }

    pub async fn system_active(&self, system: &str) -> Result<SystemActiveBuild, ManagerError> {
        if self.resolve_system(system).await.is_none() {
            return Err(ManagerError::UnknownSystem(system.to_string()));
        }
        let jobs = self.inner.jobs.read().await;
        // Prefer the running job over a queued one.
        let build = jobs
            .values()
            .map(|r| r.job.clone())
            .find(|j| j.system == system && j.state.is_active())
            .or_else(|| {
                jobs.values()
                    .map(|r| r.job.clone())
                    .find(|j| j.system == system && j.state == JobState::Queued)
            });
        Ok(match build {
            Some(mut b) => {
                if b.state == JobState::Building
                    && let Some(started) = b.started_at
                {
                    let elapsed = (chrono::Utc::now() - started).num_seconds().max(0) as u64;
                    b.progress.elapsed_secs = elapsed;
                    if let Some(total) = b.progress.estimated_total_secs {
                        b.progress.estimated_remaining_secs = Some(total.saturating_sub(elapsed));
                    }
                }
                SystemActiveBuild {
                    system: system.to_string(),
                    active: b.state.is_active(),
                    build: Some(b),
                }
            }
            None => SystemActiveBuild {
                system: system.to_string(),
                active: false,
                build: None,
            },
        })
    }

    pub async fn log(&self, id: Uuid, tail: Option<usize>) -> Option<BuildLog> {
        let jobs = self.inner.jobs.read().await;
        let rec = jobs.get(&id)?;
        let lines: Vec<&str> = match tail {
            Some(n) => rec
                .log
                .iter()
                .rev()
                .take(n)
                .rev()
                .map(String::as_str)
                .collect(),
            None => rec.log.iter().map(String::as_str).collect(),
        };
        Some(BuildLog {
            id,
            state: rec.job.state,
            log: lines.join("\n"),
            truncated: rec.log_truncated,
        })
    }

    /// Snapshot + backlog + live receiver for SSE streaming.
    pub async fn subscribe(
        &self,
        id: Uuid,
    ) -> Option<(BuildJob, Vec<String>, broadcast::Receiver<StreamEvent>)> {
        let jobs = self.inner.jobs.read().await;
        let rec = jobs.get(&id)?;
        Some((
            rec.job.clone(),
            rec.log.iter().cloned().collect(),
            rec.bus.subscribe(),
        ))
    }

    pub async fn cancel(&self, id: Uuid) -> Result<BuildJob, ManagerError> {
        // Fast path: still queued → remove from queue.
        {
            let mut queue = self.inner.queue.lock().await;
            if let Some(pos) = queue.iter().position(|q| *q == id) {
                queue.remove(pos);
                let mut jobs = self.inner.jobs.write().await;
                let rec = jobs.get_mut(&id).ok_or(ManagerError::UnknownJob(id))?;
                rec.job.state = JobState::Cancelled;
                rec.job.finished_at = Some(chrono::Utc::now());
                rec.job.queue_position = 0;
                let _ = rec.bus.send(StreamEvent::Done {
                    state: JobState::Cancelled,
                    exit_code: None,
                });
                let job = rec.job.clone();
                drop(jobs);
                // refresh_queue_positions() locks the queue: release it first.
                drop(queue);
                self.refresh_queue_positions().await;
                return Ok(job);
            }
        }
        let jobs = self.inner.jobs.read().await;
        let rec = jobs.get(&id).ok_or(ManagerError::UnknownJob(id))?;
        if rec.job.state.is_terminal() {
            return Err(ManagerError::AlreadyFinished);
        }
        if !rec.job.state.is_active() {
            return Err(ManagerError::AlreadyFinished);
        }
        rec.cancel.cancel();
        Ok(rec.job.clone())
    }

    // --------------------------------------------------------------- estimates

    /// Estimate build times. Refreshes history from disk first so estimates
    /// always reflect the latest logs.
    pub async fn estimate(&self, req: EstimateRequest) -> Result<EstimateResponse, ManagerError> {
        let sys = self
            .resolve_system(&req.system)
            .await
            .ok_or_else(|| ManagerError::UnknownSystem(req.system.clone()))?;
        self.refresh_history().await;

        let mut plan: Vec<String> = Vec::new();
        let mut packages: Vec<String> = req.packages.clone();
        if !req.targets.is_empty() {
            let pretend = self
                .inner
                .runner
                .run_pretend(&sys, &req.targets, &[])
                .await
                .map_err(|e| ManagerError::Internal(format!("pretend failed: {e}")))?;
            plan = parse_pretend_plan(&pretend);
            packages.extend(plan.clone());
        }

        let history = self.inner.history.read().await;
        let estimates = history.estimate_many(&req.system, &packages);
        let with = estimates
            .iter()
            .filter(|e| e.estimated_secs.is_some())
            .count();
        let total = if with == estimates.len() && !estimates.is_empty() {
            Some(estimates.iter().filter_map(|e| e.estimated_secs).sum())
        } else if estimates.iter().any(|e| e.estimated_secs.is_some()) {
            // Partial: sum what we know (documented as a lower bound).
            Some(estimates.iter().filter_map(|e| e.estimated_secs).sum())
        } else {
            None
        };
        Ok(EstimateResponse {
            system: req.system,
            packages: estimates,
            plan,
            total_estimated_secs: total,
            packages_with_history: with,
            packages_without_history: packages.len().saturating_sub(with),
        })
    }

    pub async fn history(
        &self,
        system: &str,
        package: &str,
    ) -> Result<HistoryResponse, ManagerError> {
        if self.resolve_system(system).await.is_none() {
            return Err(ManagerError::UnknownSystem(system.to_string()));
        }
        self.refresh_history().await;
        let history = self.inner.history.read().await;
        let est = history.estimate(system, package);
        let stats = est
            .source_system
            .as_deref()
            .and_then(|src| history.stats(src, package));
        Ok(HistoryResponse {
            system: system.to_string(),
            package: package.to_string(),
            samples: est.samples,
            average_secs: est.average_secs,
            median_secs: est.estimated_secs,
            min_secs: stats.as_ref().map(|s| s.min_secs),
            max_secs: stats.as_ref().map(|s| s.max_secs),
            source_system: est.source_system,
            fallback: est.fallback,
        })
    }

    async fn refresh_history(&self) {
        let systems = self.effective_systems().await;
        let mut history = self.inner.history.write().await;
        history.refresh_all(&systems);
    }

    // -------------------------------------------------------------- dispatcher

    async fn refresh_queue_positions(&self) {
        let queue = self.inner.queue.lock().await;
        let mut jobs = self.inner.jobs.write().await;
        for (i, id) in queue.iter().enumerate() {
            if let Some(rec) = jobs.get_mut(id) {
                rec.job.queue_position = i + 1;
            }
        }
    }

    async fn dispatch_loop(&self) {
        loop {
            self.inner.notify.notified().await;
            loop {
                let next = self.claim_next().await;
                let Some(id) = next else { break };
                let worker = self.clone();
                tokio::spawn(async move { worker.run_job(id).await });
            }
        }
    }

    /// Atomically dequeue the next job that fits the concurrency policy and
    /// transition it to `Resolving`, so each id is handed out exactly
    /// once. Returns `None` when no job is runnable right now.
    /// Stop the world for process shutdown: no new claims, queued jobs
    /// are marked cancelled (they will never run), live jobs get their
    /// cancel token, and we wait (bounded) for running builds to die so
    /// their emerge trees are reaped before we exit. In-memory state is
    /// lost on exit either way.
    ///
    /// The wait is bounded (10s): a job stuck somewhere uninterruptible
    /// (e.g. inside a hung `emerge -p`, which has no cancel point) must
    /// not wedge process shutdown. Streaming builds die promptly via
    /// their cancel branch.
    pub async fn shutdown(&self) {
        self.inner.draining.store(true, Ordering::SeqCst);
        {
            // Lock order queue -> jobs matches cancel(): no inversion.
            let mut queue = self.inner.queue.lock().await;
            let mut jobs = self.inner.jobs.write().await;
            queue.retain(|id| {
                if let Some(rec) = jobs.get_mut(id)
                    && rec.job.state == JobState::Queued
                {
                    rec.job.state = JobState::Cancelled;
                    rec.job.finished_at = Some(chrono::Utc::now());
                    rec.job.queue_position = 0;
                    let _ = rec.bus.send(StreamEvent::Done {
                        state: JobState::Cancelled,
                        exit_code: None,
                    });
                    return false;
                }
                true
            });
            for rec in jobs.values() {
                if !rec.job.state.is_terminal() {
                    rec.cancel.cancel();
                }
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            {
                let jobs = self.inner.jobs.read().await;
                if jobs.values().all(|r| r.job.state.is_terminal()) {
                    return;
                }
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::SeqCst)
    }

    async fn claim_next(&self) -> Option<Uuid> {
        if self.is_draining() {
            return None;
        }
        let mut queue = self.inner.queue.lock().await;
        loop {
            if queue.is_empty() {
                return None;
            }
            let candidate: Option<(usize, Uuid)> = {
                let jobs = self.inner.jobs.read().await;
                let running = jobs.values().filter(|r| r.job.state.is_active()).count();
                if running >= self.inner.config.server.max_concurrent_builds {
                    return None;
                }
                let busy: Vec<&str> = jobs
                    .values()
                    .filter(|r| r.job.state.is_active())
                    .map(|r| r.job.system.as_str())
                    .collect();
                queue.iter().enumerate().find_map(|(i, id)| {
                    jobs.get(id)
                        .filter(|r| r.job.state == JobState::Queued)
                        .filter(|r| !busy.contains(&r.job.system.as_str()))
                        .map(|_| (i, *id))
                })
            };
            let (pos, id) = candidate?;
            // Re-verify under the write lock: cancel() may have flipped the
            // job between our read and this write.
            {
                let mut jobs = self.inner.jobs.write().await;
                let still_queued = jobs
                    .get(&id)
                    .is_some_and(|r| r.job.state == JobState::Queued);
                if !still_queued {
                    queue.remove(pos);
                    continue;
                }
                let rec = jobs.get_mut(&id).expect("just checked");
                rec.job.state = JobState::Resolving;
                rec.job.started_at = Some(chrono::Utc::now());
                rec.job.queue_position = 0;
                let _ = rec.bus.send(StreamEvent::Status {
                    state: JobState::Resolving,
                });
                queue.remove(pos);
            }
            drop(queue);
            self.refresh_queue_positions().await;
            return Some(id);
        }
    }

    async fn set_state(&self, id: Uuid, state: JobState) {
        let mut jobs = self.inner.jobs.write().await;
        if let Some(rec) = jobs.get_mut(&id) {
            rec.job.state = state;
            rec.job.queue_position = 0;
            let _ = rec.bus.send(StreamEvent::Status { state });
        }
    }

    async fn fail(&self, id: Uuid, error: String) {
        {
            // Scope the guard: prune_finished() below needs jobs.write too.
            let mut jobs = self.inner.jobs.write().await;
            if let Some(rec) = jobs.get_mut(&id) {
                rec.job.state = JobState::Failed;
                rec.job.error = Some(error);
                rec.job.finished_at = Some(chrono::Utc::now());
                let _ = rec.bus.send(StreamEvent::Done {
                    state: JobState::Failed,
                    exit_code: rec.job.exit_code,
                });
            }
        }
        self.prune_finished().await;
        self.inner.notify.notify_one();
    }

    async fn finish(&self, id: Uuid, state: JobState, exit_code: Option<i32>) {
        {
            // Scope the guard: prune_finished() below needs jobs.write too.
            let mut jobs = self.inner.jobs.write().await;
            if let Some(rec) = jobs.get_mut(&id) {
                rec.job.state = state;
                rec.job.exit_code = exit_code;
                rec.job.finished_at = Some(chrono::Utc::now());
                let _ = rec.bus.send(StreamEvent::Done { state, exit_code });
            }
        }
        self.prune_finished().await;
        self.inner.notify.notify_one();
    }

    async fn prune_finished(&self) {
        let keep = self.inner.config.server.keep_finished_jobs;
        if keep == 0 {
            return;
        }
        let mut jobs = self.inner.jobs.write().await;
        let mut terminal: Vec<(chrono::DateTime<chrono::Utc>, Uuid)> = jobs
            .values()
            .filter(|r| r.job.state.is_terminal())
            .map(|r| (r.job.created_at, r.job.id))
            .collect();
        if terminal.len() <= keep {
            return;
        }
        terminal.sort();
        let drop_n = terminal.len() - keep;
        for (_, id) in terminal.into_iter().take(drop_n) {
            jobs.remove(&id);
        }
    }

    // ------------------------------------------------------------- job runner

    async fn run_job(&self, id: Uuid) {
        // Already dequeued and transitioned to Resolving by claim_next().
        let claimed = {
            let jobs = self.inner.jobs.read().await;
            match jobs.get(&id) {
                Some(rec) if rec.job.state == JobState::Resolving => Some((
                    rec.job.system.clone(),
                    rec.job.targets.clone(),
                    rec.job.emerge_args.clone(),
                    rec.job.auto_fix_conflicts,
                    rec.cancel.clone(),
                )),
                _ => None,
            }
        };
        let Some((sys_name, targets, emerge_args, auto_fix, cancel)) = claimed else {
            return;
        };
        if cancel.is_cancelled() || self.is_draining() {
            self.finish(id, JobState::Cancelled, None).await;
            return;
        }
        let sys = match self.resolve_system(&sys_name).await {
            Some(s) => s,
            None => {
                self.fail(id, format!("system '{sys_name}' is no longer available"))
                    .await;
                return;
            }
        };

        // 1. Resolve the plan, auto-unmerging blockers while allowed.
        let mut backend = RunnerBackend {
            runner: &self.inner.runner,
            sys: &sys,
            targets: &targets,
        };
        let resolved = match resolve_with_retry(&mut backend, auto_fix, MAX_RESOLVE_ATTEMPTS).await
        {
            Ok(r) => r,
            Err(ResolveError::Blocked(blocks)) => {
                let hint = if auto_fix {
                    format!("still blocked after {MAX_RESOLVE_ATTEMPTS} resolve attempts")
                } else {
                    "auto-fix disabled for this build".to_string()
                };
                self.fail(id, format!("blocked packages ({hint}): {blocks}"))
                    .await;
                return;
            }
            Err(ResolveError::Machine(e)) => {
                self.fail(id, format!("machine failed to start: {e}")).await;
                return;
            }
            Err(ResolveError::Backend(e)) => {
                self.fail(id, format!("emerge -p failed: {e}")).await;
                return;
            }
        };
        {
            let keep_lines = self.inner.config.server.keep_log_lines;
            let mut jobs = self.inner.jobs.write().await;
            if let Some(rec) = jobs.get_mut(&id) {
                rec.job.plan = resolved.plan.clone();
                rec.job.progress.total_packages = resolved.plan.len();
                rec.job.unmerged_blockers = resolved.unmerged.clone();
                // Surface auto-fix activity in the log/stream.
                if !resolved.unmerged.is_empty() {
                    let marker = format!(
                        ">>> auto-unmerged blockers: {}",
                        resolved.unmerged.join(", ")
                    );
                    rec.push_log(marker.clone(), keep_lines);
                    let _ = rec.bus.send(StreamEvent::Log { line: marker });
                    for log in &resolved.unmerge_logs {
                        for line in log.lines() {
                            rec.push_log(line.to_string(), keep_lines);
                            let _ = rec.bus.send(StreamEvent::Log {
                                line: line.to_string(),
                            });
                        }
                    }
                }
            }
        }
        if cancel.is_cancelled() || self.is_draining() {
            self.finish(id, JobState::Cancelled, None).await;
            return;
        }

        // 2. Estimate from history (own logs, else other systems').
        self.set_state(id, JobState::Estimating).await;
        self.refresh_history().await;
        {
            let history = self.inner.history.read().await;
            let mut jobs = self.inner.jobs.write().await;
            if let Some(rec) = jobs.get_mut(&id) {
                let estimates: Vec<PackageEstimate> =
                    history.estimate_many(&sys.name, &rec.job.plan);
                let total: u64 = estimates.iter().filter_map(|e| e.estimated_secs).sum();
                let any = estimates.iter().any(|e| e.estimated_secs.is_some());
                rec.job.estimates = estimates;
                rec.job.progress.estimated_total_secs = if any { Some(total) } else { None };
                rec.job.progress.estimated_remaining_secs = rec.job.progress.estimated_total_secs;
            }
        }
        if cancel.is_cancelled() || self.is_draining() {
            self.finish(id, JobState::Cancelled, None).await;
            return;
        }

        // 3. Build, streaming output.
        self.set_state(id, JobState::Building).await;
        let build_started = chrono::Utc::now();
        let mut full_args = emerge_args.clone();
        full_args.extend(targets.clone());
        let mut spawned = match self.inner.runner.spawn_emerge(&sys, &full_args).await {
            Ok(s) => s,
            Err(e) => {
                self.fail(id, format!("spawn emerge failed: {e}")).await;
                return;
            }
        };

        let keep_lines = self.inner.config.server.keep_log_lines;
        let mut live = LiveProgress::default();
        // Seed totals from the plan so "x of y" is right even before the
        // first Emerging line.
        {
            let jobs = self.inner.jobs.read().await;
            if let Some(rec) = jobs.get(&id) {
                live.total_packages = rec.job.plan.len();
            }
        }

        loop {
            tokio::select! {
                line = spawned.lines.recv() => {
                    let Some(line) = line else { break };
                    let mut jobs = self.inner.jobs.write().await;
                    if let Some(rec) = jobs.get_mut(&id) {
                        rec.push_log(line.clone(), keep_lines);
                        let _ = rec.bus.send(StreamEvent::Log { line: line.clone() });
                        if live.feed(&line) {
                            // Prefer the plan total (pretend is authoritative);
                            // fall back to emerge's own counter.
                            let total = if !rec.job.plan.is_empty() {
                                rec.job.plan.len()
                            } else {
                                live.total_packages
                            };
                            let index = if !rec.job.plan.is_empty() {
                                live.current_index.min(total.max(1))
                            } else {
                                live.current_index
                            };
                            rec.job.progress.current_package = live.current_package.clone();
                            rec.job.progress.current_index = index;
                            rec.job.progress.total_packages = total;
                            let elapsed = (chrono::Utc::now() - build_started).num_seconds().max(0) as u64;
                            rec.job.progress.elapsed_secs = elapsed;
                            if let Some(t) = rec.job.progress.estimated_total_secs {
                                rec.job.progress.estimated_remaining_secs =
                                    Some(t.saturating_sub(elapsed));
                            }
                            let _ = rec.bus.send(StreamEvent::Progress {
                                progress: rec.job.progress.clone(),
                            });
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    spawned.kill_tree().await;
                    let _ = spawned.child.wait().await;
                    self.finish(id, JobState::Cancelled, None).await;
                    return;
                }
            }
        }

        let status = match spawned.child.wait().await {
            Ok(s) => s,
            Err(e) => {
                self.fail(id, format!("waiting for emerge: {e}")).await;
                return;
            }
        };
        if cancel.is_cancelled() {
            self.finish(id, JobState::Cancelled, None).await;
        } else if status.success() {
            // Mark progress complete.
            {
                let mut jobs = self.inner.jobs.write().await;
                if let Some(rec) = jobs.get_mut(&id) {
                    rec.job.progress.current_index = rec.job.progress.total_packages;
                    rec.job.progress.estimated_remaining_secs = Some(0);
                }
            }
            self.finish(id, JobState::Done, status.code()).await;
        } else {
            let tail = {
                let jobs = self.inner.jobs.read().await;
                jobs.get(&id)
                    .map(|r| {
                        r.log
                            .iter()
                            .rev()
                            .take(5)
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            };
            {
                let mut jobs = self.inner.jobs.write().await;
                if let Some(rec) = jobs.get_mut(&id) {
                    rec.job.exit_code = status.code();
                    rec.job.error = Some(format!(
                        "emerge exited with status {}.\nLast output:\n{}",
                        status.code().unwrap_or(-1),
                        tail
                    ));
                }
            }
            self.finish(id, JobState::Failed, status.code()).await;
        }
    }
}

/// [`BlockResolver`] wired to the real runner for one build.
struct RunnerBackend<'a> {
    runner: &'a Runner,
    sys: &'a SystemConfig,
    targets: &'a [String],
}

/// Map runner failures to resolve errors, keeping machine-start failures
/// distinct so callers don't report them as emerge failures.
fn runner_resolve_err(e: RunnerError) -> ResolveError {
    match &e {
        RunnerError::MachineCtl(msg) | RunnerError::MachineTimeout(msg) => {
            ResolveError::Machine(msg.clone())
        }
        RunnerError::NoLeader(_) | RunnerError::NotRoot => ResolveError::Machine(e.to_string()),
        _ => ResolveError::Backend(e.to_string()),
    }
}

#[async_trait::async_trait]
impl BlockResolver for RunnerBackend<'_> {
    async fn pretend(&mut self) -> Result<String, ResolveError> {
        self.runner
            .run_pretend(self.sys, self.targets, &[])
            .await
            .map_err(runner_resolve_err)
    }

    async fn unmerge(&mut self, blockers: &[String]) -> Result<String, ResolveError> {
        let out = self
            .runner
            .run_unmerge(self.sys, blockers)
            .await
            .map_err(runner_resolve_err)?;
        Ok(out.combined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_failures_stay_distinct_from_emerge_failures() {
        assert_eq!(
            runner_resolve_err(RunnerError::MachineCtl("denied".into())),
            ResolveError::Machine("denied".into())
        );
        assert_eq!(
            runner_resolve_err(RunnerError::MachineTimeout("m".into())),
            ResolveError::Machine("m".into())
        );
        assert!(matches!(
            runner_resolve_err(RunnerError::NoLeader("m".into())),
            ResolveError::Machine(_)
        ));
        assert!(matches!(
            runner_resolve_err(RunnerError::NotRoot),
            ResolveError::Machine(_)
        ));
        assert!(matches!(
            runner_resolve_err(RunnerError::Spawn(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "nope"
            ))),
            ResolveError::Backend(_)
        ));
    }
}
