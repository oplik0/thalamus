//! Teams API handlers

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::teams::domain::{
    MemberInfo, Team, TEAM_ROLE_ADMIN,
};
use crate::features::teams::dto::{
    AddMemberRequest, CreateProjectRequest, CreateTeamRequest, MemberResponse, ProjectResponse,
    SetParentRequest, TeamResponse, UpdateMemberRoleRequest, UpdateProjectRequest,
    UpdateTeamRequest,
};
use crate::middleware::{ApiKeyAuth, require_scope};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post, put},
};
use uuid::Uuid;

/// Create teams router
pub fn router() -> Router<AppState> {
    Router::new()
        // Teams
        .route("/v1/teams", post(create_team))
        .route("/v1/teams", get(list_teams))
        .route("/v1/teams/{id}", get(get_team))
        .route("/v1/teams/{id}", put(update_team))
        .route("/v1/teams/{id}", delete(delete_team))
        .route("/v1/teams/{id}/parent", put(set_parent))
        .route("/v1/teams/{id}/parent", delete(remove_parent))
        // Members
        .route("/v1/teams/{id}/members", post(add_member))
        .route("/v1/teams/{id}/members", get(list_members))
        .route("/v1/teams/{id}/members/{user_id}", delete(remove_member))
        .route("/v1/teams/{id}/members/{user_id}", put(update_member_role))
        // Projects
        .route("/v1/teams/{team_id}/projects", post(create_project))
        .route("/v1/teams/{team_id}/projects", get(list_projects))
        .route("/v1/teams/{team_id}/projects/{id}", get(get_project))
        .route("/v1/teams/{team_id}/projects/{id}", put(update_project))
        .route("/v1/teams/{team_id}/projects/{id}", delete(delete_project))
}

// ─────────────────────────────────────────────────────────────────────────────
// Team Handlers
// ─────────────────────────────────────────────────────────────────────────────

async fn create_team(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<CreateTeamRequest>,
) -> Result<Json<TeamResponse>> {
    require_scope(&auth, "teams:create")?;

    // Validate parent team if provided
    if let Some(parent_id) = req.parent_team_id {
        let parent = state
            .team_repository
            .get_by_id(parent_id)
            .await?;
        if parent.is_none() {
            return Err(Error::NotFound(format!(
                "Parent team not found: {}",
                parent_id
            )));
        }
    }

    // Create the team
    let team = state
        .team_repository
        .create(
            req.name,
            None, // slug generated from name
            req.description,
            req.parent_team_id,
        )
        .await?;

    // Create default Casbin policies for the team
    state
        .team_permission_service
        .create_default_team_policies(team.id)
        .await?;

    // Add creator as team admin
    state
        .membership_repository
        .add_member(team.id, auth.user_id, TEAM_ROLE_ADMIN.to_string())
        .await?;

    // Add Casbin role for creator
    state
        .team_permission_service
        .add_user_role(auth.user_id, TEAM_ROLE_ADMIN.to_string(), team.id)
        .await?;

    tracing::info!(
        team_id = %team.id,
        user_id = %auth.user_id,
        "Team created with creator as admin"
    );

    Ok(Json(team_to_response(team)))
}

async fn list_teams(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
) -> Result<Json<Vec<TeamResponse>>> {
    require_scope(&auth, "teams:read")?;

    let teams = state.team_repository.list_for_user(auth.user_id).await?;

    Ok(Json(teams.into_iter().map(team_to_response).collect()))
}

async fn get_team(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<TeamResponse>> {
    require_scope(&auth, "teams:read")?;

    let team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is member (including ancestors)
    let is_member = state
        .team_hierarchy_resolver
        .is_member_including_ancestors(auth.user_id, id)
        .await?;

    if !is_member {
        return Err(Error::Authorization(
            "You are not a member of this team".to_string(),
        ));
    }

    Ok(Json(team_to_response(team)))
}

async fn update_team(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateTeamRequest>,
) -> Result<Json<TeamResponse>> {
    require_scope(&auth, "teams:update")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can update team details".to_string(),
        ));
    }

    // Update team
    let updated = state
        .team_repository
        .update(id, req.name, req.description, req.is_active)
        .await?;

    Ok(Json(team_to_response(updated)))
}

async fn delete_team(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "teams:delete")?;

    // Verify team exists
    let team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can delete teams".to_string(),
        ));
    }

    // 1. Revoke all API keys for this team
    sqlx::query!(
        "UPDATE api_keys SET is_active = false, revoked_at = NOW() WHERE team_id = $1 AND is_active = true",
        id
    )
    .execute(&state.db_pool)
    .await?;

    // 2. Soft-delete all projects
    sqlx::query!(
        "UPDATE projects SET deleted_at = NOW() WHERE team_id = $1 AND deleted_at IS NULL",
        id
    )
    .execute(&state.db_pool)
    .await?;

    // 3. Soft-delete all memberships
    sqlx::query!(
        "UPDATE team_memberships SET deleted_at = NOW() WHERE team_id = $1 AND deleted_at IS NULL",
        id
    )
    .execute(&state.db_pool)
    .await?;

    // 4. Remove all Casbin policies for this team
    state
        .team_permission_service
        .remove_team_policies(id)
        .await?;

    // 5. Soft-delete the team
    state.team_repository.delete(id).await?;

    tracing::info!(
        team_id = %id,
        team_name = %team.name,
        "Team deleted with cascade"
    );

    Ok(Json(serde_json::json!({
        "message": "Team deleted successfully",
        "id": id,
        "name": team.name,
    })))
}

async fn set_parent(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
    Json(req): Json<SetParentRequest>,
) -> Result<Json<TeamResponse>> {
    require_scope(&auth, "teams:update")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can change parent team".to_string(),
        ));
    }

    let new_parent_id = req.parent_team_id;

    // Check for self-parent
    if let Some(parent_id) = new_parent_id {
        if parent_id == id {
            return Err(Error::InvalidInput(
                "A team cannot be its own parent".to_string(),
            ));
        }

        // Verify parent team exists
        let parent = state
            .team_repository
            .get_by_id(parent_id)
            .await?;
        if parent.is_none() {
            return Err(Error::NotFound(format!(
                "Parent team not found: {}",
                parent_id
            )));
        }

        // Check for cycles using recursive CTE
        let descendants = state
            .team_hierarchy_resolver
            .get_descendant_teams(id)
            .await?;

        if descendants.contains(&parent_id) {
            return Err(Error::InvalidInput(
                "Setting this parent would create a cycle".to_string(),
            ));
        }
    }

    // Update parent
    let _updated = state
        .team_repository
        .update(id, None, None, None)
        .await?;

    // Update parent_team_id separately
    sqlx::query!(
        "UPDATE teams SET parent_team_id = $1, updated_at = NOW() WHERE id = $2",
        new_parent_id,
        id
    )
    .execute(&state.db_pool)
    .await?;

    // Fetch updated team
    let updated_team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    Ok(Json(team_to_response(updated_team)))
}

async fn remove_parent(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<TeamResponse>> {
    require_scope(&auth, "teams:update")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can remove parent team".to_string(),
        ));
    }

    // Remove parent
    sqlx::query!(
        "UPDATE teams SET parent_team_id = NULL, updated_at = NOW() WHERE id = $1",
        id
    )
    .execute(&state.db_pool)
    .await?;

    // Fetch updated team
    let updated_team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    Ok(Json(team_to_response(updated_team)))
}

// ─────────────────────────────────────────────────────────────────────────────
// Member Handlers
// ─────────────────────────────────────────────────────────────────────────────

async fn add_member(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Result<Json<MemberResponse>> {
    require_scope(&auth, "teams:manage_members")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can add members".to_string(),
        ));
    }

    // Check if user is already a member
    let existing = state
        .membership_repository
        .get_member(id, req.user_id)
        .await?;

    if existing.is_some() {
        return Err(Error::InvalidInput(
            "User is already a member of this team".to_string(),
        ));
    }

    // Validate role
    let valid_roles = vec!["team_admin", "team_member", "team_readonly"];
    if !valid_roles.contains(&req.role.as_str()) {
        return Err(Error::InvalidInput(format!(
            "Invalid role: {}. Must be one of: {:?}",
            req.role, valid_roles
        )));
    }

    // Add member
    let member = state
        .membership_repository
        .add_member(id, req.user_id, req.role.clone())
        .await?;

    // Add Casbin role
    state
        .team_permission_service
        .add_user_role(req.user_id, req.role, id)
        .await?;

    // Fetch user details for response
    let user = sqlx::query!(
        "SELECT username, email FROM users WHERE id = $1",
        req.user_id
    )
    .fetch_one(&state.db_pool)
    .await?;

    Ok(Json(MemberResponse {
        id: member.id,
        user_id: member.user_id,
        username: user.username,
        email: user.email,
        role: member.role,
        created_at: member.created_at,
    }))
}

async fn list_members(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<MemberResponse>>> {
    require_scope(&auth, "teams:read")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is member (including ancestors)
    let is_member = state
        .team_hierarchy_resolver
        .is_member_including_ancestors(auth.user_id, id)
        .await?;

    if !is_member {
        return Err(Error::Authorization(
            "You are not a member of this team".to_string(),
        ));
    }

    let members = state.membership_repository.list_members(id).await?;

    Ok(Json(
        members.into_iter().map(member_info_to_response).collect(),
    ))
}

async fn remove_member(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "teams:manage_members")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can remove members".to_string(),
        ));
    }

    // Check if target user is a member
    let target = state
        .membership_repository
        .get_member(id, user_id)
        .await?;

    if target.is_none() {
        return Err(Error::NotFound(format!(
            "Member not found in team: {}",
            user_id
        )));
    }

    // Remove member
    state.membership_repository.remove_member(id, user_id).await?;

    // Remove Casbin role
    state
        .team_permission_service
        .remove_user_role(user_id, id)
        .await?;

    Ok(Json(serde_json::json!({
        "message": "Member removed successfully",
        "user_id": user_id,
        "team_id": id,
    })))
}

async fn update_member_role(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateMemberRoleRequest>,
) -> Result<Json<MemberResponse>> {
    require_scope(&auth, "teams:manage_members")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can change member roles".to_string(),
        ));
    }

    // Validate role
    let valid_roles = vec!["team_admin", "team_member", "team_readonly"];
    if !valid_roles.contains(&req.role.as_str()) {
        return Err(Error::InvalidInput(format!(
            "Invalid role: {}. Must be one of: {:?}",
            req.role, valid_roles
        )));
    }

    // Update member role
    let member = state
        .membership_repository
        .update_member_role(id, user_id, req.role.clone())
        .await?;

    // Update Casbin role
    state
        .team_permission_service
        .update_user_role(user_id, req.role, id)
        .await?;

    // Fetch user details
    let user = sqlx::query!(
        "SELECT username, email FROM users WHERE id = $1",
        user_id
    )
    .fetch_one(&state.db_pool)
    .await?;

    Ok(Json(MemberResponse {
        id: member.id,
        user_id: member.user_id,
        username: user.username,
        email: user.email,
        role: member.role,
        created_at: member.created_at,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Project Handlers
// ─────────────────────────────────────────────────────────────────────────────

async fn create_project(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(team_id): Path<Uuid>,
    Json(req): Json<CreateProjectRequest>,
) -> Result<Json<ProjectResponse>> {
    require_scope(&auth, "projects:create")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(team_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", team_id)))?;

    // Check if user is member
    let is_member = state
        .team_hierarchy_resolver
        .is_member_including_ancestors(auth.user_id, team_id)
        .await?;

    if !is_member {
        return Err(Error::Authorization(
            "You are not a member of this team".to_string(),
        ));
    }

    // Create project
    let project = state
        .project_repository
        .create(team_id, req.name, req.description, req.metadata)
        .await?;

    Ok(Json(project_to_response(project)))
}

async fn list_projects(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(team_id): Path<Uuid>,
) -> Result<Json<Vec<ProjectResponse>>> {
    require_scope(&auth, "projects:read")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(team_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", team_id)))?;

    // Check if user is member
    let is_member = state
        .team_hierarchy_resolver
        .is_member_including_ancestors(auth.user_id, team_id)
        .await?;

    if !is_member {
        return Err(Error::Authorization(
            "You are not a member of this team".to_string(),
        ));
    }

    let projects = state.project_repository.list_by_team(team_id).await?;

    Ok(Json(
        projects.into_iter().map(project_to_response).collect(),
    ))
}

async fn get_project(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path((team_id, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ProjectResponse>> {
    require_scope(&auth, "projects:read")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(team_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", team_id)))?;

    // Check if user is member
    let is_member = state
        .team_hierarchy_resolver
        .is_member_including_ancestors(auth.user_id, team_id)
        .await?;

    if !is_member {
        return Err(Error::Authorization(
            "You are not a member of this team".to_string(),
        ));
    }

    let project = state
        .project_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Project not found: {}", id)))?;

    // Verify project belongs to team
    if project.team_id != team_id {
        return Err(Error::NotFound(
            "Project not found in this team".to_string(),
        ));
    }

    Ok(Json(project_to_response(project)))
}

async fn update_project(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path((team_id, id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateProjectRequest>,
) -> Result<Json<ProjectResponse>> {
    require_scope(&auth, "projects:update")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(team_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", team_id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(team_id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can update projects".to_string(),
        ));
    }

    let project = state
        .project_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Project not found: {}", id)))?;

    // Verify project belongs to team
    if project.team_id != team_id {
        return Err(Error::NotFound(
            "Project not found in this team".to_string(),
        ));
    }

    // Update project
    let updated = state
        .project_repository
        .update(id, req.name, req.description, req.metadata)
        .await?;

    Ok(Json(project_to_response(updated)))
}

async fn delete_project(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path((team_id, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "projects:delete")?;

    // Verify team exists
    let _team = state
        .team_repository
        .get_by_id(team_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Team not found: {}", team_id)))?;

    // Check if user is admin
    let membership = state
        .membership_repository
        .get_member(team_id, auth.user_id)
        .await?;

    let is_admin = membership
        .as_ref()
        .map(|m| m.role == TEAM_ROLE_ADMIN)
        .unwrap_or(false);

    if !is_admin {
        return Err(Error::Authorization(
            "Only team admins can delete projects".to_string(),
        ));
    }

    let project = state
        .project_repository
        .get_by_id(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("Project not found: {}", id)))?;

    // Verify project belongs to team
    if project.team_id != team_id {
        return Err(Error::NotFound(
            "Project not found in this team".to_string(),
        ));
    }

    // Soft-delete project
    state.project_repository.delete(id).await?;

    Ok(Json(serde_json::json!({
        "message": "Project deleted successfully",
        "id": id,
        "name": project.name,
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper Functions
// ─────────────────────────────────────────────────────────────────────────────

fn team_to_response(team: Team) -> TeamResponse {
    TeamResponse {
        id: team.id,
        name: team.name,
        slug: team.slug,
        description: team.description,
        parent_team_id: team.parent_team_id,
        is_active: team.is_active,
        created_at: team.created_at,
        updated_at: team.updated_at,
    }
}

fn member_info_to_response(info: MemberInfo) -> MemberResponse {
    MemberResponse {
        id: info.membership.id,
        user_id: info.membership.user_id,
        username: info.username,
        email: info.email,
        role: info.membership.role,
        created_at: info.membership.created_at,
    }
}

fn project_to_response(project: crate::features::teams::domain::Project) -> ProjectResponse {
    ProjectResponse {
        id: project.id,
        team_id: project.team_id,
        name: project.name,
        description: project.description,
        metadata: project.metadata,
        created_at: project.created_at,
        updated_at: project.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_router_compiles() {
        let _router = router();
    }
}
