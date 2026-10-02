//! Shared filesystem helpers for send-rs' persistent files.
//!
//! `config.rs` owns the TOML/keymap logic but delegates the actual disk IO
//! here, and providers call the same helpers directly for their own artifacts
//! (WhatsApp's `wp_cache.json`, `wa.db`, …). Centralizing IO keeps two policies
//! in one place: parent-directory creation, and owner-only permissions for
//! files that hold session keys or message history.

use anyhow::{Result, anyhow};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::backend::Chat;

/// Unix owner read/write, nothing for group or world.
#[cfg(unix)]
const OWNER_ONLY: u32 = 0o600;

/// Create `path` and any missing parents.
pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Read a file as UTF-8, tightening it to owner-only as it is opened.
///
/// Repair is best-effort: a file we cannot `chmod` (e.g. one we do not own) is
/// still readable, so a failed repair only warns. Missing files propagate the
/// usual `NotFound` error to the caller.
pub(crate) fn read_to_string(path: &Path) -> io::Result<String> {
    if let Err(e) = repair_owner_only(path) {
        warn!("Could not tighten {} permissions: {e}", path.display());
    }

    std::fs::read_to_string(path)
}

/// Write `contents` to `path`, creating missing parents and the file
/// owner-only.
///
/// The mode is applied by `OpenOptions` at creation, so it does not depend on
/// the process umask. A pre-existing file keeps its old mode, so it is
/// tightened afterwards too.
pub(crate) fn write_owner_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(OWNER_ONLY);
    }

    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.flush()?;
    drop(file);

    repair_owner_only(path)
}

/// Tighten `path` to owner-only, removing any group/world bits.
///
/// Missing files are ignored: there is nothing to protect until the artifact
/// exists. Owner permission bits are preserved so a read-only file stays
/// read-only. On non-Unix platforms this is a no-op.
pub(crate) fn repair_owner_only(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };

        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(mode & !0o077);
            std::fs::set_permissions(path, permissions)?;
        }
    }

    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

/// Path to the TUI's chat-list cache (`chats.json`).
fn chats_path() -> Result<PathBuf> {
    Ok(crate::config::Config::user_config_dir()?.join("chats.json"))
}

/// Persist the TUI's chat-list cache for faster boots.
pub(crate) fn write_chats(chats: &[Chat]) -> Result<()> {
    // Unit tests run from a clean slate: never write into a real user's config
    // dir, which would leak into other tests' `AppState::new`.
    if cfg!(test) {
        return Ok(());
    }

    let raw = serde_json::to_string_pretty(chats)?;
    write_owner_only(&chats_path()?, raw.as_bytes())?;
    Ok(())
}

/// Load the TUI's chat-list cache. Tests always see an empty list (see
/// [`write_chats`]).
pub(crate) fn read_chats() -> Result<Vec<Chat>> {
    if cfg!(test) {
        return Ok(vec![]);
    }

    let path = chats_path()?;
    if !path.exists() {
        return Err(anyhow!("Path does not exist"));
    }

    Ok(serde_json::from_str(&read_to_string(&path)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A freshly written artifact is owner-only (and its parents exist)
    /// regardless of the umask the process happens to run under.
    #[test]
    fn write_owner_only_creates_owner_only_file_and_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("wp_cache.json");

        write_owner_only(&path, b"{}").unwrap();

        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600, "artifact must stay owner-only");

        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
    }

    /// A file that predates the policy (or was created by SQLite with the
    /// default mode) is repaired on the next read.
    #[test]
    fn read_tightens_broad_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wa.db");
        std::fs::write(&path, b"sqlite").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

            assert_eq!(read_to_string(&path).unwrap(), "sqlite");
            assert_eq!(mode_of(&path), 0o600);
        }
    }

    /// Repairing a file that does not exist yet is a no-op, not an error: the
    /// artifact may be created later by a provider that never starts.
    #[test]
    fn repair_missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        repair_owner_only(&dir.path().join("absent.db")).unwrap();
    }

    /// Owner-only files must not be made more permissive, and existing owner
    /// bits (e.g. a read-only cache) are preserved. Repair is idempotent.
    #[test]
    fn repair_is_idempotent_and_keeps_owner_bits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wa.db");
        std::fs::write(&path, b"x").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();

            repair_owner_only(&path).unwrap();
            repair_owner_only(&path).unwrap();

            assert_eq!(mode_of(&path), 0o400, "owner read-only must be preserved");
        }
    }
}
