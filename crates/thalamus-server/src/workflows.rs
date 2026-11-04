//! Authenticated change and run endpoints. Catalog-wide changes are restricted to
//! platform administrators until the catalog supplies authoritative service owners.
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Component, Path as FsPath, PathBuf},
    process::Command,
    sync::{Arc, OnceLock},
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
use thalamus_runs::{Run, RunKind, Workload};
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
pub(crate) struct GitOpsConfig {
    pub(crate) repository_dir: PathBuf,
    pub(crate) worktree_root: PathBuf,
    #[serde(default = "default_render_script_path")]
    pub(crate) render_script_path: PathBuf,
    #[serde(default = "default_workload_namespace")]
    pub(crate) workload_namespace: String,
    /// A JSON Schema generated from the KCL Service schema, committed alongside
    /// the catalog. The form reads it at the same base revision as the service.
    #[serde(default)]
    pub(crate) service_schema_path: Option<PathBuf>,
    pub(crate) policies: BTreeMap<String, ServicePolicy>,
    #[serde(default)]
    pub(crate) github: Option<GitHubConfig>,
}

fn default_render_script_path() -> PathBuf {
    "/usr/local/lib/thalamus/render-catalog.py".into()
}

fn valid_render_script_path(script: &FsPath, repository: &FsPath, worktrees: &FsPath) -> bool {
    script.is_absolute()
        && !script.as_os_str().as_encoded_bytes().contains(&0)
        && script
            .components()
            .all(|component| component != Component::ParentDir)
        && !script.starts_with(repository)
        && !script.starts_with(worktrees)
}

fn default_workload_namespace() -> String {
    "thalamus-inference".into()
}

fn valid_workload_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace.len() <= 63
        && namespace
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !namespace.starts_with('-')
        && !namespace.ends_with('-')
}

fn catalog_render_command(
    script_path: &FsPath,
    repository: &FsPath,
    worktrees: &FsPath,
    namespace: &str,
) -> Result<RenderCommand, &'static str> {
    let script = std::fs::canonicalize(script_path)
        .map_err(|_| "configured catalog render script is unavailable")?;
    if !std::fs::metadata(&script)
        .map_err(|_| "configured catalog render script is unavailable")?
        .is_file()
    {
        return Err("configured catalog render script is not a regular file");
    }
    let repository = std::fs::canonicalize(repository)
        .map_err(|_| "configured GitOps repository is unavailable")?;
    let worktrees = std::fs::canonicalize(worktrees)
        .map_err(|_| "configured GitOps worktree root is unavailable")?;
    if script.starts_with(&repository) || script.starts_with(&worktrees) {
        return Err(
            "catalog render script must be outside the GitOps repository and worktree root",
        );
    }
    Ok(RenderCommand {
        executable: "python3".into(),
        args: vec![
            "-I".into(),
            script.into_os_string(),
            "--write".into(),
            "--workload-namespace".into(),
            namespace.into(),
        ],
    })
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServicePolicy {
    pub(crate) target_branch: String,
    pub(crate) validation_tier: String,
    pub(crate) rollout: Rollout,
    /// Complete operator-owned policy shared verbatim with the worker and CI
    /// guard. Its digest binds approvals to all scheduling and forge settings.
    #[serde(default)]
    pub(crate) controller: Option<thalamus_changes::RolloutPolicy>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitHubConfig {
    pub(crate) repository: String,
    pub(crate) api_base: String,
    pub(crate) app_id: u64,
    pub(crate) installation_id: u64,
    pub(crate) private_key_file: PathBuf,
    /// The GitHub user ID of this App's bot, used to authenticate commit
    /// status authors independently of the branch rule's App-ID pin.
    pub(crate) ci_bot_user_id: u64,
    pub(crate) actor: String,
    pub(crate) commit_author_name: String,
    pub(crate) commit_author_email: String,
    #[serde(default)]
    pub(crate) webhook_secret: Option<String>,
    #[serde(skip)]
    client: Arc<OnceLock<Arc<forge::github::Github>>>,
}
/// Reuse one GitHub App client for the life of the configuration. Octocrab
/// refreshes its installation token automatically before expiry.
pub(crate) fn github_client(config: &GitHubConfig) -> Result<Arc<forge::github::Github>, ApiError> {
    if let Some(client) = config.client.get() {
        return Ok(client.clone());
    }
    let repo = forge::RepoRef::parse(forge::ForgeKind::Github, &config.repository)
        .map_err(|_| unavailable("invalid GitHub repository configuration"))?;
    let key = std::fs::read(&config.private_key_file)
        .map_err(|_| unavailable("GitHub App private key is unavailable"))?;
    let client = forge::github::Github::new_app(
        repo,
        &config.api_base,
        config.app_id,
        config.installation_id,
        &key,
    )
    .map_err(|_| unavailable("invalid GitHub App configuration"))?;
    Ok(config.client.get_or_init(|| Arc::new(client)).clone())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfig {
    /// Profiles specify only operator-approved workload and runner configuration.
    /// Execution evidence is independently observed at admission and by the worker.
    profiles_file: PathBuf,
    max_evidence_age_seconds: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunProfile {
    team_id: Uuid,
    request: crate::workflow_evidence::ProfileRequest,
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
    /// The required-status refresher is needed only for scheduled in-place
    /// services. Runs-only and ordinary preview installations need no timer.
    pub fn rollout_ci_enabled(&self) -> bool {
        self.gitops.as_ref().is_some_and(|config| {
            config
                .github
                .as_ref()
                .is_some_and(|github| github.webhook_secret.is_some())
                && config.policies.values().any(|policy| {
                    policy.controller.as_ref().is_some_and(|controller| {
                        controller.mode == thalamus_changes::RolloutMode::InPlace
                    })
                })
        })
    }

    fn validate(&self) -> Result<(), String> {
        if self.gitops.is_none() && self.runs.is_none() {
            return Err("THALAMUS_WORKFLOWS_CONFIG must configure gitops or runs".into());
        }
        if let Some(config) = &self.gitops {
            if !config.repository_dir.is_absolute()
                || !config.worktree_root.is_absolute()
                || !valid_render_script_path(
                    &config.render_script_path,
                    &config.repository_dir,
                    &config.worktree_root,
                )
                || !valid_workload_namespace(&config.workload_namespace)
                || config.service_schema_path.as_ref().is_some_and(|path| {
                    path.as_os_str().is_empty()
                        || !path
                            .components()
                            .all(|part| matches!(part, Component::Normal(_)))
                })
                || config.policies.is_empty()
            {
                return Err("invalid GitOps paths, workload namespace, or service policies".into());
            }
            for (service, policy) in &config.policies {
                if !valid_name(service)
                    || policy.target_branch.trim().is_empty()
                    || policy.validation_tier.trim().is_empty()
                    || policy.validation_tier.chars().any(char::is_control)
                {
                    return Err("invalid GitOps service policy".into());
                }
                if matches!(&policy.rollout, Rollout::Gated | Rollout::InPlace { .. })
                    && policy.controller.is_none()
                {
                    return Err("gated and in-place services require a rollout controller".into());
                }
                if let Some(controller) = &policy.controller {
                    if self.runs.is_none() {
                        return Err("rollout controller requires trusted run profiles".into());
                    }
                    let repository = config
                        .github
                        .as_ref()
                        .and_then(|github| {
                            forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository).ok()
                        })
                        .ok_or_else(|| {
                            "rollout controller requires a configured GitHub repository".to_owned()
                        })?;
                    let mode = match &policy.rollout {
                        Rollout::Gated => thalamus_changes::RolloutMode::Gated,
                        Rollout::Rolling => thalamus_changes::RolloutMode::Rolling,
                        Rollout::InPlace { .. } => thalamus_changes::RolloutMode::InPlace,
                    };
                    if controller.validate().is_err()
                        || controller.service != *service
                        || controller.repository != repository
                        || controller.target_branch != policy.target_branch
                        || controller.validation_tier != policy.validation_tier
                        || controller.mode != mode
                    {
                        return Err(
                            "rollout controller does not match GitOps service policy".into()
                        );
                    }
                    if mode == thalamus_changes::RolloutMode::InPlace {
                        let github = config
                            .github
                            .as_ref()
                            .expect("repository was checked above");
                        if !controller.trust_merge_group_ci
                            || github
                                .webhook_secret
                                .as_ref()
                                .is_none_or(|secret| secret.len() < 32)
                            || !controller.required_checks.iter().any(|check| {
                                check.context == "thalamus/rollout-window"
                                    && check.app_id == github.app_id
                                    && check.status_creator_id == Some(github.ci_bot_user_id)
                            })
                        {
                            return Err("in-place rollout requires the signed webhook and App-pinned rollout-window status".into());
                        }
                    }
                }
            }
            if let Some(github) = &config.github {
                if forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository).is_err()
                    || github.api_base.trim().is_empty()
                    || github.app_id == 0
                    || github.installation_id == 0
                    || github.ci_bot_user_id == 0
                    || !github.private_key_file.is_absolute()
                    || github.actor.trim().is_empty()
                    || github.commit_author_name.trim().is_empty()
                    || github.commit_author_email.trim().is_empty()
                    || github.actor.chars().any(char::is_control)
                    || github.commit_author_name.chars().any(char::is_control)
                    || github
                        .webhook_secret
                        .as_ref()
                        .is_some_and(|secret| secret.len() < 32)
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
        .route("/runs/{id}/promote", post(crate::run_promotion::promote))
}

pub(crate) fn unavailable(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message)
}
pub(crate) fn bad(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}
fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not found")
}

pub(crate) fn change_error(error: thalamus_changes::Error) -> ApiError {
    use thalamus_changes::Error;
    match error {
        Error::Invalid(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        Error::Conflict(message) => ApiError::new(StatusCode::CONFLICT, message),
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

pub(crate) fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
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

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
        && !name.starts_with('.')
        && !name.ends_with('.')
}

pub(crate) fn valid_sha(sha: &str) -> bool {
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

/// The service patch and optional catalog artifact form one atomic change.
pub(crate) struct ChangePatches {
    pub service: Vec<PatchInput>,
    pub new_artifact: Option<NewArtifact>,
}

pub(crate) enum PatchOperation {
    Set(Value),
    Remove,
}

pub(crate) fn patches(file: &FsPath, input: Vec<PatchInput>) -> Result<Vec<Patch>, ApiError> {
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

pub(crate) fn change_file(change: &Change) -> Result<PathBuf, ApiError> {
    let first = change
        .request
        .patches
        .iter()
        .find(|patch| safe_service_file(&patch.file))
        .ok_or_else(|| bad("change has no service patches"))?;
    if change.request.revert_commit.is_some() || !safe_service_file(&first.file) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "change is outside the service workflow",
        ));
    }
    let mut artifacts = 0;
    for patch in &change.request.patches {
        if patch.file == first.file {
            if patch.path.is_empty() || patch.path[0] == "name" {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "change is outside the service workflow",
                ));
            }
        } else if patch.file == FsPath::new("catalog/artifacts.yaml") {
            artifacts += 1;
            let valid = patch.path.len() == 1
                && matches!(&patch.operation, Operation::Set(value)
                    if artifact_from_yaml(&patch.path[0], value).is_some());
            if artifacts > 1 || !valid {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "invalid artifact catalog patch",
                ));
            }
        } else {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "change is outside the service workflow",
            ));
        }
    }
    Ok(first.file.clone())
}

/// Bind the selected service and patch target to the actual pinned Git tree,
/// not only to a possibly newer local catalog or a filename chosen by a client.
pub(crate) fn verify_base_service(
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

pub(crate) fn preview_config(
    config: &GitOpsConfig,
    file: &FsPath,
) -> Result<PreviewConfig, ApiError> {
    if !safe_service_file(file) {
        return Err(bad("invalid service path"));
    }
    Ok(PreviewConfig {
        worktree_root: config.worktree_root.clone(),
        render: catalog_render_command(
            &config.render_script_path,
            &config.repository_dir,
            &config.worktree_root,
            &config.workload_namespace,
        )
        .map_err(unavailable)?,
    })
}
pub(crate) fn worker_config(
    config: &GitOpsConfig,
    file: &FsPath,
) -> Result<WorkerConfig, ApiError> {
    let github = config
        .github
        .as_ref()
        .ok_or_else(|| unavailable("GitHub publication is not configured"))?;
    let preview = preview_config(config, file)?;
    let repository = forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository)
        .map_err(|_| unavailable("invalid GitHub repository configuration"))?;
    Ok(WorkerConfig {
        worktree_root: preview.worktree_root,
        render: preview.render,
        commit_author: CommitAuthor {
            name: github.commit_author_name.clone(),
            email: github.commit_author_email.clone(),
        },
        repository,
        forge_actor: github.actor.clone(),
    })
}

pub(crate) fn gitops(state: &AppState) -> Result<&GitOpsConfig, ApiError> {
    state
        .config
        .workflow
        .as_ref()
        .and_then(|c| c.gitops.as_ref())
        .ok_or_else(|| unavailable("GitOps workflow is not configured"))
}
/// Open only the operator-configured repository, with both remote directions
/// bound to the configured forge identity and its selected service policy.
pub(crate) fn open_repo(config: &GitOpsConfig, branch: &str) -> Result<GitOpsRepo, ApiError> {
    let github = config
        .github
        .as_ref()
        .ok_or_else(|| unavailable("GitOps forge repository is not configured"))?;
    GitOpsRepo::open(&config.repository_dir)
        .and_then(|repo| repo.bind_origin(&github.repository, branch))
        .map_err(|_| unavailable("GitOps origin does not match the configured forge repository"))
}

/// Resolve the service file from a commit, not the mutable or separately
/// refreshed catalog checkout. Duplicate names and non-regular blobs fail closed.
pub(crate) fn catalog_service_file(
    config: &GitOpsConfig,
    base: &str,
    name: &str,
) -> Result<PathBuf, ApiError> {
    if !valid_name(name) || !valid_sha(base) {
        return Err(bad("invalid service or commit"));
    }
    let repo = open_repo(config, &policy(config, name)?.target_branch)?;
    repo.service_file(base, name).map_err(|error| match error {
        thalamus_gitops::Error::ServiceNotFound => not_found(),
        _ => unavailable("committed service catalog is unavailable or invalid"),
    })
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
    let repo = open_repo(config, &policy(config, service)?.target_branch)?;
    repo.fetch()
        .map_err(|_| unavailable("GitOps repository could not be refreshed"))?;
    let file = catalog_service_file(config, from, service)?;
    verify_base_service(config, from, &file, service)?;
    verify_base_service(config, to, &file, service)?;
    Ok((repo, preview_config(config, &file)?))
}

pub(crate) fn policy<'a>(
    config: &'a GitOpsConfig,
    service: &str,
) -> Result<&'a ServicePolicy, ApiError> {
    config
        .policies
        .get(service)
        .ok_or_else(|| unavailable("no reviewed GitOps policy is configured for this service"))
}

pub(crate) fn check_current_policy(change: &Change, config: &GitOpsConfig) -> Result<(), ApiError> {
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

#[utoipa::path(get, path = "/api/v1/changes", responses((status = 200, body = ChangesView), (status = 401, description = "Authentication required"), (status = 503, description = "Changes unavailable")))]
pub(crate) async fn list_changes(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<ChangesView>, ApiError> {
    let store = thalamus_changes::Store::new(state.db.clone());
    match state
        .identity
        .begin_platform_admin(caller.principal())
        .await
    {
        Ok(mut guard) => {
            let changes = store
                .list_in_transaction(guard.transaction(), 100)
                .await
                .map_err(change_error)?;
            guard.commit().await?;
            Ok(Json(ChangesView {
                changes: changes.into_iter().map(Into::into).collect(),
            }))
        }
        Err(thalamus_identity::Error::Forbidden) => {
            let teams = state
                .identity
                .list_teams_principal(caller.principal())
                .await?;
            let config = gitops(&state)?;
            let mut visible = Vec::new();
            for team in teams.into_iter().filter(|team| team.role == "team_admin") {
                let services: Vec<String> = config
                    .policies
                    .iter()
                    .filter(|(_, policy)| {
                        policy
                            .controller
                            .as_ref()
                            .is_some_and(|controller| controller.owner_team == team.id)
                    })
                    .map(|(service, _)| service.clone())
                    .collect();
                if services.is_empty() {
                    continue;
                }
                let mut guard = state
                    .identity
                    .begin_authorized(caller.principal(), team.id, true)
                    .await?;
                let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
                    "SELECT id,created_at FROM thalamus_changes WHERE request->>'service'=ANY($1) ORDER BY created_at DESC,id DESC LIMIT 100"
                ).bind(&services).fetch_all(&mut **guard.transaction()).await?;
                for (id, created_at) in rows {
                    let change = store
                        .get_in_transaction(guard.transaction(), id)
                        .await
                        .map_err(change_error)?;
                    visible.push((created_at, ChangeView::from(change)));
                }
                guard.commit().await?;
            }
            visible.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
            visible.truncate(100);
            Ok(Json(ChangesView {
                changes: visible.into_iter().map(|(_, view)| view).collect(),
            }))
        }
        Err(error) => Err(error.into()),
    }
}

/// Catalog intent for a new immutable packaged model. These administrator
/// declarations are reviewed in Git; they are not runtime evidence.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewArtifact {
    name: String,
    uri: String,
    model_digest: String,
    tokenizer_digest: String,
}

fn content_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl NewArtifact {
    fn valid(&self) -> bool {
        let name = self.name.as_bytes();
        if name.is_empty()
            || name.len() > 128
            || !name[0].is_ascii_alphanumeric()
            || !name[name.len() - 1].is_ascii_alphanumeric()
            || !name
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(b))
            || !content_digest(&self.model_digest)
            || !content_digest(&self.tokenizer_digest)
            || self.uri.len() > 1024
        {
            return false;
        }
        let Some(image) = self.uri.strip_prefix("oci://") else {
            return false;
        };
        let Some((repository, digest)) = image.rsplit_once("@sha256:") else {
            return false;
        };
        if repository.is_empty()
            || !repository.contains('/')
            || repository
                .bytes()
                .any(|b| !(b.is_ascii_alphanumeric() || b"-._/:".contains(&b)))
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return false;
        }
        url::Url::parse(&self.uri).is_ok_and(|url| {
            url.scheme() == "oci"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
    }
}

fn artifact_from_yaml(name: &str, value: &serde_yaml::Value) -> Option<NewArtifact> {
    let map = value.as_mapping()?;
    if map.len() != 3 {
        return None;
    }
    let artifact = NewArtifact {
        name: name.to_owned(),
        uri: value.get("uri")?.as_str()?.to_owned(),
        model_digest: value.get("modelDigest")?.as_str()?.to_owned(),
        tokenizer_digest: value.get("tokenizerDigest")?.as_str()?.to_owned(),
    };
    artifact.valid().then_some(artifact)
}

fn artifact_matches_patch(artifact: &NewArtifact, patch: &Map<String, Value>) -> bool {
    if !artifact.valid() {
        return false;
    }
    let Some(deployments) = patch.get("deployments").and_then(Value::as_array) else {
        return false;
    };
    let image = artifact.uri.strip_prefix("oci://").unwrap_or_default();
    deployments
        .iter()
        .filter(|deployment| {
            deployment.get("artifact").and_then(Value::as_str) == Some(artifact.name.as_str())
                && deployment.get("image").and_then(Value::as_str) == Some(image)
                && deployment.get("packagedModel").and_then(Value::as_bool) == Some(true)
        })
        .count()
        == 1
}

fn new_artifact_patch(
    config: &GitOpsConfig,
    base: &str,
    artifact: NewArtifact,
) -> Result<Patch, ApiError> {
    if !artifact.valid() {
        return Err(bad("invalid immutable artifact intent"));
    }
    let file = FsPath::new("catalog/artifacts.yaml");
    let bytes = crate::workflow_form::read_regular_blob(&config.repository_dir, base, file)?;
    let document: serde_yaml::Value = serde_yaml::from_slice(&bytes)
        .map_err(|_| unavailable("committed artifact catalog is invalid"))?;
    let catalog = document
        .as_mapping()
        .ok_or_else(|| unavailable("committed artifact catalog is invalid"))?;
    if catalog.contains_key(artifact.name.as_str()) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "artifact name already exists; select another name",
        ));
    }
    let value = serde_yaml::to_value(serde_json::json!({
        "uri": artifact.uri,
        "modelDigest": artifact.model_digest,
        "tokenizerDigest": artifact.tokenizer_digest
    }))
    .map_err(|_| bad("invalid artifact intent"))?;
    Ok(Patch {
        file: file.to_owned(),
        path: vec![artifact.name],
        operation: Operation::Set(value),
    })
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateChange {
    service: String,
    base_commit: String,
    patch: Map<String, Value>,
    #[serde(default)]
    new_artifact: Option<NewArtifact>,
}

#[utoipa::path(post, path = "/api/v1/changes", params(("idempotency-key" = Option<String>, Header)), request_body = CreateChange, responses((status = 200, body = ChangeView), (status = 400, description = "Invalid request"), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 409, description = "Change conflict"), (status = 503, description = "Changes unavailable")))]
pub(crate) async fn create_change(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    headers: HeaderMap,
    Json(input): Json<CreateChange>,
) -> Result<Json<ChangeView>, ApiError> {
    let key = idempotency_key(&headers)?;
    if input
        .new_artifact
        .as_ref()
        .is_some_and(|artifact| !artifact_matches_patch(artifact, &input.patch))
    {
        return Err(bad(
            "new artifact must match exactly one packaged deployment image",
        ));
    }
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
        ChangePatches {
            service: patch,
            new_artifact: input.new_artifact,
        },
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
    input: ChangePatches,
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
    let repo = open_repo(config, &policy.target_branch)?;
    repo.fetch()
        .map_err(|_| unavailable("GitOps repository could not be refreshed"))?;
    let file = catalog_service_file(config, &base_commit, &service_name)?;
    let mut patch = patches(&file, input.service)?;
    if let Some(artifact) = input.new_artifact {
        if patch.len() >= 256 {
            return Err(bad("too many change patches"));
        }
        patch.push(new_artifact_patch(config, &base_commit, artifact)?);
    }
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
    let config = gitops(&state)?;
    check_current_policy(&change, config)?;
    let file = change_file(&change)?;
    let repo = open_repo(config, &change.request.target_branch)?;
    repo.fetch()
        .map_err(|_| unavailable("GitOps repository could not be refreshed"))?;
    let base = repo
        .resolve_base(&change.request.target_branch)
        .map_err(|_| unavailable("target branch is unavailable"))?;
    verify_base_service(config, &base, &file, &change.request.service)?;
    let preview = preview_config(config, &file)?;
    let actor = guard.actor_id().to_string();
    let result = if change.pr_number.is_some() {
        let github = config
            .github
            .as_ref()
            .ok_or_else(|| unavailable("GitHub publication is not configured"))?;
        let forge = github_client(github)?;
        let worker = worker_config(config, &file)?;
        let rollout_policy = crate::rollout::policy_for_change(&state, &change)?;
        store
            .refresh_published_in_transaction(
                guard.transaction(),
                id,
                &actor,
                &thalamus_changes::PublicationContext {
                    repo: &repo,
                    forge: forge.as_ref(),
                    config: &worker,
                },
                &rollout_policy,
                &base,
            )
            .await
    } else {
        store
            .refresh_in_transaction(guard.transaction(), id, &actor, &repo, &preview, &base)
            .await
    };
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
    let repo = open_repo(config, &change.request.target_branch)?;
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
    pub(crate) url: String,
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
    let worker = worker_config(config, &file)?;
    let forge = github_client(github)?;
    let repo = open_repo(config, &change.request.target_branch)?;
    // The same operator renderer and author are used for first publication and
    // for a refreshed branch update.
    let actor = guard.actor_id().to_string();
    let published = if change.pr_number.is_some() && change.state == "validated" {
        let policy = crate::rollout::policy_for_change(&state, &change)?;
        store
            .publish_refreshed_in_transaction(
                guard.transaction(),
                id,
                &actor,
                &thalamus_changes::PublicationContext {
                    repo: &repo,
                    forge: forge.as_ref(),
                    config: &worker,
                },
                &policy,
            )
            .await
    } else {
        store
            .publish_in_transaction(
                guard.transaction(),
                id,
                &actor,
                &repo,
                forge.as_ref(),
                &worker,
            )
            .await
    }
    .map_err(change_error)?;
    guard.audit(None, "change.publish", &id.to_string()).await?;
    guard.commit().await?;
    store
        .comment_rendered_diff(id, forge.as_ref(), &github.actor)
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
    pub project_id: Uuid,
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
            project_id: run.request.project_id,
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

pub(crate) fn evidence_fresh(
    observed: DateTime<Utc>,
    now: DateTime<Utc>,
    maximum_seconds: i64,
) -> bool {
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

/// Shared admission for browser and verified MCP principals. Profiles supply
/// workload and runner configuration; runtime evidence comes from live observation.
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
    let access = state
        .policy_source
        .as_ref()
        .ok_or_else(|| unavailable("service access policy is unavailable"))?
        .load()
        .map_err(|_| unavailable("service access policy is unavailable"))?;
    let service_id = access
        .service_id(&input.service)
        .map_err(|_| unavailable("service access policy is unavailable"))?
        .ok_or_else(not_found)?;
    let committed_deployment = access
        .includes_deployment(&service_id, &input.deployment)
        .map_err(|_| unavailable("service access policy is unavailable"))?;
    let teams = access
        .authorized_teams(&state.identity, copy_principal(&principal), &service_id)
        .await
        .map_err(|error| match error {
            thalamus_access::Error::Forbidden => not_found(),
            thalamus_access::Error::Identity(error) => error.into(),
            thalamus_access::Error::Source => unavailable("service access policy is unavailable"),
        })?;
    if !teams.contains(&team) {
        return Err(not_found());
    }
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
    // Recheck the committed policy revision after taking the identity lock.
    access
        .committed_services()
        .map_err(|_| unavailable("service access policy is unavailable"))?;
    let actor = guard.actor_id().to_string();
    let store = thalamus_runs::Store::new(state.db.clone());
    // Live observation timestamps naturally change. A replay must return its
    // original admitted run rather than fingerprint a second observation.
    let existing_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM thalamus_runs WHERE team=$1 AND idempotency_key=$2")
            .bind(team.to_string())
            .bind(key)
            .fetch_optional(&mut **guard.transaction())
            .await?;
    if let Some(id) = existing_id {
        let existing = store
            .get_in_transaction(guard.transaction(), &team.to_string(), id)
            .await
            .map_err(run_error)?;
        if existing.actor != actor
            || existing.request.project_id != project
            || existing.request.service != input.service
            || existing.request.deployment != input.deployment
            || existing.request.kind != input.kind
            || input
                .expected_runner
                .as_ref()
                .is_some_and(|runner| runner != &existing.request.runner)
            || input
                .expected_workload
                .as_ref()
                .is_some_and(|workload| workload != &existing.request.workload)
        {
            return Err(run_error(thalamus_runs::Error::IdempotencyConflict));
        }
        guard.commit().await?;
        return Ok(existing.into());
    }
    // A generated candidate is not in the pinned service catalog yet. Only
    // the exact active, owner/project-bound durable rollout stage may admit it;
    // its run binding is recorded in the same transaction as the run itself.
    let changes = thalamus_changes::Store::new(state.db.clone());
    let candidate = if committed_deployment {
        None
    } else {
        let config = gitops(state)?;
        let controller = policy(config, &input.service)?
            .controller
            .as_ref()
            .filter(|controller| {
                controller.owner_team == team
                    && controller.mode == thalamus_changes::RolloutMode::Gated
            })
            .ok_or_else(not_found)?;
        Some(
            changes
                .authorize_active_candidate_in_transaction(
                    guard.transaction(),
                    &input.service,
                    &input.deployment,
                    controller.owner_team,
                    project,
                )
                .await
                .map_err(change_error)?
                .ok_or_else(not_found)?,
        )
    };
    let config = state
        .config
        .workflow
        .as_ref()
        .and_then(|c| c.runs.as_ref())
        .ok_or_else(|| {
            unavailable("run creation is unavailable: trusted runner profiles are not configured")
        })?;
    let profiles = read_profiles(&config.profiles_file)?;
    let mut matching = profiles.into_iter().filter(|p| {
        p.team_id == team
            && p.request.project_id == project
            && p.request.service == input.service
            && p.request.kind == input.kind
            // A caller may select among operator-provided profiles by runner
            // name, but cannot create or supply the selected profile itself.
            && input.expected_runner.as_ref().is_none_or(|runner| runner == &p.request.runner)
    });
    let profile = matching.next().ok_or_else(|| {
        unavailable("no trusted run profile and runtime evidence for this project and deployment")
    })?;
    if matching.next().is_some() {
        return Err(unavailable("run profile is ambiguous"));
    }
    // No observation is accepted from the profile; the trusted observer reads
    // the actual selected route, deployment, runtime, and conditions now.
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
    let request = crate::workflow_evidence::bind_evidence(
        state,
        team,
        profile.request,
        &input.deployment,
        config.max_evidence_age_seconds,
        candidate.as_ref().map(|candidate| {
            (
                candidate.revision.as_str(),
                candidate.catalog_commit.as_str(),
            )
        }),
    )
    .await?;
    let run = store
        .create_in_transaction(
            guard.transaction(),
            &team.to_string(),
            &actor,
            key,
            &request,
        )
        .await
        .map_err(run_error)?;
    if let Some(candidate) = &candidate {
        changes
            .bind_candidate_run_in_transaction(guard.transaction(), run.id, candidate)
            .await
            .map_err(change_error)?;
    }
    guard
        .audit(agent, "run.create", &run.id.to_string())
        .await?;
    guard.commit().await?;
    Ok(run.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gitops_settings() -> Value {
        serde_json::json!({
            "gitops": {
                "repository_dir": "/git/repo",
                "worktree_root": "/git/worktrees",
                "policies": {
                    "svc": {
                        "target_branch": "main",
                        "validation_tier": "expected",
                        "rollout": {"mode": "rolling"}
                    }
                }
            }
        })
    }

    struct RenderPaths {
        root: PathBuf,
        repository: PathBuf,
        worktrees: PathBuf,
        script: PathBuf,
    }

    impl RenderPaths {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("thalamus-render-config-{}", Uuid::new_v4()));
            std::fs::create_dir_all(root.join("repo")).unwrap();
            std::fs::create_dir_all(root.join("worktrees")).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            let repository = root.join("repo");
            let worktrees = root.join("worktrees");
            let script = root.join("renderer.py");
            std::fs::write(&script, b"").unwrap();
            Self {
                root,
                repository,
                worktrees,
                script,
            }
        }

        fn configure(&self, value: &mut Value) {
            value["gitops"]["repository_dir"] = serde_json::json!(&self.repository);
            value["gitops"]["worktree_root"] = serde_json::json!(&self.worktrees);
            value["gitops"]["render_script_path"] = serde_json::json!(&self.script);
        }
    }

    impl Drop for RenderPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn assert_catalog_render(render: &RenderCommand, script: &FsPath, namespace: &str) {
        use std::ffi::OsString;

        assert_eq!(render.executable, OsString::from("python3"));
        assert_eq!(
            render.args,
            vec![
                OsString::from("-I"),
                OsString::from(script.as_os_str()),
                OsString::from("--write"),
                OsString::from("--workload-namespace"),
                OsString::from(namespace),
            ]
        );
    }

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
    fn preview_renders_the_catalog_in_the_configured_namespace() {
        let mut value = gitops_settings();
        let defaults: WorkflowConfig = serde_json::from_value(value.clone()).unwrap();
        assert!(defaults.validate().is_ok());
        assert_eq!(
            defaults.gitops.unwrap().render_script_path,
            PathBuf::from("/usr/local/lib/thalamus/render-catalog.py")
        );
        let paths = RenderPaths::new();
        paths.configure(&mut value);
        let config: WorkflowConfig = serde_json::from_value(value.clone()).unwrap();
        assert!(config.validate().is_ok());
        let gitops = config.gitops.as_ref().unwrap();
        assert_eq!(gitops.workload_namespace, "thalamus-inference");
        for file in ["catalog/services/svc.yaml", "catalog/services/other.yml"] {
            let preview = preview_config(gitops, FsPath::new(file)).unwrap();
            assert_eq!(preview.worktree_root, paths.worktrees);
            assert_catalog_render(&preview.render, &paths.script, "thalamus-inference");
        }
        for file in [
            "catalog/artifacts.yaml",
            "catalog/services/../secret.yaml",
            "/catalog/services/svc.yaml",
        ] {
            assert!(preview_config(gitops, FsPath::new(file)).is_err());
        }

        value["gitops"]["workload_namespace"] = serde_json::json!("custom-workloads");
        let config: WorkflowConfig = serde_json::from_value(value).unwrap();
        assert!(config.validate().is_ok());
        let preview = preview_config(
            config.gitops.as_ref().unwrap(),
            FsPath::new("catalog/services/svc.yaml"),
        )
        .unwrap();
        assert_catalog_render(&preview.render, &paths.script, "custom-workloads");
    }

    #[test]
    fn publication_uses_the_same_catalog_renderer_and_path_authorization() {
        let mut value = gitops_settings();
        let paths = RenderPaths::new();
        paths.configure(&mut value);
        let alternate = paths.root.join("alternate.py");
        std::fs::write(&alternate, b"").unwrap();
        value["gitops"]["github"] = serde_json::json!({
            "repository": "https://github.com/example/catalog.git",
            "api_base": "https://api.github.com",
            "app_id": 1,
            "installation_id": 2,
            "private_key_file": "/secrets/github-app.pem",
            "ci_bot_user_id": 3,
            "actor": "example[bot]",
            "commit_author_name": "Example Bot",
            "commit_author_email": "bot@example.com"
        });
        for (configured, expected, script) in [
            (None, "thalamus-inference", &paths.script),
            (Some("custom-workloads"), "custom-workloads", &alternate),
        ] {
            let mut value = value.clone();
            value["gitops"]["render_script_path"] = serde_json::json!(script);
            if let Some(namespace) = configured {
                value["gitops"]["workload_namespace"] = serde_json::json!(namespace);
            }
            let config: WorkflowConfig = serde_json::from_value(value).unwrap();
            assert!(config.validate().is_ok());
            let gitops = config.gitops.as_ref().unwrap();
            let worker = worker_config(gitops, FsPath::new("catalog/services/svc.yaml")).unwrap();
            assert_eq!(worker.worktree_root, paths.worktrees);
            assert_catalog_render(&worker.render, script, expected);
            assert!(worker_config(gitops, FsPath::new("catalog/artifacts.yaml")).is_err());
        }
    }

    #[test]
    fn render_script_configuration_requires_an_external_absolute_path() {
        for script in ["/opt/thalamus/render.py", "/git/repo-other/render.py"] {
            let mut value = gitops_settings();
            value["gitops"]["render_script_path"] = serde_json::json!(script);
            let config: WorkflowConfig = serde_json::from_value(value).unwrap();
            assert!(config.validate().is_ok(), "valid script: {script:?}");
        }
        for script in [
            "",
            "scripts/render-catalog.py",
            "/git/repo",
            "/git/repo/render.py",
            "/git/worktrees/render.py",
            "/tmp/../git/repo/render.py",
            "/opt/bad\0path",
        ] {
            let mut value = gitops_settings();
            value["gitops"]["render_script_path"] = serde_json::json!(script);
            let config: WorkflowConfig = serde_json::from_value(value).unwrap();
            assert!(config.validate().is_err(), "invalid script: {script:?}");
        }
        let mut value = gitops_settings();
        value["gitops"]["render_script_path"] = Value::Null;
        assert!(serde_json::from_value::<WorkflowConfig>(value).is_err());
    }

    #[test]
    fn render_command_requires_existing_external_regular_file_and_roots() {
        let paths = RenderPaths::new();
        let render = |script: &FsPath, repository: &FsPath, worktrees: &FsPath| {
            catalog_render_command(script, repository, worktrees, "test-ns")
        };
        let missing = paths.root.join("missing");
        assert!(render(&missing, &paths.repository, &paths.worktrees).is_err());
        assert!(render(&paths.root, &paths.repository, &paths.worktrees).is_err());
        assert!(render(&paths.script, &missing, &paths.worktrees).is_err());
        assert!(render(&paths.script, &paths.repository, &missing).is_err());
        for root in [&paths.repository, &paths.worktrees] {
            let script = root.join("render.py");
            std::fs::write(&script, b"").unwrap();
            assert!(render(&script, &paths.repository, &paths.worktrees).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn render_command_resolves_script_and_root_symlinks() {
        use std::os::unix::fs::symlink;

        let paths = RenderPaths::new();
        let alias = paths.root.join("renderer-alias.py");
        symlink(&paths.script, &alias).unwrap();
        let render =
            catalog_render_command(&alias, &paths.repository, &paths.worktrees, "test-ns").unwrap();
        assert_catalog_render(&render, &paths.script, "test-ns");

        let inside = paths.repository.join("render.py");
        std::fs::write(&inside, b"").unwrap();
        let unsafe_alias = paths.root.join("unsafe-alias.py");
        symlink(&inside, &unsafe_alias).unwrap();
        assert!(catalog_render_command(
            &unsafe_alias,
            &paths.repository,
            &paths.worktrees,
            "test-ns"
        )
        .is_err());
        let repository_alias = paths.root.join("repo-alias");
        symlink(&paths.repository, &repository_alias).unwrap();
        assert!(
            catalog_render_command(&inside, &repository_alias, &paths.worktrees, "test-ns")
                .is_err()
        );
    }

    #[test]
    fn gitops_config_requires_dns_namespace_and_rejects_legacy_render_fields() {
        let valid_namespaces = [
            "a".to_owned(),
            "0".to_owned(),
            "a-1".to_owned(),
            "a".repeat(63),
        ];
        for namespace in valid_namespaces {
            let mut value = gitops_settings();
            value["gitops"]["workload_namespace"] = serde_json::json!(&namespace);
            let config: WorkflowConfig = serde_json::from_value(value).unwrap();
            assert!(config.validate().is_ok(), "valid namespace: {namespace:?}");
        }

        let invalid_namespaces = [
            "".to_owned(),
            "-".to_owned(),
            "-start".to_owned(),
            "end-".to_owned(),
            "has.dot".to_owned(),
            "Uppercase".to_owned(),
            "has_underscore".to_owned(),
            "has/slash".to_owned(),
            "has space".to_owned(),
            "nul\0byte".to_owned(),
            "é".to_owned(),
            "a".repeat(64),
        ];
        for namespace in invalid_namespaces {
            let mut value = gitops_settings();
            value["gitops"]["workload_namespace"] = serde_json::json!(&namespace);
            let config: WorkflowConfig = serde_json::from_value(value).unwrap();
            assert!(
                config.validate().is_err(),
                "invalid namespace: {namespace:?}"
            );
        }
        for invalid in [Value::Null, serde_json::json!(42)] {
            let mut value = gitops_settings();
            value["gitops"]["workload_namespace"] = invalid;
            assert!(serde_json::from_value::<WorkflowConfig>(value).is_err());
        }

        for (field, legacy_value) in [
            ("render_executable", serde_json::json!("kcl")),
            (
                "render_args",
                serde_json::json!(["main.k", "-D", "service_file={service_file}"]),
            ),
        ] {
            let mut value = gitops_settings();
            value["gitops"][field] = legacy_value;
            assert!(
                serde_json::from_value::<WorkflowConfig>(value).is_err(),
                "legacy field {field} must be rejected"
            );
        }
    }

    #[test]
    fn config_requires_absolute_evidence_path() {
        let config: WorkflowConfig = serde_json::from_value(serde_json::json!({"runs":{"profiles_file":"relative.json", "max_evidence_age_seconds":60}})).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn in_place_requires_the_configured_app_window_guard() {
        let mut value = serde_json::json!({
            "gitops": {
                "repository_dir": "/git/repo",
                "worktree_root": "/git/worktrees",
                "workload_namespace": "thalamus-inference",
                "github": {
                    "repository": "https://github.com/example/catalog.git",
                    "api_base": "https://api.github.com",
                    "app_id": 42,
                    "installation_id": 43,
                    "private_key_file": "/secrets/github-app.pem",
                    "ci_bot_user_id": 44,
                    "actor": "thalamus[bot]",
                    "commit_author_name": "Thalamus",
                    "commit_author_email": "thalamus@example.invalid",
                    "webhook_secret": "01234567890123456789012345678901"
                },
                "policies": {"svc": {
                    "target_branch": "main",
                    "validation_tier": "functional",
                    "rollout": {"mode": "in_place", "window_start": "2026-01-01T00:00:00Z", "window_end": "2026-01-01T01:00:00Z"},
                    "controller": {
                        "service": "svc",
                        "repository": {"forge": "github", "host": "github.com", "namespace": "example", "name": "catalog"},
                        "target_branch": "main",
                        "owner_team": Uuid::new_v4(),
                        "validation_tier": "functional",
                        "mode": "in_place",
                        "required_checks": [{"context": "thalamus/rollout-window", "app_id": 42, "status_creator_id": 44}],
                        "required_approvers": ["reviewer"],
                        "trust_administrators": true,
                        "trust_merge_group_ci": true,
                        "merge_margin_seconds": 60,
                        "request_timeout_seconds": 15
                    }
                }}
            },
            "runs": {"profiles_file": "/config/profiles.json", "max_evidence_age_seconds": 60}
        });
        let parse =
            |value: &Value| serde_json::from_value::<WorkflowConfig>(value.clone()).unwrap();
        assert!(parse(&value).validate().is_ok());
        value["gitops"]["policies"]["svc"]["controller"]["required_checks"][0]
            ["status_creator_id"] = serde_json::json!(45);
        assert!(parse(&value).validate().is_err());
        value["gitops"]["policies"]["svc"]["controller"]["required_checks"][0]
            ["status_creator_id"] = serde_json::json!(44);
        value["gitops"]["github"]["webhook_secret"] = Value::Null;
        assert!(parse(&value).validate().is_err());
    }

    #[test]
    fn new_artifact_requires_immutable_content_and_matching_packaged_image() {
        let image = format!("registry.example/model@sha256:{}", "a".repeat(64));
        let artifact = NewArtifact {
            name: "model-v2".into(),
            uri: format!("oci://{image}"),
            model_digest: format!("sha256:{}", "b".repeat(64)),
            tokenizer_digest: format!("sha256:{}", "c".repeat(64)),
        };
        let matching = serde_json::json!({"deployments":[
            {"name":"stable", "artifact":"model-v2", "image":image, "packagedModel":true}
        ]});
        let map = matching.as_object().unwrap();
        assert!(artifact_matches_patch(&artifact, map));
        let mut incorrect = matching.clone();
        incorrect["deployments"][0]["packagedModel"] = Value::Bool(false);
        assert!(!artifact_matches_patch(
            &artifact,
            incorrect.as_object().unwrap()
        ));
        let mut duplicate = matching.clone();
        let deployment = duplicate["deployments"][0].clone();
        duplicate["deployments"]
            .as_array_mut()
            .unwrap()
            .push(deployment);
        assert!(!artifact_matches_patch(
            &artifact,
            duplicate.as_object().unwrap()
        ));
        let mut invalid = artifact;
        invalid.tokenizer_digest = "sha256:mutable".into();
        assert!(!invalid.valid());
    }

    #[test]
    fn prepared_change_finds_service_after_artifact_patch() {
        let artifact = serde_yaml::to_value(serde_json::json!({
            "uri": format!("oci://registry.example/model@sha256:{}", "a".repeat(64)),
            "modelDigest": format!("sha256:{}", "b".repeat(64)),
            "tokenizerDigest": format!("sha256:{}", "c".repeat(64)),
        }))
        .unwrap();
        let service_file = PathBuf::from("catalog/services/svc.yaml");
        let mut change = Change {
            id: Uuid::new_v4(),
            actor: "actor".into(),
            idempotency_key: "key".into(),
            request: Proposal {
                service: "svc".into(),
                base_sha: "d".repeat(40),
                target_branch: "main".into(),
                title: "svc".into(),
                validation_tier: "functional".into(),
                rollout: Rollout::Gated,
                revert_commit: None,
                patches: vec![
                    Patch {
                        file: PathBuf::from("catalog/artifacts.yaml"),
                        path: vec!["model-v2".into()],
                        operation: Operation::Set(artifact),
                    },
                    Patch {
                        file: service_file.clone(),
                        path: vec!["deployments".into()],
                        operation: Operation::Set(serde_yaml::Value::Sequence(vec![])),
                    },
                ],
            },
            state: "validated".into(),
            branch: "thalamus/example".into(),
            head_sha: None,
            pr_number: None,
            pr_url: None,
            base_render_sha256: None,
            proposed_render_sha256: None,
            data_diff: None,
            rendered_diff: None,
            error: None,
        };
        assert_eq!(change_file(&change).unwrap(), service_file);
        change.request.patches.push(Patch {
            file: PathBuf::from("catalog/artifacts.yaml"),
            path: vec!["other".into()],
            operation: Operation::Remove,
        });
        assert!(change_file(&change).is_err());
    }
}
