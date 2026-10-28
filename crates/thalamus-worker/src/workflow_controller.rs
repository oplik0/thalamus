//! Worker-owned reconciliation of operator-configured GitOps workflows.
//!
//! The Kubernetes lease limits concurrent sweeps, but is not a fencing token:
//! the changes store serializes durable transitions and external side effects
//! across workers. Neither the configuration nor credentials come from callers.
use std::{
    collections::BTreeMap,
    path::{Component, Path as FsPath, PathBuf},
};

use chrono::Duration;
use serde::Deserialize;
use sqlx::PgPool;
use thalamus_changes::{PreviewConfig, Rollout, RolloutMode, RolloutPolicy, Store, WorkerConfig};
use thalamus_gitops::{CommitAuthor, GitOpsRepo, RenderCommand};
use thalamus_runs::{Artifacts, Templates};
use thalamus_runtime::admission::RunnerProfiles;

use crate::{
    workflow_validation::{WorkflowValidation, WorkflowValidationConfig},
    Result,
};

// This is the same operator-owned JSON accepted by thalamus-server's
// workflows::WorkflowConfig. Keep its fields and validation in sync.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowConfig {
    #[serde(default)]
    gitops: Option<GitOpsConfig>,
    #[serde(default)]
    runs: Option<RunConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitOpsConfig {
    repository_dir: PathBuf,
    worktree_root: PathBuf,
    #[serde(default = "default_render_script_path")]
    render_script_path: PathBuf,
    #[serde(default = "default_workload_namespace")]
    workload_namespace: String,
    #[serde(default)]
    service_schema_path: Option<PathBuf>,
    policies: BTreeMap<String, ServicePolicy>,
    #[serde(default)]
    github: Option<GitHubConfig>,
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
    namespace: String,
) -> std::result::Result<RenderCommand, &'static str> {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServicePolicy {
    target_branch: String,
    validation_tier: String,
    rollout: Rollout,
    #[serde(default)]
    controller: Option<RolloutPolicy>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitHubConfig {
    repository: String,
    api_base: String,
    app_id: u64,
    installation_id: u64,
    private_key_file: PathBuf,
    ci_bot_user_id: u64,
    actor: String,
    commit_author_name: String,
    commit_author_email: String,
    #[serde(default)]
    webhook_secret: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfig {
    profiles_file: PathBuf,
    max_evidence_age_seconds: i64,
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

impl WorkflowConfig {
    fn parse(value: &str) -> std::result::Result<Self, &'static str> {
        if value.trim().is_empty() {
            return Err("THALAMUS_WORKFLOWS_CONFIG must not be empty");
        }
        let config: Self = serde_json::from_str(value)
            .map_err(|_| "THALAMUS_WORKFLOWS_CONFIG is invalid JSON or has invalid fields")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.gitops.is_none() && self.runs.is_none() {
            return Err("THALAMUS_WORKFLOWS_CONFIG must configure gitops or runs");
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
                return Err("invalid GitOps paths, workload namespace, or service policies");
            }
            for (service, policy) in &config.policies {
                if !valid_name(service)
                    || policy.target_branch.trim().is_empty()
                    || policy.validation_tier.trim().is_empty()
                    || policy.validation_tier.chars().any(char::is_control)
                {
                    return Err("invalid GitOps service policy");
                }
                if matches!(&policy.rollout, Rollout::Gated | Rollout::InPlace { .. })
                    && policy.controller.is_none()
                {
                    return Err("gated and in-place services require a rollout controller");
                }
                if let Some(controller) = &policy.controller {
                    if self.runs.is_none() {
                        return Err("rollout controller requires trusted run profiles");
                    }
                    let repository = config
                        .github
                        .as_ref()
                        .and_then(|github| {
                            forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository).ok()
                        })
                        .ok_or("rollout controller requires a configured GitHub repository")?;
                    let mode = match &policy.rollout {
                        Rollout::Gated => RolloutMode::Gated,
                        Rollout::Rolling => RolloutMode::Rolling,
                        Rollout::InPlace { .. } => RolloutMode::InPlace,
                    };
                    if controller.validate().is_err()
                        || controller.service != *service
                        || controller.repository != repository
                        || controller.target_branch != policy.target_branch
                        || controller.validation_tier != policy.validation_tier
                        || controller.mode != mode
                    {
                        return Err("rollout controller does not match GitOps service policy");
                    }
                    if mode == RolloutMode::InPlace {
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
                            return Err("in-place rollout requires the signed webhook and App-pinned rollout-window status");
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
                    || github.commit_author_email.chars().any(char::is_control)
                    || github
                        .webhook_secret
                        .as_ref()
                        .is_some_and(|secret| secret.len() < 32)
                {
                    return Err("invalid GitHub publication configuration");
                }
            }
        }
        if let Some(config) = &self.runs {
            if !config.profiles_file.is_absolute()
                || config.max_evidence_age_seconds <= 0
                || Duration::try_seconds(config.max_evidence_age_seconds).is_none()
            {
                return Err("invalid run profiles path or evidence freshness limit");
            }
        }
        Ok(())
    }
}

struct ServiceController {
    service: String,
    target_branch: String,
    repo: GitOpsRepo,
    policy: Option<RolloutPolicy>,
}

pub(crate) struct WorkflowController {
    store: Store,
    // Retaining this client retains Octocrab's renewable installation-token cache.
    forge: forge::github::Github,
    preview: PreviewConfig,
    worker: WorkerConfig,
    services: Vec<ServiceController>,
    validation: Option<WorkflowValidation>,
}

impl WorkflowController {
    /// Parse exactly the operator configuration used by the server. A worker
    /// without GitOps/forge configuration has no external workflow scheduler.
    pub(crate) fn from_env(
        db: PgPool,
        client: kube::Client,
        artifacts: Artifacts,
        runtime_json: &str,
        runner_targets: &BTreeMap<String, String>,
        templates: &Templates,
        runner_profiles: &RunnerProfiles,
    ) -> Result<Option<Self>> {
        let value = match std::env::var("THALAMUS_WORKFLOWS_CONFIG") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => return Err("THALAMUS_WORKFLOWS_CONFIG is not valid Unicode".into()),
        };
        let WorkflowConfig { gitops, runs } = WorkflowConfig::parse(&value)?;
        let Some(config) = gitops else {
            return Ok(None);
        };
        let Some(github) = config.github else {
            return Ok(None);
        };

        let repository = forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository)
            .map_err(|_| "invalid GitHub repository configuration")?;
        // Do not include paths, key material, or forge error text in diagnostics.
        let private_key = std::fs::read(&github.private_key_file)
            .map_err(|_| "GitHub App private key is unavailable")?;
        let forge = forge::github::Github::new_app(
            repository.clone(),
            &github.api_base,
            github.app_id,
            github.installation_id,
            &private_key,
        )
        .map_err(|_| "invalid GitHub App configuration")?;

        let validation = if config
            .policies
            .values()
            .any(|policy| policy.controller.is_some())
        {
            let runs = runs.ok_or("rollout controller requires trusted run profiles")?;
            Some(WorkflowValidation::new(
                db.clone(),
                client,
                artifacts,
                runtime_json,
                WorkflowValidationConfig {
                    profiles_file: runs.profiles_file,
                    runner_targets: runner_targets.clone(),
                    templates: templates.clone(),
                    test_profiles: runner_profiles.clone(),
                },
            )?)
        } else {
            None
        };
        let repo = GitOpsRepo::open(&config.repository_dir)
            .map_err(|_| "cannot open the configured GitOps repository")?;
        let render = catalog_render_command(
            &config.render_script_path,
            &config.repository_dir,
            &config.worktree_root,
            config.workload_namespace,
        )?;
        let preview = PreviewConfig {
            worktree_root: config.worktree_root.clone(),
            render: render.clone(),
        };
        let worker = WorkerConfig {
            worktree_root: config.worktree_root,
            render,
            commit_author: CommitAuthor {
                name: github.commit_author_name,
                email: github.commit_author_email,
            },
            repository,
            forge_actor: github.actor,
        };
        let mut services = Vec::with_capacity(config.policies.len());
        for (service, policy) in config.policies {
            let bound_repo = repo
                .bind_origin(&github.repository, &policy.target_branch)
                .map_err(|_| "GitOps origin does not match the configured forge repository")?;
            services.push(ServiceController {
                service,
                target_branch: policy.target_branch,
                repo: bound_repo,
                policy: policy.controller,
            });
        }
        Ok(Some(Self {
            store: Store::new(db),
            forge,
            preview,
            worker,
            services,
            validation,
        }))
    }

    /// Called once during each worker lease sweep. A failure for one service
    /// does not prevent the remaining policies from making progress; the next
    /// sweep retries failed work using the store's durable reconciliation state.
    pub(crate) async fn reconcile(&self) -> Result<()> {
        let mut failed = false;
        for service in &self.services {
            if self
                .store
                .reconcile_pr_previews(
                    &service.repo,
                    &self.forge,
                    &self.preview,
                    &thalamus_changes::AdvisoryScope {
                        repository: &self.worker.repository,
                        target_branch: &service.target_branch,
                        service: &service.service,
                        forge_actor: &self.worker.forge_actor,
                    },
                )
                .await
                .is_err()
            {
                failed = true;
            }
            if let Some(policy) = &service.policy {
                if let Some(validation) = &self.validation {
                    if self
                        .store
                        .reconcile_service(
                            &service.service,
                            &service.repo,
                            &self.forge,
                            &self.worker,
                            policy,
                            validation,
                        )
                        .await
                        .is_err()
                    {
                        failed = true;
                    }
                } else {
                    // Construction rejects this configuration; fail closed if
                    // that invariant ever changes.
                    failed = true;
                }
            }
        }
        if failed {
            return Err("one or more workflow services failed to reconcile".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn settings() -> Value {
        json!({
            "gitops": {
                "repository_dir": "/var/lib/thalamus/catalog.git",
                "worktree_root": "/var/lib/thalamus/worktrees",
                "service_schema_path": "catalog/service.schema.json",
                "policies": {
                    "model-service": {
                        "target_branch": "main",
                        "validation_tier": "standard",
                        "rollout": {"mode": "rolling"}
                    }
                },
                "github": {
                    "repository": "https://github.com/example/catalog.git",
                    "api_base": "https://api.github.com",
                    "app_id": 1,
                    "installation_id": 2,
                    "private_key_file": "/var/run/secrets/github/app.pem",
                    "ci_bot_user_id": 3,
                    "actor": "example[bot]",
                    "commit_author_name": "Example Bot",
                    "commit_author_email": "bot@example.com",
                    "webhook_secret": "01234567890123456789012345678901"
                }
            },
            "runs": {
                "profiles_file": "/var/lib/thalamus/profiles.json",
                "max_evidence_age_seconds": 3600
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
            let root = std::env::temp_dir().join(format!(
                "thalamus-worker-render-config-{}",
                uuid::Uuid::new_v4()
            ));
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
            value["gitops"]["repository_dir"] = json!(&self.repository);
            value["gitops"]["worktree_root"] = json!(&self.worktrees);
            value["gitops"]["render_script_path"] = json!(&self.script);
        }
    }

    impl Drop for RenderPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn parses(value: &Value) -> bool {
        WorkflowConfig::parse(&value.to_string()).is_ok()
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
    fn configured_namespace_controls_full_catalog_rendering() {
        let mut value = settings();
        let defaults = WorkflowConfig::parse(&value.to_string()).unwrap();
        assert_eq!(
            defaults.gitops.unwrap().render_script_path,
            PathBuf::from("/usr/local/lib/thalamus/render-catalog.py")
        );
        let paths = RenderPaths::new();
        paths.configure(&mut value);
        let config = WorkflowConfig::parse(&value.to_string())
            .unwrap()
            .gitops
            .unwrap();
        assert_eq!(config.workload_namespace, "thalamus-inference");
        let render = catalog_render_command(
            &config.render_script_path,
            &config.repository_dir,
            &config.worktree_root,
            config.workload_namespace,
        )
        .unwrap();
        assert_catalog_render(&render, &paths.script, "thalamus-inference");

        value["gitops"]["workload_namespace"] = json!("custom-workloads");
        let alternate = paths.root.join("alternate.py");
        std::fs::write(&alternate, b"").unwrap();
        value["gitops"]["render_script_path"] = json!(&alternate);
        let config = WorkflowConfig::parse(&value.to_string())
            .unwrap()
            .gitops
            .unwrap();
        assert_eq!(config.workload_namespace, "custom-workloads");
        let render = catalog_render_command(
            &config.render_script_path,
            &config.repository_dir,
            &config.worktree_root,
            config.workload_namespace,
        )
        .unwrap();
        assert_catalog_render(&render, &alternate, "custom-workloads");
    }

    #[test]
    fn render_script_configuration_requires_an_external_absolute_path() {
        let mut base = settings();
        base["gitops"]["repository_dir"] = json!("/git/repo");
        base["gitops"]["worktree_root"] = json!("/git/worktrees");
        for script in ["/opt/thalamus/render.py", "/git/repo-other/render.py"] {
            let mut value = base.clone();
            value["gitops"]["render_script_path"] = json!(script);
            assert!(parses(&value), "valid script: {script:?}");
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
            let mut value = base.clone();
            value["gitops"]["render_script_path"] = json!(script);
            assert!(!parses(&value), "invalid script: {script:?}");
        }
        base["gitops"]["render_script_path"] = Value::Null;
        assert!(!parses(&base));
    }

    #[test]
    fn render_command_requires_existing_external_regular_file_and_roots() {
        let paths = RenderPaths::new();
        let render = |script: &FsPath, repository: &FsPath, worktrees: &FsPath| {
            catalog_render_command(script, repository, worktrees, "test-ns".into())
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
        let render = catalog_render_command(
            &alias,
            &paths.repository,
            &paths.worktrees,
            "test-ns".into(),
        )
        .unwrap();
        assert_catalog_render(&render, &paths.script, "test-ns");

        let inside = paths.repository.join("render.py");
        std::fs::write(&inside, b"").unwrap();
        let unsafe_alias = paths.root.join("unsafe-alias.py");
        symlink(&inside, &unsafe_alias).unwrap();
        assert!(catalog_render_command(
            &unsafe_alias,
            &paths.repository,
            &paths.worktrees,
            "test-ns".into()
        )
        .is_err());
        let repository_alias = paths.root.join("repo-alias");
        symlink(&paths.repository, &repository_alias).unwrap();
        assert!(catalog_render_command(
            &inside,
            &repository_alias,
            &paths.worktrees,
            "test-ns".into()
        )
        .is_err());
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
            let mut value = settings();
            value["gitops"]["workload_namespace"] = json!(&namespace);
            assert!(parses(&value), "valid namespace: {namespace:?}");
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
            let mut value = settings();
            value["gitops"]["workload_namespace"] = json!(&namespace);
            assert!(!parses(&value), "invalid namespace: {namespace:?}");
        }
        for invalid in [Value::Null, json!(42)] {
            let mut value = settings();
            value["gitops"]["workload_namespace"] = invalid;
            assert!(!parses(&value));
        }

        for (field, legacy_value) in [
            ("render_executable", json!("kcl")),
            (
                "render_args",
                json!(["main.k", "-D", "service_file={service_file}"]),
            ),
        ] {
            let mut value = settings();
            value["gitops"][field] = legacy_value;
            assert!(!parses(&value), "legacy field {field} must be rejected");
        }
    }

    #[test]
    fn accepts_server_shape_and_rejects_unknown_or_expiring_credentials() {
        let mut value = settings();
        assert!(parses(&value));
        let mut preview_only = value.clone();
        preview_only["gitops"]
            .as_object_mut()
            .unwrap()
            .remove("github");
        preview_only.as_object_mut().unwrap().remove("runs");
        assert!(parses(&preview_only));
        value["gitops"]["github"]["token"] = json!("obsolete");
        assert!(!parses(&value));
        assert!(WorkflowConfig::parse(" ").is_err());
        assert!(WorkflowConfig::parse("{}").is_err());
        assert!(WorkflowConfig::parse(
            r#"{"runs":{"profiles_file":"/profiles.json","max_evidence_age_seconds":1}}"#
        )
        .is_ok());
    }

    #[test]
    fn rejects_unsafe_paths_and_invalid_evidence_settings() {
        let mut value = settings();
        value["gitops"]["service_schema_path"] = json!("../schema.json");
        assert!(!parses(&value));
        value = settings();
        value["gitops"]["github"]["private_key_file"] = json!("relative.pem");
        assert!(!parses(&value));
        value = settings();
        value["runs"]["profiles_file"] = json!("profiles.json");
        assert!(!parses(&value));
        value = settings();
        value["runs"]["max_evidence_age_seconds"] = json!(0);
        assert!(!parses(&value));
    }

    #[test]
    fn gated_and_in_place_require_a_controller() {
        let mut value = settings();
        assert!(parses(&value));
        value["gitops"]["policies"]["model-service"]["rollout"] = json!({"mode": "gated"});
        assert!(!parses(&value));
        value["gitops"]["policies"]["model-service"]["rollout"] = json!({
            "mode": "in_place",
            "window_start": "2026-01-01T00:00:00Z",
            "window_end": "2026-01-01T01:00:00Z"
        });
        assert!(!parses(&value));
    }

    #[test]
    fn rejects_mismatched_controller_and_missing_runs() {
        let mut value = settings();
        value["gitops"]["policies"]["model-service"]["controller"] = json!({
            "service": "model-service",
            "repository": {"forge": "github", "host": "github.com", "namespace": "example", "name": "catalog"},
            "target_branch": "main",
            "owner_team": "00000000-0000-0000-0000-000000000001",
            "validation_tier": "standard",
            "mode": "rolling",
            "required_checks": [],
            "required_approvers": [],
            "trust_administrators": false,
            "trust_merge_group_ci": false,
            "merge_margin_seconds": 0,
            "request_timeout_seconds": 0
        });
        assert!(parses(&value));
        for (field, bad_value) in [
            ("service", json!("other")),
            ("target_branch", json!("other")),
            ("validation_tier", json!("other")),
            ("mode", json!("gated")),
            (
                "repository",
                json!({"forge": "github", "host": "github.com", "namespace": "other", "name": "catalog"}),
            ),
        ] {
            let mut mismatch = value.clone();
            mismatch["gitops"]["policies"]["model-service"]["controller"][field] = bad_value;
            assert!(
                !parses(&mismatch),
                "controller {field} must match the service policy"
            );
        }
        value.as_object_mut().unwrap().remove("runs");
        assert!(!parses(&value));
    }

    #[test]
    fn in_place_requires_signed_webhook_and_app_pinned_window_status() {
        let mut value = settings();
        value["gitops"]["policies"]["model-service"]["rollout"] = json!({
            "mode": "in_place",
            "window_start": "2026-01-01T00:00:00Z",
            "window_end": "2026-01-01T01:00:00Z"
        });
        value["gitops"]["policies"]["model-service"]["controller"] = json!({
            "service": "model-service",
            "repository": {"forge": "github", "host": "github.com", "namespace": "example", "name": "catalog"},
            "target_branch": "main",
            "owner_team": "00000000-0000-0000-0000-000000000001",
            "validation_tier": "standard",
            "mode": "in_place",
            "required_checks": [{
                "context": "thalamus/rollout-window",
                "app_id": 1,
                "status_creator_id": 3
            }],
            "required_approvers": ["reviewer"],
            "trust_administrators": true,
            "trust_merge_group_ci": true,
            "merge_margin_seconds": 40,
            "request_timeout_seconds": 10
        });
        assert!(parses(&value));
        let mut mismatch = value.clone();
        mismatch["gitops"]["github"]["webhook_secret"] = Value::Null;
        assert!(!parses(&mismatch));
        mismatch = value.clone();
        mismatch["gitops"]["policies"]["model-service"]["controller"]["trust_merge_group_ci"] =
            json!(false);
        assert!(!parses(&mismatch));
        mismatch = value.clone();
        mismatch["gitops"]["policies"]["model-service"]["controller"]["merge_margin_seconds"] =
            json!(39);
        assert!(!parses(&mismatch));
        for (field, wrong) in [
            ("context", json!("other")),
            ("app_id", json!(2)),
            ("status_creator_id", json!(4)),
        ] {
            mismatch = value.clone();
            mismatch["gitops"]["policies"]["model-service"]["controller"]["required_checks"][0]
                [field] = wrong;
            assert!(!parses(&mismatch));
        }
    }
}
