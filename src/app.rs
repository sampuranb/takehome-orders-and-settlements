//! Application shell, route table, and shared presentational components.
//!
//! Compiled for both targets. Nothing here touches the database or the auth
//! service; the only server-only code is the three-line status override in
//! [`NotFound`], which is behind `#[cfg(feature = "ssr")]`.
//!
//! Two similarly named items live side by side, deliberately:
//!
//! - [`shell`] (lowercase) renders the **HTML document** — `<html>`, `<head>`,
//!   the hydration scripts. Axum calls it once per server-rendered response.
//! - [`Shell`] (capitalised) is the **layout component** — the persistent
//!   header, navigation, error boundary, and routed `<Outlet/>`. The router
//!   renders it as the parent of every page.

use leptos::prelude::*;
use leptos_meta::{provide_meta_context, MetaTags, Stylesheet, Title};
use leptos_router::components::{Outlet, ParentRoute, Route, Router, Routes, A};
use leptos_router::path;

use crate::auth::{provide_auth, AccountNav, AuthPage, Protected};
use crate::orders::{EditOrderPage, OrderDetailPage, OrderEditor, OrdersPage};

/// Renders the full HTML document for every server-rendered response.
///
/// `HydrationScripts` emits the `<script>` tags that load `/pkg/orders.js`,
/// which in turn calls `hydrate`. The stylesheet link is *not* here: it is
/// declared by [`App`] through `leptos_meta` and hoisted into this `<head>` by
/// `MetaTags`, which keeps the document and the app's metadata in one place.
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                // Opts the document into Pico's dark-mode block and stops the
                // browser flashing a light form control before CSS applies.
                <meta name="color-scheme" content="light dark" />
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

/// Application root: metadata, stylesheet, and the route table.
///
/// `generate_route_list(App)` in `main.rs` walks this same tree at startup to
/// discover the paths Axum must hand to Leptos, so the route table has exactly
/// one definition for both server and browser.
#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    // One auth context for the whole tree. Created here rather than inside
    // `Shell` so the header, the protected pages, and the auth page all read
    // the same identity and the same three actions, and so signing in or out
    // invalidates every one of them at once.
    provide_auth();

    view! {
        // cargo-leptos always publishes the processed `style-file` at
        // /pkg/<output-name>.css. `hash-files` is off, so the href is stable
        // and plain `Stylesheet` is correct; `HashedStylesheet` would be needed
        // only if hashed filenames were enabled.
        <Stylesheet id="leptos" href="/pkg/orders.css" />
        <Title text="Orders and Settlements" />

        <Router>
            <Routes fallback=NotFound>
                // Sits outside the layout parent on purpose: everything under
                // that parent is gated by `Protected`, and the page you are
                // sent to when you are not signed in must not be. It renders
                // its own `Chrome`, exactly as `NotFound` does.
                <Route path=path!("auth") view=AuthGate />

                // The parent contributes no path segment: it exists purely to
                // wrap every page in the persistent layout. It produces no
                // route of its own — only the flattened leaves below are
                // registered with Axum.
                <ParentRoute path=path!("") view=Shell>
                    <Route path=path!("") view=DashboardPage />
                    <Route path=path!("orders") view=OrdersPage />
                    // Before `orders/:id`, or "new" would be matched as an id.
                    <Route path=path!("orders/new") view=NewOrderPage />
                    <Route path=path!("orders/:id") view=OrderDetailPage />
                    <Route path=path!("orders/:id/edit") view=EditOrderPage />
                </ParentRoute>
            </Routes>
        </Router>
    }
}

/// The auth page inside the shared chrome.
///
/// A thin wrapper rather than putting `Chrome` inside `AuthPage` itself: the
/// chrome is this module's concern, and `src/auth.rs` should not have to know
/// what the surrounding page furniture is.
#[component]
fn AuthGate() -> impl IntoView {
    view! {
        <Chrome>
            <AuthPage />
        </Chrome>
    }
}

/// Persistent layout: skip link, header navigation, error boundary, outlet.
///
/// Rendered once as the parent of every page, so client-side navigation swaps
/// only the `<Outlet/>` contents and leaves the header and footer DOM untouched.
///
/// `Protected` wraps the outlet rather than each page, so a route added in a
/// later feature is private by default. Forgetting to gate a new page is the
/// easy mistake, and this makes the easy path the safe one — though the real
/// boundary is still `require_user` inside each server function, since anything
/// the browser decides can be skipped by not using a browser.
#[component]
fn Shell() -> impl IntoView {
    view! {
        <Chrome>
            // Catches any `Err` returned by a page below. It runs during SSR
            // too: children render into a scratch buffer, and if anything threw
            // the fallback is sent instead, then resumed on hydration.
            <ErrorBoundary fallback=|errors| {
                view! {
                    <article class="error-panel" role="alert">
                        <header>"Something went wrong"</header>
                        <ul>
                            {move || {
                                errors
                                    .get()
                                    .into_iter()
                                    .map(|(_, error)| view! { <li>{error.to_string()}</li> })
                                    .collect::<Vec<_>>()
                            }}
                        </ul>
                    </article>
                }
            }>
                <Protected>
                    <Outlet />
                </Protected>
            </ErrorBoundary>
        </Chrome>
    }
}

/// Skip link, header navigation, main landmark, and footer.
///
/// Extracted from [`Shell`] because the router renders its `fallback` *outside*
/// the matched route tree: without this, a 404 would lose the navigation and
/// the page container entirely. [`Shell`] and [`NotFound`] are its two callers.
///
/// Everything here is plain server-rendered HTML with no reactive state, so it
/// is fully usable before the WASM bundle finishes downloading — the links are
/// real `<a href>` elements that fall back to full page loads.
#[component]
fn Chrome(children: Children) -> impl IntoView {
    view! {
        <a class="skip-link" href="#main">
            "Skip to main content"
        </a>

        <header class="container">
            <nav aria-label="Primary">
                <ul>
                    <li>
                        <strong>"Orders and Settlements"</strong>
                    </li>
                </ul>
                <ul>
                    // `exact` matters only on "/": without it the dashboard
                    // link would report itself as current on every page.
                    <li>
                        <A href="/" exact=true>
                            "Dashboard"
                        </A>
                    </li>
                    <li>
                        <A href="/orders">"Orders"</A>
                    </li>
                    <li>
                        <A href="/orders/new">"New order"</A>
                    </li>
                    // Resolves asynchronously and renders nothing until it
                    // does, so the rest of the navigation never waits on the
                    // auth service to become interactive.
                    <AccountNav />
                </ul>
            </nav>
        </header>

        <main id="main" class="container">{children()}</main>

        <footer class="container">
            <small>"Internal tool. Amounts are stored and calculated in cents."</small>
        </footer>
    }
}

/// Router-level fallback for a path Axum forwarded but no route matched.
///
/// Without the status override this would render inside a `200 OK`, because
/// Leptos rendered a page successfully — it simply was not the page asked for.
#[component]
fn NotFound() -> impl IntoView {
    #[cfg(feature = "ssr")]
    {
        // `use_context` rather than `expect_context`: some render paths do not
        // provide `ResponseOptions`, and a missing status is far better than a
        // panic inside the 404 handler.
        if let Some(response) = use_context::<leptos_axum::ResponseOptions>() {
            response.set_status(axum::http::StatusCode::NOT_FOUND);
        }
    }

    view! {
        <Title text="Not found - Orders and Settlements" />
        <Chrome>
            <h1>"404 - page not found"</h1>
            <p>"That page does not exist."</p>
            <p>
                <A href="/">"Back to the dashboard"</A>
            </p>
        </Chrome>
    }
}

// ---------------------------------------------------------------------------
// Placeholder pages
//
// Each is replaced by the feature named in its body. They exist now so the
// route table, navigation, and layout can be exercised and reviewed before any
// data model exists.
// ---------------------------------------------------------------------------

#[component]
fn DashboardPage() -> impl IntoView {
    view! {
        <Title text="Dashboard - Orders and Settlements" />
        <h1>"Dashboard"</h1>
        <p>
            "The order summary table, status filters, and outstanding totals arrive in Feature 8."
        </p>

        // Not decoration and not sample data: the shared components below have
        // no other caller until Feature 5, and the review needs to see them
        // render and take keyboard focus. Feature 8 replaces this section with
        // the real dashboard.
        <article>
            <header>
                <h2>"Shared components"</h2>
            </header>
            <p>"Status tones, as derived by Feature 5:"</p>
            <p>
                <StatusBadge status="pending" />
                " "
                <StatusBadge status="partially_paid" />
                " "
                <StatusBadge status="paid" />
                " "
                <StatusBadge status="overdue" />
            </p>
            <p>
                "Money rendering: " <MoneyText cents=0 /> ", " <MoneyText cents=123456 /> ", "
                <MoneyText cents=-2550 /> "."
            </p>
            <label>
                "Focus target"
                <input type="text" placeholder="Tab to me to check the focus ring" />
            </label>
            <FieldError message=Some("Field errors render like this.".to_string()) />
        </article>
    }
}

#[component]
fn NewOrderPage() -> impl IntoView {
    view! {
        <Title text="New order - Orders and Settlements" />
        <h1>"New order"</h1>
        <p>"Totals are calculated on the server from the values you enter here."</p>
        <OrderEditor />
    }
}

// ---------------------------------------------------------------------------
// Shared presentational components
//
// Presentation only. No component here derives a status, converts a currency,
// or computes an amount due: those calculations stay server-side in the
// services, and these components render what the server already decided.
// ---------------------------------------------------------------------------

/// Renders a derived order status as a coloured badge.
///
/// The tone is carried in `data-tone` rather than a class so the CSS mapping
/// stays in one selector block and an unknown status degrades to neutral
/// instead of rendering unstyled.
#[component]
pub fn StatusBadge(#[prop(into)] status: String) -> impl IntoView {
    let tone = status_tone(&status);
    let label = humanize_status(&status);

    view! {
        <span class="badge" data-tone=tone>
            {label}
        </span>
    }
}

/// Renders an `i64` cent amount as a currency string.
///
/// The raw cent value is kept in `data-cents` so tests and any later export can
/// read the exact integer rather than parsing the formatted text back.
#[component]
pub fn MoneyText(cents: i64) -> impl IntoView {
    view! {
        <span class="money" class:money-negative=cents < 0 data-cents=cents>
            {format_cents(cents)}
        </span>
    }
}

/// Renders a validation message for a single form field, or nothing.
///
/// `role="alert"` so a message appearing after a failed submission is announced
/// without moving focus away from the field the user is correcting.
///
/// The prop is a `Signal` rather than a plain `Option<String>` because the
/// message arrives *after* the render that drew the field: a form submits, the
/// server answers, and the message has to appear without the surrounding input
/// being rebuilt and losing focus. `into` still accepts a literal `Option` for
/// the static cases.
#[component]
pub fn FieldError(#[prop(into)] message: Signal<Option<String>>) -> impl IntoView {
    move || {
        message.get().map(|message| {
            view! {
                <small class="field-error" role="alert">
                    {message}
                </small>
            }
        })
    }
}

/// Formats a signed cent amount as `-$1,234.56`.
///
/// Uses `unsigned_abs` rather than `abs`: negating `i64::MIN` would panic in
/// debug and wrap in release.
pub fn format_cents(cents: i64) -> String {
    let magnitude = cents.unsigned_abs();
    let whole = magnitude / 100;
    let fraction = magnitude % 100;

    let digits = whole.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }

    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}${grouped}.{fraction:02}")
}

/// Maps a derived status to one of four visual tones.
///
/// Unknown input is neutral rather than a panic: this is a rendering decision,
/// and Feature 5's enum is the place that makes the status set exhaustive.
fn status_tone(status: &str) -> &'static str {
    match normalize_status(status).as_str() {
        "paid" => "ok",
        "partially_paid" => "warn",
        "overdue" => "bad",
        _ => "neutral",
    }
}

/// Turns `partially_paid` into `Partially paid`.
fn humanize_status(status: &str) -> String {
    let spaced = normalize_status(status).replace('_', " ");
    let mut characters = spaced.chars();

    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

fn normalize_status(status: &str) -> String {
    status.trim().to_lowercase().replace(['-', ' '], "_")
}

#[cfg(test)]
mod tests {
    use super::{format_cents, humanize_status, status_tone};

    #[test]
    fn formats_whole_and_partial_amounts() {
        assert_eq!(format_cents(0), "$0.00");
        assert_eq!(format_cents(5), "$0.05");
        assert_eq!(format_cents(50), "$0.50");
        assert_eq!(format_cents(100), "$1.00");
        assert_eq!(format_cents(1_099), "$10.99");
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(format_cents(123_456), "$1,234.56");
        assert_eq!(format_cents(100_000_000), "$1,000,000.00");
        assert_eq!(format_cents(99_999), "$999.99");
    }

    #[test]
    fn formats_negative_amounts() {
        assert_eq!(format_cents(-1), "-$0.01");
        assert_eq!(format_cents(-2_550), "-$25.50");
    }

    #[test]
    fn survives_the_extremes() {
        // `-i64::MIN` is not representable; this must not panic or wrap.
        assert_eq!(format_cents(i64::MIN), "-$92,233,720,368,547,758.08");
        assert_eq!(format_cents(i64::MAX), "$92,233,720,368,547,758.07");
    }

    #[test]
    fn maps_every_derived_status_to_a_tone() {
        assert_eq!(status_tone("paid"), "ok");
        assert_eq!(status_tone("partially_paid"), "warn");
        assert_eq!(status_tone("overdue"), "bad");
        assert_eq!(status_tone("pending"), "neutral");
    }

    #[test]
    fn tolerates_status_spelling_variants() {
        assert_eq!(status_tone("Partially Paid"), "warn");
        assert_eq!(status_tone("partially-paid"), "warn");
        assert_eq!(status_tone(" PAID "), "ok");
    }

    #[test]
    fn unknown_status_is_neutral_not_a_panic() {
        assert_eq!(status_tone("refunded"), "neutral");
        assert_eq!(status_tone(""), "neutral");
    }

    #[test]
    fn humanizes_status_labels() {
        assert_eq!(humanize_status("partially_paid"), "Partially paid");
        assert_eq!(humanize_status("paid"), "Paid");
        assert_eq!(humanize_status(""), "");
    }
}
