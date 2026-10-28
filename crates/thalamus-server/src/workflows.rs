//! Authenticated change and run endpoints. Catalog-wide changes are restricted to
//! platform administrators until the catalog supplies authoritative service owners.
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Component, Path as FsPath, PathBuf},
    process::Command,
    sync::Arc,
};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Extension, Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thalamus_changes::{Change, PreviewConfig, Proposal, Rollout, WorkerConfig};
use thalamus_gitops::{CommitAuthor, GitOpsRepo, Operation, Patch, RenderCommand};
use thalamus_identity::VerifiedPrincipal;
use thalamus_runs::{Run, RunKind, RunRequest, Workload};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{ApiError, AppState, RequestPrincipal};

/// Optional operator-owned workflow configuration. The JSON value of
/// THALAMUS_WORKFLOWS_CONFIG contains `gitops` and/or `runs` objects. Nothing in
/// these settings is taken from an API caller.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfig {
    #[serde(default)]
    gitops: Option<GitOpsConfig>,
    #[serde(default)]
    runs: Option<RunConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitOpsConfig {
    repository_dir: PathBuf,
    worktree_root: PathBuf,
    render_executable: String,
    /// Exactly one argument must contain {service_file}; it is substituted as
    /// data in an argv element, never interpreted by a shell.
    render_args: Vec<String>,
    policies: BTreeMap<String, ServicePolicy>,
    #[serde(default)]
    github: Option<GitHubConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServicePolicy {
    target_branch: String,
    validation_tier: String,
    rollout: Rollout,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitHubConfig {
    repository: String,
    api_base: String,
    token: String,
    actor: String,
    commit_author_name: String,
    commit_author_email: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfig {
    /// A trusted collector/operator writes this file, not the browser. Profiles
    /// must bind a project/team and contain real, independently observed evidence.
    profiles_file: PathBuf,
    max_evidence_age_seconds: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunProfile {
    team_id: Uuid,
    request: RunRequest,
}

impl WorkflowConfig {
    pub fn from_env() -> Result<Option<Self>, String> {
        let value = match std::env::var("THALAMUS_WORKFLOWS_CONFIG") {
            Ok(value) if !value.trim().is_empty() => value,
            Ok(_) => return Err("THALAMUS_WORKFLOWS_CONFIG must not be empty".into()),
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => return Err("THALAMUS_WORKFLOWS_CONFIG is not valid Unicode".into()),
        };
        let config: Self = serde_json::from_str(&value)
            .map_err(|_| "THALAMUS_WORKFLOWS_CONFIG is invalid JSON or has invalid fields")?;
        config.validate()?;
        Ok(Some(config))
    }

    fn validate(&self) -> Result<(), String> {
        if self.gitops.is_none() && self.runs.is_none() {
            return Err("THALAMUS_WORKFLOWS_CONFIG must configure gitops or runs".into());
        }
        if let Some(config) = &self.gitops {
            if !config.repository_dir.is_absolute()
                || !config.worktree_root.is_absolute()
                || config.render_executable.trim().is_empty()
                || config.render_executable.contains('\0')
                || config.render_args.iter().any(|s| s.contains('\0'))
                || config
                    .render_args
                    .iter()
                    .map(|s| s.matches("{service_file}").count())
                    .sum::<usize>()
                    != 1
                || config.policies.is_empty()
            {
                return Err("invalid GitOps paths, render command, or service policies".into());
            }
            for (service, policy) in &config.policies {
                if !valid_name(service)
                    || policy.target_branch.trim().is_empty()
                    || policy.validation_tier.trim().is_empty()
                    || policy.validation_tier.chars().any(char::is_control)
                {
                    return Err("invalid GitOps service policy".into());
                }
            }
            if let Some(github) = &config.github {
                if forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository).is_err()
                    || github.api_base.trim().is_empty()
                    || github.token.trim().is_empty()
                    || github.actor.trim().is_empty()
                    || github.commit_author_name.trim().is_empty()
                    || github.commit_author_email.trim().is_empty()
                    || github.actor.chars().any(char::is_control)
                    || github.commit_author_name.chars().any(char::is_control)
                    || github.commit_author_email.chars().any(char::is_control)
                {
                    return Err("invalid GitHub publication configuration".into());
                }
            }
        }
        if let Some(config) = &self.runs {
            if !config.profiles_file.is_absolute()
                || config.max_evidence_age_seconds <= 0
                || Duration::try_seconds(config.max_evidence_age_seconds).is_none()
            {
                return Err("invalid run profiles path or evidence freshness limit".into());
            }
        }
        Ok(())
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/changes", get(list_changes).post(create_change))
        .route("/changes/{id}/validate", post(validate_change))
        .route("/changes/{id}/refresh", post(refresh_change))
        .route("/changes/{id}/open-pr", post(open_pr))
        .route("/runs", get(list_runs).post(create_run))
        .route("/runs/{id}", get(get_run))
}

fn unavailable(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message)
}
fn bad(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}
fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not found")
}

fn change_error(error: thalamus_changes::Error) -> ApiError {
    use thalamus_changes::Error;
    match error {
        Error::Invalid(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        Error::IdempotencyConflict => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency key already used for another request",
        ),
        Error::NotFound => not_found(),
        Error::GitOps => {
            unavailable("GitOps operation failed; inspect the configured repository and renderer")
        }
        Error::Forge => unavailable("forge operation failed; inspect the configured forge"),
        _ => unavailable("change service unavailable"),
    }
}

fn run_error(error: thalamus_runs::Error) -> ApiError {
    use thalamus_runs::Error;
    match error {
        Error::Invalid(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        Error::IdempotencyConflict => ApiError::new(
            StatusCode::CONFLICT,
            "idempotency key already used for another request",
        ),
        Error::NotFound => not_found(),
        _ => unavailable("run service unavailable"),
    }
}

fn validate_idempotency_key(value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(bad("invalid idempotency key"));
    }
    Ok(())
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(value) = values.next() else {
        return Ok(Uuid::new_v4().to_string());
    };
    if values.next().is_some() {
        return Err(bad("duplicate idempotency key"));
    }
    let value = value.to_str().map_err(|_| bad("invalid idempotency key"))?;
    validate_idempotency_key(value)?;
    Ok(value.to_owned())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
        && !name.starts_with('.')
        && !name.ends_with('.')
}

fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|c| c.is_ascii_hexdigit())
}

fn safe_service_file(file: &FsPath) -> bool {
    let mut parts = file.components();
    matches!(parts.next(), Some(Component::Normal(s)) if s == "catalog")
        && matches!(parts.next(), Some(Component::Normal(s)) if s == "services")
        && matches!(parts.next(), Some(Component::Normal(_)))
        && parts.next().is_none()
        && matches!(
            file.extension().and_then(|s| s.to_str()),
            Some("yaml" | "yml")
        )
        && file.to_str().is_some()
}

/// A patch is scoped by the server to the selected service's catalog file.
pub(crate) struct PatchInput {
    pub path: Vec<String>,
    pub operation: PatchOperation,
}

pub(crate) enum PatchOperation {
    Set(Value),
    Remove,
}

fn patches(file: &FsPath, input: Vec<PatchInput>) -> Result<Vec<Patch>, ApiError> {
    if !safe_service_file(file) || input.is_empty() || input.len() > 256 {
        return Err(bad("a nonempty structured service patch is required"));
    }
    let mut result = Vec::with_capacity(input.len());
    for patch in input {
        if patch.path.is_empty()
            || patch.path.len() > 32
            || patch.path[0] == "name"
            || patch
                .path
                .iter()
                .any(|key| key.is_empty() || key.len() > 256 || key.chars().any(char::is_control))
        {
            return Err(bad("invalid patch path; a service name cannot be changed"));
        }
        let operation = match patch.operation {
            PatchOperation::Set(value) => {
                Operation::Set(serde_yaml::to_value(value).map_err(|_| bad("invalid patch value"))?)
            }
            PatchOperation::Remove => Operation::Remove,
        };
        result.push(Patch {
            file: file.to_owned(),
            path: patch.path,
            operation,
        });
    }
    Ok(result)
}

fn change_file(change: &Change) -> Result<PathBuf, ApiError> {
    let first = change
        .request
        .patches
        .first()
        .ok_or_else(|| bad("change has no service patches"))?;
    if change.request.revert_commit.is_some()
        || !safe_service_file(&first.file)
        || change
            .request
            .patches
            .iter()
            .any(|p| p.file != first.file || p.path.is_empty() || p.path[0] == "name")
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "change is outside the service workflow",
        ));
    }
    Ok(first.file.clone())
}

/// Bind the selected service and patch target to the actual pinned Git tree,
/// not only to a possibly newer local catalog or a filename chosen by a client.
fn verify_base_service(
    config: &GitOpsConfig,
    base: &str,
    file: &FsPath,
    service: &str,
) -> Result<(), ApiError> {
    if !valid_sha(base) || !safe_service_file(file) || !valid_name(service) {
        return Err(bad("invalid service, commit, or catalog path"));
    }
    let spec = format!(
        "{}:{}",
        base,
        file.to_str().ok_or_else(|| bad("invalid catalog path"))?
    );
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(&config.repository_dir)
        .arg("show")
        .arg(spec)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .map_err(|_| unavailable("cannot inspect the configured GitOps repository"))?;
    if !output.status.success() {
        return Err(unavailable(
            "service file or base commit is unavailable in the GitOps repository",
        ));
    }
    let document: serde_yaml::Value = serde_yaml::from_slice(&output.stdout)
        .map_err(|_| unavailable("service file at the base commit is invalid"))?;
    if document.get("name").and_then(serde_yaml::Value::as_str) != Some(service) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "service identity at the base commit does not match the selected service",
        ));
    }
    Ok(())
}

fn preview_config(config: &GitOpsConfig, file: &FsPath) -> Result<PreviewConfig, ApiError> {
    if !safe_service_file(file) {
        return Err(bad("invalid service path"));
    }
    let file = file.to_str().ok_or_else(|| bad("invalid service path"))?;
    Ok(PreviewConfig {
        worktree_root: config.worktree_root.clone(),
        render: RenderCommand {
            executable: config.render_executable.clone().into(),
            args: config
                .render_args
                .iter()
                .map(|arg| arg.replace("{service_file}", file).into())
                .collect(),
        },
    })
}

fn gitops(state: &AppState) -> Result<&GitOpsConfig, ApiError> {
    state
        .config
        .workflow
        .as_ref()
        .and_then(|c| c.gitops.as_ref())
        .ok_or_else(|| unavailable("GitOps workflow is not configured"))
}

fn catalog_service_file(state: &AppState, name: &str) -> Result<PathBuf, ApiError> {
    if !valid_name(name) {
        return Err(bad("invalid service name"));
    }
    let services = crate::catalog::load_services(&state.config.catalog_dir)
        .map_err(|_| unavailable("catalog is unavailable"))?;
    let service = services
        .iter()
        .find(|service| service.name == name)
        .ok_or_else(not_found)?;
    let root = std::fs::canonicalize(&state.config.catalog_dir)
        .map_err(|_| unavailable("catalog is unavailable"))?;
    let source = std::fs::canonicalize(&service.source)
        .map_err(|_| unavailable("catalog service file is unavailable"))?;
    if source.parent() != Some(root.join("services").as_path()) {
        return Err(unavailable(
            "catalog service path is outside the services directory",
        ));
    }
    let file = FsPath::new("catalog/services").join(
        source
            .file_name()
            .ok_or_else(|| unavailable("invalid catalog service path"))?,
    );
    if !safe_service_file(&file) {
        return Err(unavailable("invalid catalog service path"));
    }
    Ok(file)
}

/// The caller authenticates and authorizes the inspection. Both revisions
/// must contain the selected service at the same catalog path.
pub(crate) fn comparison_config(
    state: &AppState,
    service: &str,
    from: &str,
    to: &str,
) -> Result<(GitOpsRepo, PreviewConfig), ApiError> {
    let config = gitops(state)?;
    let file = catalog_service_file(state, service)?;
    let repo = GitOpsRepo::open(&config.repository_dir)
        .map_err(|_| unavailable("GitOps repository is unavailable"))?;
    repo.fetch()
        .map_err(|_| unavailable("GitOps repository could not be refreshed"))?;
    verify_base_service(config, from, &file, service)?;
    verify_base_service(config, to, &file, service)?;
    Ok((repo, preview_config(config, &file)?))
}

fn policy<'a>(config: &'a GitOpsConfig, service: &str) -> Result<&'a ServicePolicy, ApiError> {
    config
        .policies
        .get(service)
        .ok_or_else(|| unavailable("no reviewed GitOps policy is configured for this service"))
}

fn check_current_policy(change: &Change, config: &GitOpsConfig) -> Result<(), ApiError> {
    let policy = policy(config, &change.request.service)?;
    if change.request.target_branch != policy.target_branch
        || change.request.validation_tier != policy.validation_tier
        || serde_json::to_value(&change.request.rollout).ok()
            != serde_json::to_value(&policy.rollout).ok()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "service policy changed; create and validate a new proposal",
        ));
    }
    Ok(())
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ChangeView {
    pub id: Uuid,
    pub service: String,
    pub state: String,
    pub base_commit: String,
}
impl From<Change> for ChangeView {
    fn from(value: Change) -> Self {
        Self {
            id: value.id,
            service: value.request.service,
            state: value.state,
            base_commit: value.request.base_sha,
        }
    }
}
#[derive(Serialize, ToSchema)]
pub(crate) struct ChangesView {
    changes: Vec<ChangeView>,
}

#[utoipa::path(get, path = "/api/v1/changes", responses((status = 200, body = ChangesView), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 503, description = "Changes unavailable")))]
pub(crate) async fn list_changes(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<ChangesView>, ApiError> {
    let mut guard = state
        .identity
        .begin_platform_admin(caller.principal())
        .await?;
    let changes = thalamus_changes::Store::new(state.db.clone())
        .list_in_transaction(guard.transaction(), 100)
        .await
        .map_err(change_error)?;
    guard.commit().await?;
    Ok(Json(ChangesView {
        changes: changes.into_iter().map(Into::into).collect(),
    }))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateChange {
    service: String,
    base_commit: String,
    patch: Map<String, Value>,
}

#[utoipa::path(post, path = "/api/v1/changes", params(("idempotency-key" = Option<String>, Header)), request_body = CreateChange, responses((status = 200, body = ChangeView), (status = 400, description = "Invalid request"), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 409, description = "Change conflict"), (status = 503, description = "Changes unavailable")))]
pub(crate) async fn create_change(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    headers: HeaderMap,
    Json(input): Json<CreateChange>,
) -> Result<Json<ChangeView>, ApiError> {
    let key = idempotency_key(&headers)?;
    let patch = input
        .patch
        .into_iter()
        .map(|(field, value)| PatchInput {
            path: vec![field],
            operation: PatchOperation::Set(value),
        })
        .collect();
    let change = propose_for(
        &state,
        caller.principal(),
        input.service,
        input.base_commit,
        patch,
        &key,
        None,
    )
    .await?;
    Ok(Json(change))
}

/// Shared browser/MCP admission. The user lock, authorization, durable proposal
/// and identity audit are committed in the same transaction.
pub(crate) async fn propose_for(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
    service_name: String,
    base_commit: String,
    input: Vec<PatchInput>,
    key: &str,
    agent: Option<&str>,
) -> Result<ChangeView, ApiError> {
    let mut guard = state.identity.begin_platform_admin(principal).await?;
    validate_idempotency_key(key)?;
    if !valid_name(&service_name) || !valid_sha(&base_commit) {
        return Err(bad("service and full hexadecimal base commit are required"));
    }
    let base_commit = base_commit.to_ascii_lowercase();
    let config = gitops(state)?;
    let policy = policy(config, &service_name)?;
    let file = catalog_service_file(state, &service_name)?;
    let patch = patches(&file, input)?;
    let repo = GitOpsRepo::open(&config.repository_dir)
        .map_err(|_| unavailable("GitOps repository is unavailable"))?;
    repo.fetch()
        .map_err(|_| unavailable("GitOps repository could not be refreshed"))?;
    verify_base_service(config, &base_commit, &file, &service_name)?;
    let proposal = Proposal {
        service: service_name.clone(),
        base_sha: base_commit,
        target_branch: policy.target_branch.clone(),
        title: service_name,
        patches: patch,
        validation_tier: policy.validation_tier.clone(),
        rollout: policy.rollout.clone(),
        revert_commit: None,
    };
    let actor = guard.actor_id().to_string();
    let change = thalamus_changes::Store::new(state.db.clone())
        .propose_in_transaction(guard.transaction(), &actor, key, proposal)
        .await
        .map_err(change_error)?;
    guard
        .audit(agent, "change.propose", &change.id.to_string())
        .await?;
    guard.commit().await?;
    Ok(change.into())
}

/// Refresh invalidates the previous validation; the new base must be reviewed
/// and explicitly validated before another publication.
#[utoipa::path(post, path = "/api/v1/changes/{id}/refresh", params(("id" = Uuid, Path)), responses((status = 200, body = ChangeView), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 404, description = "Change not found"), (status = 409, description = "Change conflict"), (status = 503, description = "Changes unavailable")))]
pub(crate) async fn refresh_change(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<ChangeView>, ApiError> {
    let mut guard = state
        .identity
        .begin_platform_admin(caller.principal())
        .await?;
    let store = thalamus_changes::Store::new(state.db.clone());
    let change = store
        .get_in_transaction(guard.transaction(), id)
        .await
        .map_err(change_error)?;
    if change.pr_number.is_some() {
        return Err(ApiError::new(StatusCode::CONFLICT, "cannot refresh an open pull request without safely invalidating its existing approvals"));
    }
    let config = gitops(&state)?;
    check_current_policy(&change, config)?;
    let file = change_file(&change)?;
    let repo = GitOpsRepo::open(&config.repository_dir)
        .map_err(|_| unavailable("GitOps repository is unavailable"))?;
    repo.fetch()
        .map_err(|_| unavailable("GitOps repository could not be refreshed"))?;
    let base = repo
        .resolve_base(&change.request.target_branch)
        .map_err(|_| unavailable("target branch is unavailable"))?;
    verify_base_service(config, &base, &file, &change.request.service)?;
    let preview = preview_config(config, &file)?;
    let actor = guard.actor_id().to_string();
    let result = store
        .refresh_in_transaction(guard.transaction(), id, &actor, &repo, &preview, &base)
        .await;
    // A failed render can still invalidate a previously validated preview.
    guard.audit(None, "change.refresh", &id.to_string()).await?;
    guard.commit().await?;
    Ok(Json(result.map_err(change_error)?.into()))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ValidationView {
    data_diff: String,
    rendered_diff: String,
    errors: Vec<String>,
}
#[utoipa::path(post, path = "/api/v1/changes/{id}/validate", params(("id" = Uuid, Path)), responses((status = 200, body = ValidationView), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 404, description = "Change not found"), (status = 409, description = "Change conflict"), (status = 503, description = "Validation unavailable")))]
pub(crate) async fn validate_change(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<ValidationView>, ApiError> {
    let mut guard = state
        .identity
        .begin_platform_admin(caller.principal())
        .await?;
    let store = thalamus_changes::Store::new(state.db.clone());
    let change = store
        .get_in_transaction(guard.transaction(), id)
        .await
        .map_err(change_error)?;
    let config = gitops(&state)?;
    check_current_policy(&change, config)?;
    let file = change_file(&change)?;
    verify_base_service(
        config,
        &change.request.base_sha,
        &file,
        &change.request.service,
    )?;
    let repo = GitOpsRepo::open(&config.repository_dir)
        .map_err(|_| unavailable("GitOps repository is unavailable"))?;
    let preview = preview_config(config, &file)?;
    let actor = guard.actor_id().to_string();
    let result = store
        .validate_in_transaction(guard.transaction(), id, &actor, &repo, &preview)
        .await;
    // A render failure invalidates old validation and must be committed too.
    guard
        .audit(None, "change.validate", &id.to_string())
        .await?;
    guard.commit().await?;
    let validated = result.map_err(change_error)?;
    let data_diff = validated
        .data_diff
        .ok_or_else(|| unavailable("validated data diff is unavailable"))?;
    let rendered_diff = validated
        .rendered_diff
        .ok_or_else(|| unavailable("validated render diff is unavailable"))?;
    Ok(Json(ValidationView {
        data_diff,
        rendered_diff,
        errors: Vec::new(),
    }))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PrView {
    url: String,
}
#[utoipa::path(post, path = "/api/v1/changes/{id}/open-pr", params(("id" = Uuid, Path)), responses((status = 200, body = PrView), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 404, description = "Change not found"), (status = 409, description = "Change conflict"), (status = 503, description = "Publication unavailable")))]
pub(crate) async fn open_pr(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<PrView>, ApiError> {
    let mut guard = state
        .identity
        .begin_platform_admin(caller.principal())
        .await?;
    let store = thalamus_changes::Store::new(state.db.clone());
    let change = store
        .get_in_transaction(guard.transaction(), id)
        .await
        .map_err(change_error)?;
    let config = gitops(&state)?;
    check_current_policy(&change, config)?;
    let file = change_file(&change)?;
    verify_base_service(
        config,
        &change.request.base_sha,
        &file,
        &change.request.service,
    )?;
    let github = config
        .github
        .as_ref()
        .ok_or_else(|| unavailable("GitHub publication is not configured"))?;
    let repository = forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository)
        .map_err(|_| unavailable("invalid GitHub repository configuration"))?;
    let forge =
        forge::github::Github::new(repository.clone(), &github.api_base, github.token.clone())
            .map_err(|_| unavailable("invalid GitHub publication configuration"))?;
    let repo = GitOpsRepo::open(&config.repository_dir)
        .map_err(|_| unavailable("GitOps repository is unavailable"))?;
    let preview = preview_config(config, &file)?;
    let worker = WorkerConfig {
        worktree_root: preview.worktree_root,
        render: preview.render,
        commit_author: CommitAuthor {
            name: github.commit_author_name.clone(),
            email: github.commit_author_email.clone(),
        },
        repository,
        forge_actor: github.actor.clone(),
    };
    let actor = guard.actor_id().to_string();
    let published = store
        .publish_in_transaction(guard.transaction(), id, &actor, &repo, &forge, &worker)
        .await
        .map_err(change_error)?;
    guard.audit(None, "change.publish", &id.to_string()).await?;
    guard.commit().await?;
    store
        .comment_rendered_diff(id, &forge, &github.actor)
        .await
        .map_err(change_error)?;
    let url = published
        .pr_url
        .ok_or_else(|| unavailable("forge did not return a pull request URL"))?;
    Ok(Json(PrView { url }))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RunView {
    pub id: Uuid,
    pub service: String,
    pub deployment: String,
    pub kind: &'static str,
    pub state: String,
    pub revision: String,
}
impl From<Run> for RunView {
    fn from(run: Run) -> Self {
        let kind = match run.request.kind {
            RunKind::Validation => "validation",
            RunKind::Benchmark => "benchmark",
            RunKind::Debug => "debug",
        };
        Self {
            id: run.id,
            service: run.request.service,
            deployment: run.request.deployment,
            kind,
            state: run.state,
            revision: run.request.evidence.desired_configuration,
        }
    }
}
#[derive(Serialize, ToSchema)]
pub(crate) struct RunsView {
    runs: Vec<RunView>,
}

#[utoipa::path(get, path = "/api/v1/runs", responses((status = 200, body = RunsView), (status = 401, description = "Authentication required"), (status = 503, description = "Runs unavailable")))]
pub(crate) async fn list_runs(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<RunsView>, ApiError> {
    let teams = state
        .identity
        .list_teams_principal(caller.principal())
        .await?;
    let store = thalamus_runs::Store::new(state.db.clone());
    let mut runs = Vec::new();
    for team in teams {
        let mut guard = state
            .identity
            .begin_authorized(caller.principal(), team.id, false)
            .await?;
        let team_runs = store
            .list_in_transaction(guard.transaction(), &team.id.to_string(), 100)
            .await
            .map_err(run_error)?;
        guard.commit().await?;
        runs.extend(team_runs);
    }
    runs.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    runs.truncate(100);
    Ok(Json(RunsView {
        runs: runs.into_iter().map(Into::into).collect(),
    }))
}

#[utoipa::path(get, path = "/api/v1/runs/{id}", params(("id" = Uuid, Path)), responses((status = 200, body = RunView), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Run not found"), (status = 503, description = "Runs unavailable")))]
pub(crate) async fn get_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<RunView>, ApiError> {
    Ok(Json(get_run_for(&state, caller.principal(), id).await?))
}

fn copy_principal<'a>(principal: &VerifiedPrincipal<'a>) -> VerifiedPrincipal<'a> {
    match principal {
        VerifiedPrincipal::Session(token) => VerifiedPrincipal::Session(token),
        VerifiedPrincipal::Oidc { issuer, subject } => VerifiedPrincipal::Oidc { issuer, subject },
    }
}

/// Retrieve a run only while the current identity is authorized for its team.
pub(crate) async fn get_run_for(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
    id: Uuid,
) -> Result<RunView, ApiError> {
    state
        .identity
        .authenticate_principal(copy_principal(&principal))
        .await?;
    let team: String = sqlx::query_scalar("SELECT team FROM thalamus_runs WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let team_id =
        Uuid::parse_str(&team).map_err(|_| unavailable("run has an invalid team identity"))?;
    let mut guard = state
        .identity
        .begin_authorized(principal, team_id, false)
        .await
        .map_err(|error| match error {
            thalamus_identity::Error::Forbidden => not_found(),
            other => other.into(),
        })?;
    let run = thalamus_runs::Store::new(state.db.clone())
        .get_in_transaction(guard.transaction(), &team, id)
        .await
        .map_err(run_error)?;
    guard.commit().await?;
    Ok(run.into())
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateRun {
    pub service: String,
    pub deployment: String,
    pub kind: RunKind,
    /// Required for admission; a missing or nil project ID is rejected.
    #[schema(required)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub expected_runner: Option<String>,
    #[serde(default)]
    pub expected_workload: Option<Workload>,
}

fn run_project(input: &CreateRun) -> Result<Uuid, ApiError> {
    if !valid_name(&input.service)
        || !valid_name(&input.deployment)
        || matches!(&input.kind, RunKind::Debug)
    {
        return Err(bad("invalid service, deployment, or run kind"));
    }
    input
        .project_id
        .filter(|id| !id.is_nil())
        .ok_or_else(|| bad("project_id is required"))
}

fn evidence_fresh(observed: DateTime<Utc>, now: DateTime<Utc>, maximum_seconds: i64) -> bool {
    maximum_seconds > 0
        && observed <= now
        && Duration::try_seconds(maximum_seconds)
            .is_some_and(|limit| now.signed_duration_since(observed) <= limit)
}

fn read_profiles(path: &FsPath) -> Result<Vec<RunProfile>, ApiError> {
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    let file = std::fs::File::open(path)
        .map_err(|_| unavailable("trusted run evidence is unavailable"))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable("trusted run evidence is unavailable"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(unavailable("trusted run evidence is too large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| unavailable("trusted run evidence is invalid"))
}

#[utoipa::path(post, path = "/api/v1/runs", params(("idempotency-key" = Option<String>, Header)), request_body = CreateRun, responses((status = 200, body = RunView), (status = 400, description = "Invalid request"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Project not found"), (status = 409, description = "Run conflict"), (status = 503, description = "Runs unavailable")))]
pub(crate) async fn create_run(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    headers: HeaderMap,
    Json(input): Json<CreateRun>,
) -> Result<Json<RunView>, ApiError> {
    let key = idempotency_key(&headers)?;
    Ok(Json(
        create_run_for(&state, caller.principal(), input, &key, None).await?,
    ))
}

/// Shared admission for browser and verified MCP principals. The workload and
/// runtime identity are sourced only from an operator-owned evidence profile.
pub(crate) async fn create_run_for(
    state: &AppState,
    principal: VerifiedPrincipal<'_>,
    input: CreateRun,
    key: &str,
    agent: Option<&str>,
) -> Result<RunView, ApiError> {
    state
        .identity
        .authenticate_principal(copy_principal(&principal))
        .await?;
    validate_idempotency_key(key)?;
    let project = run_project(&input)?;
    let team: Uuid = sqlx::query_scalar("SELECT team_id FROM identity_projects WHERE id=$1")
        .bind(project)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let mut guard = state
        .identity
        .begin_authorized(principal, team, false)
        .await?;
    let project_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM identity_projects WHERE id=$1 AND team_id=$2)",
    )
    .bind(project)
    .bind(team)
    .fetch_one(&mut **guard.transaction())
    .await?;
    if !project_exists {
        return Err(not_found());
    }
    let config = state.config.workflow.as_ref().and_then(|c| c.runs.as_ref())
        .ok_or_else(|| unavailable("run creation is unavailable: trusted runtime evidence and worker configuration are not configured"))?;
    let services = crate::catalog::load_services(&state.config.catalog_dir)
        .map_err(|_| unavailable("catalog is unavailable"))?;
    let service = services
        .iter()
        .find(|s| s.name == input.service)
        .ok_or_else(not_found)?;
    if !service
        .deployments
        .iter()
        .any(|d| d.routing.deployment == input.deployment)
    {
        return Err(not_found());
    }
    let profiles = read_profiles(&config.profiles_file)?;
    let mut matching = profiles.into_iter().filter(|p| {
        p.team_id == team
            && p.request.project_id == project
            && p.request.service == input.service
            && p.request.deployment == input.deployment
            && p.request.kind == input.kind
    });
    let profile = matching.next().ok_or_else(|| {
        unavailable("no trusted run profile and runtime evidence for this project and deployment")
    })?;
    if matching.next().is_some() {
        return Err(unavailable("run profile is ambiguous"));
    }
    if !evidence_fresh(
        profile.request.evidence.conditions.observed_at,
        Utc::now(),
        config.max_evidence_age_seconds,
    ) {
        return Err(unavailable(
            "runtime evidence is stale or has a future observation time",
        ));
    }
    profile
        .request
        .validate()
        .map_err(|_| unavailable("trusted run profile is invalid"))?;
    if input
        .expected_runner
        .as_ref()
        .is_some_and(|runner| runner != &profile.request.runner)
        || input
            .expected_workload
            .as_ref()
            .is_some_and(|workload| workload != &profile.request.workload)
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "runner or workload does not match the trusted run profile",
        ));
    }
    let actor = guard.actor_id().to_string();
    let run = thalamus_runs::Store::new(state.db.clone())
        .create_in_transaction(
            guard.transaction(),
            &team.to_string(),
            &actor,
            key,
            &profile.request,
        )
        .await
        .map_err(run_error)?;
    guard
        .audit(agent, "run.create", &run.id.to_string())
        .await?;
    guard.commit().await?;
    Ok(run.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_fields_are_scoped_and_names_cannot_be_changed() {
        let patch = vec![
            PatchInput {
                path: vec!["deployments".into()],
                operation: PatchOperation::Set(serde_json::json!([{"name":"d"}])),
            },
            PatchInput {
                path: vec!["gateway".into()],
                operation: PatchOperation::Set(Value::Null),
            },
        ];
        let result = patches(FsPath::new("catalog/services/a.yaml"), patch).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result
            .iter()
            .all(|p| p.path.len() == 1 && safe_service_file(&p.file)));
        assert!(matches!(
            &result
                .iter()
                .find(|p| p.path[0] == "gateway")
                .unwrap()
                .operation,
            Operation::Set(serde_yaml::Value::Null)
        ));
        assert!(patches(
            FsPath::new("catalog/services/a.yaml"),
            vec![PatchInput {
                path: vec!["name".into()],
                operation: PatchOperation::Set(serde_json::json!("other"))
            }]
        )
        .is_err());
        assert!(patches(FsPath::new("catalog/services/../../other.yaml"), vec![]).is_err());
        assert!(patches(
            FsPath::new("catalog/services/a.yaml"),
            vec![PatchInput {
                path: vec!["deployments".into(), "0".into(), "image".into()],
                operation: PatchOperation::Remove
            }]
        )
        .is_ok());
    }

    #[test]
    fn identifiers_and_evidence_fail_closed() {
        assert!(valid_sha(&"a".repeat(40)));
        assert!(!valid_sha("main"));
        assert!(!valid_name("../service"));
        assert!(!safe_service_file(FsPath::new(
            "catalog/services/../secret.yaml"
        )));
        let now = Utc::now();
        assert!(evidence_fresh(now - Duration::seconds(5), now, 5));
        assert!(!evidence_fresh(now - Duration::seconds(6), now, 5));
        assert!(!evidence_fresh(now + Duration::seconds(1), now, 5));
        assert!(!evidence_fresh(now, now, 0));
    }

    #[test]
    fn run_input_requires_scoped_project_and_supported_kind() {
        let project = Uuid::new_v4();
        let mut input: CreateRun = serde_json::from_value(serde_json::json!({
            "service": "svc", "deployment": "dep", "kind": "validation", "project_id": project
        }))
        .unwrap();
        assert_eq!(run_project(&input).unwrap(), project);
        input.project_id = None;
        assert!(run_project(&input).is_err());
        input.project_id = Some(Uuid::nil());
        assert!(run_project(&input).is_err());
        input.project_id = Some(project);
        input.kind = RunKind::Debug;
        assert!(run_project(&input).is_err());
        assert!(serde_json::from_value::<CreateRun>(serde_json::json!({
            "service": "svc", "deployment": "dep", "kind": "other", "project_id": project
        }))
        .is_err());
    }

    #[test]
    fn config_requires_explicit_argv_and_absolute_evidence_path() {
        let value = serde_json::json!({"gitops": {"repository_dir":"/git/repo", "worktree_root":"/git/worktrees", "render_executable":"kcl", "render_args":["main.k", "-D", "service_file={service_file}"], "policies":{"svc":{"target_branch":"main", "validation_tier":"expected", "rollout":{"mode":"rolling"}}}}});
        let mut config: WorkflowConfig = serde_json::from_value(value).unwrap();
        assert!(config.validate().is_ok());
        config.gitops.as_mut().unwrap().render_args.clear();
        assert!(config.validate().is_err());
        let config: WorkflowConfig = serde_json::from_value(serde_json::json!({"runs":{"profiles_file":"relative.json", "max_evidence_age_seconds":60}})).unwrap();
        assert!(config.validate().is_err());
    }
}
