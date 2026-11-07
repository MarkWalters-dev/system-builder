//! Running `emerge` on the host or inside an nspawn container.
//!
//! # No in-container agent
//!
//! Deliberately, there is **no** helper app installed in the containers:
//!
//! * containers: root `nsenter` into the machine leader's namespaces
//!   (`nsenter --target <leader> -m -u -i -n -p --wdns=/ -- env -i …
//!   <emerge> …`), which streams stdout/stderr over plain pipes and
//!   propagates the exit code;
//! * host (`buildhost`): plain local `emerge ...`.
//!
//! `nsenter` — not `systemd-run --machine=` or `machinectl shell` —
//! because the builder guests run **OpenRC**, and on this host both of
//! those require guest systemd (`org.freedesktop.systemd1` on the guest
//! bus). `nsenter` is init-agnostic: it only needs the machine running.
//!
//! Build-time history needs no container access at all: `emerge.log` is read
//! directly from the host via `/var/lib/machines/<name>/var/log/emerge.log`
//! (see [`crate::history`]).
//!
//! An agent would only pay off if containers had to queue builds while the
//! host API is unreachable, or for unprivileged fine-grained control. Neither
//! applies here, so shell-out keeps deployment to a single binary.
//!
//! # Machine lifecycle and privileges
//!
//! Before every build step the target machine is checked with
//! `machinectl show` and started with `machinectl start` when absent, so a
//! stopped builder starts on demand. gb-server itself must run as root (the
//! systemd unit does): starting machines and entering namespaces needs
//! privilege, and the server deliberately never uses `sudo`, password
//! prompts, or pty sessions (`machinectl shell` is never invoked). Every
//! child gets stdin detached so nothing can ever block on an interactive
//! prompt.
//!
//! # Cancellation
//!
//! The streaming build runs in its own process group (`process_group(0)`),
//! so cancel kills the whole tree — nsenter parent, emerge, ebuild helpers —
//! with one group SIGKILL (see [`Spawned::kill_tree`]). This matters because
//! `nsenter` forks before exec (the forked child is what lands in the
//! target pid namespace), so killing just the direct child would orphan
//! the build inside the container.

use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::config::SystemConfig;

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("spawn failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("machine '{0}' did not become ready in time")]
    MachineTimeout(String),
    #[error("machinectl failed: {0}")]
    MachineCtl(String),
    #[error("machine '{0}' has no leader (not running?)")]
    NoLeader(String),
    #[error("entering containers needs root (run gb-server as root — see install.sh/server unit)")]
    NotRoot,
    #[error("io: {0}")]
    Io(#[source] std::io::Error),
}

/// Update semantics shared by pretend and build: only merge what is
/// outdated, including USE-change rebuilds and dependencies. Without
/// `--update`, `emerge -p @system` lists the whole set as `[ebuild R]`
/// and every build reinstalls it, even when nothing changed.
pub const UPDATE_FLAGS: [&str; 3] = ["--update", "--newuse", "--deep"];

/// Args for `emerge -p`: quiet, update semantics, then any extra args
/// and the targets. Pure so tests can assert the flags without spawning.
fn pretend_args(targets: &[String], extra_args: &[String]) -> Vec<String> {
    let mut args = vec!["-p".to_string(), "--quiet".to_string()];
    args.extend(UPDATE_FLAGS.iter().map(|s| s.to_string()));
    args.extend(extra_args.iter().cloned());
    args.extend(targets.iter().cloned());
    args
}

/// A spawned emerge with merged line-oriented output.
pub struct Spawned {
    pub child: tokio::process::Child,
    /// Merged stdout+stderr lines (stderr lines keep a `stderr: ` prefix).
    pub lines: mpsc::UnboundedReceiver<String>,
}

impl Spawned {
    /// Kill the whole build tree: the child runs in its own process group
    /// (see [`Runner::spawn_emerge`]), so a group SIGKILL reaches nsenter,
    /// emerge, and any ebuild helpers alike. Already-exited children are
    /// left alone (which also avoids signalling a recycled pgid).
    pub async fn kill_tree(&mut self) {
        let exited = matches!(self.child.try_wait(), Ok(Some(_)));
        if !exited {
            if let Some(pid) = self.child.id() {
                // SAFETY: kill(2) with a signal number is async-safe and
                // takes no pointers; a negative pid targets the process
                // group. ESRCH (group already gone) is harmless here.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            // Belt and braces for the direct child itself.
            let _ = self.child.kill().await;
        }
    }
}

#[derive(Debug, Clone)]
pub struct Runner {
    pub dry_run: bool,
}

impl Runner {
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }

    /// Build the host-side command for a target: a fake local script in
    /// dry-run, plain `emerge` for host systems, and root `nsenter` into
    /// the machine leader's namespaces for containers.
    pub async fn command_for(
        &self,
        sys: &SystemConfig,
        emerge_args: &[String],
    ) -> Result<(String, Vec<String>), RunnerError> {
        if self.dry_run {
            return Ok(self.dry_run_command(emerge_args));
        }
        match &sys.nspawn {
            None => Ok(direct_argv(sys, emerge_args)),
            Some(machine) => {
                require_root()?;
                let leader = machine_leader(machine).await?;
                Ok(nsenter_argv(leader, sys, emerge_args))
            }
        }
    }

    fn dry_run_command(&self, emerge_args: &[String]) -> (String, Vec<String>) {
        // Fake a short build that prints Emerging lines. Pretend runs are
        // handled separately (see `run_pretend`) so this is build-only.
        let n = emerge_args.len().clamp(1, 5);
        let mut script = String::from("i=1; ");
        for i in 1..=n.max(3) {
            script.push_str(&format!(
                "echo '>>> Emerging ({i} of {t}) app-fake/pkg{i}-1.0::gentoo'; sleep 0.1; ",
                t = n.max(3)
            ));
        }
        script.push_str("echo 'done';");
        ("sh".to_string(), vec!["-c".to_string(), script])
    }

    /// Start the machine if needed. No-op for host systems and dry-run.
    pub async fn ensure_machine_running(&self, sys: &SystemConfig) -> Result<(), RunnerError> {
        let Some(machine) = sys.nspawn.as_deref() else {
            return Ok(());
        };
        if self.dry_run {
            return Ok(());
        }
        if machine_known(machine).await {
            return Ok(());
        }
        let out = tokio::process::Command::new("machinectl")
            .arg("start")
            .arg(machine)
            // Never inherit stdin: a password prompt here would hang the build.
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(RunnerError::Spawn)?;
        if !out.status.success() {
            return Err(RunnerError::MachineCtl(with_auth_hint(
                String::from_utf8_lossy(&out.stderr).trim(),
            )));
        }
        // Poll until the machine is addressable.
        for _ in 0..30 {
            if machine_known(machine).await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(RunnerError::MachineTimeout(machine.to_string()))
    }

    /// Spawn `emerge` for streaming (build phase).
    pub async fn spawn_emerge(
        &self,
        sys: &SystemConfig,
        emerge_args: &[String],
    ) -> Result<Spawned, RunnerError> {
        self.ensure_machine_running(sys).await?;
        let (program, args) = self.command_for(sys, emerge_args).await?;
        tracing::info!(system = %sys.name, program = %program, "spawning emerge");
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group so cancel kills the whole tree (see
        // kill_tree). process_group is a safe pre-fork flag.
        cmd.as_std_mut().process_group(0);
        let mut child = cmd.spawn().map_err(RunnerError::Spawn)?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(out) = stdout {
            pump_lines(out, tx.clone(), None);
        }
        if let Some(err) = stderr {
            pump_lines(err, tx, Some("stderr: "));
        }
        Ok(Spawned { child, lines: rx })
    }

    /// Run `emerge -p` (plus args) to completion and capture stdout.
    pub async fn run_pretend(
        &self,
        sys: &SystemConfig,
        targets: &[String],
        extra_args: &[String],
    ) -> Result<String, RunnerError> {
        if self.dry_run {
            // Canned plan: one package per target.
            let mut out = String::from("These are the packages that would be merged:\n");
            for (i, t) in targets.iter().enumerate() {
                let atom = if t.starts_with('@') {
                    format!("app-fake/pkg{}-1.0::gentoo", i + 1)
                } else {
                    format!("{}-1.0::gentoo", t.trim_end_matches("*"))
                };
                out.push_str(&format!("[ebuild   R    ] {atom}\n"));
            }
            return Ok(out);
        }
        self.ensure_machine_running(sys).await?;
        let args = pretend_args(targets, extra_args);
        let (program, full_args) = self.command_for(sys, &args).await?;
        let out = tokio::process::Command::new(&program)
            .args(&full_args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(RunnerError::Spawn)?;
        // Pretend failing (e.g. blocks) still prints useful output; return
        // stdout regardless, lifting stderr into it when empty.
        if out.stdout.is_empty() && !out.stderr.is_empty() {
            Ok(String::from_utf8_lossy(&out.stderr).into_owned())
        } else {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }

    /// Unmerge blocking packages to completion. Returns success plus combined
    /// output for the job log. A non-zero exit is returned, not raised: the
    /// resolve loop re-checks for blocks either way.
    pub async fn run_unmerge(
        &self,
        sys: &SystemConfig,
        blockers: &[String],
    ) -> Result<UnmergeOutput, RunnerError> {
        if self.dry_run {
            return Ok(UnmergeOutput {
                success: true,
                combined: format!("[dry-run] would unmerge: {}", blockers.join(" ")),
            });
        }
        self.ensure_machine_running(sys).await?;
        let (program, full_args) = self.command_for(sys, &unmerge_args(blockers)).await?;
        let out = tokio::process::Command::new(&program)
            .args(&full_args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(RunnerError::Spawn)?;
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr);
        }
        Ok(UnmergeOutput {
            success: out.status.success(),
            combined,
        })
    }
}

/// Result of one `emerge --unmerge` run.
#[derive(Debug, Clone)]
pub struct UnmergeOutput {
    pub success: bool,
    pub combined: String,
}

/// Args for unmerging blockers: quiet, non-interactive.
fn unmerge_args(blockers: &[String]) -> Vec<String> {
    let mut args = vec!["--unmerge".to_string(), "--quiet".to_string()];
    args.extend(blockers.iter().cloned());
    args
}

/// Host systems: plain local `emerge`. Pure so tests can assert routing
/// without executing anything.
fn direct_argv(sys: &SystemConfig, emerge_args: &[String]) -> (String, Vec<String>) {
    (
        sys.emerge_bin.to_string_lossy().into_owned(),
        emerge_args.to_vec(),
    )
}

/// Containers: root `nsenter` into the leader's namespaces. Pure; the
/// leader comes from [`machine_leader`].
///
/// Notes baked in from live probing:
/// * all five namespaces (mount/uts/ipc/net/pid) so emerge sees the
///   guest's filesystem, network, and a sane `/proc` (`subset=pid`
///   hides everything without the pid namespace);
/// * `--wdns=/` (attached `=`: both `-w` spellings take an *optional*
///   arg, so a separate value would be executed as the command) — and
///   `wdns`, not `wd`, because `wd` resolves on the host;
/// * `env -i` with a minimal deterministic environment instead of
///   inheriting the server's.
fn nsenter_argv(leader: u32, sys: &SystemConfig, emerge_args: &[String]) -> (String, Vec<String>) {
    let mut args = vec![
        "--target".to_string(),
        leader.to_string(),
        "--mount".to_string(),
        "--uts".to_string(),
        "--ipc".to_string(),
        "--net".to_string(),
        "--pid".to_string(),
        "--wdns=/".to_string(),
        "--".to_string(),
        "/usr/bin/env".to_string(),
        "-i".to_string(),
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "HOME=/root".to_string(),
        "TERM=dumb".to_string(),
        // Portage colorizes even over pipes; keep logs (and the progress
        // parser) free of escape codes.
        "NOCOLOR=true".to_string(),
        sys.emerge_bin.to_string_lossy().into_owned(),
    ];
    args.extend(emerge_args.iter().cloned());
    ("nsenter".to_string(), args)
}

fn require_root() -> Result<(), RunnerError> {
    // SAFETY: geteuid(2) takes no arguments and only reads process state.
    if unsafe { libc::geteuid() } != 0 {
        return Err(RunnerError::NotRoot);
    }
    Ok(())
}

/// Resolve the machine leader PID via `machinectl show`. Retries briefly:
/// right after `machinectl start` the machine can be listed a moment
/// before its leader is visible.
async fn machine_leader(machine: &str) -> Result<u32, RunnerError> {
    for _ in 0..5 {
        let out = tokio::process::Command::new("machinectl")
            .arg("show")
            .arg(machine)
            .arg("-p")
            .arg("Leader")
            .arg("--value")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(RunnerError::Spawn)?;
        if out.status.success()
            && let Some(leader) = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid != 0)
        {
            return Ok(leader);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(RunnerError::NoLeader(machine.to_string()))
}

async fn machine_known(machine: &str) -> bool {
    tokio::process::Command::new("machinectl")
        .arg("show")
        .arg(machine)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// When machinectl fails for privilege reasons, say so plainly: gb-server
/// must run as root (the systemd unit does). It never uses sudo or
/// password prompts, and all its commands are pty-free.
fn with_auth_hint(stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    let authish = [
        "password",
        "authentication",
        "not authorized",
        "access denied",
        "permission denied",
        "polkit",
        "org.freedesktop.machine1",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if authish {
        format!(
            "{stderr} (gb-server must run as root to start machines — see install.sh/server unit)"
        )
    } else {
        stderr.to_string()
    }
}

fn pump_lines<R>(reader: R, tx: mpsc::UnboundedSender<String>, prefix: Option<&'static str>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = match prefix {
                Some(p) => format!("{p}{line}"),
                None => line,
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn host_sys() -> SystemConfig {
        SystemConfig {
            name: "buildhost".into(),
            nspawn: None,
            machine_path: PathBuf::from("/"),
            emerge_log: PathBuf::from("/var/log/emerge.log"),
            emerge_bin: PathBuf::from("/usr/bin/emerge"),
        }
    }

    fn nspawn_sys() -> SystemConfig {
        SystemConfig {
            name: "laptop".into(),
            nspawn: Some("gentoo-builder-laptop".into()),
            machine_path: PathBuf::from("/var/lib/machines/gentoo-builder-laptop"),
            emerge_log: PathBuf::from("/var/log/emerge.log"),
            emerge_bin: PathBuf::from("/usr/bin/emerge"),
        }
    }

    #[test]
    fn auth_hint_added_for_privilege_failures() {
        let hinted = with_auth_hint("Error: Not authorized to start machine");
        assert!(hinted.contains("must run as root"), "got: {hinted}");
        let plain = with_auth_hint("Error: No machine 'x' known");
        assert!(!plain.contains("must run as root"), "got: {plain}");
    }

    #[test]
    fn unmerge_builds_quiet_noninteractive_args() {
        assert_eq!(
            unmerge_args(&["a/b".to_string(), "<c/d-1.0".to_string()]),
            vec!["--unmerge", "--quiet", "a/b", "<c/d-1.0"]
        );
    }

    #[test]
    fn pretend_runs_as_an_update() {
        assert_eq!(
            pretend_args(&["@system".to_string()], &[]),
            vec!["-p", "--quiet", "--update", "--newuse", "--deep", "@system"]
        );
        // Extra args are preserved, ahead of the targets.
        assert_eq!(
            pretend_args(&["a/b".to_string()], &["--oneshot".to_string()]),
            vec![
                "-p",
                "--quiet",
                "--update",
                "--newuse",
                "--deep",
                "--oneshot",
                "a/b"
            ]
        );
    }

    #[tokio::test]
    async fn host_runs_emerge_directly() {
        let r = Runner::new(false);
        let (prog, args) = r
            .command_for(&host_sys(), &["@system".into()])
            .await
            .unwrap();
        assert_eq!(prog, "/usr/bin/emerge");
        assert_eq!(args, vec!["@system".to_string()]);
    }

    #[test]
    fn container_goes_through_nsenter() {
        let (prog, args) = nsenter_argv(4242, &nspawn_sys(), &["@system".into()]);
        assert_eq!(prog, "nsenter");
        assert!(args.contains(&"--target".to_string()));
        assert!(args.contains(&"4242".to_string()));
        for ns in ["--mount", "--uts", "--ipc", "--net", "--pid"] {
            assert!(args.contains(&ns.to_string()), "missing {ns}");
        }
        // Attached `=` (separate value would be executed as the command)
        // and wdns (wd resolves on the host).
        assert!(args.contains(&"--wdns=/".to_string()));
        assert!(args.contains(&"--".to_string()));
        assert!(args.contains(&"/usr/bin/env".to_string()));
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"NOCOLOR=true".to_string()));
        assert!(args.contains(&"/usr/bin/emerge".to_string()));
        assert!(args.contains(&"@system".to_string()));
    }

    #[tokio::test]
    async fn kill_tree_kills_the_whole_process_group() {
        // Dry-run spawn: a local sh script with sleep grandchildren, no
        // root or machines needed. kill_tree must leave no member behind.
        let r = Runner::new(true);
        let args = vec!["a".to_string(); 5]; // five ~0.1s sleeps
        let mut spawned = r.spawn_emerge(&host_sys(), &args).await.unwrap();
        let pid = spawned.child.id().unwrap();
        spawned.kill_tree().await;
        let status = spawned.child.wait().await.unwrap();
        assert!(!status.success(), "build survived kill_tree: {status:?}");
        // Signal 0 to the group: nonzero (ESRCH, same user so never
        // EPERM) proves every member is gone. Poll briefly for reaping.
        let mut gone = false;
        for _ in 0..20 {
            // SAFETY: signal 0 sends nothing; negative pid = group check.
            if unsafe { libc::kill(-(pid as i32), 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(gone, "process group {pid} still has live members");
    }

    #[tokio::test]
    async fn bogus_machine_fails_before_any_spawn() {
        // ensure_machine_running rejects unknown machines (non-root: auth
        // hint; root: machinectl start fails), so this errors on every
        // privilege level without spawning anything.
        let mut sys = nspawn_sys();
        sys.nspawn = Some("definitely-not-a-machine-xyz".into());
        let r = Runner::new(false);
        assert!(r.run_pretend(&sys, &["a/b".into()], &[]).await.is_err());
    }
}
