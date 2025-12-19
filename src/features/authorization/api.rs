//! Authorization API endpoints
//!
//! Provides HTTP endpoints for managing Casbin policies and role assignments.
//! All endpoints require admin authentication.

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::authorization::domain::PolicyManager;
use crate::middleware::{ApiKeyAuth, require_scope};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};

/// Policy request/response types

#[derive(Debug, Deserialize)]
pub struct CreatePolicyRequest {
    pub subject: String,
    pub domain: String,
    pub object: String,
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct PolicyResponse {
    pub subject: String,
    pub domain: String,
    pub object: String,
    pub action: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    pub user: String,
    pub role: String,
    pub domain: String,
}

#[derive(Debug, Serialize)]
pub struct RoleResponse {
    pub user: String,
    pub role: String,
    pub domain: String,
}

#[derive(Debug, Serialize)]
pub struct RolesListResponse {
    pub user: String,
    pub domain: String,
    pub roles: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PoliciesListResponse {
    pub policies: Vec<PolicyResponse>,
}

/// Create the authorization router
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/policies", get(list_policies).post(create_policy))
        .route(
            "/policies/{subject}/{domain}",
            get(get_policies_for_subject),
        )
        .route(
            "/policies/{subject}/{domain}/{object}/{action}",
            delete(delete_policy),
        )
        .route("/roles", post(assign_role))
        .route(
            "/roles/{user}/{domain}",
            get(list_roles).delete(remove_role),
        )
}

/// List all policies
async fn list_policies(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
) -> Result<Json<PoliciesListResponse>> {
    require_scope(&auth, "admin")?;

    let authorizer = state
        .authorizer
        .as_ref()
        .ok_or_else(|| Error::Config("Authorization not initialized".to_string()))?;

    let policies = authorizer.get_policies().await?;

    let response = PoliciesListResponse {
        policies: policies
            .into_iter()
            .filter_map(|p| {
                if p.len() >= 4 {
                    Some(PolicyResponse {
                        subject: p[0].clone(),
                        domain: p[1].clone(),
                        object: p[2].clone(),
                        action: p[3].clone(),
                    })
                } else {
                    None
                }
            })
            .collect(),
    };

    Ok(Json(response))
}

/// Create a new policy
async fn create_policy(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Json(request): Json<CreatePolicyRequest>,
) -> Result<Json<PolicyResponse>> {
    require_scope(&auth, "admin")?;

    let authorizer = state
        .authorizer
        .as_ref()
        .ok_or_else(|| Error::Config("Authorization not initialized".to_string()))?;

    authorizer
        .add_policy(
            &request.subject,
            &request.domain,
            &request.object,
            &request.action,
        )
        .await?;

    Ok(Json(PolicyResponse {
        subject: request.subject,
        domain: request.domain,
        object: request.object,
        action: request.action,
    }))
}

/// Get policies for a specific subject in a domain
async fn get_policies_for_subject(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Path((subject, domain)): Path<(String, String)>,
) -> Result<Json<PoliciesListResponse>> {
    require_scope(&auth, "admin")?;

    let authorizer = state
        .authorizer
        .as_ref()
        .ok_or_else(|| Error::Config("Authorization not initialized".to_string()))?;

    let policies = authorizer
        .get_policies_for_subject(&subject, &domain)
        .await?;

    let response = PoliciesListResponse {
        policies: policies
            .into_iter()
            .filter_map(|p| {
                if p.len() >= 4 {
                    Some(PolicyResponse {
                        subject: p[0].clone(),
                        domain: p[1].clone(),
                        object: p[2].clone(),
                        action: p[3].clone(),
                    })
                } else {
                    None
                }
            })
            .collect(),
    };

    Ok(Json(response))
}

/// Delete a policy
async fn delete_policy(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Path((subject, domain, object, action)): Path<(String, String, String, String)>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "admin")?;

    let authorizer = state
        .authorizer
        .as_ref()
        .ok_or_else(|| Error::Config("Authorization not initialized".to_string()))?;

    let removed = authorizer
        .remove_policy(&subject, &domain, &object, &action)
        .await?;

    Ok(Json(serde_json::json!({
        "removed": removed,
        "subject": subject,
        "domain": domain,
        "object": object,
        "action": action,
    })))
}

/// Assign a role to a user in a domain
async fn assign_role(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Json(request): Json<CreateRoleRequest>,
) -> Result<Json<RoleResponse>> {
    require_scope(&auth, "admin")?;

    let authorizer = state
        .authorizer
        .as_ref()
        .ok_or_else(|| Error::Config("Authorization not initialized".to_string()))?;

    authorizer
        .add_role(&request.user, &request.role, &request.domain)
        .await?;

    Ok(Json(RoleResponse {
        user: request.user,
        role: request.role,
        domain: request.domain,
    }))
}

/// List roles for a user in a domain
async fn list_roles(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Path((user, domain)): Path<(String, String)>,
) -> Result<Json<RolesListResponse>> {
    require_scope(&auth, "admin")?;

    let authorizer = state
        .authorizer
        .as_ref()
        .ok_or_else(|| Error::Config("Authorization not initialized".to_string()))?;

    let roles = authorizer.get_roles(&user, &domain).await?;

    Ok(Json(RolesListResponse {
        user,
        domain,
        roles,
    }))
}

/// Remove a role assignment
async fn remove_role(
    ApiKeyAuth(auth): ApiKeyAuth,
    State(state): State<AppState>,
    Path((user, domain)): Path<(String, String)>,
    Json(request): Json<RemoveRoleRequest>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "admin")?;

    let authorizer = state
        .authorizer
        .as_ref()
        .ok_or_else(|| Error::Config("Authorization not initialized".to_string()))?;

    let removed = authorizer
        .remove_role(&user, &request.role, &domain)
        .await?;

    Ok(Json(serde_json::json!({
        "removed": removed,
        "user": user,
        "role": request.role,
        "domain": domain,
    })))
}

#[derive(Debug, Deserialize)]
pub struct RemoveRoleRequest {
    pub role: String,
}
