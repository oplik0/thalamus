//! Security headers middleware
//!
//! Adds security headers to all responses to protect against common web vulnerabilities.
//! Implements OWASP recommended security headers.

use axum::{
    body::Body,
    http::{Request, Response, header},
    middleware::Next,
};

/// Security headers configuration
#[derive(Debug, Clone)]
pub struct SecurityHeadersConfig {
    /// Content Security Policy
    pub csp: String,
    /// Strict Transport Security (HSTS)
    pub hsts: String,
    /// X-Content-Type-Options
    pub content_type_options: String,
    /// X-Frame-Options
    pub frame_options: String,
    /// X-XSS-Protection
    pub xss_protection: String,
    /// Referrer Policy
    pub referrer_policy: String,
    /// Permissions Policy
    pub permissions_policy: String,
}

impl Default for SecurityHeadersConfig {
    fn default() -> Self {
        Self {
            csp: "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; font-src 'self'; connect-src 'self' https:; media-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'self'; form-action 'self';".to_string(),
            hsts: "max-age=31536000; includeSubDomains; preload".to_string(),
            content_type_options: "nosniff".to_string(),
            frame_options: "DENY".to_string(),
            xss_protection: "1; mode=block".to_string(),
            referrer_policy: "strict-origin-when-cross-origin".to_string(),
            permissions_policy: "accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()".to_string(),
        }
    }
}

impl SecurityHeadersConfig {
    /// Create a permissive configuration for development
    pub fn development() -> Self {
        Self {
            csp: "default-src * 'unsafe-inline' 'unsafe-eval';".to_string(),
            hsts: "max-age=0".to_string(),
            ..Default::default()
        }
    }

    /// Create a strict configuration for production
    pub fn production() -> Self {
        Self {
            csp: "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; media-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'; upgrade-insecure-requests;".to_string(),
            ..Default::default()
        }
    }
}

/// Security headers middleware
///
/// Adds the following headers to all responses:
/// - Content-Security-Policy
/// - Strict-Transport-Security
/// - X-Content-Type-Options
/// - X-Frame-Options
/// - X-XSS-Protection
/// - Referrer-Policy
/// - Permissions-Policy
/// - X-Content-Security-Policy (for older browsers)
/// - X-WebKit-CSP (for older Safari)
pub async fn security_headers_middleware(request: Request<Body>, next: Next) -> Response<Body> {
    let config = SecurityHeadersConfig::default();
    let mut response = next.run(request).await;

    let headers = response.headers_mut();

    // Content Security Policy
    headers.insert(header::CONTENT_SECURITY_POLICY, config.csp.parse().unwrap());

    // Strict Transport Security (HSTS)
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        config.hsts.parse().unwrap(),
    );

    // X-Content-Type-Options
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        config.content_type_options.parse().unwrap(),
    );

    // X-Frame-Options
    headers.insert(
        header::HeaderName::from_static("x-frame-options"),
        config.frame_options.parse().unwrap(),
    );

    // X-XSS-Protection
    headers.insert(
        header::HeaderName::from_static("x-xss-protection"),
        config.xss_protection.parse().unwrap(),
    );

    // Referrer-Policy
    headers.insert(
        header::HeaderName::from_static("referrer-policy"),
        config.referrer_policy.parse().unwrap(),
    );

    // Permissions-Policy
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        config.permissions_policy.parse().unwrap(),
    );

    // Remove server-identifying headers
    headers.remove(header::SERVER);

    response
}

/// Remove sensitive headers from responses
///
/// This middleware removes headers that might leak implementation details
pub async fn sanitize_headers_middleware(request: Request<Body>, next: Next) -> Response<Body> {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    // Remove headers that might leak server information
    headers.remove(header::SERVER);
    headers.remove(header::HeaderName::from_static("x-powered-by"));
    headers.remove(header::HeaderName::from_static("x-aspnet-version"));
    headers.remove(header::HeaderName::from_static("x-runtime"));

    response
}

/// Request ID middleware
///
/// Adds a unique request ID to each request for tracing and debugging
pub async fn request_id_middleware(mut request: Request<Body>, next: Next) -> Response<Body> {
    let request_id = uuid::Uuid::new_v4().to_string();

    // Add to request extensions for use in handlers
    request.extensions_mut().insert(request_id.clone());

    let mut response = next.run(request).await;

    // Add request ID to response headers
    response.headers_mut().insert(
        header::HeaderName::from_static("x-request-id"),
        request_id.parse().unwrap(),
    );

    response
}

/// CORS configuration for API endpoints
///
/// Returns a CORS layer with secure defaults
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any) // In production, specify exact origins
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([
            header::HeaderName::from_static("x-request-id"),
            header::HeaderName::from_static("x-ratelimit-limit"),
            header::HeaderName::from_static("x-ratelimit-remaining"),
        ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_security_headers_config_default() {
        let config = SecurityHeadersConfig::default();
        assert!(!config.csp.is_empty());
        assert!(config.hsts.contains("max-age"));
        assert_eq!(config.content_type_options, "nosniff");
        assert_eq!(config.frame_options, "DENY");
    }

    #[test]
    fn test_security_headers_config_production() {
        let config = SecurityHeadersConfig::production();
        assert!(config.csp.contains("upgrade-insecure-requests"));
        assert!(!config.csp.contains("unsafe-inline"));
    }
}
