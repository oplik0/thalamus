//! MCP API DTOs

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

use crate::features::mcp::domain::{
    McpAuthConfig, McpServer, McpServerCreate, McpServerUpdate, McpTool, McpToolCallResult,
    McpToolset, McpTransport,
};

/// Request to create an MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateMcpServerRequest {
    pub alias: String,
    pub name: Option<String>,
    pub transport: McpTransport,
    pub url: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub auth: McpAuthConfig,
    #[serde(default)]
    pub static_headers: HashMap<String, String>,
    #[serde(default)]
    pub extra_headers: Vec<String>,
    pub timeout: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub allowed_scopes: Vec<String>,
    pub team_id: Option<Uuid>,
}

impl CreateMcpServerRequest {
    pub fn into_domain(self, created_by: Option<Uuid>) -> McpServerCreate {
        McpServerCreate {
            alias: self.alias,
            name: self.name,
            transport: self.transport,
            url: self.url,
            command: self.command,
            args: self.args,
            env: self.env,
            auth: self.auth,
            static_headers: self.static_headers,
            extra_headers: self.extra_headers,
            timeout: self.timeout,
            description: self.description,
            allowed_scopes: self.allowed_scopes,
            team_id: self.team_id,
            created_by,
        }
    }
}

/// Request to update an MCP server.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateMcpServerRequest {
    pub alias: Option<String>,
    pub name: Option<String>,
    pub transport: Option<McpTransport>,
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub auth: Option<McpAuthConfig>,
    pub static_headers: Option<HashMap<String, String>>,
    pub extra_headers: Option<Vec<String>>,
    pub timeout: Option<String>,
    pub description: Option<String>,
    pub allowed_scopes: Option<Vec<String>>,
    pub team_id: Option<Uuid>,
    pub is_enabled: Option<bool>,
}

impl UpdateMcpServerRequest {
    pub fn into_domain(self) -> McpServerUpdate {
        McpServerUpdate {
            alias: self.alias,
            name: self.name.map(Some),
            transport: self.transport,
            url: self.url.map(Some),
            command: self.command.map(Some),
            args: self.args,
            env: self.env,
            auth: self.auth,
            static_headers: self.static_headers,
            extra_headers: self.extra_headers,
            timeout: self.timeout,
            description: self.description.map(Some),
            allowed_scopes: self.allowed_scopes,
            team_id: self.team_id.map(Some),
            is_enabled: self.is_enabled,
        }
    }
}

/// Masked authentication configuration for API responses.
///
/// Never exposes raw credentials. Only reveals the auth *type* and
/// non-secret metadata so callers can understand how a server is
/// authenticated without being able to impersonate it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpAuthConfigResponse {
    None,
    BearerToken,
    ApiKey { header: String },
    Headers { header_names: Vec<String> },
}

impl From<McpAuthConfig> for McpAuthConfigResponse {
    fn from(auth: McpAuthConfig) -> Self {
        match auth {
            McpAuthConfig::None => Self::None,
            McpAuthConfig::BearerToken { .. } => Self::BearerToken,
            McpAuthConfig::ApiKey { header, .. } => Self::ApiKey { header },
            McpAuthConfig::Headers { headers } => Self::Headers {
                header_names: headers.keys().cloned().collect(),
            },
        }
    }
}

/// MCP server response.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerResponse {
    pub id: Uuid,
    pub alias: String,
    pub name: Option<String>,
    pub transport: String,
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub auth: McpAuthConfigResponse,
    pub static_headers: HashMap<String, String>,
    pub extra_headers: Vec<String>,
    pub timeout: String,
    pub description: Option<String>,
    pub allowed_scopes: Vec<String>,
    pub team_id: Option<Uuid>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<McpServer> for McpServerResponse {
    fn from(s: McpServer) -> Self {
        Self {
            id: s.id,
            alias: s.alias,
            name: s.name,
            transport: s.transport.to_string(),
            url: s.url,
            command: s.command,
            args: s.args,
            env: s.env,
            auth: s.auth.into(),
            static_headers: s.static_headers,
            extra_headers: s.extra_headers,
            timeout: s.timeout,
            description: s.description,
            allowed_scopes: s.allowed_scopes,
            team_id: s.team_id,
            created_by: s.created_by,
            created_at: s.created_at,
            updated_at: s.updated_at,
        }
    }
}

/// Response for listing MCP servers.
#[derive(Debug, Clone, Serialize)]
pub struct ListMcpServersResponse {
    pub servers: Vec<McpServerResponse>,
}

/// Response for listing tools.
#[derive(Debug, Clone, Serialize)]
pub struct ListMcpToolsResponse {
    pub server_id: String,
    pub tools: Vec<McpTool>,
}

/// Request to call a tool via the REST API.
#[derive(Debug, Clone, Deserialize)]
pub struct CallMcpToolRequest {
    pub server_id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Response for a tool call.
#[derive(Debug, Clone, Serialize)]
pub struct CallMcpToolResponse {
    pub server_id: String,
    pub name: String,
    pub result: McpToolCallResult,
}

/// Request to create a toolset.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateMcpToolsetRequest {
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
}

/// Toolset response.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolsetResponse {
    pub id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub is_auto: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<McpToolset> for McpToolsetResponse {
    fn from(t: McpToolset) -> Self {
        Self {
            id: t.id,
            team_id: t.team_id,
            name: t.name,
            slug: t.slug,
            description: t.description,
            is_auto: t.is_auto,
            created_at: t.created_at,
            updated_at: t.updated_at,
        }
    }
}

/// Item in a toolset response.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolsetItemResponse {
    pub server_id: Uuid,
    pub server_alias: String,
    pub tool_name: Option<String>,
}

/// Detailed toolset response including items.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolsetDetailResponse {
    #[serde(flatten)]
    pub toolset: McpToolsetResponse,
    pub items: Vec<McpToolsetItemResponse>,
}

/// Request to add an item to a toolset.
#[derive(Debug, Clone, Deserialize)]
pub struct AddToolsetItemRequest {
    pub server_id: Uuid,
    pub tool_name: Option<String>,
}

/// Request to remove an item from a toolset.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoveToolsetItemRequest {
    pub server_id: Uuid,
    pub tool_name: Option<String>,
}

/// Toolset with resolved tools, suitable for LLM clients.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolsetToolsResponse {
    pub toolset: McpToolsetResponse,
    pub tools: Vec<crate::features::mcp::domain::ToolsetToolEntry>,
}
