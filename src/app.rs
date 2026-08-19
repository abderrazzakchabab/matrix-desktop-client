//! The egui application: login screen, room list, timeline and composer.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use eframe::egui;
use matrix_sdk::{
    ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId},
    Client, EncryptionState,
};
use matrix_sdk_ui::timeline::{
    EventSendState, EventTimelineItem, TimelineDetails, TimelineEventShieldState, TimelineItem,
    TimelineItemKind, VirtualTimelineItem,
};
use poll_promise::Promise;
use tokio::sync::mpsc;

use crate::backend::{self, AuthResult, BackendEvent, LiveTimeline};

/// Everything about a live, logged-in session that the UI needs.
struct LiveSession {
    client: Arc<Client>,
    user_id: String,
    device_id: String,
    homeserver: String,
    /// `Some(true/false)` once we know whether our cross-signing identity is
    /// verified; `None` while the check is still in flight.
    verification: Option<bool>,
    /// Last crypto status message (bootstrap result etc.).
    crypto_status: Option<String>,
    /// Tells the sync loop to stop on logout.
    sync_shutdown: Arc<AtomicBool>,
    /// Handle of the sync loop task.
    sync_task: Option<tokio::task::JoinHandle<()>>,
}

/// The root egui app.
pub struct MatrixApp {
    // --- login form ---
    homeserver: String,
    username: String,
    password: String,
    login_error: Option<String>,
    auth_pending: Option<Promise<Result<AuthResult, String>>>,

    // --- restore-on-startup ---
    restore_attempted: bool,

    // --- live session ---
    session: Option<LiveSession>,
    logout_pending: Option<Promise<Result<(), String>>>,

    // --- rooms ---
    selected_room: Option<OwnedRoomId>,
    auto_selected: bool,
    join_input: String,
    join_pending: Option<Promise<Result<(), String>>>,

    // --- timeline ---
    timeline: Option<LiveTimeline>,
    timeline_pending: Option<Promise<Result<LiveTimeline, String>>>,
    send_pending: Option<Promise<Result<(), String>>>,
    composer: String,
    last_send_error: Option<String>,

    // --- backend events / status ---
    events_tx: mpsc::Sender<BackendEvent>,
    events_rx: mpsc::Receiver<BackendEvent>,
    status: String,
    /// One-line description of a previously saved session (shown on the login
    /// screen before restore kicks in). Cached to avoid disk I/O per frame.
    saved_session_info: Option<String>,
}

impl MatrixApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        // Comfortable spacing for a chat app.
        cc.egui_ctx.style_mut_of(egui::Theme::Dark, |style| {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
        });

        let (events_tx, events_rx) = mpsc::channel(64);

        let saved_session_info = crate::session::load_session().ok().flatten().map(|p| {
            format!(
                "Saved session found: {} on {}",
                p.session.meta.user_id, p.homeserver
            )
        });

        Self {
            homeserver: "https://matrix.org".to_owned(),
            username: String::new(),
            password: String::new(),
            login_error: None,
            auth_pending: None,
            restore_attempted: false,
            session: None,
            logout_pending: None,
            selected_room: None,
            auto_selected: false,
            join_input: String::new(),
            join_pending: None,
            timeline: None,
            timeline_pending: None,
            send_pending: None,
            composer: String::new(),
            last_send_error: None,
            events_tx,
            events_rx,
            status: "Welcome. Sign in to get started.".to_owned(),
            saved_session_info,
        }
    }

    // ---------------------------------------------------------------- auth --

    fn start_login(&mut self) {
        if self.auth_pending.is_some() {
            return;
        }
        let homeserver = self.homeserver.trim().to_owned();
        let username = self.username.trim().to_owned();
        let password = self.password.clone();
        self.login_error = None;
        self.status = "Signing in…".to_owned();
        self.auth_pending = Some(Promise::spawn_async(async move {
            backend::login(homeserver, username, password).await
        }));
    }

    fn start_restore(&mut self) {
        let persisted = match crate::session::load_session() {
            Ok(Some(p)) => p,
            Ok(None) => {
                self.restore_attempted = true;
                return;
            }
            Err(e) => {
                self.login_error = Some(format!(
                    "A saved session exists but could not be read: {e}. Please log in again."
                ));
                self.restore_attempted = true;
                return;
            }
        };
        self.restore_attempted = true;
        self.status = format!("Reconnecting as {}…", persisted.session.meta.user_id);
        self.auth_pending = Some(Promise::spawn_async(async move {
            backend::restore(persisted).await
        }));
    }

    fn finish_auth(&mut self, result: Result<AuthResult, String>) {
        match result {
            Ok(auth) => {
                // Persist a fresh login so the next launch reconnects silently.
                if let Some(persisted) = &auth.persisted {
                    match crate::session::save_session(persisted) {
                        Ok(_) => {
                            self.status =
                                format!("Connected as {} (session saved locally)", auth.user_id);
                        }
                        Err(e) => {
                            self.status = format!(
                                "Connected as {}, but the session could not be saved: {e}",
                                auth.user_id
                            );
                        }
                    }
                } else {
                    self.status = format!("Connected as {}", auth.user_id);
                }

                let shutdown = Arc::new(AtomicBool::new(false));
                let sync_task = backend::start_sync(
                    auth.client.clone(),
                    shutdown.clone(),
                    self.events_tx.clone(),
                );

                // First-run encryption setup (fresh login only — we hold the
                // password). Non-fatal: failures surface as a status message.
                if let Some(persisted) = &auth.persisted {
                    let client = auth.client.clone();
                    let password = persisted.passphrase.clone();
                    let tx = self.events_tx.clone();
                    let sync_shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        // Give the first sync a moment to populate identities.
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        if sync_shutdown.load(Ordering::Relaxed) {
                            return;
                        }
                        backend::bootstrap_crypto(client, password, tx).await;
                    });
                }

                // Report verification status of our own identity.
                {
                    let client = auth.client.clone();
                    let tx = self.events_tx.clone();
                    let sync_shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
                        if sync_shutdown.load(Ordering::Relaxed) {
                            return;
                        }
                        backend::check_verification(client, tx).await;
                    });
                }

                self.session = Some(LiveSession {
                    client: auth.client,
                    user_id: auth.user_id,
                    device_id: auth.device_id,
                    homeserver: auth.homeserver,
                    verification: None,
                    crypto_status: None,
                    sync_shutdown: shutdown,
                    sync_task: Some(sync_task),
                });
                self.login_error = None;
                self.selected_room = None;
                self.auto_selected = false;
                self.timeline = None;
                self.composer.clear();
                self.password.clear();
            }
            Err(e) => {
                self.login_error = Some(e.clone());
                self.status = e;
            }
        }
    }

    fn start_logout(&mut self) {
        if self.logout_pending.is_some() {
            return;
        }
        let Some(session) = &self.session else { return };
        let client = session.client.clone();
        self.logout_pending = Some(Promise::spawn_async(async move {
            backend::logout(&client).await
        }));
    }

    fn finish_logout(&mut self, result: Result<(), String>) {
        // Always tear the local session down, even if revoking the token
        // failed (the token may already be invalid).
        if let Some(session) = self.session.take() {
            session.sync_shutdown.store(true, Ordering::Relaxed);
            if let Some(task) = session.sync_task {
                task.abort();
            }
        }
        self.timeline = None;
        self.timeline_pending = None;
        self.selected_room = None;
        self.auto_selected = false;
        self.composer.clear();
        self.last_send_error = None;
        crate::session::delete_session();
        match result {
            Ok(()) => {
                self.status = "Logged out. Your session was removed from this device.".to_owned();
                self.login_error = None;
            }
            Err(e) => {
                self.status = "Logged out locally (the server could not be notified).".to_owned();
                self.login_error = Some(e);
            }
        }
    }

    // --------------------------------------------------------------- rooms --

    fn select_room(&mut self, room_id: OwnedRoomId) {
        if self.selected_room.as_ref() == Some(&room_id) && self.timeline.is_some() {
            return;
        }
        self.selected_room = Some(room_id.clone());
        self.timeline = None;
        self.last_send_error = None;
        let Some(session) = &self.session else { return };
        let client = session.client.clone();
        self.timeline_pending = Some(Promise::spawn_async(async move {
            backend::open_timeline(&client, room_id).await
        }));
    }

    fn finish_timeline_pending(&mut self) {
        let Some(promise) = self.timeline_pending.take() else {
            return;
        };
        match promise.try_take() {
            Ok(Ok(live_timeline)) => {
                self.status = format!("Viewing {}", live_timeline.room_id);
                self.timeline = Some(live_timeline);
            }
            Ok(Err(e)) => {
                self.status = e;
                self.timeline = None;
            }
            Err(promise) => self.timeline_pending = Some(promise),
        }
    }

    fn join_room(&mut self) {
        let input = self.join_input.trim().to_owned();
        if input.is_empty() || self.join_pending.is_some() {
            return;
        }
        let Some(session) = &self.session else { return };
        let client = session.client.clone();
        self.join_input.clear();
        self.status = format!("Joining {input}…");
        self.join_pending = Some(Promise::spawn_async(async move {
            backend::join_room(&client, &input).await
        }));
    }

    fn accept_invite(&mut self, room_id: OwnedRoomId) {
        if self.join_pending.is_some() {
            return;
        }
        let Some(session) = &self.session else { return };
        let client = session.client.clone();
        self.status = format!("Accepting invitation to {room_id}…");
        self.join_pending = Some(Promise::spawn_async(async move {
            backend::accept_invite(&client, &room_id).await
        }));
    }

    // ------------------------------------------------------------- messages --

    fn send_message(&mut self) {
        let text = self.composer.trim().to_owned();
        if text.is_empty() || self.send_pending.is_some() {
            return;
        }
        let Some(live) = &self.timeline else { return };
        let timeline = live.timeline.clone();
        self.composer.clear();
        self.last_send_error = None;
        self.send_pending = Some(Promise::spawn_async(async move {
            backend::send_text(&timeline, &text).await
        }));
    }

    fn finish_send_pending(&mut self) {
        let Some(promise) = self.send_pending.take() else {
            return;
        };
        match promise.try_take() {
            Ok(Ok(())) => {
                // The local echo in the timeline shows the final send state.
            }
            Ok(Err(e)) => {
                self.last_send_error = Some(e.clone());
                self.status = e;
            }
            Err(promise) => self.send_pending = Some(promise),
        }
    }

    // -------------------------------------------------------------- events --

    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                BackendEvent::SyncError(msg) => {
                    self.status = msg;
                }
                BackendEvent::Crypto(msg) => {
                    if let Some(session) = &mut self.session {
                        session.crypto_status = Some(msg.clone());
                    }
                    self.status = msg;
                }
                BackendEvent::Verification(verified) => {
                    if let Some(session) = &mut self.session {
                        session.verification = Some(verified);
                    }
                    self.status = if verified {
                        "Your session is verified for end-to-end encryption.".to_owned()
                    } else {
                        "You're connected. E2EE: verification needed — see session details."
                            .to_owned()
                    };
                }
            }
        }
    }

    fn poll_promises(&mut self) {
        // Auth (login or restore).
        if let Some(promise) = self.auth_pending.take() {
            match promise.try_take() {
                Ok(result) => self.finish_auth(result),
                Err(promise) => self.auth_pending = Some(promise),
            }
        }
        // Logout.
        if let Some(promise) = self.logout_pending.take() {
            match promise.try_take() {
                Ok(result) => self.finish_logout(result),
                Err(promise) => self.logout_pending = Some(promise),
            }
        }
        // Room open.
        self.finish_timeline_pending();
        // Join / invite accept.
        if let Some(promise) = self.join_pending.take() {
            match promise.try_take() {
                Ok(Ok(())) => {
                    self.status = "Joined.".to_owned();
                }
                Ok(Err(e)) => {
                    self.status = e;
                }
                Err(promise) => self.join_pending = Some(promise),
            }
        }
        // Send.
        self.finish_send_pending();
    }

    // --------------------------------------------------------------- panels --

    fn login_panel(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(36.0);
            ui.heading(egui::RichText::new("🛋 Matrix Desk").size(30.0));
            ui.label("A comfortable seat in the Matrix");
            ui.add_space(20.0);
        });

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(28, 16))
            .show(ui, |ui| {
                ui.set_min_width(420.0);
                ui.label(egui::RichText::new("Sign in").strong().size(16.0));
                ui.add_space(8.0);

                egui::Grid::new("login_form")
                    .num_columns(2)
                    .spacing([12.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Homeserver");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.homeserver)
                                .hint_text("https://matrix.org")
                                .desired_width(320.0),
                        );
                        ui.end_row();

                        ui.label("Username");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.username)
                                .hint_text("@alice:matrix.org")
                                .desired_width(320.0),
                        );
                        ui.end_row();

                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.password)
                                .password(true)
                                .desired_width(320.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(10.0);
                let busy = self.auth_pending.is_some();
                ui.horizontal(|ui| {
                    let label = if busy { "Signing in…" } else { "Log in" };
                    if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                        self.start_login();
                    }
                    if busy {
                        ui.spinner();
                    }
                });

                if let Some(err) = &self.login_error {
                    ui.add_space(6.0);
                    ui.colored_label(egui::Color32::from_rgb(235, 110, 110), err);
                }
            });

        ui.add_space(12.0);

        // Session area: shows who we're connecting as (restored session) or
        // who we connected as.
        let session_line = if let Some(session) = &self.session {
            Some(format!(
                "Connected as {} (device {}) on {}",
                session.user_id, session.device_id, session.homeserver
            ))
        } else {
            self.saved_session_info.clone()
        };
        if let Some(line) = session_line {
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::symmetric(28, 10))
                .show(ui, |ui| {
                    ui.set_min_width(420.0);
                    ui.label(egui::RichText::new("Session").strong().small());
                    ui.label(egui::RichText::new(line).weak());
                });
        }
    }

    fn rooms_panel(&mut self, ui: &mut egui::Ui) {
        let mut room_to_select: Option<OwnedRoomId> = None;
        let mut invite_to_accept: Option<OwnedRoomId> = None;
        let mut join_clicked = false;

        ui.add_space(4.0);
        ui.heading("Rooms");
        ui.separator();

        let Some(session) = &self.session else { return };

        let joined = session.client.joined_rooms();
        let selected = self.selected_room.clone();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("rooms_scroll")
            .show(ui, |ui| {
                if joined.is_empty() {
                    ui.label(egui::RichText::new("No rooms yet — sync in progress…").weak());
                }
                for room in &joined {
                    let name = backend::room_display_name(room);
                    let encrypted = backend::room_is_encrypted(room);
                    let unread = room.num_unread_messages();
                    let prefix = if encrypted { "🔒 " } else { "" };
                    let unread_suffix = if unread > 0 {
                        format!("  ●{unread}")
                    } else {
                        String::new()
                    };
                    let label = format!("{prefix}{name}{unread_suffix}");
                    let is_selected = selected.as_deref() == Some(room.room_id());
                    if ui
                        .selectable_label(is_selected, egui::RichText::new(label).monospace())
                        .clicked()
                    {
                        room_to_select = Some(room.room_id().to_owned());
                    }
                }
            });

        let invited = session.client.invited_rooms();
        if !invited.is_empty() {
            ui.separator();
            ui.label(egui::RichText::new("Invitations").strong());
            for room in &invited {
                ui.horizontal(|ui| {
                    let name = backend::room_display_name(room);
                    ui.label(egui::RichText::new(name).monospace());
                    if ui.button("Accept").clicked() {
                        invite_to_accept = Some(room.room_id().to_owned());
                    }
                });
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.join_input)
                    .hint_text("#room:server")
                    .desired_width(150.0),
            );
            if ui.button("Join").clicked() {
                join_clicked = true;
            }
        });

        if let Some(id) = room_to_select {
            self.select_room(id);
        }
        if let Some(id) = invite_to_accept {
            self.accept_invite(id);
        }
        if join_clicked {
            self.join_room();
        }
    }

    fn chat_panel(&mut self, ui: &mut egui::Ui) {
        let Some(live) = self.timeline.as_mut() else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("Select a room from the list to start chatting.")
                        .weak()
                        .size(16.0),
                );
            });
            return;
        };

        // Apply timeline diffs produced by the background subscription task.
        while let Ok(diffs) = live.diffs_rx.try_recv() {
            for diff in diffs {
                diff.apply(&mut live.items);
            }
        }

        // Room header.
        let room_name = self
            .session
            .as_ref()
            .and_then(|s| s.client.get_room(&live.room_id))
            .map(|r| backend::room_display_name(&r))
            .unwrap_or_else(|| live.room_id.to_string());
        ui.horizontal(|ui| {
            ui.heading(room_name);
            let encrypted = matches!(
                live.timeline.room().encryption_state(),
                EncryptionState::Encrypted
            );
            if encrypted {
                ui.label(egui::RichText::new("🔒 encrypted").weak());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(status) = &self.last_send_error {
                    ui.colored_label(egui::Color32::from_rgb(235, 110, 110), status);
                }
            });
        });
        ui.separator();

        // Messages.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .id_salt("timeline_scroll")
            .show(ui, |ui| {
                for item in live.items.iter() {
                    render_timeline_item(ui, item);
                }
            });

        ui.separator();

        // Composer.
        let mut send_requested = false;
        let mut enter_pressed = false;
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.composer)
                    .hint_text("Message…")
                    .desired_width(f32::INFINITY),
            );
            if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                enter_pressed = true;
            }
            let can_send = !self.composer.trim().is_empty() && self.send_pending.is_none();
            if ui
                .add_enabled(can_send, egui::Button::new("Send"))
                .clicked()
            {
                send_requested = true;
            }
        });
        if enter_pressed || send_requested {
            self.send_message();
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("🛋 Matrix Desk").weak().small());
            ui.separator();
            ui.label(egui::RichText::new(&self.status).weak().small());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(session) = &self.session {
                    let e2ee = match session.verification {
                        Some(true) => "E2EE verified ✓",
                        Some(false) => "E2EE: verification needed ⚠",
                        None => "E2EE: checking…",
                    };
                    ui.label(egui::RichText::new(e2ee).small().weak());
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "{} · {}",
                            session.user_id, session.homeserver
                        ))
                        .small()
                        .weak(),
                    );
                }
            });
        });
    }
}

impl eframe::App for MatrixApp {
    /// Non-painting work, called before each [`Self::ui`]: process background
    /// events, resolve promises, kick off restore and auto-select the first
    /// room.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Kick off the restore-on-startup flow once, when not yet logged in.
        if !self.restore_attempted && self.session.is_none() && self.auth_pending.is_none() {
            self.start_restore();
        }

        self.drain_events();
        self.poll_promises();

        // Auto-select the first room once the initial sync populates the list.
        let first_room = if !self.auto_selected
            && self.session.is_some()
            && self.selected_room.is_none()
            && self.timeline_pending.is_none()
        {
            self.session.as_ref().and_then(|s| {
                s.client
                    .joined_rooms()
                    .first()
                    .map(|room| room.room_id().to_owned())
            })
        } else {
            None
        };
        if let Some(room_id) = first_room {
            self.auto_selected = true;
            self.select_room(room_id);
        }

        // Keep the loop ticking while async work is pending or the timeline
        // may have new diffs; otherwise refresh periodically so the room list
        // and unread counts stay fresh.
        let busy = self.auth_pending.is_some()
            || self.logout_pending.is_some()
            || self.join_pending.is_some()
            || self.timeline_pending.is_some()
            || self.send_pending.is_some();
        if busy {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("header").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("🛋 Matrix Desk");
                ui.label(egui::RichText::new("a desktop chat seat").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.session.is_some() && ui.button("Log out").clicked() {
                        self.start_logout();
                    }
                });
            });
        });

        if self.session.is_some() {
            egui::Panel::left("rooms")
                .resizable(true)
                .default_size(250.0)
                .min_size(170.0)
                .show(ui, |ui| self.rooms_panel(ui));
            egui::CentralPanel::default_margins().show(ui, |ui| self.chat_panel(ui));
        } else {
            egui::CentralPanel::default_margins().show(ui, |ui| self.login_panel(ui));
        }

        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
    }
}

// ------------------------------------------------------------- rendering --

/// Render a single timeline item (message, date divider, …).
fn render_timeline_item(ui: &mut egui::Ui, item: &Arc<TimelineItem>) {
    match item.kind() {
        TimelineItemKind::Event(event) => render_event_item(ui, event),
        TimelineItemKind::Virtual(virtual_item) => match virtual_item {
            VirtualTimelineItem::DateDivider(timestamp) => {
                ui.add_space(8.0);
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new(format_date(*timestamp)).weak().small());
                });
            }
            VirtualTimelineItem::TimelineStart | VirtualTimelineItem::ReadMarker => {}
        },
    }
}

/// Render one event: timestamp, sender, body and E2EE shield.
fn render_event_item(ui: &mut egui::Ui, event: &EventTimelineItem) {
    let sender = sender_display_name(event);
    let body = message_body(event);
    let time = format_time(event.timestamp());
    let shield = shield_icon(event);
    let is_own = event.is_own();

    let sender_color = color_for_sender(&sender);
    let text_color = if is_own {
        ui.visuals().weak_text_color()
    } else {
        ui.visuals().text_color()
    };

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(time).small().weak());
        ui.label(egui::RichText::new(sender).strong().color(sender_color));
        ui.label(egui::RichText::new(body).color(text_color));
        if let Some(icon) = shield {
            ui.label(egui::RichText::new(icon).small());
        }
    });

    if let Some(state) = event.send_state() {
        let label = match state {
            EventSendState::NotSentYet { .. } => "sending…",
            EventSendState::Sent { .. } => "✓ sent",
            EventSendState::SendingFailed { .. } => "✗ send failed",
        };
        ui.label(egui::RichText::new(label).weak().small());
    }
}

/// Best-effort display name of the sender (falls back to the user id).
fn sender_display_name(event: &EventTimelineItem) -> String {
    match event.sender_profile() {
        TimelineDetails::Ready(profile) => profile
            .display_name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| event.sender().to_string()),
        _ => event.sender().to_string(),
    }
}

/// Extract the text body of a message, handling non-text and undecryptable
/// events gracefully.
fn message_body(event: &EventTimelineItem) -> String {
    let content = event.content();
    if content.is_redacted() {
        return "(message removed)".to_owned();
    }
    match content.as_message() {
        Some(message) => message.body().to_owned(),
        None if content.as_unable_to_decrypt().is_some() => {
            "🔒 Unable to decrypt this message".to_owned()
        }
        None => "(non-text message)".to_owned(),
    }
}

/// E2EE shield icon for a message: warning when the sender/device isn't
/// verified, magnifier when authenticity can't be guaranteed yet, none when
/// everything is fine.
fn shield_icon(event: &EventTimelineItem) -> Option<&'static str> {
    match event.get_shield(false) {
        TimelineEventShieldState::Red { .. } => Some("⚠️"),
        TimelineEventShieldState::Grey { .. } => Some("🔍"),
        TimelineEventShieldState::None => None,
    }
}

/// Deterministic, pleasant color per sender.
fn color_for_sender(sender: &str) -> egui::Color32 {
    let hash = sender
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    SENDER_COLORS[(hash as usize) % SENDER_COLORS.len()]
}

/// The color palette used for senders, exposed for tests.
const SENDER_COLORS: [egui::Color32; 8] = [
    egui::Color32::from_rgb(255, 179, 128),
    egui::Color32::from_rgb(171, 224, 132),
    egui::Color32::from_rgb(150, 205, 255),
    egui::Color32::from_rgb(240, 170, 240),
    egui::Color32::from_rgb(255, 220, 130),
    egui::Color32::from_rgb(130, 225, 210),
    egui::Color32::from_rgb(230, 150, 150),
    egui::Color32::from_rgb(200, 200, 160),
];

/// Convert a ruma `MilliSecondsSinceUnixEpoch` to plain `u64` milliseconds.
///
/// js_int's `UInt` has no direct u64 accessor, but converts losslessly via
/// `i64` for any realistic timestamp.
fn millis_to_u64(timestamp: MilliSecondsSinceUnixEpoch) -> u64 {
    i64::from(timestamp.0).max(0) as u64
}

/// Format a ruma timestamp as `HH:MM`.
fn format_time(timestamp: MilliSecondsSinceUnixEpoch) -> String {
    let duration = std::time::Duration::from_millis(millis_to_u64(timestamp));
    let system_time = std::time::SystemTime::UNIX_EPOCH + duration;
    let local: chrono::DateTime<chrono::Local> = system_time.into();
    local.format("%H:%M").to_string()
}

/// Format a date divider timestamp as `YYYY-MM-DD`.
fn format_date(timestamp: MilliSecondsSinceUnixEpoch) -> String {
    let duration = std::time::Duration::from_millis(millis_to_u64(timestamp));
    let system_time = std::time::SystemTime::UNIX_EPOCH + duration;
    let local: chrono::DateTime<chrono::Local> = system_time.into();
    local.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_color_is_stable_and_in_palette() {
        let a = color_for_sender("@alice:matrix.org");
        let b = color_for_sender("@alice:matrix.org");
        assert_eq!(a, b);
        assert!(SENDER_COLORS.contains(&a));
    }

    #[test]
    fn timestamps_format_as_hhmm() {
        // 2024-01-02 03:04:05 UTC
        let ts = MilliSecondsSinceUnixEpoch::from_system_time(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1704164645),
        )
        .unwrap();
        let out = format_time(ts);
        // Contains a colon and is short; exact value depends on the local TZ.
        assert!(out.len() == 5, "unexpected length for {out}");
        assert!(out.contains(':'));
    }
}
