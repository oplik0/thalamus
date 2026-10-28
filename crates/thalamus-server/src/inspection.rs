//! Principal-aware inspection of catalog intent, rendered revisions, and run evidence.
//! The catalog does not currently bind services to teams, so repository-wide
//! comparison and evidence inspection require platform administration.
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Extension, Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use thalamus_identity::VerifiedPrincipal;
use uuid::Uuid;

use crate::{ApiError, AppState};

/// Operator configuration for the trusted tenant-labeled Kubernetes log pipeline.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectionConfig {
    loki_endpoint: String,
    trusted_tenant_labels: bool,
}

impl InspectionConfig {
    pub fn from_env() -> Result<Option<Self>, String> {
        let value = match std::env::var("THALAMUS_INSPECTION_CONFIG") {
            Ok(value) if !value.trim().is_empty() => value,
            Ok(_) => return Err("THALAMUS_INSPECTION_CONFIG must not be empty".into()),
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => return Err("THALAMUS_INSPECTION_CONFIG is not valid Unicode".into()),
        };
        let config: Self =
            serde_json::from_str(&value).map_err(|_| "invalid THALAMUS_INSPECTION_CONFIG")?;
        if !config.trusted_tenant_labels {
            return Err("inspection logs require collector-authenticated tenant labels".into());
        }
        thalamus_logs::LokiClient::new(&config.loki_endpoint)
            .map_err(|_| "invalid inspection Loki endpoint")?;
        Ok(Some(config))
    }
}

fn unavailable(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message)
}
fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "service not found")
}

fn log_bounds(seconds: u32, limit: u32) -> Result<(), ApiError> {
    if !(1..=3600).contains(&seconds) || !(1..=500).contains(&limit) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "logs require 1–3600 seconds and 1–500 lines",
        ));
    }
    Ok(())
}

fn single_team(teams: &[Uuid]) -> Result<Uuid, ApiError> {
    match teams {
        [team] => Ok(*team),
        _ => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "logs require one unambiguous authorized team",
        )),
    }
}

fn copy_principal<'a>(principal: &VerifiedPrincipal<'a>) -> VerifiedPrincipal<'a> {
    match principal {
        VerifiedPrincipal::Session(token) => VerifiedPrincipal::Session(token),
        VerifiedPrincipal::Oidc { issuer, subject } => VerifiedPrincipal::Oidc { issuer, subject },
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct RunEvidence {
    id: Uuid,
    deployment: String,
    kind: String,
    state: String,
    revision: String,
    observed_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

async fn evidence(
    guard: &mut thalamus_identity::AuthorizedIdentity<'_>,
    service: &str,
) -> Result<Vec<RunEvidence>, ApiError> {
    let rows = sqlx::query("SELECT id, request, state, updated_at FROM thalamus_runs WHERE request->>'service'=$1 ORDER BY updated_at DESC, id DESC LIMIT 100")
        .bind(service).fetch_all(&mut **guard.transaction()).await?;
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let request: serde_json::Value = row.try_get("request")?;
        let request: thalamus_runs::RunRequest = serde_json::from_value(request)
            .map_err(|_| unavailable("stored run evidence is invalid"))?;
        result.push(RunEvidence {
            id: row.try_get("id")?,
            deployment: request.deployment,
            kind: match request.kind {
                thalamus_runs::RunKind::Validation => "validation",
                thalamus_runs::RunKind::Benchmark => "benchmark",
                thalamus_runs::RunKind::Debug => "debug",
            }
            .into(),
            state: row.try_get("state")?,
            revision: request.evidence.desired_configuration,
            observed_at: Some(request.evidence.conditions.observed_at),
            updated_at: row.try_get("updated_at")?,
        });
    }
    Ok(result)
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Comparison {
    service: String,
    from: String,
    to: String,
    data_diff: String,
    rendered_diff: String,
    /// Run configuration digests are not Git commit IDs. These are recent
    /// service runs, not evidence asserted to apply to either compared commit.
    run_revision_relation: &'static str,
    runs: Vec<RunEvidence>,
}

pub(crate) async fn compare_revisions(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
    service: &str,
    from: &str,
    to: &str,
) -> Result<Comparison, ApiError> {
    let mut guard = state.identity.begin_platform_admin(principal).await?;
    let (repo, preview) = crate::workflows::comparison_config(state, service, from, to)?;
    let runs = evidence(&mut guard, service).await?;
    let from_owned = from.to_owned();
    let to_owned = to.to_owned();
    let base_path = preview
        .worktree_root
        .join(format!("inspect-{}-base", Uuid::new_v4()));
    let head_path = preview
        .worktree_root
        .join(format!("inspect-{}-head", Uuid::new_v4()));
    let result = tokio::task::spawn_blocking(move || {
        repo.preview_refs(
            &from_owned,
            &to_owned,
            &base_path,
            &head_path,
            &preview.render,
        )
    })
    .await
    .map_err(|_| unavailable("revision comparison failed"))?
    .map_err(|_| unavailable("revision render or diff failed"))?;
    guard.commit().await?;
    Ok(Comparison {
        service: service.into(),
        from: from.into(),
        to: to.into(),
        data_diff: result.data_diff,
        rendered_diff: result.rendered_diff,
        run_revision_relation: "unlinked",
        runs,
    })
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Compatibility {
    service: String,
    deployments: Vec<DeploymentCompatibility>,
}
#[derive(Serialize, utoipa::ToSchema)]
struct DeploymentCompatibility {
    deployment: String,
    artifact: String,
    hardware: String,
    #[schema(value_type = Option<serde_json::Value>)]
    hardware_definition: Option<serde_yaml::Value>,
    recipe: Option<String>,
    tier: &'static str,
    reason: &'static str,
    evidence: Vec<RunEvidence>,
}

pub(crate) async fn check_compatibility(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
    service: &str,
) -> Result<Compatibility, ApiError> {
    let mut guard = state.identity.begin_platform_admin(principal).await?;
    let (policy, catalog) = crate::catalog::committed_catalog(state).await?;
    let selected = catalog
        .into_iter()
        .find(|item| item.name == service)
        .ok_or_else(not_found)?;
    let hardware: serde_yaml::Value = serde_yaml::from_slice(
        &policy
            .committed_catalog_blob("hardware.yaml")
            .map_err(policy_error)?,
    )
    .map_err(|_| unavailable("hardware catalog is invalid"))?;
    let mut runs = evidence(&mut guard, service).await?;
    let mut deployments = Vec::new();
    for deployment in selected.deployments {
        let name = deployment.routing.deployment;
        let mut matching = Vec::new();
        let mut rest = Vec::new();
        for run in runs {
            if run.deployment == name {
                matching.push(run);
            } else {
                rest.push(run);
            }
        }
        runs = rest;
        let definition = hardware.get(deployment.hardware.as_str()).cloned();
        deployments.push(DeploymentCompatibility {
            deployment: name, artifact: deployment.artifact, hardware: deployment.hardware,
            hardware_definition: definition, recipe: None, tier: "unknown",
            reason: "The catalog does not define a recipe or bind the current configuration to pinned validation evidence.",
            evidence: matching,
        });
    }
    guard.commit().await?;
    Ok(Compatibility {
        service: service.into(),
        deployments,
    })
}

fn policy_error(error: thalamus_access::Error) -> ApiError {
    match error {
        thalamus_access::Error::Forbidden => not_found(),
        thalamus_access::Error::Identity(error) => error.into(),
        thalamus_access::Error::Source => unavailable("service access policy is unavailable"),
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Logs {
    service: String,
    deployment: String,
    entries: Vec<thalamus_logs::LogEntry>,
}

pub(crate) async fn get_logs(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
    service: &str,
    deployment: &str,
    since_seconds: u32,
    limit: u32,
) -> Result<Logs, ApiError> {
    log_bounds(since_seconds, limit)?;
    let config = state
        .config
        .inspection
        .as_ref()
        .ok_or_else(|| unavailable("trusted Kubernetes log collection is not configured"))?;
    let source = state
        .policy_source
        .as_ref()
        .ok_or_else(|| unavailable("service access policy is not configured"))?;
    let policy = source.load().map_err(policy_error)?;
    let id = policy
        .service_id(service)
        .map_err(policy_error)?
        .ok_or_else(not_found)?;
    if !policy
        .includes_deployment(&id, deployment)
        .map_err(policy_error)?
    {
        return Err(not_found());
    }
    let teams = policy
        .authorized_teams(&state.identity, copy_principal(&principal), &id)
        .await
        .map_err(policy_error)?;
    let team = single_team(&teams)?;
    let team_string = team.to_string();
    let binding =
        thalamus_logs::AuthorizedDeployment::new(&team_string, &id.namespace, &id.name, deployment)
            .map_err(|_| unavailable("invalid authorized log binding"))?;
    let client = thalamus_logs::LokiClient::new(&config.loki_endpoint)
        .map_err(|_| unavailable("invalid log service configuration"))?;
    let entries = client
        .recent(
            &binding,
            &team_string,
            &id.name,
            deployment,
            u64::from(since_seconds),
            limit as usize,
        )
        .await
        .map_err(|_| unavailable("bounded deployment logs are unavailable"))?;
    // Authorization and the published policy may change during the network request.
    let teams = policy
        .authorized_teams(&state.identity, principal, &id)
        .await
        .map_err(policy_error)?;
    if single_team(&teams)? != team
        || !policy
            .includes_deployment(&id, deployment)
            .map_err(policy_error)?
    {
        return Err(not_found());
    }
    Ok(Logs {
        service: id.name,
        deployment: deployment.into(),
        entries,
    })
}

#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct CompareQuery {
    from: String,
    to: String,
}
#[utoipa::path(
    get, path = "/api/v1/services/{id}/compare",
    params(("id" = String, Path, description = "Service name"), ("from" = String, Query, description = "Base revision"), ("to" = String, Query, description = "Target revision")),
    responses((status = 200, body = Comparison), (status = 400, description = "Invalid path or query"), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 503, description = "Comparison or identity service unavailable"))
)]
pub(crate) async fn compare_route(
    State(state): State<Arc<AppState>>,
    Path(service): Path<String>,
    Query(query): Query<CompareQuery>,
    Extension(caller): Extension<crate::RequestPrincipal>,
) -> Result<Json<Comparison>, ApiError> {
    Ok(Json(
        compare_revisions(&state, caller.principal(), &service, &query.from, &query.to).await?,
    ))
}
#[utoipa::path(
    get, path = "/api/v1/services/{id}/compatibility",
    params(("id" = String, Path, description = "Service name")),
    responses((status = 200, body = Compatibility), (status = 400, description = "Invalid path"), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 404, description = "Service not found"), (status = 503, description = "Catalog or identity service unavailable"))
)]
pub(crate) async fn compatibility_route(
    State(state): State<Arc<AppState>>,
    Path(service): Path<String>,
    Extension(caller): Extension<crate::RequestPrincipal>,
) -> Result<Json<Compatibility>, ApiError> {
    Ok(Json(
        check_compatibility(&state, caller.principal(), &service).await?,
    ))
}
#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct LogsQuery {
    deployment: String,
    since_seconds: u32,
    limit: u32,
}
#[utoipa::path(
    get, path = "/api/v1/services/{id}/logs",
    params(("id" = String, Path, description = "Service name"), ("deployment" = String, Query, description = "Deployment name"), ("since_seconds" = u32, Query, description = "Lookback in seconds, from 1 to 3600"), ("limit" = u32, Query, description = "Maximum lines, from 1 to 500")),
    responses((status = 200, body = Logs), (status = 400, description = "Invalid path, query, or bounds"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Service or deployment not found"), (status = 503, description = "Logs or identity service unavailable"))
)]
pub(crate) async fn logs_route(
    State(state): State<Arc<AppState>>,
    Path(service): Path<String>,
    Query(query): Query<LogsQuery>,
    Extension(caller): Extension<crate::RequestPrincipal>,
) -> Result<Json<Logs>, ApiError> {
    Ok(Json(
        get_logs(
            &state,
            caller.principal(),
            &service,
            &query.deployment,
            query.since_seconds,
            query.limit,
        )
        .await?,
    ))
}
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/services/{id}/compare", get(compare_route))
        .route("/services/{id}/compatibility", get(compatibility_route))
        .route("/services/{id}/logs", get(logs_route))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_bounds_are_strict_and_inclusive() {
        assert!(log_bounds(1, 1).is_ok());
        assert!(log_bounds(3600, 500).is_ok());
        for (seconds, limit) in [(0, 1), (3601, 1), (1, 0), (1, 501)] {
            assert!(log_bounds(seconds, limit).is_err());
        }
    }

    #[test]
    fn logs_never_guess_a_tenant() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert!(single_team(&[]).is_err());
        assert_eq!(single_team(&[a]).unwrap(), a);
        assert!(single_team(&[a, b]).is_err());
    }
}
