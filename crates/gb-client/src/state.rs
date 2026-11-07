//! Client-side state: which build this machine submitted last.
//!
//! Stored outside the config file, under `$XDG_STATE_HOME/gb-client/`
//! (`~/.local/state/gb-client/` when unset). `cancel` with no id uses it
//! to target your own last build instead of someone else's.

use std::path::PathBuf;
use uuid::Uuid;

/// Where the last submitted build id lives. `None` when no home is known.
pub fn state_file_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("gb-client").join("last_build_id"))
}

/// Remember a submitted build. Best-effort and silent: tracking state must
/// never fail the build that was just submitted.
pub fn record_last_build(id: Uuid) {
    let Some(path) = state_file_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let _ = std::fs::write(path, id.to_string());
}

/// The last submitted build id, if any. Missing files and corrupt contents
/// both yield `None` (as if nothing was ever submitted).
pub fn read_last_build() -> Option<Uuid> {
    let path = state_file_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    Uuid::parse_str(text.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// XDG_STATE_HOME is process-global, so all state cases run serially in
    /// this one test (no other test in this binary touches that variable).
    #[test]
    fn record_and_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let state_home = tmp.path().join("state");
        let prior = std::env::var("XDG_STATE_HOME").ok();
        // SAFETY: this is the only test in the binary that touches
        // XDG_STATE_HOME, and the value is restored afterwards.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };

        // Nothing recorded yet.
        assert_eq!(read_last_build(), None);
        // Record + read back.
        let id = Uuid::new_v4();
        record_last_build(id);
        assert_eq!(
            state_file_path().unwrap(),
            state_home.join("gb-client/last_build_id")
        );
        assert_eq!(read_last_build(), Some(id));
        // A newer submission overwrites.
        let id2 = Uuid::new_v4();
        record_last_build(id2);
        assert_eq!(read_last_build(), Some(id2));
        // Corrupt contents read as "nothing submitted".
        std::fs::write(state_file_path().unwrap(), "not-a-uuid\n").unwrap();
        assert_eq!(read_last_build(), None);

        unsafe {
            match prior {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }
}
