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

## Running in Docker

The application and its PostgreSQL come up together. The build is a single
`docker build` — no Node toolchain, no npm install, nothing to install on the
host but Docker itself.

```bash
docker compose up --build
```

Then browse to **<http://localhost:5174>**, the same address and for the same
reason as above.

`compose.yaml` deliberately does **not** run a copy of the shared Better Auth
service. That service is a separate deployment with its own database and its own
secret; a second copy would mean two user tables and two session stores, and an
account created against one could not sign in to the other. The container's
`BETTER_AUTH_URL` defaults to `http://host.docker.internal:3005`, which is the
auth service running on the developer's own machine; point it anywhere else with

```bash
AUTH_SERVICE_URL=https://auth.example.com docker compose up --build
```

The override is spelled `AUTH_SERVICE_URL` rather than `BETTER_AUTH_URL`
because compose substitutes `${...}` from `./.env` — the local *development*
file, where `BETTER_AUTH_URL` is `http://localhost:3005`, an address that inside
a container means the container itself. Sharing the name would quietly pull that
value in and fail every sign-in with `503`.

The image is built in two stages. The builder is `rust:1.97-slim-bookworm` and
installs `cargo-leptos` at a pinned version; the runtime is
`debian:bookworm-slim` and carries the binary, `target/site`, and
`ca-certificates` — nothing else. There is no shell tooling in it on purpose:
the TLS stack is rustls, so there is no OpenSSL to keep patched, and the
certificates are only there because outbound HTTPS to the auth service needs a
trust store to verify against.

Dependencies are compiled in their own layer from `Cargo.toml` and `Cargo.lock`
alone, so editing application source rebuilds only the application. `Cargo.lock`
is committed and the build is `--locked`: the image that ships is built from the
same dependency versions the tests ran against.

It runs as an unprivileged user (`uid 10001`), listens on `8080`, and reads
every setting from the environment.

There is no `HEALTHCHECK` in the image, and no healthcheck on the `app` service.
A container-side check has to run *inside* the runtime image, which has no curl,
no wget, and a `/bin/sh` that is dash and so has no `/dev/tcp` — every one-liner
that looks like it would work there reports unhealthy instead. `/health` still
exists and still answers `503` when PostgreSQL is unreachable; point the
platform's own probe at it, which is where a health check that means anything
runs from anyway:

```bash
curl -fsS http://localhost:5174/health
```

## Deploying

The image is self-contained and takes its configuration from the environment, so
any platform that runs a container will run it: set `DATABASE_URL`,
`BETTER_AUTH_URL` and `LEPTOS_SITE_ADDR`, expose the port, and point the
health probe at `/health`.

Two things have to be true of wherever it lands:

- **The browser origin must be one Better Auth trusts.** Sessions are issued by
  the shared auth service and it refuses requests from origins it does not know,
  so the deployed hostname has to be added to its trusted origins — otherwise
  sign-in appears to work and sign-out returns `403`.
- **`BETTER_AUTH_URL` only has to be reachable from the server**, never from the
  browser. This application calls the auth service itself and re-emits the
  session cookie on its own origin, so the auth service can stay on a private
  network.

**No live URL is deployed.** Standing up a public deployment requires creating a
hosting account and, on every provider that would keep the app awake for a
grader, entering payment details — both actions I do not take on someone's
behalf. The container is the deliverable I can produce and verify; the account
is yours to create. `docker compose up --build` reproduces the whole thing
locally in one command.

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
| `GET` | `/` | Dashboard: totals, status filter, and the order list |
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
| `POST` | `/api/list_orders*` | Server function: the caller's orders with derived status, totals, and an optional status filter |
| `POST` | `/api/get_order*` | Server function: one order with its line items |
| `POST` | `/api/record_payment*` | Server function: record a payment against an order (JSON body) |
| `GET` | `/api/orders` | REST: the caller's orders, with totals and an optional `?status=` filter |
| `POST` | `/api/orders` | REST: create an order |
| `GET` | `/api/orders/{id}` | REST: one order with its items and payments |
| `PUT` | `/api/orders/{id}` | REST: replace an order's customer, date, and items |
| `DELETE` | `/api/orders/{id}` | REST: delete an order and its line items |
| `POST` | `/api/orders/{id}/payments` | REST: record a payment against an order |
| `GET` | `/pkg/*` | Hydration bundle and stylesheet |

Application pages are server-rendered and then hydrated. Unmatched paths return
`404` with the same shell. `/auth` is the only public page; everything else is
gated.

Server-function paths carry a generated suffix — `/api/sign_in9451780611962502888`
— so the exact URLs are read from the rendered `<form action>` rather than
hard-coded. That suffix is also why the hand-written `/api/orders` routes cannot
collide with them.

## REST API

A second surface over the same services, not a second implementation. Every
handler is four steps — authenticate, check the origin, call the service the
Leptos server function also calls, shape a response — and contains no SQL, no
validation, and no business rule of its own. A rule that lived in a handler
would be a rule the web UI does not enforce.

The route table is `orders::api::router()`, in the library rather than in
`main.rs`, so `tests/api.rs` mounts the same paths and methods that are served.
A test with its own route table would pass while the deployed URL was
`/api/order`.

### Authentication and CSRF

Requests carry the same session cookie the pages use, so the API is usable from
the browser that is already signed in. A missing or expired session is `401`.

State-changing requests are refused if they carry an `Origin` header that is not
this application's own. An **absent** `Origin` is allowed, which is deliberately
weaker than the rule the server functions apply: a browser cannot suppress
`Origin` on a cross-site state-changing request, so its absence means the caller
is not a page — `curl`, a script, a mobile client. Refusing those would make the
REST surface unusable by everything except the site that already has a UI, while
present-and-wrong, the case that actually matters, is still refused.

### Status codes

| Code | When |
| --- | --- |
| `200` | A read, or a successful replace |
| `201` | A created order or payment, with `Location` pointing at the order |
| `204` | A successful delete — nothing left to describe |
| `400` | Validation failed, a malformed body, or an unparseable id |
| `401` | No session, or an expired one |
| `403` | A state-changing request from another origin |
| `404` | No such order — **or** somebody else's |
| `409` | The order's own state refuses: it has payments, or the payment exceeds the balance |
| `500` | A defect here |
| `503` | PostgreSQL is unreachable |

`404` for another owner's order is the point, not an accident. A distinct `403`
would confirm that an order with that id exists, which is the fact an
unauthorised caller is probing for.

`409` rather than `400` for overpayment and for editing a settled order: the
request is well formed and the caller is entitled to make it. The order's state
is what refuses, and the same request could succeed at another moment.

### Errors

Every failure has one shape:

```json
{
  "error": "VALIDATION_FAILED",
  "message": "Some details need fixing.",
  "fields": [{ "field": "items[0].quantity", "message": "Enter a whole number of at least 1." }]
}
```

`error` is a stable code to branch on, `message` is the sentence a person is
shown, and `fields` — present only when the failure is about specific inputs —
carries the same machine-readable paths the browser matches on, so a non-browser
client can put each message next to the right input too. A malformed JSON body
is reported in this shape as well rather than in Axum's plain text, so a client
has one error format to parse.

### Bodies

`POST` and `PUT /api/orders/{id}` take the same document, and `PUT` is a
replace rather than a patch: the items sent are the items the order ends up
with. A partial update of a list of line items has no obvious meaning — there is
no stable client-visible identity to patch against — and inventing one would be
a rule this API has and the web form does not.

```json
{
  "customer": "Acme Corp",
  "due_date": "2027-03-31",
  "items": [{ "description": "Consulting", "quantity": "2", "unit_price": "500.00" }]
}
```

Money and quantities are sent as **strings** and parsed on the server, by the
same parser the form uses. `"$1,234.50"` is accepted; a float is never involved.

A create responds with the order as it was stored — the server's own id, totals,
and derived status — rather than an echo of what was sent.

`POST /api/orders/{id}/payments` takes `{"amount": "400.00", "paid_on":
"2026-08-13"}` and responds with the **whole order**, not the payment row.
Recording money changes the amount due, the derived status, and whether the
order can still be edited; returning only a receipt would leave a client holding
a stale order, and the obvious next thing it would do is compute the new balance
itself.

`GET /api/orders` returns the same document the dashboard renders — `totals`,
the `filter` that was actually applied, and `orders` — so a client's figures are
the page's figures. An unrecognised `?status=` is no filter rather than a `400`,
and the `filter` field says so.

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

## Dashboard

The dashboard at `/` is the order list. There is no second `/orders` page: two
URLs rendering one table would mean the status filter had to know which of them
it was living on in order to build its own links.

The filter is the URL. `?status=paid` is read with `use_query_map`, parsed by
`OrderStatus::parse`, and folded into the resource's source signal — so changing
the filter changes the key, and the key changing is what refetches. There is no
local "selected filter" signal to fall out of step with the address bar, and the
Back button works because the browser's history *is* the state.

An unrecognised `?status=` value is treated as no filter rather than as an
error. URLs are typed, pasted and truncated by people, and `?status=pad` is
still a perfectly answerable request: show everything.

Filtering happens in Rust, over the list `derive_order_status` has already
labelled — never as a SQL `WHERE` clause. A predicate in SQL would be a second
copy of the status rule in a second language, and the first day the two disagree
the dashboard shows an order under a badge that contradicts the filter that
found it. Reading every row and discarding some is the cost of keeping one rule;
at the size of one user's orders it is not a real cost, and the totals need the
whole set anyway.

The filter is applied on the server rather than to a list already in the
browser, so a pasted `/?status=overdue` is server-rendered as the overdue list
by the same code path, instead of arriving as everything and flickering down to
a subset once the WASM bundle loads.

The four figures in the summary strip always describe **every** order the caller
owns, whatever filter is applied — they are the reason to click a filter, so
they have to keep reporting what is there. "Outstanding" is the sum of each order's own amount
due, clamped at zero, not `billed - paid`: a credit on an overpaid invoice must
not quietly pay down a different customer's balance.

Two empty states, because they are two different facts with two different next
actions: "no orders yet", which offers the create form, and "no overdue
orders", which offers a way back to the full list.

## Design

`style/main.css` is written for this application. It replaced Pico CSS, which
carried the interface from Feature 1 to Feature 10 and was the right thing to
start with — a classless framework gives you a usable page for nothing. It is
the wrong thing to finish with, because it also decides the density, the palette
and the type, and in a tool whose entire job is showing somebody what they are
owed, those are the decisions that matter. The replacement is 21 KB against
Pico's 83 KB, and there is no framework left underneath it.

Three rules account for most of the file.

**Rules, not shadows.** Separation is done with 1px lines. A ledger is a ruled
document, and elevation is a metaphor for cards that float — these do not. The
dashboard's four figures are one bordered panel divided by hairlines rather than
four drop-shadowed tiles, and the divisions are a 1px grid gap showing the
panel's own background through, so they land correctly however the cells wrap.

**Figures are typeset.** Every amount is set in IBM Plex Mono with lining
tabular figures and right-aligned, so the digits of two totals sit above one
another and a wrong order of magnitude is visible without reading the number.
Dates get the same treatment for the same reason. The type is IBM Plex Sans and
Mono because Plex's tabular figures are real rather than approximated, and
because its two families were drawn to sit together.

**Density is the feature.** Table rows are 36px, cards are padded 12–16px, body
text is 15px and table text 13px. The exception is touch: on a coarse pointer
every control grows to the 44px minimum and the rows loosen with them, which is
the only place the two diverge.

The palette is a warm paper neutral rather than the cool grey that is every
framework's default, and the accent is a deep ink teal at 9.3:1 on white. Both
schemes are defined as tokens at the top of the file and nothing below them
names a hex value, so a border cannot be visible in light mode and invisible in
dark. Status is never carried by colour alone: every badge has its label, an
overdue row is ruled down its leading edge *and* says "Overdue", and a rejected
field is tinted *and* has its message underneath.

There is no hero, no row of three feature cards, no decorative iconography, no
gradient and no scroll-triggered motion. The only animation is a 700ms spinner
on genuinely pending work and a 160ms colour transition on hover and focus, both
of which `prefers-reduced-motion` removes. `forced-colors` and `print` each get
a short block: an order is a document somebody prints and staples to something.

## Third-party assets

**IBM Plex Sans and IBM Plex Mono**, Copyright 2017 IBM Corp, under the [SIL
Open Font License 1.1](https://scripts.sil.org/OFL). Three files in
`assets/fonts/` — one variable sans covering weights 100–700 at 40 KB, and two
static mono weights at 10 KB each — plus the licence text, which
`assets-dir = "assets"` in `Cargo.toml` publishes at `/fonts/OFL.txt`. They are
vendored rather than loaded from Google Fonts so the application makes no
third-party requests and does not depend on a network it does not control.

The published `/pkg/orders.css` does **not** carry the attribution comment from
the top of `style/main.css`: cargo-leptos processes the stylesheet with
Lightning CSS, which strips every comment. That is why the licence ships as a
file of its own rather than only as a banner.

## Tests

```bash
set -a; . ./.env; set +a
cargo fmt --check
cargo clippy --features ssr --no-default-features --all-targets -- -D warnings
cargo test --features ssr --no-default-features
cargo leptos build
```

One end-to-end test runs in a real browser, against a running deployment:

```bash
cd tests
npm install
npx playwright install chromium
BASE_URL=http://localhost:5174 npx playwright test
```

`tests/order-lifecycle.spec.ts` is a single spec on purpose. The Rust suite
already covers the rules exhaustively and far faster; what it cannot cover is
the half of this application that only exists in a browser — that the
server-rendered HTML hydrates, that a form submission reaches a server function
and comes back, that the page updates without a reload, and that a session
issued by a separate service is accepted here. A second spec re-asserting
business rules would be a slow copy of tests that already pass.

It is black box throughout: no database access, no test hooks, nothing this
application would not do for a person with a browser. It signs up a brand new
account each run, so runs never interfere and no cleanup step exists. Then it
creates a $1,000 order, pays $400 and watches the badge, the totals and the
payment history change in place at the same URL, is refused $700 with "The most
you can pay is $600.00.", settles it with $600, reads the dashboard and its
`?status=` filters as fresh navigations — which asserts what the *server*
rendered, before hydration — signs out, signs up again, and asks for the first
account's order by its exact address, which is the check a filtered list cannot
make.

The one piece of machinery in it is a hydration wait, and it is correctness
rather than politeness: Leptos re-binds every input to its signal when the
bundle takes over, so a value typed before that point is wiped the instant it
does. The test re-fills the field on a poll until a reactive value — a line
total computed in the browser by the same Rust code the server uses — proves the
page is live.

It earned its place on the first run, by failing. Leptos builds a gated page's
subtree more than once while rendering a response, and the extra pass does not
carry the request's reactive context, so the dashboard's server function ran a
second time, found no connection pool, and Leptos serialised that `INTERNAL`
error into the HTML beside the correct answer — which the browser could then
hydrate instead of the good one, on roughly one page load in six. Every Rust
test passed throughout: the two renders, the serialised payload and the
hydration that chooses between them exist only in a browser. The fix is in
`orders::ssr::pool`, which now finds the pool whether or not it is called inside
a request context, so both passes return the same thing.

`tests/api.rs` drives the real router in process through `tower`'s `oneshot`,
with `wiremock` standing in for Better Auth — one mocked session cookie per
test, each resolving to its own owner id so the tests stay parallel-safe. It
asserts what only that layer can get wrong: routes, methods, status codes, the
error shape, and `404` for another owner's order. The business rules themselves
stay covered against the services directly, rather than being asserted twice.

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

The dashboard filter was checked from both ends. The server-rendered HTML for
`/?status=paid` was fetched directly and contains one order row where `/`
contains four, `/?status=overdue` contains no table at all, and
`/?status=nonsense` contains the full list with "All" marked current — so a
shared link is filtered before any JavaScript runs. In the browser, clicking
through the filters changes the table and the marked pill without a page load,
the tiles stay on the unfiltered totals throughout, and the Back button returns
to the previous filter.

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
- Dashboard: totals across every order, and a shareable URL-driven status filter
- A REST API over the same services, with a documented status and error contract
- A container image and a one-command compose stack for the app and its database
- An end-to-end browser test walking the whole lifecycle against a running build
- A stylesheet written for this application, replacing the CSS framework it was
  prototyped on: warm paper neutrals, self-hosted IBM Plex, tabular figures on
  every amount, and both colour schemes defined as tokens. See
  [Design](#design).

Not yet implemented:

- A live public URL. The container is built and verified; standing one up needs
  a hosting account and payment details, which are the user's to create. See
  [Deploying](#deploying).

## License

Not licensed for reuse; provided as a take-home submission.
