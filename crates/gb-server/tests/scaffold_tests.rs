//! First-run scaffolding tests: spawn the real `gb-server` binary with a
//! redirected HOME so the developer's own `~/.config` is never touched.

use std::path::PathBuf;
use std::time::Duration;

fn server_binary() -> PathBuf {
    // Walk up from the test executable to the profile dir holding the binary.
    // (Works for both the standard deps/ layout and custom build layouts.)
    for dir in std::env::current_exe()
        .unwrap()
        .ancestors()
        .skip(1)
        .map(|p| p.to_path_buf())
    {
        let cand = dir.join("gb-server");
        if cand.is_file() {
            return cand;
        }
    }
    panic!(
        "gb-server binary not found above {}",
        std::env::current_exe().unwrap().display()
    );
}

fn live_system_config_present() -> bool {
    PathBuf::from("/etc/system-builder/config.toml").exists()
}

async fn run(
    home: &std::path::Path,
    workdir: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    let mut cmd = tokio::process::Command::new(server_binary());
    cmd.args(args)
        .current_dir(workdir)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("SYSTEM_BUILDER_DRY_RUN");
    tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .expect("gb-server took too long (did it start serving instead of exiting?)")
        .expect("spawn gb-server")
}

#[tokio::test]
async fn explicit_missing_config_is_scaffolded() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let custom = tmp.path().join("custom/server.toml");

    let out = run(
        &tmp.path().join("home"),
        &work,
        &["--config", custom.to_str().unwrap()],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&custom).expect("template was created");
    assert!(text.contains("max_concurrent_builds"));
    assert!(text.contains("[[systems]]"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("created"), "stderr was: {stderr}");
}

#[tokio::test]
async fn first_run_scaffolds_user_config() {
    if live_system_config_present() {
        eprintln!("skipping: /etc/system-builder/config.toml exists on this machine, so the");
        eprintln!("lookup chain would use it instead of scaffolding (correct behavior).");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let out = run(&home, &work, &[]).await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let user_config = home.join(".config/gb-server/config.toml");
    let text = std::fs::read_to_string(&user_config).expect("user template was created");
    assert!(text.contains("bind = "), "template was: {text}");
    // Everything commented: no active settings.
    for line in text.lines() {
        let line = line.trim();
        assert!(
            line.is_empty() || line.starts_with('#'),
            "active line: {line}"
        );
    }
}

#[tokio::test]
async fn disabled_discovery_without_systems_still_fails() {
    // Hermetic even with a live /etc config: the user file wins the chain.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let user_config = home.join(".config/gb-server/config.toml");
    std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    std::fs::write(&user_config, "[discovery]\nenabled = false\n").unwrap();

    let out = run(&home, &work, &[]).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no [[systems]] configured and discovery is disabled"),
        "stderr was: {stderr}"
    );
}

#[tokio::test]
async fn second_run_starts_serving_with_defaults() {
    // A bare template (discovery on) is a working server: it binds and stays
    // up. Port 0 keeps this hermetic (no fixed test port needed).
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let user_config = home.join(".config/gb-server/config.toml");
    std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    std::fs::write(
        &user_config,
        format!(
            "{}\n[server]\nbind = \"127.0.0.1:0\"\n",
            gb_core::config::template()
        ),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(server_binary())
        .current_dir(&work)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("SYSTEM_BUILDER_DRY_RUN")
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn gb-server");
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let alive = child.try_wait().expect("poll child").is_none();
    let _ = child.kill().await;
    let out = child.wait_with_output().await.expect("reap child");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(alive, "server exited early. stderr was:\n{stderr}");
    assert!(
        stderr.contains("starting gb-server"),
        "stderr was:\n{stderr}"
    );
}

#[tokio::test]
async fn sigterm_shuts_down_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let user_config = home.join(".config/gb-server/config.toml");
    std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    std::fs::write(
        &user_config,
        format!(
            "{}\n[server]\nbind = \"127.0.0.1:0\"\n",
            gb_core::config::template()
        ),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(server_binary())
        .current_dir(&work)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("SYSTEM_BUILDER_DRY_RUN")
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn gb-server");
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    assert!(child.try_wait().expect("poll child").is_none());
    // SIGTERM (not SIGKILL): the server must shut down and exit 0.
    let pid = child.id().expect("child pid");
    tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .await
        .expect("run kill");
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("server exited promptly")
        .expect("reap child");
    assert!(out.status.success(), "status: {}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("shutting down"), "stderr was:\n{stderr}");
}
