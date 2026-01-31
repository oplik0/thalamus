//! Global error types for Thalamus
//!
//! This module defines the error types used throughout the application,
//! with conversions from various underlying error types.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// Application result type
pub type Result<T> = std::result::Result<T, Error>;

/// Application error type
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Database errors
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Configuration errors
    #[error("Configuration error: {0}")]
    Config(String),

    /// Authentication errors
    #[error("Authentication failed: {0}")]
    Authentication(String),

    /// Authorization errors
    #[error("Authorization failed: {0}")]
    Authorization(String),

    /// Not found errors
    #[error("Resource not found: {0}")]
    NotFound(String),

    /// Invalid input errors
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// Backend communication errors
    #[error("Backend error: {0}")]
    Backend(String),

    /// Service unavailable (all backends at capacity)
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    /// Internal server errors
    #[error("Internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Get the HTTP status code for this error
    #[must_use]
    pub const fn status_code(&self) -> StatusCode {
        match self {
            Self::Database(_) | Self::Internal(_) | Self::Config(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Self::Authentication(_) => StatusCode::UNAUTHORIZED,
            Self::Authorization(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::InvalidInput(_) => StatusCode::BAD_REQUEST,
            Self::Backend(_) => StatusCode::BAD_GATEWAY,
            Self::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// Error response body
#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let error_message = self.to_string();

        // Log the error
        tracing::error!(
            error = %error_message,
            status = %status,
            "Request error"
        );

        let body = Json(ErrorResponse {
            error: error_message,
            details: None,
        });

        (status, body).into_response()
    }
}

// Convenience conversions
impl From<anyhow::Error> for Error {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

impl From<sqlx::migrate::MigrateError> for Error {
    fn from(err: sqlx::migrate::MigrateError) -> Self {
        Self::Database(sqlx::Error::Migrate(Box::new(err)))
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Internal(format!("Serialization error: {}", err))
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Self::Backend(format!("HTTP client error: {}", err))
    }
}

impl From<pasetors::errors::Error> for Error {
    fn from(err: pasetors::errors::Error) -> Self {
        Self::Authentication(format!("Token error: {}", err))
    }
}
