//! Serializable application error.
//!
//! `AppError` crosses three boundaries and must render identically on all of
//! them: Leptos server functions (serialized to the browser), the REST API
//! (JSON), and SSR HTML. It therefore carries no `sqlx`, `axum`, or `reqwest`
//! types — only owned data that serde can move across the wire.
//!
//! Later features extend this enum: authentication in Feature 3, field
//! validation and overflow in Feature 4, not-found and immutable-order
//! conflicts in Feature 5, and actionable overpayment detail in Feature 6.

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
}

impl AppError {
    /// Stable machine-readable code. Clients match on this, not on the message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Internal => "INTERNAL",
            Self::DependencyUnavailable(_) => "DEPENDENCY_UNAVAILABLE",
        }
    }

    /// HTTP status for the REST surface and for SSR error responses.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Internal => 500,
            Self::DependencyUnavailable(_) => 503,
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
mod ssr {
    use super::{AppError, ErrorBody};
    use axum::{
        http::StatusCode,
        response::{IntoResponse, Response},
        Json,
    };

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
