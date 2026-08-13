//! Orders and their line items: parsing, validation, arithmetic, persistence,
//! and the creation form.
//!
//! Money never exists as a float in this module. A submitted `"$1,234.50"` is
//! parsed straight to the integer `123450` and stays an `i64` count of cents
//! through validation, multiplication, addition, Postgres `BIGINT`, and back out
//! to the formatter in [`crate::app::format_cents`]. There is no rounding step
//! anywhere, because there is nothing to round.
//!
//! Every multiplication and addition is checked. `i64` cents covers roughly
//! ±92 quadrillion dollars, so overflow is not a realistic invoice — but the
//! alternative to a checked operation is a total that silently wraps negative,
//! and a billing system that can produce a negative total from positive inputs
//! is worse than one that refuses the input.
//!
//! Parsing and arithmetic are compiled for **both** targets on purpose: the
//! browser uses them to show a running total as the user types, and the server
//! uses the identical code to decide what is actually stored. The preview cannot
//! disagree with the saved order, because it is not a second implementation.
//! The browser's answer is never trusted — [`create_order`] re-parses the raw
//! strings server-side and ignores anything the client computed.

use leptos::prelude::*;
// Named by `#[server(input = Json)]` below, which expands to a bare `Json`
// path and so needs the codec in scope on both targets.
use leptos::server_fn::codec::Json;
use leptos_meta::Title;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use leptos_router::NavigateOptions;
use serde::{Deserialize, Serialize};

use chrono::NaiveDate;
use uuid::Uuid;

use crate::app::{FieldError, MoneyText, StatusBadge};
use crate::error::{AppError, AppResult};
use crate::payments::{
    calculate_maximum_payment_cents, PaymentForm, PaymentHistory, PaymentRecord, RecordPayment,
};

/// Upper bound on line items in one order.
///
/// Not a database limit — a blast radius. Without it a single request can ask
/// this process to allocate and insert an unbounded number of rows, and "the
/// form only ever sends a few" is an assumption about the browser, which is the
/// one participant that cannot be trusted.
pub const MAX_ITEMS: usize = 100;

/// Longest accepted customer name.
pub const MAX_CUSTOMER_LEN: usize = 200;

/// Longest accepted line-item description.
pub const MAX_DESCRIPTION_LEN: usize = 500;

// ---------------------------------------------------------------------------
// Wire types
//
// Everything the browser submits arrives as a string, including the numbers.
// That is not laziness: `<input type="number">` still yields text, and the
// moment the client parses an amount it becomes the client's opinion of the
// amount. Keeping the raw strings until the server validates them means the
// server sees exactly what the user typed.
// ---------------------------------------------------------------------------

/// One line item as submitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderItemInput {
    pub description: String,
    pub quantity: String,
    pub unit_price: String,
}

/// A whole order as submitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewOrderInput {
    pub customer: String,
    pub due_date: String,
    pub items: Vec<OrderItemInput>,
}

/// A line item that has survived validation.
///
/// `line_total_cents` is stored rather than recomputed on read so the database
/// holds the arithmetic that was actually agreed, and the `CHECK` constraint in
/// `migrations/001_orders.sql` re-states the multiplication where a future bug
/// cannot talk its way past it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedItem {
    pub description: String,
    pub quantity: i64,
    pub unit_price_cents: i64,
    pub line_total_cents: i64,
}

/// An order that has survived validation and is safe to persist.
///
/// Only [`validate_create_order`] produces one. Nothing downstream re-checks
/// these values, so nothing downstream should be able to invent them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedOrder {
    pub customer: String,
    pub due_date: NaiveDate,
    pub total_cents: i64,
    pub items: Vec<ValidatedItem>,
}

// ---------------------------------------------------------------------------
// Read models
//
// What a page needs, rather than what a table holds. `status` and
// `amount_due_cents` are computed on the server and sent down finished, so no
// component can arrive at a different answer than the one the API reports.
// ---------------------------------------------------------------------------

/// One row of the order list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderSummary {
    pub id: Uuid,
    pub customer: String,
    pub due_date: NaiveDate,
    pub total_cents: i64,
    pub paid_cents: i64,
    pub item_count: i64,
    pub status: OrderStatus,
}

impl OrderSummary {
    pub fn amount_due_cents(&self) -> i64 {
        self.total_cents.saturating_sub(self.paid_cents)
    }
}

/// One line item as it is read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderLine {
    pub description: String,
    pub quantity: i64,
    pub unit_price_cents: i64,
    pub line_total_cents: i64,
}

/// A whole order with its line items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderDetail {
    pub id: Uuid,
    pub customer: String,
    pub due_date: NaiveDate,
    pub total_cents: i64,
    pub paid_cents: i64,
    pub status: OrderStatus,
    /// False once the order has a payment against it. The browser uses this to
    /// hide the edit and delete controls; the server re-derives it inside the
    /// transaction, because hiding a control is not the same as refusing one.
    pub editable: bool,
    /// The server's today, the same value [`OrderStatus`] above was derived
    /// against. Sent down so the payment form's default date and the overdue
    /// rule cannot disagree — the browser's own clock is a different machine's
    /// opinion, and it is the one that can be wrong.
    pub today: NaiveDate,
    pub items: Vec<OrderLine>,
    /// Every payment against this order, newest first.
    ///
    /// Carried on the same DTO as the totals rather than fetched separately, so
    /// the history and the `paid_cents` it adds up to are read in the same
    /// request and cannot disagree. A second resource would let the page show a
    /// list of payments whose sum is not the figure printed above it.
    pub payments: Vec<PaymentRecord>,
}

impl OrderDetail {
    pub fn amount_due_cents(&self) -> i64 {
        self.total_cents.saturating_sub(self.paid_cents)
    }
}

// ---------------------------------------------------------------------------
// Derived status
// ---------------------------------------------------------------------------

/// Where an order stands. Never stored — always computed from the total, the
/// payments against it, and today's date.
///
/// Storing it would create a second source of truth that goes stale on its own:
/// an order becomes overdue because a day passed, with nothing writing to the
/// database at all. There is no event to hang an update on, so there is nothing
/// to store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    PartiallyPaid,
    Paid,
    Overdue,
}

impl OrderStatus {
    /// The stable wire form. Matches the serde representation and the value
    /// `crate::app::StatusBadge` maps to a tone.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::PartiallyPaid => "partially_paid",
            Self::Paid => "paid",
            Self::Overdue => "overdue",
        }
    }

    /// What a person reads.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::PartiallyPaid => "Partially paid",
            Self::Paid => "Paid",
            Self::Overdue => "Overdue",
        }
    }

    /// Every variant, in the order the dashboard filter lists them.
    ///
    /// Exhaustive by construction rather than by discipline: adding a variant
    /// to the enum breaks [`as_str`](Self::as_str)'s `match`, and the compiler
    /// stops there before this array can silently omit it from the filter.
    pub const ALL: [OrderStatus; 4] = [
        Self::Pending,
        Self::PartiallyPaid,
        Self::Paid,
        Self::Overdue,
    ];

    /// The inverse of [`as_str`](Self::as_str), for a `?status=` query value.
    ///
    /// Returns `None` for anything unrecognised, and the caller is expected to
    /// read that as "no filter" rather than as an error. A URL is typed, pasted
    /// and truncated by people; `?status=pad` should show the dashboard, not an
    /// error page, because the request is still perfectly answerable — the user
    /// simply named no filter this code recognises.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == raw)
    }
}

/// The counts and money printed above the dashboard table.
///
/// Always describes **every** order the caller owns, never the filtered subset.
/// The tiles are the reason to click a filter, so they have to keep reporting
/// what is there while you are looking at one slice of it — a filtered count
/// would answer a question nobody asked ("how many overdue orders are overdue")
/// and lose the one being asked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardTotals {
    pub order_count: usize,
    /// Billed across every order, paid or not.
    pub total_cents: i64,
    pub paid_cents: i64,
    /// What is still owed: the sum of each order's own amount due, with an
    /// overpaid order contributing zero rather than a negative.
    ///
    /// Not `total_cents - paid_cents`. Those two differ the moment one order is
    /// overpaid, and the difference is not a rounding detail — a £5 surplus on
    /// a settled invoice would silently pay down £5 of a different customer's
    /// balance, and the dashboard would under-report what is actually owed.
    /// Money owed on one order is not money owed on another, so the sum is
    /// taken per order and a credit is never allowed to travel.
    ///
    /// Overpayment is refused on the way in ([`crate::payments::record_payment`]),
    /// so a negative amount due means the row was written by something other
    /// than this application. The clamp is what keeps that a display oddity on
    /// one row instead of a wrong figure at the top of the page.
    pub outstanding_cents: i64,
    /// How many orders sit in each status, in [`OrderStatus::ALL`] order.
    pub by_status: [usize; 4],
}

impl DashboardTotals {
    pub fn count_of(&self, status: OrderStatus) -> usize {
        self.by_status[OrderStatus::ALL
            .iter()
            .position(|candidate| *candidate == status)
            .expect("ALL contains every variant")]
    }
}

/// Adds up a list of summaries. Pure, so the totals are testable without a
/// database and cannot drift from the rows they describe — both come from one
/// `Vec<OrderSummary>` in [`list_orders`].
///
/// Saturating rather than checked: this is a display figure derived from values
/// that were each already validated on the way in, and a dashboard that refuses
/// to render because a hypothetical sum overflowed is worse than one that pins
/// at `i64::MAX`. The write paths that must not be wrong stay checked.
pub fn summarise_orders(orders: &[OrderSummary]) -> DashboardTotals {
    let mut totals = DashboardTotals {
        order_count: orders.len(),
        ..Default::default()
    };

    for order in orders {
        totals.total_cents = totals.total_cents.saturating_add(order.total_cents);
        totals.paid_cents = totals.paid_cents.saturating_add(order.paid_cents);
        totals.outstanding_cents = totals
            .outstanding_cents
            .saturating_add(order.amount_due_cents().max(0));

        let slot = OrderStatus::ALL
            .iter()
            .position(|candidate| *candidate == order.status)
            .expect("ALL contains every variant");
        totals.by_status[slot] += 1;
    }

    totals
}

/// Keeps the orders matching `filter`, or all of them when there is no filter.
///
/// A free function, and the only place the dashboard's filter is applied, so
/// "does `?status=paid` mean the same thing as the Paid badge" is one assertion
/// against one function rather than a property of a request handler.
pub fn filter_by_status(
    orders: Vec<OrderSummary>,
    filter: Option<OrderStatus>,
) -> Vec<OrderSummary> {
    match filter {
        // Compares the derived status, never re-derives it. This function
        // cannot invent a fifth answer, because it does not compute any.
        Some(wanted) => orders
            .into_iter()
            .filter(|order| order.status == wanted)
            .collect(),
        None => orders,
    }
}

/// What the dashboard renders: the whole picture, and the slice being viewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dashboard {
    /// Over every order the caller owns, regardless of `filter`.
    pub totals: DashboardTotals,
    /// The filter that produced `orders`, echoed back so the page renders the
    /// filter the server actually applied rather than the one the URL asked
    /// for. They differ whenever `?status=` names something unrecognised.
    pub filter: Option<OrderStatus>,
    pub orders: Vec<OrderSummary>,
}

/// Derives an order's status. The only place this decision is made.
///
/// Precedence is paid, then overdue, then partially paid, then pending — and
/// the order of those tests is the whole specification. Putting "paid" first is
/// what makes a settled invoice stay settled after its due date passes: an
/// order nobody owes anything on is not overdue, however old it is.
///
/// `today` and `paid_cents` are parameters rather than things this function
/// fetches, which is what makes every branch reachable from a test without a
/// clock or a database.
pub fn derive_order_status(
    total_cents: i64,
    paid_cents: i64,
    due_date: NaiveDate,
    today: NaiveDate,
) -> OrderStatus {
    // `>=` rather than `==`: Feature 6 refuses overpayment, but if a row ever
    // did carry more than the total, "paid" is the honest answer and an
    // equality test would silently report it as partially paid.
    //
    // A zero-total order is therefore paid from the moment it exists, which is
    // correct — there is nothing outstanding on it.
    if paid_cents >= total_cents {
        return OrderStatus::Paid;
    }

    if due_date < today {
        return OrderStatus::Overdue;
    }

    if paid_cents > 0 {
        return OrderStatus::PartiallyPaid;
    }

    OrderStatus::Pending
}

// ---------------------------------------------------------------------------
// Parsing
//
// Each parser returns the message the user should see, not an error type. There
// is exactly one caller — the validator — and the message is the whole point.
// ---------------------------------------------------------------------------

/// Parses a typed amount into whole cents.
///
/// Accepts what a person actually types into a money field: `1234.50`, `$1,234.50`,
/// `1234`, `.50`, `1234.5`. Rejects negatives (no line item costs less than
/// nothing), three or more decimal places (a third digit is either a typo or a
/// precision this system does not keep, and silently discarding it is how a
/// cent goes missing), and anything else non-numeric.
pub fn parse_money_to_cents(raw: &str) -> Result<i64, &'static str> {
    const TOO_LARGE: &str = "That amount is larger than this system can hold.";
    const NOT_A_NUMBER: &str = "Enter an amount like 1234.50.";

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter a unit price.");
    }

    let unsigned = trimmed.strip_prefix('$').unwrap_or(trimmed).trim_start();
    if unsigned.starts_with('-') {
        return Err("Enter a unit price of zero or more.");
    }

    // Thousands separators are display, not data. Removing them here is what
    // lets a value pasted out of a spreadsheet round-trip.
    let cleaned: String = unsigned
        .chars()
        .filter(|character| *character != ',')
        .collect();

    // A second '.' stays in `fraction`, where the digit check below rejects it.
    let (whole_text, fraction_text) = match cleaned.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (cleaned.as_str(), ""),
    };

    if whole_text.is_empty() && fraction_text.is_empty() {
        return Err(NOT_A_NUMBER);
    }
    if !whole_text
        .chars()
        .all(|character| character.is_ascii_digit())
        || !fraction_text
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return Err(NOT_A_NUMBER);
    }
    if fraction_text.len() > 2 {
        return Err("Amounts carry at most two decimal places.");
    }

    let whole: i64 = if whole_text.is_empty() {
        0
    } else {
        whole_text.parse().map_err(|_| TOO_LARGE)?
    };

    // "1.5" is fifty cents, not five. Padding on the right is the difference.
    let cents: i64 = match fraction_text.len() {
        0 => 0,
        1 => fraction_text.parse::<i64>().map_err(|_| NOT_A_NUMBER)? * 10,
        _ => fraction_text.parse::<i64>().map_err(|_| NOT_A_NUMBER)?,
    };

    whole
        .checked_mul(100)
        .and_then(|dollars| dollars.checked_add(cents))
        .ok_or(TOO_LARGE)
}

/// Parses a typed quantity into a positive whole number.
///
/// Fractional quantities are refused rather than rounded. "1.5 units" of
/// something priced per unit has no defined total, and guessing one is how a
/// disputed invoice starts.
pub fn parse_quantity(raw: &str) -> Result<i64, &'static str> {
    const NOT_A_NUMBER: &str = "Enter the quantity as a whole number.";

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter a quantity.");
    }

    let cleaned: String = trimmed
        .chars()
        .filter(|character| *character != ',')
        .collect();
    if cleaned.is_empty() || !cleaned.chars().all(|character| character.is_ascii_digit()) {
        return Err(NOT_A_NUMBER);
    }

    let quantity: i64 = cleaned
        .parse()
        .map_err(|_| "That quantity is larger than this system can hold.")?;

    if quantity < 1 {
        return Err("Enter a quantity of one or more.");
    }

    Ok(quantity)
}

/// Parses the `YYYY-MM-DD` value an `<input type="date">` submits.
///
/// A past date is accepted. Recording an invoice that was already due is a
/// normal thing to do, and the application's own idea of "overdue" is derived
/// from this date at read time rather than fixed at creation.
pub fn parse_due_date(raw: &str) -> Result<NaiveDate, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter a due date.");
    }

    NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").map_err(|_| "Enter the due date as YYYY-MM-DD.")
}

// ---------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------

/// `quantity × unit price`, in cents.
pub fn calculate_line_total_cents(quantity: i64, unit_price_cents: i64) -> AppResult<i64> {
    quantity
        .checked_mul(unit_price_cents)
        .ok_or(AppError::AmountOutOfRange)
}

/// The sum of every line total, in cents.
pub fn calculate_order_total_cents(items: &[ValidatedItem]) -> AppResult<i64> {
    items.iter().try_fold(0_i64, |running, item| {
        running
            .checked_add(item.line_total_cents)
            .ok_or(AppError::AmountOutOfRange)
    })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn invalid(field: impl Into<String>, message: impl Into<String>) -> crate::error::FieldError {
    crate::error::FieldError::new(field, message)
}

/// Validates a submitted order and computes its totals.
///
/// Collects **every** problem before returning. A form that reports one failure
/// per round trip makes the user discover the rest by trial, and each trial
/// costs a request.
pub fn validate_create_order(input: &NewOrderInput) -> AppResult<ValidatedOrder> {
    let mut errors = Vec::new();

    let customer = input.customer.trim().to_string();
    if customer.is_empty() {
        errors.push(invalid("customer", "Enter the customer's name."));
    } else if customer.chars().count() > MAX_CUSTOMER_LEN {
        errors.push(invalid(
            "customer",
            format!("Keep the customer name under {MAX_CUSTOMER_LEN} characters."),
        ));
    }

    let due_date = match parse_due_date(&input.due_date) {
        Ok(date) => Some(date),
        Err(message) => {
            errors.push(invalid("due_date", message));
            None
        }
    };

    if input.items.is_empty() {
        errors.push(invalid("items", "Add at least one line item."));
    } else if input.items.len() > MAX_ITEMS {
        errors.push(invalid(
            "items",
            format!("An order can hold at most {MAX_ITEMS} line items."),
        ));
    }

    // Validated even when the count is out of range, so a user who pasted 200
    // rows still sees which of them are also malformed.
    let items: Vec<ValidatedItem> = input
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| validate_item(index, item, &mut errors))
        .collect();

    if !errors.is_empty() {
        return Err(AppError::ValidationFailed(errors));
    }

    // Unreachable while `errors` is empty: `due_date` is `None` only when a
    // message was pushed. Expressed as a fallback rather than an `unwrap` so a
    // future edit to the branch above cannot turn a logic slip into a panic.
    let Some(due_date) = due_date else {
        return Err(AppError::ValidationFailed(vec![invalid(
            "due_date",
            "Enter a due date.",
        )]));
    };

    let total_cents = calculate_order_total_cents(&items)?;

    Ok(ValidatedOrder {
        customer,
        due_date,
        total_cents,
        items,
    })
}

/// Validates one line item, appending any failures under `items[N].<field>`.
///
/// The index is part of the field name so the browser can put each message
/// beside the row that caused it, and so a non-browser client gets the same
/// information without having to guess at ordering.
fn validate_item(
    index: usize,
    item: &OrderItemInput,
    errors: &mut Vec<crate::error::FieldError>,
) -> Option<ValidatedItem> {
    let description = item.description.trim().to_string();
    if description.is_empty() {
        errors.push(invalid(
            format!("items[{index}].description"),
            "Describe this line item.",
        ));
    } else if description.chars().count() > MAX_DESCRIPTION_LEN {
        errors.push(invalid(
            format!("items[{index}].description"),
            format!("Keep the description under {MAX_DESCRIPTION_LEN} characters."),
        ));
    }

    let quantity = match parse_quantity(&item.quantity) {
        Ok(quantity) => Some(quantity),
        Err(message) => {
            errors.push(invalid(format!("items[{index}].quantity"), message));
            None
        }
    };

    let unit_price_cents = match parse_money_to_cents(&item.unit_price) {
        Ok(cents) => Some(cents),
        Err(message) => {
            errors.push(invalid(format!("items[{index}].unit_price"), message));
            None
        }
    };

    let (quantity, unit_price_cents) = (quantity?, unit_price_cents?);

    // An overflowing line has one row to blame, so it is reported as a field
    // error rather than as the order-wide `AmountOutOfRange`.
    let line_total_cents = match calculate_line_total_cents(quantity, unit_price_cents) {
        Ok(total) => total,
        Err(_) => {
            errors.push(invalid(
                format!("items[{index}].unit_price"),
                "That quantity and price multiply to more than this system can hold.",
            ));
            return None;
        }
    };

    if description.is_empty() {
        return None;
    }

    Some(ValidatedItem {
        description,
        quantity,
        unit_price_cents,
        line_total_cents,
    })
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[cfg(feature = "ssr")]
pub mod ssr {
    use super::{
        derive_order_status, validate_create_order, NewOrderInput, OrderDetail, OrderLine,
        OrderSummary, ValidatedOrder,
    };
    use crate::error::{AppError, AppResult};
    use chrono::{NaiveDate, Utc};
    use leptos::prelude::use_context;
    use sqlx::{PgConnection, PgPool, QueryBuilder};
    use std::sync::OnceLock;
    use uuid::Uuid;

    /// The date the overdue rule compares against.
    ///
    /// UTC, and deliberately so. A due date is a calendar date with no time
    /// zone attached to it, and this application stores no time zone for a user
    /// or a customer to be correct relative to — so there is no local midnight
    /// to prefer. One fixed reference that every request agrees on beats a
    /// per-connection one; Postgres's `CURRENT_DATE` resolves against the
    /// session's `timezone` setting and could quietly disagree with this.
    ///
    /// The single call site. `derive_order_status` takes `today` as a parameter
    /// so every branch of it is reachable from a test without a clock.
    fn today() -> NaiveDate {
        Utc::now().date_naive()
    }

    // `paid_cents` and `has_payments` are read as correlated subqueries against
    // `payments` rather than by a second round trip, so an order and the money
    // against it are answered by one statement and cannot disagree.
    //
    // Neither the `COALESCE` nor the `::BIGINT` is decoration, and both fail in
    // the same place — at decode time, with an opaque error:
    //
    //   COALESCE  a bare `sum()` over no rows is SQL NULL, not `0`, so `i64`
    //             cannot receive it. That is every order until its first
    //             payment, which is the default state of every order.
    //   ::BIGINT  `sum(bigint)` returns NUMERIC in Postgres, not BIGINT — it is
    //             widened so a sum of many bigints cannot overflow. `i64`
    //             cannot receive that either, and the cast back is safe here
    //             because the total it is compared against is itself an i64.

    /// One row of the order list, as the database returns it.
    ///
    /// `sqlx::FromRow` lives on this type and not on [`OrderSummary`], which
    /// crosses the wire to the browser: `sqlx` is an `ssr`-only dependency, and
    /// a derive that names it would fail to resolve in the wasm build.
    #[derive(sqlx::FromRow)]
    struct SummaryRow {
        id: Uuid,
        customer: String,
        due_date: NaiveDate,
        total_cents: i64,
        paid_cents: i64,
        item_count: i64,
    }

    #[derive(sqlx::FromRow)]
    struct OrderRow {
        id: Uuid,
        customer: String,
        due_date: NaiveDate,
        total_cents: i64,
        paid_cents: i64,
        has_payments: bool,
    }

    #[derive(sqlx::FromRow)]
    struct LineRow {
        description: String,
        quantity: i64,
        unit_price_cents: i64,
        line_total_cents: i64,
    }

    /// The pool `main.rs` created at startup, registered here once so that it
    /// can be found from anywhere in the process.
    ///
    /// Set by [`set_pool`] before the listener binds, and never replaced.
    static PROCESS_POOL: OnceLock<PgPool> = OnceLock::new();

    /// Records the process-wide pool. Called once, from `main`, before the
    /// server starts accepting requests.
    pub fn set_pool(pool: PgPool) {
        // A second call would mean two pools in one process, which is the thing
        // this module exists to prevent. Ignoring it keeps the first.
        let _ = PROCESS_POOL.set(pool);
    }

    /// The connection pool `main.rs` created at startup.
    ///
    /// One pool per process. A server function that built its own would double
    /// the connection count against a database whose limit is the reason
    /// pooling exists.
    ///
    /// Context first, then the process pool. The fallback is not belt and
    /// braces: Leptos builds parts of a page more than once while rendering a
    /// response, and the extra pass does not always carry the request's
    /// reactive context. Without a way to reach the pool from there, a
    /// duplicated render of the dashboard failed with `INTERNAL` and Leptos
    /// serialised that failure into the HTML next to the real answer — where
    /// the browser could hydrate it instead. Both passes now return the same
    /// thing, which is the property that matters. See `auth::Protected`.
    ///
    /// Cloning a `PgPool` clones a handle to the same pool, not a connection.
    pub fn pool() -> AppResult<PgPool> {
        if let Some(pool) = use_context::<PgPool>() {
            return Ok(pool);
        }

        PROCESS_POOL.get().cloned().ok_or_else(|| {
            // Only reachable before `set_pool` has run, which cannot happen for
            // a request: the listener binds after it. A wiring defect, then,
            // not a user-visible condition.
            tracing::error!("no database pool in context or in the process");
            AppError::Internal
        })
    }

    /// Writes an order and all of its line items, or neither.
    ///
    /// The order's `total_cents` is the sum of rows that live in a different
    /// table, which no `CHECK` constraint can see. The transaction is therefore
    /// the only thing holding that invariant: a crash between the two statements
    /// would otherwise leave a stored total that no longer describes the items,
    /// and every payment decision downstream reads that total.
    ///
    /// Ids are UUID v7, generated here rather than by the database. v7 sorts by
    /// creation time, so the primary-key index stays append-ordered instead of
    /// scattering writes across the B-tree the way v4 does — and generating them
    /// in Rust means the id is known before the insert, without a round trip.
    pub async fn insert_order(
        pool: &PgPool,
        owner_user_id: &str,
        order: &ValidatedOrder,
    ) -> AppResult<Uuid> {
        let order_id = Uuid::now_v7();
        let mut transaction = pool.begin().await?;

        sqlx::query(
            "INSERT INTO orders (id, owner_user_id, customer, due_date, total_cents) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(order_id)
        .bind(owner_user_id)
        .bind(&order.customer)
        .bind(order.due_date)
        .bind(order.total_cents)
        .execute(&mut *transaction)
        .await?;

        insert_items(&mut transaction, order_id, order).await?;

        transaction.commit().await?;

        Ok(order_id)
    }

    /// Writes every line item of an order in one statement.
    ///
    /// Shared by the create and update paths so both agree on positions and on
    /// the stored line totals; takes a connection rather than a pool because it
    /// is only ever correct inside somebody else's transaction.
    async fn insert_items(
        transaction: &mut PgConnection,
        order_id: Uuid,
        order: &ValidatedOrder,
    ) -> AppResult<()> {
        if order.items.is_empty() {
            return Ok(());
        }

        // `validate_create_order` caps this at MAX_ITEMS, but these functions
        // are callable on their own, so the cast that would otherwise be a
        // silent truncation is checked here rather than assumed upstream.
        let positions = (0..order.items.len())
            .map(|position| {
                i32::try_from(position).map_err(|_| {
                    tracing::error!(count = order.items.len(), "order has too many line items");
                    AppError::Internal
                })
            })
            .collect::<AppResult<Vec<i32>>>()?;

        // One multi-row INSERT rather than a statement per item: the round trips
        // are what cost, not the rows.
        let mut builder = QueryBuilder::new(
            "INSERT INTO order_items \
             (id, order_id, position, description, quantity, unit_price_cents, line_total_cents) ",
        );
        builder.push_values(
            positions.into_iter().zip(order.items.iter()),
            |mut row, (position, item)| {
                row.push_bind(Uuid::now_v7())
                    .push_bind(order_id)
                    .push_bind(position)
                    .push_bind(item.description.clone())
                    .push_bind(item.quantity)
                    .push_bind(item.unit_price_cents)
                    .push_bind(item.line_total_cents);
            },
        );
        builder.build().execute(&mut *transaction).await?;

        Ok(())
    }

    /// Validate, then persist. The single entry point both the browser form and
    /// the REST API in Feature 9 call, so neither surface can acquire its own
    /// idea of what a valid order is.
    pub async fn create_order_service(
        pool: &PgPool,
        owner_user_id: &str,
        input: &NewOrderInput,
    ) -> AppResult<Uuid> {
        let order = validate_create_order(input)?;
        let order_id = insert_order(pool, owner_user_id, &order).await?;

        tracing::info!(
            %order_id,
            items = order.items.len(),
            total_cents = order.total_cents,
            "order created"
        );

        Ok(order_id)
    }

    // -----------------------------------------------------------------------
    // Reads
    //
    // Every statement below filters on `owner_user_id` in the same `WHERE`
    // clause that matches the id. That is not a convention to remember — it is
    // what makes "no such order" and "somebody else's order" indistinguishable
    // in the result, and therefore indistinguishable in the answer.
    // -----------------------------------------------------------------------

    /// Every order the caller owns, soonest due first.
    ///
    /// One query, not one per order: the item count comes from a `LEFT JOIN`
    /// and a `GROUP BY` rather than a follow-up read per row.
    pub async fn list_orders_for_user(
        pool: &PgPool,
        owner_user_id: &str,
    ) -> AppResult<Vec<OrderSummary>> {
        let rows: Vec<SummaryRow> = sqlx::query_as(
            "SELECT orders.id, \
                    orders.customer, \
                    orders.due_date, \
                    orders.total_cents, \
                    COALESCE(( \
                        SELECT sum(payments.amount_cents) \
                        FROM payments \
                        WHERE payments.order_id = orders.id \
                    ), 0)::BIGINT AS paid_cents, \
                    count(order_items.id) AS item_count \
             FROM orders \
             LEFT JOIN order_items ON order_items.order_id = orders.id \
             WHERE orders.owner_user_id = $1 \
             GROUP BY orders.id \
             ORDER BY orders.due_date ASC, orders.created_at ASC",
        )
        .bind(owner_user_id)
        .fetch_all(pool)
        .await?;

        // One `today` for the whole list, so two rows in the same table cannot
        // be judged against different days.
        let today = today();

        Ok(rows
            .into_iter()
            .map(|row| OrderSummary {
                status: derive_order_status(row.total_cents, row.paid_cents, row.due_date, today),
                id: row.id,
                customer: row.customer,
                due_date: row.due_date,
                total_cents: row.total_cents,
                paid_cents: row.paid_cents,
                item_count: row.item_count,
            })
            .collect())
    }

    /// One order with its line items and its payments, or [`AppError::NotFound`].
    ///
    /// Three statements rather than a join: a join between an order and its
    /// items returns the order's columns once per item, and reassembling that is
    /// more code than the round trip saves for a single order. Adding payments
    /// to that join is worse than additive — items multiply by payments, so a
    /// four-line order with three payments comes back as twelve rows describing
    /// seven facts.
    ///
    /// The three reads are not in a transaction and do not need to be. Nothing
    /// here decides anything: it is a snapshot for a page, and the write path
    /// that must not race —
    /// [`crate::payments::ssr::record_payment_transaction`] — re-reads
    /// everything it needs behind [`lock_owned_order`] rather than trusting a
    /// number this function produced.
    pub async fn find_order_for_user(
        pool: &PgPool,
        owner_user_id: &str,
        order_id: Uuid,
    ) -> AppResult<OrderDetail> {
        let order: OrderRow = sqlx::query_as(
            "SELECT id, \
                    customer, \
                    due_date, \
                    total_cents, \
                    COALESCE(( \
                        SELECT sum(payments.amount_cents) \
                        FROM payments \
                        WHERE payments.order_id = orders.id \
                    ), 0)::BIGINT AS paid_cents, \
                    EXISTS ( \
                        SELECT 1 FROM payments WHERE payments.order_id = orders.id \
                    ) AS has_payments \
             FROM orders \
             WHERE id = $1 AND owner_user_id = $2",
        )
        .bind(order_id)
        .bind(owner_user_id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound)?;

        let items: Vec<LineRow> = sqlx::query_as(
            "SELECT description, quantity, unit_price_cents, line_total_cents \
             FROM order_items \
             WHERE order_id = $1 \
             ORDER BY position ASC",
        )
        .bind(order_id)
        .fetch_all(pool)
        .await?;

        // Ownership was already established by the order query above, which
        // returned `NotFound` for a row that is not the caller's. This runs on
        // an id that has been proven, which is why it takes no owner.
        let payments = crate::payments::ssr::list_payments_for_order(pool, order_id).await?;

        // One `today` for the status and for the value sent to the browser, so
        // a page cannot show a date the status was not derived against.
        let today = today();

        Ok(OrderDetail {
            status: derive_order_status(order.total_cents, order.paid_cents, order.due_date, today),
            today,
            editable: !order.has_payments,
            id: order.id,
            customer: order.customer,
            due_date: order.due_date,
            total_cents: order.total_cents,
            paid_cents: order.paid_cents,
            payments,
            items: items
                .into_iter()
                .map(|row| OrderLine {
                    description: row.description,
                    quantity: row.quantity,
                    unit_price_cents: row.unit_price_cents,
                    line_total_cents: row.line_total_cents,
                })
                .collect(),
        })
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    /// Locks the caller's order for the rest of the transaction.
    ///
    /// `FOR UPDATE` on the order row is the serialization point for everything
    /// that touches the order, including the payment insert in
    /// [`crate::payments`]. Ownership is part of the same `WHERE`, so a row that
    /// is not the caller's is never locked and never found.
    ///
    /// **Every write path must call this first, on the same connection, inside
    /// the same transaction.** The order row is being used as a proxy lock for
    /// the payments table, which is what makes "read the sum, decide, insert"
    /// safe under Postgres's default READ COMMITTED isolation: a second
    /// transaction blocks here until the first commits, and its next statement
    /// then runs against a snapshot that already includes the first payment. A
    /// write that reads the sum *before* taking this lock reintroduces exactly
    /// the race this design exists to close.
    ///
    /// Postgres refuses `FOR UPDATE` alongside an aggregate or a `GROUP BY`,
    /// which is why the lock is taken on the order row alone and every sum is a
    /// separate statement.
    pub async fn lock_owned_order(
        transaction: &mut PgConnection,
        owner_user_id: &str,
        order_id: Uuid,
    ) -> AppResult<()> {
        sqlx::query("SELECT id FROM orders WHERE id = $1 AND owner_user_id = $2 FOR UPDATE")
            .bind(order_id)
            .bind(owner_user_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;

        Ok(())
    }

    /// Refuses to change an order that money has been recorded against.
    ///
    /// Called inside the transaction and after [`lock_owned_order`], which is
    /// the only position where the answer stays true until the commit. Anywhere
    /// earlier — including "just before `begin`" — a payment can land between
    /// the check and the write, and the order changes underneath money that was
    /// paid against the old version of it.
    async fn ensure_no_payments(transaction: &mut PgConnection, order_id: Uuid) -> AppResult<()> {
        let has_payments: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM payments WHERE order_id = $1)")
                .bind(order_id)
                .fetch_one(&mut *transaction)
                .await?;

        if has_payments {
            tracing::info!(%order_id, "refused to change an order that has payments");
            return Err(AppError::OrderHasPayments);
        }

        Ok(())
    }

    /// Replaces an order's contents in one transaction.
    ///
    /// The line items are deleted and reinserted rather than diffed. A line item
    /// has no identity a user ever refers to — it is a row in a list they
    /// retyped — and matching old rows to new ones by position would invent an
    /// identity that the edit did not preserve.
    pub async fn update_order_transaction(
        pool: &PgPool,
        owner_user_id: &str,
        order_id: Uuid,
        order: &ValidatedOrder,
    ) -> AppResult<()> {
        let mut transaction = pool.begin().await?;

        lock_owned_order(&mut transaction, owner_user_id, order_id).await?;
        ensure_no_payments(&mut transaction, order_id).await?;

        sqlx::query(
            "UPDATE orders \
             SET customer = $1, due_date = $2, total_cents = $3, updated_at = now() \
             WHERE id = $4",
        )
        .bind(&order.customer)
        .bind(order.due_date)
        .bind(order.total_cents)
        .bind(order_id)
        .execute(&mut *transaction)
        .await?;

        sqlx::query("DELETE FROM order_items WHERE order_id = $1")
            .bind(order_id)
            .execute(&mut *transaction)
            .await?;

        insert_items(&mut transaction, order_id, order).await?;

        transaction.commit().await?;

        Ok(())
    }

    /// Deletes an order. Its line items go with it through `ON DELETE CASCADE`.
    pub async fn delete_order_transaction(
        pool: &PgPool,
        owner_user_id: &str,
        order_id: Uuid,
    ) -> AppResult<()> {
        let mut transaction = pool.begin().await?;

        lock_owned_order(&mut transaction, owner_user_id, order_id).await?;
        ensure_no_payments(&mut transaction, order_id).await?;

        sqlx::query("DELETE FROM orders WHERE id = $1")
            .bind(order_id)
            .execute(&mut *transaction)
            .await?;

        transaction.commit().await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Services
    //
    // Validate, then persist. The single entry point for each operation, so the
    // browser and Feature 9's REST surface cannot acquire different ideas of
    // what an order is.
    // -----------------------------------------------------------------------

    pub async fn update_order_service(
        pool: &PgPool,
        owner_user_id: &str,
        order_id: Uuid,
        input: &NewOrderInput,
    ) -> AppResult<()> {
        // The submitted values are revalidated and the total recomputed from
        // scratch. An edit is a new order that happens to keep an id.
        let order = validate_create_order(input)?;
        update_order_transaction(pool, owner_user_id, order_id, &order).await?;

        tracing::info!(
            %order_id,
            items = order.items.len(),
            total_cents = order.total_cents,
            "order updated"
        );

        Ok(())
    }

    pub async fn delete_order_service(
        pool: &PgPool,
        owner_user_id: &str,
        order_id: Uuid,
    ) -> AppResult<()> {
        delete_order_transaction(pool, owner_user_id, order_id).await?;

        tracing::info!(%order_id, "order deleted");

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// REST API
//
// A second surface over the *same* services, not a second implementation. Every
// handler below is four steps — authenticate, check the origin, call the
// service that the Leptos server function also calls, shape a response — and
// contains no SQL, no validation, and no business rule of its own. A rule that
// existed here would be a rule the web UI does not enforce.
// ---------------------------------------------------------------------------

#[cfg(feature = "ssr")]
pub mod api {
    use axum::extract::rejection::JsonRejection;
    use axum::extract::{FromRef, Path, Query, State};
    use axum::http::header::{LOCATION, ORIGIN, SET_COOKIE};
    use axum::http::{request::Parts, HeaderValue, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde::{Deserialize, Serialize};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::ssr::{
        create_order_service, delete_order_service, find_order_for_user, list_orders_for_user,
        update_order_service,
    };
    use super::{filter_by_status, summarise_orders, Dashboard, NewOrderInput, OrderStatus};
    use crate::auth::ssr::authenticate;
    use crate::auth::AuthUser;
    use crate::error::{AppError, AppResult, FieldError};

    /// The `/api` route table, mounted by `main.rs` and by `tests/api.rs`.
    ///
    /// Generic over the router's state so the binary can mount it on its own
    /// `AppState` while the contract tests mount it on a bare `PgPool`. Both
    /// exercise the same paths and the same methods: a test that declared its
    /// own route table would pass while the deployed URL was `/api/order`.
    ///
    /// Axum 0.8 path parameters are `{id}`, not the `:id` of 0.7 and earlier —
    /// the old syntax now panics when the router is built rather than quietly
    /// failing to match.
    ///
    /// A method that a path does not implement answers `405 Method Not
    /// Allowed` automatically, because the methods are registered on one route
    /// rather than as separate routes that would shadow one another.
    pub fn router<S>() -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
        PgPool: FromRef<S>,
    {
        Router::new()
            .route("/api/orders", get(list_orders).post(create_order))
            .route(
                "/api/orders/{id}",
                get(get_order).put(update_order).delete(delete_order),
            )
            .route(
                "/api/orders/{id}/payments",
                post(crate::payments::api::record_payment),
            )
    }

    /// `?status=` on the order list.
    #[derive(Debug, Deserialize)]
    pub struct StatusQuery {
        status: Option<String>,
    }

    /// The caller, and any cookie Better Auth rotated while confirming them.
    ///
    /// [`AppError::Unauthenticated`] renders as `401`, which is the answer a
    /// missing or expired session deserves. The Leptos pages get the same
    /// answer from `require_user`; this is the same check without the reactive
    /// context that a bare Axum handler does not have.
    pub(crate) async fn caller(parts: &Parts) -> AppResult<(AuthUser, Vec<String>)> {
        let checked = authenticate(parts).await?;

        match checked.user {
            Some(user) => Ok((user, checked.set_cookies)),
            None => Err(AppError::Unauthenticated),
        }
    }

    /// Refuses a state-changing request that a *browser* sent from elsewhere.
    ///
    /// Deliberately weaker than the server-function rule, which demands a
    /// matching `Origin` outright. This API is authenticated by a cookie, so
    /// the attack to stop is a page on another origin making the browser send
    /// this one a request with the session attached — and a browser cannot
    /// suppress `Origin` on a cross-site state-changing request. An absent
    /// `Origin` therefore means the request did not come from a page at all:
    /// `curl`, a script, a mobile client. Refusing those would make the REST
    /// surface unusable by everything except the site that already has a UI.
    ///
    /// Present-and-wrong is still refused, which is the case that matters.
    pub(crate) fn guard_origin(parts: &Parts) -> AppResult<()> {
        if parts.headers.contains_key(ORIGIN) {
            crate::auth::ssr::ensure_same_origin(parts)?;
        }

        Ok(())
    }

    /// Parses a path id, or reports which field was wrong.
    ///
    /// `400` rather than `404`: the id is the caller's own input and its syntax
    /// is something they can see is wrong, so saying so leaks nothing about
    /// whose orders exist. A well-formed id that is not the caller's is the
    /// case that must be indistinguishable from missing, and that answer comes
    /// from the query, not from here.
    pub(crate) fn parse_id(raw: &str) -> AppResult<Uuid> {
        Uuid::parse_str(raw).map_err(|_| {
            AppError::ValidationFailed(vec![FieldError {
                field: "id".to_string(),
                message: "That is not a valid order id.".to_string(),
            }])
        })
    }

    /// Turns a rejected body into the same field-error shape every other
    /// failure uses, so a client has one error format to parse rather than
    /// Axum's plain text for malformed JSON and this application's JSON for
    /// everything else.
    pub(crate) fn body<T>(result: Result<Json<T>, JsonRejection>) -> AppResult<T> {
        result.map(|Json(value)| value).map_err(|rejection| {
            AppError::ValidationFailed(vec![FieldError {
                field: "body".to_string(),
                // Serde's own message: which key, and what was expected there.
                message: rejection.body_text(),
            }])
        })
    }

    /// Attaches any rotated session cookie to a finished response.
    ///
    /// One header per cookie, never joined — `Set-Cookie` cannot be
    /// comma-folded. Silence on an unencodable value would be worse than a log
    /// line, but the value is a session token, so only the failure is logged.
    pub(crate) fn with_cookies(mut response: Response, cookies: Vec<String>) -> Response {
        for cookie in cookies {
            match HeaderValue::from_str(&cookie) {
                Ok(value) => {
                    response.headers_mut().append(SET_COOKIE, value);
                }
                Err(_) => tracing::error!("auth service sent an unencodable Set-Cookie header"),
            }
        }

        response
    }

    pub(crate) fn ok<T: Serialize>(value: T, cookies: Vec<String>) -> Response {
        with_cookies(Json(value).into_response(), cookies)
    }

    /// `201` with a `Location`, which is what makes a create discoverable: the
    /// client is told where the thing it made now lives, not only what it is.
    pub(crate) fn created<T: Serialize>(
        location: &str,
        value: T,
        cookies: Vec<String>,
    ) -> Response {
        let mut response = (StatusCode::CREATED, Json(value)).into_response();

        match HeaderValue::from_str(location) {
            Ok(value) => {
                response.headers_mut().insert(LOCATION, value);
            }
            // A UUID path is always a valid header value; this arm is
            // unreachable and is here so a future change cannot make it silent.
            Err(_) => tracing::error!(%location, "could not encode a Location header"),
        }

        with_cookies(response, cookies)
    }

    /// `GET /api/orders` — the caller's orders, with totals and an optional
    /// `?status=` filter.
    ///
    /// Returns the same [`Dashboard`] document the web dashboard renders. One
    /// shape for both surfaces means the totals a client reports are the totals
    /// the page shows, and there is no second DTO to keep in step.
    ///
    /// An unrecognised `?status=` is no filter rather than a `400`, exactly as
    /// in the UI: the request is still answerable, and the `filter` field in
    /// the response says which filter was actually applied.
    async fn list_orders(
        State(pool): State<PgPool>,
        parts: Parts,
        Query(query): Query<StatusQuery>,
    ) -> AppResult<Response> {
        let (user, cookies) = caller(&parts).await?;

        let filter = query.status.as_deref().and_then(OrderStatus::parse);
        let all = list_orders_for_user(&pool, &user.id).await?;
        let totals = summarise_orders(&all);

        Ok(ok(
            Dashboard {
                totals,
                filter,
                orders: filter_by_status(all, filter),
            },
            cookies,
        ))
    }

    /// `POST /api/orders` — creates an order and returns it as it was stored.
    ///
    /// The response is a re-read rather than an echo of the request, so what
    /// the client gets back carries the server's own totals, status, and id
    /// instead of the strings it sent.
    async fn create_order(
        State(pool): State<PgPool>,
        parts: Parts,
        input: Result<Json<NewOrderInput>, JsonRejection>,
    ) -> AppResult<Response> {
        let (user, cookies) = caller(&parts).await?;
        guard_origin(&parts)?;
        let input = body(input)?;

        let order_id = create_order_service(&pool, &user.id, &input).await?;
        let order = find_order_for_user(&pool, &user.id, order_id).await?;

        Ok(created(&format!("/api/orders/{order_id}"), order, cookies))
    }

    /// `GET /api/orders/{id}` — one order with its items and payments.
    ///
    /// `404` both for an id that does not exist and for one belonging to
    /// somebody else. The two are the same answer on purpose: a distinct `403`
    /// would confirm that an order with that id exists, which is exactly the
    /// fact an unauthorised caller is probing for.
    async fn get_order(
        State(pool): State<PgPool>,
        parts: Parts,
        Path(id): Path<String>,
    ) -> AppResult<Response> {
        let (user, cookies) = caller(&parts).await?;
        let order_id = parse_id(&id)?;

        let order = find_order_for_user(&pool, &user.id, order_id).await?;

        Ok(ok(order, cookies))
    }

    /// `PUT /api/orders/{id}` — replaces an order's customer, date, and items.
    ///
    /// A replace, not a patch: the body is the same document `POST` takes, and
    /// the line items sent are the line items the order ends up with. A partial
    /// update of a list of items has no obvious meaning — there is no stable
    /// client-visible identity for a line to patch against — and inventing one
    /// would be a rule this API has and the web form does not.
    ///
    /// `409` once a payment exists against the order, because the total the
    /// money was measured against must not move under it.
    async fn update_order(
        State(pool): State<PgPool>,
        parts: Parts,
        Path(id): Path<String>,
        input: Result<Json<NewOrderInput>, JsonRejection>,
    ) -> AppResult<Response> {
        let (user, cookies) = caller(&parts).await?;
        guard_origin(&parts)?;
        let order_id = parse_id(&id)?;
        let input = body(input)?;

        update_order_service(&pool, &user.id, order_id, &input).await?;
        let order = find_order_for_user(&pool, &user.id, order_id).await?;

        Ok(ok(order, cookies))
    }

    /// `DELETE /api/orders/{id}` — removes an order and its line items.
    ///
    /// `204` with no body, because there is nothing left to describe. Deleting
    /// an order that is already gone is a `404`, not a success: the caller
    /// asked about a specific row, and pretending is not an answer.
    async fn delete_order(
        State(pool): State<PgPool>,
        parts: Parts,
        Path(id): Path<String>,
    ) -> AppResult<Response> {
        let (user, cookies) = caller(&parts).await?;
        guard_origin(&parts)?;
        let order_id = parse_id(&id)?;

        delete_order_service(&pool, &user.id, order_id).await?;

        Ok(with_cookies(
            StatusCode::NO_CONTENT.into_response(),
            cookies,
        ))
    }
}

// ---------------------------------------------------------------------------
// Server functions
// ---------------------------------------------------------------------------

/// Creates an order owned by the caller.
///
/// `input = Json` rather than the default form encoding: the payload is a
/// nested document with a variable-length array, and round-tripping that
/// through `application/x-www-form-urlencoded` depends on bracket-index
/// conventions that break the moment a row is removed and the indices go
/// sparse. JSON has one obvious representation of a list.
#[server(input = Json)]
pub async fn create_order(input: NewOrderInput) -> Result<Uuid, AppError> {
    use crate::auth::ssr::{ensure_same_origin, incoming_parts, require_user};
    use crate::error::ssr::report;
    use ssr::{create_order_service, pool};

    report(
        async move {
            let parts = incoming_parts()?;
            // A cookie is sent by the browser on any cross-site POST, so the
            // session alone does not establish that the *user* asked for this.
            ensure_same_origin(&parts)?;

            let user = require_user().await?;

            create_order_service(&pool()?, &user.id, &input).await
        }
        .await,
    )
}

/// Replaces an order the caller owns.
///
/// Ownership is not checked here. It is part of the `WHERE` clause of the
/// locking statement inside the transaction, which is the only place where the
/// answer is still true by the time the write lands.
#[server(input = Json)]
pub async fn update_order(id: Uuid, input: NewOrderInput) -> Result<(), AppError> {
    use crate::auth::ssr::{ensure_same_origin, incoming_parts, require_user};
    use crate::error::ssr::report;
    use ssr::{pool, update_order_service};

    report(
        async move {
            let parts = incoming_parts()?;
            ensure_same_origin(&parts)?;

            let user = require_user().await?;

            update_order_service(&pool()?, &user.id, id, &input).await
        }
        .await,
    )
}

/// Deletes an order the caller owns.
#[server]
pub async fn delete_order(id: Uuid) -> Result<(), AppError> {
    use crate::auth::ssr::{ensure_same_origin, incoming_parts, require_user};
    use crate::error::ssr::report;
    use ssr::{delete_order_service, pool};

    report(
        async move {
            let parts = incoming_parts()?;
            ensure_same_origin(&parts)?;

            let user = require_user().await?;

            delete_order_service(&pool()?, &user.id, id).await
        }
        .await,
    )
}

/// Every order the caller owns, and the totals across all of them.
///
/// A read, so there is no same-origin check: a `GET`-shaped request that leaks
/// nothing to a third party — the response is only readable by the origin that
/// made it — and `require_user` is what decides whose orders these are.
///
/// `filter` is applied **here, in Rust, after [`derive_order_status`]** — never
/// as a `WHERE` clause. A SQL predicate would be a second copy of the status
/// rule, written in a different language, and the first day the two disagree
/// the dashboard shows an order under a badge that contradicts the filter that
/// found it. The status is derived exactly once, in exactly one place, and the
/// filter is a test against the answer.
///
/// Reading every row and discarding some of them is the cost of that, and it is
/// the right trade at this size: the caller's own orders are a page of rows, not
/// a table scan. The totals want the whole set anyway.
#[server]
pub async fn list_orders(filter: Option<OrderStatus>) -> Result<Dashboard, AppError> {
    use crate::auth::ssr::require_user;
    use crate::error::ssr::report;
    use ssr::{list_orders_for_user, pool};

    report(
        async move {
            let user = require_user().await?;

            let all = list_orders_for_user(&pool()?, &user.id).await?;
            // Totals first, over the unfiltered list: the tiles describe
            // everything the caller owns whatever slice they are looking at.
            let totals = summarise_orders(&all);

            Ok(Dashboard {
                totals,
                filter,
                orders: filter_by_status(all, filter),
            })
        }
        .await,
    )
}

/// One order the caller owns, with its line items.
#[server]
pub async fn get_order(id: Uuid) -> Result<OrderDetail, AppError> {
    use crate::auth::ssr::require_user;
    use crate::error::ssr::report;
    use ssr::{find_order_for_user, pool};

    report(
        async move {
            let user = require_user().await?;

            find_order_for_user(&pool()?, &user.id, id).await
        }
        .await,
    )
}

// ---------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------

/// Renders an amount as the plain decimal string the money input accepts.
///
/// Deliberately not [`crate::app::format_cents`]: that one adds a currency
/// symbol and thousands separators for reading. This one produces a value that
/// [`parse_money_to_cents`] turns back into exactly the same integer, which is
/// what an input prefilled from an existing order needs.
pub fn format_cents_for_input(cents: i64) -> String {
    let magnitude = cents.unsigned_abs();
    let sign = if cents < 0 { "-" } else { "" };

    format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100)
}

/// One editable row. Every field is its own signal so typing in one cell does
/// not re-render the others, and the row is `Copy` so `<For>` can hand it out
/// without cloning.
#[derive(Debug, Clone, Copy)]
struct ItemRow {
    key: usize,
    description: RwSignal<String>,
    quantity: RwSignal<String>,
    unit_price: RwSignal<String>,
}

impl ItemRow {
    /// `key` is a monotonic counter, never the row's position. Keying `<For>`
    /// on the position would make deleting the first row look, to the diffing
    /// algorithm, like every subsequent row's content changed — and the input
    /// the user was typing in would lose focus.
    fn new(key: usize) -> Self {
        Self {
            key,
            description: RwSignal::new(String::new()),
            quantity: RwSignal::new("1".to_string()),
            unit_price: RwSignal::new(String::new()),
        }
    }

    /// A row prefilled from a stored line item.
    fn from_line(key: usize, line: &OrderLine) -> Self {
        Self {
            key,
            description: RwSignal::new(line.description.clone()),
            quantity: RwSignal::new(line.quantity.to_string()),
            unit_price: RwSignal::new(format_cents_for_input(line.unit_price_cents)),
        }
    }

    fn to_input(self) -> OrderItemInput {
        OrderItemInput {
            description: self.description.get_untracked(),
            quantity: self.quantity.get_untracked(),
            unit_price: self.unit_price.get_untracked(),
        }
    }
}

/// The order form, for a new order or an existing one.
///
/// One component for both because they are the same form: the fields, the
/// validation, the arithmetic, and the server's answer are identical, and the
/// only difference is which server function the submit dispatches to and where
/// it goes afterwards. Two components would be two places to fix a form bug.
///
/// The running total is computed in the browser from the same parsers the
/// server uses, so it is a preview of the server's answer rather than a second
/// opinion. It is never submitted: the server recomputes everything from the
/// raw strings.
#[component]
pub fn OrderEditor(
    /// The order being edited, or `None` to create one.
    #[prop(optional)]
    existing: Option<OrderDetail>,
) -> impl IntoView {
    let create = ServerAction::<CreateOrder>::new();
    let update = ServerAction::<UpdateOrder>::new();
    let navigate = use_navigate();

    let editing = existing.as_ref().map(|order| order.id);
    let customer = RwSignal::new(
        existing
            .as_ref()
            .map(|order| order.customer.clone())
            .unwrap_or_default(),
    );
    let due_date = RwSignal::new(
        existing
            .as_ref()
            // The value an `<input type="date">` accepts, which is the same
            // format `parse_due_date` reads back. `Display` for `NaiveDate` is
            // already ISO 8601; `format()` would say so explicitly but is not
            // compiled into the browser build, where chrono has no `alloc`.
            .map(|order| order.due_date.to_string())
            .unwrap_or_default(),
    );
    let initial_rows: Vec<ItemRow> = match &existing {
        Some(order) => order
            .items
            .iter()
            .enumerate()
            .map(|(key, line)| ItemRow::from_line(key, line))
            .collect(),
        None => Vec::new(),
    };
    // An order always has at least one row to type into, including a stored one
    // whose items somehow went missing.
    let next_key = RwSignal::new(initial_rows.len().max(1));
    let rows = RwSignal::new(if initial_rows.is_empty() {
        vec![ItemRow::new(0)]
    } else {
        initial_rows
    });
    // Set by any keystroke in the form, cleared on submit. See `failure`.
    let edited = RwSignal::new(false);

    // Effects do not run during SSR, so this is browser-only by construction.
    // Both land on the order's own page: after creating it, that page is the
    // proof it exists; after editing, it is where the change is visible.
    let after_create = navigate.clone();
    Effect::new(move |_| {
        if let Some(Ok(order_id)) = create.value().get() {
            after_create(&format!("/orders/{order_id}"), Default::default());
        }
    });
    Effect::new(move |_| {
        if let (Some(Ok(())), Some(order_id)) = (update.value().get(), editing) {
            navigate(&format!("/orders/{order_id}"), Default::default());
        }
    });

    // The server's verdict is about the values that were submitted, and the
    // action holds on to it until the next dispatch. Once the user changes
    // anything, those values no longer exist — leaving "Enter the customer's
    // name." under a field that now has a name in it is worse than showing
    // nothing, so the whole verdict is withdrawn on the first keystroke.
    let failure = Signal::derive(move || {
        if edited.get() {
            return None;
        }

        // Only one of the two is ever dispatched by a given mount.
        match (create.value().get(), update.value().get()) {
            (Some(Err(error)), _) | (_, Some(Err(error))) => Some(error),
            _ => None,
        }
    });

    let pending = Signal::derive(move || create.pending().get() || update.pending().get());

    // Takes an owned name because the row fields are built with `format!`.
    let message_for = move |field: String| -> Option<String> {
        failure
            .get()
            .and_then(|error| error.message_for(&field).map(ToOwned::to_owned))
    };

    let position_of = move |key: usize| {
        rows.get()
            .iter()
            .position(|row| row.key == key)
            .unwrap_or_default()
    };

    let line_total = move |row: ItemRow| -> Option<i64> {
        let quantity = parse_quantity(&row.quantity.get()).ok()?;
        let unit_price_cents = parse_money_to_cents(&row.unit_price.get()).ok()?;

        calculate_line_total_cents(quantity, unit_price_cents).ok()
    };

    // `None` when any row is incomplete or the sum would overflow, which is
    // exactly when there is no honest number to show.
    let running_total = move || {
        rows.get()
            .into_iter()
            .try_fold(0_i64, |running, row| running.checked_add(line_total(row)?))
    };

    let add_row = move |_| {
        let key = next_key.get_untracked();
        next_key.set(key + 1);
        rows.update(|rows| rows.push(ItemRow::new(key)));
    };

    let submit = move |event: leptos::ev::SubmitEvent| {
        // The form has a real `<button type="submit">` so Enter works and the
        // browser applies its own required-field handling first; this stops the
        // navigation that would otherwise discard the page.
        event.prevent_default();
        edited.set(false);

        let input = NewOrderInput {
            customer: customer.get_untracked(),
            due_date: due_date.get_untracked(),
            items: rows
                .get_untracked()
                .into_iter()
                .map(ItemRow::to_input)
                .collect(),
        };

        match editing {
            Some(id) => {
                update.dispatch(UpdateOrder { id, input });
            }
            None => {
                create.dispatch(CreateOrder { input });
            }
        }
    };

    view! {
        // One listener for every field: `input` bubbles, so a row added later
        // is covered without the handler being wired to it.
        <form on:submit=submit on:input=move |_| edited.set(true)>
            {move || {
                failure
                    .get()
                    .map(|error| {
                        let summary = if error.field_errors().is_empty() {
                            error.to_string()
                        } else {
                            "Check the highlighted fields below.".to_string()
                        };
                        view! {
                            <article class="error-panel" role="alert">
                                {summary}
                            </article>
                        }
                    })
            }}

            <div class="grid">
                <label>
                    "Customer"
                    <input
                        type="text"
                        name="customer"
                        autocomplete="off"
                        aria-invalid=move || {
                            message_for("customer".to_string()).map(|_| "true")
                        }
                        // Both, and neither is redundant. `value` is the HTML
                        // attribute, and it is the only one of the two that
                        // server-side rendering emits — without it the edit form
                        // arrives blank and stays blank, because hydration
                        // assumes the DOM already matches. `prop:value` is the
                        // live DOM property, which is what an already-rendered
                        // input actually displays once the user has typed into
                        // it.
                        value=move || customer.get()
                        prop:value=move || customer.get()
                        on:input:target=move |event| customer.set(event.target().value())
                    />
                    <FieldError message=move || message_for("customer".to_string()) />
                </label>

                <label>
                    "Due date"
                    <input
                        type="date"
                        name="due_date"
                        aria-invalid=move || {
                            message_for("due_date".to_string()).map(|_| "true")
                        }
                        value=move || due_date.get()
                        prop:value=move || due_date.get()
                        on:input:target=move |event| due_date.set(event.target().value())
                    />
                    <FieldError message=move || message_for("due_date".to_string()) />
                </label>
            </div>

            <h2>"Line items"</h2>
            <FieldError message=move || message_for("items".to_string()) />

            <div class="table-scroll">
                <table class="item-editor">
                    <thead>
                        <tr>
                            <th scope="col">"Description"</th>
                            <th scope="col">"Quantity"</th>
                            <th scope="col">"Unit price"</th>
                            <th scope="col" class="num">
                                "Line total"
                            </th>
                            <th scope="col">
                                // Named for the assistive-technology reader; the
                                // column holds one Remove button per row and a
                                // visible "Actions" header only adds noise beside
                                // buttons that already say what they do.
                                <span class="visually-hidden">"Actions"</span>
                            </th>
                        </tr>
                    </thead>
                    <tbody>
                        <For each=move || rows.get() key=|row| row.key let:row>
                            <tr>
                                <td>
                                    <input
                                        type="text"
                                        aria-label=move || {
                                            format!("Item {} description", position_of(row.key) + 1)
                                        }
                                        // The header fields have carried this
                                        // since Feature 4; the line-item fields
                                        // had only the message below them, so a
                                        // rejected row looked like a valid one
                                        // with a note attached.
                                        aria-invalid=move || {
                                            message_for(
                                                    format!(
                                                        "items[{}].description",
                                                        position_of(row.key),
                                                    ),
                                                )
                                                .map(|_| "true")
                                        }
                                        value=move || row.description.get()
                                        prop:value=move || row.description.get()
                                        on:input:target=move |event| {
                                            row.description.set(event.target().value())
                                        }
                                    />
                                    <FieldError message=move || {
                                        message_for(
                                            format!("items[{}].description", position_of(row.key)),
                                        )
                                    } />
                                </td>
                                <td>
                                    <input
                                        type="text"
                                        inputmode="numeric"
                                        aria-label=move || {
                                            format!("Item {} quantity", position_of(row.key) + 1)
                                        }
                                        aria-invalid=move || {
                                            message_for(
                                                    format!("items[{}].quantity", position_of(row.key)),
                                                )
                                                .map(|_| "true")
                                        }
                                        value=move || row.quantity.get()
                                        prop:value=move || row.quantity.get()
                                        on:input:target=move |event| {
                                            row.quantity.set(event.target().value())
                                        }
                                    />
                                    <FieldError message=move || {
                                        message_for(
                                            format!("items[{}].quantity", position_of(row.key)),
                                        )
                                    } />
                                </td>
                                <td>
                                    <input
                                        type="text"
                                        inputmode="decimal"
                                        placeholder="0.00"
                                        aria-label=move || {
                                            format!("Item {} unit price", position_of(row.key) + 1)
                                        }
                                        aria-invalid=move || {
                                            message_for(
                                                    format!(
                                                        "items[{}].unit_price",
                                                        position_of(row.key),
                                                    ),
                                                )
                                                .map(|_| "true")
                                        }
                                        value=move || row.unit_price.get()
                                        prop:value=move || row.unit_price.get()
                                        on:input:target=move |event| {
                                            row.unit_price.set(event.target().value())
                                        }
                                    />
                                    <FieldError message=move || {
                                        message_for(
                                            format!("items[{}].unit_price", position_of(row.key)),
                                        )
                                    } />
                                </td>
                                <td class="num">
                                    {move || match line_total(row) {
                                        Some(cents) => view! { <MoneyText cents=cents /> }.into_any(),
                                        None => view! { <span>"—"</span> }.into_any(),
                                    }}
                                </td>
                                <td>
                                    <button
                                        type="button"
                                        class="secondary outline"
                                        // The last row is disabled rather than
                                        // hidden: a control that vanishes under the
                                        // pointer is worse than one that says no.
                                        disabled=move || rows.get().len() <= 1
                                        on:click=move |_| {
                                            rows.update(|rows| rows.retain(|other| other.key != row.key))
                                        }
                                    >
                                        "Remove"
                                    </button>
                                </td>
                            </tr>
                        </For>
                    </tbody>
                </table>
            </div>

            <div class="editor-total">
                <button type="button" class="secondary" on:click=add_row>
                    "Add line item"
                </button>
                <p>
                    "Order total: "
                    {move || match running_total() {
                        Some(cents) => view! { <MoneyText cents=cents /> }.into_any(),
                        None => {
                            view! { <span>"—"</span> }
                                .into_any()
                        }
                    }}
                </p>
            </div>

            <div class="form-actions">
                <button type="submit" aria-busy=move || pending.get().to_string()>
                    {move || match (pending.get(), editing.is_some()) {
                        (true, _) => "Saving…",
                        (false, true) => "Save changes",
                        (false, false) => "Create order",
                    }}
                </button>
                {editing
                    .map(|order_id| {
                        view! {
                            <A href=format!("/orders/{order_id}") attr:class="secondary outline">
                                "Cancel"
                            </A>
                        }
                    })}
            </div>
        </form>
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// Renders a resource's error the same way on every page.
///
/// A failed read is not a broken page: the shell, the navigation, and the retry
/// route all still work, so the failure is reported in place rather than thrown
/// to the `ErrorBoundary` in `src/app.rs`.
#[component]
fn LoadFailure(error: AppError) -> impl IntoView {
    view! {
        <article class="error-panel" role="alert">
            {error.to_string()}
        </article>
    }
}

/// One link in the status filter.
///
/// A plain `<a>`, not the router's `<A>`. Every link here shares the path `/`
/// and differs only in the query string, and `<A>` decides "am I the current
/// page?" by comparing paths — so it would stamp `aria-current="page"` on all
/// five at once and tell a screen-reader user that they are on five pages.
/// The current one is known here, from the filter the server echoed back, so it
/// is marked here. The router still intercepts the click; nothing reloads.
#[component]
fn FilterLink(href: String, label: &'static str, count: usize, selected: bool) -> impl IntoView {
    view! {
        <a href=href aria-current=selected.then_some("page") class="filter-link">
            {label}
            <span class="filter-count" aria-hidden="true">
                {count}
            </span>
            // The bare number above is decoration to a screen reader, which
            // would otherwise hear "Overdue 3" as a heading and not as a count.
            <span class="visually-hidden">
                {if count == 1 {
                    " (1 order)".to_string()
                } else {
                    format!(" ({count} orders)")
                }}
            </span>
        </a>
    }
}

/// The status filter, driven entirely by the URL.
///
/// The links are the state. There is no local signal holding "the selected
/// filter", because that would be a second answer to a question `?status=`
/// already answers — and a wrong one the moment somebody uses the Back button.
/// Clicking a link changes the query, which changes the resource's source,
/// which refetches. Sharing the URL shares the filter, and the server renders
/// it filtered on first paint.
#[component]
fn StatusFilter(totals: DashboardTotals, current: Option<OrderStatus>) -> impl IntoView {
    view! {
        <nav class="status-filter" aria-label="Filter orders by status">
            <FilterLink
                href="/".to_string()
                label="All"
                count=totals.order_count
                selected=current.is_none()
            />
            {OrderStatus::ALL
                .into_iter()
                .map(|status| {
                    view! {
                        <FilterLink
                            href=format!("/?status={}", status.as_str())
                            label=status.label()
                            count=totals.count_of(status)
                            selected=current == Some(status)
                        />
                    }
                })
                .collect::<Vec<_>>()}
        </nav>
    }
}

/// The four figures above the table.
///
/// Every one of them was added up on the server, by [`summarise_orders`], from
/// the same rows the table below renders. Nothing here sums anything: a browser
/// that computed its own outstanding total would be a second implementation of
/// the amount-due rule, and the one place it could disagree with the server is
/// the one number a person acts on.
#[component]
fn DashboardTiles(totals: DashboardTotals) -> impl IntoView {
    view! {
        <section class="summary" aria-label="Totals across all orders">
            <div class="summary-cell">
                <h2>"Orders"</h2>
                <p class="summary-figure">{totals.order_count}</p>
            </div>
            <div class="summary-cell">
                <h2>"Billed"</h2>
                <p class="summary-figure">
                    <MoneyText cents=totals.total_cents />
                </p>
            </div>
            <div class="summary-cell">
                <h2>"Paid"</h2>
                <p class="summary-figure">
                    <MoneyText cents=totals.paid_cents />
                </p>
            </div>
            // The one emphasised figure on the page. Four cells with equal
            // weight would make the reader choose which number matters; this is
            // the one they came for, and the tint says so before they read it.
            <div class="summary-cell" data-emphasis="true">
                <h2>"Outstanding"</h2>
                <p class="summary-figure">
                    <MoneyText cents=totals.outstanding_cents />
                </p>
            </div>
        </section>
    }
}

/// The dashboard: totals, the status filter, and every order the caller owns.
///
/// The URL is the input. `use_query_map` is a signal, so `?status=` is folded
/// straight into the resource's source — changing the filter changes the key,
/// and the key changing is what refetches. There is no click handler and no
/// local "selected" state to keep in step with the address bar.
///
/// The filter is sent to the server rather than applied to a list already in
/// the browser. That costs a round trip per click and buys the thing that
/// matters: a pasted `/?status=overdue` is server-rendered as the overdue list,
/// by the same code path, instead of arriving as the full list and flickering
/// down to a subset once the WASM bundle has loaded.
#[component]
pub fn DashboardPage() -> impl IntoView {
    let query = use_query_map();
    // Unrecognised and absent are the same answer — see `OrderStatus::parse`.
    let filter = Memo::new(move |_| {
        query
            .read()
            .get("status")
            .and_then(|raw| OrderStatus::parse(&raw))
    });

    let dashboard = Resource::new(move || filter.get(), list_orders);

    view! {
        <Title text="Dashboard - Orders and Settlements" />
        <h1>"Dashboard"</h1>

        // `Transition`, not `Suspense`: switching filters refetches, and a
        // `Suspense` would blank the whole dashboard back to "Loading orders…"
        // on every click. This keeps the previous list on screen until the next
        // one arrives.
        <Transition fallback=|| view! { <p aria-busy="true">"Loading orders…"</p> }>
            {move || Suspend::new(async move {
                match dashboard.await {
                    Err(error) => view! { <LoadFailure error /> }.into_any(),
                    Ok(dashboard) => {
                        let totals = dashboard.totals;
                        let filter = dashboard.filter;
                        let orders = dashboard.orders;
                        view! {
                            <DashboardTiles totals />
                            // Rendered even when the caller owns nothing, so
                            // the counts are visible proof that "no orders" is
                            // the whole picture and not a filter hiding them.
                            <StatusFilter totals current=filter />
                            {if orders.is_empty() {
                                empty_state(totals.order_count == 0, filter).into_any()
                            } else {
                                order_table(orders).into_any()
                            }}
                        }
                            .into_any()
                    }
                }
            })}
        </Transition>
    }
}

/// Nothing to show, and which of the two reasons it is.
///
/// "You have no orders" and "this filter matches none of your orders" are
/// different facts with different next actions — create one, or clear the
/// filter — and collapsing them into one message sends half the readers to the
/// wrong button.
///
/// A function rather than a component: it takes no props a caller could get
/// wrong and has exactly one call site, in [`DashboardPage`].
fn empty_state(no_orders_at_all: bool, filter: Option<OrderStatus>) -> impl IntoView {
    let filtered = filter.filter(|_| !no_orders_at_all);

    view! {
        <article class="empty-state">
            {match filtered {
                None => {
                    view! {
                        <p>"No orders yet."</p>
                        <A href="/orders/new">"Create the first one"</A>
                    }
                        .into_any()
                }
                Some(status) => {
                    view! {
                        <p>{format!("No {} orders.", status.label().to_lowercase())}</p>
                        <a href="/">"Show every order"</a>
                    }
                        .into_any()
                }
            }}
        </article>
    }
}

/// The order table: one row per order, soonest due first.
///
/// Every column is a value the server sent down finished. `amount_due_cents`
/// is the one arithmetic here and it is a subtraction of two server-derived
/// figures on the same DTO, not a rule the browser applies.
fn order_table(orders: Vec<OrderSummary>) -> impl IntoView {
    view! {
        // Seven columns will not fit a phone at a size anybody can read, and
        // dropping columns would drop money. The table scrolls inside this box
        // instead, so the page itself never scrolls sideways.
        <div class="table-scroll">
            <table class="order-table">
                <thead>
                    <tr>
                        <th scope="col">"Customer"</th>
                        <th scope="col">"Due"</th>
                        <th scope="col">"Status"</th>
                        <th scope="col" class="num">
                            "Items"
                        </th>
                        <th scope="col" class="num">
                            "Total"
                        </th>
                        <th scope="col" class="num">
                            "Paid"
                        </th>
                        <th scope="col" class="num">
                            "Due now"
                        </th>
                    </tr>
                </thead>
                <tbody>
                    {orders
                        .into_iter()
                        .map(|order| {
                            let href = format!("/orders/{}", order.id);
                            let due = order.due_date.to_string();
                            let amount_due = order.amount_due_cents();
                            view! {
                                // Read by the stylesheet, which rules an
                                // overdue row down its leading edge. The badge
                                // in the status column says the same thing in
                                // words, so the rule adds a signal rather than
                                // being the only one.
                                <tr data-status=order.status.as_str()>
                                    <td>
                                        // The customer name is the link, so the
                                        // target is named rather than being a
                                        // bare "view" a screen reader reads out
                                        // of context.
                                        <A href=href>{order.customer}</A>
                                    </td>
                                    <td class="date">{due}</td>
                                    <td>
                                        <StatusBadge status=order.status.as_str() />
                                    </td>
                                    <td class="num">{order.item_count}</td>
                                    <td class="num">
                                        <MoneyText cents=order.total_cents />
                                    </td>
                                    <td class="num">
                                        <MoneyText cents=order.paid_cents />
                                    </td>
                                    <td class="num">
                                        <MoneyText cents=amount_due />
                                    </td>
                                </tr>
                            }
                        })
                        .collect::<Vec<_>>()}
                </tbody>
            </table>
        </div>
    }
}

/// One order, its line items, and the actions available on it.
#[component]
pub fn OrderDetailPage() -> impl IntoView {
    let order_id = use_order_id();
    let delete = ServerAction::<DeleteOrder>::new();
    let record = ServerAction::<RecordPayment>::new();
    // A recorded payment changes the paid amount, the amount due, the derived
    // status, and whether the order can still be edited. Rather than patch any
    // of those in the browser, the whole order is re-read: `version()` bumps on
    // every completed payment, which changes this source and refetches. The
    // page's numbers are the server's numbers or they are nothing.
    let order = Resource::new(
        move || (order_id.get(), record.version().get()),
        |(order_id, _)| async move {
            match order_id {
                Some(order_id) => get_order(order_id).await,
                None => Err(AppError::NotFound),
            }
        },
    );

    // A completed delete leaves this route pointing at a row that no longer
    // exists, so staying here would show the caller "that order does not
    // exist" about the order they just deleted. Only success navigates; a
    // refusal leaves the page as it is, with the reason rendered below.
    //
    // `replace`, so the browser's Back button does not return to the dead
    // detail page.
    let navigate = use_navigate();
    Effect::new(move |_| {
        if let Some(Ok(())) = delete.value().get() {
            navigate(
                "/",
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            );
        }
    });

    view! {
        <Title text="Order - Orders and Settlements" />

        <Transition fallback=|| view! { <p aria-busy="true">"Loading order…"</p> }>
            {move || Suspend::new(async move {
                match order.await {
                    Err(error) => view! { <LoadFailure error /> }.into_any(),
                    Ok(order) => view! { <OrderDetailView order delete record /> }.into_any(),
                }
            })}
        </Transition>
    }
}

/// The loaded order. Split out so the page above holds only the loading and
/// failure paths, and this holds only the rendering.
#[component]
fn OrderDetailView(
    order: OrderDetail,
    delete: ServerAction<DeleteOrder>,
    record: ServerAction<RecordPayment>,
) -> impl IntoView {
    let confirming = RwSignal::new(false);
    let order_id = order.id;
    let editable = order.editable;
    let today = order.today;
    let paid_cents = order.paid_cents;
    // Read before the move below, because taking `payments` out of `order`
    // leaves it partially moved and it can no longer be borrowed as a whole.
    let amount_due_cents = order.amount_due_cents();
    let payments = order.payments;
    // Computed by the same function the transaction uses, so the hint on the
    // form and the limit the server enforces are one rule, not two.
    let maximum_cents = calculate_maximum_payment_cents(order.total_cents, order.paid_cents);

    view! {
        // The status belongs in the heading block, not in a paragraph below it:
        // it is a property of the record, and on its own line it read as the
        // page's opening sentence.
        <div class="record-header">
            <div>
                <h1>{order.customer.clone()}</h1>
                <p class="record-meta">
                    "Due " <span class="date">{order.due_date.to_string()}</span>
                </p>
            </div>
            <StatusBadge status=order.status.as_str() />
        </div>

        <div class="table-scroll">
            <table class="order-table">
                <thead>
                    <tr>
                        <th scope="col">"Description"</th>
                        <th scope="col" class="num">
                            "Quantity"
                        </th>
                        <th scope="col" class="num">
                            "Unit price"
                        </th>
                        <th scope="col" class="num">
                            "Line total"
                        </th>
                    </tr>
                </thead>
                <tbody>
                    {order
                        .items
                        .iter()
                        .map(|line| {
                            view! {
                                <tr>
                                    <td>{line.description.clone()}</td>
                                    <td class="num">{line.quantity}</td>
                                    <td class="num">
                                        <MoneyText cents=line.unit_price_cents />
                                    </td>
                                    <td class="num">
                                        <MoneyText cents=line.line_total_cents />
                                    </td>
                                </tr>
                            }
                        })
                        .collect::<Vec<_>>()}
                </tbody>
            </table>
        </div>

        <dl class="totals">
            <dt>"Order total"</dt>
            <dd>
                <MoneyText cents=order.total_cents />
            </dd>
            <dt>"Paid"</dt>
            <dd>
                <MoneyText cents=order.paid_cents />
            </dd>
            <dt>"Amount due"</dt>
            <dd>
                <MoneyText cents=amount_due_cents />
            </dd>
        </dl>

        // Immediately below the totals it reconciles, and above the form that
        // adds to it, so the page reads as: what is owed, what has been paid,
        // and what you can do about the difference.
        <PaymentHistory payments paid_cents />

        {move || {
            (!editable)
                .then(|| {
                    view! {
                        <p class="notice">
                            "This order has payments recorded against it and can no longer be changed."
                        </p>
                    }
                })
        }}

        // Hidden once nothing is owed, rather than shown and refused. The
        // server refuses it either way — `record_payment` re-reads the balance
        // behind the row lock — so this is about not offering a dead control.
        {move || {
            if maximum_cents > 0 {
                view! { <PaymentForm order_id maximum_cents today record /> }.into_any()
            } else {
                view! { <p class="notice">"This order is paid in full."</p> }.into_any()
            }
        }}

        {move || {
            editable
                .then(|| {
                    view! {
                        <div class="form-actions">
                            // `role="button"` so it sits beside Delete as an
                            // equal action rather than as body-text link. It is
                            // still an <a>: it navigates, and middle-click and
                            // "open in new tab" keep working.
                            <A
                                href=format!("/orders/{order_id}/edit")
                                attr:class="secondary"
                                attr:role="button"
                            >
                                "Edit"
                            </A>

                            // Two steps, because a delete cannot be undone and
                            // the cascade takes the line items with it. The
                            // confirmation only exists once the page has
                            // hydrated; the delete itself is a server function
                            // that re-checks ownership and payments, so the
                            // extra click is a courtesy, not the safeguard.
                            <Show
                                when=move || confirming.get()
                                fallback=move || {
                                    view! {
                                        <button
                                            type="button"
                                            class="danger outline"
                                            on:click=move |_| confirming.set(true)
                                        >
                                            "Delete"
                                        </button>
                                    }
                                }
                            >
                                <button
                                    type="button"
                                    class="danger"
                                    aria-busy=move || delete.pending().get().to_string()
                                    on:click=move |_| {
                                        delete.dispatch(DeleteOrder { id: order_id });
                                    }
                                >
                                    "Delete permanently"
                                </button>
                                <button
                                    type="button"
                                    class="secondary outline"
                                    on:click=move |_| confirming.set(false)
                                >
                                    "Keep it"
                                </button>
                            </Show>
                        </div>
                    }
                })
        }}

        {move || {
            match delete.value().get() {
                Some(Err(error)) => Some(view! { <LoadFailure error /> }),
                _ => None,
            }
        }}
    }
}

/// The edit page: loads the order, then hands it to [`OrderEditor`].
#[component]
pub fn EditOrderPage() -> impl IntoView {
    let order_id = use_order_id();
    let order = Resource::new(
        move || order_id.get(),
        |order_id| async move {
            match order_id {
                Some(order_id) => get_order(order_id).await,
                None => Err(AppError::NotFound),
            }
        },
    );

    view! {
        <Title text="Edit order - Orders and Settlements" />
        <h1>"Edit order"</h1>

        <Transition fallback=|| view! { <p aria-busy="true">"Loading order…"</p> }>
            {move || Suspend::new(async move {
                match order.await {
                    Err(error) => view! { <LoadFailure error /> }.into_any(),
                    // The server refuses the update as well; this only avoids
                    // presenting a form that cannot be submitted.
                    Ok(order) if !order.editable => {
                        view! { <LoadFailure error=AppError::OrderHasPayments /> }.into_any()
                    }
                    Ok(order) => view! { <OrderEditor existing=order /> }.into_any(),
                }
            })}
        </Transition>
    }
}

/// The `:id` segment of the current route, parsed.
///
/// `None` for a segment that is not a UUID, which the pages above turn into the
/// same not-found answer the server gives for an id that does not exist. A
/// typo in the address bar is not a different kind of failure.
fn use_order_id() -> Memo<Option<Uuid>> {
    let params = use_params_map();

    Memo::new(move |_| {
        params
            .read()
            .get("id")
            .and_then(|raw| Uuid::parse_str(&raw).ok())
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_money_to_cents, parse_quantity};

    #[test]
    fn parses_plain_amounts() {
        assert_eq!(parse_money_to_cents("0"), Ok(0));
        assert_eq!(parse_money_to_cents("1234.50"), Ok(123_450));
        assert_eq!(parse_money_to_cents("500"), Ok(50_000));
    }

    #[test]
    fn parses_what_people_actually_type() {
        assert_eq!(parse_money_to_cents(" $1,234.50 "), Ok(123_450));
        assert_eq!(parse_money_to_cents("$ 1234.5"), Ok(123_450));
        assert_eq!(parse_money_to_cents(".50"), Ok(50));
        assert_eq!(parse_money_to_cents("5."), Ok(500));
    }

    #[test]
    fn rejects_amounts_that_would_lose_a_cent() {
        assert!(parse_money_to_cents("10.999").is_err());
        assert!(parse_money_to_cents("1.2.3").is_err());
        assert!(parse_money_to_cents("-5").is_err());
        assert!(parse_money_to_cents("$-5").is_err());
        assert!(parse_money_to_cents("abc").is_err());
        assert!(parse_money_to_cents("").is_err());
        assert!(parse_money_to_cents("$").is_err());
    }

    #[test]
    fn rejects_amounts_beyond_i64() {
        assert!(parse_money_to_cents("99999999999999999999").is_err());
        // Representable as dollars, not as cents: the ×100 overflows.
        assert!(parse_money_to_cents("9223372036854775807").is_err());
    }

    #[test]
    fn parses_quantities() {
        assert_eq!(parse_quantity("1"), Ok(1));
        assert_eq!(parse_quantity(" 42 "), Ok(42));
        assert_eq!(parse_quantity("1,000"), Ok(1_000));
    }

    #[test]
    fn rejects_unusable_quantities() {
        assert!(parse_quantity("0").is_err());
        assert!(parse_quantity("1.5").is_err());
        assert!(parse_quantity("-1").is_err());
        assert!(parse_quantity("").is_err());
        assert!(parse_quantity("two").is_err());
    }
}
