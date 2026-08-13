//! Payments: validation, the overpayment rule, and the row lock that makes it
//! hold under concurrency.
//!
//! The concurrency test at the bottom is the reason this file exists. Every
//! other assertion here would also pass against an implementation that checks
//! the balance outside a transaction — the race only appears when two payments
//! arrive at once, which is exactly the case a single-threaded test never
//! produces. It needs `DATABASE_URL` set:
//!
//! ```text
//! set -a; . ./.env; set +a; cargo test --features ssr --no-default-features
//! ```
//!
//! Every test writes under an owner id no other test uses, so they are safe to
//! run in parallel and each cleans up only its own rows.

#![cfg(feature = "ssr")]

use chrono::NaiveDate;
use orders_and_settlements::error::AppError;
use orders_and_settlements::orders::ssr::{
    create_order_service, delete_order_service, find_order_for_user, update_order_service,
};
use orders_and_settlements::orders::{NewOrderInput, OrderItemInput, OrderStatus};
use orders_and_settlements::payments::ssr::{list_payments_for_order, record_payment_service};
use orders_and_settlements::payments::{validate_payment, NewPaymentInput};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// An order for $1,000.00, due in the future so nothing is overdue by accident.
fn thousand_dollar_order() -> NewOrderInput {
    NewOrderInput {
        customer: "Acme Corp".to_string(),
        due_date: "2026-12-31".to_string(),
        items: vec![OrderItemInput {
            description: "Consulting".to_string(),
            quantity: "2".to_string(),
            unit_price: "500.00".to_string(),
        }],
    }
}

fn payment(amount: &str) -> NewPaymentInput {
    NewPaymentInput {
        amount: amount.to_string(),
        paid_on: "2026-08-13".to_string(),
    }
}

fn test_owner() -> String {
    format!("test-owner-{}", Uuid::now_v7())
}

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL is required: these tests exercise real PostgreSQL locking. \
         Start the database with `docker compose up -d db` and run \
         `set -a; . ./.env; set +a` first.",
    );

    let pool = PgPoolOptions::new()
        .max_connections(8)
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
    // Payments and line items go with the order through ON DELETE CASCADE.
    sqlx::query("DELETE FROM orders WHERE owner_user_id = $1")
        .bind(owner)
        .execute(pool)
        .await
        .expect("cleanup failed");
}

/// Creates a $1,000.00 order and returns its id.
async fn order_owing_a_thousand(pool: &PgPool, owner: &str) -> Uuid {
    create_order_service(pool, owner, &thousand_dollar_order())
        .await
        .expect("the order is valid")
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn a_payment_of_nothing_is_not_a_payment() {
    let error = validate_payment(&payment("0.00")).expect_err("zero is refused");

    assert_eq!(error.status_code(), 400);
    assert_eq!(error.code(), "VALIDATION_FAILED");
    assert_eq!(
        error.message_for("amount").unwrap(),
        "Enter an amount greater than zero."
    );
}

#[test]
fn a_negative_payment_is_refused_before_the_database_sees_it() {
    // The CHECK constraint would also catch it, but as an opaque internal
    // error. The user gets a sentence instead.
    let error = validate_payment(&payment("-50.00")).expect_err("negative is refused");

    assert!(error.message_for("amount").is_some());
}

#[test]
fn a_payment_carries_the_day_the_money_moved() {
    let validated = validate_payment(&payment("$1,234.50")).expect("valid");

    assert_eq!(validated.amount_cents, 123_450);
    assert_eq!(
        validated.paid_on,
        NaiveDate::from_ymd_opt(2026, 8, 13).unwrap()
    );
}

// ---------------------------------------------------------------------------
// The lifecycle from the assignment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_partial_payment_then_the_balance_settles_the_order() {
    let pool = connect().await;
    let owner = test_owner();
    let order_id = order_owing_a_thousand(&pool, &owner).await;

    // Nothing paid: pending, everything due.
    let before = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    assert_eq!(before.status, OrderStatus::Pending);
    assert_eq!(before.paid_cents, 0);
    assert_eq!(before.amount_due_cents(), 100_000);
    assert!(before.editable);

    record_payment_service(&pool, &owner, order_id, &payment("400.00"))
        .await
        .expect("$400 of $1,000 is acceptable");

    let partly = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    assert_eq!(partly.status, OrderStatus::PartiallyPaid);
    assert_eq!(partly.paid_cents, 40_000);
    assert_eq!(partly.amount_due_cents(), 60_000);
    // An order with money against it is frozen; the amount paid was agreed
    // against these line items.
    assert!(!partly.editable);

    record_payment_service(&pool, &owner, order_id, &payment("600.00"))
        .await
        .expect("the exact balance is acceptable");

    let settled = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    assert_eq!(settled.status, OrderStatus::Paid);
    assert_eq!(settled.paid_cents, 100_000);
    assert_eq!(settled.amount_due_cents(), 0);

    // One cent more is refused, and says so with the real remaining balance.
    let refused = record_payment_service(&pool, &owner, order_id, &payment("0.01"))
        .await
        .expect_err("a settled order takes no more money");
    assert_eq!(
        refused,
        AppError::PaymentExceedsAmountDue { maximum_cents: 0 }
    );
    assert_eq!(refused.status_code(), 409);
    assert_eq!(refused.code(), "PAYMENT_EXCEEDS_AMOUNT_DUE");
    assert_eq!(refused.to_string(), "This order is already paid in full.");

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn a_payment_larger_than_the_balance_names_the_balance() {
    let pool = connect().await;
    let owner = test_owner();
    let order_id = order_owing_a_thousand(&pool, &owner).await;

    record_payment_service(&pool, &owner, order_id, &payment("250.00"))
        .await
        .expect("a quarter is acceptable");

    let refused = record_payment_service(&pool, &owner, order_id, &payment("800.00"))
        .await
        .expect_err("$800 exceeds the $750 still owed");

    assert_eq!(
        refused,
        AppError::PaymentExceedsAmountDue {
            maximum_cents: 75_000
        }
    );
    assert!(
        refused.to_string().contains("$750.00"),
        "the message must name the real balance, got: {refused}"
    );

    // The refusal rolled back: nothing was written.
    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    assert_eq!(detail.paid_cents, 25_000);
    assert_eq!(
        list_payments_for_order(&pool, order_id)
            .await
            .unwrap()
            .len(),
        1
    );

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn an_overdue_order_that_is_settled_late_is_paid_not_overdue() {
    let pool = connect().await;
    let owner = test_owner();

    let mut input = thousand_dollar_order();
    input.due_date = "2020-01-01".to_string();
    let order_id = create_order_service(&pool, &owner, &input)
        .await
        .expect("a past due date is acceptable");

    let overdue = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    assert_eq!(overdue.status, OrderStatus::Overdue);

    // Part paid and still past the date: still overdue, because money is owed.
    record_payment_service(&pool, &owner, order_id, &payment("400.00"))
        .await
        .expect("acceptable");
    assert_eq!(
        find_order_for_user(&pool, &owner, order_id)
            .await
            .unwrap()
            .status,
        OrderStatus::Overdue
    );

    // Settled: finished, not outstanding. Nobody should be chased for it.
    record_payment_service(&pool, &owner, order_id, &payment("600.00"))
        .await
        .expect("acceptable");
    assert_eq!(
        find_order_for_user(&pool, &owner, order_id)
            .await
            .unwrap()
            .status,
        OrderStatus::Paid
    );

    cleanup(&pool, &owner).await;
}

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stranger_cannot_pay_someone_elses_order() {
    let pool = connect().await;
    let owner = test_owner();
    let stranger = test_owner();
    let order_id = order_owing_a_thousand(&pool, &owner).await;

    let refused = record_payment_service(&pool, &stranger, order_id, &payment("10.00"))
        .await
        .expect_err("a stranger must not pay it");

    // The same answer an id that was never written gets. Anything else would
    // confirm to the stranger that this order exists.
    assert_eq!(refused, AppError::NotFound);
    assert_eq!(refused.status_code(), 404);
    assert_eq!(
        list_payments_for_order(&pool, order_id)
            .await
            .unwrap()
            .len(),
        0
    );

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn paying_an_order_that_does_not_exist_is_not_found() {
    let pool = connect().await;
    let owner = test_owner();

    let refused = record_payment_service(&pool, &owner, Uuid::now_v7(), &payment("10.00"))
        .await
        .expect_err("nothing was written under this id");

    assert_eq!(refused, AppError::NotFound);
}

// ---------------------------------------------------------------------------
// Interaction with editing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_order_with_payments_can_no_longer_be_edited_or_deleted() {
    let pool = connect().await;
    let owner = test_owner();
    let order_id = order_owing_a_thousand(&pool, &owner).await;

    record_payment_service(&pool, &owner, order_id, &payment("1.00"))
        .await
        .expect("acceptable");

    let edit = update_order_service(&pool, &owner, order_id, &thousand_dollar_order())
        .await
        .expect_err("a paid-against order is frozen");
    assert_eq!(edit, AppError::OrderHasPayments);
    assert_eq!(edit.status_code(), 409);

    let delete = delete_order_service(&pool, &owner, order_id)
        .await
        .expect_err("a paid-against order is frozen");
    assert_eq!(delete, AppError::OrderHasPayments);

    // Still there, still paid.
    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    assert_eq!(detail.paid_cents, 100);

    cleanup(&pool, &owner).await;
}

#[tokio::test]
async fn deleting_an_order_takes_its_payments_with_it() {
    let pool = connect().await;
    let owner = test_owner();
    let order_id = order_owing_a_thousand(&pool, &owner).await;

    record_payment_service(&pool, &owner, order_id, &payment("100.00"))
        .await
        .expect("acceptable");

    // The service refuses while payments exist, which is the point of the
    // freeze — so the cascade is exercised the only way it can be reached, by
    // removing the order directly.
    sqlx::query("DELETE FROM orders WHERE id = $1")
        .bind(order_id)
        .execute(&pool)
        .await
        .expect("deleted");

    let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM payments WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&pool)
        .await
        .expect("the count is readable");

    assert_eq!(orphans, 0, "ON DELETE CASCADE removed the payments");
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

#[tokio::test]
async fn payments_are_listed_newest_first_with_a_deterministic_tiebreak() {
    let pool = connect().await;
    let owner = test_owner();
    let order_id = order_owing_a_thousand(&pool, &owner).await;

    let mut older = payment("100.00");
    older.paid_on = "2026-08-01".to_string();
    record_payment_service(&pool, &owner, order_id, &older)
        .await
        .expect("acceptable");

    // Two on the same later date. Ids are v7, so "recorded later" is the
    // tiebreak rather than an arbitrary byte comparison — without it these two
    // would shuffle between reads.
    let mut same_day = payment("200.00");
    same_day.paid_on = "2026-08-10".to_string();
    record_payment_service(&pool, &owner, order_id, &same_day)
        .await
        .expect("acceptable");

    let mut same_day_later = payment("300.00");
    same_day_later.paid_on = "2026-08-10".to_string();
    record_payment_service(&pool, &owner, order_id, &same_day_later)
        .await
        .expect("acceptable");

    let history = list_payments_for_order(&pool, order_id)
        .await
        .expect("readable");

    let amounts: Vec<i64> = history.iter().map(|entry| entry.amount_cents).collect();
    assert_eq!(amounts, vec![30_000, 20_000, 10_000]);

    // Reading twice gives the same order.
    let again = list_payments_for_order(&pool, order_id).await.unwrap();
    assert_eq!(
        history.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        again.iter().map(|entry| entry.id).collect::<Vec<_>>()
    );

    cleanup(&pool, &owner).await;
}

// ---------------------------------------------------------------------------
// Concurrency
//
// The reason this file exists.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_final_payments_cannot_both_succeed() {
    let pool = connect().await;
    let owner = test_owner();
    let order_id = order_owing_a_thousand(&pool, &owner).await;

    // Both ask to settle the whole $1,000, at the same moment, on different
    // connections. Without the row lock both would read "nothing paid", both
    // would pass the check, and the order would end up settled twice.
    let whole = payment("1000.00");
    let (left, right) = tokio::join!(
        record_payment_service(&pool, &owner, order_id, &whole),
        record_payment_service(&pool, &owner, order_id, &whole),
    );

    let succeeded = [&left, &right].iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        succeeded, 1,
        "exactly one payment must win; got left={left:?} right={right:?}"
    );

    let loser = match (left, right) {
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => error,
        _ => unreachable!("exactly one succeeded"),
    };
    assert_eq!(
        loser,
        AppError::PaymentExceedsAmountDue { maximum_cents: 0 },
        "the loser recalculated against the winner's committed payment"
    );

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    assert_eq!(detail.paid_cents, 100_000, "the order was settled once");
    assert_eq!(detail.status, OrderStatus::Paid);
    assert_eq!(
        list_payments_for_order(&pool, order_id)
            .await
            .unwrap()
            .len(),
        1
    );

    cleanup(&pool, &owner).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn many_simultaneous_payments_never_exceed_the_total() {
    let pool = connect().await;
    let owner = test_owner();
    let order_id = order_owing_a_thousand(&pool, &owner).await;

    // Six requests for $250 against a $1,000 order. Four can fit; the other two
    // must be refused, whatever order they happen to arrive in.
    //
    // Bound before the join: a temporary created inside the macro's argument
    // list is dropped while the future still borrows it.
    let quarter = payment("250.00");
    let results = tokio::join!(
        record_payment_service(&pool, &owner, order_id, &quarter),
        record_payment_service(&pool, &owner, order_id, &quarter),
        record_payment_service(&pool, &owner, order_id, &quarter),
        record_payment_service(&pool, &owner, order_id, &quarter),
        record_payment_service(&pool, &owner, order_id, &quarter),
        record_payment_service(&pool, &owner, order_id, &quarter),
    );

    let results = [
        results.0, results.1, results.2, results.3, results.4, results.5,
    ];
    let accepted = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(accepted, 4, "results: {results:?}");

    let detail = find_order_for_user(&pool, &owner, order_id)
        .await
        .expect("readable");
    assert_eq!(detail.paid_cents, 100_000);
    assert!(
        detail.paid_cents <= detail.total_cents,
        "the invariant this table exists to protect"
    );
    assert_eq!(detail.status, OrderStatus::Paid);

    cleanup(&pool, &owner).await;
}
