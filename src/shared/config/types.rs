//! Configuration type definitions
//!
//! These types mirror the KCL schemas defined in pkg/schemas.k

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub workers: Option<usize>,
    /// Public base URL used for OAuth callbacks and similar redirects.
    /// Falls back to `http://{host}:{port}` when not set.
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub pool_timeout: String,
    pub idle_timeout: String,
    pub max_lifetime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    None,
    BearerToken { token: String },
    ApiKey { header: String, token: String },
    Basic { username: String, password: String },
    Custom { token: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub enabled: bool,
    pub interval: String,
    pub timeout: String,
    pub endpoint: Option<String>,
    pub unhealthy_threshold: u32,
    pub healthy_threshold: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub url: String,
    pub capacity: u32,
    pub models: Vec<String>,
    #[serde(default)]
    pub currently_loaded_models: Vec<String>,
    #[serde(default)]
    pub model_loading_aware: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_delay: String,
    pub max_delay: String,
    pub exponential_backoff: bool,
    #[serde(default = "default_true")]
    pub retry_on_timeout: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub endpoints: Vec<EndpointConfig>,
    pub auth: AuthConfig,
    pub health_check: Option<HealthCheckConfig>,
    pub retry_config: Option<RetryConfig>,
    #[serde(default = "default_timeout")]
    pub timeout: String,
}

fn default_timeout() -> String {
    "30s".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub prefer_loaded_models: bool,
    #[serde(default = "default_true")]
    pub consider_queue_depth: bool,
    #[serde(default = "default_fallback")]
    pub fallback_strategy: String,
    #[serde(default = "default_hysteresis")]
    pub hysteresis_threshold: f64,
    #[serde(default)]
    pub health_weighted: bool,
    #[serde(default = "default_true")]
    pub admission_control: bool,
}

fn default_hysteresis() -> f64 {
    0.10
}

fn default_fallback() -> String {
    "round_robin".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    pub priority: u32,
    pub max_queue_size: u32,
    pub timeout: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub strategy: StrategyConfig,
    pub priority_queues: HashMap<String, QueueConfig>,
    #[serde(default = "default_queue_name")]
    pub default_queue: String,
}

fn default_queue_name() -> String {
    "realtime".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_format")]
    pub format: String,
    pub otlp_endpoint: Option<String>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_format() -> String {
    "json".to_string()
}

fn default_sample_rate() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_metrics_endpoint")]
    pub prometheus_endpoint: String,
    #[serde(default = "default_collection_interval")]
    pub collection_interval: String,
    #[serde(default = "default_true")]
    pub include_per_backend: bool,
    #[serde(default = "default_true")]
    pub include_per_model: bool,
}

fn default_metrics_endpoint() -> String {
    "/metrics".to_string()
}

fn default_collection_interval() -> String {
    "10s".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    pub tracing: TracingConfig,
    pub metrics: MetricsConfig,
    pub logging_per_team: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub redis_url: String,
    #[serde(default = "default_ttl")]
    pub default_ttl: String,
    pub max_memory: Option<String>,
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
}

fn default_ttl() -> String {
    "300s".to_string()
}

fn default_key_prefix() -> String {
    "thalamus:".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_algorithm")]
    pub algorithm: String,
    #[serde(default = "default_rpm")]
    pub default_requests_per_minute: u32,
    #[serde(default = "default_burst")]
    pub burst_size: u32,
    pub per_team_limits: Option<HashMap<String, u32>>,
}

fn default_algorithm() -> String {
    "token_bucket".to_string()
}

fn default_rpm() -> u32 {
    60
}

fn default_burst() -> u32 {
    10
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TeamMappingConfig {
    #[serde(default = "default_true")]
    pub auto_create_team: bool,
    pub default_team_id: Option<String>,
    #[serde(default)]
    pub org_to_team_mapping: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OAuthProviderType {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "github_enterprise")]
    GitHubEnterprise,
    #[serde(rename = "oidc")]
    Oidc,
}

fn default_oauth_scopes() -> Vec<String> {
    vec!["read:user".to_string(), "user:email".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProvider {
    pub name: String,
    #[serde(rename = "provider_type")]
    pub provider_type: OAuthProviderType,
    pub client_id: String,
    pub client_secret: String,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub userinfo_endpoint: Option<String>,
    pub redirect_uri: Option<String>,
    #[serde(default = "default_oauth_scopes")]
    pub scopes: Vec<String>,
    pub enterprise_url: Option<String>,
    #[serde(default)]
    pub team_mapping: TeamMappingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    pub api_key_secret: String,
    pub paseto_secret_key: String,
    pub opaque_server_setup: String,
}

/// Root configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub backends: HashMap<String, BackendConfig>,
    pub routing: RoutingConfig,
    pub observability: ObservabilityConfig,
    pub cache: Option<CacheConfig>,
    pub rate_limiting: Option<RateLimitConfig>,
    #[serde(default)]
    pub oauth_providers: Vec<OAuthProvider>,
    pub security: SecurityConfig,
}

impl Config {
    /// Validate the configuration
    ///
    /// # Errors
    /// Returns an error if any configuration value is invalid
    pub fn validate(&self) -> crate::Result<()> {
        if self.server.port == 0 {
            return Err(crate::Error::Config("Server port cannot be 0".to_string()));
        }

        if self.backends.is_empty() {
            return Err(crate::Error::Config(
                "At least one backend must be configured".to_string(),
            ));
        }

        if !self.database.url.starts_with("postgres://")
            && !self.database.url.starts_with("postgresql://")
        {
            return Err(crate::Error::Config(
                "Only PostgreSQL databases are supported".to_string(),
            ));
        }

        if !self
            .routing
            .priority_queues
            .contains_key(&self.routing.default_queue)
        {
            return Err(crate::Error::Config(format!(
                "Default queue '{}' does not exist in priority_queues",
                self.routing.default_queue
            )));
        }

        // Reject the insecure "dev" placeholder in production.
        // Check APP_ENV for "production" or "prod" (case-insensitive).
        let app_env = std::env::var("APP_ENV").unwrap_or_default();
        let app_env = app_env.trim().to_ascii_lowercase();
        if (app_env == "production" || app_env == "prod")
            && self.security.opaque_server_setup == "dev"
        {
            return Err(crate::Error::Config(
                "opaque_server_setup must not be \"dev\" in production; \
                 generate a real server setup and store it in config"
                    .to_string(),
            ));
        }

        Ok(())
    }
}
