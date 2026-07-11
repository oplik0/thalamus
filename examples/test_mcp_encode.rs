use sqlx::PgPool;
use thalamus::features::mcp::domain::{
    McpAuthConfig, McpServerCreate, McpServerRepository, McpTransport,
};
use thalamus::features::mcp::infra::SqlxMcpServerRepository;

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://opliko@localhost:5432/thalamus".to_string());
    let pool = PgPool::connect(&url).await.unwrap();
    let repo = SqlxMcpServerRepository::new(pool.clone());

    // Create a server with HTTP transport
    let server = repo
        .create(McpServerCreate {
            alias: "_encode_test".to_string(),
            name: None,
            transport: McpTransport::Http,
            url: Some("http://localhost:9999/mcp".to_string()),
            command: None,
            args: vec![],
            env: std::collections::HashMap::new(),
            auth: McpAuthConfig::None,
            static_headers: std::collections::HashMap::new(),
            extra_headers: vec![],
            timeout: Some("30s".to_string()),
            description: None,
            allowed_scopes: vec![],
            team_id: None,
            created_by: None,
        })
        .await
        .expect("Failed to create server");

    println!("Created server with transport: {:?}", server.transport);

    // Check what's stored in the DB directly
    let row: (String,) = sqlx::query_as("SELECT transport::text FROM mcp_servers WHERE alias = $1")
        .bind("_encode_test")
        .fetch_one(&pool)
        .await
        .expect("Failed to query transport");
    println!("Stored transport value in DB: {:?}", row.0);

    // Try to read it back via the repository
    let fetched = repo
        .get_by_alias("_encode_test")
        .await
        .expect("Failed to get server");
    match fetched {
        Some(s) => println!("Read back transport via repo: {:?}", s.transport),
        None => println!("FAILED to read back - decode error!"),
    }

    // Also insert a lowercase 'http' row directly and try to read it back
    sqlx::query("INSERT INTO mcp_servers (alias, transport, url) VALUES ('_encode_test_lower', 'http', 'http://localhost:9999/mcp')")
        .execute(&pool)
        .await
        .expect("Failed to insert lowercase row");

    let fetched_lower = repo.get_by_alias("_encode_test_lower").await;
    match &fetched_lower {
        Ok(Some(s)) => println!("Read back lowercase transport via repo: {:?}", s.transport),
        Ok(None) => println!("Row not found (unexpected)"),
        Err(e) => println!("FAILED to read lowercase row: {}", e),
    }

    // Cleanup
    repo.delete(server.id).await.ok();
    sqlx::query("DELETE FROM mcp_servers WHERE alias IN ('_encode_test', '_encode_test_lower')")
        .execute(&pool)
        .await
        .ok();
}
