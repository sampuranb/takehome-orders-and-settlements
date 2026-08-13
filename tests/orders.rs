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
use orders_and_settlements::orders::ssr::{
    create_order_service, delete_order_service, find_order_for_user, insert_order,
    list_orders_for_user, update_order_service,
};
use orders_and_settlements::orders::{
    calculate_line_total_cents, calculate_order_total_cents, derive_order_status, filter_by_status,
    summarise_orders, validate_create_order, NewOrderInput, OrderItemInput, OrderStatus,
    OrderSummary, ValidatedItem, ValidatedOrder, MAX_ITEMS,
};
use orders_and_settlements::payments::ssr::record_payment_service;
use orders_and_settlements::payments::NewPaymentInput;
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
// Derived status
//
// `derive_order_status` takes `today` as a parameter, so every branch is
// reachable without waiting for a date to arrive or stubbing a clock.
// ---------------------------------------------------------------------------

fn day(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("a real calendar date")
}

#[test]
fn an_unpaid_order_before_its_due_date_is_pending() {
    let status = derive_order_status(10_000, 0, day(2026, 9, 30), day(2026, 8, 13));

    assert_eq!(status, OrderStatus::Pending);
}

#[test]
fn a_partly_paid_order_before_its_due_date_is_partially_paid() {
    let status = derive_order_status(10_000, 4_000, day(2026, 9, 30), day(2026, 8, 13));

    assert_eq!(status, OrderStatus::PartiallyPaid);
}

#[test]
fn an_order_paid_in_full_is_paid() {
    let status = derive_order_status(10_000, 10_000, day(2026, 9, 30), day(2026, 8, 13));

    assert_eq!(status, OrderStatus::Paid);
}

#[test]
fn an_order_is_overdue_only_after_its_due_date_has_passed() {
    let due = day(2026, 8, 13);

    // The due date itself is not late. An invoice due today is due today.
    assert_eq!(
        derive_order_status(10_000, 0, due, due),
        OrderStatus::Pending
    );
    assert_eq!(
        derive_order_status(10_000, 0, due, day(2026, 8, 14)),
        OrderStatus::Overdue
    );
}

#[test]
fn paid_beats_overdue() {
    // The precedence that matters: an order settled after its due date is
    // finished, not outstanding. Nobody should be chased for it.
    let status = derive_order_status(10_000, 10_000, day(2026, 1, 1), day(2026, 8, 13));

    assert_eq!(status, OrderStatus::Paid);
}

#[test]
fn overdue_beats_partially_paid() {
    // The other precedence: money is still owed past the date, so the order is
    // overdue even though something was paid against it.
    let status = derive_order_status(10_000, 4_000, day(2026, 1, 1), day(2026, 8, 13));

    assert_eq!(status, OrderStatus::Overdue);
}

#[test]
fn an_overpaid_order_is_paid_not_partially_paid() {
    // Feature 6 refuses overpayment, but the comparison is `>=` rather than
    // `==` so a total that was later reduced cannot leave the order stuck.
    let status = derive_order_status(10_000, 12_000, day(2026, 1, 1), day(2026, 8, 13));

    assert_eq!(status, OrderStatus::Paid);
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

// ---------------------------------------------------------------------------
// Reads, edits, and deletes
//
// Every one of these goes through the same `owner_user_id` filter the server
// functions use. The tests that matter most are the ones where a second owner
// asks for the first owner's order: the answer must be indistinguishable from
// asking for an id that was never written.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lists_only_the_callers_orders_soonest_due_first() {
    let pool = connect().await;
    let owner = test_owner();
    let stranger = test_owner();

    let mut later = order(vec![item("Consulting", "1", "10.00")]);
    later.due_date = "2026-12-01".to_string();
    later.customer = "Later Ltd".to_string();
    let mut sooner = order(vec![
        item("Licence", "2", "5.00"),
        item("Support", "1", "1.00"),
    ]);
    sooner.due_date = "2026-09-01".to_string();
    sooner.customer = "Sooner Ltd".to_string();

    create_order_service(&pool, &owner, &later)
        .await
        .expect("the later order is valid");
    create_order_service(&pool, &owner, &sooner)
        .await
        .expect("the sooner order is valid");
    create_order_service(&pool, &stranger, &order(vec![item("Other", "1", "1.00")]))
        .await
        .expect("the stranger's order is valid");

    let listed = list_orders_for_user(&pool, &owner)
        .await
        .expect("the list is readable");

    assert_eq!(listed.len(), 2, "the stranger's order must not appear");
    assert_eq!(listed[0].customer, "Sooner Ltd");
    assert_eq!(listed[1].customer, "Later Ltd");

    // The item count comes from a join and a group-by, not a follow-up read.
    assert_eq!(listed[0].item_count, 2);
    assert_eq!(listed[1].item_count, 1);

    // Until Feature 6 there are no payments, so everything is still due.
    assert_eq!(listed[0].total_cents, 1_100);
    assert_eq!(listed[0].paid_cents, 0);
    assert_eq!(listed[0].amount_due_cents(), 1_100);

    cleanup(&pool, &owner).await;
    cleanup(&pool, &stranger).await;
}

#[tokio::test]
async fn reads_one_order_with_its_items_in_the_order_they_were_entered() {
    let pool = connect().await;
    let owner = test_owner();

    let input = order(vec![
        item("Consulting", "3", "19.99"),
        item("Licence", "1", "$1,000.00"),
    ]);
    let order_id = create_order_service(&pool, &owner, &input)
        .await
        .expect("the order is valid");

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("the order is readable");

    assert_eq!(detail.id, order_id);
    assert_eq!(detail.customer, "Acme Corp");
    assert_eq!(detail.total_cents, 105_997);
    assert_eq!(detail.amount_due_cents(), 105_997);
    assert!(detail.editable, "an order with no payments can be changed");

    let descriptions: Vec<&str> = detail
        .items
        .iter()
        .map(|line| line.description.as_str())
        .collect();
    assert_eq!(descriptions, vec!["Consulting", "Licence"]);
    assert_eq!(detail.items[0].line_total_cents, 5_997);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn a_stranger_cannot_read_someone_elses_order() {
    let pool = connect().await;
    let owner = test_owner();
    let stranger = test_owner();

    let order_id = create_order_service(&pool, &owner, &order(vec![item("X", "1", "1.00")]))
        .await
        .expect("the order is valid");

    let refused = find_order_for_user(&pool, &stranger, order_id)
        .await
        .expect_err("a stranger must not read it");

    // The same answer an id that was never written gets. A `403` here would
    // confirm to the stranger that this id exists.
    assert_eq!(refused, AppError::NotFound);
    assert_eq!(refused.status_code(), 404);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn an_id_that_was_never_written_is_not_found() {
    let pool = connect().await;
    let owner = test_owner();

    let missing = find_order_for_user(&pool, &owner, Uuid::now_v7())
        .await
        .expect_err("nothing was written under this id");

    assert_eq!(missing, AppError::NotFound);
}

#[tokio::test]
async fn an_edit_replaces_the_items_and_recomputes_the_total() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id = create_order_service(
        &pool,
        &owner,
        &order(vec![
            item("Consulting", "3", "19.99"),
            item("Licence", "1", "1000.00"),
        ]),
    )
    .await
    .expect("the order is valid");

    let mut edited = order(vec![item("Support", "2", "25.00")]);
    edited.customer = "Acme Holdings".to_string();
    edited.due_date = "2026-10-15".to_string();

    update_order_service(&pool, &owner, order_id, &edited)
        .await
        .expect("the edit is valid");

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("the order is still readable");

    assert_eq!(detail.customer, "Acme Holdings");
    assert_eq!(
        detail.due_date,
        NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()
    );
    // Recomputed from the submitted strings, not adjusted from the old total.
    assert_eq!(detail.total_cents, 5_000);
    assert_eq!(detail.items.len(), 1, "the old items are gone, not merged");
    assert_eq!(detail.items[0].description, "Support");

    // The replaced rows were deleted rather than left orphaned, and the new one
    // starts at position 0 again.
    let positions: Vec<i32> = sqlx::query_scalar(
        "SELECT position FROM order_items WHERE order_id = $1 ORDER BY position",
    )
    .bind(order_id)
    .fetch_all(&pool)
    .await
    .expect("the items are readable");
    assert_eq!(positions, vec![0]);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn an_invalid_edit_leaves_the_stored_order_untouched() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id = create_order_service(
        &pool,
        &owner,
        &order(vec![item("Consulting", "3", "19.99")]),
    )
    .await
    .expect("the order is valid");

    let refused = update_order_service(&pool, &owner, order_id, &order(vec![]))
        .await
        .expect_err("an order with no items is not valid");
    assert_eq!(failed_fields(&refused), vec!["items"]);

    // Validation runs before the transaction opens, so there is nothing to roll
    // back — but the point of the test is what the caller can still read.
    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("the order survived");
    assert_eq!(detail.total_cents, 5_997);
    assert_eq!(detail.items.len(), 1);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn a_stranger_cannot_edit_or_delete_someone_elses_order() {
    let pool = connect().await;
    let owner = test_owner();
    let stranger = test_owner();

    let order_id = create_order_service(&pool, &owner, &order(vec![item("X", "1", "1.00")]))
        .await
        .expect("the order is valid");

    let edit = update_order_service(
        &pool,
        &stranger,
        order_id,
        &order(vec![item("Hijacked", "1", "0.01")]),
    )
    .await
    .expect_err("a stranger must not edit it");
    assert_eq!(edit, AppError::NotFound);

    let delete = delete_order_service(&pool, &stranger, order_id)
        .await
        .expect_err("a stranger must not delete it");
    assert_eq!(delete, AppError::NotFound);

    // Neither attempt changed anything.
    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("the owner's order is intact");
    assert_eq!(detail.items[0].description, "X");

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn deleting_an_order_takes_its_line_items_with_it() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id = create_order_service(
        &pool,
        &owner,
        &order(vec![item("A", "1", "1.00"), item("B", "2", "2.00")]),
    )
    .await
    .expect("the order is valid");

    delete_order_service(&pool, &owner, order_id)
        .await
        .expect("the owner may delete it");

    assert_eq!(
        find_order_for_user(&pool, &owner, order_id)
            .await
            .expect_err("it is gone"),
        AppError::NotFound
    );

    let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM order_items WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&pool)
        .await
        .expect("the count is readable");
    assert_eq!(orphans, 0, "ON DELETE CASCADE removed the line items");

    // A second delete is not an error the caller can distinguish from deleting
    // a stranger's order, and must not be.
    assert_eq!(
        delete_order_service(&pool, &owner, order_id)
            .await
            .expect_err("it is already gone"),
        AppError::NotFound
    );

    cleanup(&pool, &owner).await;
}

// ---------------------------------------------------------------------------
// The complete detail view
//
// One read has to produce everything the page shows: the items in the order
// they were saved, every payment newest first, and totals that reconcile with
// the payments listed beneath them. These assert the read, not the rendering —
// a component can only be as correct as the DTO it is handed.
// ---------------------------------------------------------------------------

fn payment(amount: &str, paid_on: &str) -> NewPaymentInput {
    NewPaymentInput {
        amount: amount.to_string(),
        paid_on: paid_on.to_string(),
    }
}

#[tokio::test]
async fn line_items_come_back_in_the_order_they_were_saved() {
    let pool = connect().await;
    let owner = test_owner();

    // Descriptions deliberately not in alphabetical order, and prices
    // deliberately not ascending, so a query that happened to sort by either
    // would produce a different sequence than this one.
    let order_id = create_order_service(
        &pool,
        &owner,
        &order(vec![
            item("Zebra", "1", "5.00"),
            item("Apple", "1", "1.00"),
            item("Mango", "1", "9.00"),
        ]),
    )
    .await
    .expect("the order is valid");

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");

    let descriptions: Vec<&str> = detail
        .items
        .iter()
        .map(|line| line.description.as_str())
        .collect();
    assert_eq!(descriptions, vec!["Zebra", "Apple", "Mango"]);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn the_payment_history_is_newest_first() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id = create_order_service(&pool, &owner, &order(vec![item("Work", "1", "300.00")]))
        .await
        .expect("the order is valid");

    // Recorded out of date order, so "newest first" cannot be satisfied by
    // accident by insertion order.
    for (amount, paid_on) in [
        ("100.00", "2026-02-01"),
        ("50.00", "2026-04-01"),
        ("25.00", "2026-03-01"),
    ] {
        record_payment_service(&pool, &owner, order_id, &payment(amount, paid_on))
            .await
            .expect("within the balance");
    }

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");

    let dates: Vec<String> = detail
        .payments
        .iter()
        .map(|paid| paid.paid_on.to_string())
        .collect();
    assert_eq!(dates, vec!["2026-04-01", "2026-03-01", "2026-02-01"]);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn payments_on_the_same_day_keep_one_stable_order() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id = create_order_service(&pool, &owner, &order(vec![item("Work", "1", "300.00")]))
        .await
        .expect("the order is valid");

    // Three payments with nothing to tell them apart but the id. Without the
    // tiebreak the database is free to return them in any order, and the
    // history would shuffle between two reads of the same page.
    for amount in ["10.00", "20.00", "30.00"] {
        record_payment_service(&pool, &owner, order_id, &payment(amount, "2026-05-05"))
            .await
            .expect("within the balance");
    }

    let first = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    let second = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable again");

    assert_eq!(
        first.payments, second.payments,
        "the order is deterministic"
    );

    // Ids are v7, so descending id is "recorded later first" — which for
    // same-day payments is the only sequence that means anything.
    let amounts: Vec<i64> = first
        .payments
        .iter()
        .map(|paid| paid.amount_cents)
        .collect();
    assert_eq!(amounts, vec![3_000, 2_000, 1_000]);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn the_history_adds_up_to_the_totals_printed_above_it() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id = create_order_service(
        &pool,
        &owner,
        &order(vec![item("Consulting", "2", "500.00")]),
    )
    .await
    .expect("the order is valid");

    for amount in ["400.00", "150.50", "49.50"] {
        record_payment_service(&pool, &owner, order_id, &payment(amount, "2026-06-01"))
            .await
            .expect("within the balance");
    }

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");

    // The whole reason the payments and the totals travel on one DTO: a page
    // that fetched them separately could print a list that does not sum to the
    // figure above it.
    let listed: i64 = detail.payments.iter().map(|paid| paid.amount_cents).sum();
    assert_eq!(listed, detail.paid_cents, "the list sums to what was paid");
    assert_eq!(
        detail.paid_cents + detail.amount_due_cents(),
        detail.total_cents,
        "paid plus due is the total"
    );
    assert_eq!(detail.paid_cents, 60_000);
    assert_eq!(detail.amount_due_cents(), 40_000);
    assert_eq!(detail.status, OrderStatus::PartiallyPaid);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn an_unpaid_order_has_an_empty_history_and_every_action() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id = create_order_service(&pool, &owner, &order(vec![item("Work", "1", "10.00")]))
        .await
        .expect("the order is valid");

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");

    assert!(detail.payments.is_empty(), "nothing has been paid");
    assert_eq!(detail.paid_cents, 0);
    // Both conditional actions are live: the order can still be changed, and
    // there is a balance to pay.
    assert!(detail.editable);
    assert_eq!(detail.amount_due_cents(), 1_000);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn a_settled_order_offers_no_payment_and_no_edit() {
    let pool = connect().await;
    let owner = test_owner();

    let order_id = create_order_service(&pool, &owner, &order(vec![item("Work", "1", "80.00")]))
        .await
        .expect("the order is valid");

    record_payment_service(&pool, &owner, order_id, &payment("80.00", "2026-07-07"))
        .await
        .expect("settles it");

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");

    assert_eq!(detail.status, OrderStatus::Paid);
    // Nothing left to pay, so the form has nothing to offer...
    assert_eq!(detail.amount_due_cents(), 0);
    // ...and money has moved, so the record is no longer the caller's to edit.
    assert!(!detail.editable);
    assert_eq!(detail.payments.len(), 1);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn a_stranger_cannot_read_the_payment_history() {
    let pool = connect().await;
    let owner = test_owner();
    let stranger = test_owner();

    let order_id = create_order_service(&pool, &owner, &order(vec![item("Work", "1", "10.00")]))
        .await
        .expect("the order is valid");

    record_payment_service(&pool, &owner, order_id, &payment("4.00", "2026-07-07"))
        .await
        .expect("within the balance");

    // The payment query itself is not owner-scoped — it is reached only through
    // this read, which is. The ownership check has to hold here or it holds
    // nowhere.
    assert_eq!(
        find_order_for_user(&pool, &stranger, order_id)
            .await
            .expect_err("not the stranger's order"),
        AppError::NotFound
    );

    cleanup(&pool, &owner).await;
}

// ---------------------------------------------------------------------------
// Dashboard: filter parsing, totals, and the status filter
// ---------------------------------------------------------------------------

/// A summary built by hand, so the totals can be tested without a database.
fn summary(status: OrderStatus, total: i64, paid: i64) -> OrderSummary {
    OrderSummary {
        id: Uuid::now_v7(),
        customer: "Acme Corp".to_string(),
        due_date: NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
        total_cents: total,
        paid_cents: paid,
        item_count: 1,
        status,
    }
}

#[test]
fn every_status_survives_a_round_trip_through_the_url() {
    for status in OrderStatus::ALL {
        assert_eq!(
            OrderStatus::parse(status.as_str()),
            Some(status),
            "?status={} must parse back to the status that wrote it",
            status.as_str()
        );
    }
}

#[test]
fn an_unrecognised_filter_is_no_filter_rather_than_an_error() {
    // A URL is typed, pasted and truncated by people. Every one of these has to
    // land on the dashboard showing everything, not on an error page.
    for raw in ["", "paid ", "PAID", "pad", "partially-paid", "settled", "1"] {
        assert_eq!(
            OrderStatus::parse(raw),
            None,
            "{raw:?} is not a status and must not be treated as one"
        );
    }
}

#[test]
fn the_totals_of_nothing_are_zero() {
    let totals = summarise_orders(&[]);

    assert_eq!(totals.order_count, 0);
    assert_eq!(totals.total_cents, 0);
    assert_eq!(totals.paid_cents, 0);
    assert_eq!(totals.outstanding_cents, 0);
    for status in OrderStatus::ALL {
        assert_eq!(totals.count_of(status), 0);
    }
}

#[test]
fn the_tiles_count_every_status_and_add_up_the_money() {
    let orders = vec![
        summary(OrderStatus::Pending, 10_000, 0),
        summary(OrderStatus::PartiallyPaid, 20_000, 5_000),
        summary(OrderStatus::Paid, 30_000, 30_000),
        summary(OrderStatus::Overdue, 40_000, 0),
        summary(OrderStatus::Overdue, 1_000, 250),
    ];

    let totals = summarise_orders(&orders);

    assert_eq!(totals.order_count, 5);
    assert_eq!(totals.count_of(OrderStatus::Pending), 1);
    assert_eq!(totals.count_of(OrderStatus::PartiallyPaid), 1);
    assert_eq!(totals.count_of(OrderStatus::Paid), 1);
    assert_eq!(totals.count_of(OrderStatus::Overdue), 2);

    assert_eq!(totals.total_cents, 101_000);
    assert_eq!(totals.paid_cents, 35_250);
    // 10_000 + 15_000 + 0 + 40_000 + 750.
    assert_eq!(totals.outstanding_cents, 65_750);
}

#[test]
fn outstanding_is_the_sum_of_what_each_order_owes() {
    // An overpaid order owes nothing. It must not lend its surplus to the order
    // beside it, which is what both `total_cents - paid_cents` across the whole
    // set and a plain sum of `amount_due_cents()` would do: the £5 credit on the
    // settled invoice would pay down £5 of the pending one.
    let orders = vec![
        summary(OrderStatus::Paid, 5_000, 5_500),
        summary(OrderStatus::Pending, 10_000, 0),
    ];

    let totals = summarise_orders(&orders);

    // £100.00 is owed, on the pending order, by itself.
    assert_eq!(totals.outstanding_cents, 10_000);
    // And the two shortcuts really do disagree with that — 15_000 - 5_500
    // reports £95.00 outstanding. That disagreement is the whole test.
    assert_eq!(totals.total_cents - totals.paid_cents, 9_500);
}

#[test]
fn no_filter_shows_every_order() {
    let orders = vec![
        summary(OrderStatus::Pending, 100, 0),
        summary(OrderStatus::Paid, 100, 100),
    ];

    assert_eq!(filter_by_status(orders.clone(), None), orders);
}

#[test]
fn a_filter_keeps_only_its_own_status() {
    let orders = vec![
        summary(OrderStatus::Pending, 100, 0),
        summary(OrderStatus::Paid, 100, 100),
        summary(OrderStatus::Pending, 200, 0),
    ];

    let pending = filter_by_status(orders.clone(), Some(OrderStatus::Pending));
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|o| o.status == OrderStatus::Pending));

    // A status nobody is in is an empty table, not an error and not everything.
    assert!(filter_by_status(orders, Some(OrderStatus::Overdue)).is_empty());
}

#[tokio::test]
async fn the_dashboard_reports_one_owner_and_reaches_every_status() {
    let pool = connect().await;
    let owner = test_owner();
    let stranger = test_owner();

    let today = chrono::Utc::now().date_naive();
    let past = (today - chrono::Duration::days(10)).to_string();
    let future = (today + chrono::Duration::days(10)).to_string();

    let mut pending = order(vec![item("Pending work", "1", "100.00")]);
    pending.due_date = future.clone();
    let pending_id = create_order_service(&pool, &owner, &pending)
        .await
        .expect("valid");

    let mut partial = order(vec![item("Partial work", "1", "200.00")]);
    partial.due_date = future.clone();
    let partial_id = create_order_service(&pool, &owner, &partial)
        .await
        .expect("valid");
    record_payment_service(&pool, &owner, partial_id, &payment("50.00", &past))
        .await
        .expect("within the balance");

    // Due in the past *and* settled: the precedence in `derive_order_status`
    // says paid wins, and the dashboard has to agree — an invoice nobody owes
    // anything on is not overdue, however old it is.
    let mut settled = order(vec![item("Settled work", "1", "300.00")]);
    settled.due_date = past.clone();
    let settled_id = create_order_service(&pool, &owner, &settled)
        .await
        .expect("valid");
    record_payment_service(&pool, &owner, settled_id, &payment("300.00", &past))
        .await
        .expect("settles it");

    let mut overdue = order(vec![item("Late work", "1", "400.00")]);
    overdue.due_date = past.clone();
    let overdue_id = create_order_service(&pool, &owner, &overdue)
        .await
        .expect("valid");

    // Somebody else's order, in a status the owner also has. It must not appear
    // in the owner's list, and must not be counted in the owner's tiles.
    let mut theirs = order(vec![item("Not yours", "1", "999.00")]);
    theirs.due_date = future;
    create_order_service(&pool, &stranger, &theirs)
        .await
        .expect("valid");

    let all = list_orders_for_user(&pool, &owner).await.expect("readable");
    let totals = summarise_orders(&all);

    assert_eq!(totals.order_count, 4, "the stranger's order is not counted");
    assert_eq!(totals.total_cents, 100_000);
    assert_eq!(totals.paid_cents, 35_000);
    assert_eq!(totals.outstanding_cents, 65_000);

    for status in OrderStatus::ALL {
        assert_eq!(
            totals.count_of(status),
            1,
            "expected exactly one {} order",
            status.as_str()
        );
    }

    let ids = |status| -> Vec<Uuid> {
        filter_by_status(all.clone(), Some(status))
            .into_iter()
            .map(|order| order.id)
            .collect()
    };

    assert_eq!(ids(OrderStatus::Pending), vec![pending_id]);
    assert_eq!(ids(OrderStatus::PartiallyPaid), vec![partial_id]);
    assert_eq!(ids(OrderStatus::Paid), vec![settled_id]);
    assert_eq!(ids(OrderStatus::Overdue), vec![overdue_id]);

    // The paid order is due in the past and is still not in the overdue list.
    assert!(!ids(OrderStatus::Overdue).contains(&settled_id));

    cleanup(&pool, &owner).await;
    cleanup(&pool, &stranger).await;
}

#[tokio::test]
async fn a_new_owner_sees_an_empty_dashboard_rather_than_everyone_elses() {
    let pool = connect().await;
    let owner = test_owner();
    let newcomer = test_owner();

    create_order_service(&pool, &owner, &order(vec![item("Work", "1", "10.00")]))
        .await
        .expect("valid");

    let all = list_orders_for_user(&pool, &newcomer)
        .await
        .expect("readable");
    let totals = summarise_orders(&all);

    assert!(all.is_empty());
    assert_eq!(totals.order_count, 0);
    assert_eq!(totals.outstanding_cents, 0);
    // No orders at all, so every filter is empty too — the page says "no orders
    // yet", not "no paid orders".
    assert!(filter_by_status(all, Some(OrderStatus::Paid)).is_empty());

    cleanup(&pool, &owner).await;
}
