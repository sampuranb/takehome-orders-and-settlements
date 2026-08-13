# Orders and Settlements

Full-stack Rust application for creating orders with line items, recording full
or partial payments against them, and viewing a dashboard of derived status and
amounts due.

> **Status: in progress.** The foundation, authentication, and the full order
> lifecycle are complete and verified. Payments, the dashboard, and the REST API
> are not implemented yet. See [Current status](#current-status).

## Stack

One Cargo package producing one deployable binary. `cargo-leptos` builds the
native SSR server and the browser WASM bundle from the same source, so there is
no separate frontend application.

| Concern | Choice |
| --- | --- |
| Framework | Leptos 0.8 SSR with hydration |
| HTTP | Axum via `leptos_axum` |
| Database | PostgreSQL through SQLx, migrations embedded in the binary |
| Authentication | External Better Auth service (no local session store) |
| Money | Checked `i64` cents |
| Observability | `tracing`, `tower-http` trace and compression layers |

Server-only code sits behind `#[cfg(feature = "ssr")]` so it never reaches the
browser bundle. This is verified: the WASM dependency tree contains no
`sqlx`, `axum`, `tokio`, `tower-http`, or `leptos_axum`.

## Prerequisites

- Rust stable 1.94 or newer (SQLx 0.9 sets this minimum)
- The `wasm32-unknown-unknown` target
- `cargo-leptos`
- PostgreSQL 15 or newer
- The shared Better Auth service, reachable at `BETTER_AUTH_URL`

```bash
rustup target add wasm32-unknown-unknown
cargo install --locked cargo-leptos
```

## Running locally

Create the application database. It is separate from the database the shared
Better Auth service uses for users and sessions.

```bash
createdb orders_settlements
```

Copy the environment template and adjust it. `cargo-leptos` reads `.env`
automatically and passes it to the server process.

```bash
cp .env.example .env
```

Start the development server.

```bash
cargo leptos serve
```

Migrations run automatically at startup, so no separate migrate step is needed.

Then browse to **<http://localhost:5174>** — `localhost`, not `127.0.0.1`. The
two are different origins to both the browser and Better Auth, and the shared
auth service only trusts `http://localhost:5174`. Signing out from any other
origin is refused with `403`.

## Configuration

| Variable | Required | Purpose |
| --- | --- | --- |
| `DATABASE_URL` | yes | `postgres://` or `postgresql://` connection string |
| `BETTER_AUTH_URL` | yes | Absolute base URL of the shared auth service |
| `LEPTOS_OUTPUT_NAME` | yes | Set by cargo-leptos; a bare binary must set it or `/pkg/*` URLs break |
| `LEPTOS_SITE_ADDR` | no | Listen address |
| `RUST_LOG` | no | Tracing filter |

Configuration is validated before the database is contacted, and the database is
contacted and migrated before the listener is bound. A misconfigured process
exits with a specific message and status 1 rather than accepting requests it
cannot serve.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/health` | Readiness probe; `200` when PostgreSQL answers, `503` when it does not |
| `GET` | `/` | Dashboard |
| `GET` | `/orders` | Order list |
| `GET` | `/orders/new` | Order editor |
| `GET` | `/orders/:id` | Order detail |
| `GET` | `/auth` | Sign in and sign up |
| `POST` | `/api/sign_up*` | Server function: register, then sign in |
| `POST` | `/api/sign_in*` | Server function: exchange credentials for a session cookie |
| `POST` | `/api/sign_out*` | Server function: revoke the session |
| `POST` | `/api/current_user*` | Server function: resolve the caller's identity |
| `POST` | `/api/create_order*` | Server function: validate and persist an order (JSON body) |
| `POST` | `/api/update_order*` | Server function: revalidate and replace an order (JSON body) |
| `POST` | `/api/delete_order*` | Server function: delete an order and its line items |
| `POST` | `/api/list_orders*` | Server function: the caller's orders with derived status |
| `POST` | `/api/get_order*` | Server function: one order with its line items |
| `POST` | `/api/record_payment*` | Server function: record a payment against an order (JSON body) |
| `GET` | `/pkg/*` | Hydration bundle and stylesheet |

Application pages are server-rendered and then hydrated. Unmatched paths return
`404` with the same shell. `/auth` is the only public page; everything else is
gated. The REST API for orders and payments arrives with a later feature.

Server-function paths carry a generated suffix, so the exact URLs are read from
the rendered `<form action>` rather than hard-coded.

## Authentication

The shared Better Auth service is the only authority on identity. This
application stores no sessions, hashes no passwords, and mints no tokens. It
forwards the browser's cookie to Better Auth on each request and re-emits every
`Set-Cookie` header it gets back, which is what scopes the session cookie to
this origin without CORS. Better Auth's opaque `user.id` is the tenant key that
orders will be filtered by.

Three parts of the contract are worth knowing before reading `src/auth.rs`:

| Observation | Better Auth's answer | Handled as |
| --- | --- | --- |
| Missing, expired, or forged session | `200` with a JSON `null` body | Signed out (`401` when a page requires a user) |
| Auth service unreachable or `5xx` | connection error / `5xx` | `503`, never "signed out" |
| Sign-out without a trusted `Origin` | `403 MISSING_OR_NULL_ORIGIN` | `403`; the browser's own origin is validated and forwarded |

Sign-out returns **three** `Set-Cookie` headers, each re-emitted individually.

State-changing server functions run their own same-origin check in Rust before
any credential leaves the process, independently of Better Auth's trusted-origin
list. The session cookie is `SameSite=Lax`, which still permits a top-level
cross-site form `POST`, so a cookie alone is never treated as authority to act.

No cookie, token, or password is ever logged.

## Money

Every amount is an `i64` count of cents, from the moment it is parsed to the
`BIGINT` column it lands in. No floating point, no `NUMERIC`, and no rounding
step — there is nothing to round. Postgres `BIGINT` and Rust `i64` have exactly
the same range, so a value that survives validation is representable in the
database and back again.

The money field accepts what a person types: `1234.50`, `$1,234.50`, `1234`,
`.50`, `1234.5`. It refuses negatives, three or more decimal places, and
anything else non-numeric, because silently dropping a third digit is how a cent
goes missing.

Every multiplication and addition is checked. Overflow is not a realistic
invoice, but the alternative to a checked operation is a total that wraps
negative, and a billing system that turns positive inputs into a negative total
is worse than one that refuses the input.

`src/orders.rs` compiles the parsers and the arithmetic for **both** targets, so
the running total in the browser is a preview of the server's answer rather than
a second implementation of it. The browser's number is never submitted:
`create_order` re-parses the raw strings and recomputes everything server-side.

Validation reports every problem in one response, keyed by field
(`customer`, `due_date`, `items[2].quantity`), so a form does not make the user
discover the rest by trial. An order and its line items are written in one
transaction: the stored `total_cents` describes rows in another table, which no
`CHECK` constraint can see, so the transaction is the only thing holding that
invariant.

## Status

An order's status is never stored. It is derived on every read from three
things: the order total, how much has been paid against it, and today's date.

| Condition | Status |
| --- | --- |
| Paid at least the total | Paid |
| Otherwise, past its due date | Overdue |
| Otherwise, some payment recorded | Partially paid |
| Otherwise | Pending |

The order of those tests is the specification, not an implementation detail.
Paid outranks overdue, so an invoice settled late is finished rather than
outstanding; overdue outranks partially paid, so money still owed past the date
is not softened by a part payment.

Storing the status would create a second source of truth that goes stale on its
own: an order becomes overdue because a day passed, with nothing writing to the
database at all. There is no event to hang an update on, so there is nothing to
store.

"Today" is UTC. A due date is a calendar date with no time zone attached, and
this application stores no time zone for a user to be correct relative to.
`derive_order_status` takes today as a parameter rather than reading a clock, so
every branch is reachable from a test without waiting for a date to arrive.

## Payments

A payment is a row in `payments`, never an edit to the order. The order keeps
its total; how much has been paid is the sum of its payments, and the amount due
is the difference. Nothing about an order's settlement is stored on the order.

The one rule that cannot be expressed as a constraint is that payments must not
exceed the total. A `CHECK` sees a single row, and this invariant spans every
row for the order, so it is held by a transaction instead. `record_payment_transaction`
in `src/payments.rs` does exactly four things, in this order:

1. `SELECT ... FOR UPDATE` the order row (`lock_owned_order`, which also scopes
   it to the owner, so a stranger's payment fails as `404` rather than `403`)
2. read the order total and the sum of its existing payments **on that same
   connection**, inside the same transaction
3. compare, and refuse with `409 PAYMENT_EXCEEDS_AMOUNT_DUE` — carrying the
   largest payment that would still be accepted, so "too much" does not make the
   user guess
4. insert, then commit

The lock is on `orders`, but the row it protects is in `payments`. That is the
point: two concurrent final payments have no row in common to contend over, so
the order row stands in as the lock for the whole set. Under READ COMMITTED the
second transaction blocks at step 1, and when it proceeds, its next statement
sees the first payment. Both orderings end with the same total.

`tests/payments.rs` asserts this rather than describing it: two simultaneous
final payments on a multi-threaded runtime produce exactly one success and one
refusal, and six simultaneous $250 payments against a $1,000 order produce
exactly four acceptances.

Once an order has any payment, it can no longer be edited or deleted. That check
runs inside the same transaction, after the same lock, so it cannot be raced
either.

The order detail page is one read. `find_order_for_user` issues three statements
— the order, its line items in saved position order, and its payments newest
first — and returns them on a single DTO. Three statements rather than a join,
because joining items to payments multiplies them: a four-line order with three
payments would come back as twelve rows describing seven facts. They are not in
a transaction and do not need to be; nothing here decides anything, and the
write path re-reads what it needs behind the row lock rather than trusting a
number a page produced.

The history and the totals travel together for the same reason they are read
together: a page that fetched them separately could print a list of payments
that does not sum to the "Paid" figure above it. The last row of the history is
that sum, so the reconciliation is visible rather than left to the reader.

Payments on the same day are ordered by id, which is UUID v7 — so the tiebreak
means "recorded later", not an arbitrary byte comparison, and two reads of the
same page cannot shuffle the rows.

## Third-party assets

`style/main.css` begins with [Pico CSS](https://picocss.com) v2.1.1, vendored
verbatim under the MIT licence with its copyright banner intact. It is vendored
rather than fetched from a CDN so the application makes no third-party requests.

Note that the published `/pkg/orders.css` does **not** carry that banner:
cargo-leptos processes the stylesheet with Lightning CSS, which strips every
comment. The notice is retained here and in the source file.

## Tests

```bash
set -a; . ./.env; set +a
cargo fmt --check
cargo clippy --features ssr --no-default-features --all-targets -- -D warnings
cargo test --features ssr --no-default-features
cargo leptos build
```

`DATABASE_URL` must be set, and the database must be running: `tests/orders.rs`
exercises real PostgreSQL behaviour — the `CHECK` constraints, the cascade, and
the all-or-nothing write — and a fake would only re-assert what the test file
already believes. Each test writes under an owner id no other test uses, so they
run in parallel and clean up only their own rows.

`tests/payments.rs` needs the real database for the same reason, and then some:
two of its cases run on a multi-threaded runtime and issue genuinely concurrent
payments, which is the only way to observe whether the row lock does what the
transaction claims. A mocked pool would prove nothing about `FOR UPDATE`.

`tests/auth.rs` goes the other way and drives the Better Auth contract against
`wiremock` rather than the live service, because the cases that matter most are
the ones a healthy Node process will not produce on demand: an outage, a refused
connection, a rotated session cookie, a malformed body, and a rejected origin.

The foundation, the auth flow, and order creation were also verified against the
live service, a real browser, and the database: sign-up, sign-in, sign-out,
cookie re-emission on this origin, revoked and forged sessions, the same-origin
gate, hydrated navigation without a full page load, a multi-row order editor
with a live total, and the stored cent values read back out of Postgres.

The order functions were additionally exercised with `curl`, which is the only
way to see the status line rather than the decoded body: `403 FORBIDDEN_ORIGIN`
with a missing or foreign `Origin`, `401 UNAUTHENTICATED` with no session,
`400 VALIDATION_FAILED` carrying every field message for a bad submission, and
`404 NOT_FOUND` for an order id that was deleted.

The full lifecycle was walked in a browser as well: create, list, open, edit
(the form arrives prefilled, a row is removed, the running total follows),
save, then a two-step delete that redirects to an empty list. An order dated in
the past shows **Overdue** without anything having been written to it.

The settlement flow was walked in a browser too, on a $1,000 order: $400 leaves
it **Partially paid** with $600 due and replaces the edit and delete controls
with the reason they are gone; $700 is refused in place with "The most you can
pay is $600.00."; $600 settles it to **Paid**, $0.00 due, with the payment form
replaced by a statement that nothing is owed.

The history was watched appearing rather than only reloaded into: an order with
no payments shows no history section at all, the first payment makes it appear
in place without a page load, and a second payment on the same day is inserted
above the first with the footer moving to "2 payments" and the new sum.

## Current status

Implemented:

- Cargo workspace, feature separation, and cargo-leptos configuration
- Strict startup configuration with fail-fast validation
- PostgreSQL pool and embedded migrations applied at boot
- Database-backed health endpoint with a distinct unavailable response
- Server-rendered shell with working browser hydration
- Structured tracing, response compression, and graceful shutdown
- Sign-up, sign-in, and sign-out against the shared Better Auth service
- Session cookie re-emission, including rotated and clearing cookies
- Rust-side same-origin check on every state-changing server function
- Gated routes and a server-side `require_user` gate for later features
- Orders and line items: schema, integer-cent money, validation, and creation
- A dynamic line-item editor with a live total computed by the server's own code
- Order list, detail, edit, and delete, each scoped to the owner
- Derived status: pending, partially paid, paid, and overdue
- Payments and amount due, with overpayment refused under concurrency
- Orders locked against edit and delete once money is recorded against them
- One authoritative detail view: items, payment history, totals, and actions

Not yet implemented:

- Dashboard and status filter; REST API
- Deployment and the deployed URL

## License

Not licensed for reuse; provided as a take-home submission.
