//! Serializable application error.
//!
//! `AppError` crosses three boundaries and must render identically on all of
//! them: Leptos server functions (serialized to the browser), the REST API
//! (JSON), and SSR HTML. It therefore carries no `sqlx`, `axum`, or `reqwest`
//! types — only owned data that serde can move across the wire.
//!
//! Later features extend this enum: field validation and overflow in Feature 4,
//! not-found and immutable-order conflicts in Feature 5, and actionable
//! overpayment detail in Feature 6.

use leptos::server_fn::{
    codec::JsonEncoding,
    error::{FromServerFnError, ServerFnErrorErr},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Every error a client is allowed to observe.
///
/// Variants are deliberately coarse. Internal detail (SQL state, connection
/// strings, upstream response bodies) is logged server-side and never
/// serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(tag = "error", content = "detail", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AppError {
    /// An unexpected server-side failure. Detail stays in the logs.
    #[error("The request could not be completed.")]
    Internal,

    /// A dependency this request needs is reachable but not healthy, or not
    /// reachable at all. Named so the operator knows which one.
    #[error("{0} is temporarily unavailable.")]
    DependencyUnavailable(String),

    /// No valid Better Auth session accompanied the request.
    ///
    /// Deliberately distinct from [`Self::DependencyUnavailable`]: Better Auth
    /// answers `200` with a `null` body for an unknown or expired session, so
    /// "you are signed out" and "the auth service is down" are different
    /// observations that must not collapse into one message.
    #[error("You need to sign in to do that.")]
    Unauthenticated,

    /// A state-changing request arrived from an origin that is not this
    /// application's own. Checked in Rust, independently of Better Auth's own
    /// trusted-origin list, so a cookie alone is never sufficient to act.
    #[error("This request did not come from a trusted origin.")]
    ForbiddenOrigin,

    /// Better Auth refused a credential the user supplied. The message comes
    /// from Better Auth and is safe to show: it describes the submitted
    /// credentials, never the service's internals.
    #[error("{0}")]
    AuthRejected(String),

    /// One or more submitted fields were not acceptable.
    ///
    /// Carries every failure at once, keyed by field, rather than stopping at
    /// the first. A form that reports one problem per round trip makes the user
    /// discover the rest by trial.
    ///
    /// Named to match its [`Self::code`] exactly. The serde tag on this enum is
    /// derived from the variant name and is what a Rust client decodes, while
    /// `code()` is what the REST surface returns — two names for one condition
    /// would make them look like two conditions.
    #[error("{}", validation_summary(.0))]
    ValidationFailed(Vec<FieldError>),

    /// A computed amount left the range this application can represent.
    ///
    /// Reached only through `checked_mul`/`checked_add` on `i64` cents, so it
    /// is the *reason* those operations exist rather than an afterthought: the
    /// alternative to this error is a total that silently wrapped negative.
    #[error("That amount is too large. Keep the order total below $92,233,720,368,547,758.")]
    AmountOutOfRange,

    /// The server function framework failed before or after our code ran —
    /// a network drop mid-submit, or a response it could not decode. Carries
    /// the framework's own description because there is nothing server-side to
    /// look up: the failure usually happened in the browser.
    #[error("{0}")]
    Transport(String),
}

impl AppError {
    /// Stable machine-readable code. Clients match on this, not on the message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Internal => "INTERNAL",
            Self::DependencyUnavailable(_) => "DEPENDENCY_UNAVAILABLE",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::ForbiddenOrigin => "FORBIDDEN_ORIGIN",
            Self::AuthRejected(_) => "AUTH_REJECTED",
            Self::ValidationFailed(_) => "VALIDATION_FAILED",
            Self::AmountOutOfRange => "AMOUNT_OUT_OF_RANGE",
            Self::Transport(_) => "TRANSPORT",
        }
    }

    /// The per-field failures behind a [`Self::ValidationFailed`], for a form to
    /// attach to the inputs that caused them. Empty for every other variant.
    pub fn field_errors(&self) -> &[FieldError] {
        match self {
            Self::ValidationFailed(errors) => errors,
            _ => &[],
        }
    }

    /// The message for one field, if this error mentions it.
    pub fn message_for(&self, field: &str) -> Option<&str> {
        self.field_errors()
            .iter()
            .find(|error| error.field == field)
            .map(|error| error.message.as_str())
    }

    /// HTTP status for the REST surface and for SSR error responses.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Internal | Self::Transport(_) => 500,
            Self::DependencyUnavailable(_) => 503,
            Self::Unauthenticated => 401,
            Self::ForbiddenOrigin => 403,
            Self::AuthRejected(_) | Self::ValidationFailed(_) | Self::AmountOutOfRange => 400,
        }
    }
}

/// One field, one reason it was rejected.
///
/// `field` is a stable machine-readable path, not a label: `customer`,
/// `due_date`, or `items[2].quantity`. The browser matches on it to put the
/// message next to the right input, and the REST surface returns it unchanged
/// so a non-browser client can do the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Renders a validation error as one sentence, for the places that can only
/// show a single string: a log line, a REST `message`, or a plain-text client.
/// The structured list is always available alongside it.
fn validation_summary(errors: &[FieldError]) -> String {
    match errors {
        [] => "The submitted values were not accepted.".to_string(),
        [only] => only.message.clone(),
        _ => format!(
            "{} fields need attention: {}",
            errors.len(),
            errors
                .iter()
                .map(|error| error.field.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Lets `#[server]` functions return `Result<T, AppError>` directly instead of
/// wrapping every error in `ServerFnError<AppError>`.
///
/// The associated encoder is what carries the error across the wire: the server
/// serializes `AppError` as JSON into the response body and the browser decodes
/// the same bytes back into `AppError`, so a caller matches on the identical
/// enum on both sides.
impl FromServerFnError for AppError {
    type Encoder = JsonEncoding;

    fn from_server_fn_error(value: ServerFnErrorErr) -> Self {
        match value {
            // Raised in the browser, so there is no server-side log to
            // correlate with; the description is all the caller will ever get.
            ServerFnErrorErr::Request(detail)
            | ServerFnErrorErr::UnsupportedRequestMethod(detail)
            | ServerFnErrorErr::Serialization(detail)
            | ServerFnErrorErr::Deserialization(detail) => Self::Transport(detail),

            // Everything else is a defect in this application — a bad
            // registration, a malformed argument list, a response that could
            // not be built. Log it and show the client nothing.
            other => {
                #[cfg(feature = "ssr")]
                tracing::error!(error = %other, "server function framework error");
                #[cfg(not(feature = "ssr"))]
                let _ = &other;
                Self::Internal
            }
        }
    }
}

/// Result alias used by services and server functions.
pub type AppResult<T> = Result<T, AppError>;

/// Body shape for every JSON error response.
///
/// Serialize only: `error` borrows a `&'static str` from [`AppError::code`],
/// which serde cannot deserialize into. Tests read the body as JSON.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: &'static str,
    pub message: String,
}

impl From<&AppError> for ErrorBody {
    fn from(error: &AppError) -> Self {
        Self {
            error: error.code(),
            message: error.to_string(),
        }
    }
}

#[cfg(feature = "ssr")]
pub mod ssr {
    use super::{AppError, AppResult, ErrorBody};
    use axum::{
        http::StatusCode,
        response::{IntoResponse, Response},
        Json,
    };
    use leptos::prelude::use_context;
    use leptos_axum::ResponseOptions;

    /// Stamps the real HTTP status onto a failing server function's response.
    ///
    /// `server_fn` answers *every* failed server function with `500` and the
    /// encoded error in the body. A typed Rust caller does not care — it
    /// decodes the body either way — but everything else does: a proxy, a log
    /// aggregator, or a `curl` in a review sees "the server broke" when the
    /// truth is "you are not signed in". The body is already correct; this
    /// makes the status line agree with it.
    ///
    /// Wraps the whole body of a server function rather than each `?`, so a
    /// function either reports its statuses or visibly does not.
    pub fn report<T>(result: AppResult<T>) -> AppResult<T> {
        if let Err(error) = &result {
            // Absent on the REST path in Feature 9, where `IntoResponse` below
            // sets the status directly instead.
            if let Some(response) = use_context::<ResponseOptions>() {
                if let Ok(status) = StatusCode::from_u16(error.status_code()) {
                    response.set_status(status);
                }
            }
        }

        result
    }

    impl From<sqlx::Error> for AppError {
        /// Database failures are logged with their real cause and reduced to an
        /// opaque error for the client.
        fn from(error: sqlx::Error) -> Self {
            tracing::error!(error = %error, "database operation failed");
            Self::Internal
        }
    }

    impl IntoResponse for AppError {
        fn into_response(self) -> Response {
            let status = StatusCode::from_u16(self.status_code())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let body = ErrorBody::from(&self);

            (status, Json(body)).into_response()
        }
    }
}
