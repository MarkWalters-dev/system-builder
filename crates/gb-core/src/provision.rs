//! Provisioning: create builder images from scratch with client settings.
//!
//! `POST /v1/systems` with [`gb_types::ClientSpecs`] creates
//! `<prefix><name>` under the machine root: stage3 unpack, a client-tuned
//! `make.conf`, the profile link, shared binds (host repo + distfiles +
//! per-system binpkgs), and the `.nspawn` file mirroring the hand-built
//! builders. The provision job then recompiles `@world` from source
//! inside the new image — no foreign binpkgs, everything with the
//! client's own flags.

use gb_types::ClientSpecs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Only amd64 stage3 images are provisioned for now.
pub const SUPPORTED_CHOST: &str = "x86_64-pc-linux-gnu";
/// Host paths bind-mounted into every builder image (mirrors the
/// hand-built builders; the host maintains the repo, the container
/// never syncs).
pub const HOST_REPO: &str = "/var/db/repos/gentoo";
pub const HOST_DISTFILES: &str = "/var/cache/distfiles";
pub const NSPAWN_DIR: &str = "/etc/systemd/nspawn";
const STAGE3_BASE_URL: &str = "https://distfiles.gentoo.org/releases/amd64/autobuilds";

#[derive(Debug, Error)]
pub enum ProvisionError {
    #[error("invalid provision request: {0}")]
    Invalid(String),
    #[error("stage3 download failed: {0}")]
    Download(String),
    #[error("stage3 verification failed: {0}")]
    Verify(String),
    #[error("image setup failed: {0}")]
    Setup(String),
    #[error("provisioning cancelled")]
    Cancelled,
}

// ------------------------------------------------------------ validation

/// Validate a provisioned system name: a safe path segment (it becomes
/// a directory under the machine root and part of the image name).
pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("system name is empty".to_string());
    }
    if name.len() > 64 {
        return Err("system name is too long (max 64 chars)".to_string());
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return Err(format!("invalid system name '{name}'")),
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
        return Err(format!("invalid system name '{name}'"));
    }
    if name.contains("..") {
        return Err(format!("invalid system name '{name}'"));
    }
    Ok(name.to_string())
}

fn safe_flag_value(field: &str, value: &str) -> Result<(), String> {
    if value.len() > 1024 {
        return Err(format!("{field} is too long"));
    }
    if !value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, ' ' | '-' | '_' | '.' | '/' | '=' | '+' | ',' | ':')
    }) {
        return Err(format!(
            "{field} has invalid characters (send literal flags, no variable references)"
        ));
    }
    Ok(())
}

fn safe_flag_list(field: &str, flags: &[String], cpu: bool) -> Result<(), String> {
    if flags.len() > 512 {
        return Err(format!("{field} has too many entries"));
    }
    for f in flags {
        let bare = f.strip_prefix('-').unwrap_or(f);
        if bare.is_empty()
            || bare.len() > 64
            || !bare
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || (!cpu && (c == '+' || c == '-')))
        {
            return Err(format!("invalid {field} entry '{f}'"));
        }
    }
    Ok(())
}

fn safe_profile(profile: &str) -> Result<(), String> {
    if profile.is_empty() || profile.len() > 256 {
        return Err("profile is empty or too long".to_string());
    }
    if profile.starts_with('/') || profile.ends_with('/') || profile.contains("//") {
        return Err(format!("invalid profile '{profile}'"));
    }
    for seg in profile.split('/') {
        if seg.is_empty()
            || seg == "."
            || seg == ".."
            || !seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
        {
            return Err(format!("invalid profile '{profile}'"));
        }
    }
    Ok(())
}

/// Validate client specs. Values land in a bash-sourced `make.conf`,
/// so anything outside a strict allowlist is rejected.
pub fn validate_specs(specs: &ClientSpecs) -> Result<(), String> {
    if specs.chost != SUPPORTED_CHOST {
        return Err(format!(
            "unsupported CHOST '{}': this server provisions {SUPPORTED_CHOST} images",
            specs.chost
        ));
    }
    if specs.cflags.trim().is_empty() {
        return Err("cflags is empty".to_string());
    }
    safe_flag_value("cflags", &specs.cflags)?;
    if let Some(march) = march_from_cflags(&specs.cflags)
        && march == "native"
    {
        return Err(
            "cflags uses -march=native, which would resolve to the server's CPU; \
             resolve it on the client (gcc -march=native -Q --help=target) and send a concrete -march=<cpu>"
                .to_string(),
        );
    }
    safe_flag_list("cpu_flags_x86", &specs.cpu_flags_x86, true)?;
    safe_flag_list("use_flags", &specs.use_flags, false)?;
    safe_profile(&specs.profile)?;
    Ok(())
}

// ------------------------------------------------------------- rendering

/// Extract `-march=<cpu>` from CFLAGS (last one wins, like gcc).
/// Returns `None` when no valid `-march` is present.
pub fn march_from_cflags(cflags: &str) -> Option<String> {
    let mut march = None;
    for token in cflags.split_whitespace() {
        if let Some(value) = token.strip_prefix("-march=")
            && !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            march = Some(value.to_string());
        }
    }
    march
}

/// Render the image's `make.conf`, mirroring the hand-built builders:
/// client CFLAGS verbatim as `COMMON_FLAGS`, everything derived from it.
pub fn render_make_conf(specs: &ClientSpecs, jobs: usize) -> String {
    let mut out = String::from(
        "# Generated by gb-server provision from the client's own settings.\n\
         # The image's @world was compiled from source with these flags.\n",
    );
    out.push_str(&format!("COMMON_FLAGS=\"{}\"\n", specs.cflags));
    out.push_str("CFLAGS=\"${COMMON_FLAGS}\"\n");
    out.push_str("CXXFLAGS=\"${COMMON_FLAGS}\"\n");
    out.push_str("FCFLAGS=\"${COMMON_FLAGS}\"\n");
    out.push_str("FFLAGS=\"${COMMON_FLAGS}\"\n");
    if let Some(march) = march_from_cflags(&specs.cflags) {
        out.push_str(&format!("RUSTFLAGS=\"-C target-cpu={march}\"\n"));
    }
    if !specs.cpu_flags_x86.is_empty() {
        out.push_str(&format!(
            "CPU_FLAGS_X86=\"{}\"\n",
            specs.cpu_flags_x86.join(" ")
        ));
    }
    if !specs.use_flags.is_empty() {
        out.push_str(&format!("USE=\"{}\"\n", specs.use_flags.join(" ")));
    }
    out.push_str(&format!("MAKEOPTS=\"-j{}\"\n", jobs.max(1)));
    // The image's own binpkgs (same settings, reusable on rebuilds).
    out.push_str("FEATURES=\"buildpkg\"\n");
    out
}

/// Render the `.nspawn` file, mirroring the hand-built builders: bootable,
/// unprivileged-userns off, host repo + distfiles shared, per-system
/// binpkgs persisted on the host.
pub fn render_nspawn(binserver_path: &Path) -> String {
    format!(
        "# Generated by gb-server provision.\n\
         [Exec]\n\
         Boot=yes\n\
         PrivateUsers=no\n\
         Capability=CAP_NET_ADMIN\n\
         \n\
         [Files]\n\
         Bind={HOST_REPO}\n\
         Bind={HOST_DISTFILES}\n\
         Bind={}:/var/cache/binpkgs\n",
        binserver_path.display(),
    )
}

/// Repo config for the image: the host's repo is bind-mounted in, so the
/// container never syncs (the host maintains it).
pub fn render_repos_conf() -> &'static str {
    "[gentoo]\n\
     location = /var/db/repos/gentoo\n\
     sync-type = git\n\
     sync-uri = https://github.com/gentoo-mirror/gentoo.git\n\
     auto-sync = no\n"
}

// ---------------------------------------------------------------- stage3

/// Autobuilds index filename for a stage3 variant (`openrc`/`systemd`).
pub fn autobuilds_txt_name(variant: &str) -> String {
    format!("latest-stage3-amd64-{variant}.txt")
}

/// Parse the autobuilds index: first non-comment field naming a stage3
/// tarball, returned as the path relative to the autobuilds directory.
/// Tolerant of trailing fields (sizes, hashes) and blank lines.
pub fn parse_autobuilds_txt(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(field) = line.split_whitespace().next()
            && field.contains("stage3")
            && field.ends_with(".tar.xz")
        {
            return Some(field.to_string());
        }
    }
    None
}

/// Parse a Gentoo `.DIGESTS` file for `filename`, preferring SHA512 then
/// SHA256. Tolerates clearsigned (`.asc`) armor and blank lines.
pub fn parse_digest_sha512(digests: &str, filename: &str) -> Option<String> {
    parse_digest(digests, filename, "SHA512").or_else(|| parse_digest(digests, filename, "SHA256"))
}

fn parse_digest(digests: &str, filename: &str, want_algo: &str) -> Option<String> {
    let mut algo = String::new();
    for line in digests.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("-----") || line.starts_with("Hash:") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            let rest = rest.trim();
            if let Some(name) = rest.strip_suffix("HASH") {
                algo = name.trim().to_string();
            }
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(hash), Some(file)) = (fields.next(), fields.next()) else {
            continue;
        };
        let file = file.trim_start_matches('*');
        if algo == want_algo && (file == filename || file.ends_with(&format!("/{filename}"))) {
            return Some(hash.to_string());
        }
    }
    None
}

/// SHA512 hex of a file. Blocking (multi-second for a stage3); callers
/// run it in `spawn_blocking`.
pub fn sha512_file(path: &Path) -> Result<String, std::io::Error> {
    use sha2::{Digest, Sha512};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha512::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn sha512_file_async(path: PathBuf) -> Result<String, ProvisionError> {
    tokio::task::spawn_blocking(move || sha512_file(&path))
        .await
        .map_err(|e| ProvisionError::Verify(format!("hash task failed: {e}")))?
        .map_err(|e| ProvisionError::Verify(format!("cannot hash stage3: {e}")))
}

async fn cancelled(cancel: &CancellationToken) {
    cancel.cancelled().await;
}

/// Ensure a verified stage3 tarball is cached, downloading it on first
/// use. Re-verifies the cached copy every time (cheap next to an
/// `@world` recompile) and prunes older cached stage3s.
pub async fn ensure_stage3(
    cache_dir: &Path,
    variant: &str,
    cancel: &CancellationToken,
) -> Result<PathBuf, ProvisionError> {
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ProvisionError::Download(format!("http client: {e}")))?;
    let txt_url = format!("{STAGE3_BASE_URL}/{}", autobuilds_txt_name(variant));
    let txt = tokio::select! {
        r = http.get(&txt_url).send() => r
            .map_err(|e| ProvisionError::Download(format!("{txt_url}: {e}")))?
            .error_for_status()
            .map_err(|e| ProvisionError::Download(format!("{txt_url}: {e}")))?
            .text()
            .await
            .map_err(|e| ProvisionError::Download(format!("{txt_url}: {e}")))?,
        _ = cancelled(cancel) => return Err(ProvisionError::Cancelled),
    };
    let rel = parse_autobuilds_txt(&txt).ok_or_else(|| {
        ProvisionError::Download(format!("no stage3 tarball listed in {txt_url}"))
    })?;
    let filename = rel.rsplit('/').next().unwrap_or(&rel).to_string();
    let tarball_url = format!("{STAGE3_BASE_URL}/{rel}");

    // DIGESTS (plain or clearsigned) live next to the tarball.
    let mut expected = None;
    for suffix in [".DIGESTS", ".DIGESTS.asc"] {
        let url = format!("{tarball_url}{suffix}");
        let fetched = tokio::select! {
            r = http.get(&url).send() => r.ok(),
            _ = cancelled(cancel) => return Err(ProvisionError::Cancelled),
        };
        let Some(resp) = fetched else { continue };
        if !resp.status().is_success() {
            continue;
        }
        let text = resp
            .text()
            .await
            .map_err(|e| ProvisionError::Download(format!("{url}: {e}")))?;
        if let Some(hash) = parse_digest_sha512(&text, &filename) {
            expected = Some(hash);
            break;
        }
    }
    let expected = expected
        .ok_or_else(|| ProvisionError::Verify(format!("no usable digest for {filename}")))?;

    std::fs::create_dir_all(cache_dir)
        .map_err(|e| ProvisionError::Setup(format!("cannot create cache dir: {e}")))?;
    let target = cache_dir.join(&filename);
    if target.exists() {
        let actual = tokio::select! {
            h = sha512_file_async(target.clone()) => h?,
            _ = cancelled(cancel) => return Err(ProvisionError::Cancelled),
        };
        if actual.eq_ignore_ascii_case(&expected) {
            prune_stage3_cache(cache_dir, &filename);
            return Ok(target);
        }
        let _ = std::fs::remove_file(&target);
    }

    let tmp = cache_dir.join(format!("{filename}.part"));
    let mut resp = tokio::select! {
        r = http.get(&tarball_url).send() => r
            .map_err(|e| ProvisionError::Download(format!("{tarball_url}: {e}")))?
            .error_for_status()
            .map_err(|e| ProvisionError::Download(format!("{tarball_url}: {e}")))?,
        _ = cancelled(cancel) => return Err(ProvisionError::Cancelled),
    };
    let download = async {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| ProvisionError::Download(format!("cannot write {filename}: {e}")))?;
        loop {
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| ProvisionError::Download(format!("{filename}: {e}")))?;
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk)
                .await
                .map_err(|e| ProvisionError::Download(format!("cannot write {filename}: {e}")))?;
        }
        file.flush()
            .await
            .map_err(|e| ProvisionError::Download(format!("cannot write {filename}: {e}")))?;
        drop(file);
        tokio::fs::rename(&tmp, &target)
            .await
            .map_err(|e| ProvisionError::Download(format!("cannot store {filename}: {e}")))?;
        Ok::<(), ProvisionError>(())
    };
    tokio::select! {
        r = download => r?,
        _ = cancelled(cancel) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(ProvisionError::Cancelled);
        }
    }
    let actual = tokio::select! {
        h = sha512_file_async(target.clone()) => h?,
        _ = cancelled(cancel) => {
            let _ = std::fs::remove_file(&target);
            return Err(ProvisionError::Cancelled);
        }
    };
    if !actual.eq_ignore_ascii_case(&expected) {
        let _ = std::fs::remove_file(&target);
        return Err(ProvisionError::Verify(format!(
            "checksum mismatch for {filename}"
        )));
    }
    prune_stage3_cache(cache_dir, &filename);
    Ok(target)
}

/// Drop older cached stage3 tarballs, keeping `keep`.
fn prune_stage3_cache(cache_dir: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != keep && name.starts_with("stage3-") && name.ends_with(".tar.xz") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ------------------------------------------------------- image assembly

/// `tar` argv for unpacking a stage3 (xattrs + numeric owners preserved).
pub fn unpack_argv(tarball: &Path, root: &Path) -> (String, Vec<String>) {
    (
        "tar".to_string(),
        vec![
            "xpf".to_string(),
            tarball.to_string_lossy().into_owned(),
            "-C".to_string(),
            root.to_string_lossy().into_owned(),
            "--xattrs-include=*.*".to_string(),
            "--numeric-owner".to_string(),
        ],
    )
}

/// Unpack a stage3 tarball into `root` (created by the caller).
/// Cancellable: on cancel the `tar` child is killed.
pub async fn unpack_stage3(
    tarball: &Path,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<(), ProvisionError> {
    use std::process::Stdio;
    let (program, args) = unpack_argv(tarball, root);
    let mut child = tokio::process::Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ProvisionError::Setup(format!("spawn tar: {e}")))?;
    let status = tokio::select! {
        s = child.wait() => s.map_err(|e| ProvisionError::Setup(format!("tar failed: {e}")))?,
        _ = cancelled(cancel) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(ProvisionError::Cancelled);
        }
    };
    if !status.success() {
        return Err(ProvisionError::Setup(format!(
            "tar exited with status {}",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// Write the image's portage config: client `make.conf`, the bind-mount
/// `repos.conf`, and the profile symlink. `host_repo` is checked for the
/// profile (the container sees the same tree via bind-mount).
pub fn write_image_files(
    root: &Path,
    specs: &ClientSpecs,
    jobs: usize,
    host_repo: &Path,
) -> Result<(), ProvisionError> {
    let portage = root.join("etc/portage");
    std::fs::create_dir_all(portage.join("repos.conf"))
        .map_err(|e| ProvisionError::Setup(format!("cannot create portage dirs: {e}")))?;
    std::fs::write(portage.join("make.conf"), render_make_conf(specs, jobs))
        .map_err(|e| ProvisionError::Setup(format!("cannot write make.conf: {e}")))?;
    std::fs::write(portage.join("repos.conf/gentoo.conf"), render_repos_conf())
        .map_err(|e| ProvisionError::Setup(format!("cannot write repos.conf: {e}")))?;
    if !host_repo.join("profiles").join(&specs.profile).is_dir() {
        return Err(ProvisionError::Setup(format!(
            "profile '{}' not found in the host repo ({}); sync the host repo first",
            specs.profile,
            host_repo.display()
        )));
    }
    let link = portage.join("make.profile");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(
        Path::new(HOST_REPO).join("profiles").join(&specs.profile),
        &link,
    )
    .map_err(|e| ProvisionError::Setup(format!("cannot link make.profile: {e}")))?;
    Ok(())
}

/// Write `/etc/systemd/nspawn/<image>.nspawn`, creating the dir.
pub fn write_nspawn_file(
    nspawn_dir: &Path,
    image: &str,
    binserver_path: &Path,
) -> Result<(), ProvisionError> {
    std::fs::create_dir_all(nspawn_dir)
        .map_err(|e| ProvisionError::Setup(format!("cannot create nspawn dir: {e}")))?;
    std::fs::write(
        nspawn_dir.join(format!("{image}.nspawn")),
        render_nspawn(binserver_path),
    )
    .map_err(|e| ProvisionError::Setup(format!("cannot write .nspawn file: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha512_bytes(data: &[u8]) -> String {
        use sha2::{Digest, Sha512};
        format!("{:x}", Sha512::digest(data))
    }

    fn specs() -> ClientSpecs {
        ClientSpecs {
            chost: SUPPORTED_CHOST.to_string(),
            cflags: "-march=znver3 -O2 -pipe".to_string(),
            cpu_flags_x86: vec!["aes".into(), "avx2".into()],
            use_flags: vec!["ssl".into(), "-X".into()],
            profile: "default/linux/amd64/23.0/desktop".to_string(),
        }
    }

    #[test]
    fn names_accept_safe_segments() {
        for good in ["laptop", "gw2", "a-b_c.d", "X1"] {
            assert_eq!(validate_name(good).unwrap(), good);
        }
    }

    #[test]
    fn names_reject_escapes_and_garbage() {
        for bad in [
            "", "  ", "..", ".", "a/b", "../x", "a..b", "-x", ".x", "x y", "x$y", "über",
        ] {
            assert!(validate_name(bad).is_err(), "accepted '{bad}'");
        }
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn specs_accept_client_settings() {
        assert!(validate_specs(&specs()).is_ok());
    }

    #[test]
    fn specs_reject_bad_chost_and_native_march() {
        let mut s = specs();
        s.chost = "x86_64-pc-linux-musl".into();
        assert!(validate_specs(&s).unwrap_err().contains("CHOST"));
        let mut s = specs();
        s.cflags = "-march=native -O2 -pipe".into();
        assert!(validate_specs(&s).unwrap_err().contains("-march=native"));
    }

    #[test]
    fn specs_reject_shell_metacharacters() {
        for cflags in [
            "\"-O2\"; rm -rf /",
            "$(id)",
            "`id`",
            "${COMMON_FLAGS}",
            "line\nCFLAGS=\"x\"",
        ] {
            let mut s = specs();
            s.cflags = cflags.into();
            assert!(validate_specs(&s).is_err(), "accepted '{cflags}'");
        }
        let mut s = specs();
        s.cpu_flags_x86 = vec!["avx2;id".into()];
        assert!(validate_specs(&s).is_err());
        let mut s = specs();
        s.use_flags = vec!["a/b".into()];
        assert!(validate_specs(&s).is_err());
    }

    #[test]
    fn specs_reject_bad_profiles() {
        for profile in [
            "",
            "/absolute",
            "trailing/",
            "a//b",
            "../escape",
            "a/../../b",
            "has space/x",
        ] {
            let mut s = specs();
            s.profile = profile.into();
            assert!(validate_specs(&s).is_err(), "accepted '{profile}'");
        }
    }

    #[test]
    fn march_extraction_last_wins() {
        assert_eq!(
            march_from_cflags("-O2 -march=znver1 -march=znver3 -pipe").as_deref(),
            Some("znver3")
        );
        assert_eq!(march_from_cflags("-O2 -pipe"), None);
        assert_eq!(march_from_cflags("-march="), None);
    }

    #[test]
    fn make_conf_mirrors_hand_built_shape() {
        let text = render_make_conf(&specs(), 12);
        assert!(text.contains("COMMON_FLAGS=\"-march=znver3 -O2 -pipe\"\n"));
        assert!(text.contains("CFLAGS=\"${COMMON_FLAGS}\"\n"));
        assert!(text.contains("RUSTFLAGS=\"-C target-cpu=znver3\"\n"));
        assert!(text.contains("CPU_FLAGS_X86=\"aes avx2\"\n"));
        assert!(text.contains("USE=\"ssl -X\"\n"));
        assert!(text.contains("MAKEOPTS=\"-j12\"\n"));
        assert!(text.contains("FEATURES=\"buildpkg\"\n"));
    }

    #[test]
    fn make_conf_omits_empty_optionals() {
        let mut s = specs();
        s.cflags = "-O2 -pipe".into();
        s.cpu_flags_x86.clear();
        s.use_flags.clear();
        let text = render_make_conf(&s, 0);
        assert!(!text.contains("RUSTFLAGS"));
        assert!(!text.contains("CPU_FLAGS_X86"));
        assert!(!text.contains("\nUSE="));
        assert!(text.contains("MAKEOPTS=\"-j1\"\n"));
    }

    #[test]
    fn nspawn_shares_host_repo_and_binpkgs() {
        let text = render_nspawn(Path::new("/var/binserver/laptop"));
        assert!(text.contains("Boot=yes"));
        assert!(text.contains("Bind=/var/db/repos/gentoo"));
        assert!(text.contains("Bind=/var/cache/distfiles"));
        assert!(text.contains("Bind=/var/binserver/laptop:/var/cache/binpkgs"));
    }

    #[test]
    fn autobuilds_index_parsing_skips_comments() {
        let txt = "# comment\n\n20261106T000000Z/stage3-amd64-openrc-20261106T000000Z.tar.xz 123\n";
        assert_eq!(
            parse_autobuilds_txt(txt).as_deref(),
            Some("20261106T000000Z/stage3-amd64-openrc-20261106T000000Z.tar.xz")
        );
        assert_eq!(parse_autobuilds_txt("# only comments\n"), None);
    }

    const DIGESTS: &str = "-----BEGIN PGP SIGNED MESSAGE-----\n\
         Hash: SHA512\n\
         \n\
         # MD5 HASH\n\
         aaa  stage3-amd64-openrc-x.tar.xz\n\
         # SHA512 HASH\n\
         bbb  stage3-amd64-openrc-x.tar.xz\n\
         -----BEGIN PGP SIGNATURE-----\n";

    #[test]
    fn digest_parsing_prefers_sha512() {
        assert_eq!(
            parse_digest_sha512(DIGESTS, "stage3-amd64-openrc-x.tar.xz").as_deref(),
            Some("bbb")
        );
        assert_eq!(parse_digest_sha512(DIGESTS, "other.tar.xz"), None);
        let sha256_only = "# SHA256 HASH\nccc  f.tar.xz\n";
        assert_eq!(
            parse_digest_sha512(sha256_only, "f.tar.xz").as_deref(),
            Some("ccc")
        );
    }

    #[test]
    fn sha512_file_matches_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"gentoo").unwrap();
        assert_eq!(sha512_file(&path).unwrap(), sha512_bytes(b"gentoo"));
        assert!(sha512_file(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn image_files_written_and_profile_checked() {
        let dir = tempfile::tempdir().unwrap();
        let host_repo = dir.path().join("repo");
        std::fs::create_dir_all(host_repo.join("profiles/default/linux/amd64/23.0/desktop"))
            .unwrap();
        let root = dir.path().join("root");
        write_image_files(&root, &specs(), 4, &host_repo).unwrap();
        let make_conf = std::fs::read_to_string(root.join("etc/portage/make.conf")).unwrap();
        assert!(make_conf.contains("COMMON_FLAGS=\"-march=znver3 -O2 -pipe\""));
        assert!(
            std::fs::read_to_string(root.join("etc/portage/repos.conf/gentoo.conf"))
                .unwrap()
                .contains("auto-sync = no")
        );
        assert_eq!(
            std::fs::read_link(root.join("etc/portage/make.profile")).unwrap(),
            PathBuf::from("/var/db/repos/gentoo/profiles/default/linux/amd64/23.0/desktop")
        );
        let mut s = specs();
        s.profile = "default/linux/amd64/23.0/nonexistent".into();
        assert!(write_image_files(&root, &s, 4, &host_repo).is_err());
    }

    #[test]
    fn unpack_argv_preserves_xattrs_and_owners() {
        let (program, args) = unpack_argv(Path::new("s.tar.xz"), Path::new("/root"));
        assert_eq!(program, "tar");
        assert_eq!(
            args,
            vec![
                "xpf",
                "s.tar.xz",
                "-C",
                "/root",
                "--xattrs-include=*.*",
                "--numeric-owner"
            ]
        );
    }
}
