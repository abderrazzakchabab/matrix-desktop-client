# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## Build & test

- `cargo build` / `cargo test` / `cargo clippy --all-targets -- -D warnings`. SQLite is bundled, so no system deps are needed to build; a display (X11/Wayland) is needed to run the GUI.
- egui/eframe is pinned to 0.35 (rustc 1.92+); 0.36 requires rustc 1.95 — do not bump without checking the toolchain.
- `eyeball-im` must stay on 0.8 to match matrix-sdk-ui 0.18 (imbl 6); 0.9 pulls imbl 7 and breaks `VectorDiff` types.
- egui 0.35 changed the App trait: implement `fn ui(&mut self, ui, frame)` + optional `fn logic(...)` instead of `update`; panels are `egui::Panel::top/left/...` and `CentralPanel::default_margins()`, not `SidePanel`/`TopBottomPanel`.
- matrix-sdk 0.18: `Client::builder().sqlite_store(path, passphrase)`, `client.matrix_auth().login_username(...)`, `client.restore_session(MatrixSession)`, `client.logout()`. Timeline: `matrix_sdk_ui::timeline::RoomExt::timeline()` + `timeline.subscribe()`; apply `VectorDiff`s to your own copy of the items.

## Architecture

- `src/backend.rs` owns all Matrix calls (login/restore/logout, sync loop, timeline subscription, sending, crypto bootstrap); the UI (`src/app.rs`) only drives async work through `poll_promise` promises and drains a `BackendEvent` channel each frame. Session/token persistence lives in `src/session.rs` (user config dir, never git).
- The app runs inside `#[tokio::main]`, so `Promise::spawn_async`/`tokio::spawn` work from the egui loop. Timeline items live in the UI thread; a background task forwards `VectorDiff`s over a channel.

## Smoke test

- `tests/smoke_matrix.rs` (ignored by default) exercises the real MVP loop incl. E2EE + session restore against a homeserver: `MDC_HOMESERVER=http://localhost:8008 cargo test --test smoke_matrix -- --ignored --nocapture`. Needs a homeserver that allows open registration (local Synapse in Docker: enable_registration + generous `rc_login`/`rc_registration` rate limits, else Synapse 429s logins with long Retry-After and the test crawls).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
