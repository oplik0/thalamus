//! Owner-authorized rollout control. Scheduling and merge execution remain in
//! the worker; these endpoints only record durable intent and approvals.
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Extension, Json, Router,
};
use chrono::{DateTime, Utc};
use forge::Webhooks;
use serde::{Deserialize, Serialize};
use thalamus_changes::{Change, RolloutMode, RolloutPolicy, RolloutStatus, Store};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{workflows, ApiError, AppState, RequestPrincipal};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/changes/{id}/rollout", get(get_rollout))
        .route("/changes/{id}/rollout/schedule", post(schedule_rollout))
        .route("/changes/{id}/rollout/approve", post(approve_rollout))
}

/// GitHub sends signed, raw webhook payloads here, outside browser origin and
/// session middleware. No repository, credential, or renderer comes from the event.
pub fn webhook_router() -> Router<Arc<AppState>> {
    Router::new().route("/webhooks/github/changes", post(github_webhook))
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, ApiError> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "invalid webhook"))?;
    if values.next().is_some() || value.is_empty() {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid webhook"));
    }
    Ok(value)
}

async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let config = workflows::gitops(&state)?;
    let github = config
        .github
        .as_ref()
        .ok_or_else(|| workflows::unavailable("GitHub publication is not configured"))?;
    let secret = github
        .webhook_secret
        .as_ref()
        .ok_or_else(|| workflows::unavailable("GitHub webhook is not configured"))?;
    let event = single_header(&headers, "x-github-event")?;
    let signature = single_header(&headers, "x-hub-signature-256")?;
    let forge = workflows::github_client(github)?;
    // Authenticate before doing any Git or database work. The adapter verifies
    // both HMAC and the repository identity in recognized events.
    let parsed = forge
        .webhook(event, signature, secret.as_bytes(), &body)
        .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "invalid webhook"))?;
    // A GitHub App has one webhook URL. Deliver the same authenticated event
    // to the required-status controller before the advisory/follow path.
    if matches!(event, "pull_request" | "merge_group") {
        crate::rollout_ci::webhook(State(state.clone()), headers.clone(), body.clone()).await?;
    }
    if parsed.is_none() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let event = event.to_owned();
    let signature = signature.to_owned();
    tokio::spawn(async move {
        if process_change_webhook(state, &event, &signature, body)
            .await
            .is_err()
        {
            // The periodic worker sweep retries follow and advisory previews.
            eprintln!("GitHub change webhook reconciliation failed");
        }
    });
    Ok(StatusCode::ACCEPTED)
}

async fn process_change_webhook(
    state: Arc<AppState>,
    event: &str,
    signature: &str,
    body: Bytes,
) -> Result<(), ApiError> {
    let config = workflows::gitops(&state)?;
    let github = config
        .github
        .as_ref()
        .ok_or_else(|| workflows::unavailable("GitHub publication is not configured"))?;
    let secret = github
        .webhook_secret
        .as_ref()
        .ok_or_else(|| workflows::unavailable("GitHub webhook is not configured"))?;
    let forge = workflows::github_client(github)?;
    let repository = forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository)
        .map_err(|_| workflows::unavailable("invalid GitHub repository configuration"))?;
    let store = Store::new(state.db.clone());
    for (service, policy) in &config.policies {
        let repo = workflows::open_repo(config, &policy.target_branch)?;
        repo.fetch()
            .map_err(|_| workflows::unavailable("GitOps repository could not be refreshed"))?;
        let base = repo
            .resolve_base(&policy.target_branch)
            .map_err(|_| workflows::unavailable("target branch is unavailable"))?;
        let file = workflows::catalog_service_file(config, &base, service)?;
        let preview = workflows::preview_config(config, &file)?;
        store
            .handle_webhook_preview(
                &repo,
                forge.as_ref(),
                &preview,
                &thalamus_changes::AdvisoryScope {
                    repository: &repository,
                    target_branch: &policy.target_branch,
                    service,
                    forge_actor: &github.actor,
                },
                &thalamus_changes::SignedWebhook {
                    event,
                    signature,
                    secret: secret.as_bytes(),
                    body: &body,
                },
            )
            .await
            .map_err(workflows::change_error)?;
    }
    Ok(())
}

/// The policy is sourced only from deployment configuration and checked against
/// the durable proposal before it is used by any controller or API operation.
pub(crate) fn policy_for_change(
    state: &AppState,
    change: &Change,
) -> Result<RolloutPolicy, ApiError> {
    let config = workflows::gitops(state)?;
    workflows::check_current_policy(change, config)?;
    let policy = workflows::policy(config, &change.request.service)?
        .controller
        .as_ref()
        .ok_or_else(|| {
            workflows::unavailable("rollout controller is not configured for this service")
        })?;
    policy.validate().map_err(workflows::change_error)?;
    let github = config
        .github
        .as_ref()
        .ok_or_else(|| workflows::unavailable("GitHub publication is not configured"))?;
    let repository = forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository)
        .map_err(|_| workflows::unavailable("invalid GitHub repository configuration"))?;
    if policy.repository != repository
        || policy.service != change.request.service
        || policy.target_branch != change.request.target_branch
        || policy.validation_tier != change.request.validation_tier
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "rollout policy changed; create a new proposal",
        ));
    }
    Ok(policy.clone())
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RolloutApprovalView {
    actor: Uuid,
    owner_team: Uuid,
    head_sha: String,
    base_sha: String,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    policy_digest: String,
    approved_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RolloutStageView {
    stage: String,
    change_id: Uuid,
    merged_commit: Option<String>,
    validation_run: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RolloutStatusView {
    change_id: Uuid,
    stage: String,
    prepared: bool,
    window_start: Option<DateTime<Utc>>,
    window_end: Option<DateTime<Utc>>,
    candidate_deployment: Option<String>,
    candidate_revision: Option<String>,
    old_deployment: Option<String>,
    in_place_pool: bool,
    previous_revision: Option<String>,
    workload_namespace: Option<String>,
    filler_verified_at: Option<DateTime<Utc>>,
    candidate_validation_run: Option<Uuid>,
    public_validation_run: Option<Uuid>,
    queued: bool,
    /// Local time at which the controller requested the merge.
    merge_requested_at: Option<DateTime<Utc>>,
    /// Local time at which the controller observed a merge response or merged PR.
    merge_observed_at: Option<DateTime<Utc>>,
    /// Actual merge time reported by the forge, if available.
    upstream_merged_at: Option<DateTime<Utc>>,
    error: Option<String>,
    approvals: Vec<RolloutApprovalView>,
    stages: Vec<RolloutStageView>,
}
impl From<RolloutStatus> for RolloutStatusView {
    fn from(status: RolloutStatus) -> Self {
        Self {
            change_id: status.change_id,
            stage: status.stage,
            prepared: status.prepared,
            window_start: status.window_start,
            window_end: status.window_end,
            candidate_deployment: status.candidate_deployment,
            candidate_revision: status.candidate_revision,
            old_deployment: status.old_deployment,
            in_place_pool: status.in_place_pool,
            previous_revision: status.previous_revision,
            workload_namespace: status.workload_namespace,
            filler_verified_at: status.filler_verified_at,
            candidate_validation_run: status.candidate_validation_run,
            public_validation_run: status.public_validation_run,
            queued: status.queued,
            merge_requested_at: status.merge_requested_at,
            merge_observed_at: status.merge_observed_at,
            upstream_merged_at: status.upstream_merged_at,
            error: status.error,
            approvals: status
                .approvals
                .into_iter()
                .map(|a| RolloutApprovalView {
                    actor: a.actor,
                    owner_team: a.owner_team,
                    head_sha: a.head_sha,
                    base_sha: a.base_sha,
                    window_start: a.window_start,
                    window_end: a.window_end,
                    policy_digest: a.policy_digest,
                    approved_at: a.approved_at,
                })
                .collect(),
            stages: status
                .stages
                .into_iter()
                .map(|s| RolloutStageView {
                    stage: s.stage,
                    change_id: s.change_id,
                    merged_commit: s.merged_commit,
                    validation_run: s.validation_run,
                })
                .collect(),
        }
    }
}

/// Exact reviewed snapshot. The UI submits this unchanged, so an approval
/// cannot silently authorize a newer window, PR head, base, or policy.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovalBinding {
    head_sha: String,
    base_sha: String,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    policy_digest: String,
}

fn approval_binding(
    change: &Change,
    policy: &RolloutPolicy,
    status: &RolloutStatus,
) -> Option<ApprovalBinding> {
    if !status.prepared
        || policy.mode != RolloutMode::InPlace
        || change.state != "open"
        || change.pr_number.is_none()
    {
        return None;
    }
    Some(ApprovalBinding {
        head_sha: change.head_sha.clone()?,
        base_sha: change.request.base_sha.clone(),
        window_start: status.window_start?,
        window_end: status.window_end?,
        policy_digest: policy.digest().ok()?,
    })
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RolloutView {
    mode: &'static str,
    can_schedule: bool,
    can_approve: bool,
    approval_binding: Option<ApprovalBinding>,
    status: RolloutStatusView,
}
fn view(
    change: &Change,
    policy: &RolloutPolicy,
    status: RolloutStatus,
    owner: bool,
) -> RolloutView {
    let mode = match policy.mode {
        RolloutMode::Gated => "gated",
        RolloutMode::Rolling => "rolling",
        RolloutMode::InPlace => "in_place",
    };
    let mutable = owner && policy.mode == RolloutMode::InPlace && !status.queued;
    let can_schedule =
        mutable && matches!(change.state.as_str(), "proposed" | "validated" | "open");
    let approval_binding = approval_binding(change, policy, &status);
    let margin = i64::try_from(policy.merge_margin_seconds)
        .ok()
        .and_then(chrono::Duration::try_seconds);
    let can_approve = mutable
        && margin.is_some_and(|margin| {
            approval_binding.as_ref().is_some_and(|binding| {
                binding
                    .window_end
                    .signed_duration_since(binding.window_start)
                    > margin
                    && binding.window_end.signed_duration_since(Utc::now()) > margin
            })
        });
    RolloutView {
        mode,
        can_schedule,
        can_approve,
        approval_binding,
        status: status.into(),
    }
}

async fn owner_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: Uuid,
    team: Uuid,
) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_users u JOIN identity_memberships m ON m.user_id=u.id WHERE u.id=$1 AND NOT u.disabled AND m.team_id=$2 AND m.role='team_admin')")
        .bind(actor).bind(team).fetch_one(&mut **tx).await?)
}

/// Resolve scope from the durable change; never accept team or policy from the
/// request. The returned guard locks the current identity through the mutation.
async fn authorized<'a>(
    state: &'a AppState,
    caller: &'a RequestPrincipal,
    id: Uuid,
) -> Result<
    (
        thalamus_identity::AuthorizedIdentity<'a>,
        Change,
        RolloutPolicy,
    ),
    ApiError,
> {
    // Promotion and cleanup PRs are child changes. They share the root
    // rollout's owner, policy, schedule and audit, not their own Rolling mode.
    let parents: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT change_id FROM thalamus_rollout_stages WHERE stage_change_id=$1 AND change_id<>$1"
    ).bind(id).fetch_all(&state.db).await?;
    let id = match parents.as_slice() {
        [] => id,
        [root] => *root,
        _ => {
            return Err(workflows::unavailable(
                "rollout stage ownership is ambiguous",
            ))
        }
    };
    let store = Store::new(state.db.clone());
    let initial = store.get(id).await.map_err(workflows::change_error)?;
    let policy = policy_for_change(state, &initial)?;
    let mut guard = state
        .identity
        .begin_authorized(caller.principal(), policy.owner_team, true)
        .await
        .map_err(|error| match error {
            thalamus_identity::Error::Forbidden => {
                ApiError::new(StatusCode::NOT_FOUND, "not found")
            }
            other => other.into(),
        })?;
    let change = store
        .get_in_transaction(guard.transaction(), id)
        .await
        .map_err(workflows::change_error)?;
    let current = policy_for_change(state, &change)?;
    if current.owner_team != policy.owner_team {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "rollout ownership changed",
        ));
    }
    Ok((guard, change, current))
}

#[utoipa::path(get, path = "/api/v1/changes/{id}/rollout", params(("id" = Uuid, Path)), responses((status = 200, body = RolloutView), (status = 401, description = "Authentication required"), (status = 404, description = "Change not found"), (status = 503, description = "Rollout unavailable")))]
pub(crate) async fn get_rollout(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<RolloutView>, ApiError> {
    let (mut guard, change, policy) = authorized(&state, &caller, id).await?;
    let id = change.id;
    let actor = guard.actor_id();
    let owner = owner_member(guard.transaction(), actor, policy.owner_team).await?;
    let status = Store::new(state.db.clone())
        .rollout_in_transaction(guard.transaction(), id)
        .await
        .map_err(workflows::change_error)?;
    let response = view(&change, &policy, status, owner);
    guard.commit().await?;
    Ok(Json(response))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScheduleRollout {
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
}

#[utoipa::path(post, path = "/api/v1/changes/{id}/rollout/schedule", params(("id" = Uuid, Path)), request_body = ScheduleRollout, responses((status = 200, body = RolloutView), (status = 400, description = "Invalid window or state"), (status = 401, description = "Authentication required"), (status = 403, description = "Service owner required"), (status = 404, description = "Change not found"), (status = 503, description = "Rollout unavailable")))]
pub(crate) async fn schedule_rollout(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
    Json(input): Json<ScheduleRollout>,
) -> Result<Json<RolloutView>, ApiError> {
    let (mut guard, change, policy) = authorized(&state, &caller, id).await?;
    let id = change.id;
    let actor = guard.actor_id();
    if !owner_member(guard.transaction(), actor, policy.owner_team).await? {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "service owner required",
        ));
    }
    let status = Store::new(state.db.clone())
        .schedule_in_transaction(
            guard.transaction(),
            id,
            actor,
            input.window_start,
            input.window_end,
            &policy,
        )
        .await
        .map_err(workflows::change_error)?;
    guard
        .audit(None, "rollout.schedule", &id.to_string())
        .await?;
    let response = view(&change, &policy, status, true);
    guard.commit().await?;
    Ok(Json(response))
}

#[utoipa::path(post, path = "/api/v1/changes/{id}/rollout/approve", params(("id" = Uuid, Path)), request_body = ApprovalBinding, responses((status = 200, body = RolloutView), (status = 400, description = "Approval requires an open scheduled PR"), (status = 401, description = "Authentication required"), (status = 403, description = "Service owner required"), (status = 404, description = "Change not found"), (status = 409, description = "Reviewed approval snapshot is stale"), (status = 503, description = "Rollout unavailable")))]
pub(crate) async fn approve_rollout(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
    Json(expected): Json<ApprovalBinding>,
) -> Result<Json<RolloutView>, ApiError> {
    let (mut guard, change, policy) = authorized(&state, &caller, id).await?;
    let id = change.id;
    let actor = guard.actor_id();
    if !owner_member(guard.transaction(), actor, policy.owner_team).await? {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "service owner required",
        ));
    }
    let store = Store::new(state.db.clone());
    let reviewed = store
        .rollout_in_transaction(guard.transaction(), id)
        .await
        .map_err(workflows::change_error)?;
    if approval_binding(&change, &policy, &reviewed).as_ref() != Some(&expected) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "approval snapshot changed; review the current PR and window",
        ));
    }
    store
        .approve_in_transaction(guard.transaction(), id, actor, &policy)
        .await
        .map_err(workflows::change_error)?;
    let status = store
        .rollout_in_transaction(guard.transaction(), id)
        .await
        .map_err(workflows::change_error)?;
    guard
        .audit(None, "rollout.approve", &id.to_string())
        .await?;
    let response = view(&change, &policy, status, true);
    guard.commit().await?;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use thalamus_changes::{Proposal, Rollout};

    #[test]
    fn owner_approval_snapshot_changes_with_head_window_and_policy() {
        let start = Utc::now() + Duration::minutes(5);
        let end = start + Duration::hours(1);
        let id = Uuid::new_v4();
        let mut change = Change {
            id,
            actor: "actor".into(),
            idempotency_key: "key".into(),
            request: Proposal {
                service: "svc".into(),
                base_sha: "a".repeat(40),
                target_branch: "main".into(),
                title: "svc".into(),
                patches: vec![],
                validation_tier: "functional".into(),
                rollout: Rollout::InPlace {
                    window_start: start,
                    window_end: end,
                },
                revert_commit: None,
            },
            state: "open".into(),
            branch: "thalamus/change".into(),
            head_sha: Some("b".repeat(40)),
            pr_number: Some(1),
            pr_url: None,
            base_render_sha256: None,
            proposed_render_sha256: None,
            data_diff: None,
            rendered_diff: None,
            error: None,
        };
        let mut status = RolloutStatus {
            change_id: id,
            stage: "scheduled_pr".into(),
            prepared: true,
            window_start: Some(start),
            window_end: Some(end),
            candidate_deployment: None,
            candidate_revision: None,
            old_deployment: None,
            in_place_pool: false,
            previous_revision: None,
            workload_namespace: None,
            filler_verified_at: None,
            candidate_validation_run: None,
            public_validation_run: None,
            queued: false,
            merge_requested_at: None,
            merge_observed_at: None,
            upstream_merged_at: None,
            error: None,
            approvals: vec![],
            stages: vec![],
        };
        let mut policy = RolloutPolicy {
            service: "svc".into(),
            repository: forge::RepoRef::parse(
                forge::ForgeKind::Github,
                "https://github.com/org/repo",
            )
            .unwrap(),
            target_branch: "main".into(),
            owner_team: Uuid::new_v4(),
            validation_tier: "functional".into(),
            mode: RolloutMode::InPlace,
            required_checks: vec![],
            required_approvers: vec![],
            trust_administrators: true,
            trust_merge_group_ci: true,
            merge_margin_seconds: 60,
            request_timeout_seconds: 10,
        };
        let displayed = approval_binding(&change, &policy, &status).unwrap();
        change.head_sha = Some("c".repeat(40));
        assert_ne!(
            approval_binding(&change, &policy, &status),
            Some(displayed.clone())
        );
        change.head_sha = Some("b".repeat(40));
        status.window_end = Some(end + Duration::minutes(1));
        assert_ne!(
            approval_binding(&change, &policy, &status),
            Some(displayed.clone())
        );
        status.window_end = Some(end);
        policy.owner_team = Uuid::new_v4();
        assert_ne!(approval_binding(&change, &policy, &status), Some(displayed));
    }
}
