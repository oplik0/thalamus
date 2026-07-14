//! MCP API handlers

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::mcp::domain::{
    McpServerCreate, McpServerUpdate, McpToolContent, McpToolsetCreate, validate_alias,
};
use crate::features::mcp::dto::{
    AddToolsetItemRequest, CallMcpToolRequest, CallMcpToolResponse, CreateMcpServerRequest,
    CreateMcpToolsetRequest, ListMcpServersResponse, ListMcpToolsResponse, McpServerResponse,
    McpToolsetDetailResponse, McpToolsetItemResponse, McpToolsetResponse, McpToolsetToolsResponse,
    RemoveToolsetItemRequest, UpdateMcpServerRequest,
};
use crate::middleware::{ApiKeyAuth, auth::Auth, require_scope};
use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Query, Request, State},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorCode, ErrorData, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::{
    StreamableHttpServerConfig,
    streamable_http_server::{session::local::LocalSessionManager, tower::StreamableHttpService},
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

/// Create the MCP router.
pub fn router(state: AppState) -> Router<AppState> {
    let gateway_service = mcp_gateway_service(state.mcp_service.clone());
    let gateway_routes = Router::new()
        .nest_service("/mcp", gateway_service)
        .layer(middleware::from_fn_with_state(state, mcp_gateway_auth));

    Router::new()
        // Admin CRUD for MCP servers
        .route("/admin/mcp/servers", get(list_servers).post(create_server))
        .route(
            "/admin/mcp/servers/{id}",
            get(get_server).put(update_server).delete(delete_server),
        )
        .route("/admin/mcp/servers/{id}/tools", get(list_server_tools))
        // REST tool proxy
        .route("/mcp-rest/tools/list", get(rest_list_tools))
        .route("/mcp-rest/tools/call", post(rest_call_tool))
        // Toolset management
        .route("/v1/mcp/toolset", get(list_toolsets).post(create_toolset))
        .route(
            "/v1/mcp/toolset/{id}",
            get(get_toolset).delete(delete_toolset),
        )
        .route("/v1/mcp/toolset/{id}/items", post(add_toolset_item))
        .route(
            "/v1/mcp/toolset/{id}/items/remove",
            post(remove_toolset_item),
        )
        // Team auto toolset
        .route("/toolsets/teams/{team_name}/mcp", get(get_team_mcp_toolset))
        // MCP Gateway (spec-compliant Streamable HTTP)
        .merge(gateway_routes)
}

// ─────────────────────────────────────────────────────────────────────────────
// Admin server handlers
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ListServersQuery {
    team_id: Option<Uuid>,
}

async fn list_servers(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Query(query): Query<ListServersQuery>,
) -> Result<Json<ListMcpServersResponse>> {
    require_scope(&auth, "mcp:read")?;

    let (team_id, include_global) = resolve_team_scope(&auth, query.team_id)?;
    let servers = state
        .mcp_service
        .server_repo
        .list(team_id, include_global)
        .await?;

    Ok(Json(ListMcpServersResponse {
        servers: servers.into_iter().map(McpServerResponse::from).collect(),
    }))
}

async fn create_server(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<CreateMcpServerRequest>,
) -> Result<Json<McpServerResponse>> {
    require_scope(&auth, "mcp:admin")?;

    validate_alias(&req.alias)?;
    validate_transport(&req.transport, req.url.as_deref(), req.command.as_deref())?;

    // mcp:admin is required to reach this handler, so team_id from the
    // request is trusted. If omitted the server is global (team_id = NULL).
    let team_id = req.team_id;

    let create = McpServerCreate {
        alias: req.alias,
        name: req.name,
        transport: req.transport,
        url: req.url,
        command: req.command,
        args: req.args,
        env: req.env,
        auth: req.auth,
        static_headers: req.static_headers,
        extra_headers: req.extra_headers,
        timeout: req.timeout,
        description: req.description,
        allowed_scopes: req.allowed_scopes,
        team_id,
        created_by: Some(auth.user_id),
    };

    let server = state.mcp_service.server_repo.create(create).await?;

    // Sync the team auto toolset if this server is team-scoped.
    if let Some(team_id) = server.team_id {
        let _ = state
            .mcp_service
            .toolset_repo
            .sync_team_auto_toolset(team_id)
            .await;
    }

    Ok(Json(McpServerResponse::from(server)))
}

async fn get_server(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<McpServerResponse>> {
    require_scope(&auth, "mcp:read")?;

    let server = state
        .mcp_service
        .server_repo
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("MCP server not found: {id}")))?;

    ensure_server_access(&auth, &server)?;

    Ok(Json(McpServerResponse::from(server)))
}

async fn update_server(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateMcpServerRequest>,
) -> Result<Json<McpServerResponse>> {
    require_scope(&auth, "mcp:admin")?;

    let existing = state
        .mcp_service
        .server_repo
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("MCP server not found: {id}")))?;

    ensure_server_access(&auth, &existing)?;

    if let Some(alias) = &req.alias {
        validate_alias(alias)?;
    }

    let transport = req.transport.unwrap_or(existing.transport);
    let url = req.url.as_ref().or(existing.url.as_ref()).cloned();
    let command = req.command.as_ref().or(existing.command.as_ref()).cloned();
    validate_transport(&transport, url.as_deref(), command.as_deref())?;

    let update = McpServerUpdate {
        alias: req.alias,
        name: req.name.map(Some),
        transport: req.transport,
        url: req.url.map(Some),
        command: req.command.map(Some),
        args: req.args,
        env: req.env,
        auth: req.auth,
        static_headers: req.static_headers,
        extra_headers: req.extra_headers,
        timeout: req.timeout,
        description: req.description.map(Some),
        allowed_scopes: req.allowed_scopes,
        team_id: req.team_id.map(Some),
        is_enabled: req.is_enabled,
    };

    let server = state.mcp_service.server_repo.update(id, update).await?;
    // Any connection-affecting field may have changed (including URL,
    // transport, headers, or auth), so never reuse the old rmcp session.
    state.mcp_service.session_manager.evict(id);

    // Re-sync auto toolset for both old and new team.
    for team_id in [existing.team_id, server.team_id].into_iter().flatten() {
        let _ = state
            .mcp_service
            .toolset_repo
            .sync_team_auto_toolset(team_id)
            .await;
    }

    Ok(Json(McpServerResponse::from(server)))
}

async fn delete_server(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "mcp:admin")?;

    let server = state
        .mcp_service
        .server_repo
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("MCP server not found: {id}")))?;

    ensure_server_access(&auth, &server)?;

    state.mcp_service.server_repo.delete(id).await?;
    state.mcp_service.session_manager.evict(id);

    if let Some(team_id) = server.team_id {
        let _ = state
            .mcp_service
            .toolset_repo
            .sync_team_auto_toolset(team_id)
            .await;
    }

    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn list_server_tools(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<ListMcpToolsResponse>> {
    require_scope(&auth, "mcp:invoke")?;

    let server = state
        .mcp_service
        .server_repo
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("MCP server not found: {id}")))?;

    ensure_server_access(&auth, &server)?;
    ensure_invoke_scope(&auth, &server)?;

    let tools = state.mcp_service.list_tools_for_server(&server).await?;

    Ok(Json(ListMcpToolsResponse {
        server_id: server.alias,
        tools,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// REST tool proxy
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RestListToolsQuery {
    server_id: String,
}

async fn rest_list_tools(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Query(query): Query<RestListToolsQuery>,
) -> Result<Json<ListMcpToolsResponse>> {
    require_scope(&auth, "mcp:invoke")?;

    let server = state
        .mcp_service
        .server_repo
        .get_by_alias(&query.server_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("MCP server not found: {}", query.server_id)))?;

    ensure_server_access(&auth, &server)?;
    ensure_invoke_scope(&auth, &server)?;

    let tools = state.mcp_service.list_tools_for_server(&server).await?;

    Ok(Json(ListMcpToolsResponse {
        server_id: server.alias,
        tools,
    }))
}

async fn rest_call_tool(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<CallMcpToolRequest>,
) -> Result<Json<CallMcpToolResponse>> {
    require_scope(&auth, "mcp:invoke")?;

    let server = state
        .mcp_service
        .server_repo
        .get_by_alias(&req.server_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("MCP server not found: {}", req.server_id)))?;

    ensure_server_access(&auth, &server)?;
    ensure_invoke_scope(&auth, &server)?;

    let tool_name = if let Some(stripped) =
        crate::features::mcp::domain::strip_alias_prefix(&server.alias, &req.name, "-")
    {
        stripped.to_string()
    } else {
        req.name
    };

    let result = state
        .mcp_service
        .call_tool(&server, &tool_name, req.arguments)
        .await?;

    Ok(Json(CallMcpToolResponse {
        server_id: server.alias,
        name: tool_name,
        result,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Toolset handlers
// ─────────────────────────────────────────────────────────────────────────────

async fn list_toolsets(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
) -> Result<Json<Vec<McpToolsetResponse>>> {
    require_scope(&auth, "mcp:read")?;

    let toolsets = state
        .mcp_service
        .toolset_repo
        .list_for_team(auth.team_id)
        .await?;

    Ok(Json(
        toolsets.into_iter().map(McpToolsetResponse::from).collect(),
    ))
}

async fn create_toolset(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<CreateMcpToolsetRequest>,
) -> Result<Json<McpToolsetResponse>> {
    require_scope(&auth, "mcp:admin")?;

    validate_alias(&req.slug)?;

    let toolset = state
        .mcp_service
        .toolset_repo
        .create(McpToolsetCreate {
            team_id: auth.team_id,
            name: req.name,
            slug: req.slug,
            description: req.description,
            is_auto: false,
        })
        .await?;

    Ok(Json(McpToolsetResponse::from(toolset)))
}

async fn get_toolset(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<McpToolsetDetailResponse>> {
    require_scope(&auth, "mcp:read")?;

    let toolset = state
        .mcp_service
        .toolset_repo
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Toolset not found: {id}")))?;

    ensure_toolset_access(&auth, &toolset)?;

    let items = state.mcp_service.toolset_repo.list_items(id).await?;
    let items = resolve_toolset_items(&state, items).await?;

    Ok(Json(McpToolsetDetailResponse {
        toolset: McpToolsetResponse::from(toolset),
        items,
    }))
}

async fn delete_toolset(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "mcp:admin")?;

    let toolset = state
        .mcp_service
        .toolset_repo
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Toolset not found: {id}")))?;

    ensure_toolset_access(&auth, &toolset)?;

    if toolset.is_auto {
        return Err(Error::Authorization(
            "Cannot delete an automatic team toolset".to_string(),
        ));
    }

    state.mcp_service.toolset_repo.delete(id).await?;

    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn add_toolset_item(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
    Json(req): Json<AddToolsetItemRequest>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "mcp:admin")?;

    let toolset = state
        .mcp_service
        .toolset_repo
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Toolset not found: {id}")))?;

    ensure_toolset_access(&auth, &toolset)?;

    state
        .mcp_service
        .toolset_repo
        .add_item(id, req.server_id, req.tool_name)
        .await?;

    Ok(Json(serde_json::json!({ "added": true })))
}

async fn remove_toolset_item(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
    Json(req): Json<RemoveToolsetItemRequest>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "mcp:admin")?;

    let toolset = state
        .mcp_service
        .toolset_repo
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Toolset not found: {id}")))?;

    ensure_toolset_access(&auth, &toolset)?;

    state
        .mcp_service
        .toolset_repo
        .remove_item(id, req.server_id, req.tool_name)
        .await?;

    Ok(Json(serde_json::json!({ "removed": true })))
}

async fn get_team_mcp_toolset(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(team_name): Path<String>,
) -> Result<Json<McpToolsetToolsResponse>> {
    require_scope(&auth, "mcp:invoke")?;

    let team =
        sqlx::query_as::<_, TeamRow>("SELECT * FROM teams WHERE slug = $1 AND deleted_at IS NULL")
            .bind(&team_name)
            .fetch_optional(&state.db_pool)
            .await
            .map_err(Error::Database)?
            .ok_or_else(|| Error::NotFound(format!("Team not found: {team_name}")))?;

    ensure_team_access(&auth, team.id)?;

    // Ensure the auto toolset exists and is in sync.
    let toolset = state
        .mcp_service
        .toolset_repo
        .sync_team_auto_toolset(team.id)
        .await?;

    let tools = state
        .mcp_service
        .resolve_toolset_tools(team.id, "mcp")
        .await?;

    Ok(Json(McpToolsetToolsResponse {
        toolset: McpToolsetResponse::from(toolset),
        tools,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct TeamRow {
    id: Uuid,
}

fn resolve_team_scope(auth: &Auth, query_team_id: Option<Uuid>) -> Result<(Option<Uuid>, bool)> {
    if auth.has_scope("mcp:admin") {
        Ok((query_team_id, true))
    } else {
        Ok((Some(auth.team_id), true))
    }
}

fn validate_transport(
    transport: &crate::features::mcp::domain::McpTransport,
    url: Option<&str>,
    command: Option<&str>,
) -> Result<()> {
    use crate::features::mcp::domain::McpTransport;
    match transport {
        McpTransport::Http | McpTransport::Sse => {
            if url.is_none() || url.unwrap().is_empty() {
                return Err(Error::InvalidInput(
                    "HTTP/SSE transport requires a non-empty URL".to_string(),
                ));
            }
        }
        McpTransport::Stdio => {
            if command.is_none() || command.unwrap().is_empty() {
                return Err(Error::InvalidInput(
                    "stdio transport requires a non-empty command".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn ensure_server_access(
    auth: &Auth,
    server: &crate::features::mcp::domain::McpServer,
) -> Result<()> {
    if auth.has_scope("mcp:admin") {
        return Ok(());
    }
    if let Some(team_id) = server.team_id {
        ensure_team_access(auth, team_id)
    } else {
        // Global servers are accessible to any authenticated caller with MCP scopes.
        Ok(())
    }
}

fn ensure_toolset_access(
    auth: &Auth,
    toolset: &crate::features::mcp::domain::McpToolset,
) -> Result<()> {
    if auth.has_scope("mcp:admin") {
        return Ok(());
    }
    ensure_team_access(auth, toolset.team_id)
}

fn ensure_team_access(auth: &Auth, team_id: Uuid) -> Result<()> {
    if auth.team_id == team_id {
        Ok(())
    } else {
        Err(Error::Authorization(
            "You do not have access to this team's resources".to_string(),
        ))
    }
}

fn ensure_invoke_scope(
    auth: &Auth,
    server: &crate::features::mcp::domain::McpServer,
) -> Result<()> {
    if server.allowed_scopes.is_empty() {
        return Ok(());
    }
    if server
        .allowed_scopes
        .iter()
        .any(|scope| auth.has_scope(scope))
    {
        return Ok(());
    }
    Err(Error::Authorization(
        "You do not have permission to invoke this MCP server".to_string(),
    ))
}

async fn resolve_toolset_items(
    state: &AppState,
    items: Vec<crate::features::mcp::domain::McpToolsetItem>,
) -> Result<Vec<McpToolsetItemResponse>> {
    let mut responses = Vec::with_capacity(items.len());
    for item in items {
        if let Some(server) = state
            .mcp_service
            .server_repo
            .get_by_id(item.server_id)
            .await?
        {
            responses.push(McpToolsetItemResponse {
                server_id: item.server_id,
                server_alias: server.alias,
                tool_name: item.tool_name,
            });
        }
    }
    Ok(responses)
}

// ─────────────────────────────────────────────────────────────────────────────
// MCP Gateway (Streamable HTTP server)
// ─────────────────────────────────────────────────────────────────────────────

/// The MCP-facing gateway. A new handler is made for every rmcp session; its
/// product state is shared through `McpService`, while auth stays attached to
/// the individual HTTP request context.
#[derive(Clone)]
struct ThalamusMcpGateway {
    mcp_service: Arc<crate::features::mcp::infra::McpService>,
}

impl ThalamusMcpGateway {
    fn new(mcp_service: Arc<crate::features::mcp::infra::McpService>) -> Self {
        Self { mcp_service }
    }
}

impl ServerHandler for ThalamusMcpGateway {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("thalamus-mcp-gateway", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        let auth = mcp_auth_from_context(&context)?;
        require_scope(&auth, "mcp:invoke").map_err(|err| mcp_error_from_app(&err))?;

        let servers = self
            .mcp_service
            .server_repo
            .list(Some(auth.team_id), true)
            .await
            .map_err(|err| mcp_error_from_app(&err))?;

        let mut all_tools = Vec::new();
        for server in &servers {
            if !server.is_enabled
                || (!server.allowed_scopes.is_empty()
                    && !server
                        .allowed_scopes
                        .iter()
                        .any(|scope| auth.has_scope(scope)))
            {
                continue;
            }

            match self.mcp_service.list_tools_for_server(server).await {
                Ok(tools) => {
                    for tool in tools {
                        let Value::Object(input_schema) = tool.input_schema else {
                            tracing::warn!(
                                server_id = %server.id,
                                server_alias = %server.alias,
                                tool_name = %tool.name,
                                "Skipping MCP tool with a non-object input schema"
                            );
                            continue;
                        };

                        let mut gateway_tool = Tool::default();
                        gateway_tool.name = crate::features::mcp::domain::prefixed_tool_name(
                            &server.alias,
                            &tool.name,
                            "-",
                        )
                        .into();
                        gateway_tool.description = tool.description.map(Into::into);
                        gateway_tool.input_schema = Arc::new(input_schema);
                        all_tools.push(gateway_tool);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        server_id = %server.id,
                        server_alias = %server.alias,
                        error = %err,
                        "Failed to list tools for MCP server in gateway"
                    );
                }
            }
        }

        Ok(ListToolsResult::with_all_items(all_tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let auth = mcp_auth_from_context(&context)?;
        require_scope(&auth, "mcp:invoke").map_err(|err| mcp_error_from_app(&err))?;

        let tool_name = request.name.as_ref();
        let servers = self
            .mcp_service
            .server_repo
            .list(Some(auth.team_id), true)
            .await
            .map_err(|err| mcp_error_from_app(&err))?;

        // Use the longest matching alias: `api-v2-tool` belongs to `api-v2`,
        // not `api`, if both aliases exist.
        let mut best_match: Option<(&crate::features::mcp::domain::McpServer, &str)> = None;
        for server in &servers {
            if !server.is_enabled {
                continue;
            }
            if let Some(stripped) =
                crate::features::mcp::domain::strip_alias_prefix(&server.alias, tool_name, "-")
            {
                let is_longer = best_match
                    .is_none_or(|(previous, _)| server.alias.len() > previous.alias.len());
                if is_longer {
                    best_match = Some((server, stripped));
                }
            }
        }

        let Some((server, stripped)) = best_match else {
            return Err(ErrorData::resource_not_found(
                format!("No MCP server found for tool: {tool_name}"),
                None,
            ));
        };
        ensure_invoke_scope(&auth, server).map_err(|err| mcp_error_from_app(&err))?;

        let result = match self
            .mcp_service
            .call_tool(
                server,
                stripped,
                Value::Object(request.arguments.unwrap_or_default()),
            )
            .await
        {
            Ok(result) => result,
            // A routed tool that cannot execute should be visible to the MCP
            // caller as a tool error, not hidden behind a JSON-RPC failure.
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    err.to_string(),
                )]));
            }
        };

        let output_content = result
            .content
            .into_iter()
            .map(domain_content_to_rmcp)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut response = if result.is_error {
            CallToolResult::error(output_content)
        } else {
            CallToolResult::success(output_content)
        };
        response.structured_content = result.structured_content;
        Ok(response)
    }
}

fn mcp_gateway_service(
    mcp_service: Arc<crate::features::mcp::infra::McpService>,
) -> StreamableHttpService<ThalamusMcpGateway, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(ThalamusMcpGateway::new(mcp_service.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    )
}

/// Authenticate every streamable-HTTP request and preserve the resulting
/// identity in request extensions. rmcp copies the HTTP request parts into its
/// `RequestContext`, so handler methods see the precise identity that made the
/// RPC request rather than mutable session-global auth state.
async fn mcp_gateway_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response> {
    let (mut parts, body) = request.into_parts();
    let ApiKeyAuth(auth) = ApiKeyAuth::from_request_parts(&mut parts, &state).await?;
    parts.extensions.insert(auth);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

fn mcp_auth_from_context(
    context: &RequestContext<RoleServer>,
) -> std::result::Result<Auth, ErrorData> {
    let parts = context
        .extensions
        .get::<axum::http::request::Parts>()
        .ok_or_else(|| ErrorData::internal_error("MCP request has no HTTP context", None))?;
    parts
        .extensions
        .get::<Auth>()
        .cloned()
        .ok_or_else(|| ErrorData::new(ErrorCode(-32001), "MCP request is unauthenticated", None))
}

fn domain_content_to_rmcp(content: McpToolContent) -> std::result::Result<ContentBlock, ErrorData> {
    let McpToolContent {
        content_type,
        text,
        mut extra,
    } = content;
    extra.insert("type".to_string(), Value::String(content_type));
    if let Some(text) = text {
        extra.insert("text".to_string(), Value::String(text));
    }

    serde_json::from_value(Value::Object(extra)).map_err(|err| {
        ErrorData::internal_error(
            format!("Invalid MCP tool content returned by upstream: {err}"),
            None,
        )
    })
}

fn mcp_error_from_app(err: &Error) -> ErrorData {
    let message = err.to_string();
    match err {
        Error::NotFound(_) => ErrorData::resource_not_found(message, None),
        Error::InvalidInput(_) => ErrorData::invalid_params(message, None),
        Error::Authentication(_) | Error::Authorization(_) => {
            ErrorData::new(ErrorCode(-32001), message, None)
        }
        Error::Backend(_) | Error::ServiceUnavailable(_) => {
            ErrorData::new(ErrorCode(-32000), message, None)
        }
        Error::Database(_) | Error::Config(_) | Error::Guardrail(_) | Error::Internal(_) => {
            ErrorData::internal_error(message, None)
        }
    }
}
