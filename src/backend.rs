//! Matrix backend: everything that talks to the homeserver through
//! matrix-rust-sdk. The UI never calls the SDK directly — it drives these
//! async functions through `poll_promise` and drains the events channel.

use std::sync::Arc;

use eyeball_im::{Vector, VectorDiff};
use futures_util::StreamExt;
use matrix_sdk::{
    config::SyncSettings,
    encryption::identities::UserIdentity,
    ruma::{
        api::client::{
            filter::FilterDefinition,
            uiaa::{AuthData, MatrixUserIdentifier, Password, UserIdentifier},
        },
        events::{room::message::RoomMessageEventContent, AnyMessageLikeEventContent},
        OwnedRoomId, RoomOrAliasId,
    },
    Client, LoopCtrl,
};
use matrix_sdk_ui::timeline::{RoomExt, Timeline, TimelineItem};
use tokio::sync::mpsc;

use crate::session::PersistedSession;

/// Result of a successful login or session restore.
#[derive(Clone)]
pub struct AuthResult {
    /// The logged-in SDK client.
    pub client: Arc<Client>,
    /// The `whoami`-style identity (user id).
    pub user_id: String,
    /// The device id of this session.
    pub device_id: String,
    /// The homeserver base URL the client is connected to.
    pub homeserver: String,
    /// For fresh logins: the session data that should be persisted to disk.
    /// `None` when the session was restored from disk.
    pub persisted: Option<PersistedSession>,
    /// The account password entered at login, used only for password-based
    /// UIA during first-run cross-signing bootstrap. `Some` for a fresh login,
    /// `None` when the session was restored from disk (no password known).
    pub account_password: Option<String>,
}

/// Events produced by background tasks (sync loop, crypto bootstrap) and
/// consumed by the UI each frame.
#[derive(Debug)]
pub enum BackendEvent {
    /// The sync loop ended with an error (e.g. network outage). The UI should
    /// show it and let the user retry.
    SyncError(String),
    /// Encryption bootstrap / verification status update.
    Crypto(String),
    /// Whether our own cross-signing identity is verified (`true`/`false`).
    Verification(bool),
}

/// A live, subscribed timeline for the currently selected room.
pub struct LiveTimeline {
    /// Room this timeline belongs to.
    pub room_id: OwnedRoomId,
    /// The SDK timeline handle (kept alive; also used for sending). `Arc`
    /// because `Timeline` itself is not `Clone` in matrix-sdk-ui 0.18.
    pub timeline: Arc<Timeline>,
    /// The visible items. Updated in the UI thread by applying the diffs
    /// received on `diffs_rx`.
    pub items: Vector<Arc<TimelineItem>>,
    /// Channel carrying `VectorDiff`s from the background subscription task.
    pub diffs_rx: mpsc::Receiver<Vec<VectorDiff<Arc<TimelineItem>>>>,
}

/// Log in with a username + password on the given homeserver.
///
/// Returns an [`AuthResult`] ready to hand to the UI. Errors are returned as
/// human-readable strings so the UI can display them without panicking.
pub async fn login(
    homeserver: String,
    username: String,
    password: String,
) -> Result<AuthResult, String> {
    if homeserver.trim().is_empty() {
        return Err("Please enter a homeserver URL, e.g. https://matrix.org".to_owned());
    }
    if username.trim().is_empty() || password.is_empty() {
        return Err("Username and password are required".to_owned());
    }
    let homeserver = validate_homeserver(&homeserver)?;

    let db_path = crate::session::fresh_db_path()
        .ok_or_else(|| "could not determine the user config directory".to_owned())?;
    let passphrase = crate::session::fresh_passphrase();

    let client = build_client(&homeserver, &db_path, &passphrase).await?;

    let response = client
        .matrix_auth()
        .login_username(username.trim(), &password)
        .initial_device_display_name("matrix-desktop-client")
        .await
        .map_err(|e| format!("Login failed: {e}"))?;

    tracing::info!(user = ?response.user_id, "logged in");

    let session = client
        .matrix_auth()
        .session()
        .ok_or_else(|| "Logged in but no session was available".to_owned())?;

    let persisted = PersistedSession::new(homeserver.clone(), db_path, passphrase, session);

    Ok(AuthResult {
        client: Arc::new(client),
        user_id: response.user_id.to_string(),
        device_id: response.device_id.to_string(),
        homeserver,
        persisted: Some(persisted),
        account_password: Some(password),
    })
}

/// Restore a previously persisted session and reconnect.
pub async fn restore(persisted: PersistedSession) -> Result<AuthResult, String> {
    let client = build_client(
        &persisted.homeserver,
        &persisted.db_path,
        &persisted.passphrase,
    )
    .await?;

    client
        .restore_session(persisted.session.clone())
        .await
        .map_err(|e| format!("Could not restore the previous session: {e}"))?;

    let user_id = client
        .user_id()
        .map(ToString::to_string)
        .unwrap_or_else(|| "(unknown user)".to_owned());
    let device_id = client
        .device_id()
        .map(ToString::to_string)
        .unwrap_or_else(|| "(unknown device)".to_owned());

    tracing::info!(user = %user_id, "restored session");

    Ok(AuthResult {
        client: Arc::new(client),
        user_id,
        device_id,
        homeserver: persisted.homeserver.clone(),
        persisted: None,
        account_password: None,
    })
}

/// Log out: revoke the access token on the server.
///
/// The caller is responsible for clearing local state afterwards, even when
/// this returns an error (the token may already be revoked).
pub async fn logout(client: &Client) -> Result<(), String> {
    client
        .logout()
        .await
        .map_err(|e| format!("Logout failed: {e}"))
}

/// Build a client for the given homeserver, using the SQLite store (state +
/// crypto) at `db_path`, encrypted with `passphrase`.
async fn build_client(
    homeserver: &str,
    db_path: &std::path::Path,
    passphrase: &str,
) -> Result<Client, String> {
    // Errors are inspected so that unreachable/invalid homeservers produce a
    // visible message instead of a panic.
    Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(db_path, Some(passphrase))
        .build()
        .await
        .map_err(|e| match e {
            matrix_sdk::ClientBuildError::AutoDiscovery(e) => {
                format!("Could not discover the homeserver at {homeserver}: {e}")
            }
            matrix_sdk::ClientBuildError::Url(e) => {
                format!("Invalid homeserver URL '{homeserver}': {e}")
            }
            matrix_sdk::ClientBuildError::Http(e) => {
                format!("Could not reach the homeserver at {homeserver}: {e}")
            }
            other => format!("Failed to build the client: {other}"),
        })
}

/// Validate a homeserver URL entered by the user.
///
/// Accepts `https://` and `http://` (the latter mainly for local Synapse
/// development); rejects anything that is not a valid URL or not http(s).
pub fn validate_homeserver(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    let url =
        url::Url::parse(trimmed).map_err(|e| format!("'{trimmed}' is not a valid URL: {e}"))?;
    match url.scheme() {
        "https" | "http" => Ok(trimmed.to_owned()),
        scheme => Err(format!(
            "Unsupported homeserver scheme '{scheme}'. Use an https:// URL (or http:// for a local Synapse)."
        )),
    }
}

/// Start the background sync loop.
///
/// Runs until `shutdown` is set, in which case the loop exits cleanly with
/// `LoopCtrl::Break`. Any sync error is forwarded to the UI via `events`.
pub fn start_sync(
    client: Arc<Client>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    events: mpsc::Sender<BackendEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Lazy-load room members to keep the initial sync fast on big accounts.
        let filter = FilterDefinition::with_lazy_loading();
        let sync_settings = SyncSettings::default().filter(filter.into());

        tracing::info!("starting sync loop");
        let result = client
            .sync_with_result_callback(sync_settings, |sync_result| {
                let shutdown = shutdown.clone();
                let events = events.clone();
                async move {
                    match sync_result {
                        Ok(_response) => {
                            if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                                return Ok(LoopCtrl::Break);
                            }
                            Ok(LoopCtrl::Continue)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "sync error");
                            let _ = events
                                .send(BackendEvent::SyncError(format!("Sync error: {e}")))
                                .await;
                            Ok(LoopCtrl::Continue)
                        }
                    }
                }
            })
            .await;

        if let Err(e) = result {
            tracing::error!(error = %e, "sync loop ended");
            let _ = events
                .send(BackendEvent::SyncError(format!("Sync loop ended: {e}")))
                .await;
        }
    })
}

/// Open a live timeline for the given room: builds the SDK timeline,
/// subscribes to item changes and spawns a task that forwards diffs to a
/// channel the UI drains each frame.
pub async fn open_timeline(client: &Client, room_id: OwnedRoomId) -> Result<LiveTimeline, String> {
    let room = client
        .get_room(&room_id)
        .ok_or_else(|| format!("Room {room_id} is no longer available"))?;

    let timeline = room
        .timeline()
        .await
        .map_err(|e| format!("Could not open the timeline for {room_id}: {e}"))?;

    let (items, mut stream) = timeline.subscribe().await;

    let (tx, diffs_rx) = mpsc::channel::<Vec<VectorDiff<Arc<TimelineItem>>>>(256);
    tokio::spawn(async move {
        while let Some(diffs) = stream.next().await {
            if tx.send(diffs).await.is_err() {
                break;
            }
        }
    });

    Ok(LiveTimeline {
        room_id,
        timeline: Arc::new(timeline),
        items,
        diffs_rx,
    })
}

/// Join a room given a `!room:id` or `#alias:server`.
pub async fn join_room(client: &Client, id_or_alias: &str) -> Result<(), String> {
    let parsed = RoomOrAliasId::parse(id_or_alias.trim())
        .map_err(|e| format!("'{id_or_alias}' is not a valid room id or alias: {e}"))?;
    let room = client
        .join_room_by_id_or_alias(&parsed, &[])
        .await
        .map_err(|e| format!("Could not join {id_or_alias}: {e}"))?;
    tracing::info!(room_id = %room.room_id(), "joined room");
    Ok(())
}

/// Accept an invitation (equivalent to joining the invited room).
pub async fn accept_invite(client: &Client, room_id: &OwnedRoomId) -> Result<(), String> {
    let room = client
        .get_room(room_id)
        .ok_or_else(|| format!("Room {room_id} is no longer available"))?;
    room.join()
        .await
        .map_err(|e| format!("Could not accept the invitation: {e}"))?;
    Ok(())
}

/// Send a text message to the given timeline.
///
/// The message shows up in the timeline as a local echo immediately; its final
/// `send_state` (sent / failed) is reflected through the diff stream.
pub async fn send_text(timeline: &Timeline, text: &str) -> Result<(), String> {
    let content =
        AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent::text_plain(text.trim()));
    timeline
        .send(content)
        .await
        .map_err(|e| format!("Could not send the message: {e}"))?;
    Ok(())
}

/// Bootstrap cross-signing for a fresh login (first-run encryption setup).
///
/// Uses the SDK's convenience helper with password-based UIA. Failures are
/// reported through the events channel — the app keeps working either way.
pub async fn bootstrap_crypto(
    client: Arc<Client>,
    password: String,
    events: mpsc::Sender<BackendEvent>,
) {
    // Password-based UIA: only possible when we know the user id and password
    // (i.e. a fresh login). If we can't construct it, bootstrap without auth.
    let auth_data = client.user_id().map(|user_id| {
        AuthData::Password(Password::new(
            UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.to_string())),
            password,
        ))
    });

    let result = client
        .encryption()
        .bootstrap_cross_signing_if_needed(auth_data)
        .await;

    let msg = match result {
        Ok(()) => "Cross-signing is set up — your session is ready for verified E2EE.".to_owned(),
        Err(e) => format!("Encryption setup was skipped: {e}. You can still chat, but other devices may not trust this one yet."),
    };
    let _ = events.send(BackendEvent::Crypto(msg)).await;
}

/// Report whether our own identity is verified (cross-signing trusted).
pub async fn check_verification(client: Arc<Client>, events: mpsc::Sender<BackendEvent>) {
    let result = verify_own_identity(&client).await;
    match result {
        Ok(Some(verified)) => {
            let _ = events.send(BackendEvent::Verification(verified)).await;
        }
        Ok(None) => {
            let _ = events
                .send(BackendEvent::Crypto(
                    "No cross-signing identity found yet — keep the app open while the session syncs.".to_owned(),
                ))
                .await;
        }
        Err(e) => {
            let _ = events
                .send(BackendEvent::Crypto(format!(
                    "Could not check E2EE status: {e}"
                )))
                .await;
        }
    }
}

/// Query our own [`UserIdentity`] and report whether it is verified.
async fn verify_own_identity(client: &Client) -> Result<Option<bool>, String> {
    let Some(user_id) = client.user_id() else {
        return Ok(None);
    };
    let identity: Option<UserIdentity> = client
        .encryption()
        .get_user_identity(user_id)
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(identity.map(|i| i.is_verified()))
}

/// Best-effort display name for a room, computed synchronously from the
/// already-synced state (never blocks on the network).
pub fn room_display_name(room: &matrix_sdk::Room) -> String {
    if let Some(name) = room.cached_display_name() {
        return name.to_string();
    }
    if let Some(name) = room.name().filter(|n| !n.is_empty()) {
        return name;
    }
    if let Some(alias) = room.canonical_alias() {
        return alias.to_string();
    }
    room.room_id().to_string()
}

/// True when the room is E2EE-encrypted (as far as we know).
pub fn room_is_encrypted(room: &matrix_sdk::Room) -> bool {
    use matrix_sdk::{EncryptionState, RoomState};
    matches!(
        (room.state(), room.encryption_state()),
        (RoomState::Joined, EncryptionState::Encrypted)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homeserver_validation_accepts_https() {
        assert_eq!(
            validate_homeserver("https://matrix.org").unwrap(),
            "https://matrix.org"
        );
        assert_eq!(
            validate_homeserver("  https://example.com  ").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn homeserver_validation_accepts_local_http() {
        assert_eq!(
            validate_homeserver("http://localhost:8008").unwrap(),
            "http://localhost:8008"
        );
    }

    #[test]
    fn homeserver_validation_rejects_garbage() {
        assert!(validate_homeserver("not a url").is_err());
        assert!(validate_homeserver("ftp://matrix.org").is_err());
        assert!(validate_homeserver("").is_err());
    }

    #[test]
    fn homeserver_validation_rejects_empty() {
        assert!(validate_homeserver("   ").is_err());
    }
}
