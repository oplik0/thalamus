//! MCP domain types and traits

use crate::error::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Transport protocol for an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum McpTransport {
    Http,
    Sse,
    Stdio,
}

impl std::fmt::Display for McpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http => write!(f, "http"),
            Self::Sse => write!(f, "sse"),
            Self::Stdio => write!(f, "stdio"),
        }
    }
}

impl std::str::FromStr for McpTransport {
    type Err = crate::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "sse" => Ok(Self::Sse),
            "stdio" => Ok(Self::Stdio),
            _ => Err(crate::Error::InvalidInput(format!(
                "Unknown MCP transport: {s}"
            ))),
        }
    }
}

/// Authentication configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpAuthConfig {
    None,
    BearerToken { token: String },
    ApiKey { header: String, token: String },
    Headers { headers: HashMap<String, String> },
}

impl Default for McpAuthConfig {
    fn default() -> Self {
        Self::None
    }
}

/// Persistent MCP server entity.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct McpServer {
    pub id: Uuid,
    pub alias: String,
    pub name: Option<String>,
    pub transport: McpTransport,
    pub url: Option<String>,
    pub command: Option<String>,
    #[sqlx(json)]
    pub args: Vec<String>,
    #[sqlx(json)]
    pub env: HashMap<String, String>,
    #[sqlx(json)]
    pub auth: McpAuthConfig,
    #[sqlx(json)]
    pub static_headers: HashMap<String, String>,
    #[sqlx(json)]
    pub extra_headers: Vec<String>,
    pub timeout: String,
    pub description: Option<String>,
    #[sqlx(json)]
    pub allowed_scopes: Vec<String>,
    pub team_id: Option<Uuid>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub is_enabled: bool,
    pub is_healthy: Option<bool>,
    pub last_health_check_at: Option<DateTime<Utc>>,
    pub consecutive_health_failures: i32,
    pub last_tool_sync_at: Option<DateTime<Utc>>,
}

/// Input for creating a server in the repository.
#[derive(Debug, Clone)]
pub struct McpServerCreate {
    pub alias: String,
    pub name: Option<String>,
    pub transport: McpTransport,
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub auth: McpAuthConfig,
    pub static_headers: HashMap<String, String>,
    pub extra_headers: Vec<String>,
    pub timeout: Option<String>,
    pub description: Option<String>,
    pub allowed_scopes: Vec<String>,
    pub team_id: Option<Uuid>,
    pub created_by: Option<Uuid>,
}

/// Input for updating a server.
#[derive(Debug, Clone, Default)]
pub struct McpServerUpdate {
    pub alias: Option<String>,
    pub name: Option<Option<String>>,
    pub transport: Option<McpTransport>,
    pub url: Option<Option<String>>,
    pub command: Option<Option<String>>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub auth: Option<McpAuthConfig>,
    pub static_headers: Option<HashMap<String, String>>,
    pub extra_headers: Option<Vec<String>>,
    pub timeout: Option<String>,
    pub description: Option<Option<String>>,
    pub allowed_scopes: Option<Vec<String>>,
    pub team_id: Option<Option<Uuid>>,
    pub is_enabled: Option<bool>,
}

/// Tool metadata returned by an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Content item returned by a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolContent {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Content fields beyond `type` and `text` (for example image, audio,
    /// resource, and embedded-resource payloads). Preserve these instead of
    /// silently discarding newer MCP content variants at the gateway boundary.
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One tool entry in a resolved toolset.
#[derive(Debug, Clone, Serialize)]
pub struct ToolsetToolEntry {
    pub server_id: String,
    pub server_alias: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Result of calling an MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallResult {
    pub content: Vec<McpToolContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
}

/// Server info returned by initialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub protocol_version: String,
}

/// MCP client abstraction.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// Return server metadata from the completed MCP initialization handshake.
    async fn initialize(&self) -> Result<McpServerInfo>;

    /// List tools exposed by the server.
    async fn list_tools(&self) -> Result<Vec<McpTool>>;

    /// Call a tool by name with JSON arguments.
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolCallResult>;
}

/// Factory for creating MCP clients bound to a server configuration.
#[async_trait]
pub trait McpClientFactory: Send + Sync {
    /// Create a client for the given server configuration. Implementations may
    /// establish the handshake eagerly; callers obtain its metadata through
    /// `McpClient::initialize`.
    async fn create(&self, server: &McpServer) -> Result<Box<dyn McpClient>>;
}

/// Repository for MCP server persistence.
#[async_trait]
pub trait McpServerRepository: Send + Sync {
    /// List servers visible to a team. If `team_id` is None, only global servers are returned.
    /// If `include_global` is true and `team_id` is provided, both team and global servers are
    /// returned.
    async fn list(&self, team_id: Option<Uuid>, include_global: bool) -> Result<Vec<McpServer>>;

    /// Get a server by alias.
    async fn get_by_alias(&self, alias: &str) -> Result<Option<McpServer>>;

    /// Get a server by id.
    async fn get_by_id(&self, id: Uuid) -> Result<Option<McpServer>>;

    /// Create a new server.
    async fn create(&self, server: McpServerCreate) -> Result<McpServer>;

    /// Update an existing server.
    async fn update(&self, id: Uuid, update: McpServerUpdate) -> Result<McpServer>;

    /// Soft-delete a server.
    async fn delete(&self, id: Uuid) -> Result<()>;
}

/// Toolset entity.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct McpToolset {
    pub id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub is_auto: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Input for creating a toolset.
#[derive(Debug, Clone)]
pub struct McpToolsetCreate {
    pub team_id: Uuid,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub is_auto: bool,
}

/// One item in a toolset.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct McpToolsetItem {
    pub id: Uuid,
    pub toolset_id: Uuid,
    pub server_id: Uuid,
    pub tool_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Repository for MCP toolsets.
#[async_trait]
pub trait McpToolsetRepository: Send + Sync {
    /// List toolsets for a team.
    async fn list_for_team(&self, team_id: Uuid) -> Result<Vec<McpToolset>>;

    /// Get a toolset by team and slug.
    async fn get_by_team_slug(&self, team_id: Uuid, slug: &str) -> Result<Option<McpToolset>>;

    /// Get a toolset by id.
    async fn get_by_id(&self, id: Uuid) -> Result<Option<McpToolset>>;

    /// Create a toolset.
    async fn create(&self, toolset: McpToolsetCreate) -> Result<McpToolset>;

    /// Add a server (or specific tool) to a toolset.
    async fn add_item(
        &self,
        toolset_id: Uuid,
        server_id: Uuid,
        tool_name: Option<String>,
    ) -> Result<()>;

    /// Remove a server/tool from a toolset.
    async fn remove_item(
        &self,
        toolset_id: Uuid,
        server_id: Uuid,
        tool_name: Option<String>,
    ) -> Result<()>;

    /// List items in a toolset.
    async fn list_items(&self, toolset_id: Uuid) -> Result<Vec<McpToolsetItem>>;

    /// Delete a toolset.
    async fn delete(&self, id: Uuid) -> Result<()>;

    /// Ensure the auto toolset for a team exists and contains all visible servers.
    async fn sync_team_auto_toolset(&self, team_id: Uuid) -> Result<McpToolset>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Session management
// ─────────────────────────────────────────────────────────────────────────────

/// An active, pooled session to an MCP server.
pub struct McpSession {
    pub server_id: Uuid,
    pub client: Box<dyn McpClient>,
    pub server_info: McpServerInfo,
    pub connected_at: Instant,
    last_used_ms: AtomicU64,
    active_requests: AtomicU64,
}

impl McpSession {
    /// Create a new session with `last_used` set to now.
    #[must_use]
    pub fn new(
        server_id: Uuid,
        client: Box<dyn McpClient>,
        server_info: McpServerInfo,
        connected_at: Instant,
    ) -> Self {
        let now_ms = now_millis(connected_at);
        Self {
            server_id,
            client,
            server_info,
            connected_at,
            last_used_ms: AtomicU64::new(now_ms),
            active_requests: AtomicU64::new(0),
        }
    }

    /// Mark this session as used right now.
    pub fn touch(&self) {
        self.last_used_ms
            .store(now_millis(Instant::now()), Ordering::Relaxed);
    }

    /// Time since last use.
    pub fn idle_duration(&self) -> Duration {
        let last_ms = self.last_used_ms.load(Ordering::Relaxed);
        let now_ms = now_millis(Instant::now());
        Duration::from_millis(now_ms.saturating_sub(last_ms))
    }

    /// Mark an in-flight operation so TTL eviction cannot replace a live
    /// streamable-HTTP session during a long-running tool call.
    pub fn acquire(&self) -> McpSessionLease<'_> {
        self.active_requests.fetch_add(1, Ordering::Relaxed);
        self.touch();
        McpSessionLease { session: self }
    }

    /// A session can be evicted only when no request is using it and its idle
    /// deadline has passed.
    pub fn is_evictable(&self, ttl: Duration) -> bool {
        self.active_requests.load(Ordering::Relaxed) == 0 && self.idle_duration() >= ttl
    }
}

/// RAII lease for a session operation. Dropping the lease marks the session as
/// recently used after the request finishes, including error paths.
#[derive(Debug)]
pub struct McpSessionLease<'a> {
    session: &'a McpSession,
}

impl Drop for McpSessionLease<'_> {
    fn drop(&mut self) {
        self.session.active_requests.fetch_sub(1, Ordering::Relaxed);
        self.session.touch();
    }
}

fn now_millis(t: Instant) -> u64 {
    // Use elapsed since an arbitrary anchor (process start is fine).
    // We only ever compare against itself, so absolute epoch doesn't matter.
    static ANCHOR: OnceLock<Instant> = OnceLock::new();
    let anchor = ANCHOR.get_or_init(|| t);
    u64::try_from(t.saturating_duration_since(*anchor).as_millis()).unwrap_or(u64::MAX)
}

impl std::fmt::Debug for McpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSession")
            .field("server_id", &self.server_id)
            .field("client", &"<dyn McpClient>")
            .field("server_info", &self.server_info)
            .field("connected_at", &self.connected_at)
            .field("last_used_ms", &self.last_used_ms.load(Ordering::Relaxed))
            .field(
                "active_requests",
                &self.active_requests.load(Ordering::Relaxed),
            )
            .finish()
    }
}

/// Manages a pool of MCP sessions.
#[async_trait]
pub trait McpSessionManager: Send + Sync {
    /// Get or create a session for the given server.
    async fn get_or_connect(&self, server: &McpServer) -> Result<Arc<McpSession>>;

    /// Invalidate and remove a session (e.g., after a health check failure).
    fn invalidate(&self, server_id: Uuid);

    /// Evict all sessions for this server (called on disable/delete).
    fn evict(&self, server_id: Uuid);

    /// Run a health check against the given server. Returns true if healthy.
    async fn health_check(&self, server: &McpServer) -> Result<bool>;

    /// Number of active sessions.
    fn session_count(&self) -> usize;
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Validate an MCP alias: lowercase/uppercase letters, digits, underscore, hyphen.
pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty() {
        return Err(crate::Error::InvalidInput(
            "alias cannot be empty".to_string(),
        ));
    }
    if alias.len() > 64 {
        return Err(crate::Error::InvalidInput(
            "alias must be at most 64 characters".to_string(),
        ));
    }
    if !alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(crate::Error::InvalidInput(
            "alias must be alphanumeric, underscore, or hyphen".to_string(),
        ));
    }
    Ok(())
}

/// Build a prefixed tool name for a server alias.
pub fn prefixed_tool_name(alias: &str, tool_name: &str, separator: &str) -> String {
    format!("{alias}{separator}{tool_name}")
}

/// Strip an alias prefix from a tool name.
pub fn strip_alias_prefix<'a>(alias: &str, prefixed: &'a str, separator: &str) -> Option<&'a str> {
    let prefix = format!("{alias}{separator}");
    prefixed.strip_prefix(&prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_alias() {
        assert!(validate_alias("places_api").is_ok());
        assert!(validate_alias("Places-API_2").is_ok());
        assert!(validate_alias("").is_err());
        assert!(validate_alias("places.api").is_err());
    }

    #[test]
    fn test_prefixed_tool_name() {
        assert_eq!(
            prefixed_tool_name("places", "getPlaces", "-"),
            "places-getPlaces"
        );
    }

    #[test]
    fn test_strip_alias_prefix() {
        assert_eq!(
            strip_alias_prefix("places", "places-getPlaces", "-"),
            Some("getPlaces")
        );
        assert_eq!(strip_alias_prefix("places", "getPlaces", "-"), None);
    }
}
