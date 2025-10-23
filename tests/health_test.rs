//! Integration tests for health check endpoint

#[path = "common/mod.rs"]
mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt; // for `oneshot`

#[tokio::test]
async fn test_health_check_returns_ok() {
    // Initialize logging for tests
    common::init_test_logging();

    // Build the router
    let app = thalamus::bootstrap::build_router();

    // Make request to health endpoint
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Parse response body
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();

    // Verify response contains expected fields
    assert!(body_str.contains("\"status\":\"ok\""));
    assert!(body_str.contains("\"version\":"));
}

#[tokio::test]
async fn test_health_check_response_structure() {
    common::init_test_logging();

    let app = thalamus::bootstrap::build_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Verify content type
    let content_type = response.headers().get("content-type").unwrap();
    assert!(content_type.to_str().unwrap().contains("application/json"));
}
