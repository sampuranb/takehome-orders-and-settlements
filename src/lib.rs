//! Shared crate root.
//!
//! Compiled twice by cargo-leptos: natively with the `ssr` feature (linked into
//! the server binary) and to `wasm32-unknown-unknown` with the `hydrate`
//! feature (shipped to the browser). Anything that must not reach the browser
//! belongs behind `#[cfg(feature = "ssr")]`.
//!
//! The shell and `App` below are a temporary scaffold that proves the SSR ->
//! hydration pipeline end to end. Feature 2 replaces them with the real
//! `src/app.rs` shell, route table, error boundary, and shared components.

pub mod error;

use leptos::prelude::*;
use leptos_meta::{provide_meta_context, MetaTags, Title};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::StaticSegment;

/// Renders the full HTML document for every server-rendered response.
///
/// `HydrationScripts` emits the `<script>` tags that load `/pkg/orders.js`,
/// which in turn calls [`hydrate`]. No `<Stylesheet>` tag is present because
/// this feature declares no `style-file` in `[package.metadata.leptos]`; adding
/// one without the other would make every page request a missing CSS file.
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <AutoReload options=options.clone() />
                <HydrationScripts options />
                <MetaTags />
            </head>
            <body>
                <App />
            </body>
        </html>
    }
}

/// Temporary application root. Replaced by `app::App` in Feature 2.
#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    view! {
        <Title text="Orders and Settlements" />
        <Router>
            <main>
                <Routes fallback=|| view! { <p>"Page not found."</p> }>
                    <Route path=StaticSegment("") view=ScaffoldPage />
                </Routes>
            </main>
        </Router>
    }
}

/// Placeholder page. The counter is not decoration: if the button increments,
/// the WASM bundle loaded and hydration attached to the server-rendered DOM.
#[component]
fn ScaffoldPage() -> impl IntoView {
    let clicks = RwSignal::new(0);

    view! {
        <h1>"Orders and Settlements"</h1>
        <p>"Feature 1 scaffold. Orders, payments, and the dashboard follow."</p>
        <button on:click=move |_| *clicks.write() += 1>"Hydration check: " {clicks}</button>
    }
}

/// Browser entry point. cargo-leptos wires the generated `orders.js` to call
/// this once the WASM module instantiates.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
