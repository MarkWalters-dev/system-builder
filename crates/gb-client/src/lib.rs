//! `gb-client` library: talk to the system-builder API, watch builds,
//! and run the follow-up local `emerge`.

pub mod config;
pub mod specs;
pub mod sse;
pub mod state;

use gb_types::{
    ApiError, BuildJob, BuildLog, BuildSummary, CreateBuildRequest, EstimateRequest,
    EstimateResponse, HistoryResponse, JobState, ProvisionRequest, ProvisionResponse, QueueStatus,
    StreamEvent, SystemActiveBuild, SystemInfo,
};
use std::process::ExitStatus;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("server error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("bad stream event: {0}")]
    Stream(String),
    #[error("{0}")]
    Usage(String),
}

/// Thin async client for the system-builder API.
#[derive(Debug, Clone)]
pub struct Client {
    base: String,
    http: reqwest::Client,
    /// Timeout-free client for [`Client::watch`]: reqwest applies the
    /// client timeout to the whole response body, which would kill any
    /// watch lasting longer than `timeout_secs`.
    stream_http: reqwest::Client,
}

impl Client {
    pub fn new(base_url: &str, timeout_secs: u64) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()?;
        let stream_http = reqwest::Client::builder().build()?;
        Ok(Self {
            base: base_url.trim_end_matches('/').to_string(),
            http,
            stream_http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, ClientError> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let message = resp
            .json::<ApiError>()
            .await
            .map(|e| e.error)
            .unwrap_or_else(|_| format!("http {status}"));
        Err(ClientError::Api { status, message })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let resp = self.http.get(format!("{}{path}", self.base)).send().await?;
        let resp = Self::check(resp).await?;
        Ok(resp.json::<T>().await?)
    }

    async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        Ok(resp.json::<T>().await?)
    }

    pub async fn submit(&self, req: &CreateBuildRequest) -> Result<BuildJob, ClientError> {
        self.post_json("/v1/builds", req).await
    }

    pub async fn get(&self, id: &str) -> Result<BuildJob, ClientError> {
        self.get_json(&format!("/v1/builds/{id}")).await
    }

    pub async fn list(
        &self,
        state: Option<JobState>,
        system: Option<&str>,
    ) -> Result<Vec<BuildSummary>, ClientError> {
        let mut path = String::from("/v1/builds?");
        if let Some(s) = state {
            let v = serde_json::to_value(s).expect("state serializes");
            path.push_str(&format!("state={}&", v.as_str().unwrap_or_default()));
        }
        if let Some(s) = system {
            path.push_str(&format!("system={s}&"));
        }
        self.get_json(&path).await
    }

    pub async fn queue(&self) -> Result<QueueStatus, ClientError> {
        self.get_json("/v1/queue").await
    }

    pub async fn active(&self) -> Result<Vec<BuildSummary>, ClientError> {
        self.get_json("/v1/builds/active").await
    }

    pub async fn systems(&self) -> Result<Vec<SystemInfo>, ClientError> {
        self.get_json("/v1/systems").await
    }

    pub async fn provision(
        &self,
        req: &ProvisionRequest,
    ) -> Result<ProvisionResponse, ClientError> {
        self.post_json("/v1/systems", req).await
    }

    pub async fn system_active(&self, system: &str) -> Result<SystemActiveBuild, ClientError> {
        self.get_json(&format!("/v1/systems/{system}/active")).await
    }

    pub async fn cancel(&self, id: &str) -> Result<BuildJob, ClientError> {
        let resp = self
            .http
            .delete(format!("{}/v1/builds/{id}", self.base))
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        Ok(resp.json::<BuildJob>().await?)
    }

    pub async fn log(&self, id: &str, tail: Option<usize>) -> Result<BuildLog, ClientError> {
        let path = match tail {
            Some(n) => format!("/v1/builds/{id}/log?tail={n}"),
            None => format!("/v1/builds/{id}/log"),
        };
        self.get_json(&path).await
    }

    pub async fn estimate(&self, req: &EstimateRequest) -> Result<EstimateResponse, ClientError> {
        self.post_json("/v1/estimates", req).await
    }

    pub async fn history(
        &self,
        system: &str,
        package: &str,
    ) -> Result<HistoryResponse, ClientError> {
        self.get_json(&format!("/v1/history?system={system}&package={package}"))
            .await
    }

    /// Best-effort ntfy ping (`POST {base}/{topic}`). Callers treat
    /// failure as a warning, never fatal. Uses a short dedicated
    /// timeout so a dead notification server can't stall the command.
    pub async fn notify(
        &self,
        base: &str,
        topic: &str,
        title: &str,
        message: &str,
        priority: Option<&str>,
    ) -> Result<(), ClientError> {
        let url = format!("{}/{}", base.trim_end_matches('/'), topic.trim());
        let mut req = self
            .http
            .post(url)
            .header("Title", title)
            .body(message.to_string())
            .timeout(Duration::from_secs(15));
        if let Some(p) = priority {
            req = req.header("Priority", p);
        }
        let resp = req.send().await?;
        Self::check(resp).await?;
        Ok(())
    }

    /// Stream a build's events until it reaches a terminal state.
    ///
    /// Calls `on_event` for every event (including the final `Done`) and
    /// returns the terminal state plus exit code.
    pub async fn watch(
        &self,
        id: &str,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<(JobState, Option<i32>), ClientError> {
        let resp = self
            .stream_http
            .get(format!("{}/v1/builds/{id}/stream", self.base))
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        let mut stream = resp.bytes_stream();
        let mut parser = sse::Parser::new();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ClientError::Stream(e.to_string()))?;
            for raw in parser.feed(&chunk) {
                if raw.data.is_empty() {
                    continue;
                }
                let ev: StreamEvent = serde_json::from_str(&raw.data)
                    .map_err(|e| ClientError::Stream(format!("{e}: {}", raw.data)))?;
                let done = match &ev {
                    StreamEvent::Done { state, exit_code } => Some((*state, *exit_code)),
                    _ => None,
                };
                on_event(ev);
                if let Some(final_) = done {
                    return Ok(final_);
                }
            }
        }
        Err(ClientError::Stream(
            "server closed the stream before the build finished (it may still be running; re-watch to resume)".to_string(),
        ))
    }

    /// Poll a build's status until it reaches a terminal state.
    ///
    /// Same contract as [`Client::watch`] (events through `on_event`,
    /// terminal state plus exit code back), but over plain status fetches:
    /// the CLI falls back to this when the event stream itself breaks, so
    /// a network blip mid-build doesn't lose the watch. `Status` is
    /// emitted on state changes, `Progress` on every poll, `Done` once.
    pub async fn poll_until_done(
        &self,
        id: &str,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<(JobState, Option<i32>), ClientError> {
        let mut last_state: Option<JobState> = None;
        loop {
            let job = self.get(id).await?;
            if last_state != Some(job.state) {
                last_state = Some(job.state);
                on_event(StreamEvent::Status { state: job.state });
            }
            on_event(StreamEvent::Progress {
                progress: job.progress.clone(),
            });
            if job.state.is_terminal() {
                let done = StreamEvent::Done {
                    state: job.state,
                    exit_code: job.exit_code,
                };
                on_event(done);
                return Ok((job.state, job.exit_code));
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

/// Choose the target for `cancel` with no id: your own last submitted
/// build while it is still queued or running, else the most recently
/// submitted queued build, else the most recently submitted running
/// build, else nothing to cancel.
///
/// `saved_state` is `None` when the recorded id is unknown to the server
/// (it restarted, or pruned the job) — that falls through to the queue,
/// exactly like an already-finished recording.
pub fn select_cancel_target(
    saved_id: Option<uuid::Uuid>,
    saved_state: Option<JobState>,
    queued: &[BuildSummary],
    active: &[BuildSummary],
) -> Option<uuid::Uuid> {
    if let Some(id) = saved_id
        && matches!(saved_state, Some(s) if s == JobState::Queued || s.is_active())
    {
        return Some(id);
    }
    newest(queued).or_else(|| newest(active)).map(|j| j.id)
}

/// Newest summary by submission time, if any.
fn newest(jobs: &[BuildSummary]) -> Option<&BuildSummary> {
    jobs.iter().max_by_key(|j| j.created_at)
}

/// Run the follow-up local `emerge` on this machine.
///
/// Stdio is inherited so the user sees emerge's normal interactive output.
/// When `sudo` is true the command runs as `sudo <bin> ...`.
pub async fn run_local_emerge(
    bin: &str,
    sudo: bool,
    args: &[String],
    targets: &[String],
) -> Result<ExitStatus, std::io::Error> {
    let mut cmd = if sudo {
        let mut c = tokio::process::Command::new("sudo");
        c.arg(bin);
        c
    } else {
        tokio::process::Command::new(bin)
    };
    cmd.args(args).args(targets);
    cmd.status().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn summary(id: uuid::Uuid, state: JobState, secs: i64) -> BuildSummary {
        BuildSummary {
            id,
            system: "laptop".to_string(),
            targets: vec!["@system".to_string()],
            state,
            queue_position: 0,
            progress: Default::default(),
            created_at: Utc.timestamp_opt(secs, 0).unwrap(),
        }
    }

    #[test]
    fn cancel_target_prefers_own_last_build_while_cancellable() {
        let mine = uuid::Uuid::new_v4();
        let other = summary(uuid::Uuid::new_v4(), JobState::Queued, 200);
        for state in [
            JobState::Queued,
            JobState::Resolving,
            JobState::Estimating,
            JobState::Building,
        ] {
            assert_eq!(
                select_cancel_target(Some(mine), Some(state), std::slice::from_ref(&other), &[]),
                Some(mine),
                "for state {state:?}"
            );
        }
    }

    #[test]
    fn cancel_target_falls_back_to_newest_queued_then_active() {
        let mine = uuid::Uuid::new_v4();
        let old_queued = summary(uuid::Uuid::new_v4(), JobState::Queued, 100);
        let new_queued = summary(uuid::Uuid::new_v4(), JobState::Queued, 300);
        let running = summary(uuid::Uuid::new_v4(), JobState::Building, 400);
        // Finished, cancelled, failed, or unknown recordings all fall through.
        for saved_state in [
            None,
            Some(JobState::Done),
            Some(JobState::Failed),
            Some(JobState::Cancelled),
        ] {
            assert_eq!(
                select_cancel_target(
                    Some(mine),
                    saved_state,
                    &[old_queued.clone(), new_queued.clone()],
                    std::slice::from_ref(&running),
                ),
                Some(new_queued.id),
                "for saved state {saved_state:?}"
            );
        }
        // No recording at all behaves the same.
        assert_eq!(
            select_cancel_target(
                None,
                None,
                &[old_queued.clone(), new_queued.clone()],
                std::slice::from_ref(&running),
            ),
            Some(new_queued.id)
        );
        // Nothing queued: newest running build.
        assert_eq!(
            select_cancel_target(None, None, &[], std::slice::from_ref(&running)),
            Some(running.id)
        );
        // Nothing anywhere: nothing to cancel.
        assert_eq!(select_cancel_target(None, None, &[], &[]), None);
        assert_eq!(
            select_cancel_target(Some(mine), Some(JobState::Done), &[], &[]),
            None
        );
    }
}
