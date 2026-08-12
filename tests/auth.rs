//! Contract tests for the Better Auth integration.
//!
//! These run against `wiremock` rather than the live service, on purpose. The
//! behaviours that actually matter here are the failure modes — an outage, a
//! rotated cookie, a malformed body, a rejected origin — and a healthy Node
//! process will not produce any of them on demand. The mocked replies are
//! transcribed from probes against the real service, including the two shapes
//! that are easy to guess wrong: `get-session` answering `200 null` instead of
//! `401`, and `sign-out` returning three separate `Set-Cookie` headers.
//!
//! Everything under test is the pure, context-free half of `src/auth.rs`. The
//! Leptos server functions are a thin wrapper over exactly these calls.

#![cfg(feature = "ssr")]

use axum::http::{request::Parts, Request};
use orders_and_settlements::{
    auth::ssr::{ensure_same_origin, AuthClient},
    error::AppError,
};
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

/// The exact cookie the live service sets: `Path=/`, `HttpOnly`, `SameSite=Lax`,
/// and crucially **no `Domain` and no `Secure`**. That is what allows this
/// application to re-emit it on its own origin.
const SESSION_COOKIE: &str =
    "better-auth.session_token=tok.sig; Max-Age=604800; Path=/; HttpOnly; SameSite=Lax";

fn user_body() -> serde_json::Value {
    serde_json::json!({
        "token": "tok",
        "user": {
            "id": "usr_01HX",
            "name": "Ada Lovelace",
            "email": "ada@example.com",
            "emailVerified": false,
            "image": null,
            "createdAt": "2026-08-12T09:00:00.000Z",
            "updatedAt": "2026-08-12T09:00:00.000Z"
        }
    })
}

fn session_body() -> serde_json::Value {
    serde_json::json!({
        "session": {
            "id": "ses_01HX",
            "userId": "usr_01HX",
            "expiresAt": "2026-08-19T09:00:00.000Z",
            "token": "tok"
        },
        "user": {
            "id": "usr_01HX",
            "name": "Ada Lovelace",
            "email": "ada@example.com",
            "emailVerified": false
        }
    })
}

/// Builds request parts the way Axum would hand them to a server function.
fn parts(headers: &[(&str, &str)]) -> Parts {
    let mut request = Request::builder().uri("/api/sign_in");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }

    request
        .body(())
        .expect("the test request is well formed")
        .into_parts()
        .0
}

// ---------------------------------------------------------------------------
// Credential exchange
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sign_in_returns_the_identity_and_the_session_cookie() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/auth/sign-in/email"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(user_body())
                .append_header("set-cookie", SESSION_COOKIE),
        )
        .expect(1)
        .mount(&server)
        .await;

    let reply = AuthClient::new(server.uri())
        .sign_in("ada@example.com", "correct horse battery")
        .await
        .expect("a 200 reply is a successful sign-in");

    assert_eq!(reply.value.id, "usr_01HX");
    assert_eq!(reply.value.email, "ada@example.com");
    assert_eq!(reply.value.name, "Ada Lovelace");
    // The cookie must survive verbatim: rewriting any attribute here would
    // change the scope the browser stores it under.
    assert_eq!(reply.cookies, vec![SESSION_COOKIE.to_string()]);
}

#[tokio::test]
async fn a_trailing_slash_on_the_base_url_does_not_double_up() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/auth/sign-in/email"))
        .respond_with(ResponseTemplate::new(200).set_body_json(user_body()))
        .expect(1)
        .mount(&server)
        .await;

    AuthClient::new(format!("{}/", server.uri()))
        .sign_in("ada@example.com", "correct horse battery")
        .await
        .expect("the path must resolve identically with or without the slash");
}

#[tokio::test]
async fn wrong_credentials_surface_better_auths_own_message() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/auth/sign-in/email"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "code": "INVALID_EMAIL_OR_PASSWORD",
            "message": "Invalid email or password"
        })))
        .mount(&server)
        .await;

    let error = AuthClient::new(server.uri())
        .sign_in("ada@example.com", "wrong")
        .await
        .expect_err("a 401 is a rejection");

    // Not Unauthenticated: the caller is *trying* to authenticate, and telling
    // them to sign in would be circular.
    assert_eq!(
        error,
        AppError::AuthRejected("Invalid email or password".to_string())
    );
    assert_eq!(error.status_code(), 400);
}

#[tokio::test]
async fn a_duplicate_signup_is_rejected_not_treated_as_an_outage() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/auth/sign-up/email"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "code": "USER_ALREADY_EXISTS_USE_ANOTHER_EMAIL",
            "message": "User already exists. Use another email."
        })))
        .mount(&server)
        .await;

    let error = AuthClient::new(server.uri())
        .sign_up("Ada Lovelace", "ada@example.com", "correct horse battery")
        .await
        .expect_err("a 422 is a rejection");

    assert_eq!(
        error,
        AppError::AuthRejected("User already exists. Use another email.".to_string())
    );
}

#[tokio::test]
async fn a_rejection_without_a_usable_message_still_says_something() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/auth/sign-in/email"))
        .respond_with(ResponseTemplate::new(400).set_body_string("not json at all"))
        .mount(&server)
        .await;

    let error = AuthClient::new(server.uri())
        .sign_in("ada@example.com", "wrong")
        .await
        .expect_err("a 400 is a rejection");

    assert_eq!(
        error,
        AppError::AuthRejected("Those credentials were not accepted.".to_string())
    );
}

// ---------------------------------------------------------------------------
// Session resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_cookie_resolves_to_the_opaque_user_id() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/auth/get-session"))
        // Proves the browser's cookie is forwarded verbatim. Without this the
        // service would answer for the wrong session, or for none.
        .and(header("cookie", "better-auth.session_token=tok.sig"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_body()))
        .expect(1)
        .mount(&server)
        .await;

    let reply = AuthClient::new(server.uri())
        .get_session("better-auth.session_token=tok.sig")
        .await
        .expect("a 200 reply is not an outage");

    assert_eq!(reply.value.expect("the session is valid").id, "usr_01HX");
}

#[tokio::test]
async fn an_invalid_session_is_signed_out_not_an_error() {
    let server = MockServer::start().await;

    // The single most surprising part of the contract: Better Auth answers 200
    // with a bare `null` for a session it does not recognise.
    Mock::given(method("GET"))
        .and(path("/api/auth/get-session"))
        .respond_with(ResponseTemplate::new(200).set_body_string("null"))
        .mount(&server)
        .await;

    let reply = AuthClient::new(server.uri())
        .get_session("better-auth.session_token=forged.sig")
        .await
        .expect("200 null must not be read as a failure");

    assert!(reply.value.is_none());
}

#[tokio::test]
async fn an_empty_body_is_also_signed_out() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/auth/get-session"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let reply = AuthClient::new(server.uri())
        .get_session("better-auth.session_token=tok.sig")
        .await
        .expect("an empty body must not be read as a failure");

    assert!(reply.value.is_none());
}

#[tokio::test]
async fn a_rotated_session_cookie_is_carried_back_out() {
    let server = MockServer::start().await;
    let rotated =
        "better-auth.session_token=fresh.sig; Max-Age=604800; Path=/; HttpOnly; SameSite=Lax";

    Mock::given(method("GET"))
        .and(path("/api/auth/get-session"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(session_body())
                .append_header("set-cookie", rotated),
        )
        .mount(&server)
        .await;

    let reply = AuthClient::new(server.uri())
        .get_session("better-auth.session_token=stale.sig")
        .await
        .expect("a rotated cookie accompanies a valid session");

    // Dropping this would work until a long-lived session passed its updateAge
    // and then silently expired mid-use.
    assert_eq!(reply.cookies, vec![rotated.to_string()]);
}

#[tokio::test]
async fn an_auth_outage_is_a_503_not_a_sign_out() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/auth/get-session"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let error = AuthClient::new(server.uri())
        .get_session("better-auth.session_token=tok.sig")
        .await
        .expect_err("a 5xx from the auth service is an outage");

    assert!(matches!(error, AppError::DependencyUnavailable(_)));
    assert_eq!(error.status_code(), 503);
}

#[tokio::test]
async fn an_unreachable_auth_service_is_a_503() {
    // Port 1 is privileged, so nothing can be listening on it and the connection
    // is refused immediately. Shutting a mock server down instead would be
    // racy: the socket outlives the handle long enough to answer 404, which
    // this code would correctly read as a rejection rather than an outage.
    let error = AuthClient::new("http://127.0.0.1:1")
        .get_session("better-auth.session_token=tok.sig")
        .await
        .expect_err("a refused connection is an outage");

    assert!(
        matches!(error, AppError::DependencyUnavailable(_)),
        "expected an outage, got {error:?}"
    );
    assert_eq!(error.status_code(), 503);
}

#[tokio::test]
async fn an_unreadable_body_is_an_outage_not_a_sign_out() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/auth/get-session"))
        // 200 with a shape we cannot read: the service is misbehaving, which is
        // not the same as the visitor being signed out.
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"user": 42})))
        .mount(&server)
        .await;

    let error = AuthClient::new(server.uri())
        .get_session("better-auth.session_token=tok.sig")
        .await
        .expect_err("a malformed 200 is an outage");

    assert!(matches!(error, AppError::DependencyUnavailable(_)));
}

// ---------------------------------------------------------------------------
// Sign-out
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sign_out_preserves_every_clearing_cookie() {
    let server = MockServer::start().await;
    let cleared = [
        "better-auth.session_token=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax",
        "better-auth.session_data=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax",
        "better-auth.dont_remember=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax",
    ];

    let mut response =
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true}));
    for cookie in cleared {
        response = response.append_header("set-cookie", cookie);
    }

    Mock::given(method("POST"))
        .and(path("/api/auth/sign-out"))
        .and(header("cookie", "better-auth.session_token=tok.sig"))
        // Better Auth answers 403 without this; forwarding the browser's own
        // validated origin is what makes sign-out work at all.
        .and(header("origin", "http://localhost:5174"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;

    let reply = AuthClient::new(server.uri())
        .sign_out("better-auth.session_token=tok.sig", "http://localhost:5174")
        .await
        .expect("a 200 reply is a successful sign-out");

    // All three, individually. Comma-folding them into one header would leave
    // the browser holding two stale cookies.
    assert_eq!(reply.cookies.len(), 3);
    for cookie in cleared {
        assert!(
            reply.cookies.iter().any(|value| value == cookie),
            "missing {cookie}"
        );
    }
}

#[tokio::test]
async fn a_rejected_origin_is_reported_as_forbidden_not_as_bad_credentials() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/auth/sign-out"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "code": "ORIGIN_NOT_ALLOWED",
            "message": "Invalid origin"
        })))
        .mount(&server)
        .await;

    let error = AuthClient::new(server.uri())
        .sign_out("better-auth.session_token=tok.sig", "http://evil.example")
        .await
        .expect_err("an untrusted origin is refused");

    // Better Auth's wording describes *our* misconfiguration, so it is replaced
    // rather than shown.
    assert_eq!(error, AppError::ForbiddenOrigin);
    assert_eq!(error.status_code(), 403);
}

// ---------------------------------------------------------------------------
// Origin check
//
// This is the Rust-side CSRF gate. It runs before any credential leaves the
// process, and it does not depend on Better Auth agreeing with it.
// ---------------------------------------------------------------------------

#[test]
fn a_same_origin_request_is_accepted_and_yields_the_origin() {
    let origin = ensure_same_origin(&parts(&[
        ("host", "localhost:5174"),
        ("origin", "http://localhost:5174"),
    ]))
    .expect("a request from our own page is trusted");

    // The value is not just a verdict: it is what gets forwarded to sign-out.
    assert_eq!(origin, "http://localhost:5174");
}

#[test]
fn a_cross_origin_request_is_refused() {
    let error = ensure_same_origin(&parts(&[
        ("host", "localhost:5174"),
        ("origin", "http://evil.example"),
    ]))
    .expect_err("a request from another site is not trusted");

    assert_eq!(error, AppError::ForbiddenOrigin);
}

#[test]
fn a_request_with_no_origin_header_is_refused() {
    // A cookie alone is not authority to act: SameSite=Lax still permits a
    // top-level cross-site form POST.
    let error = ensure_same_origin(&parts(&[("host", "localhost:5174")]))
        .expect_err("a state-changing request must prove where it came from");

    assert_eq!(error, AppError::ForbiddenOrigin);
}

#[test]
fn a_request_with_no_host_header_is_refused() {
    let error = ensure_same_origin(&parts(&[("origin", "http://localhost:5174")]))
        .expect_err("without a Host there is nothing to compare against");

    assert_eq!(error, AppError::ForbiddenOrigin);
}

#[test]
fn the_same_host_on_a_different_port_is_a_different_origin() {
    let error = ensure_same_origin(&parts(&[
        ("host", "localhost:5174"),
        ("origin", "http://localhost:3000"),
    ]))
    .expect_err("port is part of the origin");

    assert_eq!(error, AppError::ForbiddenOrigin);
}

#[test]
fn tls_termination_is_honoured_through_x_forwarded_proto() {
    let origin = ensure_same_origin(&parts(&[
        ("host", "orders.example.com"),
        ("origin", "https://orders.example.com"),
        ("x-forwarded-proto", "https"),
    ]))
    .expect("behind a TLS-terminating proxy the scheme comes from the header");

    assert_eq!(origin, "https://orders.example.com");
}

#[test]
fn only_the_first_forwarded_proto_is_trusted() {
    // Proxies append, so the left-most value is the one the client saw. A
    // second value must not be able to downgrade the comparison.
    let origin = ensure_same_origin(&parts(&[
        ("host", "orders.example.com"),
        ("origin", "https://orders.example.com"),
        ("x-forwarded-proto", "https, http"),
    ]))
    .expect("the client-facing scheme wins");

    assert_eq!(origin, "https://orders.example.com");
}

#[test]
fn a_nonsense_forwarded_proto_falls_back_rather_than_being_echoed() {
    let error = ensure_same_origin(&parts(&[
        ("host", "orders.example.com"),
        ("origin", "javascript://orders.example.com"),
        ("x-forwarded-proto", "javascript"),
    ]))
    .expect_err("only http and https are schemes this application will assume");

    assert_eq!(error, AppError::ForbiddenOrigin);
}

// ---------------------------------------------------------------------------
// Error contract
// ---------------------------------------------------------------------------

#[test]
fn every_auth_error_maps_to_the_status_a_client_expects() {
    assert_eq!(AppError::Unauthenticated.status_code(), 401);
    assert_eq!(AppError::Unauthenticated.code(), "UNAUTHENTICATED");
    assert_eq!(AppError::ForbiddenOrigin.status_code(), 403);
    assert_eq!(AppError::ForbiddenOrigin.code(), "FORBIDDEN_ORIGIN");
}

#[test]
fn app_error_survives_the_round_trip_through_a_server_function() {
    use leptos::server_fn::error::FromServerFnError;

    // This is how a server function's `Err` actually reaches the browser: the
    // server encodes it into the response body and the client decodes it back.
    // If this broke, every failure would arrive as an opaque decoding error.
    for error in [
        AppError::Unauthenticated,
        AppError::ForbiddenOrigin,
        AppError::AuthRejected("Invalid email or password".to_string()),
        AppError::DependencyUnavailable("The authentication service".to_string()),
    ] {
        assert_eq!(AppError::de(error.ser()), error);
    }
}

#[test]
fn a_dropped_connection_reaches_the_caller_as_a_transport_error() {
    use leptos::server_fn::error::{FromServerFnError, ServerFnErrorErr};

    let error =
        AppError::from_server_fn_error(ServerFnErrorErr::Request("network error".to_string()));

    assert_eq!(error, AppError::Transport("network error".to_string()));
}
