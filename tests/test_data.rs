//! Test Data Seeding Module
//!
//! This module provides functions to seed sample data into the development database
//! for manual testing and debugging purposes.
//!
//! Usage:
//!   Run with: `cargo test --test test_data -- --ignored --nocapture`
//!   Or use the helper script: `test-seed`

use chrono::{Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Initialize database connection for seeding
async fn init_seed_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_string());
        format!("postgres://{user}@localhost:5432/thalamus")
    });

    PgPool::connect(&database_url).await.expect(
        "Failed to connect to development database. Is PostgreSQL running? Try: services-up",
    )
}

/// Run migrations before seeding
async fn ensure_migrations(pool: &PgPool) {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .expect("Failed to run migrations");
}

/// Create a sample user (upsert pattern - safe to run multiple times)
pub async fn seed_sample_user(pool: &PgPool, username: &str, email: &str) -> anyhow::Result<Uuid> {
    let user_id = Uuid::new_v4();

    // Use ON CONFLICT to make this idempotent
    let result = sqlx::query_scalar!(
        r#"
        INSERT INTO users (id, username, email)
        VALUES ($1, $2, $3)
        ON CONFLICT (username) DO UPDATE SET
            email = EXCLUDED.email,
            updated_at = NOW()
        RETURNING id
        "#,
        user_id,
        username,
        email
    )
    .fetch_one(pool)
    .await?;

    Ok(result)
}

/// Create a sample team (upsert pattern)
pub async fn seed_sample_team(pool: &PgPool, name: &str) -> anyhow::Result<Uuid> {
    let team_id = Uuid::new_v4();

    let result = sqlx::query_scalar!(
        r#"
        INSERT INTO teams (id, name)
        VALUES ($1, $2)
        ON CONFLICT (name) DO UPDATE SET
            updated_at = NOW()
        RETURNING id
        "#,
        team_id,
        name
    )
    .fetch_one(pool)
    .await?;

    Ok(result)
}

/// Create a team membership (idempotent)
pub async fn seed_team_membership(
    pool: &PgPool,
    user_id: Uuid,
    team_id: Uuid,
    role: &str,
) -> anyhow::Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO team_memberships (user_id, team_id, role)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id, team_id) DO UPDATE SET
            role = EXCLUDED.role
        "#,
        user_id,
        team_id,
        role
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Generate a new random API key
fn generate_random_key() -> String {
    use base64::{Engine as _, engine::general_purpose};
    use rand_08::RngCore;
    use rand_08::rngs::OsRng;

    let mut secret_bytes = vec![0u8; 32];
    let mut public_bytes = vec![0u8; 16];
    let mut rng = OsRng;
    rng.fill_bytes(&mut secret_bytes);
    rng.fill_bytes(&mut public_bytes);

    let secret_part = general_purpose::URL_SAFE_NO_PAD.encode(&secret_bytes);
    let public_part = general_purpose::URL_SAFE_NO_PAD.encode(&public_bytes);

    format!("thl_{}_{}", public_part, secret_part)
}

/// Hash an API key secret using Argon2
async fn hash_key_secret(_pool: &PgPool, key: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{SaltString, rand_core::OsRng};
    use argon2::{Argon2, Params, PasswordHasher};

    // Parse the key format: prefix_id_secret
    let last_underscore = key
        .rfind('_')
        .ok_or_else(|| anyhow::anyhow!("Invalid key format"))?;
    let (_, secret) = key.split_at(last_underscore);
    let secret = &secret[1..]; // Remove the underscore

    let salt = SaltString::generate(&mut OsRng);

    // Get api_key_secret from config (use default for seeding)
    let secret_bytes = b"test_secret_key_must_be_at_least_32_bytes_long";
    let argon2 = Argon2::new_with_secret(
        secret_bytes,
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        Params::new(1024, 2, 1, Some(64)).unwrap(),
    )
    .map_err(|e| anyhow::anyhow!("Failed to create Argon2 instance: {}", e))?;

    let key_hash = argon2
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("Failed to hash key: {}", e))?
        .to_string();

    Ok(key_hash)
}

/// Create a sample API key for testing
pub async fn seed_sample_api_key(
    pool: &PgPool,
    user_id: Uuid,
    team_id: Uuid,
    name: &str,
    scopes: Vec<String>,
) -> anyhow::Result<String> {
    let full_key = generate_random_key();
    let key_id = Uuid::new_v4();
    let key_prefix = &full_key[..12.min(full_key.len())];
    let expires_at = Utc::now() + Duration::days(90);

    // Hash the key secret for storage
    let key_hash = hash_key_secret(pool, &full_key).await?;

    // Extract the public key_id from the key (format: thl_public_secret)
    let public_id = if let Some(pos) = full_key.find('_') {
        let after_prefix = &full_key[pos + 1..];
        if let Some(pos2) = after_prefix.find('_') {
            &after_prefix[..pos2]
        } else {
            after_prefix
        }
    } else {
        return Err(anyhow::anyhow!("Invalid key format: no underscore found"));
    };

    // Insert the API key
    sqlx::query!(
        r#"
        INSERT INTO api_keys (
            id, key_id, key_hash, key_prefix,
            user_id, team_id, name, description,
            scopes, is_active, expires_at, created_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, true, $10, NOW())
        ON CONFLICT (id) DO UPDATE SET
            name = EXCLUDED.name,
            scopes = EXCLUDED.scopes,
            expires_at = EXCLUDED.expires_at
        "#,
        key_id,
        public_id,
        key_hash,
        key_prefix,
        user_id,
        team_id,
        name,
        None::<String>, // description
        &scopes,
        expires_at
    )
    .execute(pool)
    .await?;

    println!("Created API key: {} (key_id: {})", full_key, key_id);
    println!("  Scopes: {:?}", scopes);
    println!("  Expires: {}", expires_at);

    Ok(full_key)
}

/// Create a sample backend configuration (in-memory only, no DB table)
pub async fn seed_sample_backend(
    _pool: &PgPool,
    name: &str,
    base_url: &str,
    provider: &str,
) -> anyhow::Result<Uuid> {
    let backend_id = Uuid::new_v4();

    println!("Created backend: {} (id: {})", name, backend_id);
    println!("  URL: {}", base_url);
    println!("  Provider: {}", provider);

    Ok(backend_id)
}

/// Seed complete development dataset
/// This creates a realistic set of users, teams, API keys, and backends for testing
#[ignore = "Seed test data - run manually with: cargo test --test test_data -- --ignored --nocapture"]
#[tokio::test]
async fn seed_development_data() {
    println!("==========================================");
    println!("Seeding Development Database with Test Data");
    println!("==========================================\n");

    let pool = init_seed_pool().await;
    ensure_migrations(&pool).await;

    // Create sample users
    println!("\n--- Creating Sample Users ---");
    let admin_user = seed_sample_user(&pool, "admin", "admin@thalamus.local")
        .await
        .expect("Failed to seed admin user");
    println!("Admin user: {} (admin)", admin_user);

    let dev_user = seed_sample_user(&pool, "developer", "dev@thalamus.local")
        .await
        .expect("Failed to seed developer user");
    println!("Developer user: {} (developer)", dev_user);

    let test_user = seed_sample_user(&pool, "tester", "test@thalamus.local")
        .await
        .expect("Failed to seed tester user");
    println!("Tester user: {} (tester)", test_user);

    // Create sample teams
    println!("\n--- Creating Sample Teams ---");
    let default_team = seed_sample_team(&pool, "default")
        .await
        .expect("Failed to seed default team");
    println!("Default team: {}", default_team);

    let dev_team = seed_sample_team(&pool, "engineering")
        .await
        .expect("Failed to seed engineering team");
    println!("Engineering team: {}", dev_team);

    let qa_team = seed_sample_team(&pool, "qa")
        .await
        .expect("Failed to seed qa team");
    println!("QA team: {}", qa_team);

    // Create team memberships
    println!("\n--- Creating Team Memberships ---");
    seed_team_membership(&pool, admin_user, default_team, "admin")
        .await
        .expect("Failed to create admin membership");
    println!("admin -> default (admin)");

    seed_team_membership(&pool, dev_user, dev_team, "admin")
        .await
        .expect("Failed to create dev admin membership");
    println!("developer -> engineering (admin)");

    seed_team_membership(&pool, dev_user, default_team, "member")
        .await
        .expect("Failed to create dev member membership");
    println!("developer -> default (member)");

    seed_team_membership(&pool, test_user, qa_team, "admin")
        .await
        .expect("Failed to create qa admin membership");
    println!("tester -> qa (admin)");

    seed_team_membership(&pool, test_user, default_team, "member")
        .await
        .expect("Failed to create tester member membership");
    println!("tester -> default (member)");

    // Create sample API keys
    println!("\n--- Creating Sample API Keys ---");

    // Admin key with full permissions
    let admin_key = seed_sample_api_key(
        &pool,
        admin_user,
        default_team,
        "Admin Master Key",
        vec![
            "api_keys:create".to_string(),
            "api_keys:read".to_string(),
            "api_keys:revoke".to_string(),
            "chat:read".to_string(),
            "chat:write".to_string(),
            "backends:manage".to_string(),
        ],
    )
    .await
    .expect("Failed to create admin API key");

    // Developer key with chat permissions
    let dev_key = seed_sample_api_key(
        &pool,
        dev_user,
        dev_team,
        "Developer Key",
        vec!["chat:read".to_string(), "chat:write".to_string()],
    )
    .await
    .expect("Failed to create developer API key");

    // Read-only key for testing
    let readonly_key = seed_sample_api_key(
        &pool,
        test_user,
        qa_team,
        "QA Read-Only Key",
        vec!["chat:read".to_string(), "api_keys:read".to_string()],
    )
    .await
    .expect("Failed to create read-only API key");

    // Create sample backends
    println!("\n--- Creating Sample Backends ---");

    // Ollama backend (local)
    let _ollama = seed_sample_backend(&pool, "ollama-local", "http://localhost:11434", "ollama")
        .await
        .expect("Failed to create ollama backend");

    // Example vLLM backend
    let _vllm = seed_sample_backend(
        &pool,
        "vllm-production",
        "http://vllm.internal:8000",
        "vllm",
    )
    .await
    .expect("Failed to create vllm backend");

    // Example llama.cpp backend
    let _llamacpp = seed_sample_backend(&pool, "llamacpp-dev", "http://localhost:8080", "llamacpp")
        .await
        .expect("Failed to create llamacpp backend");

    // Summary
    println!("\n==========================================");
    println!("Development Data Seeded Successfully!");
    println!("==========================================\n");

    println!("Sample API Keys (save these for testing):");
    println!("  Admin Key (full permissions):");
    println!("    {}", admin_key);
    println!();
    println!("  Developer Key (chat only):");
    println!("    {}", dev_key);
    println!();
    println!("  Read-Only Key (qa team):");
    println!("    {}", readonly_key);
    println!();

    println!("Sample Backends:");
    println!("  - ollama-local (http://localhost:11434)");
    println!("  - vllm-production (http://vllm.internal:8000)");
    println!("  - llamacpp-dev (http://localhost:8080)");
    println!();

    println!("Test Endpoints:");
    println!("  GET  /v1/auth/whoami      - Verify authentication");
    println!("  GET  /v1/api-keys         - List your API keys");
    println!("  POST /v1/api-keys         - Create new API key");
    println!("  GET  /v1/backends         - List configured backends");
    println!("  POST /v1/chat/completions - Send chat request");
    println!();

    println!("You can now start the server with: run");
    println!(
        "And test with: curl -H 'Authorization: Bearer <key>' http://localhost:3000/v1/auth/whoami"
    );
}

/// Seed minimal test data for quick testing
#[ignore = "Seed minimal data - run manually with: cargo test test_seed_minimal -- --ignored --nocapture"]
#[tokio::test]
async fn test_seed_minimal() {
    println!("Seeding minimal test data...");

    let pool = init_seed_pool().await;
    ensure_migrations(&pool).await;

    // Just create one user and one API key
    let user = seed_sample_user(&pool, "testuser", "test@example.com")
        .await
        .expect("Failed to seed user");

    let team = seed_sample_team(&pool, "default")
        .await
        .expect("Failed to seed team");

    seed_team_membership(&pool, user, team, "admin")
        .await
        .expect("Failed to create membership");

    let key = seed_sample_api_key(
        &pool,
        user,
        team,
        "Test Key",
        vec!["chat:read".to_string(), "chat:write".to_string()],
    )
    .await
    .expect("Failed to create API key");

    println!("\nMinimal data seeded.");
    println!("Test API Key: {}", key);
}
