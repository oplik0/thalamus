//! Retryable control-plane-only reconciliation; Git remains the serving writer.
use super::*;
use crate::rollout::{
    RolloutMode, RolloutPolicy, RolloutStatus, ValidationGate, ValidationProgress,
    ValidationRequirement, ValidationStage,
};
use chrono::Duration;

impl Store {
    /// Sweep a configured service; child PRs are discovered through their parent.
    /// Each preview renders the full catalog from its pinned revision.
    pub async fn reconcile_service<F, G>(
        &self,
        service: &str,
        repo: &GitOpsRepo,
        forge: &F,
        config: &WorkerConfig,
        policy: &RolloutPolicy,
        gate: &G,
    ) -> Result<()>
    where
        F: ChangeRequests + Comments + BranchPolicies + forge::GuardedQueue,
        G: ValidationGate,
    {
        if service != policy.service || config.repository != policy.repository {
            self.withdraw_queued(service, forge, &config.repository)
                .await?;
            return Err(Error::Invalid(
                "worker and service policy repository differ",
            ));
        }
        if let Err(error) = policy.validate() {
            self.withdraw_queued(service, forge, &config.repository)
                .await?;
            return Err(error);
        }
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT c.id FROM thalamus_changes c JOIN thalamus_rollouts r ON r.change_id=c.id WHERE c.request->>'service'=$1 AND c.request->>'target_branch'=$2 AND r.stage NOT IN ('completed','failed','closed') AND NOT EXISTS (SELECT 1 FROM thalamus_rollout_stages s WHERE s.stage_change_id=c.id AND s.change_id<>c.id) ORDER BY c.created_at,c.id")
            .bind(service).bind(&policy.target_branch).fetch_all(&self.pool).await?;
        let mut first_error = None;
        for id in ids {
            match self.reconcile(id, repo, forge, config, policy, gate).await {
                Ok(()) => {}
                Err(error) => {
                    sqlx::query(
                        "UPDATE thalamus_rollouts SET error=$2,updated_at=now() WHERE change_id=$1",
                    )
                    .bind(id)
                    .bind(error.to_string())
                    .execute(&self.pool)
                    .await?;
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn withdraw_queued<F: forge::GuardedQueue>(
        &self,
        service: &str,
        forge: &F,
        repository: &forge::RepoRef,
    ) -> Result<()> {
        let rows = sqlx::query("SELECT c.id,c.pr_number FROM thalamus_changes c JOIN thalamus_rollouts r ON r.change_id=c.id WHERE r.queued AND c.request->>'service'=$1 AND c.pr_number IS NOT NULL")
            .bind(service).fetch_all(&self.pool).await?;
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let number = u64::try_from(row.try_get::<i64, _>("pr_number")?)
                .map_err(|_| Error::Invalid("invalid PR number"))?;
            forge
                .dequeue(repository, number)
                .await
                .map_err(|_| Error::Forge)?;
            sqlx::query("UPDATE thalamus_rollouts SET queued=false,queue_confirmed=false,filler_verified_at=NULL,updated_at=now() WHERE change_id=$1")
                .bind(id).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Perform the same real reconciliation as a worker tick. It does not bypass
    /// any approval, validation, review, target, or upstream policy gate.
    pub async fn reconcile<F, G>(
        &self,
        id: Uuid,
        repo: &GitOpsRepo,
        forge: &F,
        config: &WorkerConfig,
        policy: &RolloutPolicy,
        gate: &G,
    ) -> Result<()>
    where
        F: ChangeRequests + Comments + BranchPolicies + forge::GuardedQueue,
        G: ValidationGate,
    {
        let mut lock = self.pool.begin().await?;
        let acquired: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,0))")
                .bind(format!("thalamus-rollout:{id}"))
                .fetch_one(&mut *lock)
                .await?;
        if !acquired {
            return Ok(());
        }
        let result = self
            .reconcile_locked(id, repo, forge, config, policy, gate)
            .await;
        if result.is_err() && config.repository == policy.repository {
            let status = self.rollout(id).await?;
            if status.queued {
                let change = self.get(id).await?;
                if let Some(number) = change.pr_number.and_then(|n| u64::try_from(n).ok()) {
                    if forge.dequeue(&policy.repository, number).await.is_ok() {
                        sqlx::query("UPDATE thalamus_rollouts SET queued=false,queue_confirmed=false,updated_at=now() WHERE change_id=$1")
                            .bind(id).execute(&self.pool).await?;
                    }
                }
            }
        }
        lock.commit().await?;
        result
    }

    async fn reconcile_locked<F, G>(
        &self,
        id: Uuid,
        repo: &GitOpsRepo,
        forge: &F,
        config: &WorkerConfig,
        policy: &RolloutPolicy,
        gate: &G,
    ) -> Result<()>
    where
        F: ChangeRequests + Comments + BranchPolicies + forge::GuardedQueue,
        G: ValidationGate,
    {
        let change = self.get(id).await?;
        // A fresh proof is published below even if forge checks are still
        // pending; clearing it on that expected error would deadlock CI.
        sqlx::query("UPDATE thalamus_rollouts SET filler_verified_at=NULL WHERE change_id=$1 AND in_place_pool AND stage='scheduled_pr'")
            .bind(id).execute(&self.pool).await?;
        if config.repository != policy.repository {
            return Err(Error::Invalid("worker repository does not match policy"));
        }
        policy.check(&change)?;
        let status = self.rollout(id).await?;
        let changed = sqlx::query("UPDATE thalamus_rollouts SET owner_team=$2,policy_digest=$3 WHERE change_id=$1 AND ((owner_team IS NULL AND policy_digest IS NULL) OR (owner_team=$2 AND policy_digest=$3) OR (stage='scheduled_pr' AND NOT queued))")
            .bind(id).bind(policy.owner_team).bind(policy.digest()?).execute(&self.pool).await?.rows_affected();
        if changed != 1 {
            if status.queued {
                if let Some(number) = change.pr_number.and_then(|n| u64::try_from(n).ok()) {
                    forge
                        .dequeue(&policy.repository, number)
                        .await
                        .map_err(|_| Error::Forge)?;
                    sqlx::query("UPDATE thalamus_rollouts SET queued=false,queue_confirmed=false WHERE change_id=$1")
                        .bind(id).execute(&self.pool).await?;
                }
            }
            return Err(Error::Conflict(
                "rollout policy changed; renewed review is required",
            ));
        }
        repo.fetch().map_err(|_| Error::GitOps)?;
        let current_base = repo
            .resolve_base(&policy.target_branch)
            .map_err(|_| Error::GitOps)?;
        let stage_change = match status.stages.iter().find(|s| s.stage == status.stage) {
            Some(stage) => Some(self.get(stage.change_id).await?),
            None => None,
        };
        let render_base = stage_change
            .as_ref()
            .map(|c| c.request.base_sha.as_str())
            .unwrap_or(&current_base);
        if let Some(stage_change) = &stage_change {
            if !stage_change.request.patches.is_empty()
                && repo
                    .service_file(render_base, &policy.service)
                    .map_err(|_| Error::GitOps)?
                    != stages::service_file(stage_change)?
            {
                return Err(Error::Conflict("service catalog path changed"));
            }
        }
        let scoped = WorkerConfig {
            worktree_root: config.worktree_root.clone(),
            render: config.render.clone(),
            commit_author: config.commit_author.clone(),
            repository: config.repository.clone(),
            forge_actor: config.forge_actor.clone(),
        };
        match status.stage.as_str() {
            "candidate_pr" | "promotion_pr" | "cleanup_pr" | "change_pr" | "scheduled_pr" => {
                let stage = status.stages.iter().find(|s| s.stage == status.stage);
                if stage.is_none() {
                    if matches!(status.stage.as_str(), "promotion_pr" | "cleanup_pr") {
                        return self.create_followup(&change, &status, repo, &scoped).await;
                    }
                    return Err(Error::Invalid("rollout stage is missing"));
                }
                let stage = stage.expect("checked above");
                let mut current = self.get(stage.change_id).await?;
                if stage.change_id != id && current.state == "proposed" {
                    let preview = PreviewConfig {
                        worktree_root: scoped.worktree_root.clone(),
                        render: scoped.render.clone(),
                    };
                    current = self
                        .validate(current.id, &change.actor, repo, &preview)
                        .await?;
                }
                if stage.change_id != id && current.state == "validated" {
                    current = self
                        .publish(current.id, &change.actor, repo, forge, &scoped)
                        .await?;
                }
                if current.pr_number.is_none() {
                    return Ok(());
                }
                current = self
                    .follow(current.id, forge, &policy.repository, &policy.target_branch)
                    .await?;
                if current.state == "closed" {
                    sqlx::query("UPDATE thalamus_rollouts SET stage='closed',queued=false,error='change request closed',updated_at=now() WHERE change_id=$1")
                        .bind(id).execute(&self.pool).await?;
                    return Ok(());
                }
                if current.state == "merged" {
                    let number =
                        u64::try_from(current.pr_number.ok_or(Error::Invalid("missing PR"))?)
                            .map_err(|_| Error::Invalid("invalid PR number"))?;
                    let pr = forge.get_change(number).await.map_err(|_| Error::Forge)?;
                    let commit = pr
                        .merge_commit
                        .ok_or(Error::Invalid("forge did not provide merged commit"))?;
                    repo.fetch().map_err(|_| Error::GitOps)?;
                    if !repo
                        .is_ancestor(&commit, &policy.target_branch)
                        .map_err(|_| Error::GitOps)?
                    {
                        return Err(Error::Invalid(
                            "merged commit is not in the configured target",
                        ));
                    }
                    let next = match status.stage.as_str() {
                        "candidate_pr" => "candidate_validation",
                        "promotion_pr" | "change_pr" | "scheduled_pr" => "public_validation",
                        "cleanup_pr" => "completed",
                        _ => unreachable!(),
                    };
                    let mut tx = self.pool.begin().await?;
                    sqlx::query("UPDATE thalamus_rollout_stages SET merged_commit=$3 WHERE change_id=$1 AND stage=$2 AND (merged_commit IS NULL OR merged_commit=$3)")
                        .bind(id).bind(&status.stage).bind(&commit).execute(&mut *tx).await?;
                    sqlx::query("UPDATE thalamus_rollouts SET stage=$2,queued=false,queue_confirmed=false,merge_observed_at=CASE WHEN $3='scheduled_pr' THEN COALESCE(merge_observed_at,now()) ELSE merge_observed_at END,error=NULL,updated_at=now() WHERE change_id=$1 AND stage=$3")
                        .bind(id).bind(next).bind(&status.stage).execute(&mut *tx).await?;
                    tx.commit().await?;
                    return Ok(());
                }
                if status.stage == "scheduled_pr" && current.state == "open" {
                    if let Err(error) = self
                        .reconcile_scheduled(
                            &current,
                            &status,
                            &PublicationContext {
                                repo,
                                forge,
                                config: &scoped,
                            },
                            policy,
                            gate,
                        )
                        .await
                    {
                        // If a queued request becomes unverifiable, withdraw it. Dequeue
                        // itself is not atomic with a merge already in progress.
                        if status.queued {
                            if let Ok(number) = u64::try_from(current.pr_number.unwrap_or(-1)) {
                                if forge.dequeue(&policy.repository, number).await.is_ok() {
                                    sqlx::query("UPDATE thalamus_rollouts SET queued=false,queue_confirmed=false,updated_at=now() WHERE change_id=$1")
                                        .bind(id).execute(&self.pool).await?;
                                }
                            }
                        }
                        return Err(error);
                    }
                }
                Ok(())
            }
            "candidate_validation" | "public_validation" => {
                self.reconcile_validation(&change, &status, repo, &scoped, policy, gate)
                    .await
            }
            "completed" | "failed" | "closed" => Ok(()),
            _ => Err(Error::Invalid("unknown rollout stage")),
        }
    }

    async fn create_followup(
        &self,
        change: &Change,
        status: &RolloutStatus,
        repo: &GitOpsRepo,
        config: &WorkerConfig,
    ) -> Result<()> {
        let promotion = status.stage == "promotion_pr";
        let preceding = if promotion {
            "candidate_pr"
        } else {
            "promotion_pr"
        };
        let merged = status
            .stages
            .iter()
            .find(|s| s.stage == preceding)
            .and_then(|s| s.merged_commit.as_deref())
            .ok_or(Error::Invalid("previous rollout merge is missing"))?;
        repo.fetch().map_err(|_| Error::GitOps)?;
        if !repo
            .is_ancestor(merged, &change.request.target_branch)
            .map_err(|_| Error::GitOps)?
        {
            return Err(Error::Invalid("previous rollout merge is not in target"));
        }
        let base = repo
            .resolve_base(&change.request.target_branch)
            .map_err(|_| Error::GitOps)?;
        let patches = stages::next_stage_patches(repo, config, change, status, &base, promotion)?;
        let mut request = change.request.clone();
        request.base_sha = base;
        request.title = format!(
            "{} {}",
            if promotion { "Promote" } else { "Clean up" },
            request.service
        );
        request.patches = patches;
        request.revert_commit = None;
        request.rollout = Rollout::Rolling;
        let mut tx = self.pool.begin().await?;
        let child = self
            .propose_in_transaction(
                &mut tx,
                &change.actor,
                &format!("rollout:{}:{}", change.id, status.stage),
                request,
            )
            .await?;
        sqlx::query("INSERT INTO thalamus_rollout_stages(change_id,stage,stage_change_id) VALUES ($1,$2,$3) ON CONFLICT(change_id,stage) DO NOTHING")
            .bind(change.id).bind(&status.stage).bind(child.id).execute(&mut *tx).await?;
        audit(
            &mut tx,
            change.id,
            &change.actor,
            if promotion {
                "promotion_proposed"
            } else {
                "cleanup_proposed"
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn reconcile_validation<G: ValidationGate>(
        &self,
        change: &Change,
        status: &RolloutStatus,
        repo: &GitOpsRepo,
        config: &WorkerConfig,
        policy: &RolloutPolicy,
        gate: &G,
    ) -> Result<()> {
        let candidate = status.stage == "candidate_validation";
        let source_stage = if candidate {
            "candidate_pr"
        } else if policy.mode == RolloutMode::Gated {
            "promotion_pr"
        } else if policy.mode == RolloutMode::InPlace {
            "scheduled_pr"
        } else {
            "change_pr"
        };
        let commit = status
            .stages
            .iter()
            .find(|s| s.stage == source_stage)
            .and_then(|s| s.merged_commit.clone())
            .ok_or(Error::Invalid("merged validation revision missing"))?;
        repo.fetch().map_err(|_| Error::GitOps)?;
        if !repo
            .is_ancestor(&commit, &policy.target_branch)
            .map_err(|_| Error::GitOps)?
        {
            return Err(Error::Invalid(
                "validation revision is not in configured target",
            ));
        }
        let (deployment, revision) = if let (Some(deployment), Some(revision)) =
            (&status.candidate_deployment, &status.candidate_revision)
        {
            (deployment.clone(), revision.clone())
        } else {
            stages::public_target(repo, config, change, &commit)?
        };
        let updated = sqlx::query("UPDATE thalamus_rollouts SET candidate_deployment=$2,candidate_revision=$3 WHERE change_id=$1 AND (candidate_deployment IS NULL OR candidate_deployment=$2) AND (candidate_revision IS NULL OR candidate_revision=$3)")
            .bind(change.id).bind(&deployment).bind(&revision).execute(&self.pool).await?.rows_affected();
        if updated != 1 {
            return Err(Error::Invalid("validation deployment identity changed"));
        }
        let candidate_commit = if !candidate && policy.mode == RolloutMode::Gated {
            Some(
                status
                    .stages
                    .iter()
                    .find(|s| s.stage == "candidate_pr")
                    .and_then(|s| s.merged_commit.clone())
                    .ok_or(Error::Invalid("candidate merged revision missing"))?,
            )
        } else {
            None
        };
        let candidate_run = if candidate_commit.is_some() {
            Some(
                status
                    .candidate_validation_run
                    .ok_or(Error::Invalid("candidate validation proof missing"))?,
            )
        } else {
            None
        };
        let requirement = ValidationRequirement {
            change_id: change.id,
            owner_team: policy.owner_team,
            actor: change.actor.clone(),
            stage: if candidate {
                ValidationStage::Candidate
            } else {
                ValidationStage::Public
            },
            service: change.request.service.clone(),
            deployment: deployment.clone(),
            revision: revision.clone(),
            catalog_commit: commit.clone(),
            candidate_run_id: candidate_run,
            candidate_catalog_commit: candidate_commit,
        };
        let progress = gate.reconcile(&requirement).await?;
        let (run, next, error) = match progress {
            ValidationProgress::Pending { run_id } => (run_id, None, None),
            ValidationProgress::Passed {
                run_id,
                catalog_commit,
                backend,
            } => {
                if catalog_commit != commit
                    || backend != format!("{}/{}/{}", change.request.service, deployment, revision)
                {
                    return Err(Error::Invalid(
                        "validation evidence does not match merged backend",
                    ));
                }
                (
                    Some(run_id),
                    Some(if candidate {
                        "promotion_pr"
                    } else if policy.mode == RolloutMode::Gated {
                        "cleanup_pr"
                    } else {
                        "completed"
                    }),
                    None,
                )
            }
            ValidationProgress::Failed { run_id } => (
                Some(run_id),
                Some("failed"),
                Some("validation failed; retain existing deployments and consider a revert PR"),
            ),
        };
        if let Some(run_id) = run {
            let bound = self
                .authorize_validation_run(run_id)
                .await?
                .ok_or(Error::Invalid(
                    "validation run lacks durable rollout binding",
                ))?;
            if !bound.active
                || bound.change_id != change.id
                || bound.stage != requirement.stage
                || bound.owner_team != policy.owner_team
                || bound.service != requirement.service
                || bound.deployment != requirement.deployment
                || bound.revision != requirement.revision
                || bound.catalog_commit != requirement.catalog_commit
            {
                return Err(Error::Invalid(
                    "validation run binding does not match rollout",
                ));
            }
        }
        let mut tx = self.pool.begin().await?;
        let column = if candidate {
            "candidate_validation_run"
        } else {
            "public_validation_run"
        };
        let sql = format!("UPDATE thalamus_rollouts SET {column}=COALESCE($2,{column}),stage=COALESCE($3,stage),error=$4,updated_at=now() WHERE change_id=$1 AND stage=$5");
        sqlx::query(&sql)
            .bind(change.id)
            .bind(run)
            .bind(next)
            .bind(error)
            .bind(&status.stage)
            .execute(&mut *tx)
            .await?;
        if let Some(run) = run {
            sqlx::query("UPDATE thalamus_rollout_stages SET validation_run=$3 WHERE change_id=$1 AND stage=$2")
                .bind(change.id).bind(source_stage).bind(run).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn reconcile_scheduled<F: ChangeRequests + forge::GuardedQueue, G: ValidationGate>(
        &self,
        change: &Change,
        status: &RolloutStatus,
        publication: &PublicationContext<'_, F>,
        policy: &RolloutPolicy,
        gate: &G,
    ) -> Result<()> {
        let repo = publication.repo;
        let forge = publication.forge;
        let config = publication.config;
        if !status.prepared {
            return Err(Error::Invalid("in-place classification is not prepared"));
        }
        let number = u64::try_from(
            change
                .pr_number
                .ok_or(Error::Invalid("missing scheduled PR"))?,
        )
        .map_err(|_| Error::Invalid("invalid PR number"))?;
        let head = change
            .head_sha
            .as_deref()
            .ok_or(Error::Invalid("missing scheduled head"))?;
        let end = status.window_end.ok_or(Error::Invalid("window required"))?;
        let start = status
            .window_start
            .ok_or(Error::Invalid("window required"))?;
        let margin = i64::try_from(policy.merge_margin_seconds)
            .map_err(|_| Error::Invalid("invalid merge margin"))?;
        let cutoff = end
            .checked_sub_signed(Duration::seconds(margin))
            .ok_or(Error::Invalid("invalid window"))?;
        let now = Utc::now();
        let approval = self
            .approval_current(change.id, policy, head, &change.request.base_sha, now)
            .await?;
        let confirmed: bool =
            sqlx::query_scalar("SELECT queue_confirmed FROM thalamus_rollouts WHERE change_id=$1")
                .bind(change.id)
                .fetch_one(&self.pool)
                .await?;
        if status.queued && (!confirmed || !approval || now >= cutoff) {
            forge
                .dequeue(&policy.repository, number)
                .await
                .map_err(|_| Error::Forge)?;
            sqlx::query("UPDATE thalamus_rollouts SET queued=false,queue_confirmed=false,updated_at=now() WHERE change_id=$1")
                .bind(change.id).execute(&self.pool).await?;
        }
        if now < start || now >= cutoff || !approval {
            if status.in_place_pool {
                sqlx::query(
                    "UPDATE thalamus_rollouts SET filler_verified_at=NULL WHERE change_id=$1",
                )
                .bind(change.id)
                .execute(&self.pool)
                .await?;
            }
            return Ok(());
        }
        let pr = forge.get_change(number).await.map_err(|_| Error::Forge)?;
        if pr.state != ChangeState::Open
            || pr.source_branch != change.branch
            || pr.source_repository.as_ref() != Some(&policy.repository)
            || pr.target_branch != policy.target_branch
            || pr.head_sha != head
            || pr.base_sha != change.request.base_sha
        {
            return Err(Error::Invalid("scheduled PR identity or revision changed"));
        }
        repo.fetch().map_err(|_| Error::GitOps)?;
        if repo
            .resolve_base(&policy.target_branch)
            .map_err(|_| Error::GitOps)?
            != pr.base_sha
            || repo.fetch_pull_head(number).map_err(|_| Error::GitOps)? != pr.head_sha
        {
            return Err(Error::Invalid("scheduled PR target or head moved"));
        }
        let preview_config = PreviewConfig {
            worktree_root: config.worktree_root.clone(),
            render: config.render.clone(),
        };
        let preview = Self::compare_revisions(repo, &preview_config, &pr.base_sha, &pr.head_sha)?;
        if !preview_matches(change, &preview) {
            return Err(Error::Invalid("scheduled PR render changed; revalidate"));
        }
        if status.in_place_pool {
            let deployment = status
                .old_deployment
                .as_deref()
                .ok_or(Error::Invalid("old pooled deployment missing"))?;
            let namespace = status
                .workload_namespace
                .as_deref()
                .ok_or(Error::Invalid("pooled workload namespace missing"))?;
            let revision = status
                .previous_revision
                .as_deref()
                .ok_or(Error::Invalid("old pooled revision missing"))?;
            if !stages::zero_filler_render(&preview.base_render, namespace, deployment)?
                || !stages::zero_filler_render(&preview.proposed_render, namespace, deployment)?
            {
                return Err(Error::Invalid(
                    "both GitOps renders must keep the filler at zero",
                ));
            }
            let safety = InPlaceSafetyRequirement {
                change_id: change.id,
                owner_team: policy.owner_team,
                service: change.request.service.clone(),
                deployment: deployment.to_owned(),
                namespace: namespace.to_owned(),
                base_commit: change.request.base_sha.clone(),
                revision: revision.to_owned(),
            };
            match gate.verify_in_place(&safety).await? {
                InPlaceSafety::Pending => {
                    if status.queued {
                        forge
                            .dequeue(&policy.repository, number)
                            .await
                            .map_err(|_| Error::Forge)?;
                        sqlx::query("UPDATE thalamus_rollouts SET queued=false,queue_confirmed=false WHERE change_id=$1")
                            .bind(change.id).execute(&self.pool).await?;
                    }
                    return Ok(());
                }
                InPlaceSafety::Ready {
                    observed_at,
                    catalog_commit,
                    deployment: actual,
                    revision: actual_revision,
                } => {
                    let age = Utc::now().signed_duration_since(observed_at);
                    if age < Duration::zero()
                        || age > Duration::seconds(30)
                        || catalog_commit != safety.base_commit
                        || actual != safety.deployment
                        || actual_revision != safety.revision
                        || !self.authorize_in_place_safety(&safety).await?
                    {
                        return Err(Error::Invalid(
                            "pooled filler safety evidence is stale or mismatched",
                        ));
                    }
                    sqlx::query("UPDATE thalamus_rollouts SET filler_verified_at=$2,updated_at=now() WHERE change_id=$1")
                        .bind(change.id).bind(observed_at).execute(&self.pool).await?;
                }
            }
        }
        let request = forge::GuardedQueueRequest {
            repository: policy.repository.clone(),
            number,
            target_branch: policy.target_branch.clone(),
            head_sha: head.to_owned(),
            base_sha: pr.base_sha,
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
        };
        // Persist intent: a timeout can leave the result of a direct merge unknown.
        sqlx::query("UPDATE thalamus_rollouts SET queued=true,queue_confirmed=false,merge_requested_at=COALESCE(merge_requested_at,now()),updated_at=now() WHERE change_id=$1")
            .bind(change.id).execute(&self.pool).await?;
        let mut tx = self.pool.begin().await?;
        let locked = self.get_in_transaction(&mut tx, change.id).await?;
        let now = Utc::now();
        let filler_fresh: bool = if status.in_place_pool {
            sqlx::query_scalar("SELECT filler_verified_at IS NOT NULL AND filler_verified_at<=now() AND filler_verified_at>now()-interval '30 seconds' FROM thalamus_rollouts WHERE change_id=$1")
                .bind(change.id).fetch_one(&mut *tx).await?
        } else {
            true
        };
        if now < start
            || now >= cutoff
            || !filler_fresh
            || !self
                .approval_current_in_transaction(
                    &mut tx,
                    &locked,
                    policy,
                    head,
                    &change.request.base_sha,
                    now,
                )
                .await?
        {
            tx.rollback().await?;
            forge
                .dequeue(&policy.repository, number)
                .await
                .map_err(|_| Error::Forge)?;
            sqlx::query("UPDATE thalamus_rollouts SET queued=false,queue_confirmed=false,updated_at=now() WHERE change_id=$1").bind(change.id).execute(&self.pool).await?;
            return Ok(());
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(policy.request_timeout_seconds),
            forge.merge_guarded_direct(&request),
        )
        .await;
        match result {
            Ok(Ok(forge::GuardedMergeResult::Merged { .. })) => {
                sqlx::query("UPDATE thalamus_rollouts SET queued=false,queue_confirmed=false,merge_observed_at=COALESCE(merge_observed_at,now()),error=NULL,updated_at=now() WHERE change_id=$1")
                    .bind(change.id).execute(&mut *tx).await?;
            }
            Ok(Ok(forge::GuardedMergeResult::Enqueued(_))) => {
                tx.commit().await?;
                forge
                    .dequeue(&policy.repository, number)
                    .await
                    .map_err(|_| Error::Forge)?;
                return Err(Error::Conflict(
                    "scheduled in-place merge queue is incompatible",
                ));
            }
            Ok(Err(forge::Error::Unsupported(message)))
                if message == "scheduled in-place merge queue is incompatible" =>
            {
                tx.commit().await?;
                return Err(Error::Conflict(
                    "scheduled in-place merge queue is incompatible",
                ));
            }
            Ok(Err(_)) | Err(_) => {
                tx.commit().await?;
                return Err(Error::Forge);
            }
        }
        tx.commit().await?;
        Ok(())
    }
}
