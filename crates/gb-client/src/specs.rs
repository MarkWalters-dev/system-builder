//! Gather this machine's Portage specs for provisioning.
//!
//! `gb-client provision` sends [`gb_types::ClientSpecs`] so the server can
//! compile the new image with the client's own settings: `CHOST`,
//! `CFLAGS`, `CPU_FLAGS_X86`, `USE` (all from `emerge --info`, falling
//! back to `cpuid2cpuflags` for CPU flags) and the active profile (from
//! `/etc/portage/make.profile`). A literal `-march=native` is resolved
//! to a concrete `-march=<cpu>` via gcc — `native` inside the builder
//! container would describe the *server's* CPU.

use gb_types::ClientSpecs;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SpecsError {
    #[error("cannot run {0}: {1}")]
    Command(String, #[source] std::io::Error),
    #[error("{0} failed: {1}")]
    Failed(String, String),
    #[error("cannot parse {0}: {1}")]
    Parse(String, String),
    #[error("{0}")]
    Missing(String),
}

/// Parsed `emerge --info` subset. `cpu_flags_x86` is `None` when the key
/// is absent or blank (caller falls back to `cpuid2cpuflags`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EmergeInfo {
    pub chost: Option<String>,
    pub cflags: Option<String>,
    pub cpu_flags_x86: Option<Vec<String>>,
    pub use_flags: Vec<String>,
}

/// Parse `KEY="value"` pairs from `emerge --info`. Later duplicates
/// win. Real output packs many pairs per line (`USE="..." ABI_X86="64"
/// ...`), so lines are scanned pair by pair, not split on `=`.
pub fn parse_emerge_info(text: &str) -> EmergeInfo {
    let mut info = EmergeInfo::default();
    for line in text.lines() {
        for (key, value) in parse_pairs(line) {
            match key.as_str() {
                "CHOST" if !value.is_empty() => info.chost = Some(value),
                "CFLAGS" if !value.is_empty() => info.cflags = Some(value),
                "CPU_FLAGS_X86" => {
                    let flags: Vec<String> = value.split_whitespace().map(str::to_string).collect();
                    info.cpu_flags_x86 = if flags.is_empty() { None } else { Some(flags) };
                }
                "USE" => {
                    info.use_flags = value.split_whitespace().map(str::to_string).collect();
                }
                _ => {}
            }
        }
    }
    info
}

/// Split one line into `KEY=value` pairs. Quoted values may contain
/// spaces; bare values run to the next whitespace. Anything else on
/// the line (prose, `Unset:` lists) is skipped.
fn parse_pairs(line: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut pairs = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
            i += 1;
        }
        let ks = i;
        while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if ks == i {
            break;
        }
        let key: String = chars[ks..i].iter().collect();
        if i >= chars.len() || chars[i] != '=' {
            continue;
        }
        i += 1; // '='
        let value = if i < chars.len() && chars[i] == '"' {
            i += 1;
            let vs = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            let value: String = chars[vs..i].iter().collect();
            if i < chars.len() {
                i += 1; // closing quote
            }
            value
        } else {
            let vs = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            chars[vs..i].iter().collect()
        };
        pairs.push((key, value));
    }
    pairs
}

/// Tolerant parse of `cpuid2cpuflags` output: a bare flag list, a
/// `KEY: flags` line, or a `KEY="flags"` line all work.
pub fn parse_cpu_flags_output(text: &str) -> Vec<String> {
    let text = text.trim();
    let inner = match (text.find('"'), text.rfind('"')) {
        (Some(a), Some(b)) if a < b => &text[a + 1..b],
        _ => match text.split_once(':') {
            Some((_, rest)) => rest,
            None => text,
        },
    };
    inner.split_whitespace().map(str::to_string).collect()
}

/// Profile path relative to `profiles/` from a `make.profile` link
/// target (e.g. `/var/db/repos/gentoo/profiles/default/linux/...`).
pub fn profile_from_link_target(target: &str) -> Option<String> {
    let (_, rel) = target.split_once("profiles/")?;
    if rel.is_empty() {
        return None;
    }
    Some(rel.trim_end_matches('/').to_string())
}

/// Concrete `-march` value from `gcc -march=native -Q --help=target`.
pub fn gcc_march_from_help(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-march=") {
            let arch = rest.split_whitespace().next().unwrap_or_default();
            if !arch.is_empty() && arch != "native" {
                return Some(arch.to_string());
            }
        }
    }
    None
}

fn run_capture(program: &str, args: &[&str]) -> Result<String, SpecsError> {
    let what = format!("{program} {}", args.join(" "));
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| SpecsError::Command(what.clone(), e))?;
    if !out.status.success() {
        return Err(SpecsError::Failed(
            what,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve a literal `-march=native` in CFLAGS to the concrete CPU via
/// gcc. Returned unchanged when no `native` is present.
pub fn resolve_native_march(cflags: &str) -> Result<String, SpecsError> {
    if !cflags.split_whitespace().any(|t| t == "-march=native") {
        return Ok(cflags.to_string());
    }
    let help = run_capture("gcc", &["-march=native", "-Q", "--help=target"])?;
    let arch = gcc_march_from_help(&help).ok_or_else(|| {
        SpecsError::Parse(
            "gcc -march=native -Q --help=target".to_string(),
            "no concrete -march found".to_string(),
        )
    })?;
    Ok(cflags.replace("-march=native", &format!("-march={arch}")))
}

/// Detect the active profile from `/etc/portage/make.profile`.
pub fn detect_profile() -> Result<String, SpecsError> {
    let target = std::fs::read_link("/etc/portage/make.profile").map_err(|e| {
        SpecsError::Missing(format!(
            "cannot read /etc/portage/make.profile ({e}); pass --profile explicitly"
        ))
    })?;
    profile_from_link_target(&target.to_string_lossy()).ok_or_else(|| {
        SpecsError::Missing(
            "cannot derive a repo profile from /etc/portage/make.profile; pass --profile explicitly"
                .to_string(),
        )
    })
}

/// Gather this machine's specs for a provision request. Fails loudly
/// outside Gentoo (no `emerge`) or without CPU flag sources.
pub fn gather_specs(profile_override: Option<String>) -> Result<ClientSpecs, SpecsError> {
    let info_text = run_capture("emerge", &["--info"]).map_err(|e| match e {
        SpecsError::Command(_, _) => SpecsError::Missing(
            "cannot run emerge --info (provisioning runs on Gentoo clients)".to_string(),
        ),
        other => other,
    })?;
    let info = parse_emerge_info(&info_text);
    let chost = info.chost.ok_or_else(|| {
        SpecsError::Parse("emerge --info".to_string(), "no CHOST found".to_string())
    })?;
    let cflags = info.cflags.ok_or_else(|| {
        SpecsError::Parse("emerge --info".to_string(), "no CFLAGS found".to_string())
    })?;
    let cpu_flags_x86 = match info.cpu_flags_x86 {
        Some(flags) => flags,
        None => {
            let out = run_capture("cpuid2cpuflags", &[]).map_err(|e| match e {
                SpecsError::Command(_, _) => SpecsError::Missing(
                    "no CPU_FLAGS_X86 in emerge --info and cpuid2cpuflags is missing \
                     (emerge app-portage/cpuid2cpuflags or set CPU_FLAGS_X86)"
                        .to_string(),
                ),
                other => other,
            })?;
            parse_cpu_flags_output(&out)
        }
    };
    let profile = match profile_override {
        Some(p) => p,
        None => detect_profile()?,
    };
    let cflags = resolve_native_march(&cflags)?;
    Ok(ClientSpecs {
        chost,
        cflags,
        cpu_flags_x86,
        use_flags: info.use_flags,
        profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "Portage 3.0.66.1 (python 3.12.3-final-0, default/linux/amd64/23.0/desktop, gcc-13, glibc-2.39)\n\
        System Settings\n\
        CHOST=\"x86_64-pc-linux-gnu\"\n\
        CFLAGS=\"-march=znver3 -O2 -pipe\"\n\
        CPU_FLAGS_X86=\"aes avx2 sse4_2\"\n\
        USE=\"X ssl -gtk\"\n";

    #[test]
    fn emerge_info_parses_wanted_keys() {
        let info = parse_emerge_info(INFO);
        assert_eq!(
            info,
            EmergeInfo {
                chost: Some("x86_64-pc-linux-gnu".into()),
                cflags: Some("-march=znver3 -O2 -pipe".into()),
                cpu_flags_x86: Some(vec!["aes".into(), "avx2".into(), "sse4_2".into()]),
                use_flags: vec!["X".into(), "ssl".into(), "-gtk".into()],
            }
        );
    }

    #[test]
    fn emerge_info_parses_packed_pairs_per_line() {
        // Real `emerge --info` packs many pairs per line.
        let info = parse_emerge_info(
            "USE=\"ssl -X zlib\" ABI_X86=\"64\" CPU_FLAGS_X86=\"aes avx2\" XTABLES_ADDONS=\"quota2\"\n\
             Unset:  ADDR2LINE, AR, AS\n\
             CFLAGS=\"-march=znver3 -O2 -pipe\" CHOST=\"x86_64-pc-linux-gnu\"\n",
        );
        assert_eq!(info.chost.as_deref(), Some("x86_64-pc-linux-gnu"));
        assert_eq!(info.cflags.as_deref(), Some("-march=znver3 -O2 -pipe"));
        assert_eq!(
            info.cpu_flags_x86,
            Some(vec!["aes".to_string(), "avx2".to_string()])
        );
        assert_eq!(info.use_flags, vec!["ssl", "-X", "zlib"]);
    }

    #[test]
    fn emerge_info_tolerates_missing_keys() {
        let info = parse_emerge_info("CHOST=\"x\"\nUSE=\"\"\n");
        assert_eq!(info.cflags, None);
        assert_eq!(info.cpu_flags_x86, None);
        assert!(info.use_flags.is_empty());
    }

    #[test]
    fn cpu_flags_output_shapes() {
        assert_eq!(parse_cpu_flags_output("aes avx2\n"), vec!["aes", "avx2"]);
        assert_eq!(
            parse_cpu_flags_output("CPU_FLAGS_X86=\"aes avx2\""),
            vec!["aes", "avx2"]
        );
        assert_eq!(
            parse_cpu_flags_output("CPU_FLAGS_X86: aes avx2"),
            vec!["aes", "avx2"]
        );
    }

    #[test]
    fn profile_link_shapes() {
        assert_eq!(
            profile_from_link_target(
                "/var/db/repos/gentoo/profiles/default/linux/amd64/23.0/desktop"
            )
            .as_deref(),
            Some("default/linux/amd64/23.0/desktop")
        );
        assert_eq!(
            profile_from_link_target(
                "../../../var/db/repos/gentoo/profiles/default/linux/amd64/23.0"
            )
            .as_deref(),
            Some("default/linux/amd64/23.0")
        );
        assert_eq!(profile_from_link_target("/etc/portage/profile"), None);
        assert_eq!(profile_from_link_target("profiles/"), None);
    }

    #[test]
    fn gcc_march_shapes() {
        let help = "The following options are target specific:\n  -march=                               znver3\n";
        assert_eq!(gcc_march_from_help(help).as_deref(), Some("znver3"));
        assert_eq!(gcc_march_from_help("nothing here"), None);
    }

    #[test]
    fn native_march_passthrough_without_native() {
        assert_eq!(
            resolve_native_march("-march=znver3 -O2").unwrap(),
            "-march=znver3 -O2"
        );
    }
}
