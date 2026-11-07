//! `gb-client`: run on a client machine to start a remote build via the
//! system-builder API, watch it, then run a local `emerge` on success.
//!
//! Exit codes: 0 ok · 1 API/usage error · 2 remote build failed or cancelled
//! · 3 local emerge failed.

use clap::{Parser, Subcommand};
use gb_client::Client;
use gb_types::{BuildJob, JobState, StreamEvent};
use std::path::{Path, PathBuf};

const EXIT_API_ERROR: i32 = 1;
const EXIT_BUILD_FAILED: i32 = 2;
const EXIT_LOCAL_FAILED: i32 = 3;

#[derive(Debug, Parser)]
#[command(
    name = "gb-client",
    version,
    about = "Client for the Gentoo system-builder API"
)]
struct Cli {
    /// Config file. Defaults to $HOME/.config/gb-client/config.toml, which is
    /// created with commented defaults on first run when missing.
    ///
    /// Values resolve as: flag > environment > config file > built-in default.
    /// The server is required (it has no default).
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,
    /// API base URL (required unless set in the config file).
    #[arg(long, env = "GB_SERVER", global = true)]
    server: Option<String>,
    /// Timeout in seconds for single API calls (streaming is unbounded).
    #[arg(long, env = "GB_TIMEOUT", global = true)]
    timeout: Option<u64>,
    /// Print machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Submit a remote build, watch it, then run a local emerge on success.
    Build {
        /// System to build for (server config name, e.g. `laptop`).
        #[arg(long, env = "GB_SYSTEM")]
        system: Option<String>,
        /// What to build; repeatable. Atoms (`sys-apps/foo`) or sets (`@system`).
        #[arg(required_unless_present = "target_flag")]
        target: Vec<String>,
        /// Same as the positional targets; repeatable. Merged with them.
        #[arg(long = "target")]
        target_flag: Vec<String>,
        /// Extra args for the remote emerge; repeatable. Replaces the
        /// server default (`--update --newuse --deep --verbose`) entirely.
        #[arg(long)]
        emerge_arg: Vec<String>,
        /// Do not auto-unmerge blocking packages; fail on conflicts instead.
        #[arg(long)]
        no_auto_fix: bool,
        /// Submit and exit (prints the build id); implies --no-local.
        #[arg(long)]
        no_watch: bool,
        /// Watch but skip the local emerge step.
        #[arg(long)]
        no_local: bool,
        /// Local emerge targets; repeatable. Defaults to the remote targets.
        #[arg(long)]
        local_target: Vec<String>,
        /// Extra args for the local emerge; repeatable. Defaults to
        /// `--update --newuse --deep --verbose`; any explicit arg
        /// replaces those defaults entirely.
        #[arg(long, default_values_t = [String::from("--update"), String::from("--newuse"), String::from("--deep"), String::from("--verbose")])]
        local_arg: Vec<String>,
        /// Local emerge binary.
        #[arg(long, default_value = "emerge")]
        local_bin: String,
        /// Run the local emerge via sudo.
        #[arg(long)]
        sudo: bool,
    },
    /// Stream an existing build until it finishes.
    Watch {
        /// Build id (uuid). Defaults to the latest build.
        id: Option<String>,
    },
    /// Show one build's status.
    Status {
        /// Build id (uuid). Defaults to the latest build.
        id: Option<String>,
    },
    /// List builds (newest first).
    List {
        /// Filter by state: queued, resolving, estimating, building,
        /// done, failed, cancelled.
        #[arg(long)]
        state: Option<String>,
        /// Filter by system.
        #[arg(long)]
        system: Option<String>,
    },
    /// Show the queue: slots, running and queued builds.
    Queue,
    /// Show currently running builds.
    Active,
    /// Show known systems and whether each is idle or building.
    Systems,
    /// Show the active (or next queued) build for one system.
    SystemActive {
        /// System name.
        system: String,
    },
    /// Cancel a queued build or kill a running one.
    Cancel {
        /// Build id (uuid). Defaults to your last submitted build while it
        /// is still queued or running, else the most recently submitted
        /// queued build, else the most recently submitted running build.
        id: Option<String>,
    },
    /// Show a build's buffered emerge output.
    Log {
        /// Build id (uuid). Defaults to the latest build.
        id: Option<String>,
        /// Show only the last N lines.
        #[arg(long)]
        tail: Option<usize>,
    },
    /// Estimate build times from history.
    Estimate {
        /// System to estimate for.
        #[arg(long, env = "GB_SYSTEM")]
        system: Option<String>,
        /// Explicit category/packagename entries; repeatable.
        #[arg(long)]
        package: Vec<String>,
        /// Emerge targets to resolve via `emerge -p` first; repeatable.
        #[arg(long)]
        target: Vec<String>,
    },
    /// Show build-time history for one package.
    History {
        /// System whose logs to prefer.
        #[arg(long, env = "GB_SYSTEM")]
        system: Option<String>,
        /// Package (`category/name`, version optional).
        #[arg(long)]
        package: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(async_main(cli)),
        Err(e) => {
            eprintln!("error: cannot start runtime: {e}");
            EXIT_API_ERROR
        }
    };
    std::process::exit(code);
}

async fn async_main(cli: Cli) -> i32 {
    // Config file first: explicit --config, else the per-user file. A missing
    // file is scaffolded with commented defaults; the command then proceeds
    // with flag > environment > config file > built-in default (except the
    // server, which has no default and is required).
    let file = match load_file_config(cli.config.as_deref()) {
        Ok((f, created)) => {
            if let Some(path) = created {
                eprintln!(
                    "created {} with commented defaults; edit it to skip these flags.",
                    path.display()
                );
            }
            f
        }
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_API_ERROR;
        }
    };
    // clap already merged flag + environment into these Options.
    // The server has no built-in default: guessing localhost just fails
    // confusingly on real clients, so a missing server is a usage error.
    let server = cli
        .server
        .or(file.server.clone())
        .filter(|s| !s.trim().is_empty());
    let Some(server) = server else {
        return need("--server (or set server in the config file)");
    };
    let timeout = cli
        .timeout
        .or(file.timeout)
        .unwrap_or(gb_client::config::DEFAULT_TIMEOUT);
    let client = match Client::new(&server, timeout) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_API_ERROR;
        }
    };
    let json = cli.json;
    let notify = notify_target(&file);
    match cli.command {
        Commands::Build {
            system,
            target,
            target_flag,
            emerge_arg,
            no_auto_fix,
            no_watch,
            no_local,
            local_target,
            local_arg,
            local_bin,
            sudo,
        } => {
            let system = match system.or(file.system.clone()) {
                Some(s) => s,
                None => return need("--system (or set system in the config file)"),
            };
            let mut targets = target;
            targets.extend(target_flag);
            cmd_build(
                &client,
                json,
                system,
                targets,
                emerge_arg,
                !no_auto_fix,
                no_watch,
                no_local,
                local_target,
                local_arg,
                local_bin,
                sudo,
                notify.clone(),
            )
            .await
        }
        Commands::Watch { id } => match resolve_id(&client, id).await {
            Ok(full) => cmd_watch(&client, json, &full, notify.clone()).await,
            Err(e) => api_err(e),
        },
        Commands::Status { id } => {
            let id = match resolve_id(&client, id).await {
                Ok(full) => full,
                Err(e) => return api_err(e),
            };
            match client.get(&id).await {
                Ok(job) => {
                    if json {
                        print_json(&job);
                    } else {
                        print_job(&job);
                    }
                    0
                }
                Err(e) => api_err(e),
            }
        }
        Commands::List { state, system } => {
            let state = match state.map(|s| parse_state(&s)).transpose() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    return EXIT_API_ERROR;
                }
            };
            match client.list(state, system.as_deref()).await {
                Ok(jobs) => {
                    if json {
                        print_json(&jobs);
                    } else {
                        for j in &jobs {
                            println!(
                                "{} {} {} {} [{}]",
                                j.id,
                                j.system,
                                state_str(j.state),
                                j.targets.join(","),
                                progress_str(&j.progress)
                            );
                        }
                    }
                    0
                }
                Err(e) => api_err(e),
            }
        }
        Commands::Queue => match client.queue().await {
            Ok(q) => {
                if json {
                    print_json(&q);
                } else {
                    println!(
                        "slots: {}/{}  queued: {}",
                        q.running,
                        q.max_concurrent_builds,
                        q.queued.len()
                    );
                    for b in &q.active {
                        println!(
                            "  running {} {} [{}]",
                            b.id,
                            b.system,
                            progress_str(&b.progress)
                        );
                    }
                    for b in &q.queued {
                        println!(
                            "  queued#{} {} {} {}",
                            b.queue_position,
                            b.id,
                            b.system,
                            b.targets.join(",")
                        );
                    }
                }
                0
            }
            Err(e) => api_err(e),
        },
        Commands::Active => match client.active().await {
            Ok(jobs) => {
                if json {
                    print_json(&jobs);
                } else if jobs.is_empty() {
                    println!("no active builds");
                } else {
                    for j in &jobs {
                        println!(
                            "{} {} {} [{}]",
                            j.id,
                            j.system,
                            state_str(j.state),
                            progress_str(&j.progress)
                        );
                    }
                }
                0
            }
            Err(e) => api_err(e),
        },
        Commands::Systems => match client.systems().await {
            Ok(systems) => {
                if json {
                    print_json(&systems);
                } else {
                    for s in &systems {
                        println!(
                            "{} {} {} {:?} {}",
                            s.name,
                            if s.nspawn.is_some() { "nspawn" } else { "host" },
                            s.nspawn.as_deref().unwrap_or("-"),
                            s.status,
                            s.active_build_id
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| "-".into())
                        );
                    }
                }
                0
            }
            Err(e) => api_err(e),
        },
        Commands::SystemActive { system } => match client.system_active(&system).await {
            Ok(a) => {
                if json {
                    print_json(&a);
                } else if let Some(b) = a.build {
                    println!(
                        "{} active={} [{}]",
                        a.system,
                        a.active,
                        progress_str(&b.progress)
                    );
                    if let Some(pkg) = b.progress.current_package {
                        println!("  building: {pkg}");
                    }
                } else {
                    println!("{}: idle, nothing queued", a.system);
                }
                0
            }
            Err(e) => api_err(e),
        },
        Commands::Cancel { id } => {
            // An explicit id cancels exactly that build; an omitted id
            // targets your own last submission, else the newest queued
            // (or running) build. Never the newest build overall: on a
            // shared server that could be someone else's finished job.
            let id = match id {
                Some(s) => match resolve_id(&client, Some(s)).await {
                    Ok(full) => full,
                    Err(e) => return api_err(e),
                },
                None => match resolve_cancel_target(&client).await {
                    Ok(full) => full,
                    Err(e) => return api_err(e),
                },
            };
            match client.cancel(&id).await {
                Ok(job) => {
                    if json {
                        print_json(&job);
                    } else {
                        println!("{} -> {}", job.id, state_str(job.state));
                    }
                    0
                }
                Err(e) => api_err(e),
            }
        }
        Commands::Log { id, tail } => {
            let id = match resolve_id(&client, id).await {
                Ok(full) => full,
                Err(e) => return api_err(e),
            };
            match client.log(&id, tail).await {
                Ok(log) => {
                    if json {
                        print_json(&log);
                    } else {
                        if log.truncated {
                            println!("... (earliest lines truncated by server) ...");
                        }
                        println!("{}", log.log);
                    }
                    0
                }
                Err(e) => api_err(e),
            }
        }
        Commands::Estimate {
            system,
            package,
            target,
        } => {
            let system = match system.or(file.system.clone()) {
                Some(s) => s,
                None => return need("--system (or set system in the config file)"),
            };
            let req = gb_types::EstimateRequest {
                system,
                packages: package,
                targets: target,
            };
            match client.estimate(&req).await {
                Ok(resp) => {
                    if json {
                        print_json(&resp);
                    } else {
                        for p in &resp.packages {
                            match p.estimated_secs {
                                Some(s) => println!(
                                    "{} ~{} (avg {}s, {} samples{})",
                                    p.package,
                                    fmt_dur(s),
                                    p.average_secs.unwrap_or(s),
                                    p.samples,
                                    p.source_system
                                        .as_deref()
                                        .map(|src| format!(
                                            " from {src}{}",
                                            if p.fallback { " [fallback]" } else { "" }
                                        ))
                                        .unwrap_or_default()
                                ),
                                None => println!("{} no history", p.package),
                            }
                        }
                        if let Some(t) = resp.total_estimated_secs {
                            println!(
                                "total ~{} ({} with history, {} without)",
                                fmt_dur(t),
                                resp.packages_with_history,
                                resp.packages_without_history
                            );
                        }
                    }
                    0
                }
                Err(e) => api_err(e),
            }
        }
        Commands::History { system, package } => {
            let system = match system.or(file.system.clone()) {
                Some(s) => s,
                None => return need("--system (or set system in the config file)"),
            };
            match client.history(&system, &package).await {
                Ok(h) => {
                    if json {
                        print_json(&h);
                    } else {
                        match h.median_secs {
                            Some(m) => println!(
                                "{} on {}: median {} avg {} min {} max {} ({} samples{})",
                                h.package,
                                h.source_system.as_deref().unwrap_or(&h.system),
                                fmt_dur(m),
                                fmt_dur(h.average_secs.unwrap_or(m)),
                                fmt_dur(h.min_secs.unwrap_or(m)),
                                fmt_dur(h.max_secs.unwrap_or(m)),
                                h.samples,
                                if h.fallback { " [fallback]" } else { "" }
                            ),
                            None => println!("{}: no history on any system", h.package),
                        }
                    }
                    0
                }
                Err(e) => api_err(e),
            }
        }
    }
}

/// Load the client config file: explicit `--config`, else the per-user file.
/// A missing file is scaffolded with commented defaults. Returns the config
/// plus the path when a file was just created (for a first-run note).
fn load_file_config(
    flag: Option<&Path>,
) -> Result<(gb_client::config::FileConfig, Option<PathBuf>), gb_client::config::ConfigError> {
    let path = match flag {
        Some(p) => Some(p.to_path_buf()),
        None => gb_client::config::default_path(),
    };
    match path {
        Some(path) => gb_client::config::load_or_scaffold(&path)
            .map(|(cfg, created)| (cfg, created.then(|| path.clone()))),
        // No home known: proceed with built-in defaults, nothing to scaffold.
        None => Ok((gb_client::config::FileConfig::default(), None)),
    }
}

fn need(what: &str) -> i32 {
    eprintln!("error: missing required {what}");
    EXIT_API_ERROR
}

/// Ntfy destination resolved from the config file. `None` disables
/// notifications.
#[derive(Debug, Clone)]
struct NotifyTarget {
    base: String,
    topic: String,
}

/// Resolve notification settings: a blank or missing topic disables
/// notifications, a blank or missing server falls back to ntfy.sh.
fn notify_target(cfg: &gb_client::config::FileConfig) -> Option<NotifyTarget> {
    let topic = cfg.notify_topic.clone().filter(|t| !t.trim().is_empty())?;
    let base = cfg
        .notify_server
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| gb_client::config::DEFAULT_NOTIFY_SERVER.to_string());
    Some(NotifyTarget {
        base,
        topic: topic.trim().to_string(),
    })
}

/// Resolve an id argument: an explicit id must be a full UUID (short
/// prefixes are rejected with a hint instead of a server 400), while an
/// omitted id means the latest build.
async fn resolve_id(client: &Client, id: Option<String>) -> Result<String, gb_client::ClientError> {
    use gb_client::ClientError;
    match id {
        Some(s) => {
            uuid::Uuid::parse_str(&s).map_err(|_| {
                ClientError::Usage(format!(
                    "invalid build id '{s}' (want the full id from 'list', or omit it for the latest build)"
                ))
            })?;
            Ok(s)
        }
        None => {
            let jobs = client.list(None, None).await?;
            jobs.into_iter()
                .next()
                .map(|j| j.id.to_string())
                .ok_or_else(|| ClientError::Usage("no builds yet".to_string()))
        }
    }
}

/// Resolve `cancel` with no id: your last submitted build while it is still
/// cancellable, else the newest queued (or running) build. Names the
/// recorded id on stderr when falling past it, so the choice is visible.
async fn resolve_cancel_target(client: &Client) -> Result<String, gb_client::ClientError> {
    use gb_client::ClientError;
    let saved = gb_client::state::read_last_build();
    let mut saved_state = None;
    if let Some(id) = saved
        && let Ok(job) = client.get(&id.to_string()).await
    {
        saved_state = Some(job.state);
    }
    let queue = client.queue().await?;
    match gb_client::select_cancel_target(saved, saved_state, &queue.queued, &queue.active) {
        Some(id) => {
            if Some(id) != saved
                && let Some(was) = saved
            {
                eprintln!(
                    "note: last submitted build {was} is no longer cancellable; cancelling {id} instead"
                );
            }
            Ok(id.to_string())
        }
        None => {
            if let Some(was) = saved {
                eprintln!(
                    "note: last submitted build {was} is no longer cancellable; nothing else queued or running"
                );
            }
            Err(ClientError::Usage(
                "nothing to cancel (no queued or running builds)".to_string(),
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_build(
    client: &Client,
    json: bool,
    system: String,
    targets: Vec<String>,
    emerge_args: Vec<String>,
    auto_fix: bool,
    no_watch: bool,
    no_local: bool,
    mut local_targets: Vec<String>,
    local_args: Vec<String>,
    local_bin: String,
    sudo: bool,
    notify: Option<NotifyTarget>,
) -> i32 {
    let req = gb_types::CreateBuildRequest {
        system,
        targets: targets.clone(),
        emerge_args,
        auto_fix_conflicts: Some(auto_fix),
    };
    let job = match client.submit(&req).await {
        Ok(j) => j,
        Err(e) => return api_err(e),
    };
    // Remember the submission so bare `cancel` targets this build.
    gb_client::state::record_last_build(job.id);
    let id = job.id.to_string();
    if no_watch {
        if json {
            print_json(&job);
        } else {
            eprintln!(
                "submitted build {id} ({}), state: {}",
                job.system,
                state_str(job.state)
            );
            println!("{id}");
        }
        return 0;
    }
    if !json {
        eprintln!("submitted build {id} ({}); watching…", job.system);
    }
    let code = cmd_watch(client, json, &id, notify.clone()).await;
    if code != 0 {
        return code;
    }
    if no_local {
        return 0;
    }
    if local_targets.is_empty() {
        local_targets = targets;
    }
    if !json {
        eprintln!(
            "remote build done; running local: {}{} {} {}",
            if sudo { "sudo " } else { "" },
            local_bin,
            local_args.join(" "),
            local_targets.join(" ")
        );
    }
    match gb_client::run_local_emerge(&local_bin, sudo, &local_args, &local_targets).await {
        Ok(status) if status.success() => {
            if !json {
                eprintln!("local emerge succeeded");
            }
            if let Some(n) = &notify {
                send_notify(
                    client,
                    n,
                    "system-builder: local emerge done",
                    &local_done_message(true, None, &local_targets),
                    None,
                )
                .await;
            }
            0
        }
        Ok(status) => {
            eprintln!("local emerge failed with status: {status}");
            if let Some(n) = &notify {
                send_notify(
                    client,
                    n,
                    "system-builder: local emerge failed",
                    &local_done_message(false, status.code(), &local_targets),
                    Some("high"),
                )
                .await;
            }
            EXIT_LOCAL_FAILED
        }
        Err(e) => {
            eprintln!("error: cannot run local emerge: {e}");
            if let Some(n) = &notify {
                let mut msg = local_done_message(false, None, &local_targets);
                msg.push_str(&format!("\n{}", truncate(&e.to_string(), 200)));
                send_notify(
                    client,
                    n,
                    "system-builder: local emerge failed",
                    &msg,
                    Some("high"),
                )
                .await;
            }
            EXIT_LOCAL_FAILED
        }
    }
}

/// Watch a build to completion. Returns a process exit code.
///
/// The event stream is primary; when the stream itself breaks mid-build
/// (network blip, server restart), watching continues by polling status
/// so a long build's watch (and its follow-up local emerge) survives.
async fn cmd_watch(client: &Client, json: bool, id: &str, notify: Option<NotifyTarget>) -> i32 {
    let render = |ev: StreamEvent| render_event(json, &ev);
    match client.watch(id, render).await {
        Ok((state, exit_code)) => finish_watch(client, json, id, state, exit_code, notify).await,
        Err(gb_client::ClientError::Stream(msg)) => {
            if !json {
                eprintln!("stream failed ({msg}); polling status until it finishes…");
            }
            let render = |ev: StreamEvent| render_event(json, &ev);
            match client.poll_until_done(id, render).await {
                Ok((state, exit_code)) => {
                    finish_watch(client, json, id, state, exit_code, notify).await
                }
                Err(e) => api_err(e),
            }
        }
        Err(e) => api_err(e),
    }
}

/// Exit code + failure detail for a finished watch, shared by the stream
/// and poll paths.
async fn finish_watch(
    client: &Client,
    json: bool,
    id: &str,
    state: JobState,
    exit_code: Option<i32>,
    notify: Option<NotifyTarget>,
) -> i32 {
    if state == JobState::Done {
        return 0;
    }
    // Fetch the failure detail for a useful message.
    let job = client.get(id).await.ok();
    let detail = job
        .as_ref()
        .and_then(|j| j.error.clone())
        .unwrap_or_default();
    if !json {
        eprintln!("remote build {id} finished: {state:?} (exit {exit_code:?})");
        if !detail.is_empty() {
            eprintln!("{detail}");
        }
    }
    if let (Some(n), Some(job)) = (&notify, &job) {
        let mut msg = format!(
            "system-builder: remote build for {} {} ({})",
            job.system,
            state_str(job.state),
            job.targets.join(" ")
        );
        if let Some(first) = detail
            .lines()
            .next()
            .map(str::trim)
            .filter(|l| !l.is_empty())
        {
            msg.push('\n');
            msg.push_str(&truncate(first, 200));
        }
        send_notify(
            client,
            n,
            &format!("system-builder: remote build {}", state_str(job.state)),
            &msg,
            Some("high"),
        )
        .await;
    }
    EXIT_BUILD_FAILED
}

/// Best-effort notification: failures warn on stderr, never the exit code.
async fn send_notify(
    client: &Client,
    target: &NotifyTarget,
    title: &str,
    message: &str,
    priority: Option<&str>,
) {
    if let Err(e) = client
        .notify(&target.base, &target.topic, title, message, priority)
        .await
    {
        eprintln!("warning: notification failed: {e}");
    }
}

fn local_done_message(ok: bool, code: Option<i32>, targets: &[String]) -> String {
    let where_ = local_hostname()
        .map(|h| format!(" on {h}"))
        .unwrap_or_default();
    let what = if targets.is_empty() {
        String::new()
    } else {
        format!(": {}", targets.join(" "))
    };
    match (ok, code) {
        (true, _) => format!("system-builder: local emerge{where_} succeeded{what}"),
        (false, Some(c)) => {
            format!("system-builder: local emerge{where_} failed (exit {c}){what}")
        }
        (false, None) => format!("system-builder: local emerge{where_} failed{what}"),
    }
}

/// Best-effort short hostname for notification messages.
fn local_hostname() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let short = String::from_utf8_lossy(&out.stdout)
        .split('.')
        .next()?
        .trim()
        .to_string();
    (!short.is_empty()).then_some(short)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

fn render_event(json: bool, ev: &StreamEvent) {
    if json {
        println!("{}", serde_json::to_string(ev).unwrap_or_default());
        return;
    }
    match ev {
        StreamEvent::Status { state } => eprintln!(">>> state: {state:?}"),
        StreamEvent::Log { line } => println!("{line}"),
        StreamEvent::Progress { progress } => {
            eprintln!(">>> [{}]", progress_str(progress));
        }
        StreamEvent::Done { state, exit_code } => {
            eprintln!(">>> done: {state:?} (exit {exit_code:?})");
        }
    }
}

fn api_err(e: gb_client::ClientError) -> i32 {
    eprintln!("error: {e}");
    EXIT_API_ERROR
}

fn print_json<T: serde::Serialize>(v: &T) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn state_str(s: JobState) -> &'static str {
    match s {
        JobState::Queued => "queued",
        JobState::Resolving => "resolving",
        JobState::Estimating => "estimating",
        JobState::Building => "building",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

fn parse_state(s: &str) -> Result<JobState, String> {
    serde_json::from_str::<JobState>(&format!("\"{s}\""))
        .map_err(|_| format!("invalid state '{s}' (try: queued, building, done, failed, ...)"))
}

fn progress_str(p: &gb_types::BuildProgress) -> String {
    let mut s = format!("{}/{}", p.current_index, p.total_packages);
    if let Some(pkg) = &p.current_package {
        s.push_str(&format!(" {pkg}"));
    }
    if p.elapsed_secs > 0 {
        s.push_str(&format!(" elapsed {}s", p.elapsed_secs));
    }
    if let Some(r) = p.estimated_remaining_secs {
        s.push_str(&format!(" ETA {}s", r));
    }
    s
}

fn fmt_dur(secs: u64) -> String {
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 5400 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn print_job(job: &BuildJob) {
    println!("id:       {}", job.id);
    println!("system:   {}", job.system);
    println!("state:    {}", state_str(job.state));
    println!("targets:  {}", job.targets.join(" "));
    println!("auto-fix: {}", job.auto_fix_conflicts);
    if !job.unmerged_blockers.is_empty() {
        println!("unmerged: {}", job.unmerged_blockers.join(", "));
    }
    if job.queue_position > 0 {
        println!("queued:   #{}", job.queue_position);
    }
    println!("progress: [{}]", progress_str(&job.progress));
    if !job.plan.is_empty() {
        println!("plan:     {} packages", job.plan.len());
    }
    if let Some(t) = job.progress.estimated_total_secs {
        println!("estimate: ~{}", fmt_dur(t));
    }
    if let Some(e) = &job.error {
        println!("error:    {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn build_cmd(args: &[&str]) -> Result<Commands, clap::Error> {
        let mut full = vec!["gb-client"];
        full.extend(args.iter());
        Cli::try_parse_from(full).map(|cli| cli.command)
    }

    fn targets_of(cmd: &Commands) -> (Vec<String>, Vec<String>) {
        match cmd {
            Commands::Build {
                target,
                target_flag,
                ..
            } => (target.clone(), target_flag.clone()),
            other => panic!("expected build, got {other:?}"),
        }
    }

    #[test]
    fn build_targets_are_positional() {
        let cmd = build_cmd(&["build", "@system", "sys-apps/foo"]).unwrap();
        let (target, flag) = targets_of(&cmd);
        assert_eq!(target, vec!["@system", "sys-apps/foo"]);
        assert!(flag.is_empty());
    }

    #[test]
    fn build_target_flag_still_accepted() {
        let cmd = build_cmd(&["build", "--target", "@system"]).unwrap();
        let (target, flag) = targets_of(&cmd);
        assert!(target.is_empty());
        assert_eq!(flag, vec!["@system"]);
    }

    #[test]
    fn build_with_no_targets_is_rejected() {
        assert!(build_cmd(&["build"]).is_err());
    }

    #[test]
    fn status_and_watch_default_to_latest_build() {
        for cmd in ["status", "watch", "log"] {
            let cli = Cli::try_parse_from(["gb-client", cmd]).unwrap();
            let id = match cli.command {
                Commands::Status { id } => id,
                Commands::Watch { id } => id,
                Commands::Log { id, .. } => id,
                other => panic!("expected {cmd}, got {other:?}"),
            };
            assert_eq!(id, None, "for {cmd}");
        }
        let cli = Cli::try_parse_from(["gb-client", "status", "abc"]).unwrap();
        assert!(
            matches!(cli.command, Commands::Status { id: Some(_) }),
            "explicit id still parses"
        );
    }

    #[test]
    fn notify_target_defaults_and_blanks() {
        use gb_client::config::{DEFAULT_NOTIFY_SERVER, FileConfig};
        assert!(notify_target(&FileConfig::default()).is_none());
        let blank = FileConfig {
            notify_topic: Some("  ".into()),
            ..Default::default()
        };
        assert!(notify_target(&blank).is_none());
        let cfg = FileConfig {
            notify_topic: Some("my-topic".into()),
            ..Default::default()
        };
        let t = notify_target(&cfg).unwrap();
        assert_eq!(t.topic, "my-topic");
        assert_eq!(t.base, DEFAULT_NOTIFY_SERVER);
        let cfg = FileConfig {
            notify_topic: Some(" t ".into()),
            notify_server: Some("https://n.example".into()),
            ..Default::default()
        };
        let t = notify_target(&cfg).unwrap();
        assert_eq!(t.topic, "t");
        assert_eq!(t.base, "https://n.example");
        // Blank server falls back to the default too.
        let cfg = FileConfig {
            notify_topic: Some("t".into()),
            notify_server: Some(" ".into()),
            ..Default::default()
        };
        assert_eq!(notify_target(&cfg).unwrap().base, DEFAULT_NOTIFY_SERVER);
    }

    #[test]
    fn local_done_message_names_outcome_and_targets() {
        let msg = local_done_message(true, None, &["@system".to_string()]);
        assert!(msg.contains("succeeded"), "was: {msg}");
        assert!(msg.contains("@system"), "was: {msg}");
        let msg = local_done_message(false, Some(3), &["a/b".to_string()]);
        assert!(msg.contains("failed"), "was: {msg}");
        assert!(msg.contains("exit 3"), "was: {msg}");
    }
}
