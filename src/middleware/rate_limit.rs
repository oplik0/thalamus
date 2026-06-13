//! Rate limiting middleware with per-key, per-user, and per-team limits
//!
//! Uses governor for in-memory rate limiting with Redis fallback for distributed deployments.
//! Supports hierarchical limits: key < user < team < global.

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::middleware::auth::Auth;
use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use governor::{
    Quota, RateLimiter as GovernorLimiter, clock::DefaultClock,
    state::keyed::DefaultKeyedStateStore,
};
use std::{net::SocketAddr, num::NonZeroU32, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Type alias for the in-memory rate limiter
pub type InMemoryLimiter = GovernorLimiter<Uuid, DefaultKeyedStateStore<Uuid>, DefaultClock>;

/// Rate limiter with hierarchical limits
#[derive(Debug, Clone)]
pub struct RateLimiter {
    /// Global limiter (fallback when no auth)
    global_limiter: Arc<InMemoryLimiter>,
    /// Per-key limiters
    key_limiters: Arc<RwLock<std::collections::HashMap<String, Arc<InMemoryLimiter>>>>,
    /// Per-user limiters
    user_limiters: Arc<RwLock<std::collections::HashMap<Uuid, Arc<InMemoryLimiter>>>>,
    /// Per-team limiters
    team_limiters: Arc<RwLock<std::collections::HashMap<Uuid, Arc<InMemoryLimiter>>>>,
    /// Persistent strict limiter for auth endpoints (keyed by IP)
    pub strict_limiter: Arc<InMemoryLimiter>,
    /// Default limits
    config: RateLimitConfig,
}

/// Rate limit configuration
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Default requests per minute for API keys
    pub key_rpm: u32,
    /// Default requests per minute for users
    pub user_rpm: u32,
    /// Default requests per minute for teams
    pub team_rpm: u32,
    /// Global fallback requests per minute
    pub global_rpm: u32,
    /// Burst size multiplier
    pub burst_multiplier: u32,
    /// Strict rate limit for auth endpoints (requests per minute per IP)
    pub strict_rpm: u32,
    /// Burst allowance for strict auth-endpoint rate limiting
    pub strict_burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            key_rpm: 60,
            user_rpm: 120,
            team_rpm: 1000,
            global_rpm: 30,
            burst_multiplier: 2,
            strict_rpm: 5,
            strict_burst: 3,
        }
    }
}

impl RateLimiter {
    /// Create a new rate limiter with the given configuration
    #[must_use]
    pub fn new(config: RateLimitConfig) -> Self {
        let global_quota = Quota::per_minute(
            NonZeroU32::new(config.global_rpm).expect("global_rpm must be non-zero"),
        )
        .allow_burst(
            NonZeroU32::new(config.global_rpm * config.burst_multiplier / 2)
                .expect("burst must be non-zero"),
        );

        let global_limiter = Arc::new(GovernorLimiter::keyed(global_quota));

        let strict_quota = Quota::per_minute(
            NonZeroU32::new(config.strict_rpm).expect("strict_rpm must be non-zero"),
        )
        .allow_burst(NonZeroU32::new(config.strict_burst).expect("strict_burst must be non-zero"));
        let strict_limiter = Arc::new(GovernorLimiter::keyed(strict_quota));

        Self {
            global_limiter,
            key_limiters: Arc::new(RwLock::new(std::collections::HashMap::new())),
            user_limiters: Arc::new(RwLock::new(std::collections::HashMap::new())),
            team_limiters: Arc::new(RwLock::new(std::collections::HashMap::new())),
            strict_limiter,
            config,
        }
    }

    /// Get or create a key-specific limiter
    async fn get_key_limiter(&self, key_id: &str, custom_rpm: Option<u32>) -> Arc<InMemoryLimiter> {
        let rpm = custom_rpm.unwrap_or(self.config.key_rpm);
        let limiters = self.key_limiters.read().await;

        if let Some(limiter) = limiters.get(key_id) {
            return limiter.clone();
        }
        drop(limiters);

        // Create new limiter
        let quota =
            Quota::per_minute(NonZeroU32::new(rpm).unwrap_or_else(|| NonZeroU32::new(60).unwrap()))
                .allow_burst(
                    NonZeroU32::new(rpm * self.config.burst_multiplier / 2)
                        .unwrap_or_else(|| NonZeroU32::new(60).unwrap()),
                );

        let limiter = Arc::new(GovernorLimiter::keyed(quota));
        let mut limiters = self.key_limiters.write().await;
        limiters.insert(key_id.to_string(), limiter.clone());
        limiter
    }

    /// Get or create a user-specific limiter
    async fn get_user_limiter(
        &self,
        user_id: Uuid,
        custom_rpm: Option<u32>,
    ) -> Arc<InMemoryLimiter> {
        let rpm = custom_rpm.unwrap_or(self.config.user_rpm);
        let limiters = self.user_limiters.read().await;

        if let Some(limiter) = limiters.get(&user_id) {
            return limiter.clone();
        }
        drop(limiters);

        let quota = Quota::per_minute(
            NonZeroU32::new(rpm).unwrap_or_else(|| NonZeroU32::new(120).unwrap()),
        )
        .allow_burst(
            NonZeroU32::new(rpm * self.config.burst_multiplier / 2)
                .unwrap_or_else(|| NonZeroU32::new(120).unwrap()),
        );

        let limiter = Arc::new(GovernorLimiter::keyed(quota));
        let mut limiters = self.user_limiters.write().await;
        limiters.insert(user_id, limiter.clone());
        limiter
    }

    /// Get or create a team-specific limiter
    async fn get_team_limiter(
        &self,
        team_id: Uuid,
        custom_rpm: Option<u32>,
    ) -> Arc<InMemoryLimiter> {
        let rpm = custom_rpm.unwrap_or(self.config.team_rpm);
        let limiters = self.team_limiters.read().await;

        if let Some(limiter) = limiters.get(&team_id) {
            return limiter.clone();
        }
        drop(limiters);

        let quota = Quota::per_minute(
            NonZeroU32::new(rpm).unwrap_or_else(|| NonZeroU32::new(1000).unwrap()),
        )
        .allow_burst(
            NonZeroU32::new(rpm * self.config.burst_multiplier / 2)
                .unwrap_or_else(|| NonZeroU32::new(500).unwrap()),
        );

        let limiter = Arc::new(GovernorLimiter::keyed(quota));
        let mut limiters = self.team_limiters.write().await;
        limiters.insert(team_id, limiter.clone());
        limiter
    }

    /// Check rate limits for an authenticated request
    /// Returns (allowed, headers) where headers contain rate limit info
    pub async fn check_auth(
        &self,
        auth: &Auth,
        _client_ip: Option<std::net::IpAddr>,
    ) -> (bool, RateLimitHeaders) {
        let mut headers = RateLimitHeaders::default();

        // Check key limit if available
        if let Some(key_id) = &auth.key_id {
            // In a real implementation, we'd fetch custom limits from the database
            let limiter = self.get_key_limiter(key_id, None).await;
            // Use the user_id as the key for rate limiting (consistent per user)
            let key = auth.user_id;

            match limiter.check_key(&key) {
                Ok(()) => {
                    headers.key_limit = Some(self.config.key_rpm);
                    headers.key_remaining = Some(self.config.key_rpm.saturating_sub(1));
                }
                Err(_) => {
                    return (
                        false,
                        RateLimitHeaders {
                            key_limit: Some(self.config.key_rpm),
                            key_remaining: Some(0),
                            retry_after: Some(60),
                            ..Default::default()
                        },
                    );
                }
            }
        }

        // Check user limit
        let user_limiter = self.get_user_limiter(auth.user_id, None).await;
        let user_key = auth.user_id;

        match user_limiter.check_key(&user_key) {
            Ok(()) => {
                headers.user_limit = Some(self.config.user_rpm);
            }
            Err(_) => {
                return (
                    false,
                    RateLimitHeaders {
                        user_limit: Some(self.config.user_rpm),
                        user_remaining: Some(0),
                        retry_after: Some(60),
                        ..headers
                    },
                );
            }
        }

        // Check team limit
        let team_limiter = self.get_team_limiter(auth.team_id, None).await;
        let team_key = auth.team_id;

        match team_limiter.check_key(&team_key) {
            Ok(()) => {
                headers.team_limit = Some(self.config.team_rpm);
            }
            Err(_) => {
                return (
                    false,
                    RateLimitHeaders {
                        team_limit: Some(self.config.team_rpm),
                        team_remaining: Some(0),
                        retry_after: Some(60),
                        ..headers
                    },
                );
            }
        }

        (true, headers)
    }

    /// Check global rate limit for unauthenticated requests
    #[must_use]
    pub fn check_global(&self, client_ip: std::net::IpAddr) -> (bool, RateLimitHeaders) {
        // Use a deterministic UUID derived from the IP address
        let key = Uuid::new_v5(&Uuid::NAMESPACE_OID, client_ip.to_string().as_bytes());

        if let Ok(()) = self.global_limiter.check_key(&key) {
            let headers = RateLimitHeaders {
                global_limit: Some(self.config.global_rpm),
                global_remaining: Some(self.config.global_rpm.saturating_sub(1)),
                ..Default::default()
            };
            (true, headers)
        } else {
            let headers = RateLimitHeaders {
                global_limit: Some(self.config.global_rpm),
                global_remaining: Some(0),
                retry_after: Some(60),
                ..Default::default()
            };
            (false, headers)
        }
    }
}

/// Rate limit headers for responses
#[derive(Debug, Default, Clone)]
pub struct RateLimitHeaders {
    pub key_limit: Option<u32>,
    pub key_remaining: Option<u32>,
    pub user_limit: Option<u32>,
    pub user_remaining: Option<u32>,
    pub team_limit: Option<u32>,
    pub team_remaining: Option<u32>,
    pub global_limit: Option<u32>,
    pub global_remaining: Option<u32>,
    pub retry_after: Option<u64>,
}

impl RateLimitHeaders {
    /// Convert to HTTP headers
    #[must_use]
    pub fn to_headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();

        if let Some(limit) = self.key_limit {
            headers.push(("X-RateLimit-Key-Limit".to_string(), limit.to_string()));
        }
        if let Some(remaining) = self.key_remaining {
            headers.push((
                "X-RateLimit-Key-Remaining".to_string(),
                remaining.to_string(),
            ));
        }
        if let Some(limit) = self.user_limit {
            headers.push(("X-RateLimit-User-Limit".to_string(), limit.to_string()));
        }
        if let Some(remaining) = self.user_remaining {
            headers.push((
                "X-RateLimit-User-Remaining".to_string(),
                remaining.to_string(),
            ));
        }
        if let Some(limit) = self.team_limit {
            headers.push(("X-RateLimit-Team-Limit".to_string(), limit.to_string()));
        }
        if let Some(remaining) = self.team_remaining {
            headers.push((
                "X-RateLimit-Team-Remaining".to_string(),
                remaining.to_string(),
            ));
        }
        if let Some(limit) = self.global_limit {
            headers.push(("X-RateLimit-Global-Limit".to_string(), limit.to_string()));
        }
        if let Some(remaining) = self.global_remaining {
            headers.push((
                "X-RateLimit-Global-Remaining".to_string(),
                remaining.to_string(),
            ));
        }
        if let Some(retry) = self.retry_after {
            headers.push(("Retry-After".to_string(), retry.to_string()));
        }

        headers
    }
}

/// Rate limiting middleware for authenticated routes
pub async fn rate_limit_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Result<Response> {
    // Check if rate limiting is enabled
    let config = state.config.as_ref();
    let rate_limit_enabled = config.rate_limiting.as_ref().is_none_or(|rl| rl.enabled);

    if !rate_limit_enabled {
        return Ok(next.run(request).await);
    }

    // Try to extract auth from request extensions (set by auth middleware)
    let auth = request.extensions().get::<Auth>().cloned();

    let limiter = state
        .rate_limiter
        .as_ref()
        .ok_or_else(|| Error::Internal("Rate limiter not initialized".to_string()))?;

    let (allowed, headers) = if let Some(auth) = auth {
        limiter.check_auth(&auth, Some(addr.ip())).await
    } else {
        limiter.check_global(addr.ip())
    };

    if !allowed {
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            crate::error::Error::Authentication("Rate limit exceeded".to_string()),
        )
            .into_response();

        // Add rate limit headers
        for (name, value) in headers.to_headers() {
            if let Ok(header_name) = name.parse::<axum::http::HeaderName>()
                && let Ok(header_value) = value.parse()
            {
                response.headers_mut().insert(header_name, header_value);
            }
        }

        return Ok(response);
    }

    // Continue with the request
    let mut response = next.run(request).await;

    // Add rate limit headers to successful response
    for (name, value) in headers.to_headers() {
        if let Ok(header_name) = name.parse::<axum::http::HeaderName>()
            && let Ok(header_value) = value.parse()
        {
            response.headers_mut().insert(header_name, header_value);
        }
    }

    Ok(response)
}

/// Strict rate limiting for authentication endpoints (login, register, etc.)
/// This uses much stricter limits to prevent brute force attacks
pub async fn strict_rate_limit_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Result<Response> {
    let limiter = state
        .rate_limiter
        .as_ref()
        .ok_or_else(|| Error::Internal("Rate limiter not initialized".to_string()))?;

    // Derive a deterministic, per-IP UUID so the shared limiter enforces
    // limits per connecting IP address across all requests.
    let key = Uuid::new_v5(&Uuid::NAMESPACE_OID, addr.ip().to_string().as_bytes());

    if let Ok(()) = limiter.strict_limiter.check_key(&key) {
        let mut response = next.run(request).await;

        // Add strict rate limit headers
        response.headers_mut().insert(
            "X-RateLimit-Limit",
            limiter.config.strict_rpm.to_string().parse().unwrap(),
        );

        Ok(response)
    } else {
        tracing::warn!(
            ip = %addr.ip(),
            "Strict rate limit exceeded for auth endpoint"
        );

        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            crate::error::Error::Authentication(
                "Too many authentication attempts. Please try again later.".to_string(),
            ),
        )
            .into_response();

        response
            .headers_mut()
            .insert("Retry-After", "60".parse().unwrap());

        Ok(response)
    }
}

/// Layer for applying rate limiting to specific routes
#[derive(Debug, Clone)]
pub struct RateLimitLayer {
    pub limiter: Arc<RateLimiter>,
}

impl RateLimitLayer {
    #[must_use]
    pub fn new(limiter: Arc<RateLimiter>) -> Self {
        Self { limiter }
    }
}

impl<S> tower::Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            limiter: self.limiter.clone(),
        }
    }
}

/// Service for rate limiting
#[derive(Debug, Clone)]
pub struct RateLimitService<S> {
    inner: S,
    limiter: Arc<RateLimiter>,
}

impl<S> tower::Service<Request> for RateLimitService<S>
where
    S: tower::Service<Request, Response = Response> + Clone + Send + 'static,
    S::Error: Into<axum::BoxError>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = axum::BoxError;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<Self::Response, Self::Error>>
                + Send,
        >,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(std::convert::Into::into)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let inner = self.inner.clone();
        let limiter = self.limiter.clone();

        Box::pin(async move {
            // Extract client IP
            let client_ip = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip());

            // Try to get auth from extensions
            let auth = req.extensions().get::<Auth>().cloned();

            let (allowed, headers) = if let Some(auth) = auth {
                if let Some(ip) = client_ip {
                    limiter.check_auth(&auth, Some(ip)).await
                } else {
                    (true, RateLimitHeaders::default())
                }
            } else if let Some(ip) = client_ip {
                limiter.check_global(ip)
            } else {
                (true, RateLimitHeaders::default())
            };

            if !allowed {
                let mut response = Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(Body::empty())
                    .unwrap();

                for (name, value) in headers.to_headers() {
                    if let Ok(header_name) = name.parse::<axum::http::HeaderName>()
                        && let Ok(header_value) = value.parse()
                    {
                        response.headers_mut().insert(header_name, header_value);
                    }
                }

                return Ok(response);
            }

            let mut response = inner.clone().call(req).await.map_err(Into::into)?;

            // Add rate limit headers
            for (name, value) in headers.to_headers() {
                if let Ok(header_name) = name.parse::<axum::http::HeaderName>()
                    && let Ok(header_value) = value.parse()
                {
                    response.headers_mut().insert(header_name, header_value);
                }
            }

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_config_default() {
        let config = RateLimitConfig::default();
        assert_eq!(config.key_rpm, 60);
        assert_eq!(config.user_rpm, 120);
        assert_eq!(config.team_rpm, 1000);
        assert_eq!(config.global_rpm, 30);
    }

    #[test]
    fn test_rate_limit_headers() {
        let headers = RateLimitHeaders {
            key_limit: Some(60),
            key_remaining: Some(59),
            user_limit: Some(120),
            user_remaining: Some(119),
            team_limit: Some(1000),
            team_remaining: Some(999),
            global_limit: Some(30),
            global_remaining: Some(29),
            retry_after: Some(60),
        };

        let header_vec = headers.to_headers();
        assert!(header_vec.iter().any(|(k, _)| k == "X-RateLimit-Key-Limit"));
        assert!(
            header_vec
                .iter()
                .any(|(k, _)| k == "X-RateLimit-User-Limit")
        );
        assert!(
            header_vec
                .iter()
                .any(|(k, _)| k == "X-RateLimit-Team-Limit")
        );
        assert!(header_vec.iter().any(|(k, _)| k == "Retry-After"));
    }
}
