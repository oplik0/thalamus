//! Worker-backed rollout validation. Profiles select work; only independently
//! observed routing and content-addressed results can establish success.
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use k8s_openapi::api::batch::v1::Job;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;
use thalamus_changes::{
    Error, InPlaceSafety, InPlaceSafetyRequirement, Result, ValidationGate, ValidationProgress,
    ValidationRequirement, ValidationStage,
};
use thalamus_runs::{Artifacts, Run, RunKind, RunRequest, Store, Templates, Workload};
use thalamus_runtime::{
    admission::{
        requires_agentic, resolve_runner_config, validate_profiles, validate_test_workload,
        RunnerProfiles, TestProfile,
    },
    records::Records,
    LiveObserver, RouteSnapshot, RuntimeConfig, ServiceRoutes,
};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::runtime_integration::verify_validation_job;

const MAX_PROFILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunProfile {
    team_id: Uuid,
    request: ProfileRequest,
}

/// Only planned test inputs belong in the operator profile. Observation
/// metadata is obtained separately from the live route at admission.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileRequest {
    project_id: Uuid,
    kind: RunKind,
    service: String,
    runner: String,
    workload: Workload,
}

struct VerifiedExecution {
    evidence: thalamus_runtime::records::VerifiedEvidence,
    options: Value,
    agentic: bool,
}

pub(crate) struct WorkflowValidationConfig {
    pub(crate) profiles_file: PathBuf,
    pub(crate) runner_targets: BTreeMap<String, String>,
    pub(crate) templates: Templates,
    pub(crate) test_profiles: RunnerProfiles,
}

/// All configuration here is operator-controlled; it is never route or result proof.
pub(crate) struct WorkflowValidation {
    db: PgPool,
    changes: thalamus_changes::Store,
    runs: Store,
    records: Records,
    artifacts: Artifacts,
    observer: LiveObserver,
    config: RuntimeConfig,
    profiles_file: PathBuf,
    runner_targets: BTreeMap<String, String>,
    templates: Templates,
    test_profiles: RunnerProfiles,
}

impl WorkflowValidation {
    pub(crate) fn new(
        db: PgPool,
        client: kube::Client,
        artifacts: Artifacts,
        runtime_json: &str,
        settings: WorkflowValidationConfig,
    ) -> Result<Self> {
        let WorkflowValidationConfig {
            profiles_file,
            runner_targets,
            templates,
            test_profiles,
        } = settings;
        if !profiles_file.is_absolute()
            || runner_targets.is_empty()
            || runner_targets.iter().any(|(runner, target)| {
                runner.trim().is_empty() || !matches!(target.as_str(), "candidate" | "public")
            })
        {
            return Err(Error::Invalid("invalid workflow validation configuration"));
        }
        validate_profiles(&test_profiles)
            .map_err(|_| Error::Invalid("invalid workflow validation test profiles"))?;
        let config: RuntimeConfig = serde_json::from_str(runtime_json)
            .map_err(|_| Error::Invalid("invalid workflow runtime configuration"))?;
        let observer = LiveObserver::new(client, config.clone())
            .map_err(|_| Error::Invalid("invalid workflow runtime configuration"))?;
        Ok(Self {
            changes: thalamus_changes::Store::new(db.clone()),
            runs: Store::new(db.clone()),
            records: Records::new(db.clone()),
            db,
            artifacts,
            observer,
            config,
            profiles_file,
            runner_targets,
            templates,
            test_profiles,
        })
    }

    fn approved_test_profile<'a>(
        &'a self,
        requirement: &ValidationRequirement,
        profile: &ProfileRequest,
    ) -> Result<&'a TestProfile> {
        let test = self
            .test_profiles
            .get(&requirement.service)
            .and_then(|profiles| profiles.get(&profile.runner))
            .ok_or(Error::Invalid("workflow runner test profile unavailable"))?;
        if test.kind != RunKind::Validation
            || test.route_target != stage_name(requirement.stage)
            || !self
                .templates
                .supports_validation_runner(&profile.runner, &profile.workload.name)
            || validate_test_workload(test, &profile.workload).is_err()
        {
            return Err(Error::Invalid(
                "workflow runner or test profile is not authorized",
            ));
        }
        Ok(test)
    }

    async fn reconcile_inner(
        &self,
        requirement: &ValidationRequirement,
    ) -> Result<ValidationProgress> {
        validate_requirement(requirement)?;
        let binding = self
            .config
            .routes
            .get(&requirement.service)
            .ok_or(Error::Invalid("service has no workflow runtime binding"))?;
        let authorization = self
            .changes
            .authorize_validation(requirement, binding.project_id)
            .await?;
        let route = expected_route(binding, requirement, authorization.route.as_deref())?;
        let observer = &self.observer;
        let candidate = self.candidate_proof(requirement, binding).await?;
        let key = idempotency_key(requirement);
        // Do not irrevocably bind this stage's deterministic key to a run
        // against the old Flux revision while the merged catalog is converging.
        // Existing runs are looked up first and never require the profile again.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM thalamus_runs WHERE team=$1 AND idempotency_key=$2)",
        )
        .bind(requirement.owner_team.to_string())
        .bind(&key)
        .fetch_one(&self.db)
        .await?;
        let admission = if exists {
            None
        } else {
            let profile = select_profile(
                read_profiles(&self.profiles_file).await?,
                requirement,
                binding.project_id,
                &self.runner_targets,
            )?;
            let test = self.approved_test_profile(requirement, &profile)?;
            let agentic = requires_agentic(&profile.kind, &test.options)
                .map_err(|_| Error::Invalid("invalid workflow test options"))?;
            if candidate.as_ref().is_some_and(|old| {
                !same_validation_options(old.agentic, &old.options, agentic, &test.options)
            }) {
                return Err(Error::Invalid(
                    "candidate and public validation options differ",
                ));
            }
            let snapshot = match observe_stage(observer, requirement, &route, agentic).await {
                Ok(snapshot) => snapshot,
                Err(thalamus_runtime::Error::Invalid(_) | thalamus_runtime::Error::Changed) => {
                    return Ok(ValidationProgress::Pending { run_id: None })
                }
                Err(thalamus_runtime::Error::Kubernetes(kube::Error::Api(error)))
                    if error.code == 404 =>
                {
                    return Ok(ValidationProgress::Pending { run_id: None })
                }
                Err(_) => return Err(Error::Invalid("workflow route observation unavailable")),
            };
            let backend = format!(
                "{}/{}/{}",
                requirement.service, requirement.deployment, requirement.revision
            );
            if !route_matches_mode(requirement, &route, &snapshot, agentic)
                || !snapshot_matches(&snapshot, requirement, &snapshot.route, &backend)
                || candidate
                    .as_ref()
                    .is_some_and(|old| !same_serving_revision(&old.evidence.ingestion, &snapshot))
            {
                return Ok(ValidationProgress::Pending { run_id: None });
            }
            Some((snapshot, profile, test))
        };
        let Some(run) = self
            .find_or_create(requirement, binding, &key, admission)
            .await?
        else {
            return Ok(ValidationProgress::Pending { run_id: None });
        };
        if !run_matches(&run, requirement, binding, &key) {
            return Err(Error::IdempotencyConflict);
        }
        let progress = match run.state.as_str() {
            "pending" | "running" | "ingesting" => ValidationProgress::Pending {
                run_id: Some(run.id),
            },
            "failed" | "ingestion_failed" => ValidationProgress::Failed { run_id: run.id },
            "succeeded" => {
                return self
                    .completed(requirement, &route, &run, candidate.as_ref(), observer)
                    .await
            }
            _ => return Err(Error::Invalid("invalid workflow validation run state")),
        };
        Ok(progress)
    }

    /// The actor lock serializes authorization changes with run creation. Lookup
    /// precedes profile reads, so retries survive profile rotation or removal.
    async fn find_or_create(
        &self,
        requirement: &ValidationRequirement,
        binding: &ServiceRoutes,
        key: &str,
        admission: Option<(RouteSnapshot, ProfileRequest, &TestProfile)>,
    ) -> Result<Option<Run>> {
        let actor = Uuid::parse_str(&requirement.actor)
            .map_err(|_| Error::Invalid("invalid workflow validation actor"))?;
        if actor.is_nil() || actor.to_string() != requirement.actor {
            return Err(Error::Invalid("invalid workflow validation actor"));
        }
        let mut tx = self.db.begin().await?;
        let user: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM identity_users WHERE id=$1 AND NOT disabled FOR UPDATE",
        )
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
        let authorized: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM identity_users u JOIN identity_projects p ON p.id=$3 AND p.team_id=$2 LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$2 WHERE u.id=$1 AND NOT u.disabled AND (u.platform_admin OR m.role IS NOT NULL))",
        )
        .bind(actor)
        .bind(requirement.owner_team)
        .bind(binding.project_id)
        .fetch_one(&mut *tx)
        .await?;
        let project: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM identity_projects WHERE id=$1 AND team_id=$2 FOR SHARE",
        )
        .bind(binding.project_id)
        .bind(requirement.owner_team)
        .fetch_optional(&mut *tx)
        .await?;
        if user.is_none() || !authorized || project.is_none() {
            return Err(Error::Invalid(
                "workflow validation actor or project is not authorized",
            ));
        }
        let team = requirement.owner_team.to_string();
        let existing: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM thalamus_runs WHERE team=$1 AND idempotency_key=$2")
                .bind(&team)
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?;
        let run = if let Some(id) = existing {
            // Only the gate can create this binding, in the same transaction
            // as the run. Do not adopt a caller-created run that squatted the
            // deterministic idempotency key before rollout validation.
            let bound: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM thalamus_rollout_runs WHERE run_id=$1 AND change_id=$2 AND stage=$3)",
            )
            .bind(id).bind(requirement.change_id).bind(stage_name(requirement.stage))
            .fetch_one(&mut *tx).await?;
            if !bound {
                return Err(Error::IdempotencyConflict);
            }
            self.runs
                .get_in_transaction(&mut tx, &team, id)
                .await
                .map_err(run_error)?
        } else {
            let Some((snapshot, profile, test)) = admission else {
                tx.commit().await?;
                return Ok(None);
            };
            let config = resolve_runner_config(
                &snapshot,
                &requirement.deployment,
                &RunKind::Validation,
                stage_name(requirement.stage),
                &binding.public,
                &test.options,
            )
            .map_err(|_| Error::Invalid("workflow test profile does not match observed route"))?;
            thalamus_runs::validate_observed_validation_config(&config).map_err(|_| {
                Error::Invalid("workflow validation configuration is not executable")
            })?;
            let evidence = thalamus_runtime::admission::request_evidence(
                &snapshot,
                &requirement.deployment,
                &profile.runner,
                &profile.workload,
                &test.options,
            )
            .map_err(|_| Error::Invalid("workflow admission observation is incomplete"))?;
            let request = RunRequest {
                project_id: profile.project_id,
                kind: profile.kind,
                service: profile.service,
                deployment: requirement.deployment.clone(),
                runner: profile.runner,
                workload: profile.workload,
                evidence,
            };
            request
                .validate()
                .map_err(|_| Error::Invalid("invalid workflow validation profile"))?;
            self.runs
                .create_in_transaction(&mut tx, &team, &requirement.actor, key, &request)
                .await
                .map_err(run_error)?
        };
        if !run_matches(&run, requirement, binding, key) {
            return Err(Error::IdempotencyConflict);
        }
        // The worker can discover a run immediately after commit; bind the
        // authorized rollout before making the new run visible to it.
        self.changes
            .bind_validation_run_in_transaction(&mut tx, requirement, binding.project_id, run.id)
            .await?;
        tx.commit().await?;
        Ok(Some(run))
    }

    /// Verify historical execution without requiring that its old route or
    /// Flux commit is still current. This is also used for candidate proof
    /// after promotion has deliberately changed routing and the catalog.
    async fn verified_completed(
        &self,
        requirement: &ValidationRequirement,
        route: &str,
        run: &Run,
    ) -> Result<Option<VerifiedExecution>> {
        if run.state != "succeeded" {
            return Ok(None);
        }
        let backend = format!(
            "{}/{}/{}",
            requirement.service, requirement.deployment, requirement.revision
        );
        let Some(job_uid) = run.job_uid.as_deref().filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        if run.job_name != thalamus_runs::job_name(run.id)
            || run.job_namespace.as_deref() != Some(self.config.namespace.as_str())
        {
            return Ok(None);
        }
        let Some(spec) = run.job_spec.as_ref() else {
            return Ok(None);
        };
        let job: Job = match serde_json::from_value(serde_json::json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": run.job_name, "namespace": run.job_namespace},
            "spec": spec,
        })) {
            Ok(job) => job,
            Err(_) => return Ok(None),
        };
        let Ok(config) = thalamus_runtime::admission::options_from_job(run, &job) else {
            return Ok(None);
        };
        let Ok(agentic) = requires_agentic(&RunKind::Validation, &config) else {
            return Ok(None);
        };
        let evidence = match self.records.get_verified(run.id).await {
            Ok(Some(evidence)) => evidence,
            Ok(None) => return Ok(None),
            Err(thalamus_runtime::records::Error::Database(error)) => {
                return Err(Error::Database(error))
            }
            Err(_) => return Ok(None),
        };
        if evidence.run_id != run.id
            || evidence.request_fingerprint != run.fingerprint
            || evidence.runner.job != run.job_name
            || evidence.runner.job_uid != job_uid
            || evidence.runner.pod_owner_job_uid != job_uid
            || evidence.runner.pod_uid.is_empty()
            || evidence.runner.image_id.is_empty()
            || !evidence.prelaunch.same_identity(&evidence.ingestion)
            || !route_matches_mode(requirement, route, &evidence.prelaunch, agentic)
            || !route_matches_mode(requirement, route, &evidence.ingestion, agentic)
            || !snapshot_matches(
                &evidence.prelaunch,
                requirement,
                &evidence.prelaunch.route,
                &backend,
            )
            || !snapshot_matches(
                &evidence.ingestion,
                requirement,
                &evidence.prelaunch.route,
                &backend,
            )
            || verify_validation_job(
                run,
                &job,
                &evidence.prelaunch,
                stage_name(requirement.stage),
                &evidence.prelaunch.route,
                &backend,
            )
            .is_err()
            || verify_validation_job(
                run,
                &job,
                &evidence.ingestion,
                stage_name(requirement.stage),
                &evidence.prelaunch.route,
                &backend,
            )
            .is_err()
        {
            return Ok(None);
        }
        let Ok(options) = ValidationOptions::deserialize(&config) else {
            return Ok(None);
        };
        let refs = match run.artifacts.as_deref() {
            Some([artifact]) => artifact,
            _ => return Ok(None),
        };
        let bytes = match self.artifacts.read(run.id, refs).await {
            Ok(bytes) => bytes,
            Err(thalamus_runs::Error::ArtifactIntegrity) => return Ok(None),
            Err(thalamus_runs::Error::Database(error)) => return Err(Error::Database(error)),
            Err(_) => return Err(Error::Invalid("workflow validation artifact unavailable")),
        };
        let summary: ValidationSummary = match serde_json::from_slice(&bytes) {
            Ok(summary) => summary,
            Err(_) => return Ok(None),
        };
        if !summary_matches(&summary, run, requirement.stage, &backend, &options) {
            return Ok(None);
        }
        Ok(Some(VerifiedExecution {
            evidence,
            options: config,
            agentic,
        }))
    }

    async fn candidate_proof(
        &self,
        requirement: &ValidationRequirement,
        binding: &ServiceRoutes,
    ) -> Result<Option<VerifiedExecution>> {
        let (id, commit) = match (
            requirement.candidate_run_id,
            requirement.candidate_catalog_commit.as_deref(),
        ) {
            (None, None) => return Ok(None),
            (Some(id), Some(commit)) => (id, commit),
            _ => return Err(Error::Invalid("invalid candidate validation prerequisite")),
        };
        // Anchor the candidate run and commit in this rollout's durable state,
        // not merely in the requirement or a caller-supplied run identifier.
        let anchored: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM thalamus_rollouts r JOIN thalamus_rollout_stages s ON s.change_id=r.change_id WHERE r.change_id=$1 AND r.candidate_validation_run=$2 AND s.stage='candidate_pr' AND s.merged_commit=$3)",
        )
        .bind(requirement.change_id).bind(id).bind(commit)
        .fetch_one(&self.db).await?;
        if !anchored {
            return Err(Error::Invalid("candidate validation proof unavailable"));
        }
        let mut candidate = requirement.clone();
        candidate.stage = ValidationStage::Candidate;
        candidate.catalog_commit = commit.to_owned();
        candidate.candidate_run_id = None;
        candidate.candidate_catalog_commit = None;
        let candidate_route = format!("{}-candidate", candidate.deployment);
        let route = expected_route(binding, &candidate, Some(&candidate_route))?;
        let run = self
            .runs
            .get(&requirement.owner_team.to_string(), id)
            .await
            .map_err(run_error)?;
        if !run_matches(&run, &candidate, binding, &idempotency_key(&candidate)) {
            return Err(Error::Invalid(
                "candidate validation run does not belong to rollout",
            ));
        }
        self.verified_completed(&candidate, &route, &run)
            .await?
            .map(Some)
            .ok_or(Error::Invalid("candidate validation proof unavailable"))
    }

    async fn completed(
        &self,
        requirement: &ValidationRequirement,
        route: &str,
        run: &Run,
        candidate: Option<&VerifiedExecution>,
        observer: &LiveObserver,
    ) -> Result<ValidationProgress> {
        let failed = || ValidationProgress::Failed { run_id: run.id };
        let Some(verified) = self.verified_completed(requirement, route, run).await? else {
            return Ok(failed());
        };
        if candidate.is_some_and(|old| {
            !same_validation_options(
                old.agentic,
                &old.options,
                verified.agentic,
                &verified.options,
            ) || !same_serving_revision(&old.evidence.ingestion, &verified.evidence.ingestion)
        }) {
            return Ok(failed());
        }
        let agentic = verified.agentic;
        let evidence = &verified.evidence;
        let backend = format!(
            "{}/{}/{}",
            requirement.service, requirement.deployment, requirement.revision
        );
        // Read live state after all durable and artifact reads. Requiring the
        // entire identity prevents reusing a result after a route/runtime change.
        let current = observe_stage(observer, requirement, route, agentic)
            .await
            .map_err(|_| Error::Invalid("current workflow route observation unavailable"))?;
        if !route_matches_mode(requirement, route, &current, agentic)
            || !snapshot_matches(&current, requirement, &evidence.ingestion.route, &backend)
            || !evidence.ingestion.same_identity(&current)
        {
            return Ok(failed());
        }
        Ok(ValidationProgress::Passed {
            run_id: run.id,
            catalog_commit: requirement.catalog_commit.clone(),
            backend,
        })
    }

    /// The scheduler has verified the reviewed base and target renders. This
    /// independently observes the live workload and drained filler controllers;
    /// operator settings or the proposal itself are never treated as proof.
    async fn verify_in_place_inner(
        &self,
        requirement: &InPlaceSafetyRequirement,
    ) -> Result<InPlaceSafety> {
        let binding = self
            .config
            .routes
            .get(&requirement.service)
            .ok_or(Error::Invalid("in-place service has no runtime binding"))?;
        if requirement.change_id.is_nil()
            || requirement.owner_team.is_nil()
            || requirement.namespace != self.config.namespace
            || binding.team_id != requirement.owner_team
            || binding.service.name != requirement.service
            || !valid_commit(&requirement.base_commit)
            || requirement.revision.len() != 64
            || !valid_commit(&requirement.revision)
        {
            return Err(Error::Invalid("invalid in-place safety identity"));
        }
        if !self.changes.authorize_in_place_safety(requirement).await? {
            return Ok(InPlaceSafety::Pending);
        }
        let before = match self
            .observer
            .observe_route(&requirement.service, &binding.public)
            .await
        {
            Ok(snapshot) if in_place_route_matches(&snapshot, requirement, &binding.public) => {
                snapshot
            }
            _ => return Ok(InPlaceSafety::Pending),
        };
        let observed = match self
            .observer
            .observe_filler_state(&requirement.service, &requirement.deployment)
            .await
        {
            Ok(observed) => observed,
            // Missing, changing or inaccessible resources cannot prove a drain.
            Err(_) => return Ok(InPlaceSafety::Pending),
        };
        let commit = match flux_commit(&observed.flux_revision) {
            Some(commit) if commit == requirement.base_commit => commit.to_owned(),
            _ => return Ok(InPlaceSafety::Pending),
        };
        if observed.service != requirement.service
            || observed.deployment != requirement.deployment
            || observed.filler.strip_suffix("-filler") != Some(requirement.deployment.as_str())
            || observed.revision != requirement.revision
            || observed.serving_uid.is_empty()
            || observed.filler_uid.is_empty()
            || observed.serving_uid == observed.filler_uid
            || observed.pool.is_empty()
            || observed.pool_uid.is_empty()
            || observed.queue.is_empty()
            || !observed.verified_zero()
        {
            return Ok(InPlaceSafety::Pending);
        }
        let after = match self
            .observer
            .observe_route(&requirement.service, &binding.public)
            .await
        {
            Ok(snapshot) if in_place_route_matches(&snapshot, requirement, &binding.public) => {
                snapshot
            }
            _ => return Ok(InPlaceSafety::Pending),
        };
        if !before.same_identity(&after)
            || before.observed_at > observed.observed_at
            || observed.observed_at > after.observed_at
        {
            return Ok(InPlaceSafety::Pending);
        }
        // Do not hold an authorization transaction across Kubernetes calls.
        // Recheck it afterwards; the scheduler rechecks under lock at merge.
        if !self.changes.authorize_in_place_safety(requirement).await? {
            return Ok(InPlaceSafety::Pending);
        }
        let now = chrono::Utc::now();
        if after.observed_at > now
            || now.signed_duration_since(before.observed_at) > chrono::Duration::seconds(30)
        {
            return Ok(InPlaceSafety::Pending);
        }
        Ok(InPlaceSafety::Ready {
            observed_at: before.observed_at,
            catalog_commit: commit,
            deployment: observed.deployment,
            revision: observed.revision,
        })
    }
}

impl ValidationGate for WorkflowValidation {
    fn reconcile<'a>(
        &'a self,
        requirement: &'a ValidationRequirement,
    ) -> Pin<Box<dyn Future<Output = Result<ValidationProgress>> + Send + 'a>> {
        Box::pin(self.reconcile_inner(requirement))
    }

    fn verify_in_place<'a>(
        &'a self,
        requirement: &'a InPlaceSafetyRequirement,
    ) -> Pin<Box<dyn Future<Output = Result<InPlaceSafety>> + Send + 'a>> {
        Box::pin(self.verify_in_place_inner(requirement))
    }
}

// The candidate selector is derived only after the persisted rollout has
// authorized this requirement. Public routing uses the configured public name.
async fn observe_stage(
    observer: &LiveObserver,
    requirement: &ValidationRequirement,
    public_route: &str,
    agentic: bool,
) -> thalamus_runtime::Result<RouteSnapshot> {
    match (requirement.stage, agentic) {
        (ValidationStage::Candidate, true) => {
            observer
                .observe_agentic_candidate(&requirement.service, &requirement.deployment)
                .await
        }
        (ValidationStage::Public, true) => {
            observer.observe_agentic_public(&requirement.service).await
        }
        (ValidationStage::Candidate, false) => {
            observer
                .observe_candidate(&requirement.service, &requirement.deployment)
                .await
        }
        (ValidationStage::Public, false) => {
            observer
                .observe_route(&requirement.service, public_route)
                .await
        }
    }
}

fn route_matches_mode(
    requirement: &ValidationRequirement,
    public_route: &str,
    snapshot: &RouteSnapshot,
    agentic: bool,
) -> bool {
    snapshot.service == requirement.service
        && thalamus_runtime::admission::matches_route(
            snapshot,
            &requirement.deployment,
            &RunKind::Validation,
            stage_name(requirement.stage),
            public_route,
            agentic,
        )
}

fn run_error(error: thalamus_runs::Error) -> Error {
    match error {
        thalamus_runs::Error::Database(error) => Error::Database(error),
        thalamus_runs::Error::IdempotencyConflict => Error::IdempotencyConflict,
        _ => Error::Invalid("workflow validation run unavailable"),
    }
}

fn stage_name(stage: ValidationStage) -> &'static str {
    match stage {
        ValidationStage::Candidate => "candidate",
        ValidationStage::Public => "public",
    }
}

fn idempotency_key(requirement: &ValidationRequirement) -> String {
    format!(
        "rollout:{}:{}:{}",
        requirement.change_id,
        stage_name(requirement.stage),
        requirement.catalog_commit
    )
}

fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn validate_requirement(requirement: &ValidationRequirement) -> Result<()> {
    let valid_part = |value: &str| {
        !value.is_empty()
            && value.trim() == value
            && !value.contains('/')
            && !value.chars().any(char::is_control)
    };
    if requirement.change_id.is_nil()
        || requirement.owner_team.is_nil()
        || !valid_part(&requirement.service)
        || !valid_part(&requirement.deployment)
        || !valid_part(&requirement.revision)
        || !valid_commit(&requirement.catalog_commit)
    {
        return Err(Error::Invalid("invalid workflow validation requirement"));
    }
    match (
        requirement.stage,
        requirement.candidate_run_id,
        &requirement.candidate_catalog_commit,
    ) {
        (_, None, None) => {}
        (ValidationStage::Public, Some(id), Some(commit))
            if !id.is_nil() && valid_commit(commit) => {}
        _ => return Err(Error::Invalid("invalid candidate validation prerequisite")),
    }
    Ok(())
}

fn expected_route(
    binding: &ServiceRoutes,
    requirement: &ValidationRequirement,
    authorized_route: Option<&str>,
) -> Result<String> {
    if binding.team_id != requirement.owner_team
        || binding.project_id.is_nil()
        || binding.service.name != requirement.service
        || binding.public.is_empty()
    {
        return Err(Error::Invalid(
            "workflow service ownership or route binding mismatch",
        ));
    }
    let candidate = format!("{}-candidate", requirement.deployment);
    if binding.public == candidate {
        return Err(Error::Invalid(
            "candidate and public routes must be distinct",
        ));
    }
    match requirement.stage {
        ValidationStage::Public if authorized_route.is_none() => Ok(binding.public.clone()),
        ValidationStage::Candidate if authorized_route == Some(candidate.as_str()) => Ok(candidate),
        _ => Err(Error::Invalid(
            "authorized workflow route does not match stage",
        )),
    }
}

fn in_place_route_matches(
    snapshot: &RouteSnapshot,
    requirement: &InPlaceSafetyRequirement,
    public_route: &str,
) -> bool {
    if snapshot.service != requirement.service
        || snapshot.route != public_route
        || snapshot.deployments.len() != 1
        || snapshot.weights.len() != 1
        || snapshot
            .weights
            .get(&requirement.deployment)
            .is_none_or(|weight| *weight == 0)
    {
        return false;
    }
    let target = &snapshot.deployments[0];
    let mut backend = target.backend.split('/');
    target.deployment == requirement.deployment
        && target.revision == requirement.revision
        && backend.next() == Some(requirement.service.as_str())
        && backend.next() == Some(requirement.deployment.as_str())
        && backend.next() == Some(requirement.revision.as_str())
        && backend.next().is_none()
        && flux_commit(&target.flux_revision) == Some(requirement.base_commit.as_str())
}

fn run_matches(
    run: &Run,
    requirement: &ValidationRequirement,
    binding: &ServiceRoutes,
    key: &str,
) -> bool {
    run.id != Uuid::nil()
        && run.team == requirement.owner_team.to_string()
        && run.actor == requirement.actor
        && run.idempotency_key == key
        && run.request.project_id == binding.project_id
        && run.request.service == requirement.service
        && run.request.deployment == requirement.deployment
        && run.request.kind == RunKind::Validation
        && run.request.workload.name == "functional"
        && run
            .request
            .fingerprint()
            .is_ok_and(|fingerprint| fingerprint == run.fingerprint)
}

async fn read_profiles(path: &Path) -> Result<Vec<RunProfile>> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| Error::Invalid("workflow validation profiles unavailable"))?;
    let mut bytes = Vec::new();
    file.take(MAX_PROFILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Error::Invalid("workflow validation profiles unavailable"))?;
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(Error::Invalid("workflow validation profiles too large"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::Invalid("invalid workflow validation profiles"))
}

fn select_profile(
    profiles: Vec<RunProfile>,
    requirement: &ValidationRequirement,
    project: Uuid,
    runner_targets: &BTreeMap<String, String>,
) -> Result<ProfileRequest> {
    let mut matches = profiles.into_iter().filter(|profile| {
        profile.team_id == requirement.owner_team
            && profile.request.project_id == project
            && profile.request.service == requirement.service
            && profile.request.kind == RunKind::Validation
            && runner_targets
                .get(&profile.request.runner)
                .map(String::as_str)
                == Some(stage_name(requirement.stage))
    });
    let profile = matches
        .next()
        .ok_or(Error::Invalid("no trusted workflow validation profile"))?;
    if matches.next().is_some() || profile.request.workload.name != "functional" {
        return Err(Error::Invalid(
            "ambiguous or invalid workflow validation profile",
        ));
    }
    Ok(profile.request)
}

fn flux_commit(revision: &str) -> Option<&str> {
    let (reference, hash) = revision.rsplit_once('@')?;
    if reference.trim().is_empty()
        || reference.trim() != reference
        || reference.chars().any(char::is_control)
    {
        return None;
    }
    let (algorithm, commit) = hash.split_once(':')?;
    if !matches!((algorithm, commit.len()), ("sha1", 40) | ("sha256", 64)) || !valid_commit(commit)
    {
        return None;
    }
    Some(commit)
}

fn snapshot_matches(
    snapshot: &RouteSnapshot,
    requirement: &ValidationRequirement,
    route: &str,
    backend: &str,
) -> bool {
    if snapshot.service != requirement.service
        || snapshot.route != route
        || snapshot.deployments.len() != 1
        || snapshot.weights.len() != 1
        || snapshot
            .weights
            .get(&requirement.deployment)
            .is_none_or(|weight| *weight == 0)
    {
        return false;
    }
    let target = &snapshot.deployments[0];
    if snapshot.agentic != target.underlying_model.is_some() {
        return false;
    }
    target.deployment == requirement.deployment
        && target.revision == requirement.revision
        && target.backend == backend
        && flux_commit(&target.flux_revision) == Some(requirement.catalog_commit.as_str())
}

/// Promotion may change routing, Flux revision and scaling, but must preserve
/// the independently observed normalized serving revision and runtime.
fn same_serving_revision(candidate: &RouteSnapshot, public: &RouteSnapshot) -> bool {
    if candidate.service != public.service
        || candidate.deployments.len() != 1
        || public.deployments.len() != 1
    {
        return false;
    }
    candidate.deployments[0].same_serving_revision(&public.deployments[0])
}

fn same_validation_options(
    candidate_agentic: bool,
    candidate: &Value,
    public_agentic: bool,
    public: &Value,
) -> bool {
    candidate_agentic == public_agentic && candidate == public
}

#[derive(Deserialize)]
struct ValidationOptions {
    tool_parser: Option<String>,
    reasoning: bool,
    responses_continuation: bool,
    #[serde(default)]
    responses: bool,
    #[serde(default)]
    anthropic: bool,
    context_tokens: u32,
}

#[derive(Deserialize)]
struct ValidationSummary {
    run_id: Uuid,
    request: RunRequest,
    suite: String,
    runner_version: String,
    tool_parser: Option<String>,
    answering_backends: Vec<String>,
    checks: Vec<ValidationCheck>,
    passed: bool,
}

#[derive(Deserialize)]
struct ValidationCheck {
    name: String,
    status: String,
    detail: String,
    semantics: String,
}

fn summary_matches(
    summary: &ValidationSummary,
    run: &Run,
    stage: ValidationStage,
    backend: &str,
    options: &ValidationOptions,
) -> bool {
    if summary.run_id != run.id
        || summary.request != run.request
        || summary.suite != stage_name(stage)
        || summary.runner_version.trim().is_empty()
        || !summary.passed
        || summary.tool_parser != options.tool_parser
        || summary.answering_backends.len() != 1
        || summary.answering_backends[0] != backend
        || options.context_tokens == 0
        || options.context_tokens > run.request.workload.model_budget_tokens
    {
        return false;
    }
    let mut checks = BTreeMap::new();
    for check in &summary.checks {
        if check.name.trim().is_empty()
            || check.detail.trim().is_empty()
            || check.semantics.trim().is_empty()
            || !matches!(check.status.as_str(), "passed" | "unsupported")
            || checks.insert(check.name.as_str(), check).is_some()
        {
            return false;
        }
    }
    let status = |name: &str, expected: &str| {
        checks
            .get(name)
            .is_some_and(|check| check.status == expected)
    };
    let optional =
        |name: &str, enabled: bool| status(name, if enabled { "passed" } else { "unsupported" });
    for name in [
        "authentication",
        "authorization",
        "chat",
        "streaming",
        "context_limit",
    ] {
        if !status(name, "passed") {
            return false;
        }
    }
    if !optional("tool_loop", options.tool_parser.is_some())
        || !optional("reasoning", options.reasoning)
        || !optional("responses_continuation", options.responses_continuation)
    {
        return false;
    }
    let context = |name: &str| {
        checks
            .get(name)
            .and_then(|check| check.detail.strip_prefix("accepted "))
            .and_then(|detail| {
                detail.strip_suffix(" prompt tokens; quality under load not measured")
            })
            .and_then(|count| count.parse::<u32>().ok())
            .is_some_and(|count| {
                count >= options.context_tokens && count <= run.request.workload.model_budget_tokens
            })
    };
    if !context("context_limit") {
        return false;
    }
    for (prefix, enabled) in [
        ("responses", options.responses),
        ("anthropic", options.anthropic),
    ] {
        for suffix in [
            "",
            "_authentication",
            "_authorization",
            "_streaming",
            "_tool_loop",
            "_context_limit",
        ] {
            let name = format!("{prefix}{suffix}");
            let required = enabled && (suffix != "_tool_loop" || options.tool_parser.is_some());
            if !optional(&name, required) {
                return false;
            }
            if suffix == "_context_limit" && enabled && !context(&name) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn requirement(stage: ValidationStage) -> ValidationRequirement {
        ValidationRequirement {
            change_id: Uuid::from_u128(1),
            owner_team: Uuid::from_u128(2),
            actor: Uuid::from_u128(3).to_string(),
            stage,
            service: "service".into(),
            deployment: "model".into(),
            revision: "revision-1".into(),
            catalog_commit: "a".repeat(40),
            candidate_run_id: None,
            candidate_catalog_commit: None,
        }
    }

    fn request(runner: &str) -> RunRequest {
        serde_json::from_value(json!({
            "project_id": Uuid::from_u128(4),
            "kind": "validation", "service": "service", "deployment": "model", "runner": runner,
            "workload": {
                "name": "functional", "seed": 1, "concurrency": 1, "duration_seconds": 120,
                "warmup_seconds": 0, "max_requests": 100, "model_budget_tokens": 1000,
                "tool_budget_calls": 10
            },
            "evidence": {
                "desired_configuration": "asserted", "observed_runtime": "asserted",
                "test_definition": "asserted",
                "conditions": {
                    "hardware": "asserted", "nodes": ["node"], "replicas": 1,
                    "cache_state": "unknown", "competing_traffic": "unknown",
                    "observed_at": "2026-01-01T00:00:00Z"
                }
            }
        }))
        .unwrap()
    }

    fn profile_request(runner: &str) -> ProfileRequest {
        let request = request(runner);
        ProfileRequest {
            project_id: request.project_id,
            kind: request.kind,
            service: request.service,
            runner: request.runner,
            workload: request.workload,
        }
    }

    fn run() -> Run {
        let req = requirement(ValidationStage::Candidate);
        let request = request("candidate-runner");
        let now = chrono::DateTime::from_timestamp(0, 0).unwrap();
        Run {
            id: Uuid::from_u128(5),
            team: req.owner_team.to_string(),
            actor: req.actor.clone(),
            idempotency_key: idempotency_key(&req),
            fingerprint: request.fingerprint().unwrap(),
            request,
            job_name: thalamus_runs::job_name(Uuid::from_u128(5)),
            job_uid: Some("job-uid".into()),
            ownership_nonce: Uuid::from_u128(6),
            job_spec: None,
            job_namespace: Some("namespace".into()),
            state: "succeeded".into(),
            artifacts: None,
            error: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn options() -> ValidationOptions {
        ValidationOptions {
            tool_parser: None,
            reasoning: false,
            responses_continuation: false,
            responses: false,
            anthropic: false,
            context_tokens: 100,
        }
    }

    fn summary(run: &Run, stage: ValidationStage) -> ValidationSummary {
        let mut checks = Vec::new();
        let mut add = |name: &str, status: &str, detail: &str| {
            checks.push(ValidationCheck {
                name: name.into(),
                status: status.into(),
                detail: detail.into(),
                semantics: "client".into(),
            });
        };
        for name in ["authentication", "authorization", "chat", "streaming"] {
            add(name, "passed", "observed");
        }
        add(
            "context_limit",
            "passed",
            "accepted 100 prompt tokens; quality under load not measured",
        );
        for name in ["tool_loop", "reasoning", "responses_continuation"] {
            add(name, "unsupported", "not configured");
        }
        for prefix in ["responses", "anthropic"] {
            for suffix in [
                "",
                "_authentication",
                "_authorization",
                "_streaming",
                "_tool_loop",
                "_context_limit",
            ] {
                add(
                    &format!("{prefix}{suffix}"),
                    "unsupported",
                    "not configured",
                );
            }
        }
        ValidationSummary {
            run_id: run.id,
            request: run.request.clone(),
            suite: stage_name(stage).into(),
            runner_version: "1".into(),
            tool_parser: None,
            answering_backends: vec!["service/model/revision-1".into()],
            checks,
            passed: true,
        }
    }

    fn route(commit: &str) -> RouteSnapshot {
        serde_json::from_value(json!({
            "observed_at": "2026-01-01T00:00:00Z",
            "service": "service", "route": "model-candidate",
            "hostnames": ["example.test"], "paths": ["/v1/chat/completions"], "ports": [443],
            "listeners": [{"hostname": "example.test", "port": 443, "protocol": "HTTPS"}],
            "weights": {"model": 1},
            "deployments": [{
                "deployment": "model", "revision": "revision-1",
                "flux_revision": format!("main@sha1:{commit}"), "backend": "service/model/revision-1",
                "rollout_configuration": format!("sha256:{}", "c".repeat(64)),
                "desired": {"rendered_resources": "x", "resolved_dependencies": "y"},
                "runtime": {"image_digests": {}, "model_content": "x", "tokenizer_content": "y",
                    "effective_configuration": "z", "component_versions": {}},
                "engine_spec": {"name": "engine", "image": "image"},
                "nodes": ["node"]
            }]
        })).unwrap()
    }

    #[test]
    fn stage_keys_are_stable_and_distinct() {
        let candidate = requirement(ValidationStage::Candidate);
        let public = requirement(ValidationStage::Public);
        assert_eq!(
            idempotency_key(&candidate),
            format!(
                "rollout:{}:candidate:{}",
                candidate.change_id, candidate.catalog_commit
            )
        );
        assert_ne!(idempotency_key(&candidate), idempotency_key(&public));
        let mut changed = candidate.clone();
        changed.catalog_commit = "b".repeat(40);
        assert_ne!(idempotency_key(&candidate), idempotency_key(&changed));
    }

    #[test]
    fn agentic_route_mode_is_bound_to_stage_and_observed_chain() {
        let candidate = requirement(ValidationStage::Candidate);
        let mut snapshot = route(&candidate.catalog_commit);
        assert!(route_matches_mode(
            &candidate,
            "model-candidate",
            &snapshot,
            false
        ));
        assert!(!route_matches_mode(
            &candidate,
            "model-candidate",
            &snapshot,
            true
        ));
        snapshot.agentic = true;
        snapshot.route = format!("agentic-cand-{}", "a".repeat(32));
        assert!(route_matches_mode(
            &candidate,
            "model-candidate",
            &snapshot,
            true
        ));
        assert!(!route_matches_mode(
            &candidate,
            "model-candidate",
            &snapshot,
            false
        ));
        assert!(!snapshot_matches(
            &snapshot,
            &candidate,
            &snapshot.route,
            "service/model/revision-1"
        ));
        let underlying = thalamus_runtime::UnderlyingModelIdentity {
            rollout_configuration: snapshot.deployments[0].rollout_configuration.clone(),
            runtime: snapshot.deployments[0].runtime.clone(),
        };
        snapshot.deployments[0].underlying_model = Some(underlying);
        assert!(snapshot_matches(
            &snapshot,
            &candidate,
            &snapshot.route,
            "service/model/revision-1"
        ));
        let public = requirement(ValidationStage::Public);
        assert!(!route_matches_mode(&public, "service", &snapshot, true));
        snapshot.route = "service-agent".into();
        assert!(route_matches_mode(&public, "service", &snapshot, true));
        snapshot.agentic = false;
        assert!(!route_matches_mode(&public, "service", &snapshot, true));
    }

    #[test]
    fn resume_reads_agentic_mode_from_persisted_job() {
        let run = run();
        let config = json!({
            "base_url": "https://agent.example/v1", "model": "service", "suite": "candidate",
            "backend_header": "x-thalamus-backend", "expected_backend": "service/model/revision-1",
            "forbidden_model": "forbidden", "context_prompt": "test", "context_tokens": 100,
            "tool_parser": null, "reasoning": false, "responses": false, "anthropic": false,
            "responses_continuation": true, "timeout_seconds": 30
        });
        let mut job: Job = serde_json::from_value(json!({
            "apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": run.job_name},
            "spec": {"template": {"spec": {"containers": [{
                "name": "runner", "image": "runner", "env": [
                    {"name": "THALAMUS_RUN_ID", "value": run.id.to_string()},
                    {"name": "THALAMUS_RUN_REQUEST", "value": serde_json::to_string(&run.request).unwrap()},
                    {"name": "THALAMUS_VALIDATION_CONFIG", "value": config.to_string()}
                ]
            }]}}}
        })).unwrap();
        let actual = thalamus_runtime::admission::options_from_job(&run, &job).unwrap();
        assert!(requires_agentic(&RunKind::Validation, &actual).unwrap());
        let runner = &mut job
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .containers[0];
        let duplicate = runner.env.as_ref().unwrap()[2].clone();
        runner.env.as_mut().unwrap().push(duplicate);
        assert!(thalamus_runtime::admission::options_from_job(&run, &job).is_err());
    }

    #[test]
    fn candidate_prerequisites_must_be_a_complete_pair_for_public_stage() {
        let mut public = requirement(ValidationStage::Public);
        assert!(validate_requirement(&public).is_ok());
        public.candidate_run_id = Some(Uuid::from_u128(5));
        assert!(validate_requirement(&public).is_err());
        public.candidate_catalog_commit = Some("b".repeat(40));
        assert!(validate_requirement(&public).is_ok());
        public.stage = ValidationStage::Candidate;
        assert!(validate_requirement(&public).is_err());
    }

    #[test]
    fn profile_contains_planned_inputs_only() {
        let mut request = serde_json::to_value(request("candidate-runner")).unwrap();
        request.as_object_mut().unwrap().remove("evidence");
        request.as_object_mut().unwrap().remove("deployment");
        let mut profile = json!({"team_id": Uuid::from_u128(2), "request": request});
        assert!(serde_json::from_value::<RunProfile>(profile.clone()).is_ok());
        let mut with_evidence = profile.clone();
        with_evidence["request"]["evidence"] = json!({"fabricated": true});
        assert!(serde_json::from_value::<RunProfile>(with_evidence).is_err());
        profile["request"]["deployment"] = json!("caller-selected");
        assert!(serde_json::from_value::<RunProfile>(profile).is_err());
    }

    #[test]
    fn trusted_profile_is_unique_and_stage_and_project_scoped() {
        let req = requirement(ValidationStage::Candidate);
        let targets = BTreeMap::from([
            ("candidate-runner".into(), "candidate".into()),
            ("public-runner".into(), "public".into()),
        ]);
        let profiles = || {
            vec![
                RunProfile {
                    team_id: req.owner_team,
                    request: profile_request("candidate-runner"),
                },
                RunProfile {
                    team_id: req.owner_team,
                    request: profile_request("public-runner"),
                },
            ]
        };
        assert_eq!(
            select_profile(profiles(), &req, Uuid::from_u128(4), &targets)
                .unwrap()
                .runner,
            "candidate-runner"
        );
        let public = requirement(ValidationStage::Public);
        assert_eq!(
            select_profile(profiles(), &public, Uuid::from_u128(4), &targets)
                .unwrap()
                .runner,
            "public-runner"
        );
        assert!(select_profile(profiles(), &req, Uuid::from_u128(9), &targets).is_err());
        let mut other_project = profile_request("candidate-runner");
        other_project.project_id = Uuid::from_u128(9);
        let mut scoped = profiles();
        scoped.push(RunProfile {
            team_id: req.owner_team,
            request: other_project,
        });
        assert!(select_profile(scoped, &req, Uuid::from_u128(4), &targets).is_ok());
        let mut ambiguous = profiles();
        ambiguous.push(RunProfile {
            team_id: req.owner_team,
            request: profile_request("candidate-runner"),
        });
        assert!(select_profile(ambiguous, &req, Uuid::from_u128(4), &targets).is_err());
    }

    #[test]
    fn flux_revision_and_snapshot_must_bind_exact_commit_and_target() {
        let req = requirement(ValidationStage::Candidate);
        let good = format!("main@sha1:{}", req.catalog_commit);
        assert_eq!(flux_commit(&good), Some(req.catalog_commit.as_str()));
        assert!(flux_commit(&format!("@sha1:{}", req.catalog_commit)).is_none());
        assert!(flux_commit(&format!("main@sha256:{}", req.catalog_commit)).is_none());
        assert!(flux_commit(&format!("main@sha1:{}", "A".repeat(40))).is_none());
        assert_eq!(
            flux_commit(&format!("main@sha256:{}", "b".repeat(64)))
                .unwrap()
                .len(),
            64
        );
        let mut observed = route(&req.catalog_commit);
        assert!(snapshot_matches(
            &observed,
            &req,
            "model-candidate",
            "service/model/revision-1"
        ));
        observed.deployments[0].flux_revision = format!("main@sha1:{}", "b".repeat(40));
        assert!(!snapshot_matches(
            &observed,
            &req,
            "model-candidate",
            "service/model/revision-1"
        ));
        observed = route(&req.catalog_commit);
        observed.deployments.push(observed.deployments[0].clone());
        assert!(!snapshot_matches(
            &observed,
            &req,
            "model-candidate",
            "service/model/revision-1"
        ));
        observed = route(&req.catalog_commit);
        observed.weights.insert(req.deployment.clone(), 0);
        assert!(!snapshot_matches(
            &observed,
            &req,
            "model-candidate",
            "service/model/revision-1"
        ));
        assert!(!snapshot_matches(
            &route(&req.catalog_commit),
            &req,
            "public",
            "service/model/revision-1"
        ));
    }

    #[test]
    fn promotion_preserves_the_observed_serving_revision() {
        let candidate = route(&"a".repeat(40));
        let mut public = candidate.clone();
        public.route = "public".into();
        public.deployments[0].flux_revision = format!("main@sha1:{}", "b".repeat(40));
        assert!(same_serving_revision(&candidate, &public));
        public.deployments[0].engine_spec["image"] = json!("different");
        assert!(!same_serving_revision(&candidate, &public));
        public = candidate.clone();
        public.deployments[0].nodes = vec!["different-node".into()];
        assert!(same_serving_revision(&candidate, &public));
        public.deployments[0].desired.rendered_resources = "different-replica-topology".into();
        assert!(same_serving_revision(&candidate, &public));
        public.deployments[0].rollout_configuration = format!("sha256:{}", "d".repeat(64));
        assert!(!same_serving_revision(&candidate, &public));
        public = candidate.clone();
        public.deployments[0].runtime.model_content = "different".into();
        assert!(!same_serving_revision(&candidate, &public));
    }

    #[test]
    fn mixed_agentic_and_direct_stages_cannot_share_promotion_proof() {
        let candidate = route(&"a".repeat(40));
        let mut public = candidate.clone();
        public.agentic = true;
        public.route = "service-agent".into();
        let model = &candidate.deployments[0];
        public.deployments[0].underlying_model = Some(thalamus_runtime::UnderlyingModelIdentity {
            rollout_configuration: model.rollout_configuration.clone(),
            runtime: model.runtime.clone(),
        });
        public.deployments[0].rollout_configuration = format!("sha256:{}", "d".repeat(64));
        public.deployments[0]
            .runtime
            .component_versions
            .insert("agentic".into(), "observed".into());
        assert!(!same_serving_revision(&candidate, &public));
        assert!(!same_serving_revision(&public, &candidate));
    }

    #[test]
    fn candidate_and_public_require_identical_protocol_mode_and_test_options() {
        let candidate = json!({
            "responses": true, "responses_continuation": true, "anthropic": false,
            "tool_parser": "parser", "context_tokens": 1024, "reasoning": false
        });
        let direct = json!({
            "responses": false, "responses_continuation": false, "anthropic": false,
            "tool_parser": "parser", "context_tokens": 1024, "reasoning": false
        });
        assert!(!same_validation_options(true, &candidate, false, &direct));
        assert!(!same_validation_options(false, &direct, true, &candidate));
        assert!(same_validation_options(true, &candidate, true, &candidate));
        assert!(!same_validation_options(
            false, &candidate, true, &candidate
        ));
        let mut changed = candidate.clone();
        changed["context_tokens"] = json!(2048);
        assert!(!same_validation_options(true, &candidate, true, &changed));
    }

    #[test]
    fn summary_requires_real_stage_identity_checks_and_backend() {
        let run = run();
        let backend = "service/model/revision-1";
        let good = summary(&run, ValidationStage::Candidate);
        assert!(summary_matches(
            &good,
            &run,
            ValidationStage::Candidate,
            backend,
            &options()
        ));
        assert!(!summary_matches(
            &good,
            &run,
            ValidationStage::Public,
            backend,
            &options()
        ));
        assert!(!summary_matches(
            &good,
            &run,
            ValidationStage::Candidate,
            "service/model/old",
            &options()
        ));
        let mut wrong = summary(&run, ValidationStage::Candidate);
        wrong.request.runner = "other".into();
        assert!(!summary_matches(
            &wrong,
            &run,
            ValidationStage::Candidate,
            backend,
            &options()
        ));
        let mut wrong = summary(&run, ValidationStage::Candidate);
        wrong.checks[0].status = "failed".into();
        assert!(!summary_matches(
            &wrong,
            &run,
            ValidationStage::Candidate,
            backend,
            &options()
        ));
        let mut wrong = summary(&run, ValidationStage::Candidate);
        wrong.checks.pop();
        assert!(!summary_matches(
            &wrong,
            &run,
            ValidationStage::Candidate,
            backend,
            &options()
        ));
        let mut wrong = summary(&run, ValidationStage::Candidate);
        wrong.checks.push(ValidationCheck {
            name: "chat".into(),
            status: "passed".into(),
            detail: "x".into(),
            semantics: "client".into(),
        });
        assert!(!summary_matches(
            &wrong,
            &run,
            ValidationStage::Candidate,
            backend,
            &options()
        ));
        let mut wrong = summary(&run, ValidationStage::Candidate);
        wrong
            .checks
            .iter_mut()
            .find(|check| check.name == "context_limit")
            .unwrap()
            .detail = "accepted 50 prompt tokens; quality under load not measured".into();
        assert!(!summary_matches(
            &wrong,
            &run,
            ValidationStage::Candidate,
            backend,
            &options()
        ));
    }
}
