//! Integration tests for health check endpoint

#[path = "common/mod.rs"]
mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sqlx::PgPool;
use tower::ServiceExt; // for `oneshot`

use common::transactional::init_test_state;

#[sqlx::test]
async fn test_health_check_returns_ok(pool: PgPool) {
    // Initialize logging for tests
    common::init_test_logging();

    // Initialize test state with transactional pool
    let state = init_test_state(pool).await;

    // Build the router
    let app = thalamus::bootstrap::build_router(state);

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

#[sqlx::test]
async fn test_health_check_response_structure(pool: PgPool) {
    common::init_test_logging();

    let state = init_test_state(pool).await;
    let app = thalamus::bootstrap::build_router(state);

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
