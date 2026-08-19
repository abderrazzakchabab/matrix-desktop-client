# Matrix Desk — a native desktop Matrix client

A comfortable desktop companion for Matrix — a chat "seat" rather than a bare
terminal client. Built on:

- **[matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk)** (`matrix-sdk`
  0.18 + `matrix-sdk-ui`) for all protocol work, including end-to-end encryption
  (Olm/Megolm via the SDK's bundled crypto store),
- **[egui](https://github.com/emilk/egui)** via `eframe` for the UI
  (OpenGL/glow renderer, X11 + Wayland on Linux, native macOS windowing).

This is milestone 1: a working horizontal MVP (login, session persistence, sync,
room list, timeline, composer, E2EE). It is a standalone client — no websocket /
relay wiring to any control plane.

## Features (current scope)

- **Login** with homeserver URL + username + password (works against
  matrix.org or a local Synapse).
- **Session persistence** — the session (access/refresh tokens) is stored in the
  user's config directory (`~/.config/matrix-desktop-client/` on Linux,
  `~/Library/Application Support/matrix-desktop-client/` on macOS) so the app
  reconnects on restart without re-entering credentials. Log out clears it.
- **Live sync** — the SDK sync loop keeps room state current (lazy member
  loading filter).
- **Room list** — joined rooms with names, encryption markers and unread
  counts; invitations with an Accept button; join rooms by alias/id.
- **Timeline** — text messages with sender display name, timestamp, E2EE shield
  (🔒 encrypted / ⚠️ verification needed / 🔍 unknown device), date dividers,
  and send-state indicators for your own messages (✓ / ✗ / sending…).
- **Composer** — send text messages (Enter or the Send button); failures are
  shown inline.
- **E2EE** — SDK crypto store with SQLite persistence, first-run cross-signing
  bootstrap (password-based UIA) and a visible verification indicator. If
  encryption setup can't complete, the app degrades gracefully instead of
  failing cryptically.

Out of scope for this milestone: media uploads, reactions, threads, spaces,
verification flows, notifications, profile/avatar rendering.

## Building

Requirements: a Rust toolchain (rustc ≥ 1.92, e.g. via
[rustup](https://rustup.rs/)). SQLite is **bundled**, so no system SQLite dev
package is needed. Linux needs the usual GUI runtime libs (X11/Wayland
libraries, OpenGL); macOS needs nothing extra.

```sh
git clone https://github.com/abderrazzakchabab/matrix-desktop-client
cd matrix-desktop-client

# Development build + run
cargo run

# Release build
cargo build --release
# binary: target/release/matrix-desktop-client
```

Logs go to stderr. The default level is `warn`; set `RUST_LOG` for more, e.g.
`RUST_LOG=info cargo run`.

### Linux native dependencies (build/runtime)

The build itself needs no apt packages (SQLite is bundled, winit's X11/Wayland
backends are pure Rust). To actually display the app you need a running X11 or
Wayland session with OpenGL support. On minimal headless systems install e.g.:

```sh
sudo apt install libxkbcommon-dev libxkbcommon-x11-0 mesa-utils
```

(macOS: nothing extra — Metal/OpenGL and AppKit are part of the OS.)

## Logging in

1. Start the app. The homeserver field defaults to `https://matrix.org`.
2. Enter your username (full `@user:server` id or localpart) and password.
3. Press **Log in**. On success the room list fills in after the first sync
   and the first joined room opens automatically.
4. To chat with multiple users, use your homeserver's directory or invite
   yourself to a room; accept invitations from the **Invitations** section or
   use the **Join** box with `#alias:server`.

On the next launch the app reconnects using the saved session — no login
prompt. Use **Log out** to revoke the session and clear local state.

### Session storage & security

- Session file: `~/.config/matrix-desktop-client/session.json` (Linux) or
  `~/Library/Application Support/matrix-desktop-client/session.json` (macOS),
  written with `0600` permissions.
- Matrix state + encryption keys: SQLite database under
  `~/.config/matrix-desktop-client/db/` (Linux) or
  `~/Library/Application Support/matrix-desktop-client/db/` (macOS),
  encrypted with a passphrase stored alongside the session.
- All of this lives outside the repository; nothing sensitive is ever
  committed (see `.gitignore`).

## Development

```sh
cargo fmt --check      # formatting gate (CI)
cargo clippy --all-targets -- -D warnings   # lint gate (CI)
cargo test             # unit tests (no homeserver needed)
```

### End-to-end smoke test

`tests/smoke_matrix.rs` validates the whole MVP loop against a real
homeserver (register → login → sync → encrypted room → send → decrypt →
restore session → send again). It is `#[ignore]`d by default. Point it at a
dev homeserver that allows open registration (e.g. a local Synapse with
`enable_registration: true`) and run:

```sh
MDC_HOMESERVER=http://localhost:8008 \
  cargo test --test smoke_matrix -- --ignored --nocapture
```

Architecture (all in `src/`):

- `main.rs` — entry point; runs the app inside a Tokio runtime so the egui
  loop can drive async SDK calls.
- `app.rs` — the egui UI (login screen, room list, timeline, composer, status).
- `backend.rs` — all Matrix operations (login/restore/logout, sync loop,
  timeline subscription, sending, crypto bootstrap) and the `BackendEvent`
  channel that background tasks use to talk to the UI.
- `session.rs` — session persistence layout and helpers.

The UI is synchronous; every async operation is bridged with
`poll_promise::Promise` and drained in the frame loop, and timeline updates
arrive as `eyeball_im::VectorDiff`s applied in the UI thread.

## CI

`.github/workflows/ci.yml` runs `cargo fmt --check`, `clippy -D warnings`,
`cargo test` and `cargo build --release` on both `ubuntu-latest` and
`macos-latest`, gated on PRs and pushes to `main`.

## License

MIT OR Apache-2.0.
