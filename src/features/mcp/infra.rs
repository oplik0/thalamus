//! MCP infrastructure implementations
//!
//! Sections:
//! 1. SQLx repositories
//! 2. rmcp transport client (Streamable HTTP and stdio)
//! 3. Session pool (McpSessionManager)
//! 4. Health monitor
//! 5. McpService

use crate::bootstrap::parse_duration;
use crate::error::{Error, Result};
use crate::features::mcp::domain::{
    McpAuthConfig, McpClient, McpClientFactory, McpServer, McpServerCreate, McpServerInfo,
    McpServerRepository, McpServerUpdate, McpSession, McpSessionManager, McpTool,
    McpToolCallResult, McpToolContent, McpToolset, McpToolsetCreate, McpToolsetItem,
    McpToolsetRepository, McpTransport, ToolsetToolEntry,
};
use async_trait::async_trait;
use dashmap::DashMap;
use reqwest::{Client, header};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, ClientRequest, Implementation,
    ListToolsRequest, PaginatedRequestParams, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::{
    ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess,
    streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// 1. SQLx MCP Server Repository
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SqlxMcpServerRepository {
    pool: PgPool,
}

impl SqlxMcpServerRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl McpServerRepository for SqlxMcpServerRepository {
    async fn list(&self, team_id: Option<Uuid>, include_global: bool) -> Result<Vec<McpServer>> {
        let rows = if let Some(team_id) = team_id {
            if include_global {
                sqlx::query_as::<_, McpServer>(
                    r#"
                    SELECT *
                    FROM mcp_servers
                    WHERE deleted_at IS NULL
                      AND is_enabled = TRUE
                      AND (team_id = $1 OR team_id IS NULL)
                    ORDER BY alias
                    "#,
                )
                .bind(team_id)
                .fetch_all(&self.pool)
                .await
            } else {
                sqlx::query_as::<_, McpServer>(
                    r#"
                    SELECT *
                    FROM mcp_servers
                    WHERE deleted_at IS NULL
                      AND is_enabled = TRUE
                      AND team_id = $1
                    ORDER BY alias
                    "#,
                )
                .bind(team_id)
                .fetch_all(&self.pool)
                .await
            }
        } else {
            sqlx::query_as::<_, McpServer>(
                r#"
                SELECT *
                FROM mcp_servers
                WHERE deleted_at IS NULL
                  AND is_enabled = TRUE
                  AND team_id IS NULL
                ORDER BY alias
                "#,
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(Error::Database)?;

        Ok(rows)
    }

    async fn get_by_alias(&self, alias: &str) -> Result<Option<McpServer>> {
        let row = sqlx::query_as::<_, McpServer>(
            r#"
            SELECT *
            FROM mcp_servers
            WHERE alias = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(alias)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(row)
    }

    async fn get_by_id(&self, id: Uuid) -> Result<Option<McpServer>> {
        let row = sqlx::query_as::<_, McpServer>(
            r#"
            SELECT *
            FROM mcp_servers
            WHERE id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(row)
    }

    async fn create(&self, server: McpServerCreate) -> Result<McpServer> {
        let row = sqlx::query_as::<_, McpServer>(
            r#"
            INSERT INTO mcp_servers (
                alias, name, transport, url, command, args, env, auth,
                static_headers, extra_headers, timeout, description,
                allowed_scopes, team_id, created_by
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
            RETURNING *
            "#,
        )
        .bind(&server.alias)
        .bind(&server.name)
        .bind(server.transport)
        .bind(&server.url)
        .bind(&server.command)
        .bind(sqlx::types::Json(&server.args))
        .bind(sqlx::types::Json(&server.env))
        .bind(sqlx::types::Json(&server.auth))
        .bind(sqlx::types::Json(&server.static_headers))
        .bind(sqlx::types::Json(&server.extra_headers))
        .bind(server.timeout.as_deref().unwrap_or("30s"))
        .bind(&server.description)
        .bind(sqlx::types::Json(&server.allowed_scopes))
        .bind(server.team_id)
        .bind(server.created_by)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
                Error::InvalidInput(format!(
                    "MCP server alias '{}' already exists",
                    server.alias
                ))
            }
            _ => Error::Database(e),
        })?;

        Ok(row)
    }

    async fn update(&self, id: Uuid, update: McpServerUpdate) -> Result<McpServer> {
        let row = sqlx::query_as::<_, McpServer>(
            r#"
            UPDATE mcp_servers
            SET
                alias = COALESCE($2, alias),
                name = COALESCE($3, name),
                transport = COALESCE($4, transport),
                url = COALESCE($5, url),
                command = COALESCE($6, command),
                args = COALESCE($7, args),
                env = COALESCE($8, env),
                auth = COALESCE($9, auth),
                static_headers = COALESCE($10, static_headers),
                extra_headers = COALESCE($11, extra_headers),
                timeout = COALESCE($12, timeout),
                description = COALESCE($13, description),
                allowed_scopes = COALESCE($14, allowed_scopes),
                team_id = COALESCE($15, team_id),
                is_enabled = COALESCE($16, is_enabled),
                updated_at = NOW()
            WHERE id = $1 AND deleted_at IS NULL
            RETURNING *
            "#,
        )
        .bind(id)
        .bind(&update.alias)
        .bind(update.name.as_ref().map(|v| v.as_ref()))
        .bind(update.transport)
        .bind(update.url.as_ref().map(|v| v.as_ref()))
        .bind(update.command.as_ref().map(|v| v.as_ref()))
        .bind(update.args.as_ref().map(sqlx::types::Json))
        .bind(update.env.as_ref().map(sqlx::types::Json))
        .bind(update.auth.as_ref().map(sqlx::types::Json))
        .bind(update.static_headers.as_ref().map(sqlx::types::Json))
        .bind(update.extra_headers.as_ref().map(sqlx::types::Json))
        .bind(&update.timeout)
        .bind(update.description.as_ref().map(|v| v.as_ref()))
        .bind(update.allowed_scopes.as_ref().map(sqlx::types::Json))
        .bind(update.team_id.as_ref().map(|v| v.as_ref()))
        .bind(update.is_enabled)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => Error::NotFound(format!("MCP server not found: {id}")),
            sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
                Error::InvalidInput("MCP server alias already exists".to_string())
            }
            _ => Error::Database(e),
        })?;

        Ok(row)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE mcp_servers
            SET deleted_at = NOW()
            WHERE id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(())
    }
}

/// Extension methods for health/admin purposes.
impl SqlxMcpServerRepository {
    /// List ALL servers including disabled ones (for admin/health purposes).
    pub async fn list_all(&self, include_disabled: bool) -> Result<Vec<McpServer>> {
        let rows = if include_disabled {
            sqlx::query_as::<_, McpServer>(
                "SELECT * FROM mcp_servers WHERE deleted_at IS NULL ORDER BY alias",
            )
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as::<_, McpServer>(
                "SELECT * FROM mcp_servers WHERE deleted_at IS NULL AND is_enabled = TRUE ORDER BY alias",
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(Error::Database)?;

        Ok(rows)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. SQLx MCP Toolset Repository
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SqlxMcpToolsetRepository {
    pool: PgPool,
}

impl SqlxMcpToolsetRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl McpToolsetRepository for SqlxMcpToolsetRepository {
    async fn list_for_team(&self, team_id: Uuid) -> Result<Vec<McpToolset>> {
        let rows = sqlx::query_as::<_, McpToolset>(
            r#"
            SELECT *
            FROM mcp_toolsets
            WHERE team_id = $1 AND deleted_at IS NULL
            ORDER BY name
            "#,
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(rows)
    }

    async fn get_by_team_slug(&self, team_id: Uuid, slug: &str) -> Result<Option<McpToolset>> {
        let row = sqlx::query_as::<_, McpToolset>(
            r#"
            SELECT *
            FROM mcp_toolsets
            WHERE team_id = $1 AND slug = $2 AND deleted_at IS NULL
            "#,
        )
        .bind(team_id)
        .bind(slug)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(row)
    }

    async fn get_by_id(&self, id: Uuid) -> Result<Option<McpToolset>> {
        let row = sqlx::query_as::<_, McpToolset>(
            r#"
            SELECT *
            FROM mcp_toolsets
            WHERE id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(row)
    }

    async fn create(&self, toolset: McpToolsetCreate) -> Result<McpToolset> {
        let row = sqlx::query_as::<_, McpToolset>(
            r#"
            INSERT INTO mcp_toolsets (team_id, name, slug, description, is_auto)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING *
            "#,
        )
        .bind(toolset.team_id)
        .bind(&toolset.name)
        .bind(&toolset.slug)
        .bind(&toolset.description)
        .bind(toolset.is_auto)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
                Error::InvalidInput(format!(
                    "Toolset slug '{}' already exists for this team",
                    toolset.slug
                ))
            }
            _ => Error::Database(e),
        })?;

        Ok(row)
    }

    async fn add_item(
        &self,
        toolset_id: Uuid,
        server_id: Uuid,
        tool_name: Option<String>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO mcp_toolset_items (toolset_id, server_id, tool_name)
            VALUES ($1, $2, $3)
            ON CONFLICT (toolset_id, server_id, tool_name) DO NOTHING
            "#,
        )
        .bind(toolset_id)
        .bind(server_id)
        .bind(tool_name)
        .execute(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(())
    }

    async fn remove_item(
        &self,
        toolset_id: Uuid,
        server_id: Uuid,
        tool_name: Option<String>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            DELETE FROM mcp_toolset_items
            WHERE toolset_id = $1 AND server_id = $2 AND tool_name IS NOT DISTINCT FROM $3
            "#,
        )
        .bind(toolset_id)
        .bind(server_id)
        .bind(tool_name)
        .execute(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(())
    }

    async fn list_items(&self, toolset_id: Uuid) -> Result<Vec<McpToolsetItem>> {
        let rows = sqlx::query_as::<_, McpToolsetItem>(
            r#"
            SELECT *
            FROM mcp_toolset_items
            WHERE toolset_id = $1
            ORDER BY server_id, tool_name
            "#,
        )
        .bind(toolset_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(rows)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE mcp_toolsets
            SET deleted_at = NOW()
            WHERE id = $1 AND deleted_at IS NULL AND is_auto = FALSE
            "#,
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(())
    }

    async fn sync_team_auto_toolset(&self, team_id: Uuid) -> Result<McpToolset> {
        // Atomically ensure the auto toolset exists.
        sqlx::query(
            r#"
            INSERT INTO mcp_toolsets (team_id, name, slug, description, is_auto)
            VALUES ($1, 'MCP Tools', 'mcp', 'Automatically created toolset for the team', TRUE)
            ON CONFLICT (team_id, slug) WHERE deleted_at IS NULL DO NOTHING
            "#,
        )
        .bind(team_id)
        .execute(&self.pool)
        .await
        .map_err(Error::Database)?;

        let toolset = self
            .get_by_team_slug(team_id, "mcp")
            .await?
            .ok_or_else(|| Error::Internal("Failed to ensure auto toolset".to_string()))?;

        let server_repo = SqlxMcpServerRepository::new(self.pool.clone());
        let servers = server_repo.list(Some(team_id), true).await?;

        let items = self.list_items(toolset.id).await?;
        let existing: std::collections::HashSet<_> = items
            .into_iter()
            .map(|i| (i.server_id, i.tool_name))
            .collect();

        for server in servers {
            if !existing.contains(&(server.id, None)) {
                self.add_item(toolset.id, server.id, None).await?;
            }
        }

        Ok(toolset)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. RMCP transport client
// ─────────────────────────────────────────────────────────────────────────────

/// A fully initialized rmcp client. `RunningService` owns the background
/// transport task; dropping it cancels the MCP session and lets rmcp perform
/// its protocol-level cleanup.
pub struct RmcpClient {
    connection: RunningService<RoleClient, ClientInfo>,
    timeout: Duration,
}

impl std::fmt::Debug for RmcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RmcpClient")
            .field("timeout", &self.timeout)
            .field("transport_closed", &self.connection.is_closed())
            .finish()
    }
}

impl RmcpClient {
    fn new(connection: RunningService<RoleClient, ClientInfo>, timeout: Duration) -> Self {
        Self {
            connection,
            timeout,
        }
    }

    async fn request(&self, request: ClientRequest) -> Result<ServerResult> {
        let handle = self
            .connection
            .send_cancellable_request(request, PeerRequestOptions::with_timeout(self.timeout))
            .await
            .map_err(|err| Error::Backend(format!("Failed to send MCP request: {err}")))?;

        handle
            .await_response()
            .await
            .map_err(|err| Error::Backend(format!("MCP request failed: {err}")))
    }

    fn server_info(&self) -> Result<McpServerInfo> {
        let info = self.connection.peer_info().ok_or_else(|| {
            Error::Internal("rmcp client completed initialization without server info".to_string())
        })?;

        Ok(McpServerInfo {
            name: Some(info.server_info.name.clone()),
            version: Some(info.server_info.version.clone()),
            protocol_version: info.protocol_version.to_string(),
        })
    }
}

#[async_trait]
impl McpClient for RmcpClient {
    /// rmcp completes initialization during `ServiceExt::serve`; this returns
    /// the resulting negotiated server metadata rather than issuing a second,
    /// invalid initialize request on the live session.
    async fn initialize(&self) -> Result<McpServerInfo> {
        self.server_info()
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let mut cursor: Option<String> = None;
        let mut tools = Vec::new();

        loop {
            let response = self
                .request(ClientRequest::ListToolsRequest(ListToolsRequest {
                    method: Default::default(),
                    params: cursor.as_ref().map(|cursor| {
                        PaginatedRequestParams::default().with_cursor(Some(cursor.clone()))
                    }),
                    extensions: Default::default(),
                }))
                .await?;

            let ServerResult::ListToolsResult(result) = response else {
                return Err(Error::Backend(
                    "MCP server returned an unexpected response to tools/list".to_string(),
                ));
            };

            tools.extend(result.tools.into_iter().map(|tool| McpTool {
                name: tool.name.into_owned(),
                description: tool.description.map(|description| description.into_owned()),
                input_schema: Value::Object((*tool.input_schema).clone()),
            }));

            cursor = result.next_cursor;
            if cursor.is_none() {
                return Ok(tools);
            }
        }
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpToolCallResult> {
        let arguments = arguments.as_object().cloned().ok_or_else(|| {
            Error::InvalidInput("MCP tool arguments must be a JSON object".to_string())
        })?;

        let response = self
            .request(ClientRequest::CallToolRequest(
                rmcp::model::CallToolRequest::new(
                    CallToolRequestParams::new(name.to_string()).with_arguments(arguments),
                ),
            ))
            .await?;

        let ServerResult::CallToolResult(result) = response else {
            return Err(Error::Backend(
                "MCP server returned an unexpected response to tools/call".to_string(),
            ));
        };

        let content = result
            .content
            .into_iter()
            .map(rmcp_content_to_domain)
            .collect::<Result<Vec<_>>>()?;

        Ok(McpToolCallResult {
            content,
            structured_content: result.structured_content,
            is_error: result.is_error.unwrap_or(false),
        })
    }
}

fn rmcp_content_to_domain(content: rmcp::model::ContentBlock) -> Result<McpToolContent> {
    let mut value = serde_json::to_value(content)?;
    let object = value.as_object_mut().ok_or_else(|| {
        Error::Internal("rmcp serialized a tool content block as a non-object".to_string())
    })?;

    let content_type = object
        .remove("type")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| Error::Internal("rmcp tool content block has no type".to_string()))?;
    let text = object
        .remove("text")
        .and_then(|value| value.as_str().map(str::to_owned));

    Ok(McpToolContent {
        content_type,
        text,
        extra: std::mem::take(object),
    })
}

/// Factory for fully initialized rmcp clients.
#[derive(Debug, Clone)]
pub struct RmcpClientFactory {
    client: Client,
    default_timeout: Duration,
}

impl RmcpClientFactory {
    pub fn new(client: Client, default_timeout: Duration) -> Self {
        Self {
            client,
            default_timeout,
        }
    }

    async fn connect_http(
        &self,
        server: &McpServer,
        url: String,
        timeout: Duration,
    ) -> Result<RmcpClient> {
        let (default_headers, config) = streamable_http_config(server, url)?;
        let transport = StreamableHttpClientTransport::with_client(
            self.client_with_default_headers(default_headers)?,
            config,
        );
        let connection = mcp_client_info()
            .serve(transport)
            .await
            .map_err(|err| Error::Backend(format!("MCP initialization failed: {err}")))?;

        Ok(RmcpClient::new(connection, timeout))
    }

    fn client_with_default_headers(&self, headers: header::HeaderMap) -> Result<Client> {
        if headers.is_empty() {
            return Ok(self.client.clone());
        }

        // reqwest does not allow extending an existing client's default
        // headers. Rebuild the small per-session client only when a raw
        // Authorization header is required; preserve the application's normal
        // connection-pool settings.
        Client::builder()
            .pool_idle_timeout(Some(Duration::from_secs(90)))
            .pool_max_idle_per_host(32)
            .default_headers(headers)
            .build()
            .map_err(Error::from)
    }

    async fn connect_stdio(&self, server: &McpServer, timeout: Duration) -> Result<RmcpClient> {
        let command = server.command.as_ref().ok_or_else(|| {
            Error::InvalidInput("stdio MCP server requires a command".to_string())
        })?;

        let transport =
            TokioChildProcess::new(tokio::process::Command::new(command).configure(|command| {
                command.args(&server.args).envs(&server.env);
            }))
            .map_err(|err| Error::Backend(format!("Failed to start MCP server process: {err}")))?;
        let connection = mcp_client_info()
            .serve(transport)
            .await
            .map_err(|err| Error::Backend(format!("MCP initialization failed: {err}")))?;

        Ok(RmcpClient::new(connection, timeout))
    }
}

#[async_trait]
impl McpClientFactory for RmcpClientFactory {
    async fn create(&self, server: &McpServer) -> Result<Box<dyn McpClient>> {
        let timeout = parse_duration(&server.timeout).unwrap_or(self.default_timeout);
        let client = match server.transport {
            // rmcp's Streamable HTTP transport accepts both JSON and SSE
            // responses. The old Http/Sse distinction was a limitation of the
            // hand-written parser, not a distinct MCP transport.
            McpTransport::Http | McpTransport::Sse => {
                let url = server.url.clone().ok_or_else(|| {
                    Error::InvalidInput("HTTP/SSE MCP server requires a URL".to_string())
                })?;
                self.connect_http(server, url, timeout).await?
            }
            McpTransport::Stdio => self.connect_stdio(server, timeout).await?,
        };

        Ok(Box::new(client))
    }
}

fn mcp_client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("thalamus-mcp-client", env!("CARGO_PKG_VERSION")),
    )
}

fn streamable_http_config(
    server: &McpServer,
    url: String,
) -> Result<(header::HeaderMap, StreamableHttpClientTransportConfig)> {
    let mut headers = HashMap::new();
    let mut default_headers = header::HeaderMap::new();
    for (name, value) in &server.static_headers {
        insert_mcp_header(&mut headers, &mut default_headers, name, value)?;
    }

    let mut config = StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(headers)
        .reinit_on_expired_session(true);

    match &server.auth {
        McpAuthConfig::None => {}
        McpAuthConfig::BearerToken { token } => {
            if default_headers.contains_key(header::AUTHORIZATION) {
                return Err(Error::InvalidInput(
                    "MCP server config specifies both bearer authentication and an Authorization header"
                        .to_string(),
                ));
            }
            config = config.auth_header(token.clone());
        }
        McpAuthConfig::ApiKey {
            header: name,
            token,
        } => {
            insert_mcp_header(
                &mut config.custom_headers,
                &mut default_headers,
                name,
                token,
            )?;
        }
        McpAuthConfig::Headers { headers } => {
            for (name, value) in headers {
                insert_mcp_header(
                    &mut config.custom_headers,
                    &mut default_headers,
                    name,
                    value,
                )?;
            }
        }
    }

    Ok((default_headers, config))
}

fn insert_mcp_header(
    headers: &mut HashMap<header::HeaderName, header::HeaderValue>,
    default_headers: &mut header::HeaderMap,
    name: &str,
    value: &str,
) -> Result<()> {
    let name = header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|err| Error::InvalidInput(format!("Invalid MCP header name '{name}': {err}")))?;

    let value = header::HeaderValue::from_str(value).map_err(|err| {
        Error::InvalidInput(format!("Invalid value for MCP header '{name}': {err}"))
    })?;

    // rmcp owns dynamic protocol headers. Its dedicated bearer-token setting
    // covers normal OAuth/API-key use; reqwest default headers safely carry
    // Basic or vendor Authorization schemes without allowing those schemes to
    // replace MCP-Session-Id or Accept.
    if name == header::AUTHORIZATION {
        if default_headers.insert(name, value).is_some() {
            return Err(Error::InvalidInput(
                "MCP server config specifies Authorization more than once".to_string(),
            ));
        }
        return Ok(());
    }

    headers.insert(name, value);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Session Pool
// ─────────────────────────────────────────────────────────────────────────────

/// A pooled MCP session manager backed by a DashMap.
pub struct PooledMcpSessionManager {
    sessions: DashMap<Uuid, Arc<McpSession>>,
    connect_locks: DashMap<Uuid, Arc<Mutex<()>>>,
    factory: Arc<dyn McpClientFactory>,
    ttl: Duration,
}

impl PooledMcpSessionManager {
    pub fn new(factory: Arc<dyn McpClientFactory>, ttl: Duration) -> Self {
        Self {
            sessions: DashMap::new(),
            connect_locks: DashMap::new(),
            factory,
            ttl,
        }
    }
}

impl std::fmt::Debug for PooledMcpSessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledMcpSessionManager")
            .field("sessions", &self.sessions.len())
            .field("connect_locks", &self.connect_locks.len())
            .field("ttl", &self.ttl)
            .finish()
    }
}

#[async_trait]
impl McpSessionManager for PooledMcpSessionManager {
    async fn get_or_connect(&self, server: &McpServer) -> Result<Arc<McpSession>> {
        // Evict expired sessions first.
        self.sessions
            .retain(|_, session| !session.is_evictable(self.ttl));

        // Check for existing session.
        if let Some(existing) = self.sessions.get(&server.id) {
            existing.touch();
            return Ok(existing.clone());
        }

        // Serialize cold connects per server. Without this gate, N concurrent
        // first calls all complete a handshake and race to replace one cache
        // entry, leaving N-1 short-lived sessions and child processes behind.
        let connect_lock = self
            .connect_locks
            .entry(server.id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _connect_guard = connect_lock.lock().await;

        // Another caller can have connected while this caller waited.
        if let Some(existing) = self.sessions.get(&server.id) {
            existing.touch();
            return Ok(existing.clone());
        }

        // Create a new session.
        let client = self.factory.create(server).await.map_err(|e| {
            Error::Backend(format!(
                "Failed to create MCP client for {}: {e}",
                server.alias
            ))
        })?;

        let server_info = client.initialize().await.map_err(|e| {
            Error::Backend(format!(
                "Failed to initialize MCP session for {}: {e}",
                server.alias
            ))
        })?;

        let now = Instant::now();
        let session = Arc::new(McpSession::new(server.id, client, server_info, now));

        self.sessions.insert(server.id, session.clone());
        Ok(session)
    }

    fn invalidate(&self, server_id: Uuid) {
        self.sessions.remove(&server_id);
    }

    fn evict(&self, server_id: Uuid) {
        self.sessions.remove(&server_id);
    }

    async fn health_check(&self, server: &McpServer) -> Result<bool> {
        // Create a fresh short-lived client for health checks.
        let client = match self.factory.create(server).await {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };

        match tokio::time::timeout(Duration::from_secs(10), client.initialize()).await {
            Ok(Ok(_)) => Ok(true),
            _ => Ok(false),
        }
    }

    fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. Health Monitor
// ─────────────────────────────────────────────────────────────────────────────

/// Background health monitor that periodically pings all enabled MCP servers
/// and updates their health status in the database.
pub struct McpHealthMonitor {
    pool: PgPool,
    session_manager: Arc<dyn McpSessionManager>,
    interval: Duration,
    max_consecutive_failures: i32,
}

impl std::fmt::Debug for McpHealthMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpHealthMonitor")
            .field("interval", &self.interval)
            .field("max_consecutive_failures", &self.max_consecutive_failures)
            .finish()
    }
}

impl McpHealthMonitor {
    pub fn new(
        pool: PgPool,
        session_manager: Arc<dyn McpSessionManager>,
        interval: Duration,
    ) -> Self {
        Self {
            pool,
            session_manager,
            interval,
            max_consecutive_failures: 3,
        }
    }

    /// Run the health check loop until the shutdown token is cancelled.
    pub async fn run(self, shutdown: tokio_util::sync::CancellationToken) {
        tracing::info!(
            interval_secs = self.interval.as_secs(),
            "MCP health monitor started"
        );

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("MCP health monitor shutting down");
                    return;
                }
                _ = tokio::time::sleep(self.interval) => {
                    self.check_all().await;
                }
            }
        }
    }

    async fn check_all(&self) {
        let repo = SqlxMcpServerRepository::new(self.pool.clone());
        let servers = match repo.list_all(false).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "MCP health monitor failed to list servers");
                return;
            }
        };

        for server in &servers {
            self.check_one(server).await;
        }
    }

    async fn check_one(&self, server: &McpServer) {
        let was_healthy = server.is_healthy.unwrap_or(false);
        let is_now_healthy = self
            .session_manager
            .health_check(server)
            .await
            .unwrap_or(false);

        let (consecutive_failures, new_healthy) = if is_now_healthy {
            (0, Some(true))
        } else {
            let new_count = server.consecutive_health_failures + 1;
            let still_healthy =
                new_count <= self.max_consecutive_failures && server.is_healthy.unwrap_or(true);
            (
                new_count,
                if still_healthy {
                    server.is_healthy
                } else {
                    Some(false)
                },
            )
        };

        if was_healthy != new_healthy.unwrap_or(false) {
            let state = if new_healthy.unwrap_or(false) {
                "healthy"
            } else {
                "unhealthy"
            };
            tracing::warn!(
                server_id = %server.id,
                server_alias = %server.alias,
                state,
                consecutive_failures,
                "MCP server health state changed"
            );

            // Invalidate pooled sessions when transitioning to unhealthy.
            if !new_healthy.unwrap_or(false) {
                self.session_manager.invalidate(server.id);
            }
        }

        if let Err(e) = sqlx::query(
            r#"
            UPDATE mcp_servers
            SET is_healthy = $1,
                last_health_check_at = NOW(),
                consecutive_health_failures = $2
            WHERE id = $3 AND deleted_at IS NULL
            "#,
        )
        .bind(new_healthy)
        .bind(consecutive_failures)
        .bind(server.id)
        .execute(&self.pool)
        .await
        {
            tracing::warn!(
                server_id = %server.id,
                error = %e,
                "Failed to update MCP health status"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. McpService
// ─────────────────────────────────────────────────────────────────────────────

/// High-level MCP service with session pooling.
#[derive(Clone)]
pub struct McpService {
    pub server_repo: Arc<dyn McpServerRepository>,
    pub toolset_repo: Arc<dyn McpToolsetRepository>,
    pub session_manager: Arc<dyn McpSessionManager>,
}

impl std::fmt::Debug for McpService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpService")
            .field("server_repo", &"<dyn McpServerRepository>")
            .field("toolset_repo", &"<dyn McpToolsetRepository>")
            .field("session_manager", &"<dyn McpSessionManager>")
            .finish()
    }
}

impl McpService {
    pub fn new(
        server_repo: Arc<dyn McpServerRepository>,
        toolset_repo: Arc<dyn McpToolsetRepository>,
        session_manager: Arc<dyn McpSessionManager>,
    ) -> Self {
        Self {
            server_repo,
            toolset_repo,
            session_manager,
        }
    }

    /// List tools for a server, using a pooled session.
    pub async fn list_tools_for_server(&self, server: &McpServer) -> Result<Vec<McpTool>> {
        let session = self.session_manager.get_or_connect(server).await?;
        let _lease = session.acquire();
        session.client.list_tools().await
    }

    /// Call a tool on a server, using a pooled session.
    pub async fn call_tool(
        &self,
        server: &McpServer,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolCallResult> {
        let session = self.session_manager.get_or_connect(server).await?;
        let _lease = session.acquire();
        session.client.call_tool(tool_name, arguments).await
    }

    /// Resolve a team toolset and materialize all tools from its servers.
    pub async fn resolve_toolset_tools(
        &self,
        team_id: Uuid,
        slug: &str,
    ) -> Result<Vec<ToolsetToolEntry>> {
        let toolset = self
            .toolset_repo
            .get_by_team_slug(team_id, slug)
            .await?
            .ok_or_else(|| Error::NotFound(format!("Toolset not found: {slug}")))?;

        let toolset = if toolset.is_auto {
            self.toolset_repo.sync_team_auto_toolset(team_id).await?
        } else {
            toolset
        };

        let items = self.toolset_repo.list_items(toolset.id).await?;
        let servers = self.server_repo.list(Some(team_id), true).await?;
        let server_by_id: std::collections::HashMap<Uuid, &McpServer> =
            servers.iter().map(|s| (s.id, s)).collect();

        let mut entries = Vec::new();
        for item in items {
            let Some(server) = server_by_id.get(&item.server_id) else {
                continue;
            };

            let tools = self.list_tools_for_server(server).await?;
            for tool in tools {
                if let Some(ref allowed) = item.tool_name {
                    if &tool.name != allowed {
                        continue;
                    }
                }
                entries.push(ToolsetToolEntry {
                    server_id: server.id.to_string(),
                    server_alias: server.alias.clone(),
                    name: crate::features::mcp::domain::prefixed_tool_name(
                        &server.alias,
                        &tool.name,
                        "-",
                    ),
                    description: tool.description,
                    input_schema: tool.input_schema,
                });
            }
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct TestClient;

    #[async_trait]
    impl McpClient for TestClient {
        async fn initialize(&self) -> Result<McpServerInfo> {
            Ok(McpServerInfo {
                name: Some("test".to_string()),
                version: Some("1.0.0".to_string()),
                protocol_version: "2025-11-25".to_string(),
            })
        }

        async fn list_tools(&self) -> Result<Vec<McpTool>> {
            Ok(Vec::new())
        }

        async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<McpToolCallResult> {
            Ok(McpToolCallResult {
                content: Vec::new(),
                structured_content: None,
                is_error: false,
            })
        }
    }

    #[derive(Debug, Default)]
    struct CountingFactory {
        creations: AtomicUsize,
    }

    #[async_trait]
    impl McpClientFactory for CountingFactory {
        async fn create(&self, _server: &McpServer) -> Result<Box<dyn McpClient>> {
            self.creations.fetch_add(1, Ordering::SeqCst);
            // Force concurrent callers through the connect gate.
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(Box::new(TestClient))
        }
    }

    fn test_server() -> McpServer {
        let now = Utc::now();
        McpServer {
            id: Uuid::new_v4(),
            alias: "test".to_string(),
            name: None,
            transport: McpTransport::Http,
            url: Some("http://example.invalid/mcp".to_string()),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            auth: McpAuthConfig::None,
            static_headers: HashMap::new(),
            extra_headers: Vec::new(),
            timeout: "30s".to_string(),
            description: None,
            allowed_scopes: Vec::new(),
            team_id: None,
            created_by: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            is_enabled: true,
            is_healthy: None,
            last_health_check_at: None,
            consecutive_health_failures: 0,
            last_tool_sync_at: None,
        }
    }

    #[test]
    fn streamable_http_config_preserves_custom_and_raw_authorization_headers() {
        let mut server = test_server();
        server
            .static_headers
            .insert("x-tenant-id".to_string(), "team-a".to_string());
        server.auth = McpAuthConfig::ApiKey {
            header: "Authorization".to_string(),
            token: "Basic ZGVtbzpwYXNz".to_string(),
        };

        let (default_headers, config) =
            streamable_http_config(&server, "http://example.invalid/mcp".to_string()).unwrap();

        assert_eq!(
            default_headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic ZGVtbzpwYXNz"
        );
        assert_eq!(
            config
                .custom_headers
                .get(&header::HeaderName::from_static("x-tenant-id"))
                .unwrap()
                .to_str()
                .unwrap(),
            "team-a"
        );
    }

    #[tokio::test]
    async fn concurrent_get_or_connect_creates_one_session() {
        let factory = Arc::new(CountingFactory::default());
        let manager = Arc::new(PooledMcpSessionManager::new(
            factory.clone(),
            Duration::from_secs(60),
        ));
        let server = Arc::new(test_server());

        let sessions = futures::future::join_all((0..16).map(|_| {
            let manager = manager.clone();
            let server = server.clone();
            async move { manager.get_or_connect(&server).await.unwrap() }
        }))
        .await;

        assert_eq!(factory.creations.load(Ordering::SeqCst), 1);
        assert!(
            sessions
                .iter()
                .all(|session| Arc::ptr_eq(session, &sessions[0]))
        );
    }

    #[tokio::test]
    async fn active_lease_prevents_ttl_eviction() {
        let factory = Arc::new(CountingFactory::default());
        let manager = PooledMcpSessionManager::new(factory.clone(), Duration::ZERO);
        let server = test_server();

        let first = manager.get_or_connect(&server).await.unwrap();
        let lease = first.acquire();
        let while_active = manager.get_or_connect(&server).await.unwrap();
        assert!(Arc::ptr_eq(&first, &while_active));
        assert_eq!(factory.creations.load(Ordering::SeqCst), 1);

        drop(lease);
        let replacement = manager.get_or_connect(&server).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &replacement));
        assert_eq!(factory.creations.load(Ordering::SeqCst), 2);
    }
}
