# Orders and Settlements

Full-stack Rust application for creating orders with line items, recording full
or partial payments against them, and viewing a dashboard of derived status and
amounts due.

> **Status: in progress.** The foundation is complete and verified. Orders,
> payments, the dashboard, and the REST API are not implemented yet. See
> [Current status](#current-status).

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

Start the development server. It rebuilds on change and serves at the address
in `LEPTOS_SITE_ADDR`, defaulting to `127.0.0.1:3000`.

```bash
cargo leptos serve
```

Migrations run automatically at startup, so no separate migrate step is needed.

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
| `GET` | `/` | Server-rendered application shell |
| `GET` | `/pkg/*` | Hydration bundle and static assets |

The REST API for orders and payments arrives with those features.

## Tests

```bash
cargo fmt --check
cargo clippy --features ssr --no-default-features -- -D warnings
cargo leptos build
```

Automated test coverage begins with the authentication feature. The foundation
was verified manually: startup validation branches, health success and failure,
server-rendered output, hydration in a browser, and graceful shutdown on
SIGTERM.

## Current status

Implemented:

- Cargo workspace, feature separation, and cargo-leptos configuration
- Strict startup configuration with fail-fast validation
- PostgreSQL pool and embedded migrations applied at boot
- Database-backed health endpoint with a distinct unavailable response
- Server-rendered shell with working browser hydration
- Structured tracing, response compression, and graceful shutdown

Not yet implemented:

- Authentication against the shared Better Auth service
- Orders and line items, payments, derived status
- Dashboard, order detail, REST API
- Deployment and the deployed URL

## License

Not licensed for reuse; provided as a take-home submission.
