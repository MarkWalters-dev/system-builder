//! Client end-to-end tests: real HTTP against an in-process dry-run server,
//! plus the local-emerge step with stub binaries.

use gb_client::Client;
use gb_core::config::{Config, ServerConfig, SystemConfig};
use gb_types::{CreateBuildRequest, EstimateRequest, JobState, StreamEvent};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

const LOG: &str = "\
1000:  >>> emerge (1 of 1) app-fake/pkg1-1.0 to /\n\
1042:  ::: completed emerge (1 of 1) app-fake/pkg1-1.0 to /\n";

/// Boot a dry-run server on an ephemeral port. Returns (base_url, _tmp).
async fn boot_server() -> (String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let machine = tmp.path().join("mach");
    std::fs::create_dir_all(machine.join("var/log")).unwrap();
    std::fs::write(machine.join("var/log/emerge.log"), LOG).unwrap();
    let config = Config {
        discovery: Default::default(),
        server: ServerConfig {
            dry_run: true,
            max_concurrent_builds: 1,
            ..Default::default()
        },
        systems: vec![SystemConfig {
            name: "laptop".into(),
            nspawn: Some("gentoo-builder-laptop".into()),
            machine_path: machine,
            emerge_log: "/var/log/emerge.log".into(),
            emerge_bin: "/usr/bin/emerge".into(),
        }],
    };
    let app = gb_server::routes::router(gb_core::Manager::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), tmp)
}

#[tokio::test]
async fn submit_watch_and_estimate_over_http() {
    let (base, _tmp) = boot_server().await;
    let client = Client::new(&base, 30).unwrap();

    let job = client
        .submit(&CreateBuildRequest {
            system: "laptop".into(),
            targets: vec!["@system".into()],
            emerge_args: vec![],
            auto_fix_conflicts: None,
        })
        .await
        .unwrap();
    assert_eq!(job.system, "laptop");

    let mut events = Vec::new();
    let (state, code) = client
        .watch(&job.id.to_string(), |ev| events.push(ev))
        .await
        .unwrap();
    assert_eq!(state, JobState::Done);
    assert_eq!(code, Some(0));
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Log { .. })));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Progress { .. }))
    );
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Done { .. })));

    // The follow-up status fetch works too.
    let got = client.get(&job.id.to_string()).await.unwrap();
    assert_eq!(got.state, JobState::Done);

    // Estimates flow through the client, with fixture history.
    let est = client
        .estimate(&EstimateRequest {
            system: "laptop".into(),
            packages: vec!["app-fake/pkg1".into()],
            targets: vec![],
        })
        .await
        .unwrap();
    assert_eq!(est.packages[0].estimated_secs, Some(42));
}

#[tokio::test]
async fn api_errors_surface_with_status() {
    let (base, _tmp) = boot_server().await;
    let client = Client::new(&base, 30).unwrap();
    let err = client
        .get("00000000-0000-0000-0000-000000000000")
        .await
        .unwrap_err();
    match err {
        gb_client::ClientError::Api { status, .. } => assert_eq!(status, 404),
        other => panic!("expected Api error, got {other}"),
    }
}

fn client_binary() -> std::path::PathBuf {
    // Walk up from the test executable to the profile dir holding the binary.
    // (Works for both the standard deps/ layout and custom build layouts.)
    for dir in std::env::current_exe()
        .unwrap()
        .ancestors()
        .skip(1)
        .map(|p| p.to_path_buf())
    {
        let cand = dir.join("gb-client");
        if cand.is_file() {
            return cand;
        }
    }
    panic!(
        "gb-client binary not found above {}",
        std::env::current_exe().unwrap().display()
    );
}

/// Run the real binary with a redirected HOME so the developer's own
/// `~/.config` is never touched. Extra env can override per case.
async fn run_client(
    home: &std::path::Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = tokio::process::Command::new(client_binary());
    cmd.args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("GB_SERVER")
        .env_remove("GB_SYSTEM")
        .env_remove("GB_TIMEOUT");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
        .await
        .expect("gb-client took too long")
        .expect("spawn gb-client")
}

#[tokio::test]
async fn explicit_missing_config_is_scaffolded_and_used() {
    let (base, tmp) = boot_server().await;
    let home = tmp.path().join("home");
    let custom = tmp.path().join("custom/client.toml");

    // Missing file: scaffolded, then the command proceeds with flags.
    let out = run_client(
        &home,
        &[
            "--config",
            custom.to_str().unwrap(),
            "--server",
            &base,
            "systems",
        ],
        &[],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("laptop"), "stdout was: {stdout}");
    let text = std::fs::read_to_string(&custom).expect("template was created");
    assert!(text.contains("timeout"));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("created"), "stderr was: {stderr}");

    // Now the file exists with a server value: flags can be dropped.
    std::fs::write(
        &custom,
        format!("server = \"{base}\"\nsystem = \"laptop\"\n"),
    )
    .unwrap();
    let out = run_client(
        &home,
        &["--config", custom.to_str().unwrap(), "systems"],
        &[],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn default_user_config_is_scaffolded_on_first_run() {
    let (base, tmp) = boot_server().await;
    let home = tmp.path().join("home");

    let out = run_client(&home, &["systems"], &[("GB_SERVER", &base)]).await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let user_config = home.join(".config/gb-client/config.toml");
    let text = std::fs::read_to_string(&user_config).expect("user template was created");
    assert!(text.contains("server = "), "template was: {text}");
    for line in text.lines() {
        let line = line.trim();
        assert!(
            line.is_empty() || line.starts_with('#'),
            "active line: {line}"
        );
    }
}

#[tokio::test]
async fn values_resolve_flag_over_env_over_file() {
    let (base, tmp) = boot_server().await;
    let home = tmp.path().join("home");
    let cfg = tmp.path().join("client.toml");
    std::fs::write(&cfg, format!("server = \"{base}\"\nsystem = \"laptop\"\n")).unwrap();
    let cfg = cfg.to_str().unwrap().to_string();

    // File alone is enough.
    let out = run_client(
        &home,
        &["--config", &cfg, "history", "--package", "app-fake/pkg1"],
        &[],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Env beats file: unknown system fails.
    let out = run_client(
        &home,
        &["--config", &cfg, "history", "--package", "app-fake/pkg1"],
        &[("GB_SYSTEM", "nope")],
    )
    .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown system"));

    // Flag beats env.
    let out = run_client(
        &home,
        &[
            "--config",
            &cfg,
            "history",
            "--system",
            "laptop",
            "--package",
            "app-fake/pkg1",
        ],
        &[("GB_SYSTEM", "nope")],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn build_takes_positional_targets() {
    // `gb-client build @system ...`: targets are positional (--system here;
    // on a real client the system comes from the config file). --no-watch
    // submits and exits without watching or running a local emerge.
    let (base, tmp) = boot_server().await;
    let home = tmp.path().join("home");
    let out = run_client(
        &home,
        &[
            "--server",
            &base,
            "build",
            "--system",
            "laptop",
            "@system",
            "sys-apps/foo",
            "--no-watch",
        ],
        &[],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8(out.stdout).unwrap();
    uuid::Uuid::parse_str(id.trim()).expect("stdout is the build id");
    // Both positionals reached the server as the build targets.
    let client = Client::new(&base, 30).unwrap();
    let job = client.get(id.trim()).await.unwrap();
    assert_eq!(job.targets, vec!["@system", "sys-apps/foo"]);
}

#[tokio::test]
async fn ids_default_to_latest_full_uuids_and_validate() {
    let (base, tmp) = boot_server().await;
    let home = tmp.path().join("home");
    let client = Client::new(&base, 30).unwrap();
    let job = client
        .submit(&CreateBuildRequest {
            system: "laptop".into(),
            targets: vec!["@system".into()],
            emerge_args: vec![],
            auto_fix_conflicts: None,
        })
        .await
        .unwrap();
    let full = job.id.to_string();

    // list prints full, usable ids.
    let out = run_client(&home, &["--server", &base, "list"], &[]).await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains(&full), "stdout was: {stdout}");

    // Omitted id means the latest build.
    let out = run_client(&home, &["--server", &base, "status"], &[]).await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains(&full), "stdout was: {stdout}");

    // Short prefixes are rejected with a hint, not a server 400.
    let out = run_client(&home, &["--server", &base, "status", "6a3319ce"], &[]).await;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid build id"),
        "stderr was: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // watch with no id follows the latest build to Done.
    let out = run_client(&home, &["--server", &base, "watch"], &[]).await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn poll_until_done_reaches_terminal_state() {
    let (base, _tmp) = boot_server().await;
    let client = Client::new(&base, 30).unwrap();
    let job = client
        .submit(&CreateBuildRequest {
            system: "laptop".into(),
            targets: vec!["@system".into()],
            emerge_args: vec![],
            auto_fix_conflicts: None,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    let (state, _code) = client
        .poll_until_done(&job.id.to_string(), |ev| events.push(ev))
        .await
        .unwrap();
    assert_eq!(state, JobState::Done);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Progress { .. }))
    );
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
}

/// Caught ntfy posts as (topic, title, priority, body).
type Caught = Arc<Mutex<Vec<(String, Option<String>, Option<String>, String)>>>;

/// Boot a tiny ntfy stand-in on an ephemeral port.
async fn boot_catcher() -> (String, Caught) {
    let caught: Caught = Arc::new(Mutex::new(Vec::new()));
    let got = caught.clone();
    let app = axum::Router::new().route(
        "/{topic}",
        axum::routing::post(
            |axum::extract::Path(topic): axum::extract::Path<String>,
             headers: axum::http::HeaderMap,
             body: String| async move {
                let val = |k: &str| {
                    headers
                        .get(k)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                };
                got.lock()
                    .unwrap()
                    .push((topic, val("title"), val("priority"), body));
                "ok"
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), caught)
}

#[tokio::test]
async fn notify_posts_title_priority_and_body() {
    let (catcher, caught) = boot_catcher().await;
    let client = Client::new("http://127.0.0.1:9", 30).unwrap();
    client
        .notify(&catcher, "my-topic", "t", "hello", Some("high"))
        .await
        .unwrap();
    let got = caught.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0],
        (
            "my-topic".to_string(),
            Some("t".to_string()),
            Some("high".to_string()),
            "hello".to_string()
        )
    );
}

#[tokio::test]
async fn build_notifies_when_local_emerge_finishes() {
    let (base, tmp) = boot_server().await;
    let (catcher, caught) = boot_catcher().await;
    let home = tmp.path().join("home");
    let cfg = tmp.path().join("client.toml");
    std::fs::write(
        &cfg,
        format!(
            "server = \"{base}\"\nsystem = \"laptop\"\nnotify_topic = \"t\"\nnotify_server = \"{catcher}\"\n"
        ),
    )
    .unwrap();
    let out = run_client(
        &home,
        &[
            "--config",
            cfg.to_str().unwrap(),
            "build",
            "@system",
            "--local-bin",
            "true",
        ],
        &[],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Exactly one ping: the local-done one (remote succeeded, so silent).
    let got = caught.lock().unwrap();
    assert_eq!(got.len(), 1, "caught: {got:?}");
    assert_eq!(got[0].0, "t");
    assert!(got[0].3.contains("local emerge"), "body was: {}", got[0].3);
    assert!(got[0].3.contains("succeeded"), "body was: {}", got[0].3);
}

#[tokio::test]
async fn watch_notifies_when_remote_build_fails() {
    let (base, tmp) = boot_server().await;
    let (catcher, caught) = boot_catcher().await;
    let home = tmp.path().join("home");
    let client = Client::new(&base, 30).unwrap();
    // A cancelled build exercises the same non-Done notify path as a failure.
    let job = client
        .submit(&CreateBuildRequest {
            system: "laptop".into(),
            targets: vec!["@system".into()],
            emerge_args: vec![],
            auto_fix_conflicts: None,
        })
        .await
        .unwrap();
    client.cancel(&job.id.to_string()).await.unwrap();
    let cfg = tmp.path().join("client.toml");
    std::fs::write(
        &cfg,
        format!("server = \"{base}\"\nnotify_topic = \"t\"\nnotify_server = \"{catcher}\"\n"),
    )
    .unwrap();
    let out = run_client(
        &home,
        &[
            "--config",
            cfg.to_str().unwrap(),
            "watch",
            &job.id.to_string(),
        ],
        &[],
    )
    .await;
    assert_eq!(out.status.code(), Some(2));
    let got = caught.lock().unwrap();
    assert_eq!(got.len(), 1, "caught: {got:?}");
    assert!(got[0].3.contains("remote build"), "body was: {}", got[0].3);
    assert!(got[0].3.contains("cancelled"), "body was: {}", got[0].3);
    assert_eq!(got[0].2.as_deref(), Some("high"));
}

#[tokio::test]
async fn local_emerge_defaults_to_update_semantics() {
    let (base, tmp) = boot_server().await;
    let home = tmp.path().join("home");
    // Stub local emerge that records its argv instead of merging anything.
    let log = tmp.path().join("local-calls.log");
    let stub = tmp.path().join("local-emerge-stub");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\necho \"$@\" > \"{}\"\nexit 0\n", log.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = run_client(
        &home,
        &[
            "--server",
            &base,
            "build",
            "--system",
            "laptop",
            "@system",
            "--local-bin",
            stub.to_str().unwrap(),
        ],
        &[],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let argv = std::fs::read_to_string(&log).unwrap();
    // Without update flags the local step would reinstall the whole set too.
    for flag in ["--update", "--newuse", "--deep"] {
        assert!(
            argv.split_whitespace().any(|t| t == flag),
            "local emerge argv missing {flag}: {argv}"
        );
    }
    assert!(
        argv.split_whitespace().any(|t| t == "@system"),
        "local targets missing: {argv}"
    );
}

#[tokio::test]
async fn missing_server_fails_fast_without_touching_localhost() {
    // No server anywhere (fresh HOME, env scrubbed by run_client): the
    // client must say the server is missing, not attempt localhost.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let out = run_client(&home, &["queue"], &[]).await;
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("server"), "stderr was: {stderr}");
    assert!(
        !stderr.contains("127.0.0.1"),
        "must not fall back to localhost, stderr was: {stderr}"
    );
}

#[tokio::test]
async fn cancel_with_no_state_and_empty_queue_errors_cleanly() {
    // Fresh server (zero jobs) and fresh HOME (no recorded submission):
    // bare `cancel` reports nothing to cancel instead of requiring an id.
    let (base, tmp) = boot_server().await;
    let home = tmp.path().join("home");
    let out = run_client(&home, &["--server", &base, "cancel"], &[]).await;
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nothing to cancel"), "stderr was: {stderr}");
}

#[tokio::test]
async fn cancel_defaults_to_last_submitted_build() {
    let (base, tmp) = boot_server().await;
    let home = tmp.path().join("home");
    // Submitting records the build id locally.
    let out = run_client(
        &home,
        &[
            "--server",
            &base,
            "build",
            "--system",
            "laptop",
            "@system",
            "--no-watch",
        ],
        &[],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8(out.stdout).unwrap();
    let id = id.trim().to_string();
    let state = home.join(".local/state/gb-client/last_build_id");
    assert_eq!(std::fs::read_to_string(&state).unwrap().trim(), id);
    // Bare `cancel` targets that recorded id: either it cancels it, or the
    // dry-run build already finished and the fallback note names it. Every
    // path mentions the id, so this holds regardless of the race.
    let out = run_client(&home, &["--server", &base, "cancel"], &[]).await;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains(&id),
        "cancel did not target the recorded build {id}: {combined}"
    );
}

#[tokio::test]
async fn local_emerge_reports_exit_status() {
    // Stub binaries instead of a real emerge.
    let ok = gb_client::run_local_emerge("true", false, &[], &[])
        .await
        .unwrap();
    assert!(ok.success());
    let fail = gb_client::run_local_emerge("false", false, &[], &[])
        .await
        .unwrap();
    assert!(!fail.success());
    let missing = gb_client::run_local_emerge("gb-definitely-not-a-binary", false, &[], &[]).await;
    assert!(missing.is_err());
}
