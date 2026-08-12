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
use leptos_router::hooks::use_navigate;
use serde::{Deserialize, Serialize};

use chrono::NaiveDate;
use uuid::Uuid;

use crate::app::{FieldError, MoneyText};
use crate::error::{AppError, AppResult};

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
    use super::{validate_create_order, NewOrderInput, ValidatedOrder};
    use crate::error::{AppError, AppResult};
    use leptos::prelude::use_context;
    use sqlx::{PgPool, QueryBuilder};
    use uuid::Uuid;

    /// The connection pool `main.rs` created at startup.
    ///
    /// One pool per process, provided into context by
    /// `leptos_routes_with_context`. A server function that built its own would
    /// double the connection count against a database whose limit is the reason
    /// pooling exists.
    pub fn pool() -> AppResult<PgPool> {
        use_context::<PgPool>().ok_or_else(|| {
            // Only reachable if the router stopped providing the pool, which is
            // a wiring defect, not a user-visible condition.
            tracing::error!("no database pool in context");
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
        // `validate_create_order` caps this at MAX_ITEMS, but `insert_order` is
        // callable on its own, so the cast that would otherwise be a silent
        // truncation is checked here rather than assumed upstream.
        let positions = (0..order.items.len())
            .map(|position| {
                i32::try_from(position).map_err(|_| {
                    tracing::error!(count = order.items.len(), "order has too many line items");
                    AppError::Internal
                })
            })
            .collect::<AppResult<Vec<i32>>>()?;

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

        // One multi-row INSERT rather than a statement per item: the round trips
        // are what cost, not the rows.
        if !order.items.is_empty() {
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
        }

        transaction.commit().await?;

        Ok(order_id)
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

// ---------------------------------------------------------------------------
// Creation form
// ---------------------------------------------------------------------------

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

    fn to_input(self) -> OrderItemInput {
        OrderItemInput {
            description: self.description.get_untracked(),
            quantity: self.quantity.get_untracked(),
            unit_price: self.unit_price.get_untracked(),
        }
    }
}

/// The new-order form: customer, due date, and a variable number of line items.
///
/// The running total is computed in the browser from the same parsers the
/// server uses, so it is a preview of the server's answer rather than a second
/// opinion. It is never submitted: [`create_order`] recomputes everything from
/// the raw strings.
#[component]
pub fn OrderEditor() -> impl IntoView {
    let create = ServerAction::<CreateOrder>::new();
    let navigate = use_navigate();

    let customer = RwSignal::new(String::new());
    let due_date = RwSignal::new(String::new());
    let rows = RwSignal::new(vec![ItemRow::new(0)]);
    let next_key = RwSignal::new(1_usize);
    // Set by any keystroke in the form, cleared on submit. See `failure`.
    let edited = RwSignal::new(false);

    // Effects do not run during SSR, so this is browser-only by construction.
    Effect::new(move |_| {
        if let Some(Ok(order_id)) = create.value().get() {
            navigate(&format!("/orders/{order_id}"), Default::default());
        }
    });

    // The server's verdict is about the values that were submitted, and the
    // action holds on to it until the next dispatch. Once the user changes
    // anything, those values no longer exist — leaving "Enter the customer's
    // name." under a field that now has a name in it is worse than showing
    // nothing, so the whole verdict is withdrawn on the first keystroke.
    let failure = Signal::derive(move || match create.value().get() {
        Some(Err(error)) if !edited.get() => Some(error),
        _ => None,
    });

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

        create.dispatch(CreateOrder {
            input: NewOrderInput {
                customer: customer.get_untracked(),
                due_date: due_date.get_untracked(),
                items: rows
                    .get_untracked()
                    .into_iter()
                    .map(ItemRow::to_input)
                    .collect(),
            },
        });
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
                        prop:value=move || due_date.get()
                        on:input:target=move |event| due_date.set(event.target().value())
                    />
                    <FieldError message=move || message_for("due_date".to_string()) />
                </label>
            </div>

            <h2>"Line items"</h2>
            <FieldError message=move || message_for("items".to_string()) />

            <table>
                <thead>
                    <tr>
                        <th scope="col">"Description"</th>
                        <th scope="col">"Quantity"</th>
                        <th scope="col">"Unit price"</th>
                        <th scope="col">"Line total"</th>
                        <th scope="col">"Actions"</th>
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
                            <td>
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

            <button type="submit" aria-busy=move || create.pending().get().to_string()>
                {move || if create.pending().get() { "Saving…" } else { "Create order" }}
            </button>
        </form>
    }
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
