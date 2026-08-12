//! Server binary. Built natively with the `ssr` feature only.
//!
//! Startup order is deliberate: configuration is validated, PostgreSQL is
//! connected and migrated, and only then is the TCP listener bound. A
//! misconfigured or unreachable deployment fails before it can accept a single
//! request.
//!
//! Required environment variables:
//!   DATABASE_URL       postgres:// connection string for this app's database
//!   BETTER_AUTH_URL    base URL of the shared Better Auth service
//!   LEPTOS_OUTPUT_NAME set automatically by cargo-leptos; must be set manually
//!                      for a bare binary, or every /pkg/* asset URL is empty
//! Optional:
//!   LEPTOS_SITE_ADDR   listen address (default from Cargo.toml metadata)
//!   RUST_LOG           tracing filter

#[cfg(feature = "ssr")]
use std::{sync::Arc, time::Duration};

#[cfg(feature = "ssr")]
use axum::{
    extract::{FromRef, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
#[cfg(feature = "ssr")]
use leptos::prelude::{get_configuration, provide_context, LeptosOptions};
#[cfg(feature = "ssr")]
use leptos_axum::{file_and_error_handler, generate_route_list, LeptosRoutes};
#[cfg(feature = "ssr")]
use orders_and_settlements::{error::AppError, shell, App};
#[cfg(feature = "ssr")]
use serde::Serialize;
#[cfg(feature = "ssr")]
use sqlx::{migrate::Migrator, postgres::PgPoolOptions, PgPool};
#[cfg(feature = "ssr")]
use tokio::{net::TcpListener, signal};
#[cfg(feature = "ssr")]
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
#[cfg(feature = "ssr")]
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[cfg(feature = "ssr")]
const SERVICE_NAME: &str = "orders-and-settlements";

/// Migrations are embedded in the binary at compile time, so a deployed image
/// carries its own schema history and needs no files on disk.
#[cfg(feature = "ssr")]
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Shared Axum state. `leptos_routes_with_context` also provides this to every
/// Leptos server function, so later features reach the pool through
/// `use_context::<AppState>()` without extra plumbing.
#[cfg(feature = "ssr")]
#[derive(Clone, Debug)]
pub struct AppState {
    pub leptos_options: LeptosOptions,
    pub pool: PgPool,
    pub auth_base_url: Arc<str>,
}

/// Hand-written instead of `#[derive(FromRef)]`, which would require enabling
/// axum's non-default `macros` feature to save four lines.
#[cfg(feature = "ssr")]
impl FromRef<AppState> for LeptosOptions {
    fn from_ref(state: &AppState) -> Self {
        state.leptos_options.clone()
    }
}

/// Configuration this application owns. Leptos reads `LEPTOS_*` itself and
/// `EnvFilter` reads `RUST_LOG`, so only these two are parsed by hand — not
/// enough surface to justify a configuration crate.
#[cfg(feature = "ssr")]
#[derive(Debug)]
struct Config {
    database_url: String,
    auth_base_url: String,
}

#[cfg(feature = "ssr")]
#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("missing required environment variable {0}")]
    MissingEnv(&'static str),
    #[error("invalid environment variable {0}: {1}")]
    InvalidEnv(&'static str, &'static str),
    #[error("could not connect to PostgreSQL: {0}")]
    Database(#[from] sqlx::Error),
    #[error("could not apply database migrations: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("could not load Leptos configuration: {0}")]
    LeptosConfig(String),
    #[error("could not bind the listen address: {0}")]
    Bind(#[from] std::io::Error),
}

#[cfg(feature = "ssr")]
#[derive(Debug, Serialize)]
struct HealthBody {
    status: &'static str,
    service: &'static str,
    database: &'static str,
}

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() {
    init_tracing();

    if let Err(error) = start().await {
        tracing::error!(%error, "startup aborted");
        std::process::exit(1);
    }
}

/// Logs to stdout so a container runtime owns collection. `RUST_LOG` overrides
/// the default filter.
#[cfg(feature = "ssr")]
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,orders_and_settlements=debug,tower_http=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// The real startup path. Separated from `main` so every failure returns a
/// typed error instead of a panic.
#[cfg(feature = "ssr")]
async fn start() -> Result<(), StartupError> {
    let config = load_config()?;
    let pool = create_pool(&config.database_url).await?;
    run_migrations(&pool).await?;

    let leptos_options = get_configuration(None)
        .map_err(|error| StartupError::LeptosConfig(error.to_string()))?
        .leptos_options;
    let address = leptos_options.site_addr;

    let state = AppState {
        leptos_options,
        pool,
        auth_base_url: Arc::from(config.auth_base_url.as_str()),
    };

    let router = build_router(state);
    let listener = TcpListener::bind(address).await?;
    tracing::info!(%address, service = SERVICE_NAME, "listening");

    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shutdown complete");
    Ok(())
}

/// Reads and validates every variable this application owns. Runs before the
/// pool is created, so a typo never surfaces as a confusing connection error.
#[cfg(feature = "ssr")]
fn load_config() -> Result<Config, StartupError> {
    let database_url = required_env("DATABASE_URL")?;
    if !database_url.starts_with("postgres://") && !database_url.starts_with("postgresql://") {
        return Err(StartupError::InvalidEnv(
            "DATABASE_URL",
            "must use the postgres:// or postgresql:// scheme",
        ));
    }

    let auth_base_url = required_env("BETTER_AUTH_URL")?;
    if !auth_base_url.starts_with("http://") && !auth_base_url.starts_with("https://") {
        return Err(StartupError::InvalidEnv(
            "BETTER_AUTH_URL",
            "must be an absolute http:// or https:// URL",
        ));
    }

    // cargo-leptos exports this for `watch`/`serve`. A bare binary must set it
    // explicitly, otherwise `get_configuration` yields an empty output name and
    // the generated /pkg/*.js and /pkg/*.wasm URLs silently 404.
    required_env("LEPTOS_OUTPUT_NAME")?;

    Ok(Config {
        database_url,
        // Stored without a trailing slash so Feature 3 can join paths safely.
        auth_base_url: auth_base_url.trim_end_matches('/').to_string(),
    })
}

#[cfg(feature = "ssr")]
fn required_env(key: &'static str) -> Result<String, StartupError> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(StartupError::MissingEnv(key)),
    }
}

/// `connect` opens a real connection, so an unreachable or unauthenticated
/// database fails here rather than on the first request.
#[cfg(feature = "ssr")]
async fn create_pool(database_url: &str) -> Result<PgPool, StartupError> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await?;

    tracing::info!("connected to PostgreSQL");
    Ok(pool)
}

#[cfg(feature = "ssr")]
async fn run_migrations(pool: &PgPool) -> Result<(), StartupError> {
    MIGRATOR.run(pool).await?;

    tracing::info!("database migrations applied");
    Ok(())
}

/// Assembles the single Axum router that serves the API, the health probe,
/// Leptos server functions, static assets, and server-rendered HTML.
#[cfg(feature = "ssr")]
fn build_router(state: AppState) -> Router {
    // Walks `App` to discover every `leptos_router` path so SSR can respond to
    // them directly instead of falling through to the 404 handler.
    let routes = generate_route_list(App);
    let shell_options = state.leptos_options.clone();
    let pool = state.pool.clone();

    Router::new()
        .route("/health", get(health))
        // Registers the discovered SSR routes *and*, automatically, every
        // `#[server]` function endpoint (integrations/axum/src/lib.rs:1826),
        // providing the context closure to both.
        //
        // `AppState` is defined in this binary crate, which the library cannot
        // name, so the pool is provided as a bare `PgPool` — the one type both
        // sides already share. `orders::ssr::pool()` is the only reader.
        // Cloning a `PgPool` clones a handle, not a connection.
        .leptos_routes_with_context(
            &state,
            routes,
            move || provide_context(pool.clone()),
            move || shell(shell_options.clone()),
        )
        // Serves files from `site_root`, then renders the shell with a 404 for
        // anything that is neither a file nor a known route.
        .fallback(file_and_error_handler::<AppState, _>(shell))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Readiness probe. Reports healthy only if a connection can be checked out of
/// the pool and the database answers, so a load balancer never routes traffic
/// to an instance whose database is gone.
#[cfg(feature = "ssr")]
async fn health(State(state): State<AppState>) -> Response {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => (
            StatusCode::OK,
            Json(HealthBody {
                status: "ok",
                service: SERVICE_NAME,
                database: "up",
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "health probe could not reach PostgreSQL");
            // Deliberately not the blanket `sqlx::Error -> AppError::Internal`
            // conversion: an unreachable database is a 503, not a 500.
            AppError::DependencyUnavailable("PostgreSQL".to_string()).into_response()
        }
    }
}

/// Resolves on SIGINT or SIGTERM so in-flight requests drain before exit.
/// cargo-leptos sends these on every rebuild, so the path is exercised in
/// development, not only in production.
#[cfg(feature = "ssr")]
async fn shutdown_signal() {
    let interrupt = async {
        signal::ctrl_c()
            .await
            .expect("failed to install the Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => tracing::info!("received SIGINT, draining"),
        _ = terminate => tracing::info!("received SIGTERM, draining"),
    }
}

/// Present only so `cargo check` without `--features ssr` still links a binary.
/// cargo-leptos always builds the binary with `bin-features = ["ssr"]`.
#[cfg(not(feature = "ssr"))]
fn main() {}
