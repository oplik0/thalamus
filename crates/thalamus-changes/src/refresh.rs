//! Refreshing a published PR is a two-step reviewed operation. No head, base,
//! policy or authorization read is an atomic guarantee about a later forge merge.
use super::*;

fn request(
    policy: &RolloutPolicy,
    number: u64,
    head: &str,
    base: &str,
) -> forge::GuardedQueueRequest {
    forge::GuardedQueueRequest {
        repository: policy.repository.clone(),
        number,
        target_branch: policy.target_branch.clone(),
        head_sha: head.to_owned(),
        base_sha: base.to_owned(),
        required_checks: policy
            .required_checks
            .iter()
            .map(|c| forge::RequiredCheck {
                context: c.context.clone(),
                app_id: c.app_id,
                status_creator_id: c.status_creator_id,
            })
            .collect(),
        required_approvers: policy.required_approvers.clone(),
        trust_administrators: policy.trust_administrators,
        trust_merge_group_ci: policy.trust_merge_group_ci,
    }
}

impl Store {
    /// Refresh an open PR's preview against an explicitly authorized current
    /// target. The caller must commit even on a render error to persist approval
    /// invalidation. This method does not push or claim a renewed validation.
    pub async fn refresh_published_in_transaction<
        F: ChangeRequests + BranchPolicies + forge::GuardedQueue,
    >(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
        actor: &str,
        publication: &PublicationContext<'_, F>,
        policy: &RolloutPolicy,
        expected_base: &str,
    ) -> Result<Change> {
        let repo = publication.repo;
        let forge = publication.forge;
        let config = publication.config;
        if actor.trim().is_empty() || actor.len() > 512 || actor.chars().any(char::is_control) {
            return Err(Error::Invalid("invalid actor"));
        }
        let mut change = self.get_in_transaction(tx, id).await?;
        policy.check(&change)?;
        if config.repository != policy.repository || change.state != "open" {
            return Err(Error::Invalid(
                "refresh requires an open PR on the configured repository",
            ));
        }
        let queued: bool = sqlx::query_scalar(
            "SELECT queued FROM thalamus_rollouts WHERE change_id=$1 FOR UPDATE",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        if queued {
            return Err(Error::Invalid("dequeue before refreshing"));
        }
        let number = u64::try_from(
            change
                .pr_number
                .ok_or(Error::Invalid("published PR required"))?,
        )
        .map_err(|_| Error::Invalid("invalid PR number"))?;
        let head = change
            .head_sha
            .as_deref()
            .ok_or(Error::Invalid("published head required"))?;
        let pr = forge.get_change(number).await.map_err(|_| Error::Forge)?;
        if pr.state != ChangeState::Open
            || pr.source_repository.as_ref() != Some(&policy.repository)
            || pr.source_branch != change.branch
            || pr.target_branch != policy.target_branch
            || pr.head_sha != head
            || pr.base_sha != expected_base
        {
            return Err(Error::Conflict(
                "published PR identity, head, or target changed",
            ));
        }
        repo.fetch().map_err(|_| Error::GitOps)?;
        if repo
            .resolve_base(&policy.target_branch)
            .map_err(|_| Error::GitOps)?
            != expected_base
            || repo.fetch_pull_head(number).map_err(|_| Error::GitOps)? != head
        {
            return Err(Error::Conflict("published PR refs changed"));
        }
        if change.request.base_sha == expected_base {
            return Err(Error::Invalid("target base has not moved"));
        }
        forge
            .verify_update_policy(&request(policy, number, head, expected_base))
            .await
            .map_err(|_| Error::Forge)?;
        let preview_config = PreviewConfig {
            worktree_root: config.worktree_root.clone(),
            render: config.render.clone(),
        };
        stages::ensure_refresh_safe(&change, repo, &preview_config, expected_base)?;
        change.request.base_sha = expected_base.to_owned();
        let value = serde_json::to_value(&change.request)?;
        sqlx::query("DELETE FROM thalamus_rollout_approvals WHERE change_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE thalamus_rollouts SET filler_verified_at=NULL WHERE change_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE thalamus_changes SET request=$2,state='proposed',base_render_sha256=NULL,proposed_render_sha256=NULL,data_diff=NULL,rendered_diff=NULL,error=NULL,updated_at=now() WHERE id=$1")
            .bind(id).bind(&value).execute(&mut **tx).await?;
        audit(tx, id, actor, "published_base_refreshed_review_required").await?;
        let path = config
            .worktree_root
            .join(format!("refresh-published-{id}-{}", Uuid::new_v4()));
        let result = (|| {
            let mut tree = repo
                .create_worktree(expected_base, &path)
                .map_err(|_| Error::GitOps)?;
            let result = stages::check_artifact_creation(tree.path(), &change)
                .and_then(|_| match &change.request.revert_commit {
                    Some(commit) => tree
                        .preview_revert(commit, &config.render)
                        .map_err(|_| Error::GitOps),
                    None => tree
                        .preview(&change.request.patches, &config.render)
                        .map_err(|_| Error::GitOps),
                })
                .and_then(|preview| {
                    stages::check_preview_names(tree.path(), &change)?;
                    Ok(preview)
                });
            let cleanup = repo.discard_worktree(tree).map_err(|_| Error::GitOps);
            let preview = match (result, cleanup) {
                (Ok(v), Ok(())) => v,
                (Err(e), _) | (Ok(_), Err(e)) => return Err(e),
            };
            if preview.data_diff.is_empty() {
                return Err(Error::Invalid("patch does not change catalog data"));
            }
            Ok(preview)
        })();
        match result {
            Ok(preview) => {
                let row = sqlx::query("UPDATE thalamus_changes SET base_render_sha256=$2,proposed_render_sha256=$3,data_diff=$4,rendered_diff=$5,error=NULL,updated_at=now() WHERE id=$1 RETURNING *")
                    .bind(id).bind(digest(preview.base_render.as_bytes())).bind(digest(preview.proposed_render.as_bytes()))
                    .bind(&preview.data_diff).bind(&preview.rendered_diff).fetch_one(&mut **tx).await?;
                decode(&row)
            }
            Err(error) => {
                sqlx::query("UPDATE thalamus_changes SET error=$2,updated_at=now() WHERE id=$1")
                    .bind(id)
                    .bind(error.to_string())
                    .execute(&mut **tx)
                    .await?;
                audit(tx, id, actor, "published_refresh_failed_review_required").await?;
                Err(error)
            }
        }
    }

    /// Publish a revalidated refresh, using the previously recorded head as
    /// Git's server-side lease. A retry accepts only the identical deterministic
    /// commit if the first push succeeded before the database commit.
    pub async fn publish_refreshed_in_transaction<
        F: ChangeRequests + Comments + BranchPolicies + forge::GuardedQueue,
    >(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
        actor: &str,
        publication: &PublicationContext<'_, F>,
        policy: &RolloutPolicy,
    ) -> Result<Change> {
        let repo = publication.repo;
        let forge = publication.forge;
        let config = publication.config;
        let change = self.get_in_transaction(tx, id).await?;
        policy.check(&change)?;
        if config.repository != policy.repository
            || change.state != "validated"
            || change.head_sha.is_none()
        {
            return Err(Error::Invalid("revalidated published change required"));
        }
        let queued: bool = sqlx::query_scalar(
            "SELECT queued FROM thalamus_rollouts WHERE change_id=$1 FOR UPDATE",
        )
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        if queued {
            return Err(Error::Invalid("dequeue before updating"));
        }
        let number = u64::try_from(
            change
                .pr_number
                .ok_or(Error::Invalid("published PR required"))?,
        )
        .map_err(|_| Error::Invalid("invalid PR number"))?;
        let pr = forge.get_change(number).await.map_err(|_| Error::Forge)?;
        if pr.state != ChangeState::Open
            || pr.source_repository.as_ref() != Some(&policy.repository)
            || pr.source_branch != change.branch
            || pr.target_branch != policy.target_branch
            || pr.base_sha != change.request.base_sha
        {
            return Err(Error::Conflict("published PR identity or target changed"));
        }
        repo.fetch().map_err(|_| Error::GitOps)?;
        if repo
            .resolve_base(&policy.target_branch)
            .map_err(|_| Error::GitOps)?
            != pr.base_sha
            || repo.fetch_pull_head(number).map_err(|_| Error::GitOps)? != pr.head_sha
        {
            return Err(Error::Conflict("published PR refs changed"));
        }
        forge
            .verify_update_policy(&request(policy, number, &pr.head_sha, &pr.base_sha))
            .await
            .map_err(|_| Error::Forge)?;
        sqlx::query("DELETE FROM thalamus_rollout_approvals WHERE change_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE thalamus_rollouts SET filler_verified_at=NULL WHERE change_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        self.publish_impl(tx, id, actor, publication, true).await
    }
}
