use std::sync::Arc;
use thalamus_server::{AppState, Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--openapi")) {
        println!(
            "{}",
            serde_json::to_string_pretty(&thalamus_server::openapi_document())?
        );
        return Ok(());
    }
    let config =
        Config::from_env().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await?;
    thalamus_server::migrate(&db).await?;
    let identity = thalamus_identity::IdentityService::discover(
        db.clone(),
        thalamus_identity::Config {
            issuer: config.oidc_issuer.clone(),
            client_id: config.oidc_client_id.clone(),
            client_secret: Some(config.oidc_client_secret.clone()),
            redirect_uri: format!("{}/auth/callback", config.public_origin),
            session_signing_key: config.cookie_signing_key.clone(),
            session_lifetime: chrono::Duration::hours(12),
            groups: config.groups.clone(),
        },
    )
    .await?;
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    let policy_source = config.policy_source.clone();
    let state = Arc::new(AppState {
        config,
        db,
        identity,
        capability_evidence: None,
        policy_source,
    });
    axum::serve(listener, thalamus_server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
