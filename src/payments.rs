//! Payments recorded against an order.
//!
//! The rule this module exists to enforce is one sentence long: the payments
//! against an order may never exceed its total. Enforcing it is the whole
//! difficulty, because the rule spans rows and the check is separated from the
//! write by a decision.
//!
//! Two requests that each read "nothing paid yet, $1,000 owed" and each pay
//! $1,000 will both pass a check written the obvious way, and the order ends up
//! settled twice. Postgres's default READ COMMITTED isolation does not prevent
//! that on its own: it gives each *statement* a fresh snapshot, but it does not
//! serialize a read-then-insert sequence across transactions.
//!
//! What prevents it here is [`orders::ssr::lock_owned_order`]. Every write takes
//! `FOR UPDATE` on the order row *before* reading the sum, so the order row acts
//! as a proxy lock for this table. A second request blocks on that lock until
//! the first commits, and its next statement then reads a sum that already
//! includes the first payment. The order of the four steps —
//!
//! 1. lock the order row,
//! 2. read the sum of payments,
//! 3. decide,
//! 4. insert and commit
//!
//! — is the entire correctness argument. Reading the sum before the lock, or on
//! a different connection, silently restores the race.

use leptos::prelude::*;
// Named by `#[server(input = Json)]` below, which expands to a bare `Json`
// path and so needs the codec in scope on both targets.
use leptos::server_fn::codec::Json;
use serde::{Deserialize, Serialize};

use chrono::NaiveDate;
use uuid::Uuid;

use crate::app::{format_cents, FieldError, MoneyText};
use crate::error::{AppError, AppResult};
use crate::orders::{parse_due_date, parse_money_to_cents};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A payment as submitted. Strings for the same reason orders use them: the
/// moment the browser parses an amount, the amount becomes the browser's
/// opinion of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPaymentInput {
    pub amount: String,
    pub paid_on: String,
}

/// A payment that has survived validation. Only [`validate_payment`] produces
/// one, and nothing downstream re-checks these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedPayment {
    pub amount_cents: i64,
    pub paid_on: NaiveDate,
}

/// One payment as it is read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRecord {
    pub id: Uuid,
    pub amount_cents: i64,
    pub paid_on: NaiveDate,
}

// ---------------------------------------------------------------------------
// Validation and arithmetic
//
// Compiled for both targets, so the browser's preview of what it can pay is the
// server's own answer rather than a second implementation of it.
// ---------------------------------------------------------------------------

/// The largest payment this order can still accept.
///
/// Saturating rather than checked: both inputs already survived validation and
/// are non-negative, so the only way `paid` exceeds `total` is data that
/// predates a later reduction of the total — and the honest answer to "how much
/// more can I pay" in that case is zero, not an error about arithmetic.
pub fn calculate_maximum_payment_cents(total_cents: i64, paid_cents: i64) -> i64 {
    total_cents.saturating_sub(paid_cents).max(0)
}

/// Builds a field error without naming the type.
///
/// `crate::error::FieldError` and the `FieldError` *component* imported above
/// share a name, and the component's import cannot be aliased — Leptos derives
/// `FieldErrorProps` from the identifier in the view. So the data type is
/// reached through this helper instead, exactly as `crate::orders` does.
fn invalid(field: impl Into<String>, message: impl Into<String>) -> crate::error::FieldError {
    crate::error::FieldError::new(field, message)
}

/// Checks a submitted payment, reporting every problem at once.
pub fn validate_payment(input: &NewPaymentInput) -> AppResult<ValidatedPayment> {
    let mut errors = Vec::new();

    let amount_cents = match parse_money_to_cents(input.amount.trim()) {
        // Zero is refused here rather than by the database's CHECK, so the user
        // is told why instead of being shown an internal error. A payment of
        // nothing is not a payment; it would sit in the history claiming
        // something happened.
        Ok(0) => {
            errors.push(invalid("amount", "Enter an amount greater than zero."));
            0
        }
        Ok(cents) => cents,
        Err(message) => {
            errors.push(invalid("amount", message));
            0
        }
    };

    let paid_on = match parse_due_date(input.paid_on.trim()) {
        Ok(date) => Some(date),
        Err(message) => {
            errors.push(invalid("paid_on", message));
            None
        }
    };

    if !errors.is_empty() {
        return Err(AppError::ValidationFailed(errors));
    }

    Ok(ValidatedPayment {
        amount_cents,
        // Unreachable: `errors` is empty, so the date parsed.
        paid_on: paid_on.expect("a validated payment has a date"),
    })
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[cfg(feature = "ssr")]
pub mod ssr {
    use super::{
        calculate_maximum_payment_cents, validate_payment, NewPaymentInput, PaymentRecord,
        ValidatedPayment,
    };
    use crate::error::{AppError, AppResult};
    use crate::orders::ssr::lock_owned_order;
    use chrono::NaiveDate;
    use sqlx::{PgConnection, PgPool};
    use uuid::Uuid;

    /// One payment row, as the database returns it.
    ///
    /// `sqlx::FromRow` lives here and not on [`PaymentRecord`], which crosses
    /// the wire: `sqlx` is an `ssr`-only dependency and a derive naming it would
    /// fail to resolve in the wasm build.
    #[derive(sqlx::FromRow)]
    struct PaymentRow {
        id: Uuid,
        amount_cents: i64,
        paid_on: NaiveDate,
    }

    /// The order's total, read inside the transaction that holds its lock.
    ///
    /// Read again here rather than passed in from an earlier request: the value
    /// a browser is showing was true when the page loaded, and an edit since
    /// then would make a decision based on it wrong.
    async fn order_total_in_transaction(
        transaction: &mut PgConnection,
        order_id: Uuid,
    ) -> AppResult<i64> {
        sqlx::query_scalar("SELECT total_cents FROM orders WHERE id = $1")
            .bind(order_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)
    }

    /// What has already been paid against the order, inside the transaction.
    ///
    /// `COALESCE` because a bare `sum()` over no rows is SQL `NULL` rather than
    /// `0`, and `::BIGINT` because `sum(bigint)` returns NUMERIC — Postgres
    /// widens it so a sum of many bigints cannot overflow. Neither is optional:
    /// without them this fails to decode into `i64`, and the first failure is
    /// on an order with no payments, which is every order at least once.
    pub async fn sum_payments_in_transaction(
        transaction: &mut PgConnection,
        order_id: Uuid,
    ) -> AppResult<i64> {
        let paid_cents: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(amount_cents), 0)::BIGINT FROM payments WHERE order_id = $1",
        )
        .bind(order_id)
        .fetch_one(&mut *transaction)
        .await?;

        Ok(paid_cents)
    }

    async fn insert_payment(
        transaction: &mut PgConnection,
        order_id: Uuid,
        payment: &ValidatedPayment,
    ) -> AppResult<Uuid> {
        // v7, so ids sort by creation time. Two payments recorded on the same
        // calendar date then have a deterministic order without a sequence
        // column to keep in step.
        let payment_id = Uuid::now_v7();

        sqlx::query(
            "INSERT INTO payments (id, order_id, amount_cents, paid_on) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(payment_id)
        .bind(order_id)
        .bind(payment.amount_cents)
        .bind(payment.paid_on)
        .execute(&mut *transaction)
        .await?;

        Ok(payment_id)
    }

    /// Records a payment, or refuses it, in one transaction.
    ///
    /// The four statements are in the only order that is safe. See this module's
    /// documentation for why; in short, the lock has to come first, because the
    /// sum is only trustworthy once nothing else can add to it.
    pub async fn record_payment_transaction(
        pool: &PgPool,
        owner_user_id: &str,
        order_id: Uuid,
        payment: &ValidatedPayment,
    ) -> AppResult<Uuid> {
        let mut transaction = pool.begin().await?;

        // 1. Ownership and the lock, in one statement. A stranger's order is
        //    never locked and never found.
        lock_owned_order(&mut transaction, owner_user_id, order_id).await?;

        // 2 and 3. Both reads happen behind the lock, so the decision they
        //    support is still true when the insert lands.
        let total_cents = order_total_in_transaction(&mut transaction, order_id).await?;
        let paid_cents = sum_payments_in_transaction(&mut transaction, order_id).await?;
        let maximum_cents = calculate_maximum_payment_cents(total_cents, paid_cents);

        if payment.amount_cents > maximum_cents {
            // Dropping the transaction rolls it back and releases the lock, so
            // the next waiter proceeds immediately rather than after a timeout.
            tracing::info!(
                %order_id,
                attempted_cents = payment.amount_cents,
                maximum_cents,
                "refused a payment larger than the amount due"
            );
            return Err(AppError::PaymentExceedsAmountDue { maximum_cents });
        }

        // 4. Insert and commit. The lock is held until the commit, which is
        //    what makes the sum read above final.
        let payment_id = insert_payment(&mut transaction, order_id, payment).await?;

        transaction.commit().await?;

        Ok(payment_id)
    }

    /// Validate, then persist. The single entry point, so the browser and the
    /// REST surface cannot acquire different ideas of what a payment is.
    pub async fn record_payment_service(
        pool: &PgPool,
        owner_user_id: &str,
        order_id: Uuid,
        input: &NewPaymentInput,
    ) -> AppResult<Uuid> {
        let payment = validate_payment(input)?;
        let payment_id =
            record_payment_transaction(pool, owner_user_id, order_id, &payment).await?;

        tracing::info!(
            %order_id,
            %payment_id,
            amount_cents = payment.amount_cents,
            "payment recorded"
        );

        Ok(payment_id)
    }

    /// One order's payments, newest first.
    ///
    /// Ordered by the day the money moved, then by id. The tiebreak matters:
    /// two payments on the same date have no natural order, and without it the
    /// history would shuffle between reads. Ids are v7, so the tiebreak is
    /// "recorded later" rather than an arbitrary byte comparison.
    ///
    /// Not owner-scoped, and deliberately so — it takes an `order_id` that the
    /// caller has already proven ownership of. Every caller reaches it through
    /// a read that filtered on the owner.
    pub async fn list_payments_for_order(
        pool: &PgPool,
        order_id: Uuid,
    ) -> AppResult<Vec<PaymentRecord>> {
        let rows: Vec<PaymentRow> = sqlx::query_as(
            "SELECT id, amount_cents, paid_on \
             FROM payments \
             WHERE order_id = $1 \
             ORDER BY paid_on DESC, id DESC",
        )
        .bind(order_id)
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| PaymentRecord {
                id: row.id,
                amount_cents: row.amount_cents,
                paid_on: row.paid_on,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Server functions
// ---------------------------------------------------------------------------

/// Records a payment against an order the caller owns.
///
/// Ownership is not checked here. It is part of the `WHERE` clause of the
/// locking statement inside the transaction, which is the only place where the
/// answer is still true by the time the write lands.
#[server(input = Json)]
pub async fn record_payment(order_id: Uuid, input: NewPaymentInput) -> Result<(), AppError> {
    use crate::auth::ssr::{ensure_same_origin, incoming_parts, require_user};
    use crate::error::ssr::report;
    use crate::orders::ssr::pool;
    use ssr::record_payment_service;

    report(
        async move {
            let parts = incoming_parts()?;
            ensure_same_origin(&parts)?;

            let user = require_user().await?;

            record_payment_service(&pool()?, &user.id, order_id, &input).await?;

            Ok(())
        }
        .await,
    )
}

// ---------------------------------------------------------------------------
// User interface
// ---------------------------------------------------------------------------

/// The form for recording a payment against one order.
///
/// `maximum_cents` is what the server last said was owed. It drives the hint and
/// nothing else: the server re-reads the real figure behind the row lock, so a
/// stale number here can only produce a refusal, never an accepted overpayment.
#[component]
pub fn PaymentForm(
    order_id: Uuid,
    maximum_cents: i64,
    /// Today, as the server sees it. Passed in rather than read from the
    /// browser's clock so the default date agrees with the date the overdue
    /// rule is evaluated against.
    today: NaiveDate,
    record: ServerAction<RecordPayment>,
) -> impl IntoView {
    let amount = RwSignal::new(String::new());
    let paid_on = RwSignal::new(today.to_string());
    // Suppresses a message about a value the user has since changed. Without
    // it, a corrected field keeps the complaint from the previous submission.
    let edited = RwSignal::new(false);

    let submit = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        edited.set(false);

        record.dispatch(RecordPayment {
            order_id,
            input: NewPaymentInput {
                amount: amount.get(),
                paid_on: paid_on.get(),
            },
        });
    };

    let failure = Signal::derive(move || {
        if edited.get() {
            return None;
        }

        match record.value().get() {
            Some(Err(error)) => Some(error),
            _ => None,
        }
    });

    // A per-field message when the error names a field, and the error's own
    // sentence otherwise — an overpayment refusal is about the amount even
    // though it is not a validation failure.
    let message_for = move |field: &'static str| {
        failure.get().and_then(|error| match error {
            AppError::PaymentExceedsAmountDue { .. } if field == "amount" => {
                Some(error.to_string())
            }
            _ => error.message_for(field).map(str::to_string),
        })
    };

    let other_failure = Signal::derive(move || match failure.get() {
        Some(AppError::ValidationFailed(_)) | Some(AppError::PaymentExceedsAmountDue { .. }) => {
            None
        }
        other => other.map(|error| error.to_string()),
    });

    view! {
        <form on:submit=submit on:input=move |_| edited.set(true)>
            <h2>"Record a payment"</h2>
            <p>"Up to " {move || format_cents(maximum_cents)} " is still owed."</p>

            <div class="grid">
                <label>
                    "Amount"
                    <input
                        type="text"
                        inputmode="decimal"
                        name="amount"
                        placeholder="0.00"
                        aria-invalid=move || message_for("amount").map(|_| "true")
                        value=move || amount.get()
                        prop:value=move || amount.get()
                        on:input:target=move |event| amount.set(event.target().value())
                    />
                    <FieldError message=move || message_for("amount") />
                </label>

                <label>
                    "Paid on"
                    <input
                        type="date"
                        name="paid_on"
                        aria-invalid=move || message_for("paid_on").map(|_| "true")
                        value=move || paid_on.get()
                        prop:value=move || paid_on.get()
                        on:input:target=move |event| paid_on.set(event.target().value())
                    />
                    <FieldError message=move || message_for("paid_on") />
                </label>
            </div>

            {move || {
                other_failure
                    .get()
                    .map(|message| {
                        view! {
                            <article class="error-panel" role="alert">
                                {message}
                            </article>
                        }
                    })
            }}

            <div class="form-actions">
                <button type="submit" aria-busy=move || record.pending().get().to_string()>
                    "Record payment"
                </button>
            </div>
        </form>
    }
}

/// Every payment against one order, newest first, ending in what they add up to.
///
/// The running total is the point of the last row. A history that lists three
/// payments and leaves the reader to add them up is not a reconciliation, and
/// the figure printed here is the same `paid_cents` the totals above the history
/// use — it comes from the same read, so the list and the sum cannot disagree.
///
/// Renders nothing at all when there are no payments. An empty table with a
/// "no payments yet" row says less than the payment form directly above it,
/// which already implies the same thing.
///
/// `Option` rather than an early `return ().into_any()`. This view has to swap
/// from nothing to a table the moment the first payment is recorded, and a
/// `None` leaves a placeholder node in the DOM for the table to be inserted
/// against. It is the same shape the conditional notices in
/// [`crate::orders::OrderDetailView`] use, for the same reason.
///
/// Not `<For>`: these rows carry no per-row signal to preserve, and the whole
/// view is rebuilt from a fresh [`crate::orders::OrderDetail`] each time the
/// resource resolves. Keying would buy nothing here. The line-item editor keys
/// its rows because they *are* signals; this one has nothing to lose.
#[component]
pub fn PaymentHistory(payments: Vec<PaymentRecord>, paid_cents: i64) -> impl IntoView {
    let count = payments.len();

    (count > 0).then(move || {
        view! {
        <section class="payment-history">
            <h2>"Payments"</h2>

            <table class="order-table">
                <thead>
                    <tr>
                        <th scope="col">"Paid on"</th>
                        <th scope="col">"Amount"</th>
                    </tr>
                </thead>
                <tbody>
                    {payments
                        .into_iter()
                        .map(|payment| {
                            view! {
                                <tr>
                                    // ISO 8601, because `Display` on `NaiveDate`
                                    // already produces it and `format()` needs
                                    // chrono's `alloc`, which the browser build
                                    // deliberately omits. The two would render
                                    // the same string anyway.
                                    <td>{payment.paid_on.to_string()}</td>
                                    <td>
                                        <MoneyText cents=payment.amount_cents />
                                    </td>
                                </tr>
                            }
                        })
                        .collect::<Vec<_>>()}
                </tbody>
                <tfoot>
                    <tr>
                        <th scope="row">
                            {if count == 1 {
                                "1 payment".to_string()
                            } else {
                                format!("{count} payments")
                            }}
                        </th>
                        <td>
                            <MoneyText cents=paid_cents />
                        </td>
                    </tr>
                </tfoot>
            </table>
        </section>
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(amount: &str, paid_on: &str) -> NewPaymentInput {
        NewPaymentInput {
            amount: amount.to_string(),
            paid_on: paid_on.to_string(),
        }
    }

    #[test]
    fn accepts_a_payment_in_cents() {
        let payment = validate_payment(&input("$1,234.50", "2026-08-13")).expect("valid");

        assert_eq!(payment.amount_cents, 123_450);
        assert_eq!(
            payment.paid_on,
            NaiveDate::from_ymd_opt(2026, 8, 13).unwrap()
        );
    }

    #[test]
    fn refuses_a_payment_of_nothing() {
        let error = validate_payment(&input("0", "2026-08-13")).expect_err("zero is not a payment");

        assert_eq!(
            error.message_for("amount").unwrap(),
            "Enter an amount greater than zero."
        );
    }

    #[test]
    fn reports_a_bad_amount_and_a_bad_date_together() {
        let error = validate_payment(&input("abc", "13/08/2026")).expect_err("both are wrong");

        assert!(error.message_for("amount").is_some());
        assert!(error.message_for("paid_on").is_some());
    }

    #[test]
    fn the_maximum_is_what_is_still_owed() {
        assert_eq!(calculate_maximum_payment_cents(100_000, 0), 100_000);
        assert_eq!(calculate_maximum_payment_cents(100_000, 40_000), 60_000);
        assert_eq!(calculate_maximum_payment_cents(100_000, 100_000), 0);
        // Paid more than the total, because the total was later reduced. The
        // honest answer is zero, not a negative maximum.
        assert_eq!(calculate_maximum_payment_cents(100_000, 120_000), 0);
    }
}
