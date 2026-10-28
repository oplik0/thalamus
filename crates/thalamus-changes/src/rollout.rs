//! Durable rollout control. The server authenticates callers; policy and all
//! validation adapters are supplied by the operator, never by an API request.
use super::*;
use chrono::Duration;
use std::{future::Future, pin::Pin};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutMode {
    Gated,
    Rolling,
    InPlace,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequiredCheckPin {
    pub context: String,
    pub app_id: u64,
    #[serde(default)]
    pub status_creator_id: Option<u64>,
}

/// Trusted configuration for one service. Changing any field invalidates approval.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RolloutPolicy {
    pub service: String,
    pub repository: forge::RepoRef,
    pub target_branch: String,
    pub owner_team: Uuid,
    pub validation_tier: String,
    pub mode: RolloutMode,
    pub required_checks: Vec<RequiredCheckPin>,
    pub required_approvers: Vec<String>,
    pub trust_administrators: bool,
    pub trust_merge_group_ci: bool,
    pub merge_margin_seconds: u64,
    pub request_timeout_seconds: u64,
}
impl RolloutPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.owner_team.is_nil()
            || self.service.trim().is_empty()
            || self.target_branch.trim().is_empty()
            || self.validation_tier.trim().is_empty()
            || [&self.service, &self.target_branch, &self.validation_tier]
                .iter()
                .any(|s| s.chars().any(char::is_control))
        {
            return Err(Error::Invalid("invalid rollout policy identity"));
        }
        if self.mode == RolloutMode::InPlace
            && (self.required_checks.is_empty()
                || self.required_approvers.is_empty()
                || !self.trust_administrators
                || self.request_timeout_seconds == 0
                || self
                    .request_timeout_seconds
                    .checked_add(30)
                    .is_none_or(|minimum| self.merge_margin_seconds < minimum)
                || self.merge_margin_seconds > 3600
                || self.required_checks.iter().any(|c| {
                    c.app_id == 0
                        || c.status_creator_id == Some(0)
                        || c.context.trim().is_empty()
                        || c.context.chars().any(char::is_control)
                })
                || self
                    .required_approvers
                    .iter()
                    .any(|a| a.trim().is_empty() || a.chars().any(char::is_control)))
        {
            return Err(Error::Invalid("invalid automatic merge policy"));
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }
    pub(crate) fn check(&self, change: &Change) -> Result<()> {
        self.validate()?;
        let mode = match &change.request.rollout {
            Rollout::Gated => RolloutMode::Gated,
            Rollout::Rolling => RolloutMode::Rolling,
            Rollout::InPlace { .. } => RolloutMode::InPlace,
        };
        if self.service != change.request.service
            || self.target_branch != change.request.target_branch
            || self.validation_tier != change.request.validation_tier
            || self.mode != mode
            || self.owner_team.is_nil()
        {
            return Err(Error::Invalid("configured service rollout policy changed"));
        }
        Ok(())
    }
}
/// Verified merge-group identity accepted from a signed forge webhook.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeGroup {
    pub repository: forge::RepoRef,
    pub base_ref: String,
    pub base_sha: String,
    pub head_ref: String,
    pub head_sha: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CiStatus {
    pub repository: forge::RepoRef,
    pub sha: String,
    pub status_id: i64,
    pub state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RolloutApproval {
    pub actor: Uuid,
    pub owner_team: Uuid,
    pub head_sha: String,
    pub base_sha: String,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub policy_digest: String,
    pub approved_at: DateTime<Utc>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RolloutStage {
    pub stage: String,
    pub change_id: Uuid,
    pub merged_commit: Option<String>,
    pub validation_run: Option<Uuid>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RolloutStatus {
    pub change_id: Uuid,
    pub stage: String,
    pub prepared: bool,
    pub window_start: Option<DateTime<Utc>>,
    pub window_end: Option<DateTime<Utc>>,
    pub candidate_deployment: Option<String>,
    pub candidate_revision: Option<String>,
    pub old_deployment: Option<String>,
    pub in_place_pool: bool,
    pub previous_revision: Option<String>,
    pub workload_namespace: Option<String>,
    /// Point-in-time proof; a queued merge requires continuous refresh.
    pub filler_verified_at: Option<DateTime<Utc>>,
    pub candidate_validation_run: Option<Uuid>,
    pub public_validation_run: Option<Uuid>,
    pub queued: bool,
    pub merge_requested_at: Option<DateTime<Utc>>,
    /// Local time when the merge response or merged PR was first observed.
    /// This is not the upstream merge time or Flux rollout completion.
    pub merge_observed_at: Option<DateTime<Utc>>,
    /// Actual merge time reported by GitHub; unavailable if the forge omitted it.
    pub upstream_merged_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub approvals: Vec<RolloutApproval>,
    pub stages: Vec<RolloutStage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStage {
    Candidate,
    Public,
}
#[derive(Clone, Debug)]
pub struct ValidationRequirement {
    pub change_id: Uuid,
    pub owner_team: Uuid,
    pub actor: String,
    pub stage: ValidationStage,
    pub service: String,
    pub deployment: String,
    pub revision: String,
    pub catalog_commit: String,
    /// Required for the public gate, to compare the live serving revision with
    /// independently observed candidate validation evidence.
    pub candidate_run_id: Option<Uuid>,
    pub candidate_catalog_commit: Option<String>,
}
#[derive(Clone, Debug)]
pub enum ValidationProgress {
    Pending {
        run_id: Option<Uuid>,
    },
    Passed {
        run_id: Uuid,
        catalog_commit: String,
        backend: String,
    },
    Failed {
        run_id: Uuid,
    },
}

#[derive(Clone, Debug)]
pub struct InPlaceSafetyRequirement {
    pub change_id: Uuid,
    pub owner_team: Uuid,
    pub service: String,
    pub deployment: String,
    pub namespace: String,
    pub base_commit: String,
    pub revision: String,
}
#[derive(Clone, Debug)]
pub enum InPlaceSafety {
    Pending,
    Ready {
        observed_at: DateTime<Utc>,
        catalog_commit: String,
        deployment: String,
        revision: String,
    },
}
/// The trusted worker implementation launches a real, idempotent validation run
/// using operator profiles, and independently verifies immutable artifacts,
/// job/route observations and Flux revision before returning Passed.
pub trait ValidationGate: Send + Sync {
    fn reconcile<'a>(
        &'a self,
        requirement: &'a ValidationRequirement,
    ) -> Pin<Box<dyn Future<Output = Result<ValidationProgress>> + Send + 'a>>;
    fn verify_in_place<'a>(
        &'a self,
        requirement: &'a InPlaceSafetyRequirement,
    ) -> Pin<Box<dyn Future<Output = Result<InPlaceSafety>> + Send + 'a>>;
}

fn approval(row: &sqlx::postgres::PgRow) -> Result<RolloutApproval> {
    Ok(RolloutApproval {
        actor: row.try_get("actor")?,
        owner_team: row.try_get("owner_team")?,
        head_sha: row.try_get("head_sha")?,
        base_sha: row.try_get("base_sha")?,
        window_start: row.try_get("window_start")?,
        window_end: row.try_get("window_end")?,
        policy_digest: row.try_get("policy_digest")?,
        approved_at: row.try_get("approved_at")?,
    })
}

async fn lock_owner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: Uuid,
    team: Uuid,
) -> Result<()> {
    // Identity updates also lock this user before changing memberships. A
    // platform administrator is not implicitly a service owner.
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM identity_users WHERE id=$1 AND NOT disabled FOR UPDATE")
            .bind(actor)
            .fetch_optional(&mut **tx)
            .await?;
    let owner: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_memberships WHERE user_id=$1 AND team_id=$2 AND role='team_admin')")
        .bind(actor).bind(team).fetch_one(&mut **tx).await?;
    if exists.is_none() || !owner {
        return Err(Error::Invalid("enabled service owner required"));
    }
    Ok(())
}

impl Store {
    pub async fn rollout(&self, id: Uuid) -> Result<RolloutStatus> {
        let mut tx = self.pool.begin().await?;
        let value = self.rollout_in_transaction(&mut tx, id).await?;
        tx.commit().await?;
        Ok(value)
    }
    pub async fn rollout_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
    ) -> Result<RolloutStatus> {
        let row = sqlx::query("SELECT * FROM thalamus_rollouts WHERE change_id=$1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        let approvals = sqlx::query("SELECT * FROM thalamus_rollout_approvals WHERE change_id=$1 ORDER BY approved_at,actor")
            .bind(id).fetch_all(&mut **tx).await?.iter().map(approval).collect::<Result<Vec<_>>>()?;
        let stages = sqlx::query("SELECT * FROM thalamus_rollout_stages WHERE change_id=$1 ORDER BY CASE stage WHEN 'candidate_pr' THEN 0 WHEN 'scheduled_pr' THEN 0 WHEN 'change_pr' THEN 0 WHEN 'promotion_pr' THEN 1 ELSE 2 END")
            .bind(id).fetch_all(&mut **tx).await?.iter().map(|r| Ok(RolloutStage {
                stage: r.try_get("stage")?, change_id: r.try_get("stage_change_id")?,
                merged_commit: r.try_get("merged_commit")?, validation_run: r.try_get("validation_run")?,
            })).collect::<Result<Vec<_>>>()?;
        Ok(RolloutStatus {
            change_id: id,
            stage: row.try_get("stage")?,
            prepared: row.try_get("prepared")?,
            window_start: row.try_get("window_start")?,
            window_end: row.try_get("window_end")?,
            candidate_deployment: row.try_get("candidate_deployment")?,
            candidate_revision: row.try_get("candidate_revision")?,
            old_deployment: row.try_get("old_deployment")?,
            in_place_pool: row.try_get("in_place_pool")?,
            previous_revision: row.try_get("previous_revision")?,
            workload_namespace: row.try_get("workload_namespace")?,
            filler_verified_at: row.try_get("filler_verified_at")?,
            candidate_validation_run: row.try_get("candidate_validation_run")?,
            public_validation_run: row.try_get("public_validation_run")?,
            queued: row.try_get("queued")?,
            merge_requested_at: row.try_get("merge_requested_at")?,
            merge_observed_at: row.try_get("merge_observed_at")?,
            upstream_merged_at: row.try_get("upstream_merged_at")?,
            error: row.try_get("error")?,
            approvals,
            stages,
        })
    }

    pub async fn schedule_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
        actor: Uuid,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
        policy: &RolloutPolicy,
    ) -> Result<RolloutStatus> {
        lock_owner(tx, actor, policy.owner_team).await?;
        let change = self.get_in_transaction(tx, id).await?;
        policy.check(&change)?;
        let margin = Duration::seconds(
            i64::try_from(policy.merge_margin_seconds)
                .map_err(|_| Error::Invalid("invalid merge margin"))?,
        );
        if policy.mode != RolloutMode::InPlace
            || window_start >= window_end
            || window_end.signed_duration_since(window_start) <= margin
            || window_end.signed_duration_since(Utc::now()) <= margin
        {
            return Err(Error::Invalid("invalid future in-place window"));
        }
        let row = sqlx::query("SELECT * FROM thalamus_rollouts WHERE change_id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        if row.try_get::<bool, _>("queued")? {
            return Err(Error::Invalid("dequeue before rescheduling"));
        }
        if !matches!(change.state.as_str(), "proposed" | "validated" | "open") {
            return Err(Error::Invalid("change cannot be scheduled"));
        }
        // Compare after PostgreSQL timestamp precision conversion so retries
        // with the same submitted instants do not revoke their own approval.
        let changed: bool = sqlx::query_scalar("SELECT window_start IS DISTINCT FROM $2 OR window_end IS DISTINCT FROM $3 FROM thalamus_rollouts WHERE change_id=$1")
            .bind(id).bind(window_start).bind(window_end).fetch_one(&mut **tx).await?;
        if changed {
            sqlx::query("UPDATE thalamus_rollouts SET window_start=$2,window_end=$3,merge_requested_at=NULL,merge_observed_at=NULL,upstream_merged_at=NULL,filler_verified_at=NULL,error=NULL,updated_at=now() WHERE change_id=$1")
                .bind(id).bind(window_start).bind(window_end).execute(&mut **tx).await?;
            sqlx::query("DELETE FROM thalamus_rollout_approvals WHERE change_id=$1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            audit(
                tx,
                id,
                &actor.to_string(),
                "window_scheduled_review_required",
            )
            .await?;
        }
        self.rollout_in_transaction(tx, id).await
    }

    pub async fn approve_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: Uuid,
        actor: Uuid,
        policy: &RolloutPolicy,
    ) -> Result<RolloutApproval> {
        lock_owner(tx, actor, policy.owner_team).await?;
        let change = self.get_in_transaction(tx, id).await?;
        policy.check(&change)?;
        if policy.mode != RolloutMode::InPlace
            || change.state != "open"
            || change.pr_number.is_none()
        {
            return Err(Error::Invalid("approval requires an open in-place PR"));
        }
        let head = change
            .head_sha
            .as_deref()
            .ok_or(Error::Invalid("published head required"))?;
        let row = sqlx::query("SELECT * FROM thalamus_rollouts WHERE change_id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        if !row.try_get::<bool, _>("prepared")? {
            return Err(Error::Invalid("in-place classification is not prepared"));
        }
        if row.try_get::<bool, _>("queued")? {
            return Err(Error::Invalid("cannot approve while queued"));
        }
        let start: DateTime<Utc> = row
            .try_get::<Option<_>, _>("window_start")?
            .ok_or(Error::Invalid("window required"))?;
        let end: DateTime<Utc> = row
            .try_get::<Option<_>, _>("window_end")?
            .ok_or(Error::Invalid("window required"))?;
        let margin = Duration::seconds(
            i64::try_from(policy.merge_margin_seconds)
                .map_err(|_| Error::Invalid("invalid merge margin"))?,
        );
        if start >= end
            || end.signed_duration_since(start) <= margin
            || end.signed_duration_since(Utc::now()) <= margin
        {
            return Err(Error::Invalid(
                "window is expired or too short for the merge safety margin",
            ));
        }
        let row = sqlx::query("INSERT INTO thalamus_rollout_approvals(change_id,actor,owner_team,head_sha,base_sha,window_start,window_end,policy_digest) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(change_id,actor) DO UPDATE SET owner_team=EXCLUDED.owner_team,head_sha=EXCLUDED.head_sha,base_sha=EXCLUDED.base_sha,window_start=EXCLUDED.window_start,window_end=EXCLUDED.window_end,policy_digest=EXCLUDED.policy_digest,approved_at=now() RETURNING *")
            .bind(id).bind(actor).bind(policy.owner_team).bind(head).bind(&change.request.base_sha)
            .bind(start).bind(end).bind(policy.digest()?).fetch_one(&mut **tx).await?;
        audit(tx, id, &actor.to_string(), "owner_approved_head_and_window").await?;
        approval(&row)
    }

    /// Authorize only the current pooled rollout and current owner approval.
    /// This does not itself establish any runtime or scaler evidence.
    pub async fn authorize_in_place_safety(&self, req: &InPlaceSafetyRequirement) -> Result<bool> {
        let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM thalamus_rollouts r JOIN thalamus_changes c ON c.id=r.change_id JOIN thalamus_rollout_approvals a ON a.change_id=r.change_id JOIN identity_users u ON u.id=a.actor AND NOT u.disabled JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=a.owner_team AND m.role='team_admin' WHERE r.change_id=$1 AND r.prepared AND r.in_place_pool AND r.stage='scheduled_pr' AND r.owner_team=$2 AND c.state='open' AND c.request->>'service'=$3 AND r.old_deployment=$4 AND r.workload_namespace=$5 AND c.request->>'base_sha'=$6 AND r.previous_revision=$7 AND a.owner_team=r.owner_team AND a.policy_digest=r.policy_digest AND a.head_sha=c.head_sha AND a.base_sha=$6 AND a.window_start=r.window_start AND a.window_end=r.window_end AND r.window_start<=now() AND now()<r.window_end)")
            .bind(req.change_id).bind(req.owner_team).bind(&req.service).bind(&req.deployment)
            .bind(&req.namespace).bind(&req.base_commit).bind(&req.revision).fetch_one(&self.pool).await?;
        Ok(allowed)
    }

    /// Current approval is bound to owner, policy, PR base/head and persisted window.
    /// This does not assert that GitHub can atomically enforce the window at merge time.
    pub async fn approval_current(
        &self,
        id: Uuid,
        policy: &RolloutPolicy,
        head: &str,
        base: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let change = self.get_in_transaction(&mut tx, id).await?;
        policy.check(&change)?;
        let result = self
            .approval_current_in_transaction(&mut tx, &change, policy, head, base, now)
            .await?;
        tx.commit().await?;
        Ok(result)
    }
    pub(crate) async fn approval_current_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        change: &Change,
        policy: &RolloutPolicy,
        head: &str,
        base: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        if policy.mode != RolloutMode::InPlace
            || change.state != "open"
            || change.head_sha.as_deref() != Some(head)
            || change.request.base_sha != base
        {
            return Ok(false);
        }
        // No identity row locks here: API mutations lock identity before change.
        // This is a fresh point-in-time check, not an atomic forge authorization.
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM thalamus_rollout_approvals a JOIN thalamus_rollouts r ON r.change_id=a.change_id JOIN identity_users u ON u.id=a.actor AND NOT u.disabled JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=a.owner_team AND m.role='team_admin' WHERE r.prepared AND a.change_id=$1 AND a.owner_team=$2 AND a.head_sha=$3 AND a.base_sha=$4 AND a.policy_digest=$5 AND a.window_start=r.window_start AND a.window_end=r.window_end AND r.window_start <= $6 AND $6 < r.window_end)")
            .bind(change.id).bind(policy.owner_team).bind(head).bind(base).bind(policy.digest()?).bind(now)
            .fetch_one(&mut **tx).await?;
        Ok(current)
    }
}

impl Store {
    /// Caller verifies the webhook and operator-owned repository first.
    pub async fn put_merge_group(&self, group: &MergeGroup) -> Result<()> {
        fn full_sha(s: &str) -> bool {
            matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
        }
        if !full_sha(&group.base_sha)
            || !full_sha(&group.head_sha)
            || group.base_ref.is_empty()
            || group.head_ref.is_empty()
            || group.base_ref.chars().any(char::is_control)
            || group.head_ref.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid merge group"));
        }
        sqlx::query("INSERT INTO thalamus_merge_groups(repository,base_ref,base_sha,head_ref,head_sha) VALUES ($1,$2,$3,$4,$5) ON CONFLICT(repository,head_sha) DO UPDATE SET base_ref=EXCLUDED.base_ref,base_sha=EXCLUDED.base_sha,head_ref=EXCLUDED.head_ref,updated_at=now()")
            .bind(serde_json::to_value(&group.repository)?).bind(&group.base_ref).bind(&group.base_sha)
            .bind(&group.head_ref).bind(&group.head_sha).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn merge_groups(&self, repository: &forge::RepoRef) -> Result<Vec<MergeGroup>> {
        let rows = sqlx::query(
            "SELECT * FROM thalamus_merge_groups WHERE repository=$1 ORDER BY updated_at,head_sha",
        )
        .bind(serde_json::to_value(repository)?)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(MergeGroup {
                    repository: serde_json::from_value(r.try_get("repository")?)?,
                    base_ref: r.try_get("base_ref")?,
                    base_sha: r.try_get("base_sha")?,
                    head_ref: r.try_get("head_ref")?,
                    head_sha: r.try_get("head_sha")?,
                })
            })
            .collect()
    }
    pub async fn remove_merge_group(
        &self,
        repository: &forge::RepoRef,
        head_sha: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM thalamus_merge_groups WHERE repository=$1 AND head_sha=$2")
            .bind(serde_json::to_value(repository)?)
            .bind(head_sha)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn open_pr_numbers(&self) -> Result<Vec<u64>> {
        let numbers: Vec<i64> = sqlx::query_scalar("SELECT DISTINCT pr_number FROM thalamus_changes WHERE state='open' AND pr_number IS NOT NULL ORDER BY pr_number")
            .fetch_all(&self.pool).await?;
        numbers
            .into_iter()
            .map(|n| u64::try_from(n).map_err(|_| Error::Invalid("invalid PR number")))
            .collect()
    }
    pub async fn ci_status(
        &self,
        repository: &forge::RepoRef,
        sha: &str,
    ) -> Result<Option<CiStatus>> {
        let row = sqlx::query("SELECT * FROM thalamus_ci_statuses WHERE repository=$1 AND sha=$2")
            .bind(serde_json::to_value(repository)?)
            .bind(sha)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            Ok(CiStatus {
                repository: serde_json::from_value(r.try_get("repository")?)?,
                sha: r.try_get("sha")?,
                status_id: r.try_get("status_id")?,
                state: r.try_get("state")?,
            })
        })
        .transpose()
    }
    pub async fn ci_statuses(&self, repository: &forge::RepoRef) -> Result<Vec<CiStatus>> {
        let rows =
            sqlx::query("SELECT * FROM thalamus_ci_statuses WHERE repository=$1 ORDER BY sha")
                .bind(serde_json::to_value(repository)?)
                .fetch_all(&self.pool)
                .await?;
        rows.iter()
            .map(|r| {
                Ok(CiStatus {
                    repository: serde_json::from_value(r.try_get("repository")?)?,
                    sha: r.try_get("sha")?,
                    status_id: r.try_get("status_id")?,
                    state: r.try_get("state")?,
                })
            })
            .collect()
    }
    pub async fn put_ci_status(&self, status: &CiStatus) -> Result<()> {
        if status.status_id <= 0
            || !matches!(
                status.state.as_str(),
                "pending" | "success" | "failure" | "error"
            )
        {
            return Err(Error::Invalid("invalid CI status"));
        }
        sqlx::query("INSERT INTO thalamus_ci_statuses(repository,sha,status_id,state) VALUES ($1,$2,$3,$4) ON CONFLICT(repository,sha) DO UPDATE SET status_id=EXCLUDED.status_id,state=EXCLUDED.state")
            .bind(serde_json::to_value(&status.repository)?).bind(&status.sha).bind(status.status_id).bind(&status.state).execute(&self.pool).await?;
        Ok(())
    }
}
