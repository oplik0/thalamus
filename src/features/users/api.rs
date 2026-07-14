//! User management API handlers.

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::domain::opaque::{RegistrationRequest, RegistrationResponse};
use crate::features::auth::infra::opaque_service::finish_registration_upload;
use crate::features::auth::infra::registration_start;
use crate::middleware::{ApiKeyAuth, require_scope};

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserResponse {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub is_service_account: bool,
    pub is_active: bool,
    pub has_password: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct AdminRegistrationStartRequest {
    pub username: String,
    pub email: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct AdminRegistrationFinishRequest {
    pub username: String,
    pub email: String,
    pub message: String,
    pub team_id: Option<Uuid>,
    pub role: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PasswordRegistrationStartRequest {
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct PasswordRegistrationFinishRequest {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct CreateUserResponse {
    pub user: UserResponse,
    pub team_id: Uuid,
    pub role: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/users", get(list_users))
        .route("/v1/users/{id}", get(get_user))
        .route("/v1/users/register/start", post(admin_register_start))
        .route("/v1/users/register/finish", post(admin_register_finish))
        .route("/v1/users/me/password/start", post(change_password_start))
        .route("/v1/users/me/password/finish", post(change_password_finish))
}

async fn list_users(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
) -> Result<Json<Vec<UserResponse>>> {
    require_scope(&auth, "admin")?;

    let users = sqlx::query_as::<_, UserResponse>(
        r#"
        SELECT id, username, email, is_service_account, is_active,
               opaque_registration IS NOT NULL AS has_password,
               created_at, updated_at, last_login_at
        FROM users
        ORDER BY username
        "#,
    )
    .fetch_all(&state.db_pool)
    .await?;

    Ok(Json(users))
}

async fn get_user(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<UserResponse>> {
    require_scope(&auth, "admin")?;

    let user = fetch_user(&state, id).await?;
    Ok(Json(user))
}

async fn admin_register_start(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<AdminRegistrationStartRequest>,
) -> Result<Json<RegistrationResponse>> {
    require_scope(&auth, "admin")?;
    validate_new_user_input(&req.username, &req.email)?;
    ensure_user_unique(&state, &req.username, &req.email).await?;

    let response = registration_start(
        RegistrationRequest {
            username: req.username,
            message: req.message,
        },
        &state,
    )
    .await?;

    Ok(Json(response))
}

async fn admin_register_finish(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<AdminRegistrationFinishRequest>,
) -> Result<Json<CreateUserResponse>> {
    require_scope(&auth, "admin")?;
    validate_new_user_input(&req.username, &req.email)?;
    ensure_user_unique(&state, &req.username, &req.email).await?;

    let team_id = req.team_id.unwrap_or(auth.team_id);
    let role = req.role.unwrap_or_else(|| "member".to_string());
    validate_role(&role)?;

    let team_exists = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM teams WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(team_id)
    .fetch_one(&state.db_pool)
    .await?;
    if team_exists == 0 {
        return Err(Error::NotFound(format!("Team not found: {team_id}")));
    }

    let registration_bytes = finish_registration_upload(&req.message)?;
    let user_id = Uuid::new_v4();

    let mut tx = state.db_pool.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO users (id, username, email, is_service_account, is_active, opaque_registration)
        VALUES ($1, $2, $3, false, true, $4)
        "#,
    )
    .bind(user_id)
    .bind(&req.username)
    .bind(&req.email)
    .bind(registration_bytes)
    .execute(&mut *tx)
    .await?;

    sqlx::query("INSERT INTO team_memberships (user_id, team_id, role) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(team_id)
        .bind(&role)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    let user = fetch_user(&state, user_id).await?;
    Ok(Json(CreateUserResponse {
        user,
        team_id,
        role,
    }))
}

async fn change_password_start(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<PasswordRegistrationStartRequest>,
) -> Result<Json<RegistrationResponse>> {
    let username = get_username(&state, auth.user_id).await?;
    let response = registration_start(
        RegistrationRequest {
            username,
            message: req.message,
        },
        &state,
    )
    .await?;

    Ok(Json(response))
}

async fn change_password_finish(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<PasswordRegistrationFinishRequest>,
) -> Result<Json<serde_json::Value>> {
    let registration_bytes = finish_registration_upload(&req.message)?;

    let result =
        sqlx::query("UPDATE users SET opaque_registration = $1 WHERE id = $2 AND is_active = true")
            .bind(registration_bytes)
            .bind(auth.user_id)
            .execute(&state.db_pool)
            .await?;

    if result.rows_affected() == 0 {
        return Err(Error::NotFound("Active user not found".to_string()));
    }

    Ok(Json(serde_json::json!({ "message": "Password updated" })))
}

async fn fetch_user(state: &AppState, id: Uuid) -> Result<UserResponse> {
    sqlx::query_as::<_, UserResponse>(
        r#"
        SELECT id, username, email, is_service_account, is_active,
               opaque_registration IS NOT NULL AS has_password,
               created_at, updated_at, last_login_at
        FROM users
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.db_pool)
    .await?
    .ok_or_else(|| Error::NotFound(format!("User not found: {id}")))
}

async fn get_username(state: &AppState, user_id: Uuid) -> Result<String> {
    sqlx::query_scalar::<_, String>("SELECT username FROM users WHERE id = $1 AND is_active = true")
        .bind(user_id)
        .fetch_optional(&state.db_pool)
        .await?
        .ok_or_else(|| Error::NotFound("Active user not found".to_string()))
}

async fn ensure_user_unique(state: &AppState, username: &str, email: &str) -> Result<()> {
    let existing = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM users WHERE username = $1 OR email = $2",
    )
    .bind(username)
    .bind(email)
    .fetch_one(&state.db_pool)
    .await?;

    if existing > 0 {
        return Err(Error::InvalidInput(
            "Username or email is already in use".to_string(),
        ));
    }

    Ok(())
}

fn validate_new_user_input(username: &str, email: &str) -> Result<()> {
    if username.trim().is_empty() || email.trim().is_empty() {
        return Err(Error::InvalidInput(
            "Username and email are required".to_string(),
        ));
    }

    Ok(())
}

fn validate_role(role: &str) -> Result<()> {
    match role {
        "admin" | "member" | "readonly" => Ok(()),
        _ => Err(Error::InvalidInput(
            "Role must be admin, member, or readonly".to_string(),
        )),
    }
}
