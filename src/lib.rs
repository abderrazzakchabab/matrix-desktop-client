//! matrix-desktop-client — a native desktop Matrix messaging client.
//!
//! Library crate exposing the app modules so they can be tested from
//! integration tests. The UI entry point lives in `src/main.rs`.

// Matrix-sdk types (Megolm/crypto) and the app's async glue exceed the default
// recursion limit while trait-checking; same value as `main.rs`.
#![recursion_limit = "256"]

pub mod app;
pub mod backend;
pub mod session;
