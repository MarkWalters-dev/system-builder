//! HTTP API tests. Spin the real router in-process with a dry-run manager.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gb_core::Manager;
use gb_core::config::{Config, ServerConfig, SystemConfig};
use gb_server::routes::router;
use serde_json::{Value, json};
use std::path::Path;
use tower::ServiceExt;

const LOG: &str = "\
1000:  >>> emerge (1 of 1) app-fake/pkg1-1.0 to /\n\
1042:  ::: completed emerge (1 of 1) app-fake/pkg1-1.0 to /\n";

fn test_manager(machine: &Path) -> Manager {
    std::fs::create_dir_all(machine.join("var/log")).unwrap();
    std::fs::write(machine.join("var/log/emerge.log"), LOG).unwrap();
    let config = Config {
        discovery: Default::default(),
        provision: Default::default(),
        server: ServerConfig {
            dry_run: true,
            max_concurrent_builds: 1,
            ..Default::default()
        },
        systems: vec![SystemConfig {
            name: "laptop".into(),
            nspawn: Some("gentoo-builder-laptop".into()),
            machine_path: machine.to_path_buf(),
            emerge_log: "/var/log/emerge.log".into(),
            emerge_bin: "/usr/bin/emerge".into(),
        }],
    };
    Manager::new(config)
}

fn setup() -> (tempfile::TempDir, axum::Router) {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp.path().join("mach"));
    let app = router(mgr);
    (tmp, app)
}

async fn body_json(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

fn post_json(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn health_ok() {
    let (_t, app) = setup();
    let resp = app.oneshot(get("/health")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn malformed_build_id_is_json_400() {
    let (_t, app) = setup();
    for uri in [
        "/v1/builds/zzz".to_string(),
        "/v1/builds/6a3319ce/log".to_string(),
    ] {
        let resp = app.clone().oneshot(get(&uri)).await.unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "for {uri}");
        assert!(
            body["error"].as_str().unwrap().contains("invalid build id"),
            "body was: {body}"
        );
    }
}

#[tokio::test]
async fn create_and_poll_build_to_done() {
    let (_t, app) = setup();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/builds",
            json!({
                "system": "laptop",
                "targets": ["@system"],
                // Unknown fields are ignored, so pre-removal clients still work.
                "portage_git_url": "https://example.invalid/p.git",
            }),
        ))
        .await
        .unwrap();
    let (status, created) = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_string();

    // Poll until terminal (dry-run finishes in well under a second).
    let state = loop {
        let resp = app
            .clone()
            .oneshot(get(&format!("/v1/builds/{id}")))
            .await
            .unwrap();
        let (status, job) = body_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        let s = job["state"].as_str().unwrap().to_string();
        if ["done", "failed", "cancelled"].contains(&s.as_str()) {
            break (s, job);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    assert_eq!(state.0, "done");
    assert_eq!(state.1["progress"]["total_packages"], 1);
    assert_eq!(state.1["auto_fix_conflicts"], true);
    assert!(
        state.1["unmerged_blockers"]
            .as_array()
            .is_some_and(|u| u.is_empty())
    );

    // Log endpoint works after completion.
    let resp = app
        .clone()
        .oneshot(get(&format!("/v1/builds/{id}/log?tail=50")))
        .await
        .unwrap();
    let (status, log) = body_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert!(log["log"].as_str().unwrap().contains("Emerging"));
}

#[tokio::test]
async fn create_rejects_unknown_system() {
    let (_t, app) = setup();
    let resp = app
        .oneshot(post_json(
            "/v1/builds",
            json!({
                "system": "nope",
                "targets": ["@system"],
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn queue_and_systems_endpoints() {
    let (_t, app) = setup();
    let resp = app.clone().oneshot(get("/v1/queue")).await.unwrap();
    let (status, q) = body_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(q["max_concurrent_builds"], 1);

    let resp = app.clone().oneshot(get("/v1/systems")).await.unwrap();
    let (status, systems) = body_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(systems[0]["name"], "laptop");
    assert_eq!(systems[0]["mode"], "nspawn");
    assert_eq!(systems[0]["nspawn"], "gentoo-builder-laptop");
    assert_eq!(systems[0]["status"], "idle");

    let resp = app.oneshot(get("/v1/systems/laptop/active")).await.unwrap();
    let (status, active) = body_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(active["active"], false);
}

#[tokio::test]
async fn estimates_and_history_endpoints() {
    let (_t, app) = setup();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/estimates",
            json!({ "system": "laptop", "packages": ["app-fake/pkg1"] }),
        ))
        .await
        .unwrap();
    let (status, est) = body_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(est["packages"][0]["estimated_secs"], 42);
    assert_eq!(est["packages"][0]["fallback"], false);

    let resp = app
        .oneshot(get("/v1/history?system=laptop&package=app-fake/pkg1-9.9"))
        .await
        .unwrap();
    let (status, h) = body_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h["samples"], 1);
    assert_eq!(h["median_secs"], 42);
}

#[tokio::test]
async fn stream_replays_and_ends_with_done() {
    let (_t, app) = setup();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/builds",
            json!({
                "system": "laptop",
                "targets": ["@system"],
            }),
        ))
        .await
        .unwrap();
    let (_, created) = body_json(resp).await;
    let id = created["id"].as_str().unwrap().to_string();

    let resp = app
        .oneshot(get(&format!("/v1/builds/{id}/stream")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    // The stream ends when the dry-run build finishes.
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("event: status"), "stream was:\n{text}");
    assert!(text.contains("event: done"), "stream was:\n{text}");
    assert!(text.contains("Emerging"), "stream was:\n{text}");
}

#[tokio::test]
async fn cancel_unknown_build_is_404() {
    let (_t, app) = setup();
    let id = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/builds/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

fn specs_fixture() -> Value {
    json!({
        "chost": "x86_64-pc-linux-gnu",
        "cflags": "-march=znver3 -O2 -pipe",
        "cpu_flags_x86": ["aes", "avx2"],
        "use_flags": ["ssl", "-X"],
        "profile": "default/linux/amd64/23.0/desktop",
    })
}

#[tokio::test]
async fn provision_new_system_queues_recompile_to_done() {
    let (_t, app) = setup();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/systems",
            json!({ "name": "newbox", "specs": specs_fixture() }),
        ))
        .await
        .unwrap();
    let (status, created) = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["system"]["name"], "newbox");
    assert_eq!(created["system"]["mode"], "nspawn");
    assert_eq!(created["system"]["nspawn"], "gentoo-builder-newbox");
    assert_eq!(created["job"]["targets"], json!(["@world"]));
    assert_eq!(
        created["job"]["emerge_args"],
        json!(["--emptytree", "--deep", "--newuse", "--verbose"])
    );
    assert_eq!(
        created["job"]["provision"]["specs"]["chost"],
        "x86_64-pc-linux-gnu"
    );
    let id = created["job"]["id"].as_str().unwrap().to_string();

    // Poll until terminal (dry-run finishes in well under a second).
    let state = loop {
        let resp = app
            .clone()
            .oneshot(get(&format!("/v1/builds/{id}")))
            .await
            .unwrap();
        let (status, job) = body_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        let s = job["state"].as_str().unwrap().to_string();
        if ["done", "failed", "cancelled"].contains(&s.as_str()) {
            break s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    assert_eq!(state, "done");

    // The prepare phase logged its (faked) steps.
    let resp = app
        .clone()
        .oneshot(get(&format!("/v1/builds/{id}/log?tail=50")))
        .await
        .unwrap();
    let (status, log) = body_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    let text = log["log"].as_str().unwrap();
    assert!(
        text.contains(">>> provision: create image"),
        "log was: {text}"
    );
}

#[tokio::test]
async fn provision_existing_system_is_idempotent() {
    let (_t, app) = setup();
    let resp = app
        .oneshot(post_json(
            "/v1/systems",
            json!({ "name": "laptop", "specs": specs_fixture() }),
        ))
        .await
        .unwrap();
    let (status, body) = body_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["system"]["name"], "laptop");
    assert!(body.get("job").is_none_or(Value::is_null));
}

#[tokio::test]
async fn provision_rejects_bad_requests() {
    let (_t, app) = setup();
    // Path-escaping name.
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/systems",
            json!({ "name": "../x", "specs": specs_fixture() }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Non-amd64 CHOST.
    let mut specs = specs_fixture();
    specs["chost"] = json!("aarch64-unknown-linux-gnu");
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/systems",
            json!({ "name": "pi", "specs": specs }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Profile traversal.
    let mut specs = specs_fixture();
    specs["profile"] = json!("a/../../b");
    let resp = app
        .oneshot(post_json(
            "/v1/systems",
            json!({ "name": "x", "specs": specs }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
