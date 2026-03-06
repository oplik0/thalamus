//! Breach detection and security monitoring middleware
//!
//! Detects suspicious patterns and potential security breaches:
//! - Multiple failed authentication attempts
//! - Unusual request patterns
//! - Potential credential stuffing attacks
//! - Anomalous API usage

use crate::bootstrap::AppState;
use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Breach detection configuration
#[derive(Debug, Clone)]
pub struct BreachDetectionConfig {
    /// Number of failed auth attempts before alerting
    pub failed_auth_threshold: u32,
    /// Time window for failed auth counting (seconds)
    pub failed_auth_window_secs: u64,
    /// Number of requests per minute to trigger anomaly alert
    pub request_rate_threshold: u32,
    /// Number of different endpoints accessed to flag as scanning
    pub endpoint_scan_threshold: u32,
    /// Time window for endpoint scanning detection (seconds)
    pub endpoint_scan_window_secs: u64,
    /// Enable automatic IP blocking
    pub auto_block_enabled: bool,
    /// Duration to block IPs (seconds)
    pub block_duration_secs: u64,
    /// Evict profiles that have had no activity for this many seconds (0 = never)
    pub max_profile_age_secs: u64,
}

impl Default for BreachDetectionConfig {
    fn default() -> Self {
        Self {
            failed_auth_threshold: 5,
            failed_auth_window_secs: 300,  // 5 minutes
            request_rate_threshold: 1000,  // 1000 req/min
            endpoint_scan_threshold: 20,   // 20 different endpoints
            endpoint_scan_window_secs: 60, // 1 minute
            auto_block_enabled: false,     // Disabled by default (requires manual review)
            block_duration_secs: 3600,     // 1 hour
            max_profile_age_secs: 3600,    // evict profiles inactive for > 1 hour
        }
    }
}

/// Security event types
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecurityEventType {
    FailedAuthentication,
    SuspiciousRequestPattern,
    RateLimitExceeded,
    EndpointScanning,
    PrivilegeEscalationAttempt,
    TokenReuse,
    UnusualLocation,
}

impl std::fmt::Display for SecurityEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityEventType::FailedAuthentication => write!(f, "failed_authentication"),
            SecurityEventType::SuspiciousRequestPattern => write!(f, "suspicious_pattern"),
            SecurityEventType::RateLimitExceeded => write!(f, "rate_limit_exceeded"),
            SecurityEventType::EndpointScanning => write!(f, "endpoint_scanning"),
            SecurityEventType::PrivilegeEscalationAttempt => write!(f, "privilege_escalation"),
            SecurityEventType::TokenReuse => write!(f, "token_reuse"),
            SecurityEventType::UnusualLocation => write!(f, "unusual_location"),
        }
    }
}

/// Security event for logging and alerting
#[derive(Debug, Clone)]
pub struct SecurityEvent {
    pub event_type: SecurityEventType,
    pub timestamp: Instant,
    pub client_ip: Option<IpAddr>,
    pub user_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub details: String,
    pub severity: SecuritySeverity,
}

/// Security severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecuritySeverity {
    Low,
    Medium,
    High,
    Critical,
}

impl std::fmt::Display for SecuritySeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecuritySeverity::Low => write!(f, "low"),
            SecuritySeverity::Medium => write!(f, "medium"),
            SecuritySeverity::High => write!(f, "high"),
            SecuritySeverity::Critical => write!(f, "critical"),
        }
    }
}

/// Per-client security tracking
#[derive(Debug, Default)]
struct ClientSecurityProfile {
    failed_auth_attempts: Vec<Instant>,
    requests: Vec<Instant>,
    endpoints_accessed: Vec<(Instant, String)>,
    blocked_until: Option<Instant>,
    alert_count: u32,
}

/// Breach detection engine
#[derive(Debug, Clone)]
pub struct BreachDetector {
    config: BreachDetectionConfig,
    profiles: Arc<RwLock<HashMap<IpAddr, ClientSecurityProfile>>>,
}

impl BreachDetector {
    /// Create a new breach detector with the given configuration
    pub fn new(config: BreachDetectionConfig) -> Self {
        Self {
            config,
            profiles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Return a reference to this detector's configuration
    pub fn config(&self) -> &BreachDetectionConfig {
        &self.config
    }

    /// Record a failed authentication attempt
    pub async fn record_failed_auth(&self, client_ip: IpAddr, user_id: Option<Uuid>) {
        let mut profiles = self.profiles.write().await;
        let profile = profiles.entry(client_ip).or_default();

        let now = Instant::now();
        profile.failed_auth_attempts.push(now);

        // Clean old attempts outside the window
        let window = Duration::from_secs(self.config.failed_auth_window_secs);
        profile
            .failed_auth_attempts
            .retain(|t| now.duration_since(*t) < window);

        let attempt_count = profile.failed_auth_attempts.len() as u32;

        // Check threshold
        if attempt_count >= self.config.failed_auth_threshold {
            let event = SecurityEvent {
                event_type: SecurityEventType::FailedAuthentication,
                timestamp: now,
                client_ip: Some(client_ip),
                user_id,
                team_id: None,
                details: format!(
                    "Multiple failed authentication attempts: {} in {} seconds",
                    attempt_count, self.config.failed_auth_window_secs
                ),
                severity: if attempt_count >= self.config.failed_auth_threshold * 2 {
                    SecuritySeverity::Critical
                } else {
                    SecuritySeverity::High
                },
            };

            self.log_security_event(&event);
            profile.alert_count += 1;

            // Auto-block if enabled
            if self.config.auto_block_enabled {
                profile.blocked_until =
                    Some(now + Duration::from_secs(self.config.block_duration_secs));
                tracing::warn!(
                    ip = %client_ip,
                    duration_secs = self.config.block_duration_secs,
                    "IP automatically blocked due to failed authentication"
                );
            }
        }
    }

    /// Record a request for pattern analysis
    pub async fn record_request(&self, client_ip: IpAddr, endpoint: &str, _user_id: Option<Uuid>) {
        let mut profiles = self.profiles.write().await;
        let profile = profiles.entry(client_ip).or_default();

        let now = Instant::now();
        profile.requests.push(now);
        profile.endpoints_accessed.push((now, endpoint.to_string()));

        // Clean old data
        let window = Duration::from_secs(60);
        profile.requests.retain(|t| now.duration_since(*t) < window);
        let scan_window = Duration::from_secs(self.config.endpoint_scan_window_secs);
        profile
            .endpoints_accessed
            .retain(|(t, _)| now.duration_since(*t) < scan_window);

        // Check for high request rate
        if profile.requests.len() as u32 >= self.config.request_rate_threshold {
            let event = SecurityEvent {
                event_type: SecurityEventType::SuspiciousRequestPattern,
                timestamp: now,
                client_ip: Some(client_ip),
                user_id: None,
                team_id: None,
                details: format!(
                    "High request rate detected: {} requests in 60 seconds",
                    profile.requests.len()
                ),
                severity: SecuritySeverity::Medium,
            };

            self.log_security_event(&event);
        }

        // Check for endpoint scanning
        let unique_endpoints: std::collections::HashSet<_> = profile
            .endpoints_accessed
            .iter()
            .map(|(_, e)| e.clone())
            .collect();

        if unique_endpoints.len() as u32 >= self.config.endpoint_scan_threshold {
            let event = SecurityEvent {
                event_type: SecurityEventType::EndpointScanning,
                timestamp: now,
                client_ip: Some(client_ip),
                user_id: None,
                team_id: None,
                details: format!(
                    "Potential endpoint scanning: {} unique endpoints in {} seconds",
                    unique_endpoints.len(),
                    self.config.endpoint_scan_window_secs
                ),
                severity: SecuritySeverity::High,
            };

            self.log_security_event(&event);
            profile.alert_count += 1;
        }
    }

    /// Check if an IP is currently blocked
    pub async fn is_blocked(&self, client_ip: IpAddr) -> bool {
        let profiles = self.profiles.read().await;
        if let Some(profile) = profiles.get(&client_ip) {
            if let Some(blocked_until) = profile.blocked_until {
                return Instant::now() < blocked_until;
            }
        }
        false
    }

    /// Evict profiles that have had no activity within `max_age`.
    ///
    /// A profile is kept when:
    /// - It is currently under an active block (we must honour the block), or
    /// - Its most recent request or failed-auth attempt falls within `max_age`.
    pub async fn cleanup_stale_profiles(&self, max_age: Duration) {
        let mut profiles = self.profiles.write().await;
        let now = Instant::now();
        let before = profiles.len();
        profiles.retain(|_, profile| {
            // Never remove a profile that is still under an active block.
            if profile.blocked_until.map_or(false, |t| now < t) {
                return true;
            }
            // Keep if there was a recent ordinary request.
            let recent_request = profile
                .requests
                .last()
                .map_or(false, |t| now.duration_since(*t) < max_age);
            // Keep if there was a recent failed-auth attempt.
            let recent_auth_fail = profile
                .failed_auth_attempts
                .last()
                .map_or(false, |t| now.duration_since(*t) < max_age);
            recent_request || recent_auth_fail
        });
        let removed = before.saturating_sub(profiles.len());
        if removed > 0 {
            tracing::debug!(
                removed = removed,
                remaining = profiles.len(),
                "Evicted stale breach-detection profiles"
            );
        }
    }

    /// Log a security event
    fn log_security_event(&self, event: &SecurityEvent) {
        match event.severity {
            SecuritySeverity::Critical => {
                tracing::error!(
                    event_type = %event.event_type,
                    severity = %event.severity,
                    ip = ?event.client_ip,
                    user_id = ?event.user_id,
                    details = %event.details,
                    "SECURITY ALERT: Critical security event detected"
                );
            }
            SecuritySeverity::High => {
                tracing::warn!(
                    event_type = %event.event_type,
                    severity = %event.severity,
                    ip = ?event.client_ip,
                    user_id = ?event.user_id,
                    details = %event.details,
                    "SECURITY ALERT: High severity security event"
                );
            }
            SecuritySeverity::Medium => {
                tracing::info!(
                    event_type = %event.event_type,
                    severity = %event.severity,
                    ip = ?event.client_ip,
                    details = %event.details,
                    "Security event detected"
                );
            }
            SecuritySeverity::Low => {
                tracing::debug!(
                    event_type = %event.event_type,
                    severity = %event.severity,
                    ip = ?event.client_ip,
                    details = %event.details,
                    "Security event"
                );
            }
        }
    }

    /// Get security statistics
    pub async fn get_stats(&self) -> BreachDetectionStats {
        let profiles = self.profiles.read().await;
        let now = Instant::now();

        let total_profiles = profiles.len();
        let blocked_count = profiles
            .values()
            .filter(|p| p.blocked_until.map_or(false, |t| now < t))
            .count();
        let total_alerts: u32 = profiles.values().map(|p| p.alert_count).sum();

        BreachDetectionStats {
            total_monitored_ips: total_profiles,
            currently_blocked: blocked_count,
            total_alerts,
        }
    }
}

/// Statistics for breach detection
#[derive(Debug, Clone)]
pub struct BreachDetectionStats {
    pub total_monitored_ips: usize,
    pub currently_blocked: usize,
    pub total_alerts: u32,
}

/// Breach detection middleware
///
/// Monitors requests for suspicious patterns and logs security events
pub async fn breach_detection_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let client_ip = addr.ip();

    // Check if IP is blocked
    if let Some(detector) = state.breach_detector.as_ref() {
        if detector.is_blocked(client_ip).await {
            tracing::warn!(ip = %client_ip, "Blocked IP attempted request");
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from("Access denied due to suspicious activity"))
                .unwrap();
        }

        // Record the request
        let endpoint = request.uri().path().to_string();
        let user_id = request
            .extensions()
            .get::<crate::middleware::auth::Auth>()
            .map(|auth| auth.user_id);

        detector.record_request(client_ip, &endpoint, user_id).await;
    }

    next.run(request).await
}

/// Record a failed authentication attempt
///
/// Call this from authentication handlers when auth fails
pub async fn record_auth_failure(
    state: &AppState,
    client_ip: IpAddr,
    user_id: Option<Uuid>,
    reason: &str,
) {
    if let Some(detector) = state.breach_detector.as_ref() {
        detector.record_failed_auth(client_ip, user_id).await;

        tracing::warn!(
            ip = %client_ip,
            user_id = ?user_id,
            reason = %reason,
            "Authentication failure recorded"
        );
    }
}

/// Record a privilege escalation attempt
pub async fn record_privilege_escalation(
    state: &AppState,
    client_ip: IpAddr,
    user_id: Uuid,
    attempted_action: &str,
) {
    if let Some(detector) = state.breach_detector.as_ref() {
        let event = SecurityEvent {
            event_type: SecurityEventType::PrivilegeEscalationAttempt,
            timestamp: Instant::now(),
            client_ip: Some(client_ip),
            user_id: Some(user_id),
            team_id: None,
            details: format!("User attempted unauthorized action: {}", attempted_action),
            severity: SecuritySeverity::High,
        };

        detector.log_security_event(&event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_breach_detection_config_default() {
        let config = BreachDetectionConfig::default();
        assert_eq!(config.failed_auth_threshold, 5);
        assert_eq!(config.failed_auth_window_secs, 300);
        assert!(!config.auto_block_enabled);
    }

    #[test]
    fn test_security_severity_ordering() {
        assert!(SecuritySeverity::Low < SecuritySeverity::Medium);
        assert!(SecuritySeverity::Medium < SecuritySeverity::High);
        assert!(SecuritySeverity::High < SecuritySeverity::Critical);
    }

    #[test]
    fn test_security_event_type_display() {
        assert_eq!(
            SecurityEventType::FailedAuthentication.to_string(),
            "failed_authentication"
        );
        assert_eq!(
            SecurityEventType::EndpointScanning.to_string(),
            "endpoint_scanning"
        );
    }

    #[tokio::test]
    async fn test_breach_detector_failed_auth() {
        let config = BreachDetectionConfig {
            failed_auth_threshold: 3,
            failed_auth_window_secs: 60,
            auto_block_enabled: false,
            ..Default::default()
        };

        let detector = BreachDetector::new(config);
        let ip = IpAddr::from([192, 168, 1, 1]);

        // Record 2 failures (below threshold)
        detector.record_failed_auth(ip, None).await;
        detector.record_failed_auth(ip, None).await;

        assert!(!detector.is_blocked(ip).await);

        // Record 1 more failure (at threshold)
        detector.record_failed_auth(ip, None).await;

        // Should not be blocked since auto_block is disabled
        assert!(!detector.is_blocked(ip).await);
    }

    #[tokio::test]
    async fn test_breach_detector_auto_block() {
        let config = BreachDetectionConfig {
            failed_auth_threshold: 2,
            failed_auth_window_secs: 60,
            auto_block_enabled: true,
            block_duration_secs: 1, // Short for testing
            ..Default::default()
        };

        let detector = BreachDetector::new(config);
        let ip = IpAddr::from([192, 168, 1, 1]);

        // Record failures to trigger block
        detector.record_failed_auth(ip, None).await;
        detector.record_failed_auth(ip, None).await;

        // Should be blocked
        assert!(detector.is_blocked(ip).await);

        // Wait for block to expire
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Should no longer be blocked
        assert!(!detector.is_blocked(ip).await);
    }
}
