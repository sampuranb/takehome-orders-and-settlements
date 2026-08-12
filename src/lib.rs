//! Shared crate root.
//!
//! Compiled twice by cargo-leptos: natively with the `ssr` feature (linked into
//! the server binary) and to `wasm32-unknown-unknown` with the `hydrate`
//! feature (shipped to the browser). Anything that must not reach the browser
//! belongs behind `#[cfg(feature = "ssr")]`.

pub mod app;
pub mod auth;
pub mod error;

// Re-exported at the crate root so `main.rs` keeps importing the document shell
// and the application root from one place, unchanged, as the app module grows.
pub use app::{shell, App};

/// Browser entry point. cargo-leptos wires the generated `orders.js` to call
/// this once the WASM module instantiates.
///
/// `hydrate_body` attaches to the DOM the server already produced rather than
/// replacing it, so the markup rendered here must match `App` exactly.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
