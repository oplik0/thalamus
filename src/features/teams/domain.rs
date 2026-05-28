//! Teams domain types and traits

use crate::error::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// Team entity
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub parent_team_id: Option<Uuid>,
    pub is_active: bool,
    pub budget_limit_usd: Option<f64>,
    pub rate_limit_rpm: Option<i32>,
    pub rate_limit_burst: Option<i32>,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_backends: Option<Vec<String>>,
    pub allowed_tags: Option<Vec<String>>,
    pub logging_policy: Option<String>,
    pub log_retention_days: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Team membership entity
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TeamMembership {
    pub id: Uuid,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Project entity
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Project {
    pub id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Member information for listing team members
#[derive(Debug, Clone, Serialize)]
pub struct MemberInfo {
    pub membership: TeamMembership,
    pub username: String,
    pub email: String,
}

/// Default team roles
pub const TEAM_ROLE_ADMIN: &str = "team_admin";
pub const TEAM_ROLE_MEMBER: &str = "team_member";
pub const TEAM_ROLE_READONLY: &str = "team_readonly";

/// Actions for team permission checks
#[derive(Debug)]
pub enum TeamAction {
    Create,
    Read,
    Update,
    Delete,
    ManageMembers,
    CreateProject,
    ReadProject,
    UpdateProject,
    DeleteProject,
}

impl TeamAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            TeamAction::Create => "create",
            TeamAction::Read => "read",
            TeamAction::Update => "update",
            TeamAction::Delete => "delete",
            TeamAction::ManageMembers => "manage_members",
            TeamAction::CreateProject => "create_project",
            TeamAction::ReadProject => "read_project",
            TeamAction::UpdateProject => "update_project",
            TeamAction::DeleteProject => "delete_project",
        }
    }
}

/// Team repository trait
#[async_trait]
pub trait TeamRepository: Send + Sync {
    async fn create(
        &self,
        name: String,
        slug: Option<String>,
        description: Option<String>,
        parent_team_id: Option<Uuid>,
    ) -> Result<Team>;
    async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<Team>>;
    async fn get_by_id(&self, id: Uuid) -> Result<Option<Team>>;
    async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        description: Option<String>,
        is_active: Option<bool>,
    ) -> Result<Team>;
    async fn delete(&self, id: Uuid) -> Result<()>;
}

/// Team membership repository trait
#[async_trait]
pub trait MembershipRepository: Send + Sync {
    async fn add_member(
        &self,
        team_id: Uuid,
        user_id: Uuid,
        role: String,
    ) -> Result<TeamMembership>;
    async fn remove_member(&self, team_id: Uuid, user_id: Uuid) -> Result<()>;
    async fn update_member_role(
        &self,
        team_id: Uuid,
        user_id: Uuid,
        role: String,
    ) -> Result<TeamMembership>;
    async fn list_members(&self, team_id: Uuid) -> Result<Vec<MemberInfo>>;
    async fn get_member(&self, team_id: Uuid, user_id: Uuid) -> Result<Option<TeamMembership>>;
}

/// Team hierarchy resolver
#[async_trait]
pub trait TeamHierarchyResolver: Send + Sync {
    async fn get_ancestor_teams(&self, team_id: Uuid) -> Result<Vec<Uuid>>;
    async fn get_descendant_teams(&self, team_id: Uuid) -> Result<Vec<Uuid>>;
    async fn is_member_including_ancestors(&self, user_id: Uuid, team_id: Uuid) -> Result<bool>;
}

/// Project repository trait
#[async_trait]
pub trait ProjectRepository: Send + Sync {
    async fn create(
        &self,
        team_id: Uuid,
        name: String,
        description: Option<String>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Project>;
    async fn list_by_team(&self, team_id: Uuid) -> Result<Vec<Project>>;
    async fn get_by_id(&self, id: Uuid) -> Result<Option<Project>>;
    async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        description: Option<String>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Project>;
    async fn delete(&self, id: Uuid) -> Result<()>;
}

/// Team permission service (Casbin-backed)
#[async_trait]
pub trait TeamPermissionService: Send + Sync {
    async fn create_default_team_policies(&self, team_id: Uuid) -> Result<()>;
    async fn add_user_role(&self, user_id: Uuid, role: String, team_id: Uuid) -> Result<()>;
    async fn remove_user_role(&self, user_id: Uuid, team_id: Uuid) -> Result<()>;
    async fn update_user_role(&self, user_id: Uuid, new_role: String, team_id: Uuid) -> Result<()>;
    async fn remove_team_policies(&self, team_id: Uuid) -> Result<()>;
    async fn check_team_permission(
        &self,
        user_id: Uuid,
        team_id: Uuid,
        action: TeamAction,
    ) -> Result<bool>;
}
