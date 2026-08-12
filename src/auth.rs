//! Integration with the shared Better Auth service.
//!
//! Better Auth is the only authority on identity. This application stores no
//! sessions, hashes no passwords, and mints no tokens: it forwards the browser's
//! cookie to Better Auth, believes the answer, and re-emits whatever
//! `Set-Cookie` headers come back so the cookie is stored against *this*
//! origin. The opaque `user.id` Better Auth returns is the tenant key every
//! later feature scopes its queries by.
//!
//! Three properties of the live service shaped this module, and each would have
//! been guessed wrong:
//!
//! - `GET /api/auth/get-session` answers `200` with a JSON `null` body for a
//!   missing, expired, or forged session — not `401`. "Signed out" and "the auth
//!   service is broken" are therefore different observations here, mapping to
//!   [`AppError::Unauthenticated`] and [`AppError::DependencyUnavailable`].
//! - `POST /api/auth/sign-out` returns `403 MISSING_OR_NULL_ORIGIN` unless an
//!   `Origin` header from its `TRUSTED_ORIGINS` list is forwarded, so the
//!   browser's own `Origin` is validated and passed through rather than
//!   synthesised.
//! - Sign-out clears **three** cookies, so every `Set-Cookie` header is
//!   re-emitted individually. Collapsing them into one would leave the browser
//!   holding two stale cookies after signing out.
//!
//! Nothing in this module logs a cookie, a token, or a password.

use leptos::prelude::*;
use leptos_meta::Title;
use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// The authenticated identity, as far as this application is concerned.
///
/// `id` is Better Auth's opaque user identifier. It is the tenant key: every
/// order row carries it, and every query filters on it. This application never
/// parses it, derives anything from it, or assumes a format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String,
    pub name: String,
    pub email: String,
}

// ---------------------------------------------------------------------------
// Server-only integration
// ---------------------------------------------------------------------------

#[cfg(feature = "ssr")]
pub mod ssr {
    use std::{sync::LazyLock, time::Duration};

    use axum::http::{
        header::{COOKIE, HOST, ORIGIN, SET_COOKIE},
        request::Parts,
        HeaderValue,
    };
    use leptos::prelude::use_context;
    use leptos_axum::ResponseOptions;
    use serde::{Deserialize, Serialize};

    use super::AuthUser;
    use crate::error::{AppError, AppResult};

    /// One connection pool for the whole process.
    ///
    /// Built lazily rather than in `main` so this module owns its own
    /// dependency and `main.rs` needs no knowledge of the auth service. Both
    /// timeouts are set explicitly: reqwest has no default request timeout, so
    /// a hung auth service would otherwise hold a request handler open forever
    /// instead of surfacing as a `503`.
    static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
        // rustls refuses to guess a crypto backend, and reqwest's
        // `rustls-no-provider` feature deliberately does not choose one either —
        // that is what keeps aws-lc-rs out of the build and leaves ring, which
        // sqlx already links, as the only provider in the binary. Something
        // still has to name it, or `build()` panics. `install_default` fails
        // only when a provider is already installed, which is not an error.
        let _ = rustls::crypto::ring::default_provider().install_default();

        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(3))
            .user_agent(concat!(
                "orders-and-settlements/",
                env!("CARGO_PKG_VERSION")
            ))
            // Better Auth answers `302` on some paths; following it would
            // replay a POST body against an unvetted location.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("the HTTP client configuration is static and cannot fail")
    });

    /// Human name for the dependency, used in every `503` message.
    const DEPENDENCY: &str = "The authentication service";

    /// Typed client for the Better Auth HTTP API.
    ///
    /// Holds only a base URL; the connection pool is shared and static. Cheap
    /// to construct per request, and constructible against any base URL so
    /// tests can point it at a mock server.
    #[derive(Clone, Debug)]
    pub struct AuthClient {
        base_url: String,
    }

    /// A Better Auth reply, split into the three parts callers care about,
    /// with the `Set-Cookie` headers captured *before* the body is consumed.
    #[derive(Debug)]
    struct RawReply {
        status: reqwest::StatusCode,
        cookies: Vec<String>,
        body: String,
    }

    /// Whatever Better Auth returned alongside the cookies it wants set.
    ///
    /// The cookies are carried out of the client rather than emitted from
    /// inside it so the client stays free of Leptos context and stays testable
    /// without a request in scope.
    #[derive(Debug)]
    pub struct AuthReply<T> {
        pub value: T,
        pub cookies: Vec<String>,
    }

    /// Better Auth's user object, as embedded in every successful reply.
    /// Unknown fields (`emailVerified`, `image`, timestamps) are ignored.
    #[derive(Debug, Deserialize)]
    struct UserEnvelope {
        user: AuthUser,
    }

    /// Better Auth's failure body: `{"code":"...","message":"..."}`.
    #[derive(Debug, Deserialize)]
    struct AuthErrorBody {
        code: Option<String>,
        message: Option<String>,
    }

    #[derive(Debug, Serialize)]
    struct SignUpBody<'a> {
        name: &'a str,
        email: &'a str,
        password: &'a str,
    }

    #[derive(Debug, Serialize)]
    struct SignInBody<'a> {
        email: &'a str,
        password: &'a str,
    }

    impl AuthClient {
        /// Points the client at a base URL, with or without a trailing slash.
        pub fn new(base_url: impl Into<String>) -> Self {
            let base_url = base_url.into().trim_end_matches('/').to_string();
            Self { base_url }
        }

        /// Reads `BETTER_AUTH_URL`, which `main.rs` validates as an absolute
        /// http(s) URL before the listener binds. Reaching the error arm means
        /// the process was started outside that path, which is a defect here,
        /// not a client mistake.
        pub fn from_env() -> AppResult<Self> {
            match std::env::var("BETTER_AUTH_URL") {
                Ok(url) if !url.trim().is_empty() => Ok(Self::new(url)),
                _ => {
                    tracing::error!("BETTER_AUTH_URL is unset; startup validation was bypassed");
                    Err(AppError::Internal)
                }
            }
        }

        /// Registers a user and returns the identity plus the session cookie.
        pub async fn sign_up(
            &self,
            name: &str,
            email: &str,
            password: &str,
        ) -> AppResult<AuthReply<AuthUser>> {
            let request = HTTP
                .post(self.endpoint("/api/auth/sign-up/email"))
                .json(&SignUpBody {
                    name,
                    email,
                    password,
                });

            self.user_reply(request).await
        }

        /// Exchanges credentials for a session cookie.
        pub async fn sign_in(&self, email: &str, password: &str) -> AppResult<AuthReply<AuthUser>> {
            let request = HTTP
                .post(self.endpoint("/api/auth/sign-in/email"))
                .json(&SignInBody { email, password });

            self.user_reply(request).await
        }

        /// Revokes the session behind `cookie`.
        ///
        /// `origin` must be one Better Auth trusts, or it answers `403` without
        /// clearing anything. The returned cookies are the expiring ones, and
        /// re-emitting them is what actually signs the browser out.
        pub async fn sign_out(&self, cookie: &str, origin: &str) -> AppResult<AuthReply<()>> {
            let request = HTTP
                .post(self.endpoint("/api/auth/sign-out"))
                .header(COOKIE.as_str(), cookie)
                .header(ORIGIN.as_str(), origin);

            let reply = self.send(request).await?;

            if reply.status.is_success() {
                return Ok(AuthReply {
                    value: (),
                    cookies: reply.cookies,
                });
            }

            Err(self.interpret_failure(&reply, "sign-out"))
        }

        /// Resolves a cookie to an identity.
        ///
        /// A `None` value means the session is absent, expired, or forged —
        /// Better Auth signals all three with `200` and a `null` body. Only a
        /// genuine outage produces an `Err`, which is what keeps a signed-out
        /// visitor from ever being told the service is down.
        ///
        /// Cookies come back from here too, and they matter: Better Auth
        /// rotates the session cookie once a session passes its `updateAge`,
        /// and it does so on this endpoint. Dropping that header would work
        /// perfectly until the day a long-lived session silently expired mid-use.
        pub async fn get_session(&self, cookie: &str) -> AppResult<AuthReply<Option<AuthUser>>> {
            let request = HTTP
                .get(self.endpoint("/api/auth/get-session"))
                .header(COOKIE.as_str(), cookie);

            let reply = self.send(request).await?;

            if !reply.status.is_success() {
                return Err(self.interpret_failure(&reply, "get-session"));
            }

            // `null`, `{}`, or an empty body all mean "no session".
            let trimmed = reply.body.trim();
            if trimmed.is_empty() || trimmed == "null" {
                return Ok(AuthReply {
                    value: None,
                    cookies: reply.cookies,
                });
            }

            match serde_json::from_str::<UserEnvelope>(trimmed) {
                Ok(envelope) => Ok(AuthReply {
                    value: Some(envelope.user),
                    cookies: reply.cookies,
                }),
                Err(error) => {
                    // Body deliberately not logged: it contains the session.
                    tracing::error!(error = %error, "get-session returned an unreadable body");
                    Err(AppError::DependencyUnavailable(DEPENDENCY.to_string()))
                }
            }
        }

        fn endpoint(&self, path: &str) -> String {
            format!("{}{path}", self.base_url)
        }

        /// Shared tail of sign-up and sign-in: both return `{token, user}` on
        /// success and `{code, message}` on rejection.
        async fn user_reply(
            &self,
            request: reqwest::RequestBuilder,
        ) -> AppResult<AuthReply<AuthUser>> {
            let reply = self.send(request).await?;

            if !reply.status.is_success() {
                return Err(self.interpret_failure(&reply, "credential exchange"));
            }

            match serde_json::from_str::<UserEnvelope>(&reply.body) {
                Ok(envelope) => Ok(AuthReply {
                    value: envelope.user,
                    cookies: reply.cookies,
                }),
                Err(error) => {
                    // The body holds a bearer token; only the parse error is
                    // safe to log.
                    tracing::error!(error = %error, "auth service returned an unreadable body");
                    Err(AppError::DependencyUnavailable(DEPENDENCY.to_string()))
                }
            }
        }

        /// Performs the request, capturing `Set-Cookie` headers before the body
        /// is read. `Response::text` consumes the response, so the headers have
        /// to be cloned out first.
        async fn send(&self, request: reqwest::RequestBuilder) -> AppResult<RawReply> {
            let response = request.send().await.map_err(|error| {
                // `error` renders the URL and the transport cause, never a
                // header, so it is safe to log in full.
                tracing::error!(error = %error, "could not reach the authentication service");
                AppError::DependencyUnavailable(DEPENDENCY.to_string())
            })?;

            let status = response.status();
            let cookies = response
                .headers()
                .get_all(SET_COOKIE.as_str())
                .iter()
                .filter_map(|value| value.to_str().ok().map(str::to_owned))
                .collect();

            let body = response.text().await.map_err(|error| {
                tracing::error!(error = %error, "could not read the authentication response");
                AppError::DependencyUnavailable(DEPENDENCY.to_string())
            })?;

            Ok(RawReply {
                status,
                cookies,
                body,
            })
        }

        /// Turns a non-2xx Better Auth reply into the error the user should see.
        ///
        /// A `5xx` is the service's fault and becomes a `503`. A `4xx` is a
        /// statement about the request, and Better Auth's own message
        /// ("Invalid email or password", "Password too short") is better than
        /// anything this application could invent, so it is passed through.
        fn interpret_failure(&self, reply: &RawReply, operation: &str) -> AppError {
            let parsed = serde_json::from_str::<AuthErrorBody>(&reply.body).ok();
            let code = parsed
                .as_ref()
                .and_then(|body| body.code.as_deref())
                .unwrap_or("UNKNOWN");

            if reply.status.is_server_error() {
                tracing::error!(
                    status = %reply.status,
                    code,
                    operation,
                    "authentication service failed"
                );
                return AppError::DependencyUnavailable(DEPENDENCY.to_string());
            }

            tracing::warn!(status = %reply.status, code, operation, "authentication rejected");

            // A rejected origin is this application's misconfiguration, not
            // something to explain to the user in Better Auth's words.
            if code == "MISSING_OR_NULL_ORIGIN" || code == "ORIGIN_NOT_ALLOWED" {
                return AppError::ForbiddenOrigin;
            }

            let message = parsed
                .and_then(|body| body.message)
                .filter(|message| !message.trim().is_empty())
                .unwrap_or_else(|| "Those credentials were not accepted.".to_string());

            AppError::AuthRejected(message)
        }
    }

    /// The request currently being served, or an error if there is none.
    ///
    /// `leptos_axum` provides `Parts` into context on both paths that matter:
    /// the server-function handler and the SSR render. It is absent only during
    /// `generate_route_list` at startup, where no user exists to authenticate.
    pub fn incoming_parts() -> AppResult<Parts> {
        use_context::<Parts>().ok_or_else(|| {
            tracing::error!("no request in scope; cannot authenticate");
            AppError::Internal
        })
    }

    /// The browser's session cookie header, verbatim.
    pub fn incoming_cookie(parts: &Parts) -> Option<&str> {
        parts.headers.get(COOKIE)?.to_str().ok()
    }

    /// This application's own origin, as the client addressed it.
    ///
    /// Derived from the request rather than from configuration so there is one
    /// fewer environment variable to keep in sync with Better Auth's
    /// `TRUSTED_ORIGINS`. `X-Forwarded-Proto` is honoured for deployment behind
    /// TLS termination; its first value wins, as proxies append.
    fn self_origin(parts: &Parts) -> Option<String> {
        let host = parts
            .headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            // HTTP/2 carries no Host header; hyper puts `:authority` here.
            .or_else(|| parts.uri.authority().map(|authority| authority.to_string()))?;

        let scheme = parts
            .headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|scheme| *scheme == "http" || *scheme == "https")
            .unwrap_or("http");

        Some(format!("{scheme}://{host}"))
    }

    /// Rejects any state-changing request that did not come from a page this
    /// application served, and returns the validated origin.
    ///
    /// This is a Rust-side CSRF check that does not depend on Better Auth. The
    /// session cookie is `SameSite=Lax`, which already blocks cross-site
    /// `fetch`, but Lax still permits top-level cross-site form `POST`s in some
    /// browsers — so the cookie alone is not sufficient authority to act. The
    /// returned origin is exactly what Better Auth's `sign-out` requires, which
    /// is why the check produces a value instead of just a verdict.
    pub fn ensure_same_origin(parts: &Parts) -> AppResult<String> {
        let expected = self_origin(parts).ok_or_else(|| {
            tracing::warn!("request carried neither a Host header nor an authority");
            AppError::ForbiddenOrigin
        })?;

        let origin = parts
            .headers
            .get(ORIGIN)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                tracing::warn!("state-changing request carried no Origin header");
                AppError::ForbiddenOrigin
            })?;

        if origin.eq_ignore_ascii_case(&expected) {
            Ok(expected)
        } else {
            tracing::warn!(%origin, expected = %expected, "rejected a cross-origin request");
            Err(AppError::ForbiddenOrigin)
        }
    }

    /// Re-emits Better Auth's cookies on this application's own response.
    ///
    /// Appended one header at a time, never joined: `Set-Cookie` cannot be
    /// comma-folded, and sign-out sends three of them. The cookies arrive with
    /// `Path=/`, `HttpOnly`, `SameSite=Lax` and **no** `Domain`, so re-emitting
    /// them verbatim scopes them to this origin — which is exactly what makes a
    /// separately hosted auth service usable from here without CORS.
    pub fn append_set_cookie_headers(cookies: &[String]) {
        let Some(response) = use_context::<ResponseOptions>() else {
            tracing::error!("no ResponseOptions in scope; the session cookie was dropped");
            return;
        };

        for cookie in cookies {
            match HeaderValue::from_str(cookie) {
                // The value is a session token. Only its failure is loggable.
                Err(_) => tracing::error!("auth service sent an unencodable Set-Cookie header"),
                Ok(value) => response.append_header(SET_COOKIE, value),
            }
        }
    }

    /// Resolves the caller's identity, or `None` if they are not signed in.
    ///
    /// Returns `Ok(None)` rather than an error when there is no request in
    /// scope, because `generate_route_list` renders the whole application once
    /// at startup with no HTTP request behind it.
    pub async fn optional_user() -> AppResult<Option<AuthUser>> {
        let Some(parts) = use_context::<Parts>() else {
            return Ok(None);
        };

        let Some(cookie) = incoming_cookie(&parts) else {
            return Ok(None);
        };

        let reply = AuthClient::from_env()?.get_session(cookie).await?;
        // Usually empty. When it is not, Better Auth has rotated the session
        // cookie and this is the only chance to hand the new one to the browser.
        append_set_cookie_headers(&reply.cookies);

        Ok(reply.value)
    }

    /// The authorization gate every protected server function calls first.
    ///
    /// Every later feature scopes its queries by the returned `id`, so this is
    /// the single point where "who is asking" is decided. It deliberately
    /// re-validates with Better Auth on each call rather than trusting a
    /// locally cached claim: a session revoked in another tab stops working
    /// here on the next request, not at some expiry.
    pub async fn require_user() -> AppResult<AuthUser> {
        optional_user().await?.ok_or(AppError::Unauthenticated)
    }
}

// ---------------------------------------------------------------------------
// Server functions
//
// Each is a real HTTP endpoint generated by the macro. The bodies exist only in
// the `ssr` build; the browser gets a typed stub that posts to the endpoint.
// ---------------------------------------------------------------------------

/// Registers a new user and signs them in.
///
/// Every body below is wrapped in `report`, which copies the error's real HTTP
/// status onto the response. Without it `server_fn` would answer `500` for a
/// rejected password just as readily as for a crash.
#[server]
pub async fn sign_up(name: String, email: String, password: String) -> Result<(), AppError> {
    use crate::error::ssr::report;
    use ssr::{append_set_cookie_headers, ensure_same_origin, incoming_parts, AuthClient};

    report(
        async move {
            let parts = incoming_parts()?;
            ensure_same_origin(&parts)?;

            let reply = AuthClient::from_env()?
                .sign_up(name.trim(), email.trim(), &password)
                .await?;

            append_set_cookie_headers(&reply.cookies);
            // The opaque user id is safe to log and is the only handle support
            // has on a row; the token and the password are never touched.
            tracing::info!(user_id = %reply.value.id, "user registered");
            leptos_axum::redirect("/");

            Ok(())
        }
        .await,
    )
}

/// Exchanges credentials for a session cookie on this origin.
#[server]
pub async fn sign_in(email: String, password: String) -> Result<(), AppError> {
    use crate::error::ssr::report;
    use ssr::{append_set_cookie_headers, ensure_same_origin, incoming_parts, AuthClient};

    report(
        async move {
            let parts = incoming_parts()?;
            ensure_same_origin(&parts)?;

            let reply = AuthClient::from_env()?
                .sign_in(email.trim(), &password)
                .await?;

            append_set_cookie_headers(&reply.cookies);
            tracing::info!(user_id = %reply.value.id, "user signed in");
            leptos_axum::redirect("/");

            Ok(())
        }
        .await,
    )
}

/// Revokes the session and clears every cookie Better Auth set.
///
/// Signing out while already signed out is not an error: the goal state is
/// "no session", and that is already true.
#[server]
pub async fn sign_out() -> Result<(), AppError> {
    use crate::error::ssr::report;
    use ssr::{
        append_set_cookie_headers, ensure_same_origin, incoming_cookie, incoming_parts, AuthClient,
    };

    report(
        async move {
            let parts = incoming_parts()?;
            let origin = ensure_same_origin(&parts)?;

            if let Some(cookie) = incoming_cookie(&parts) {
                let reply = AuthClient::from_env()?.sign_out(cookie, &origin).await?;
                append_set_cookie_headers(&reply.cookies);
                tracing::info!("user signed out");
            }

            leptos_axum::redirect("/auth");

            Ok(())
        }
        .await,
    )
}

/// Reports who the caller is, for rendering only.
///
/// This is not an authorization decision. Every protected server function calls
/// `require_user` itself; hiding a page from a signed-out visitor is a courtesy,
/// and hiding it is never what keeps their data safe.
#[server]
pub async fn current_user() -> Result<Option<AuthUser>, AppError> {
    crate::error::ssr::report(ssr::optional_user().await)
}

// ---------------------------------------------------------------------------
// Client-side auth state
// ---------------------------------------------------------------------------

/// The three auth actions and the identity they invalidate, shared by every
/// component through context.
///
/// The resource's source is the three action versions, so completing any of
/// them refetches the identity. Without that link, signing in would leave the
/// header and every protected page displaying the previous, stale answer until
/// a full page load.
#[derive(Copy, Clone)]
pub struct AuthContext {
    pub sign_up: ServerAction<SignUp>,
    pub sign_in: ServerAction<SignIn>,
    pub sign_out: ServerAction<SignOut>,
    pub user: Resource<Result<Option<AuthUser>, AppError>>,
}

/// Creates the auth context and makes it available to the whole application.
/// Called once, from `App`.
pub fn provide_auth() {
    let sign_up = ServerAction::<SignUp>::new();
    let sign_in = ServerAction::<SignIn>::new();
    let sign_out = ServerAction::<SignOut>::new();

    let user = Resource::new(
        move || {
            (
                sign_up.version().get(),
                sign_in.version().get(),
                sign_out.version().get(),
            )
        },
        |_| current_user(),
    );

    provide_context(AuthContext {
        sign_up,
        sign_in,
        sign_out,
        user,
    });
}

/// Panics if [`provide_auth`] has not run. Every caller is rendered underneath
/// `App`, so a failure here is a wiring defect, not a runtime condition.
pub fn expect_auth() -> AuthContext {
    expect_context::<AuthContext>()
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

/// Renders `children` only for a signed-in visitor.
///
/// `Transition` rather than `Suspense`: on a client-side navigation the identity
/// is already known, and `Suspense` would blank the page back to its fallback
/// while it revalidated.
#[component]
pub fn Protected(children: ChildrenFn) -> impl IntoView {
    use leptos::either::Either;

    let auth = expect_auth();

    view! {
        <Transition fallback=|| {
            view! { <p aria-busy="true">"Checking your session…"</p> }
        }>
            {move || {
                let children = children.clone();
                Suspend::new(async move {
                    match auth.user.await {
                        Ok(Some(_)) => Either::Left(children()),
                        Ok(None) => Either::Right(view! { <SignInRequired error=None /> }),
                        Err(error) => {
                            Either::Right(view! { <SignInRequired error=Some(error) /> })
                        }
                    }
                })
            }}
        </Transition>
    }
}

/// Shown in place of a protected page. Distinguishes "you are signed out" from
/// "the auth service is down", because only one of those is worth retrying.
#[component]
fn SignInRequired(error: Option<AppError>) -> impl IntoView {
    use leptos_router::components::A;

    let message = match &error {
        None | Some(AppError::Unauthenticated) => "Sign in to see this page.".to_string(),
        Some(error) => error.to_string(),
    };

    view! {
        <article role="status">
            <header>"Not signed in"</header>
            <p>{message}</p>
            <p>
                <A href="/auth">"Go to sign in"</A>
            </p>
        </article>
    }
}

/// The account controls in the header: the signed-in address and a sign-out
/// button, or a link to the auth page.
///
/// The fallback renders nothing rather than a placeholder, so the navigation
/// does not visibly reflow on every page load.
#[component]
pub fn AccountNav() -> impl IntoView {
    use leptos::either::Either;
    use leptos_router::components::A;

    let auth = expect_auth();

    view! {
        <Transition fallback=|| ()>
            {move || {
                Suspend::new(async move {
                    match auth.user.await {
                        Ok(Some(user)) => {
                            Either::Left(
                                view! {
                                    <li>
                                        <small>{user.email}</small>
                                    </li>
                                    <li>
                                        <ActionForm action=auth.sign_out>
                                            <button type="submit" class="outline secondary">
                                                "Sign out"
                                            </button>
                                        </ActionForm>
                                    </li>
                                },
                            )
                        }
                        _ => {
                            Either::Right(
                                view! {
                                    <li>
                                        <A href="/auth">"Sign in"</A>
                                    </li>
                                },
                            )
                        }
                    }
                })
            }}
        </Transition>
    }
}

/// The sign-in and sign-up page.
///
/// Both forms are `ActionForm`s, which means they are real `<form>` elements
/// posting to a real endpoint. They work before the WASM bundle has loaded and
/// they work with scripting disabled: on that path the server function's
/// `redirect` sets a `302` and the browser follows it. With scripting, the same
/// `Location` header drives a client-side navigation instead.
#[component]
pub fn AuthPage() -> impl IntoView {
    let auth = expect_auth();

    view! {
        <Title text="Sign in - Orders and Settlements" />
        <h1>"Sign in"</h1>
        <p>"Orders are private to the account that created them."</p>

        <div class="grid">
            <article>
                <header>
                    <strong>"I have an account"</strong>
                </header>
                <ActionForm action=auth.sign_in>
                    <label>
                        "Email"
                        <input type="email" name="email" autocomplete="username" required />
                    </label>
                    <label>
                        "Password"
                        <input
                            type="password"
                            name="password"
                            autocomplete="current-password"
                            required
                        />
                    </label>
                    <button type="submit" disabled=move || auth.sign_in.pending().get()>
                        "Sign in"
                    </button>
                    <ActionError value=Signal::derive(move || auth.sign_in.value().get()) />
                </ActionForm>
            </article>

            <article>
                <header>
                    <strong>"I am new here"</strong>
                </header>
                <ActionForm action=auth.sign_up>
                    <label>
                        "Name"
                        <input type="text" name="name" autocomplete="name" required />
                    </label>
                    <label>
                        "Email"
                        <input type="email" name="email" autocomplete="username" required />
                    </label>
                    <label>
                        "Password"
                        // Better Auth enforces its own minimum and answers
                        // PASSWORD_TOO_SHORT; the hint keeps the user from
                        // having to discover that by failing.
                        <input
                            type="password"
                            name="password"
                            autocomplete="new-password"
                            minlength="8"
                            required
                        />
                    </label>
                    <button type="submit" disabled=move || auth.sign_up.pending().get()>
                        "Create account"
                    </button>
                    <ActionError value=Signal::derive(move || auth.sign_up.value().get()) />
                </ActionForm>
            </article>
        </div>
    }
}

/// Renders whatever an action last failed with, or nothing.
///
/// Takes a derived signal rather than the action itself so one component serves
/// both forms despite their different argument types.
#[component]
fn ActionError(value: Signal<Option<Result<(), AppError>>>) -> impl IntoView {
    move || {
        value.get().and_then(Result::err).map(|error| {
            view! {
                <small class="field-error" role="alert">
                    {error.to_string()}
                </small>
            }
        })
    }
}
