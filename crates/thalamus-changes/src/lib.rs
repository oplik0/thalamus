//! Durable GitOps proposals. Apply the bundled migration before use.
//!
//! The server must authenticate and authorize the actor before calling this crate;
//! neither an actor string nor a forge review is proof of identity. Git credentials
//! and the CI-equivalent render command are deployment configuration, never requests.
//! GitHub's merge endpoint atomically guards only the head SHA, not the base or
//! target. Protect every possible target with required checks and reviews, restrict
//! retargeting and bypass, and do not treat the forge guard as an atomic guarantee.

use chrono::{DateTime, Utc};
use forge::{
    BranchPolicies, ChangeRequest, ChangeRequests, ChangeState, Comments, CreateChangeRequest,
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::path::PathBuf;
use thalamus_gitops::{CommitAuthor, GitOpsRepo, Patch, RenderCommand};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("idempotency key was already used for another request")]
    IdempotencyConflict,
    #[error("change not found")]
    NotFound,
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("serialization failed")]
    Serialization(#[from] serde_json::Error),
    // Process output can contain credentials or repository contents. Never expose it.
    #[error("GitOps operation failed; inspect the configured worker environment")]
    GitOps,
    #[error("forge operation failed")]
    Forge,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    /// Service identity is descriptive; callers must authorize every patch path.
    pub service: String,
    pub base_sha: String,
    pub target_branch: String,
    pub title: String,
    pub patches: Vec<Patch>,
    /// Inverse of a catalog-only single-parent commit; mutually exclusive with patches.
    #[serde(default)]
    pub revert_commit: Option<String>,
    /// Included in the PR description; computed by the caller from catalog evidence.
    pub validation_tier: String,
    pub rollout: Rollout,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Rollout {
    Gated,
    Rolling,
    InPlace {
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
    },
}
impl Proposal {
    pub fn validate(&self) -> Result<()> {
        if self.service.trim().is_empty()
            || self.service.len() > 253
            || self.service.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid service"));
        }
        if !matches!(self.base_sha.len(), 40 | 64)
            || !self.base_sha.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Error::Invalid("base must be a full commit ID"));
        }
        if self.target_branch.is_empty()
            || self.target_branch.len() > 255
            || self.target_branch.starts_with('-')
            || self.target_branch.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid target branch"));
        }
        if self.title.trim().is_empty()
            || self.title.len() > 256
            || self.title.chars().any(char::is_control)
            || self.validation_tier.len() > 256
            || self.validation_tier.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid title or validation tier"));
        }
        match &self.revert_commit {
            None if self.patches.is_empty() || self.patches.len() > 256 => {
                return Err(Error::Invalid("invalid patch count"))
            }
            Some(commit)
                if !self.patches.is_empty()
                    || !matches!(commit.len(), 40 | 64)
                    || !commit.bytes().all(|c| c.is_ascii_hexdigit()) =>
            {
                return Err(Error::Invalid(
                    "revert requires one full commit ID and no patches",
                ))
            }
            _ => {}
        }
        if let Rollout::InPlace {
            window_start,
            window_end,
        } = &self.rollout
        {
            if window_start >= window_end {
                return Err(Error::Invalid("invalid rollout window"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub id: Uuid,
    pub actor: String,
    pub idempotency_key: String,
    pub request: Proposal,
    pub state: String,
    pub branch: String,
    pub head_sha: Option<String>,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    pub base_render_sha256: Option<String>,
    pub proposed_render_sha256: Option<String>,
    pub data_diff: Option<String>,
    pub rendered_diff: Option<String>,
    pub error: Option<String>,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn preview_matches(change: &Change, preview: &thalamus_gitops::Preview) -> bool {
    change.base_render_sha256.as_deref() == Some(digest(preview.base_render.as_bytes()).as_str())
        && change.proposed_render_sha256.as_deref()
            == Some(digest(preview.proposed_render.as_bytes()).as_str())
        && change.data_diff.as_deref() == Some(preview.data_diff.as_str())
        && change.rendered_diff.as_deref() == Some(preview.rendered_diff.as_str())
}
fn decode(row: &sqlx::postgres::PgRow) -> Result<Change> {
    Ok(Change {
        id: row.try_get("id")?,
        actor: row.try_get("actor")?,
        idempotency_key: row.try_get("idempotency_key")?,
        request: serde_json::from_value(row.try_get("request")?)?,
        state: row.try_get("state")?,
        branch: row.try_get("branch")?,
        head_sha: row.try_get("head_sha")?,
        pr_number: row.try_get("pr_number")?,
        pr_url: row.try_get("pr_url")?,
        base_render_sha256: row.try_get("base_render_sha256")?,
        proposed_render_sha256: row.try_get("proposed_render_sha256")?,
        data_diff: row.try_get("data_diff")?,
        rendered_diff: row.try_get("rendered_diff")?,
        error: row.try_get("error")?,
    })
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}
impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    /// Caller must supply an authenticated, authorized actor, including any agent attribution.
    pub async fn propose(&self, actor: &str, key: &str, request: Proposal) -> Result<Change> {
        let mut tx = self.pool.begin().await?;
        let change = self
            .propose_in_transaction(&mut tx, actor, key, request)
            .await?;
        tx.commit().await?;
        Ok(change)
    }
    /// Caller owns the transaction and must authorize the actor in that transaction.
    pub async fn propose_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        actor: &str,
        key: &str,
        request: Proposal,
    ) -> Result<Change> {
        request.validate()?;
        if actor.trim().is_empty()
            || actor.len() > 512
            || actor.chars().any(char::is_control)
            || key.trim().is_empty()
            || key.len() > 256
            || key.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid actor or idempotency key"));
        }
        let value = serde_json::to_value(&request)?;
        let fingerprint = digest(&serde_json::to_vec(&value)?);
        let id = Uuid::new_v4();
        let branch =
            thalamus_gitops::branch_for_change(&id.to_string()).map_err(|_| Error::GitOps)?;
        let inserted = sqlx::query("INSERT INTO thalamus_changes (id,actor,idempotency_key,fingerprint,request,state,branch) VALUES ($1,$2,$3,$4,$5,'proposed',$6) ON CONFLICT (actor,idempotency_key) DO NOTHING RETURNING id")
            .bind(id).bind(actor).bind(key).bind(&fingerprint).bind(&value).bind(branch).fetch_optional(&mut **tx).await?;
        let row = sqlx::query(
            "SELECT * FROM thalamus_changes WHERE actor=$1 AND idempotency_key=$2 FOR UPDATE",
        )
        .bind(actor)
        .bind(key)
        .fetch_one(&mut **tx)
        .await?;
        if row.try_get::<String, _>("fingerprint")? != fingerprint {
            return Err(Error::IdempotencyConflict);
        }
        let change = decode(&row)?;
        if inserted.is_some() {
            audit(tx, change.id, actor, "proposed").await?;
        }
        Ok(change)
    }
    pub async fn get(&self, id: Uuid) -> Result<Change> {
        let row = sqlx::query("SELECT * FROM thalamus_changes WHERE id=$1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        decode(&row)
    }
    /// Lock the proposal while authorizing and mutating it in the same transaction.
    pub async fn get_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
    ) -> Result<Change> {
        let row = sqlx::query("SELECT * FROM thalamus_changes WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        decode(&row)
    }
    /// Bounded listing; callers must enforce authorization before returning records.
    pub async fn list(&self, limit: i64) -> Result<Vec<Change>> {
        if !(1..=500).contains(&limit) {
            return Err(Error::Invalid("list limit must be between 1 and 500"));
        }
        sqlx::query("SELECT * FROM thalamus_changes ORDER BY created_at DESC, id DESC LIMIT $1")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(decode)
            .collect()
    }
    /// List while holding the caller's authorization transaction.
    pub async fn list_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        limit: i64,
    ) -> Result<Vec<Change>> {
        if !(1..=500).contains(&limit) {
            return Err(Error::Invalid("list limit must be between 1 and 500"));
        }
        sqlx::query("SELECT * FROM thalamus_changes ORDER BY created_at DESC, id DESC LIMIT $1")
            .bind(limit)
            .fetch_all(&mut **tx)
            .await?
            .iter()
            .map(decode)
            .collect()
    }

    /// Refresh the same durable proposal against an explicitly authorized target SHA.
    /// Refresh produces a preview, but does not constitute renewed validation or review.
    pub async fn refresh(
        &self,
        id: Uuid,
        actor: &str,
        repo: &GitOpsRepo,
        config: &PreviewConfig,
        expected_base: &str,
    ) -> Result<Change> {
        let mut tx = self.pool.begin().await?;
        let result = self
            .refresh_in_transaction(&mut tx, id, actor, repo, config, expected_base)
            .await;
        tx.commit().await?;
        result
    }
    /// The caller must commit even on a render error to persist review invalidation.
    pub async fn refresh_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
        actor: &str,
        repo: &GitOpsRepo,
        config: &PreviewConfig,
        expected_base: &str,
    ) -> Result<Change> {
        if actor.trim().is_empty() || actor.len() > 512 || actor.chars().any(char::is_control) {
            return Err(Error::Invalid("invalid actor"));
        }
        let row = sqlx::query("SELECT * FROM thalamus_changes WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        let mut change = decode(&row)?;
        if !matches!(change.state.as_str(), "proposed" | "validated" | "open") {
            return Err(Error::Invalid("change is not refreshable"));
        }
        if change.pr_number.is_some() {
            return Err(Error::Invalid("published refresh requires verifiable non-bypassable forge review and check protection"));
        }
        repo.fetch().map_err(|_| Error::GitOps)?;
        let base = repo
            .resolve_base(&change.request.target_branch)
            .map_err(|_| Error::GitOps)?;
        if base != expected_base {
            return Err(Error::Invalid("target base moved during authorization"));
        }
        if base == change.request.base_sha {
            return Err(Error::Invalid("target base has not moved"));
        }
        change.request.base_sha = base;
        let path = config
            .worktree_root
            .join(format!("{}-{}", id, Uuid::new_v4()));
        let result = (|| {
            let mut worktree = repo
                .create_worktree(&change.request.base_sha, &path)
                .map_err(|_| Error::GitOps)?;
            let result = match &change.request.revert_commit {
                Some(commit) => worktree.preview_revert(commit, &config.render),
                None => worktree.preview(&change.request.patches, &config.render),
            }
            .map_err(|_| Error::GitOps);
            repo.discard_worktree(worktree).map_err(|_| Error::GitOps)?;
            let preview = result?;
            if preview.data_diff.is_empty() {
                return Err(Error::Invalid("patch does not change catalog data"));
            }
            Ok(preview)
        })();
        let request = serde_json::to_value(&change.request)?;
        match result {
            Ok(preview) => {
                let row = sqlx::query("UPDATE thalamus_changes SET request=$2,state='proposed',base_render_sha256=$3,proposed_render_sha256=$4,data_diff=$5,rendered_diff=$6,error=NULL,updated_at=now() WHERE id=$1 RETURNING *")
                    .bind(id).bind(request).bind(digest(preview.base_render.as_bytes())).bind(digest(preview.proposed_render.as_bytes()))
                    .bind(&preview.data_diff).bind(&preview.rendered_diff).fetch_one(&mut **tx).await?;
                audit(tx, id, actor, "base_refreshed_review_required").await?;
                decode(&row)
            }
            Err(error) => {
                sqlx::query("UPDATE thalamus_changes SET request=$2,state='proposed',base_render_sha256=NULL,proposed_render_sha256=NULL,data_diff=NULL,rendered_diff=NULL,error=$3,updated_at=now() WHERE id=$1")
                    .bind(id).bind(request).bind(error.to_string()).execute(&mut **tx).await?;
                audit(tx, id, actor, "base_refresh_failed_review_required").await?;
                Err(error)
            }
        }
    }

    /// Render the exact configured CI command against base and proposal, without publishing.
    pub async fn validate(
        &self,
        id: Uuid,
        actor: &str,
        repo: &GitOpsRepo,
        config: &PreviewConfig,
    ) -> Result<Change> {
        let mut tx = self.pool.begin().await?;
        let result = self
            .validate_in_transaction(&mut tx, id, actor, repo, config)
            .await;
        tx.commit().await?;
        result
    }
    /// Commit even on a render error to preserve invalidation and its audit record.
    pub async fn validate_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
        actor: &str,
        repo: &GitOpsRepo,
        config: &PreviewConfig,
    ) -> Result<Change> {
        if actor.trim().is_empty() || actor.len() > 512 || actor.chars().any(char::is_control) {
            return Err(Error::Invalid("invalid actor"));
        }
        let row = sqlx::query("SELECT * FROM thalamus_changes WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        let change = decode(&row)?;
        if !matches!(change.state.as_str(), "proposed" | "validated") {
            return Err(Error::Invalid("change is not awaiting validation"));
        }
        let result = (|| {
            repo.fetch().map_err(|_| Error::GitOps)?;
            if repo
                .resolve_base(&change.request.target_branch)
                .map_err(|_| Error::GitOps)?
                != change.request.base_sha
            {
                return Err(Error::Invalid("target base moved; refresh the proposal"));
            }
            let path = config
                .worktree_root
                .join(format!("{}-{}", id, Uuid::new_v4()));
            let mut worktree = repo
                .create_worktree(&change.request.base_sha, &path)
                .map_err(|_| Error::GitOps)?;
            let result = match &change.request.revert_commit {
                Some(commit) => worktree.preview_revert(commit, &config.render),
                None => worktree.preview(&change.request.patches, &config.render),
            }
            .map_err(|_| Error::GitOps);
            repo.discard_worktree(worktree).map_err(|_| Error::GitOps)?;
            let preview = result?;
            if preview.data_diff.is_empty() {
                return Err(Error::Invalid("patch does not change catalog data"));
            }
            Ok(preview)
        })();
        let preview = match result {
            Ok(preview) => preview,
            Err(error) => {
                sqlx::query("UPDATE thalamus_changes SET state='proposed',base_render_sha256=NULL,proposed_render_sha256=NULL,data_diff=NULL,rendered_diff=NULL,error=$2,updated_at=now() WHERE id=$1")
                    .bind(id).bind(error.to_string()).execute(&mut **tx).await?;
                audit(tx, id, actor, "validation_failed").await?;
                return Err(error);
            }
        };
        let row = sqlx::query("UPDATE thalamus_changes SET state='validated',base_render_sha256=$2,proposed_render_sha256=$3,data_diff=$4,rendered_diff=$5,error=NULL,updated_at=now() WHERE id=$1 RETURNING *")
            .bind(id).bind(digest(preview.base_render.as_bytes())).bind(digest(preview.proposed_render.as_bytes()))
            .bind(&preview.data_diff).bind(&preview.rendered_diff).fetch_one(&mut **tx).await?;
        audit(tx, id, actor, "validated").await?;
        decode(&row)
    }

    /// Serializes publication per change using a database row lock. A retry renders
    /// and pushes the same deterministic commit, then discovers an existing PR.
    /// Each attempt uses a new worktree; a process crash can leave it behind.
    pub async fn publish<F: ChangeRequests + Comments + BranchPolicies>(
        &self,
        id: Uuid,
        actor: &str,
        repo: &GitOpsRepo,
        forge: &F,
        config: &WorkerConfig,
    ) -> Result<Change> {
        let mut tx = self.pool.begin().await?;
        let change = self
            .publish_in_transaction(&mut tx, id, actor, repo, forge, config)
            .await?;
        tx.commit().await?;
        self.comment_rendered_diff(id, forge, &config.forge_actor)
            .await?;
        Ok(change)
    }
    /// Hold the caller's authorization lock through all publication side effects.
    /// Post the rendered diff only after committing this transaction.
    pub async fn publish_in_transaction<F: ChangeRequests + Comments + BranchPolicies>(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
        actor: &str,
        repo: &GitOpsRepo,
        forge: &F,
        config: &WorkerConfig,
    ) -> Result<Change> {
        if actor.trim().is_empty() || actor.len() > 512 || actor.chars().any(char::is_control) {
            return Err(Error::Invalid("invalid actor"));
        }
        let row = sqlx::query("SELECT * FROM thalamus_changes WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        let change = decode(&row)?;
        if change.pr_number.is_some() && change.state == "open" {
            return Ok(change);
        }
        if change.state != "validated" {
            return Err(Error::Invalid(
                "change must be validated before publication",
            ));
        }
        if change.pr_number.is_some() {
            return Err(Error::Invalid("published update requires verifiable non-bypassable forge review and check protection"));
        }
        if thalamus_gitops::branch_for_change(&id.to_string()).map_err(|_| Error::GitOps)?
            != change.branch
        {
            return Err(Error::Invalid("branch identity mismatch"));
        }
        repo.fetch().map_err(|_| Error::GitOps)?;
        if repo
            .resolve_base(&change.request.target_branch)
            .map_err(|_| Error::GitOps)?
            != change.request.base_sha
        {
            return Err(Error::Invalid("target base moved; refresh the proposal"));
        }
        if let Some(number) = change.pr_number {
            // Forge protection must invalidate old approvals when the head changes.
            // This is only a point-in-time check; operators must prevent policy bypass.
            let policy = forge
                .branch_policy(&change.request.target_branch)
                .await
                .map_err(|_| Error::Forge)?;
            let reviews = &policy.protection["required_pull_request_reviews"];
            let checks = &policy.protection["required_status_checks"];
            if policy.branch != change.request.target_branch
                || reviews["dismiss_stale_reviews"].as_bool() != Some(true)
                || reviews["required_approving_review_count"]
                    .as_u64()
                    .unwrap_or(0)
                    < 1
                || policy.protection["enforce_admins"]["enabled"].as_bool() != Some(true)
                || checks["strict"].as_bool() != Some(true)
                || !(checks["checks"].as_array().is_some_and(|v| !v.is_empty())
                    || checks["contexts"].as_array().is_some_and(|v| !v.is_empty()))
            {
                return Err(Error::Invalid("target must require up-to-date checks and renewed reviews, and dismiss stale approvals"));
            }
            let number = u64::try_from(number).map_err(|_| Error::Invalid("invalid PR number"))?;
            let pr = forge.get_change(number).await.map_err(|_| Error::Forge)?;
            if pr.state != ChangeState::Open
                || pr.source_branch != change.branch
                || pr.source_repository.as_ref() != Some(&config.repository)
                || pr.target_branch != change.request.target_branch
                || pr.base_sha != change.request.base_sha
            {
                return Err(Error::Invalid("existing forge change is closed, retargeted, or differs from the expected repository and base"));
            }
            if change.head_sha.is_none() {
                return Err(Error::Invalid("published change has no recorded head"));
            }
        }
        let path = config
            .worktree_root
            .join(format!("{}-{}", id, Uuid::new_v4()));
        let mut worktree = repo
            .create_worktree(&change.request.base_sha, &path)
            .map_err(|_| Error::GitOps)?;
        let prepared = (|| {
            let preview = match &change.request.revert_commit {
                Some(commit) => worktree.preview_revert(commit, &config.render),
                None => worktree.preview(&change.request.patches, &config.render),
            }
            .map_err(|_| Error::GitOps)?;
            if preview.data_diff.is_empty() {
                return Err(Error::Invalid("patch does not change catalog data"));
            }
            if !preview_matches(&change, &preview) {
                return Err(Error::Invalid(
                    "render differs from validated preview; revalidate",
                ));
            }
            let published = match change.head_sha.as_deref() {
                Some(old_head) if change.pr_number.is_some() => worktree.commit_and_push_update(
                    &id.to_string(),
                    &change.request.title,
                    &config.commit_author,
                    old_head,
                ),
                None if change.pr_number.is_none() => worktree.commit_and_push(
                    &id.to_string(),
                    &change.request.title,
                    &config.commit_author,
                ),
                _ => return Err(Error::Invalid("inconsistent published change")),
            }
            .map_err(|_| Error::GitOps)?;
            if published.branch != change.branch {
                return Err(Error::Invalid("branch identity mismatch"));
            }
            Ok((preview, published))
        })();
        repo.discard_worktree(worktree).map_err(|_| Error::GitOps)?;
        let (preview, published) = prepared?;
        let base_hash = digest(preview.base_render.as_bytes());
        let proposed_hash = digest(preview.proposed_render.as_bytes());
        // A crash after push or PR creation is recovered by enumerating existing PRs.
        let matches: Vec<ChangeRequest> = forge
            .list_changes()
            .try_filter(|pr| {
                futures_util::future::ready(
                    pr.source_branch == change.branch
                        && pr.source_repository.as_ref() == Some(&config.repository),
                )
            })
            .try_collect()
            .await
            .map_err(|_| Error::Forge)?;
        let pr = match matches.as_slice() {
            [] if change.pr_number.is_none() => {
                let body = format!("Requested by: {}\nValidation tier: {}\nRollout: {:?}\n\nRendered diff SHA-256: {}\n\nThe rendered diff is advisory; CI is authoritative.", change.actor, change.request.validation_tier, change.request.rollout, digest(preview.rendered_diff.as_bytes()));
                forge
                    .create_change(&CreateChangeRequest {
                        title: change.request.title.clone(),
                        body: Some(body),
                        source_branch: change.branch.clone(),
                        target_branch: change.request.target_branch.clone(),
                        draft: false,
                    })
                    .await
                    .map_err(|_| Error::Forge)?
            }
            [pr] => pr.clone(),
            _ => {
                return Err(Error::Invalid(
                    "missing or multiple matching change requests",
                ))
            }
        };
        if pr.state != ChangeState::Open
            || pr.head_sha != published.commit
            || pr.base_sha != change.request.base_sha
            || pr.target_branch != change.request.target_branch
            || pr.source_branch != change.branch
            || pr.source_repository.as_ref() != Some(&config.repository)
        {
            return Err(Error::Invalid("existing forge change is closed, retargeted, or differs from published repository, revision and base"));
        }
        let number = i64::try_from(pr.number).map_err(|_| Error::Invalid("invalid PR number"))?;
        if change.pr_number.is_some() && change.pr_number != Some(number) {
            return Err(Error::Invalid("existing forge change number differs"));
        }
        let row = sqlx::query("UPDATE thalamus_changes SET state='open',head_sha=$2,pr_number=$3,pr_url=$4,base_render_sha256=$5,proposed_render_sha256=$6,data_diff=$7,rendered_diff=$8,error=NULL,updated_at=now() WHERE id=$1 RETURNING *")
            .bind(id).bind(&published.commit).bind(number).bind(&pr.url).bind(base_hash).bind(proposed_hash).bind(&preview.data_diff).bind(&preview.rendered_diff).fetch_one(&mut **tx).await?;
        audit(
            tx,
            id,
            actor,
            if change.pr_number.is_some() {
                "republished_review_required"
            } else {
                "published"
            },
        )
        .await?;
        decode(&row)
    }
    /// Post the persisted preview. The configured bot login prevents another
    /// author from spoofing its recovery marker.
    pub async fn comment_rendered_diff<F: Comments>(
        &self,
        id: Uuid,
        forge: &F,
        forge_actor: &str,
    ) -> Result<()> {
        if forge_actor.is_empty() {
            return Err(Error::Invalid("forge actor is required"));
        }
        let change = self.get(id).await?;
        let number = u64::try_from(change.pr_number.ok_or(Error::Invalid("change has no PR"))?)
            .map_err(|_| Error::Invalid("invalid PR number"))?;
        let diff = change
            .rendered_diff
            .as_deref()
            .ok_or(Error::Invalid("change has no rendered diff"))?;
        let marker = format!(
            "<!-- thalamus-render:{}:{} -->",
            id,
            digest(diff.as_bytes())
        );
        let comments: Vec<forge::Comment> = forge
            .list_comments(number)
            .try_collect()
            .await
            .map_err(|_| Error::Forge)?;
        if comments
            .iter()
            .any(|c| c.author.eq_ignore_ascii_case(forge_actor) && c.body.contains(&marker))
        {
            return Ok(());
        }
        let excerpt: String = diff.chars().take(10_000).collect();
        let suffix = if excerpt.len() < diff.len() {
            "\n[Diff truncated; full diff is stored in Thalamus.]"
        } else {
            ""
        };
        let escaped = excerpt
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let body = format!(
            "{marker}\nAdvisory rendered diff; CI is authoritative.\n<pre>{escaped}</pre>{suffix}"
        );
        forge
            .create_comment(number, &body)
            .await
            .map_err(|_| Error::Forge)?;
        Ok(())
    }

    /// Compare two explicit commits in separate worktrees using the CI command.
    /// The caller is responsible for authorizing access to the repository data.
    pub fn compare_revisions(
        repo: &GitOpsRepo,
        config: &PreviewConfig,
        base: &str,
        head: &str,
    ) -> Result<thalamus_gitops::Preview> {
        repo.fetch().map_err(|_| Error::GitOps)?;
        let base_path = config
            .worktree_root
            .join(format!("compare-base-{}", Uuid::new_v4()));
        let head_path = config
            .worktree_root
            .join(format!("compare-head-{}", Uuid::new_v4()));
        repo.preview_refs(base, head, &base_path, &head_path, &config.render)
            .map_err(|_| Error::GitOps)
    }

    /// Render and comment on a hand-written PR as well as a Thalamus PR.
    /// Only commits present in the configured bare repository can be rendered.
    pub async fn comment_pr_preview<F: ChangeRequests + Comments>(
        &self,
        repo: &GitOpsRepo,
        forge: &F,
        config: &PreviewConfig,
        forge_actor: &str,
        number: u64,
    ) -> Result<()> {
        if forge_actor.is_empty() {
            return Err(Error::Invalid("forge actor is required"));
        }
        let before = forge.get_change(number).await.map_err(|_| Error::Forge)?;
        if before.state != ChangeState::Open {
            return Err(Error::Invalid("PR is not open"));
        }
        let preview = Self::compare_revisions(repo, config, &before.base_sha, &before.head_sha)?;
        let after = forge.get_change(number).await.map_err(|_| Error::Forge)?;
        if after.state != ChangeState::Open
            || before.base_sha != after.base_sha
            || before.head_sha != after.head_sha
            || before.target_branch != after.target_branch
        {
            return Err(Error::Invalid("PR moved while rendering"));
        }
        let marker = format!(
            "<!-- thalamus-pr-render:{}:{}:{} -->",
            number, before.base_sha, before.head_sha
        );
        let comments: Vec<forge::Comment> = forge
            .list_comments(number)
            .try_collect()
            .await
            .map_err(|_| Error::Forge)?;
        if comments
            .iter()
            .any(|c| c.author.eq_ignore_ascii_case(forge_actor) && c.body.contains(&marker))
        {
            return Ok(());
        }
        let excerpt: String = preview.rendered_diff.chars().take(10_000).collect();
        let suffix = if excerpt.len() < preview.rendered_diff.len() {
            "\n[Diff truncated.]"
        } else {
            ""
        };
        let escaped = excerpt
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let body = format!(
            "{marker}\nAdvisory rendered diff; CI is authoritative.\n<pre>{escaped}</pre>{suffix}"
        );
        forge
            .create_comment(number, &body)
            .await
            .map_err(|_| Error::Forge)?;
        Ok(())
    }

    /// Polling recovers missed webhooks. A webhook handler can invoke this by PR number.
    pub async fn follow<F: ChangeRequests>(&self, id: Uuid, forge: &F) -> Result<Change> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT * FROM thalamus_changes WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        let change = decode(&row)?;
        let number = u64::try_from(change.pr_number.ok_or(Error::Invalid("change has no PR"))?)
            .map_err(|_| Error::Invalid("invalid PR number"))?;
        let pr = forge.get_change(number).await.map_err(|_| Error::Forge)?;
        if pr.source_branch != change.branch || pr.target_branch != change.request.target_branch {
            return Err(Error::Invalid("PR identity changed"));
        }
        let state = match pr.state {
            ChangeState::Open if matches!(change.state.as_str(), "proposed" | "validated") => {
                change.state.as_str()
            }
            ChangeState::Open => "open",
            ChangeState::Merged => "merged",
            ChangeState::Closed => "closed",
        };
        if change.head_sha.as_deref() != Some(pr.head_sha.as_str()) {
            return Err(Error::Invalid("PR head changed; stored render is stale"));
        }
        sqlx::query("UPDATE thalamus_changes SET state=$2,updated_at=now() WHERE id=$1")
            .bind(id)
            .bind(state)
            .execute(&mut *tx)
            .await?;
        if state != change.state {
            audit(&mut tx, id, &change.actor, state).await?;
        }
        tx.commit().await?;
        self.get(id).await
    }
    /// Verify a raw forge webhook before looking up records. The caller must
    /// supply the configured repository and secret, not values from the request.
    /// Periodic `follow` calls are still required for missed notifications.
    pub async fn handle_webhook<F: ChangeRequests + forge::Webhooks>(
        &self,
        forge: &F,
        repository: &forge::RepoRef,
        event: &str,
        signature: &str,
        secret: &[u8],
        body: &[u8],
    ) -> Result<Vec<Change>> {
        let event = forge
            .webhook(event, signature, secret, body)
            .map_err(|_| Error::Forge)?;
        let ids: Vec<Uuid> = match event {
            Some(forge::WebhookEvent::ChangeRequest {
                repository: r,
                number,
                ..
            }) if &r == repository => self
                .by_pr_number(number)
                .await?
                .into_iter()
                .map(|c| c.id)
                .collect(),
            Some(forge::WebhookEvent::CheckUpdated { repository: r, sha }) if &r == repository => {
                sqlx::query_scalar(
                    "SELECT id FROM thalamus_changes WHERE head_sha=$1 AND state='open'",
                )
                .bind(sha)
                .fetch_all(&self.pool)
                .await?
            }
            Some(forge::WebhookEvent::Push { repository: r, .. }) if &r == repository => {
                sqlx::query_scalar("SELECT id FROM thalamus_changes WHERE state='open'")
                    .fetch_all(&self.pool)
                    .await?
            }
            _ => Vec::new(),
        };
        let mut changes = Vec::with_capacity(ids.len());
        for id in ids {
            changes.push(self.follow(id, forge).await?);
        }
        Ok(changes)
    }
    pub async fn by_pr_number(&self, number: u64) -> Result<Vec<Change>> {
        let number = i64::try_from(number).map_err(|_| Error::Invalid("invalid PR number"))?;
        sqlx::query("SELECT * FROM thalamus_changes WHERE pr_number=$1")
            .bind(number)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(decode)
            .collect()
    }
}
async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    actor: &str,
    action: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO thalamus_change_audit(change_id,actor,action) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(actor)
        .bind(action)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Operator-controlled worktree location and CI-equivalent render command.
#[derive(Clone)]
pub struct PreviewConfig {
    pub worktree_root: PathBuf,
    pub render: RenderCommand,
}

/// All fields are operator-controlled configuration, never user-controlled input.
#[derive(Clone)]
pub struct WorkerConfig {
    pub worktree_root: PathBuf,
    pub render: RenderCommand,
    pub commit_author: CommitAuthor,
    pub repository: forge::RepoRef,
    pub forge_actor: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use thalamus_gitops::Operation;
    fn proposal() -> Proposal {
        Proposal {
            service: "a".into(),
            base_sha: "a".repeat(40),
            target_branch: "main".into(),
            title: "Change".into(),
            patches: vec![Patch {
                file: "catalog/services/a.yaml".into(),
                path: vec![],
                operation: Operation::Remove,
            }],
            revert_commit: None,
            validation_tier: "expected".into(),
            rollout: Rollout::Rolling,
        }
    }
    #[test]
    fn validates_commit_and_window() {
        let mut p = proposal();
        assert!(p.validate().is_ok());
        p.base_sha = "main".into();
        assert!(p.validate().is_err());
        p.base_sha = "a".repeat(40);
        let now = Utc::now();
        p.rollout = Rollout::InPlace {
            window_start: now,
            window_end: now,
        };
        assert!(p.validate().is_err());
    }
    #[test]
    fn revert_is_exclusive_and_pinned() {
        let mut p = proposal();
        p.revert_commit = Some("b".repeat(40));
        assert!(p.validate().is_err());
        p.patches.clear();
        assert!(p.validate().is_ok());
        p.revert_commit = Some("main".into());
        assert!(p.validate().is_err());
    }
    #[test]
    fn fingerprint_depends_on_patch() {
        let a = proposal();
        let mut b = proposal();
        b.patches[0].path.push("image".into());
        assert_ne!(
            digest(&serde_json::to_vec(&a).unwrap()),
            digest(&serde_json::to_vec(&b).unwrap())
        );
    }
    #[test]
    fn publication_rejects_any_changed_preview() {
        let mut preview = thalamus_gitops::Preview {
            base_render: "base".into(),
            proposed_render: "new".into(),
            data_diff: "data".into(),
            rendered_diff: "diff".into(),
        };
        let change = Change {
            id: Uuid::new_v4(),
            actor: "user".into(),
            idempotency_key: "key".into(),
            request: proposal(),
            state: "validated".into(),
            branch: "branch".into(),
            head_sha: None,
            pr_number: None,
            pr_url: None,
            base_render_sha256: Some(digest(b"base")),
            proposed_render_sha256: Some(digest(b"new")),
            data_diff: Some("data".into()),
            rendered_diff: Some("diff".into()),
            error: None,
        };
        assert!(preview_matches(&change, &preview));
        preview.base_render = "other".into();
        assert!(!preview_matches(&change, &preview));
        preview.base_render = "base".into();
        preview.proposed_render = "other".into();
        assert!(!preview_matches(&change, &preview));
        preview.proposed_render = "new".into();
        preview.data_diff = "other".into();
        assert!(!preview_matches(&change, &preview));
        preview.data_diff = "data".into();
        preview.rendered_diff = "other".into();
        assert!(!preview_matches(&change, &preview));
    }
}
