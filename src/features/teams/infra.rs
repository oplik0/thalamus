//! Teams infrastructure implementations

use crate::error::{Error, Result};
use crate::features::authorization::PolicyManager;
use crate::features::authorization::domain::{AuthRequest, Authorizer};
use crate::features::authorization::infra::CasbinAuthorizer;
use crate::features::teams::domain::{
    MemberInfo, MembershipRepository, Project, ProjectRepository, Team, TeamAction,
    TeamHierarchyResolver, TeamMembership, TeamPermissionService, TeamRepository,
};
use async_trait::async_trait;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Team Repository
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SqlxTeamRepository {
    pool: PgPool,
}

impl SqlxTeamRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TeamRepository for SqlxTeamRepository {
    async fn create(
        &self,
        name: String,
        slug: Option<String>,
        description: Option<String>,
        parent_team_id: Option<Uuid>,
    ) -> Result<Team> {
        let team = sqlx::query_as::<_, Team>(
            r"
            INSERT INTO teams (name, slug, description, parent_team_id)
            VALUES ($1, $2, $3, $4)
            RETURNING *
            ",
        )
        .bind(&name)
        .bind(&slug)
        .bind(&description)
        .bind(parent_team_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(team)
    }

    async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<Team>> {
        let teams = sqlx::query_as::<_, Team>(
            r"
            SELECT t.*
            FROM teams t
            INNER JOIN team_memberships tm ON t.id = tm.team_id
            WHERE tm.user_id = $1
              AND tm.deleted_at IS NULL
              AND t.deleted_at IS NULL
            ORDER BY t.name
            ",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(teams)
    }

    async fn get_by_id(&self, id: Uuid) -> Result<Option<Team>> {
        let team = sqlx::query_as::<_, Team>(
            r"
            SELECT * FROM teams
            WHERE id = $1 AND deleted_at IS NULL
            ",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(team)
    }

    async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        description: Option<String>,
        is_active: Option<bool>,
    ) -> Result<Team> {
        let team = sqlx::query_as::<_, Team>(
            r"
            UPDATE teams
            SET
                name = COALESCE($2, name),
                description = COALESCE($3, description),
                is_active = COALESCE($4, is_active),
                updated_at = NOW()
            WHERE id = $1 AND deleted_at IS NULL
            RETURNING *
            ",
        )
        .bind(id)
        .bind(&name)
        .bind(&description)
        .bind(is_active)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => Error::NotFound(format!("Team not found: {id}")),
            _ => Error::Database(e),
        })?;

        Ok(team)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        sqlx::query(
            r"
            UPDATE teams
            SET deleted_at = NOW(), is_active = false
            WHERE id = $1 AND deleted_at IS NULL
            ",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Membership Repository
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SqlxMembershipRepository {
    pool: PgPool,
}

impl SqlxMembershipRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MembershipRepository for SqlxMembershipRepository {
    async fn add_member(
        &self,
        team_id: Uuid,
        user_id: Uuid,
        role: String,
    ) -> Result<TeamMembership> {
        let membership = sqlx::query_as::<_, TeamMembership>(
            r"
            INSERT INTO team_memberships (user_id, team_id, role)
            VALUES ($1, $2, $3)
            RETURNING *
            ",
        )
        .bind(user_id)
        .bind(team_id)
        .bind(&role)
        .fetch_one(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(membership)
    }

    async fn remove_member(&self, team_id: Uuid, user_id: Uuid) -> Result<()> {
        sqlx::query(
            r"
            UPDATE team_memberships
            SET deleted_at = NOW()
            WHERE team_id = $1 AND user_id = $2 AND deleted_at IS NULL
            ",
        )
        .bind(team_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(())
    }

    async fn update_member_role(
        &self,
        team_id: Uuid,
        user_id: Uuid,
        role: String,
    ) -> Result<TeamMembership> {
        let membership = sqlx::query_as::<_, TeamMembership>(
            r"
            UPDATE team_memberships
            SET role = $3
            WHERE team_id = $1 AND user_id = $2 AND deleted_at IS NULL
            RETURNING *
            ",
        )
        .bind(team_id)
        .bind(user_id)
        .bind(&role)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => Error::NotFound(format!(
                "Membership not found for team {team_id} and user {user_id}"
            )),
            _ => Error::Database(e),
        })?;

        Ok(membership)
    }

    async fn list_members(&self, team_id: Uuid) -> Result<Vec<MemberInfo>> {
        let rows = sqlx::query(
            r"
            SELECT tm.id, tm.user_id, tm.team_id, tm.role, tm.created_at, tm.deleted_at,
                   u.username, u.email
            FROM team_memberships tm
            INNER JOIN users u ON tm.user_id = u.id
            WHERE tm.team_id = $1 AND tm.deleted_at IS NULL
            ORDER BY tm.created_at
            ",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::Database)?;

        let members = rows
            .into_iter()
            .map(|row| MemberInfo {
                membership: TeamMembership {
                    id: row.get("id"),
                    user_id: row.get("user_id"),
                    team_id: row.get("team_id"),
                    role: row.get("role"),
                    created_at: row.get("created_at"),
                    deleted_at: row.get("deleted_at"),
                },
                username: row.get("username"),
                email: row.get("email"),
            })
            .collect();

        Ok(members)
    }

    async fn get_member(&self, team_id: Uuid, user_id: Uuid) -> Result<Option<TeamMembership>> {
        let membership = sqlx::query_as::<_, TeamMembership>(
            r"
            SELECT * FROM team_memberships
            WHERE team_id = $1 AND user_id = $2 AND deleted_at IS NULL
            ",
        )
        .bind(team_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(membership)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Project Repository
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SqlxProjectRepository {
    pool: PgPool,
}

impl SqlxProjectRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProjectRepository for SqlxProjectRepository {
    async fn create(
        &self,
        team_id: Uuid,
        name: String,
        description: Option<String>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Project> {
        let project = sqlx::query_as::<_, Project>(
            r"
            INSERT INTO projects (team_id, name, description, metadata)
            VALUES ($1, $2, $3, $4)
            RETURNING *
            ",
        )
        .bind(team_id)
        .bind(&name)
        .bind(&description)
        .bind(&metadata)
        .fetch_one(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(project)
    }

    async fn list_by_team(&self, team_id: Uuid) -> Result<Vec<Project>> {
        let projects = sqlx::query_as::<_, Project>(
            r"
            SELECT * FROM projects
            WHERE team_id = $1 AND deleted_at IS NULL
            ORDER BY name
            ",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(projects)
    }

    async fn get_by_id(&self, id: Uuid) -> Result<Option<Project>> {
        let project = sqlx::query_as::<_, Project>(
            r"
            SELECT * FROM projects
            WHERE id = $1 AND deleted_at IS NULL
            ",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(project)
    }

    async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        description: Option<String>,
        metadata: Option<serde_json::Value>,
    ) -> Result<Project> {
        let project = sqlx::query_as::<_, Project>(
            r"
            UPDATE projects
            SET
                name = COALESCE($2, name),
                description = COALESCE($3, description),
                metadata = COALESCE($4, metadata),
                updated_at = NOW()
            WHERE id = $1 AND deleted_at IS NULL
            RETURNING *
            ",
        )
        .bind(id)
        .bind(&name)
        .bind(&description)
        .bind(&metadata)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => Error::NotFound(format!("Project not found: {id}")),
            _ => Error::Database(e),
        })?;

        Ok(project)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        sqlx::query(
            r"
            UPDATE projects
            SET deleted_at = NOW()
            WHERE id = $1 AND deleted_at IS NULL
            ",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Team Hierarchy Resolver
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SqlxTeamHierarchyResolver {
    pool: PgPool,
}

impl SqlxTeamHierarchyResolver {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TeamHierarchyResolver for SqlxTeamHierarchyResolver {
    async fn get_ancestor_teams(&self, team_id: Uuid) -> Result<Vec<Uuid>> {
        let rows = sqlx::query_as::<_, (Uuid,)>(
            r"
            WITH RECURSIVE ancestors AS (
                SELECT parent_team_id
                FROM teams
                WHERE id = $1 AND deleted_at IS NULL
                UNION ALL
                SELECT t.parent_team_id
                FROM teams t
                INNER JOIN ancestors a ON t.id = a.parent_team_id
                WHERE t.deleted_at IS NULL
            )
            SELECT parent_team_id FROM ancestors WHERE parent_team_id IS NOT NULL
            ",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn get_descendant_teams(&self, team_id: Uuid) -> Result<Vec<Uuid>> {
        let rows = sqlx::query_as::<_, (Uuid,)>(
            r"
            WITH RECURSIVE descendants AS (
                SELECT id
                FROM teams
                WHERE parent_team_id = $1 AND deleted_at IS NULL
                UNION ALL
                SELECT t.id
                FROM teams t
                INNER JOIN descendants d ON t.parent_team_id = d.id
                WHERE t.deleted_at IS NULL
            )
            SELECT id FROM descendants
            ",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn is_member_including_ancestors(&self, user_id: Uuid, team_id: Uuid) -> Result<bool> {
        // Check direct membership
        let direct: Option<(bool,)> = sqlx::query_as(
            r"
            SELECT EXISTS (
                SELECT 1 FROM team_memberships
                WHERE user_id = $1 AND team_id = $2 AND deleted_at IS NULL
            )
            ",
        )
        .bind(user_id)
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        if direct.is_some_and(|(b,)| b) {
            return Ok(true);
        }

        // Check membership in any ancestor team
        let ancestor: Option<(bool,)> = sqlx::query_as(
            r"
            WITH RECURSIVE ancestors AS (
                SELECT parent_team_id
                FROM teams
                WHERE id = $2 AND deleted_at IS NULL
                UNION ALL
                SELECT t.parent_team_id
                FROM teams t
                INNER JOIN ancestors a ON t.id = a.parent_team_id
                WHERE t.deleted_at IS NULL
            )
            SELECT EXISTS (
                SELECT 1 FROM team_memberships tm
                INNER JOIN ancestors a ON tm.team_id = a.parent_team_id
                WHERE tm.user_id = $1 AND tm.deleted_at IS NULL
            )
            ",
        )
        .bind(user_id)
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::Database)?;

        Ok(ancestor.is_some_and(|(b,)| b))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Casbin Team Permission Service
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct CasbinTeamPermissionService {
    authorizer: Arc<CasbinAuthorizer>,
}

impl CasbinTeamPermissionService {
    #[must_use]
    pub fn new(authorizer: Arc<CasbinAuthorizer>) -> Self {
        Self { authorizer }
    }
}

#[async_trait]
impl TeamPermissionService for CasbinTeamPermissionService {
    async fn create_default_team_policies(&self, team_id: Uuid) -> Result<()> {
        let team_id_str = team_id.to_string();
        let policies: Vec<(&str, &str, &str)> = vec![
            ("team_admin", "*", "*"),
            ("team_member", "/v1/api-keys", "POST"),
            ("team_member", "/v1/api-keys", "GET"),
            ("team_member", "/v1/chat/completions", "POST"),
            ("team_member", "/v1/models", "GET"),
            ("team_readonly", "/v1/models", "GET"),
            ("team_readonly", "/v1/chat/completions", "POST"),
            ("team_readonly", "/health", "GET"),
        ];

        for (role, object, action) in policies {
            self.authorizer
                .add_policy(role, &team_id_str, object, action)
                .await?;
        }

        Ok(())
    }

    async fn add_user_role(&self, user_id: Uuid, role: String, team_id: Uuid) -> Result<()> {
        self.authorizer
            .add_role(&user_id.to_string(), &role, &team_id.to_string())
            .await?;
        Ok(())
    }

    async fn remove_user_role(&self, user_id: Uuid, team_id: Uuid) -> Result<()> {
        let roles = self
            .authorizer
            .get_roles(&user_id.to_string(), &team_id.to_string())
            .await?;

        for role in roles {
            self.authorizer
                .remove_role(&user_id.to_string(), &role, &team_id.to_string())
                .await?;
        }

        Ok(())
    }

    async fn update_user_role(&self, user_id: Uuid, new_role: String, team_id: Uuid) -> Result<()> {
        // Remove all existing roles
        let roles = self
            .authorizer
            .get_roles(&user_id.to_string(), &team_id.to_string())
            .await?;

        for role in roles {
            self.authorizer
                .remove_role(&user_id.to_string(), &role, &team_id.to_string())
                .await?;
        }

        // Add new role
        self.authorizer
            .add_role(&user_id.to_string(), &new_role, &team_id.to_string())
            .await?;

        Ok(())
    }

    async fn remove_team_policies(&self, team_id: Uuid) -> Result<()> {
        let policies = self.authorizer.get_policies().await?;
        let team_id_str = team_id.to_string();

        for policy in policies {
            if policy.len() >= 4 && policy[1] == team_id_str {
                self.authorizer
                    .remove_policy(&policy[0], &policy[1], &policy[2], &policy[3])
                    .await?;
            }
        }

        Ok(())
    }

    async fn check_team_permission(
        &self,
        user_id: Uuid,
        team_id: Uuid,
        action: TeamAction,
    ) -> Result<bool> {
        let request = AuthRequest {
            subject: user_id.to_string(),
            domain: team_id.to_string(),
            object: "*".to_string(),
            action: action.as_str().to_string(),
        };

        self.authorizer.is_authorized(&request).await
    }
}
