//! matrix-desktop-client — a native desktop Matrix messaging client.
//!
//! A comfortable chat seat (Buzz-style) built on:
//! - [`matrix-rust-sdk`](https://github.com/matrix-org/matrix-rust-sdk) for all
//!   Matrix protocol work, including end-to-end encryption,
//! - [`egui`](https://github.com/emilk/egui) via `eframe` for the UI.
//!
//! The UI is a plain synchronous loop; the SDK is async. We bridge them by
//! running the whole app inside a Tokio multi-thread runtime (`#[tokio::main]`)
//! and driving every async operation through `poll_promise` promises plus a
//! channel for background events (sync errors, crypto status).

#![recursion_limit = "256"]

use eframe::egui;

use matrix_desktop_client::app::MatrixApp;

/// The main entry point.
///
/// `#[tokio::main]` keeps this thread inside the Tokio runtime for the whole
/// lifetime of the eframe event loop, so `Promise::spawn_async` and
/// `tokio::spawn` work directly from the UI code while background tasks run on
/// the runtime's worker threads.
#[tokio::main]
async fn main() -> eframe::Result {
    init_logging();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 760.0])
            .with_min_inner_size([820.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Matrix Desk",
        options,
        Box::new(|cc| Ok(Box::new(MatrixApp::new(cc)))),
    )
}

/// Set up `tracing` logging. Defaults to `warn`; set `RUST_LOG` to override,
/// e.g. `RUST_LOG=info,tower_http=debug`.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
