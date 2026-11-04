//! Authorized access to verified benchmark results and comparisons.
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Extension, Json, Router,
};
use thalamus_identity::AuthorizedIdentity;
use thalamus_runs::{
    compare_benchmark_results, parse_benchmark_result, ArtifactRef, Artifacts, BenchmarkComparison,
    BenchmarkResult, Run, RunKind, Store,
};
use uuid::Uuid;

use crate::{ApiError, AppState, RequestPrincipal};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/runs/{id}/benchmark", get(get_benchmark))
        .route(
            "/runs/{id}/benchmark/compare/{other_id}",
            get(compare_benchmark),
        )
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "benchmark run not found")
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "benchmark results unavailable",
    )
}

fn authorization_error(error: thalamus_identity::Error) -> ApiError {
    match error {
        thalamus_identity::Error::Forbidden => not_found(),
        other => other.into(),
    }
}

fn run_error(error: thalamus_runs::Error) -> ApiError {
    match error {
        thalamus_runs::Error::NotFound => not_found(),
        _ => unavailable(),
    }
}

fn artifact_error(error: thalamus_runs::Error) -> ApiError {
    match error {
        thalamus_runs::Error::ArtifactIntegrity => ApiError::new(
            StatusCode::CONFLICT,
            "benchmark artifact failed integrity verification",
        ),
        _ => unavailable(),
    }
}

fn invalid_summary(_: thalamus_runs::Error) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "benchmark summary is invalid")
}

fn incompatible_results(_: thalamus_runs::Error) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "benchmark runs are not comparable")
}

struct RunTeam {
    // Keep the stored spelling for the tenant-scoped run lookup.
    stored: String,
    id: Uuid,
}

// This lookup follows workflows::get_run_for: authenticate before resolving the
// opaque ID, then re-read the run under an authorized transaction.
async fn resolve_team(state: &AppState, id: Uuid) -> Result<RunTeam, ApiError> {
    let stored: String = sqlx::query_scalar("SELECT team FROM thalamus_runs WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let team_id = Uuid::parse_str(&stored).map_err(|_| not_found())?;
    Ok(RunTeam {
        stored,
        id: team_id,
    })
}

// A second begin_authorized would wait on the very same principal row already
// locked by this guard. Check an additional team in this transaction using the
// same non-admin policy as identity::authorize_locked instead. Identity changes
// serialize on the guard's user row, so neither team's membership can become
// stale while artifacts are read.
async fn authorize_additional_team(
    guard: &mut AuthorizedIdentity<'_>,
    team: Uuid,
) -> Result<(), ApiError> {
    let actor = guard.actor_id();
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$2 WHERE u.id=$1 AND NOT u.disabled AND (u.platform_admin OR m.role IS NOT NULL))",
    )
    .bind(actor)
    .bind(team)
    .fetch_one(&mut **guard.transaction())
    .await?;
    if !allowed {
        return Err(not_found());
    }
    Ok(())
}

// The project row lock prevents project reassignment or deletion between the
// ownership check and the object read. The team-scoped run is loaded only from
// the same identity transaction.
async fn load_run(
    guard: &mut AuthorizedIdentity<'_>,
    store: &Store,
    team: &RunTeam,
    id: Uuid,
) -> Result<Run, ApiError> {
    let run = store
        .get_in_transaction(guard.transaction(), &team.stored, id)
        .await
        .map_err(run_error)?;
    let project: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM identity_projects WHERE id=$1 AND team_id=$2 FOR SHARE")
            .bind(run.request.project_id)
            .bind(team.id)
            .fetch_optional(&mut **guard.transaction())
            .await?;
    if project.is_none() {
        return Err(not_found());
    }
    Ok(run)
}

fn required_artifact(run: &Run) -> Result<&ArtifactRef, ApiError> {
    if run.request.kind != RunKind::Benchmark || run.state != "succeeded" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "benchmark run has not succeeded",
        ));
    }
    match run.artifacts.as_deref() {
        Some([artifact]) => Ok(artifact),
        _ => Err(ApiError::new(
            StatusCode::CONFLICT,
            "benchmark run does not have exactly one result artifact",
        )),
    }
}

async fn read_result(
    artifacts: &Artifacts,
    run: &Run,
    artifact: &ArtifactRef,
) -> Result<BenchmarkResult, ApiError> {
    // Artifacts::read validates the run-bound content-addressed path, size and
    // digest before the parser checks the summary's run ID and request.
    let bytes = artifacts
        .read(run.id, artifact)
        .await
        .map_err(artifact_error)?;
    parse_benchmark_result(run, bytes.as_ref()).map_err(invalid_summary)
}

#[utoipa::path(
    get,
    path = "/api/v1/runs/{id}/benchmark",
    params(("id" = Uuid, Path, description = "Benchmark run ID")),
    responses(
        (status = 200, body = BenchmarkResult),
        (status = 401, description = "Authentication required", body = crate::ErrorBody),
        (status = 404, description = "Run not found", body = crate::ErrorBody),
        (status = 409, description = "Run or benchmark summary is incomplete or invalid", body = crate::ErrorBody),
        (status = 503, description = "Benchmark results unavailable", body = crate::ErrorBody)
    )
)]
pub(crate) async fn get_benchmark(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<BenchmarkResult>, ApiError> {
    state
        .identity
        .authenticate_principal(caller.principal())
        .await?;
    let team = resolve_team(&state, id).await?;
    let mut guard = state
        .identity
        .begin_authorized(caller.principal(), team.id, false)
        .await
        .map_err(authorization_error)?;
    let store = Store::new(state.db.clone());
    let run = load_run(&mut guard, &store, &team, id).await?;
    let artifacts = state.benchmark_artifacts.as_ref().ok_or_else(unavailable)?;
    let artifact = required_artifact(&run)?;
    let result = read_result(artifacts, &run, artifact).await?;
    guard.commit().await?;
    Ok(Json(result))
}

#[utoipa::path(
    get,
    path = "/api/v1/runs/{id}/benchmark/compare/{other_id}",
    params(
        ("id" = Uuid, Path, description = "Baseline benchmark run ID"),
        ("other_id" = Uuid, Path, description = "Candidate benchmark run ID")
    ),
    responses(
        (status = 200, body = BenchmarkComparison),
        (status = 401, description = "Authentication required", body = crate::ErrorBody),
        (status = 404, description = "Run not found", body = crate::ErrorBody),
        (status = 409, description = "Runs or benchmark summaries are incomplete, invalid, or not comparable", body = crate::ErrorBody),
        (status = 503, description = "Benchmark results unavailable", body = crate::ErrorBody)
    )
)]
pub(crate) async fn compare_benchmark(
    State(state): State<Arc<AppState>>,
    Path((id, other_id)): Path<(Uuid, Uuid)>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<BenchmarkComparison>, ApiError> {
    state
        .identity
        .authenticate_principal(caller.principal())
        .await?;
    let team = resolve_team(&state, id).await?;
    let other_team = resolve_team(&state, other_id).await?;
    let mut guard = state
        .identity
        .begin_authorized(caller.principal(), team.id, false)
        .await
        .map_err(authorization_error)?;
    if other_team.id != team.id {
        authorize_additional_team(&mut guard, other_team.id).await?;
    }

    // Authorize and verify ownership of both IDs before disclosing either
    // result's readiness or contacting the artifact store.
    let store = Store::new(state.db.clone());
    let before = load_run(&mut guard, &store, &team, id).await?;
    let after = load_run(&mut guard, &store, &other_team, other_id).await?;
    let artifacts = state.benchmark_artifacts.as_ref().ok_or_else(unavailable)?;
    let before_artifact = required_artifact(&before)?;
    let after_artifact = required_artifact(&after)?;
    let before_result = read_result(artifacts, &before, before_artifact).await?;
    let after_result = read_result(artifacts, &after, after_artifact).await?;
    let comparison = compare_benchmark_results(&before, before_result, &after, after_result)
        .map_err(incompatible_results)?;
    guard.commit().await?;
    Ok(Json(comparison))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thalamus_runs::{EvidenceIdentity, RunConditions, RunRequest, Workload};

    fn succeeded_benchmark() -> Run {
        let id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let digest = "a".repeat(64);
        Run {
            id,
            team: Uuid::new_v4().to_string(),
            actor: "actor".into(),
            idempotency_key: "key".into(),
            fingerprint: "fingerprint".into(),
            request: RunRequest {
                project_id: Uuid::new_v4(),
                kind: RunKind::Benchmark,
                service: "service".into(),
                deployment: "deployment".into(),
                runner: "runner".into(),
                workload: Workload {
                    name: "load".into(),
                    seed: 1,
                    concurrency: 1,
                    duration_seconds: 10,
                    warmup_seconds: 1,
                    max_requests: 10,
                    model_budget_tokens: 0,
                    tool_budget_calls: 0,
                },
                evidence: EvidenceIdentity {
                    desired_configuration: "configuration".into(),
                    observed_runtime: "runtime".into(),
                    test_definition: "definition".into(),
                    conditions: RunConditions {
                        hardware: "hardware".into(),
                        nodes: vec!["node".into()],
                        replicas: 1,
                        cache_state: "warm".into(),
                        competing_traffic: "none".into(),
                        observed_at: now,
                    },
                },
            },
            job_name: "job".into(),
            job_uid: None,
            ownership_nonce: Uuid::new_v4(),
            job_spec: None,
            job_namespace: None,
            state: "succeeded".into(),
            artifacts: Some(vec![ArtifactRef {
                path: format!("runs/{id}/{digest}"),
                sha256: digest,
                size: 1,
            }]),
            error: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn only_succeeded_benchmarks_with_one_artifact_are_eligible() {
        let mut run = succeeded_benchmark();
        assert!(required_artifact(&run).is_ok());
        for kind in [RunKind::Validation, RunKind::Debug] {
            run.request.kind = kind;
            assert_eq!(
                required_artifact(&run).unwrap_err().status,
                StatusCode::CONFLICT
            );
        }
        run.request.kind = RunKind::Benchmark;
        for state in [
            "pending",
            "running",
            "ingesting",
            "ingestion_failed",
            "failed",
        ] {
            run.state = state.into();
            assert_eq!(
                required_artifact(&run).unwrap_err().status,
                StatusCode::CONFLICT
            );
        }
        run.state = "succeeded".into();
        let artifact = run.artifacts.as_ref().unwrap()[0].clone();
        run.artifacts = None;
        assert_eq!(
            required_artifact(&run).unwrap_err().status,
            StatusCode::CONFLICT
        );
        run.artifacts = Some(vec![]);
        assert_eq!(
            required_artifact(&run).unwrap_err().status,
            StatusCode::CONFLICT
        );
        run.artifacts = Some(vec![artifact.clone(), artifact.clone()]);
        assert_eq!(
            required_artifact(&run).unwrap_err().status,
            StatusCode::CONFLICT
        );
        run.artifacts = Some(vec![artifact]);
        assert!(required_artifact(&run).is_ok());
    }

    #[test]
    fn forbidden_run_is_hidden_and_backend_errors_are_sanitized() {
        assert_eq!(
            authorization_error(thalamus_identity::Error::Forbidden).status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            authorization_error(thalamus_identity::Error::Unauthorized).status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            run_error(thalamus_runs::Error::NotFound).status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            artifact_error(thalamus_runs::Error::ArtifactIntegrity).status,
            StatusCode::CONFLICT
        );
        let secret = "https://user:secret@example.invalid/private-object";
        for error in [
            run_error(thalamus_runs::Error::Invalid(secret.into())),
            artifact_error(thalamus_runs::Error::Invalid(secret.into())),
            invalid_summary(thalamus_runs::Error::Invalid(secret.into())),
            incompatible_results(thalamus_runs::Error::Invalid(secret.into())),
        ] {
            assert!(!error.message.contains(secret));
        }
    }
}
