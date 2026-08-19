//! End-to-end smoke test against a real homeserver.
//!
//! This test is ignored by default because it registers a throwaway account on
//! a live homeserver and sends a real message. Point it at a dev homeserver
//! (default: `http://localhost:8008`, e.g. a local Synapse started for testing)
//! and run it manually with:
//!
//! ```sh
//! MDC_HOMESERVER=http://localhost:8008 \
//!   cargo test --test smoke_matrix -- --ignored --nocapture
//! ```
//!
//! It validates the full MVP loop through the app's own `backend` module:
//! login → sync → room creation → timeline → send → message appears.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
    time::{SystemTime, UNIX_EPOCH},
};

use matrix_desktop_client::backend;
use matrix_sdk::{
    ruma::{
        api::client::{
            account::register::v3::Request as RegisterRequest,
            room::create_room::v3::Request as CreateRoomRequest,
            uiaa::{AuthData, Dummy, UiaaResponse},
        },
        events::room::message::MessageType,
        OwnedRoomId,
    },
    Client,
};
use matrix_sdk_ui::timeline::TimelineItem;
use tokio::sync::mpsc;

fn homeserver() -> String {
    std::env::var("MDC_HOMESERVER").unwrap_or_else(|_| "http://localhost:8008".to_owned())
}

/// Extract the plaintext body of a timeline item, if it is a text message.
fn message_body(item: &TimelineItem) -> Option<String> {
    let event = item.as_event()?;
    let content = event.content();
    let message = content.as_message()?;
    match message.msgtype() {
        MessageType::Text(text) => Some(text.body.clone()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access and registers a throwaway account on matrix.org"]
async fn login_sync_create_room_send_and_receive() {
    let start = Instant::now();
    let mark = |label: &str| eprintln!("[smoke] {:>6.1}s  {label}", start.elapsed().as_secs_f64());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let username = format!("mdc-smoke-{stamp}");
    let password = format!("hunter2-{stamp}-pass");

    // --- register a throwaway account -------------------------------------
    let homeserver = homeserver();
    let server_name = url::Url::parse(&homeserver)
        .expect("valid homeserver URL")
        .host_str()
        .unwrap_or("localhost")
        .to_owned();

    let client = Client::builder()
        .homeserver_url(&homeserver)
        .build()
        .await
        .expect("client build");
    let mut request = RegisterRequest::new();
    request.username = Some(username.clone());
    request.password = Some(password.clone());
    request.initial_device_display_name = Some("mdc-smoke".to_owned());

    let register = async {
        match client.matrix_auth().register(request.clone()).await {
            Ok(_) => Ok(()),
            // Synapse (and most homeservers) gate registration behind UIA;
            // complete the dummy stage and retry.
            Err(e) if matches!(e.as_ruma_api_error(), Some(UiaaResponse::AuthResponse(_))) => {
                let session = match e.as_ruma_api_error() {
                    Some(UiaaResponse::AuthResponse(info)) => info.session.clone(),
                    _ => None,
                };
                let mut retry = request.clone();
                let mut dummy = Dummy::new();
                dummy.session = session;
                retry.auth = Some(AuthData::Dummy(dummy));
                client.matrix_auth().register(retry).await.map(|_| ())
            }
            Err(other) => Err(other),
        }
    };
    register
        .await
        .expect("registration — the dev homeserver must allow open registration");
    mark("registered");

    // --- login through the app's backend ----------------------------------
    let login_username = username.clone();
    let login_password = password.clone();
    let auth = backend::login(homeserver.clone(), login_username, login_password)
        .await
        .expect("login");
    assert_eq!(auth.user_id, format!("@{username}:{server_name}"));
    mark("backend::login done");

    mark("register+login done");
    // --- start the sync loop ----------------------------------------------
    let shutdown = Arc::new(AtomicBool::new(false));
    let (events_tx, mut events_rx) = mpsc::channel(32);
    let sync_task = backend::start_sync(auth.client.clone(), shutdown.clone(), events_tx.clone());
    mark("sync started");

    // --- create a room (works immediately; sync picks it up) -----------------
    let room = auth
        .client
        .create_room(CreateRoomRequest::new())
        .await
        .expect("create room");
    mark("room created");

    // --- turn on end-to-end encryption in the room --------------------------
    room.enable_encryption().await.expect("enable encryption");
    mark("encryption enabled");

    let room_id: OwnedRoomId = room.room_id().to_owned();

    // --- wait until the sync loop learns about the new room ------------------
    let mut room_synced = false;
    let mut encrypted = false;
    for _ in 0..120 {
        if let Some(room) = auth.client.get_room(&room_id) {
            room_synced = true;
            // Wait for the m.room.encryption state event to arrive as well.
            encrypted = matches!(
                room.encryption_state(),
                matrix_sdk::EncryptionState::Encrypted
            );
            if room_synced && encrypted {
                break;
            }
        }
        // Surface sync errors instead of silently waiting.
        while let Ok(event) = events_rx.try_recv() {
            if let backend::BackendEvent::SyncError(msg) = event {
                panic!("sync error: {msg}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        room_synced,
        "new room did not appear within 60s — sync likely failed"
    );
    assert!(
        encrypted,
        "room never became encrypted after enable_encryption()"
    );
    mark("room synced and encrypted");

    let mut live = backend::open_timeline(&auth.client, room_id.clone())
        .await
        .expect("open timeline");
    mark("timeline open");

    // --- send a message and confirm it lands in the timeline ---------------
    const MESSAGE: &str = "hello from matrix-desktop-client smoke test";
    backend::send_text(&live.timeline, MESSAGE)
        .await
        .expect("send");
    mark("message sent (encrypted)");

    // The message must come back DECRYPTED (not "unable to decrypt"), proving
    // the Olm/Megolm path works end to end.
    let mut found = false;
    let mut undecryptable = false;
    for _ in 0..40 {
        // Drain diffs into the items vector, like the UI does.
        while let Ok(diffs) = live.diffs_rx.try_recv() {
            for diff in diffs {
                diff.apply(&mut live.items);
            }
        }
        for item in live.items.iter() {
            match message_body(item) {
                Some(body) if body == MESSAGE => found = true,
                Some(body) if body.contains("Unable to decrypt") => undecryptable = true,
                _ => {}
            }
        }
        if found {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        !undecryptable,
        "received an undecryptable message — E2EE setup failed"
    );
    assert!(found, "sent message never appeared in the timeline");

    // --- the message must also be sent (not stuck in the queue) ------------
    // The local echo's `send_state` transitions `NotSentYet -> Sent`, then the
    // remote echo (with an event id) replaces it. Both are proof of delivery.
    let mut sent = false;
    for _ in 0..40 {
        while let Ok(diffs) = live.diffs_rx.try_recv() {
            for diff in diffs {
                diff.apply(&mut live.items);
            }
        }
        sent = live.items.iter().any(|item| {
            message_body(item).as_deref() == Some(MESSAGE)
                && item.as_event().is_some_and(|e| {
                    e.event_id().is_some()
                        || matches!(
                            e.send_state(),
                            Some(matrix_sdk_ui::timeline::EventSendState::Sent { .. })
                        )
                })
        });
        if sent {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    if !sent {
        let states: Vec<String> = live
            .items
            .iter()
            .filter(|item| message_body(item).as_deref() == Some(MESSAGE))
            .map(|item| {
                item.as_event()
                    .map(|e| {
                        format!(
                            "event_id={:?} local_echo={} send_state={:?}",
                            e.event_id(),
                            e.is_local_echo(),
                            e.send_state()
                        )
                    })
                    .unwrap_or_else(|| "(not an event)".to_owned())
            })
            .collect();
        panic!("message never reached the server — timeline states: {states:?}");
    }

    // --- session persistence: restore the session like an app restart ------
    // Grab the persisted session, stop the first client entirely, then restore
    // a fresh client from disk and verify it can still see and use the room.
    let persisted = auth
        .persisted
        .clone()
        .expect("fresh login must persist a session");

    shutdown.store(true, Ordering::Relaxed);
    sync_task.abort();
    drop(live);
    drop(auth);
    mark("first session torn down");

    let restored = backend::restore(persisted).await.expect("restore session");
    mark("session restored from disk");

    let shutdown2 = Arc::new(AtomicBool::new(false));
    let (events_tx2, mut events_rx2) = mpsc::channel(32);
    let sync_task2 = backend::start_sync(
        restored.client.clone(),
        shutdown2.clone(),
        events_tx2.clone(),
    );

    // The restored client must reload the room from its store after the first
    // sync, and must still be able to send (E2EE keys came back with it).
    let mut room_back = false;
    for _ in 0..60 {
        if restored.client.get_room(&room_id).is_some() {
            room_back = true;
            break;
        }
        while let Ok(event) = events_rx2.try_recv() {
            if let backend::BackendEvent::SyncError(msg) = event {
                panic!("sync error after restore: {msg}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(room_back, "room did not come back after session restore");
    mark("room reloaded after restore");

    let mut live2 = backend::open_timeline(&restored.client, room_id)
        .await
        .expect("reopen timeline after restore");
    backend::send_text(&live2.timeline, MESSAGE)
        .await
        .expect("send after restore");

    let mut restored_send_ok = false;
    for _ in 0..40 {
        while let Ok(diffs) = live2.diffs_rx.try_recv() {
            for diff in diffs {
                diff.apply(&mut live2.items);
            }
        }
        restored_send_ok = live2.items.iter().any(|item| {
            message_body(item).as_deref() == Some(MESSAGE)
                && item.as_event().is_some_and(|e| {
                    e.event_id().is_some()
                        || matches!(
                            e.send_state(),
                            Some(matrix_sdk_ui::timeline::EventSendState::Sent { .. })
                        )
                })
        });
        if restored_send_ok {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        restored_send_ok,
        "could not send a message after restoring the session"
    );
    mark("message sent after restore");

    // Clean up: stop the sync loop and deactivate the throwaway account.
    // Time-box cleanup so an unresponsive server can't slow the test down.
    shutdown2.store(true, Ordering::Relaxed);
    sync_task2.abort();
    drop(live2);
    drop(restored);
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.matrix_auth().logout(),
    )
    .await;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client.account().deactivate(None, None, true),
    )
    .await;
}
