//! Order creation: validation, money arithmetic, and the write transaction.
//!
//! Gated on `ssr` because half of these tests talk to PostgreSQL. The other
//! half exercise code that is compiled for the browser too, and they run in the
//! same file so a change to a parser is checked against both its callers at
//! once.
//!
//! The database tests are not mocked. The invariants under test — the
//! `CHECK` constraints, the cascade, and the all-or-nothing write — belong to
//! PostgreSQL, and a fake would only re-assert what this test file already
//! believes. They need `DATABASE_URL` set; `compose.yaml` provides the server
//! and `.env.example` the URL:
//!
//! ```text
//! set -a; . ./.env; set +a; cargo test --features ssr --no-default-features
//! ```
//!
//! Every test writes under an owner id no other test uses, so they are safe to
//! run in parallel and each cleans up only its own rows.

#![cfg(feature = "ssr")]

use chrono::NaiveDate;
use leptos::server_fn::error::FromServerFnError;
use orders_and_settlements::error::AppError;
use orders_and_settlements::orders::ssr::{create_order_service, insert_order};
use orders_and_settlements::orders::{
    calculate_line_total_cents, calculate_order_total_cents, validate_create_order, NewOrderInput,
    OrderItemInput, ValidatedItem, ValidatedOrder, MAX_ITEMS,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn item(description: &str, quantity: &str, unit_price: &str) -> OrderItemInput {
    OrderItemInput {
        description: description.to_string(),
        quantity: quantity.to_string(),
        unit_price: unit_price.to_string(),
    }
}

fn order(items: Vec<OrderItemInput>) -> NewOrderInput {
    NewOrderInput {
        customer: "Acme Corp".to_string(),
        due_date: "2026-09-30".to_string(),
        items,
    }
}

/// The fields a validation failure named, sorted so assertions do not depend on
/// the order the validator happened to visit them in.
fn failed_fields(error: &AppError) -> Vec<String> {
    let mut fields: Vec<String> = error
        .field_errors()
        .iter()
        .map(|failure| failure.field.clone())
        .collect();
    fields.sort();
    fields
}

/// An owner id unique to one test. Better Auth ids are opaque strings, so a
/// UUID is a valid one; nothing in this schema constrains the format.
fn test_owner() -> String {
    format!("test-owner-{}", Uuid::now_v7())
}

/// Connects and applies migrations, so a checkout with an empty database is
/// enough to run this file.
async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL is required: these tests exercise real PostgreSQL constraints. \
         Start the database with `docker compose up -d db` and run \
         `set -a; . ./.env; set +a` first.",
    );

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

async fn cleanup(pool: &PgPool, owner: &str) {
    // Line items go with the order through ON DELETE CASCADE.
    sqlx::query("DELETE FROM orders WHERE owner_user_id = $1")
        .bind(owner)
        .execute(pool)
        .await
        .expect("cleanup failed");
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn accepts_a_complete_order_and_computes_its_totals() {
    let input = order(vec![
        item("Consulting", "3", "19.99"),
        item("Licence", "1", "$1,000.00"),
    ]);

    let validated = validate_create_order(&input).expect("the order is valid");

    assert_eq!(validated.customer, "Acme Corp");
    assert_eq!(
        validated.due_date,
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
    );
    assert_eq!(validated.items[0].line_total_cents, 5_997);
    assert_eq!(validated.items[1].line_total_cents, 100_000);
    assert_eq!(validated.total_cents, 105_997);
}

#[test]
fn trims_surrounding_whitespace() {
    let mut input = order(vec![item("  Consulting  ", " 2 ", " 10.00 ")]);
    input.customer = "  Acme Corp  ".to_string();

    let validated = validate_create_order(&input).expect("the order is valid");

    assert_eq!(validated.customer, "Acme Corp");
    assert_eq!(validated.items[0].description, "Consulting");
    assert_eq!(validated.items[0].quantity, 2);
    assert_eq!(validated.total_cents, 2_000);
}

#[test]
fn accepts_a_due_date_in_the_past() {
    // Recording an already-due invoice is normal. "Overdue" is derived at read
    // time from this date, not refused at creation.
    let mut input = order(vec![item("Consulting", "1", "1.00")]);
    input.due_date = "2020-01-01".to_string();

    let validated = validate_create_order(&input).expect("a past due date is acceptable");

    assert_eq!(
        validated.due_date,
        NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()
    );
}

#[test]
fn reports_every_problem_in_one_response() {
    let input = NewOrderInput {
        customer: "   ".to_string(),
        due_date: "30/09/2026".to_string(),
        items: vec![item("", "0", "abc"), item("Fine", "2", "5.00")],
    };

    let error = validate_create_order(&input).expect_err("the order is not valid");

    assert_eq!(
        failed_fields(&error),
        vec![
            "customer",
            "due_date",
            "items[0].description",
            "items[0].quantity",
            "items[0].unit_price",
        ]
    );
    // The valid second row is not mentioned.
    assert!(error.message_for("items[1].quantity").is_none());
}

#[test]
fn an_order_needs_at_least_one_line_item() {
    let error = validate_create_order(&order(vec![])).expect_err("an empty order is not valid");

    assert_eq!(failed_fields(&error), vec!["items"]);
}

#[test]
fn an_order_is_capped_at_a_hundred_line_items() {
    let items = vec![item("Consulting", "1", "1.00"); MAX_ITEMS + 1];

    let error = validate_create_order(&order(items)).expect_err("101 items is too many");

    assert_eq!(
        error.message_for("items").unwrap(),
        format!("An order can hold at most {MAX_ITEMS} line items.")
    );
}

#[test]
fn a_validation_failure_is_a_400_not_a_500() {
    let error = validate_create_order(&order(vec![])).expect_err("an empty order is not valid");

    assert_eq!(error.status_code(), 400);
    assert_eq!(error.code(), "VALIDATION_FAILED");
}

#[test]
fn a_validation_failure_survives_the_wire() {
    let error =
        validate_create_order(&order(vec![item("", "0", "")])).expect_err("the order is not valid");

    // The browser decodes the same bytes the server encoded; per-field messages
    // are useless if they do not make the trip.
    let round_tripped = AppError::de(error.ser());

    assert_eq!(round_tripped, error);
    assert_eq!(
        round_tripped.message_for("items[0].description").unwrap(),
        "Describe this line item."
    );
}

// ---------------------------------------------------------------------------
// Money arithmetic
// ---------------------------------------------------------------------------

#[test]
fn multiplies_and_sums_in_cents() {
    assert_eq!(calculate_line_total_cents(3, 1_999), Ok(5_997));
    assert_eq!(calculate_line_total_cents(0, 1_999), Ok(0));

    let items = vec![
        ValidatedItem {
            description: "a".to_string(),
            quantity: 3,
            unit_price_cents: 1_999,
            line_total_cents: 5_997,
        },
        ValidatedItem {
            description: "b".to_string(),
            quantity: 1,
            unit_price_cents: 100_000,
            line_total_cents: 100_000,
        },
    ];

    assert_eq!(calculate_order_total_cents(&items), Ok(105_997));
    assert_eq!(calculate_order_total_cents(&[]), Ok(0));
}

#[test]
fn a_line_that_overflows_blames_that_line() {
    // $92,233,720,368,547,758.07 is i64::MAX cents exactly; doubling it is not
    // representable.
    let input = order(vec![item("Consulting", "2", "92233720368547758.07")]);

    let error = validate_create_order(&input).expect_err("the line total overflows");

    assert_eq!(failed_fields(&error), vec!["items[0].unit_price"]);
}

#[test]
fn a_total_that_overflows_is_not_a_field_error() {
    // Each line is representable; their sum is not. No single input is at
    // fault, so this is the order-wide error rather than a field message.
    let input = order(vec![
        item("Consulting", "1", "92233720368547758.07"),
        item("Licence", "1", "0.01"),
    ]);

    let error = validate_create_order(&input).expect_err("the order total overflows");

    assert_eq!(error, AppError::AmountOutOfRange);
    assert_eq!(error.status_code(), 400);
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stores_an_order_with_its_line_items_in_order() {
    let pool = connect().await;
    let owner = test_owner();

    let input = order(vec![
        item("Consulting", "3", "19.99"),
        item("Licence", "1", "1000.00"),
        item("Support", "12", "5.00"),
    ]);

    let order_id = create_order_service(&pool, &owner, &input)
        .await
        .expect("the order is valid");

    let stored = sqlx::query(
        "SELECT owner_user_id, customer, due_date, total_cents FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("the order was written");

    assert_eq!(stored.get::<String, _>("owner_user_id"), owner);
    assert_eq!(stored.get::<String, _>("customer"), "Acme Corp");
    assert_eq!(
        stored.get::<NaiveDate, _>("due_date"),
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
    );
    // 5997 + 100000 + 6000. Recomputed by the server, never sent by the client.
    assert_eq!(stored.get::<i64, _>("total_cents"), 111_997);

    let items = sqlx::query(
        "SELECT position, description, quantity, unit_price_cents, line_total_cents \
         FROM order_items WHERE order_id = $1 ORDER BY position",
    )
    .bind(order_id)
    .fetch_all(&pool)
    .await
    .expect("the items were written");

    assert_eq!(items.len(), 3);
    assert_eq!(items[0].get::<i32, _>("position"), 0);
    assert_eq!(items[0].get::<String, _>("description"), "Consulting");
    assert_eq!(items[0].get::<i64, _>("quantity"), 3);
    assert_eq!(items[0].get::<i64, _>("unit_price_cents"), 1_999);
    assert_eq!(items[0].get::<i64, _>("line_total_cents"), 5_997);
    assert_eq!(items[2].get::<i32, _>("position"), 2);
    assert_eq!(items[2].get::<String, _>("description"), "Support");
    assert_eq!(items[2].get::<i64, _>("line_total_cents"), 6_000);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn an_invalid_order_never_reaches_the_database() {
    let pool = connect().await;
    let owner = test_owner();

    let error = create_order_service(&pool, &owner, &order(vec![item("", "0", "")]))
        .await
        .expect_err("the order is not valid");
    assert_eq!(error.code(), "VALIDATION_FAILED");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orders WHERE owner_user_id = $1")
        .bind(&owner)
        .fetch_one(&pool)
        .await
        .expect("the count query runs");

    assert_eq!(count, 0);
}

#[tokio::test]
async fn a_rejected_line_item_rolls_back_the_whole_order() {
    let pool = connect().await;
    let owner = test_owner();

    // Hand-built, bypassing the validator: `line_total_cents` disagrees with
    // `quantity * unit_price_cents`, which the CHECK constraint refuses. The
    // order row is inserted first, so this proves the rollback rather than the
    // ordering of the statements.
    let corrupt = ValidatedOrder {
        customer: "Acme Corp".to_string(),
        due_date: NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
        total_cents: 5_997,
        items: vec![ValidatedItem {
            description: "Consulting".to_string(),
            quantity: 3,
            unit_price_cents: 1_999,
            line_total_cents: 1,
        }],
    };

    let error = insert_order(&pool, &owner, &corrupt)
        .await
        .expect_err("the CHECK constraint rejects the line item");
    assert_eq!(error, AppError::Internal);

    let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders WHERE owner_user_id = $1")
        .bind(&owner)
        .fetch_one(&pool)
        .await
        .expect("the count query runs");

    assert_eq!(orders, 0, "the order row must not survive its items");
}

#[tokio::test]
async fn deleting_an_order_takes_its_line_items_with_it() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id =
        create_order_service(&pool, &owner, &order(vec![item("Consulting", "1", "1.00")]))
            .await
            .expect("the order is valid");

    sqlx::query("DELETE FROM orders WHERE id = $1")
        .bind(order_id)
        .execute(&pool)
        .await
        .expect("the order is deleted");

    let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM order_items WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&pool)
        .await
        .expect("the count query runs");

    assert_eq!(orphans, 0);
}

#[tokio::test]
async fn two_owners_writing_at_once_stay_separate() {
    let pool = connect().await;
    let first = test_owner();
    let second = test_owner();

    let consulting = order(vec![item("Consulting", "1", "1.00")]);
    let licence = order(vec![item("Licence", "2", "2.00")]);

    let (left, right) = tokio::join!(
        create_order_service(&pool, &first, &consulting),
        create_order_service(&pool, &second, &licence),
    );

    let left = left.expect("the first order is valid");
    let right = right.expect("the second order is valid");
    assert_ne!(left, right);

    let owner: String = sqlx::query_scalar("SELECT owner_user_id FROM orders WHERE id = $1")
        .bind(left)
        .fetch_one(&pool)
        .await
        .expect("the first order was written");
    assert_eq!(owner, first);

    cleanup(&pool, &first).await;
    cleanup(&pool, &second).await;
}
