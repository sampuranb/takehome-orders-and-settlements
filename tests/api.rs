//! Contract tests for the REST API.
//!
//! These drive the **real** router — `orders::api::router()`, the same function
//! `main.rs` mounts — through `tower`'s `oneshot`, in process and without
//! binding a port. That is the point: the business rules already have thorough
//! coverage in `tests/orders.rs` and `tests/payments.rs` against the services
//! directly, and duplicating them here would test the same code twice while
//! still missing what only this layer can get wrong — a path that does not
//! match, a method that is not registered, a status code that does not match
//! the failure, an error body in a second shape, or a handler that reports "not
//! signed in" as a `500`.
//!
//! Better Auth is a `wiremock` server keyed on the session cookie: each test
//! has its own cookie resolving to its own owner id, and anything else is
//! answered `200 null`, which is what the live service really returns for a
//! dead session. One mock server is shared by the whole file because
//! `BETTER_AUTH_URL` is process-wide and the tests run in parallel — a server
//! per test would race.
//!
//! `DATABASE_URL` must be set and PostgreSQL must be running, as for the other
//! integration tests:
//!
//! ```text
//! set -a; . ./.env; set +a; cargo test --features ssr --no-default-features
//! ```

#![cfg(feature = "ssr")]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::OnceCell;
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{header as header_matcher, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Identities
// ---------------------------------------------------------------------------

/// One identity per test, because these tests run in parallel and each one
/// deletes its owner's orders when it finishes. Sharing an owner would mean one
/// test's cleanup wiping rows another test was still counting — which is
/// exactly the failure this arrangement replaces.
///
/// The slot name is both the session cookie and the owner id, so a row left
/// behind by a failed run names the test that left it.
const SLOTS: [&str; 9] = [
    "anon",
    "expired",
    "origin",
    "lifecycle",
    "filter",
    "payment",
    "locked",
    "ada",
    "grace",
];

/// Unique per run, so a failed test that skips its cleanup cannot poison the
/// next one.
fn run_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| Uuid::now_v7().to_string())
}

fn cookie(slot: &str) -> String {
    format!("better-auth.session_token={slot}.sig")
}

fn owner(slot: &str) -> String {
    format!("api-{slot}-{}", run_id())
}

/// A cookie no slot claims. Better Auth answers `200 null` for it.
const DEAD_COOKIE: &str = "better-auth.session_token=expired-and-unknown.sig";

fn session_body(id: &str) -> Value {
    json!({
        "session": { "id": "ses_01HX", "userId": id, "expiresAt": "2099-01-01T00:00:00.000Z" },
        "user": { "id": id, "name": "Test Owner", "email": "owner@example.test", "emailVerified": true }
    })
}

/// Starts the stand-in auth service once and points the application at it.
///
/// `BETTER_AUTH_URL` is read by `AuthClient::from_env` inside the handler, so
/// it has to be set before the first request — and being process-wide, it can
/// only be set once. Every test awaits this.
async fn auth() -> &'static MockServer {
    static SERVER: OnceCell<MockServer> = OnceCell::const_new();

    SERVER
        .get_or_init(|| async {
            let server = MockServer::start().await;

            // The catch-all first: wiremock prefers the mock registered
            // earliest among those that match, so the specific cookies have to
            // be mounted after it would be wrong — they are mounted with a
            // stricter matcher and registered later, and wiremock scores by
            // specificity of the match, not by order. Mounting the exact-cookie
            // mocks first removes the question entirely.
            for slot in SLOTS {
                Mock::given(method("GET"))
                    .and(path("/api/auth/get-session"))
                    .and(header_matcher("cookie", cookie(slot).as_str()))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(session_body(&owner(slot))),
                    )
                    .mount(&server)
                    .await;
            }

            // The shape that is easy to guess wrong: an expired session is a
            // `200` carrying `null`, not a `401`.
            Mock::given(method("GET"))
                .and(path("/api/auth/get-session"))
                .respond_with(ResponseTemplate::new(200).set_body_string("null"))
                .mount(&server)
                .await;

            std::env::set_var("BETTER_AUTH_URL", server.uri());
            server
        })
        .await
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is required; run `set -a; . ./.env; set +a` first");

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("could not connect to PostgreSQL");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("could not apply migrations");

    pool
}

/// The application's own route table, on a bare `PgPool` state.
///
/// `main.rs` mounts this same function on `AppState`; the routes, methods and
/// handlers under test are therefore the ones served in production, and only
/// the state type differs.
async fn app() -> Router {
    auth().await;
    Router::new()
        .merge(orders_and_settlements::orders::api::router())
        .with_state(pool().await)
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    body: Value,
}

impl Reply {
    /// The stable code every failure carries.
    fn code(&self) -> String {
        self.body["error"]
            .as_str()
            .unwrap_or("<missing>")
            .to_string()
    }

    /// The sentence a person would be shown.
    fn message(&self) -> String {
        self.body["message"]
            .as_str()
            .unwrap_or("<missing>")
            .to_string()
    }

    fn field_errors(&self) -> Vec<(String, String)> {
        self.body["fields"]
            .as_array()
            .map(|fields| {
                fields
                    .iter()
                    .map(|entry| {
                        (
                            entry["field"].as_str().unwrap_or_default().to_string(),
                            entry["message"].as_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// One request through the router. `cookie` is the session; `body` is JSON when
/// present, and the `Origin` header is set on every state-changing request the
/// way a browser would set it.
async fn call(method: &str, uri: &str, cookie: Option<&str>, body: Option<Value>) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost:5174");

    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }

    if method != "GET" {
        request = request.header(header::ORIGIN, "http://localhost:5174");
    }

    let request = match &body {
        Some(value) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };

    let response = app().await.oneshot(request).await.expect("router answered");

    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body read");

    Reply {
        status,
        location,
        // A `204` has no body, and that is not a failure.
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn order_body(customer: &str, due: &str) -> Value {
    json!({
        "customer": customer,
        "due_date": due,
        "items": [{ "description": "Consulting", "quantity": "2", "unit_price": "500.00" }]
    })
}

async fn cleanup(slot: &str) {
    sqlx::query("DELETE FROM orders WHERE owner_user_id = $1")
        .bind(owner(slot))
        .execute(&pool().await)
        .await
        .expect("cleanup failed");
}

/// Creates an order through the API and returns its id.
async fn create_order(slot: &str, customer: &str, due: &str) -> Uuid {
    let reply = call(
        "POST",
        "/api/orders",
        Some(&cookie(slot)),
        Some(order_body(customer, due)),
    )
    .await;

    assert_eq!(reply.status, StatusCode::CREATED, "body: {}", reply.body);

    Uuid::parse_str(reply.body["id"].as_str().expect("the order carries an id"))
        .expect("a real UUID")
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_endpoint_refuses_an_anonymous_caller() {
    let id = Uuid::now_v7();

    let calls = [
        ("GET", "/api/orders".to_string()),
        ("POST", "/api/orders".to_string()),
        ("GET", format!("/api/orders/{id}")),
        ("PUT", format!("/api/orders/{id}")),
        ("DELETE", format!("/api/orders/{id}")),
        ("POST", format!("/api/orders/{id}/payments")),
    ];

    for (method, uri) in calls {
        let reply = call(method, &uri, None, Some(json!({}))).await;

        // 401, not 500: the handler must not be reaching for a Leptos context
        // that a bare Axum route does not have.
        assert_eq!(
            reply.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} answered {} with {}",
            reply.status,
            reply.body
        );
        assert_eq!(reply.code(), "UNAUTHENTICATED");
    }
}

#[tokio::test]
async fn an_expired_session_is_unauthenticated_rather_than_a_server_error() {
    // Better Auth answers `200 null` for a dead session; that must surface as
    // 401 and not as a parse failure.
    let reply = call("GET", "/api/orders", Some(DEAD_COOKIE), None).await;

    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_cross_origin_write_is_refused_but_a_toolless_one_is_not() {
    let request = Request::builder()
        .method("POST")
        .uri("/api/orders")
        .header(header::HOST, "localhost:5174")
        .header(header::COOKIE, cookie("origin"))
        .header(header::ORIGIN, "https://evil.example")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            order_body("Cross Origin Co", "2027-01-31").to_string(),
        ))
        .unwrap();

    let response = app().await.oneshot(request).await.expect("answered");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // No Origin at all is a script, not a page: a browser cannot omit it on a
    // cross-site state-changing request, so this one is allowed through.
    let request = Request::builder()
        .method("POST")
        .uri("/api/orders")
        .header(header::HOST, "localhost:5174")
        .header(header::COOKIE, cookie("origin"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(order_body("Curl Co", "2027-01-31").to_string()))
        .unwrap();

    let response = app().await.oneshot(request).await.expect("answered");
    assert_eq!(response.status(), StatusCode::CREATED);

    cleanup("origin").await;
}

// ---------------------------------------------------------------------------
// The order lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_order_can_be_created_read_replaced_and_deleted() {
    let created = call(
        "POST",
        "/api/orders",
        Some(&cookie("lifecycle")),
        Some(order_body("Lifecycle Ltd", "2027-03-31")),
    )
    .await;

    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.body["id"].as_str().expect("an id").to_string();
    // The client is told where the thing it made now lives.
    assert_eq!(created.location, Some(format!("/api/orders/{id}")));
    // The server's own totals, not an echo of the strings that were sent.
    assert_eq!(created.body["total_cents"], 100_000);
    assert_eq!(created.body["status"], "pending");
    assert_eq!(created.body["items"][0]["line_total_cents"], 100_000);

    let read = call(
        "GET",
        &format!("/api/orders/{id}"),
        Some(&cookie("lifecycle")),
        None,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body["customer"], "Lifecycle Ltd");
    assert_eq!(read.body["editable"], true);
    assert_eq!(read.body["payments"].as_array().unwrap().len(), 0);

    let replaced = call(
        "PUT",
        &format!("/api/orders/{id}"),
        Some(&cookie("lifecycle")),
        Some(json!({
            "customer": "Lifecycle Ltd",
            "due_date": "2027-04-30",
            "items": [{ "description": "Retainer", "quantity": "1", "unit_price": "250.00" }]
        })),
    )
    .await;

    assert_eq!(replaced.status, StatusCode::OK);
    assert_eq!(replaced.body["total_cents"], 25_000);
    assert_eq!(replaced.body["due_date"], "2027-04-30");
    // A replace, not a patch: the items sent are the items the order has.
    assert_eq!(replaced.body["items"].as_array().unwrap().len(), 1);

    let deleted = call(
        "DELETE",
        &format!("/api/orders/{id}"),
        Some(&cookie("lifecycle")),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    // Deleting it again is a 404. Pretending would not be an answer.
    let again = call(
        "DELETE",
        &format!("/api/orders/{id}"),
        Some(&cookie("lifecycle")),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);

    cleanup("lifecycle").await;
}

#[tokio::test]
async fn the_list_carries_totals_and_honours_the_status_filter() {
    let paid_id = create_order("filter", "Filter Paid Co", "2027-05-31").await;

    call(
        "POST",
        &format!("/api/orders/{paid_id}/payments"),
        Some(&cookie("filter")),
        Some(json!({ "amount": "1000.00", "paid_on": "2026-08-01" })),
    )
    .await;

    create_order("filter", "Filter Pending Co", "2027-05-31").await;

    let all = call("GET", "/api/orders", Some(&cookie("filter")), None).await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(all.body["totals"]["order_count"], 2);
    assert_eq!(all.body["totals"]["outstanding_cents"], 100_000);
    assert_eq!(all.body["filter"], Value::Null);
    assert_eq!(all.body["orders"].as_array().unwrap().len(), 2);

    let paid = call(
        "GET",
        "/api/orders?status=paid",
        Some(&cookie("filter")),
        None,
    )
    .await;
    assert_eq!(paid.body["filter"], "paid");
    assert_eq!(paid.body["orders"].as_array().unwrap().len(), 1);
    assert_eq!(paid.body["orders"][0]["id"], paid_id.to_string());
    // The tiles describe everything, whatever slice is being read.
    assert_eq!(paid.body["totals"]["order_count"], 2);

    // An unrecognised filter is no filter, not a 400 — and the response says
    // which filter was actually applied.
    let nonsense = call(
        "GET",
        "/api/orders?status=settled",
        Some(&cookie("filter")),
        None,
    )
    .await;
    assert_eq!(nonsense.status, StatusCode::OK);
    assert_eq!(nonsense.body["filter"], Value::Null);
    assert_eq!(nonsense.body["orders"].as_array().unwrap().len(), 2);

    cleanup("filter").await;
}

// ---------------------------------------------------------------------------
// Payments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_payment_returns_the_whole_order_and_refuses_to_exceed_the_balance() {
    let id = create_order("payment", "Payment Co", "2027-06-30").await;

    let paid = call(
        "POST",
        &format!("/api/orders/{id}/payments"),
        Some(&cookie("payment")),
        Some(json!({ "amount": "400.00", "paid_on": "2026-08-01" })),
    )
    .await;

    assert_eq!(paid.status, StatusCode::CREATED);
    assert_eq!(paid.location, Some(format!("/api/orders/{id}")));
    // The whole order, so a client never has to compute the new balance.
    assert_eq!(paid.body["paid_cents"], 40_000);
    assert_eq!(paid.body["status"], "partially_paid");
    assert_eq!(paid.body["payments"].as_array().unwrap().len(), 1);
    // Money has moved, so the order is no longer editable.
    assert_eq!(paid.body["editable"], false);

    let too_much = call(
        "POST",
        &format!("/api/orders/{id}/payments"),
        Some(&cookie("payment")),
        Some(json!({ "amount": "700.00", "paid_on": "2026-08-02" })),
    )
    .await;

    // 409, not 400: the request is well formed and the caller is entitled to
    // make it; the order's own state is what refuses.
    assert_eq!(too_much.status, StatusCode::CONFLICT);
    // And the refusal is actionable — it names the most that could be paid.
    assert_eq!(too_much.code(), "PAYMENT_EXCEEDS_AMOUNT_DUE");
    assert!(
        too_much.message().contains("600.00"),
        "expected the maximum in the message, got {:?}",
        too_much.message()
    );

    cleanup("payment").await;
}

#[tokio::test]
async fn an_order_with_payments_cannot_be_edited_or_deleted_through_the_api() {
    let id = create_order("locked", "Locked Co", "2027-07-31").await;

    call(
        "POST",
        &format!("/api/orders/{id}/payments"),
        Some(&cookie("locked")),
        Some(json!({ "amount": "10.00", "paid_on": "2026-08-01" })),
    )
    .await;

    for method in ["PUT", "DELETE"] {
        let reply = call(
            method,
            &format!("/api/orders/{id}"),
            Some(&cookie("locked")),
            Some(order_body("Locked Co", "2027-08-31")),
        )
        .await;

        assert_eq!(
            reply.status,
            StatusCode::CONFLICT,
            "{method} answered {}",
            reply.status
        );
    }

    cleanup("locked").await;
}

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

#[tokio::test]
async fn another_owners_order_is_missing_rather_than_forbidden() {
    let id = create_order("ada", "Ada's Private Co", "2027-09-30").await;

    // 404 and not 403 throughout: a 403 would confirm the order exists, which
    // is the fact the probe is after.
    for (method, uri, body) in [
        ("GET", format!("/api/orders/{id}"), None),
        (
            "PUT",
            format!("/api/orders/{id}"),
            Some(order_body("Taken Over Ltd", "2027-10-31")),
        ),
        ("DELETE", format!("/api/orders/{id}"), None),
        (
            "POST",
            format!("/api/orders/{id}/payments"),
            Some(json!({ "amount": "1.00", "paid_on": "2026-08-01" })),
        ),
    ] {
        let reply = call(method, &uri, Some(&cookie("grace")), body).await;

        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{method} {uri} answered {} with {}",
            reply.status,
            reply.body
        );
    }

    // And Grace's own list never mentions it.
    let list = call("GET", "/api/orders", Some(&cookie("grace")), None).await;
    assert_eq!(list.body["totals"]["order_count"], 0);

    cleanup("ada").await;
}

// ---------------------------------------------------------------------------
// Malformed requests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validation_failures_name_the_field_that_was_wrong() {
    let reply = call(
        "POST",
        "/api/orders",
        Some(&cookie("ada")),
        Some(json!({
            "customer": "",
            "due_date": "31/03/2027",
            "items": [{ "description": "Consulting", "quantity": "0", "unit_price": "-5" }]
        })),
    )
    .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let fields: Vec<String> = reply
        .field_errors()
        .into_iter()
        .map(|(field, _)| field)
        .collect();

    // The same machine-readable paths the browser matches on, so a non-browser
    // client can put the message next to the right input too.
    for expected in ["customer", "due_date", "items[0].quantity"] {
        assert!(
            fields.iter().any(|field| field == expected),
            "expected {expected} among {fields:?}"
        );
    }
}

#[tokio::test]
async fn a_malformed_body_reports_the_same_error_shape_as_everything_else() {
    let request = Request::builder()
        .method("POST")
        .uri("/api/orders")
        .header(header::HOST, "localhost:5174")
        .header(header::ORIGIN, "http://localhost:5174")
        .header(header::COOKIE, cookie("ada"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{ not json"))
        .unwrap();

    let response = app().await.oneshot(request).await.expect("answered");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes)
        .expect("a malformed body must not produce a plain-text error");

    assert_eq!(body["fields"][0]["field"], "body");
}

#[tokio::test]
async fn an_unparseable_id_is_rejected_without_touching_the_database() {
    let reply = call("GET", "/api/orders/not-a-uuid", Some(&cookie("ada")), None).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.field_errors()[0].0, "id");
}

#[tokio::test]
async fn an_unregistered_method_is_not_a_silent_success() {
    // PATCH is not part of this contract. Axum answers 405 because the methods
    // are registered on one route rather than as routes that shadow each other.
    let reply = call(
        "PATCH",
        &format!("/api/orders/{}", Uuid::now_v7()),
        Some(&cookie("ada")),
        Some(json!({})),
    )
    .await;

    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
}
