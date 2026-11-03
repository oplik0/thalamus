//! MCP gateway integration tests

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::ServiceExt as RmcpServiceExt;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

mod common;
use common::fixtures::{TestApiKeyBuilder, TestUserBuilder};
use common::transactional::init_test_state;

struct McpMockResponder;

impl Respond for McpMockResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
        let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");

        match method {
            "initialize" => ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": body.get("id").unwrap_or(&json!(1)),
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {
                        "name": "test-server",
                        "version": "1.0.0"
                    },
                    "capabilities": {}
                }
            })),
            "notifications/initialized" => ResponseTemplate::new(202),
            "tools/list" => ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": body.get("id").unwrap_or(&json!(2)),
                "result": {
                    "tools": [
                        {
                            "name": "getPlaces",
                            "description": "Find places",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "query": { "type": "string" }
                                },
                                "required": ["query"]
                            }
                        },
                        {
                            "name": "deletePlace",
                            "description": "Delete a place",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string" }
                                },
                                "required": ["id"]
                            }
                        }
                    ]
                }
            })),
            "tools/call" => ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": body.get("id").unwrap_or(&json!(3)),
                "result": {
                    "content": [
                        { "type": "text", "text": "Found 3 places" }
                    ],
                    "isError": false
                }
            })),
            _ => ResponseTemplate::new(404).set_body_json(json!({
                "error": "Unknown method"
            })),
        }
    }
}

async fn start_mock_mcp_server() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(McpMockResponder)
        .mount(&server)
        .await;

    server
}

fn build_request(
    method: &str,
    uri: &str,
    api_key: &str,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json");

    if let Some(body) = body {
        builder = builder.header("Content-Length", body.to_string().len().to_string());
        builder.body(Body::from(body.to_string())).unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    }
}

#[sqlx::test]
async fn test_mcp_server_crud(pool: PgPool) {
    let state = init_test_state(pool.clone()).await;
    let app = thalamus::bootstrap::build_router(state);

    let user = TestUserBuilder::new()
        .with_scope("mcp:admin")
        .with_scope("mcp:read")
        .create(&pool)
        .await;

    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("mcp:admin")
        .with_scope("mcp:read")
        .create(&pool)
        .await;

    // Create server
    let create_req = build_request(
        "POST",
        "/admin/mcp/servers",
        &api_key.key,
        Some(json!({
            "alias": "places_api",
            "name": "Places API",
            "transport": "http",
            "url": "http://localhost:9999/mcp",
            "description": "Test places API",
            "allowed_scopes": ["mcp:invoke"]
        })),
    );

    let response = app.clone().oneshot(create_req).await.unwrap();
    if response.status() != StatusCode::OK {
        let body = common::http::extract_text(response).await;
        panic!("Create server failed (CRUD test): {}", body);
    }

    let body = common::http::extract_json(response).await;
    let server_id = body["id"].as_str().unwrap();

    // List servers
    let list_req = build_request("GET", "/admin/mcp/servers", &api_key.key, None);
    let response = app.clone().oneshot(list_req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = common::http::extract_json(response).await;
    assert_eq!(body["servers"].as_array().unwrap().len(), 1);

    // Get server
    let get_req = build_request(
        "GET",
        &format!("/admin/mcp/servers/{server_id}"),
        &api_key.key,
        None,
    );
    let response = app.clone().oneshot(get_req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = common::http::extract_json(response).await;
    assert_eq!(body["alias"], "places_api");
}

#[sqlx::test]
async fn test_runtime_stdio_registration_is_disabled(pool: PgPool) {
    let state = init_test_state(pool.clone()).await;
    let app = thalamus::bootstrap::build_router(state);
    let user = TestUserBuilder::new()
        .with_scope("mcp:admin")
        .create(&pool)
        .await;
    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("mcp:admin")
        .create(&pool)
        .await;

    let request = build_request(
        "POST",
        "/admin/mcp/servers",
        &api_key.key,
        Some(json!({
            "alias": "unsafe_stdio",
            "transport": "stdio",
            "command": "sh",
            "args": ["-c", "id"],
            "env": {"SECRET": "should-not-run"}
        })),
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = common::http::extract_json(response).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("stdio MCP registration is disabled")
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_servers WHERE alias = $1")
        .bind("unsafe_stdio")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn test_mcp_rest_tool_proxy(pool: PgPool) {
    let mock = start_mock_mcp_server().await;
    let state = init_test_state(pool.clone()).await;
    let app = thalamus::bootstrap::build_router(state);

    let user = TestUserBuilder::new()
        .with_scope("mcp:admin")
        .with_scope("mcp:invoke")
        .create(&pool)
        .await;

    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("mcp:admin")
        .with_scope("mcp:invoke")
        .create(&pool)
        .await;

    // Create server pointing to mock
    let create_req = build_request(
        "POST",
        "/admin/mcp/servers",
        &api_key.key,
        Some(json!({
            "alias": "places_api",
            "transport": "http",
            "url": format!("{}/mcp", mock.uri()),
            "allowed_scopes": ["mcp:invoke"]
        })),
    );

    let response = app.clone().oneshot(create_req).await.unwrap();
    if response.status() != StatusCode::OK {
        let body = common::http::extract_text(response).await;
        panic!("Create server failed: {}", body);
    }

    // List tools via REST
    let list_req = build_request(
        "GET",
        "/mcp-rest/tools/list?server_id=places_api",
        &api_key.key,
        None,
    );
    let response = app.clone().oneshot(list_req).await.unwrap();
    if response.status() != StatusCode::OK {
        let body = common::http::extract_text(response).await;
        panic!("List tools failed: {}", body);
    }

    let body = common::http::extract_json(response).await;
    assert_eq!(body["server_id"], "places_api");

    // Call tool via REST
    let call_req = build_request(
        "POST",
        "/mcp-rest/tools/call",
        &api_key.key,
        Some(json!({
            "server_id": "places_api",
            "name": "getPlaces",
            "arguments": { "query": "coffee" }
        })),
    );
    let response = app.oneshot(call_req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = common::http::extract_json(response).await;
    assert_eq!(body["name"], "getPlaces");
    assert_eq!(body["result"]["content"][0]["text"], "Found 3 places");
}

#[sqlx::test]
async fn test_team_auto_toolset(pool: PgPool) {
    let mock = start_mock_mcp_server().await;
    let state = init_test_state(pool.clone()).await;
    let app = thalamus::bootstrap::build_router(state);

    let user = TestUserBuilder::new()
        .with_scope("mcp:admin")
        .with_scope("mcp:invoke")
        .create(&pool)
        .await;

    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("mcp:admin")
        .with_scope("mcp:invoke")
        .create(&pool)
        .await;

    // The TestApiKeyBuilder auto-creates a team for the user. Set a slug if missing.
    let team_id = api_key.team_id.unwrap();
    sqlx::query(
        "UPDATE teams SET slug = COALESCE(slug, REPLACE(LOWER(name), ' ', '-')) WHERE id = $1",
    )
    .bind(team_id)
    .execute(&pool)
    .await
    .expect("Failed to set team slug");

    let team_slug: String = sqlx::query_scalar("SELECT slug FROM teams WHERE id = $1")
        .bind(team_id)
        .fetch_one(&pool)
        .await
        .expect("Failed to get team slug");

    // Create server
    let create_req = build_request(
        "POST",
        "/admin/mcp/servers",
        &api_key.key,
        Some(json!({
            "alias": "places_api",
            "transport": "http",
            "url": format!("{}/mcp", mock.uri()),
            "allowed_scopes": ["mcp:invoke"]
        })),
    );
    let response = app.clone().oneshot(create_req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Get team auto toolset
    let toolset_req = build_request(
        "GET",
        &format!("/toolsets/teams/{team_slug}/mcp"),
        &api_key.key,
        None,
    );
    let response = app.oneshot(toolset_req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = common::http::extract_json(response).await;
    assert_eq!(body["toolset"]["slug"], "mcp");
    assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    assert_eq!(body["tools"][0]["name"], "places_api-getPlaces");
    assert_eq!(body["tools"][0]["server_alias"], "places_api");
}

#[sqlx::test]
async fn test_streamable_http_gateway_with_rmcp_client(pool: PgPool) {
    let upstream = start_mock_mcp_server().await;
    let state = init_test_state(pool.clone()).await;
    let app = thalamus::bootstrap::build_router(state);

    let user = TestUserBuilder::new()
        .with_scope("mcp:admin")
        .with_scope("mcp:invoke")
        .create(&pool)
        .await;
    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("mcp:admin")
        .with_scope("mcp:invoke")
        .create(&pool)
        .await;

    let create_req = build_request(
        "POST",
        "/admin/mcp/servers",
        &api_key.key,
        Some(json!({
            "alias": "places_api",
            "transport": "http",
            "url": format!("{}/mcp", upstream.uri()),
            "allowed_scopes": ["mcp:invoke"]
        })),
    );
    let response = app.clone().oneshot(create_req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
            .await
            .unwrap();
    });

    let mut config = StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp"))
        .auth_header(api_key.key.clone());
    // The test is only valid if the server returns and honors MCP-Session-Id.
    config.allow_stateless = false;
    let transport = StreamableHttpClientTransport::from_config(config);
    let client = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("thalamus-mcp-test-client", "1.0.0"),
    )
    .serve(transport)
    .await
    .unwrap();

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "places_api-getPlaces");

    let result = client
        .call_tool(
            CallToolRequestParams::new("places_api-getPlaces")
                .with_arguments(serde_json::from_value(json!({ "query": "coffee" })).unwrap()),
        )
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["content"][0]["text"], "Found 3 places");

    client.cancel().await.unwrap();
    shutdown.cancel();
    server.await.unwrap();
}

#[sqlx::test]
async fn test_named_toolset_is_an_mcp_endpoint(pool: PgPool) {
    let upstream = start_mock_mcp_server().await;
    let state = init_test_state(pool.clone()).await;
    let app = thalamus::bootstrap::build_router(state);
    let user = TestUserBuilder::new()
        .with_scope("mcp:admin")
        .with_scope("mcp:invoke")
        .create(&pool)
        .await;
    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("mcp:admin")
        .with_scope("mcp:invoke")
        .create(&pool)
        .await;

    let response = app
        .clone()
        .oneshot(build_request(
            "POST",
            "/admin/mcp/servers",
            &api_key.key,
            Some(json!({
                "alias": "places_api",
                "transport": "http",
                "url": format!("{}/mcp", upstream.uri()),
                "allowed_scopes": ["mcp:invoke"]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let server_id = common::http::extract_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let response = app
        .clone()
        .oneshot(build_request(
            "POST",
            "/v1/mcp/toolset",
            &api_key.key,
            Some(json!({"name": "Read only", "slug": "read-only"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let toolset_id = common::http::extract_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let response = app
        .clone()
        .oneshot(build_request(
            "POST",
            &format!("/v1/mcp/toolset/{toolset_id}/items"),
            &api_key.key,
            Some(json!({"server_id": server_id, "tool_name": "getPlaces"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
            .await
            .unwrap();
    });

    let mut config = StreamableHttpClientTransportConfig::with_uri(format!(
        "http://{address}/mcp/toolsets/read-only"
    ))
    .auth_header(api_key.key.clone());
    config.allow_stateless = false;
    let client = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("thalamus-toolset-test-client", "1.0.0"),
    )
    .serve(StreamableHttpClientTransport::from_config(config))
    .await
    .unwrap();

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "places_api-getPlaces");
    assert!(
        client
            .call_tool(
                CallToolRequestParams::new("places_api-deletePlace")
                    .with_arguments(serde_json::from_value(json!({"id": "123"})).unwrap()),
            )
            .await
            .is_err()
    );

    client.cancel().await.unwrap();
    shutdown.cancel();
    server.await.unwrap();
}
