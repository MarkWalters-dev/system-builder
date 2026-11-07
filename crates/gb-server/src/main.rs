//! `gb-server`: HTTP API for the Gentoo system-builder.

use gb_core::Config;
use gb_server::routes;
use std::path::{Path, PathBuf};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

fn usage() -> ! {
    eprintln!("usage: gb-server [--config <path>]");
    eprintln!("  without --config, the first existing file wins:");
    eprintln!("    ./config.toml");
    eprintln!("    $HOME/.config/gb-server/config.toml");
    eprintln!("    /etc/system-builder/config.toml");
    std::process::exit(2);
}

fn config_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    let mut explicit: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                explicit = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage()));
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    match pick_config_path(explicit) {
        Ok(path) => {
            // Uniform first-run rule: the resolved file always exists
            // afterwards. An explicit-but-missing path gets scaffolded too.
            ensure_config_file(&path);
            path
        }
        Err(tried) => match gb_types::config_path::user_config_path("gb-server") {
            Some(user) => {
                ensure_config_file(&user);
                user
            }
            None => {
                eprintln!("error: no config file found; tried:");
                for t in &tried {
                    eprintln!("  {}", t.display());
                }
                eprintln!("($HOME is unknown, so no template could be created)");
                std::process::exit(1);
            }
        },
    }
}

/// Create a commented template at `path` when nothing exists there yet.
///
/// A freshly scaffolded file holds no active settings, so the process exits
/// 0 with instructions instead of starting half-configured. Exiting zero
/// also keeps a fresh systemd install from restart-looping (the unit only
/// restarts on failure) before the operator has edited the config.
fn ensure_config_file(path: &Path) {
    match gb_types::config_path::scaffold_file(path, &gb_core::config::template()) {
        Ok(true) => {
            eprintln!(
                "created {} with all defaults commented out.",
                path.display()
            );
            eprintln!(
                "builders are discovered automatically; edit only to override, then run gb-server again."
            );
            std::process::exit(0);
        }
        Ok(false) => {}
        Err(e) => {
            eprintln!("error: cannot create {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// Resolve which config file to load.
///
/// An explicit `--config` path is always used as-is (a missing file surfaces
/// as a load error). Otherwise the first existing candidate wins:
/// `./config.toml`, the per-user file, then the system file.
fn pick_config_path(explicit: Option<PathBuf>) -> Result<PathBuf, Vec<PathBuf>> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let mut candidates = vec![PathBuf::from("config.toml")];
    if let Some(user) = gb_types::config_path::user_config_path("gb-server") {
        candidates.push(user);
    }
    candidates.push(PathBuf::from("/etc/system-builder/config.toml"));
    pick_first_existing(&candidates, |p| p.exists()).ok_or(candidates)
}

fn pick_first_existing(candidates: &[PathBuf], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    candidates.iter().find(|p| exists(p)).cloned()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let path = config_path();
    let config = Config::load(&path).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    let bind = config.server.bind.clone();
    let max_concurrent = config.server.max_concurrent_builds;
    let dry_run = config.dry_run();
    let manager = gb_core::Manager::new(config);
    // Resolve once at startup so the log shows what discovery found.
    let systems = manager.systems().await;
    let names = systems
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join(",");
    // Always visible (not via tracing): operators and tests rely on it.
    if names.is_empty() {
        eprintln!(
            "warning: no build systems available (no gentoo-builder-* images and no [[systems]]?)"
        );
    }
    eprintln!(
        "starting gb-server on {bind} (config: {}, systems: [{}], max_concurrent: {max_concurrent}, dry_run: {dry_run})",
        path.display(),
        names,
    );
    // SIGTERM/SIGINT kill running builds first: without this the emerge
    // trees keep running orphaned after the server exits.
    let shutdown_mgr = manager.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        eprintln!("shutting down: cancelling running builds…");
        shutdown_mgr.shutdown().await;
        std::process::exit(0);
    });

    let app = routes::router(manager).layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| {
            eprintln!("error: bind {bind}: {e}");
            std::process::exit(1);
        });
    axum::serve(listener, app).await.unwrap_or_else(|e| {
        eprintln!("error: serve: {e}");
        std::process::exit(1);
    });
}

async fn wait_for_shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn exists(existing: &HashSet<PathBuf>) -> impl Fn(&Path) -> bool + '_ {
        move |p| existing.contains(p)
    }

    #[test]
    fn explicit_path_wins_unchecked() {
        let missing = PathBuf::from("/nowhere/missing.toml");
        assert_eq!(pick_config_path(Some(missing.clone())), Ok(missing));
    }

    #[test]
    fn first_existing_candidate_wins() {
        let candidates = vec![
            PathBuf::from("config.toml"),
            PathBuf::from("/home/u/.config/gb-server/config.toml"),
            PathBuf::from("/etc/system-builder/config.toml"),
        ];
        let only_user: HashSet<PathBuf> = [candidates[1].clone()].into_iter().collect();
        assert_eq!(
            pick_first_existing(&candidates, exists(&only_user)),
            Some(candidates[1].clone())
        );
        let all: HashSet<PathBuf> = candidates.iter().cloned().collect();
        assert_eq!(
            pick_first_existing(&candidates, exists(&all)),
            Some(candidates[0].clone())
        );
        let none: HashSet<PathBuf> = HashSet::new();
        assert_eq!(pick_first_existing(&candidates, exists(&none)), None);
    }
}
