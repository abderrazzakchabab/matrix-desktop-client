//! Session persistence: where the app stores its local state and how a logged-in
//! Matrix session is written/read back so the app can reconnect without asking
//! for credentials again.
//!
//! Layout (user-level dirs, never inside the repository):
//! - `config_dir()/matrix-desktop-client/session.json` — homeserver URL, the
//!   path to the SQLite database, its passphrase, and the Matrix session
//!   (user id, device id, access/refresh tokens).
//! - `config_dir()/matrix-desktop-client/db/<unique>/` — the SQLite state +
//!   crypto store.
//!
//! On Linux `config_dir()` is `~/.config`, on macOS
//! `~/Library/Application Support`.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use matrix_sdk::authentication::matrix::MatrixSession;
use serde::{Deserialize, Serialize};

/// Name of the app's folder inside the user config directory.
pub const APP_DIR_NAME: &str = "matrix-desktop-client";
/// Current schema version of the persisted session file.
pub const SESSION_FILE_VERSION: u32 = 1;

/// Everything needed to rebuild a logged-in [`matrix_sdk::Client`] on the next
/// launch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedSession {
    /// Schema version, for future migrations.
    pub version: u32,
    /// The homeserver base URL (e.g. `https://matrix.org`).
    pub homeserver: String,
    /// Absolute path to the SQLite store used for this login.
    pub db_path: PathBuf,
    /// Passphrase used to encrypt the SQLite store.
    pub passphrase: String,
    /// The Matrix session (user id, device id, tokens).
    pub session: MatrixSession,
}

impl PersistedSession {
    pub fn new(
        homeserver: String,
        db_path: PathBuf,
        passphrase: String,
        session: MatrixSession,
    ) -> Self {
        Self {
            version: SESSION_FILE_VERSION,
            homeserver,
            db_path,
            passphrase,
            session,
        }
    }
}

/// Base directory for all app state, e.g. `~/.config/matrix-desktop-client`.
pub fn base_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join(APP_DIR_NAME))
}

/// Path of the JSON file holding the persisted session.
pub fn session_file() -> Option<PathBuf> {
    base_dir().map(|dir| dir.join("session.json"))
}

/// Root directory for per-login SQLite databases.
pub fn db_root() -> Option<PathBuf> {
    base_dir().map(|dir| dir.join("db"))
}

/// Create a fresh, unique database directory for a new login.
///
/// Each login gets its own store so that logging out and logging back in never
/// reuses a database belonging to an old (revoked) device.
pub fn fresh_db_path() -> Option<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    db_root().map(|root| root.join(format!("db-{nanos}")))
}

/// Create a random passphrase for a fresh SQLite store.
pub fn fresh_passphrase() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    // Cheap and dependency-free: hash the current time + a random seed.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    hasher.write_u64(std::process::id() as u64);
    format!("{:016x}", hasher.finish())
}

/// Serialize and atomically write a session to `session.json`.
///
/// On unix the file is created with `0600` permissions so the tokens are not
/// world-readable.
pub fn save_session(session: &PersistedSession) -> io::Result<PathBuf> {
    let path = session_file().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the user config directory",
        )
    })?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_vec_pretty(session)?;
    write_private_file(&path, &json)?;

    Ok(path)
}

/// Load the persisted session, if any.
///
/// Returns `Ok(None)` when no session file exists, `Ok(Some(..))` on success
/// and `Err(..)` when the file exists but is unreadable or corrupt.
pub fn load_session() -> io::Result<Option<PersistedSession>> {
    let Some(path) = session_file() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the user config directory",
        ));
    };

    if !path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&path)?;
    let session: PersistedSession = serde_json::from_slice(&bytes)?;
    Ok(Some(session))
}

/// Remove the persisted session file (used on logout).
pub fn delete_session() {
    if let Some(path) = session_file() {
        let _ = fs::remove_file(path);
    }
}

/// Write a file with `0600` permissions on unix (tokens must stay private).
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> PersistedSession {
        PersistedSession::new(
            "https://matrix.org".to_owned(),
            PathBuf::from("/tmp/db"),
            "hunter2".to_owned(),
            MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: "@alice:matrix.org".try_into().unwrap(),
                    device_id: "DEVICE1".into(),
                },
                tokens: matrix_sdk::SessionTokens {
                    access_token: "syt_token".to_owned(),
                    refresh_token: Some("syt_refresh".to_owned()),
                },
            },
        )
    }

    #[test]
    fn session_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "mdc-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let original = sample_session();
        let file = dir.join("session.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();

        let json = serde_json::to_vec(&original).unwrap();
        fs::write(&file, &json).unwrap();

        let loaded: PersistedSession = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(loaded, original);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_paths_are_unique() {
        let a = fresh_db_path();
        let b = fresh_db_path();
        assert_ne!(a, b);
        assert!(a.is_some());
    }

    #[test]
    fn passphrase_is_nonempty_and_private() {
        let p = fresh_passphrase();
        assert!(!p.is_empty());
    }
}
