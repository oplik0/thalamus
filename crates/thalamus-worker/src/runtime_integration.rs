//! Bind independently observed serving and runner identities to a prepared run Job.
//!
//! The caller serializes prelaunch and CREATE with the per-run advisory lock.
//! A Kubernetes Lease alone is not a fence. The Job's persisted, operator-built
//! spec is the source of runner configuration; request evidence is never used as
//! an observation.

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use serde::Deserialize;
use sqlx::PgPool;
use thalamus_access::PolicySource;
use thalamus_changes::{
    AuthorizedCandidateRun, AuthorizedValidation, Store as ChangeStore, ValidationStage,
};
use thalamus_runs::{Run, RunKind};
use thalamus_runtime::{
    binding::{check_snapshot, target_from_job, RouteChoice, Target},
    records::{Error as RecordsError, Records},
    LiveObserver, RouteSnapshot, RunnerObservation, RuntimeConfig,
};

use crate::Result;

type BindingResult<T> = std::result::Result<T, &'static str>;

pub(crate) struct RuntimeIntegration {
    config: RuntimeConfig,
    observer: LiveObserver,
    records: Records,
    changes: ChangeStore,
    policy_source: PolicySource,
}

/// A failed Job may be finalized without a diagnostic artifact when durable
/// evidence says the binding is irrecoverably invalid. Infrastructure and
/// observation read failures must instead remain retryable.
pub(crate) fn is_permanent_failure(error: &(dyn std::error::Error + 'static)) -> bool {
    matches!(
        error.downcast_ref::<RecordsError>(),
        Some(
            RecordsError::Rejected(_)
                | RecordsError::Replay
                | RecordsError::Conflict(_)
                | RecordsError::Missing(_)
                | RecordsError::Invalid(_)
                | RecordsError::NotFound
        )
    ) || matches!(
        error.downcast_ref::<thalamus_runs::Error>(),
        Some(thalamus_runs::Error::Invalid(_) | thalamus_runs::Error::ForeignJob)
    )
}

#[derive(Clone, Copy)]
enum Terminal {
    Success,
    Failure,
}

enum RolloutAuthorization {
    Validation(AuthorizedValidation),
    Candidate(AuthorizedCandidateRun),
}

struct RouteAccess {
    route: String,
    authorization: Option<RolloutAuthorization>,
}

impl RuntimeIntegration {
    pub(crate) fn new(
        json: &str,
        client: kube::Client,
        pool: PgPool,
        policy_source: PolicySource,
    ) -> Result<Self> {
        let config: RuntimeConfig =
            serde_json::from_str(json).map_err(|_| "invalid THALAMUS_RUNTIME_CONFIG")?;
        let observer = LiveObserver::new(client, config.clone())?;
        Ok(Self {
            config,
            observer,
            records: Records::new(pool.clone()),
            changes: ChangeStore::new(pool),
            policy_source,
        })
    }

    pub(crate) fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    fn check_route_mode(
        &self,
        run: &Run,
        route_target: &str,
        agentic: bool,
        snapshot: &RouteSnapshot,
    ) -> BindingResult<()> {
        let routes = self
            .config
            .routes
            .get(&run.request.service)
            .ok_or("service has no runtime route configuration")?;
        if !thalamus_runtime::admission::matches_route(
            snapshot,
            &run.request.deployment,
            &run.request.kind,
            route_target,
            &routes.public,
            agentic,
        ) {
            return Err("observed route does not match the authorized runner mode");
        }
        Ok(())
    }

    /// Resolve the operator-approved service/runner options against a live,
    /// authorized route before preparing a new Job. The caller holds the
    /// per-run create lock and persists a Job built from the returned config.
    /// A previously persisted spec is never reconstructed or overwritten.
    pub(crate) async fn resolve_config(
        &self,
        run: &Run,
        route_target: &str,
        options: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        if run.job_spec.is_some() {
            return Ok(None);
        }
        if run.state != "pending" || run.job_uid.is_some() {
            return Err(
                RecordsError::Invalid("cannot prepare a new Job for this run".into()).into(),
            );
        }
        let choice = route_choice(&run.request.kind, route_target)
            .map_err(|reason| RecordsError::Invalid(reason.into()))?;
        let access = self.route_access(run, choice, true, true).await?;
        let agentic = agentic_mode(&run.request.kind, options)?;
        let snapshot = self.observe(run, choice, &access.route, agentic).await?;
        check_rollout_snapshot(access.authorization.as_ref(), &snapshot)
            .map_err(|reason| RecordsError::Conflict(reason.into()))?;
        check_admission(run, &snapshot, options)?;
        let public = &self.config.routes[&run.request.service].public;
        let config = thalamus_runtime::admission::resolve_runner_config(
            &snapshot,
            &run.request.deployment,
            &run.request.kind,
            route_target,
            public,
            options,
        )?;
        Ok(Some(config))
    }

    /// Call under the run's Job-create advisory lock, after persisting the Job
    /// spec and checking whether the Job exists, but before the first CREATE.
    /// An existing or previously recorded Job can never acquire new prelaunch
    /// evidence: it must be bound to the original durable observation.
    pub(crate) async fn prelaunch(
        &self,
        run: &Run,
        job: &Job,
        existing: bool,
        route_target: &str,
    ) -> Result<()> {
        if run.state != "pending" {
            return Err("prelaunch requires a pending run".into());
        }
        self.check_job(run, job, existing)?;
        let target = target_from_job(run, job, route_target)
            .map_err(|reason| RecordsError::Invalid(reason.into()))?;
        let options = thalamus_runtime::admission::options_from_job(run, job)
            .map_err(|_| RecordsError::Invalid("invalid prepared runner test options".into()))?;
        let agentic = agentic_mode(&run.request.kind, &options)?;
        let access = self
            .route_access(run, target.choice, true, !existing && run.job_uid.is_none())
            .await?;
        let saved = self.records.get_prelaunch(run.id).await?;

        if existing || run.job_uid.is_some() {
            let saved = saved.ok_or_else(|| {
                RecordsError::Missing("existing Job has no prelaunch observation".into())
            })?;
            if !existing {
                return Err("recorded Job is missing".into());
            }
            let route = observation_route(agentic, &access.route, &saved.snapshot.route);
            self.check_route_mode(run, route_target, agentic, &saved.snapshot)
                .and_then(|()| check_saved(run, &target, route, &saved))
                .and_then(|()| {
                    check_rollout_snapshot(access.authorization.as_ref(), &saved.snapshot)
                })
                .map_err(|reason| RecordsError::Conflict(reason.into()))?;
            check_admission(run, &saved.snapshot, &options)?;
            return Ok(());
        }

        // Even if an earlier attempt persisted evidence and crashed before
        // CREATE, observe again. Records::prelaunch compares immutable identity
        // with the original and never replaces it.
        let snapshot = self
            .observe(run, target.choice, &access.route, agentic)
            .await?;
        let route = observation_route(agentic, &access.route, &snapshot.route);
        if let Err(reason) = self
            .check_route_mode(run, route_target, agentic, &snapshot)
            .and_then(|()| check_snapshot(run, &target, &snapshot, route))
            .and_then(|()| check_rollout_snapshot(access.authorization.as_ref(), &snapshot))
        {
            // Never persist a bad first observation. If an earlier attempt
            // already persisted one, still compare the independently observed
            // identity so a crash-before-CREATE retry cannot replace it.
            if saved.is_some() {
                self.records
                    .prelaunch(run.id, &run.fingerprint, &snapshot)
                    .await?;
            }
            return Err(RecordsError::Conflict(reason.into()).into());
        }
        if let Err(error) = check_admission(run, &snapshot, &options) {
            if saved.is_some() {
                self.records
                    .prelaunch(run.id, &run.fingerprint, &snapshot)
                    .await?;
            }
            return Err(error);
        }
        let recorded = self
            .records
            .prelaunch(run.id, &run.fingerprint, &snapshot)
            .await?;
        self.check_route_mode(run, route_target, agentic, &recorded.snapshot)
            .and_then(|()| {
                check_saved(
                    run,
                    &target,
                    observation_route(agentic, &access.route, &recorded.snapshot.route),
                    &recorded,
                )
            })
            .map_err(|reason| RecordsError::Conflict(reason.into()))?;
        Ok(())
    }

    /// Call after JobSucceeded transitions the run to ingesting and before
    /// reading or accepting a successful artifact.
    pub(crate) async fn ingestion(&self, run: &Run, job: &Job, route_target: &str) -> Result<()> {
        if run.state != "ingesting" || thalamus_worker::job_outcome(job) != Some(true) {
            return Err("runtime ingestion requires a completed successful Job".into());
        }
        self.terminal(run, job, route_target, Terminal::Success)
            .await
    }

    /// Failed-Job artifacts are diagnostic only, but must still be bound to an
    /// independently observed runner and route before they are accepted.
    pub(crate) async fn failure(&self, run: &Run, job: &Job, route_target: &str) -> Result<()> {
        if run.state != "running" || thalamus_worker::job_outcome(job) != Some(false) {
            return Err("runtime failure requires a completed failed Job".into());
        }
        self.terminal(run, job, route_target, Terminal::Failure)
            .await
    }

    /// Recover already persisted successful-run evidence even when the Job or
    /// Pod has since expired. None means observation still needs to happen;
    /// a rejected or conflicting record is an error, never a cache miss.
    pub(crate) async fn verified_ingestion(&self, run: &Run, route_target: &str) -> Result<bool> {
        if run.state != "ingesting" {
            return Err("recorded ingestion requires an ingesting run".into());
        }
        let Some(evidence) = self.records.get_ingesting(run.id).await? else {
            return Ok(false);
        };
        if evidence.run_id != run.id || evidence.request_fingerprint != run.fingerprint {
            return Err(
                RecordsError::Conflict("recorded ingestion run identity mismatch".into()).into(),
            );
        }
        self.check_recovered(
            run,
            route_target,
            &evidence.prelaunch,
            &evidence.ingestion,
            &evidence.runner,
        )
        .await?;
        Ok(true)
    }

    /// Recover a separately persisted failed-Job diagnostic observation. It
    /// never becomes successful validation or benchmark evidence.
    pub(crate) async fn verified_failure(&self, run: &Run, route_target: &str) -> Result<bool> {
        if run.state != "running" {
            return Err("recorded failure requires a running run".into());
        }
        let Some(evidence) = self.records.get_failed_job(run.id).await? else {
            return Ok(false);
        };
        if evidence.run_id != run.id || evidence.request_fingerprint != run.fingerprint {
            return Err(
                RecordsError::Conflict("recorded failure run identity mismatch".into()).into(),
            );
        }
        self.check_recovered(
            run,
            route_target,
            &evidence.prelaunch,
            &evidence.ingestion,
            &evidence.runner,
        )
        .await?;
        Ok(true)
    }

    async fn check_recovered(
        &self,
        run: &Run,
        route_target: &str,
        prelaunch: &RouteSnapshot,
        ingestion: &RouteSnapshot,
        runner: &RunnerObservation,
    ) -> Result<()> {
        if run
            .request
            .fingerprint()
            .map_err(|_| RecordsError::Invalid("invalid run request".into()))?
            != run.fingerprint
            || run.job_name != thalamus_runs::job_name(run.id)
            || run.job_namespace.as_deref() != Some(self.config.namespace.as_str())
        {
            return Err(RecordsError::Conflict("recorded run identity mismatch".into()).into());
        }
        let uid = run
            .job_uid
            .as_deref()
            .ok_or_else(|| RecordsError::Missing("recorded Job UID".into()))?;
        let spec = run
            .job_spec
            .as_ref()
            .ok_or_else(|| RecordsError::Missing("prepared Job spec".into()))?;
        let job = Job {
            spec: Some(
                JobSpec::deserialize(spec)
                    .map_err(|_| RecordsError::Invalid("invalid prepared Job spec".into()))?,
            ),
            ..Default::default()
        };
        let target = target_from_job(run, &job, route_target)
            .map_err(|reason| RecordsError::Conflict(reason.into()))?;
        let options = thalamus_runtime::admission::options_from_job(run, &job)
            .map_err(|_| RecordsError::Invalid("invalid prepared runner test options".into()))?;
        let agentic = agentic_mode(&run.request.kind, &options)?;
        let access = self.route_access(run, target.choice, false, false).await?;
        let route = observation_route(agentic, &access.route, &prelaunch.route);
        self.check_route_mode(run, route_target, agentic, prelaunch)
            .and_then(|()| check_snapshot(run, &target, prelaunch, route))
            .and_then(|()| check_rollout_snapshot(access.authorization.as_ref(), prelaunch))
            .map_err(|reason| RecordsError::Conflict(reason.into()))?;
        self.check_route_mode(run, route_target, agentic, ingestion)
            .and_then(|()| check_snapshot(run, &target, ingestion, route))
            .and_then(|()| check_rollout_snapshot(access.authorization.as_ref(), ingestion))
            .map_err(|reason| RecordsError::Conflict(reason.into()))?;
        check_admission(run, prelaunch, &options)?;
        check_admission(run, ingestion, &options)?;
        thalamus_runtime::records::verify_observations(
            prelaunch,
            ingestion,
            runner,
            &run.job_name,
            uid,
            spec,
        )?;
        Ok(())
    }

    async fn terminal(
        &self,
        run: &Run,
        job: &Job,
        route_target: &str,
        terminal: Terminal,
    ) -> Result<()> {
        let saved = self
            .records
            .get_prelaunch(run.id)
            .await?
            .ok_or_else(|| RecordsError::Missing("prelaunch observation".into()))?;

        if self.check_job(run, job, true).is_err() {
            return self
                .reject(
                    run,
                    terminal,
                    "recorded Job does not match the prepared Job",
                    None,
                    None,
                )
                .await;
        }
        let target = match target_from_job(run, job, route_target) {
            Ok(target) => target,
            Err(reason) => return self.reject(run, terminal, reason, None, None).await,
        };
        let options = match thalamus_runtime::admission::options_from_job(run, job) {
            Ok(options) => options,
            Err(_) => {
                return self
                    .reject(
                        run,
                        terminal,
                        "invalid prepared runner test options",
                        None,
                        None,
                    )
                    .await
            }
        };
        let agentic = match agentic_mode(&run.request.kind, &options) {
            Ok(agentic) => agentic,
            Err(_) => {
                return self
                    .reject(run, terminal, "invalid runner protocol options", None, None)
                    .await
            }
        };
        let access = match self.route_access(run, target.choice, false, false).await {
            Ok(access) => access,
            Err(error) if is_permanent_failure(error.as_ref()) => {
                return self
                    .reject(
                        run,
                        terminal,
                        "run route authorization does not match",
                        None,
                        None,
                    )
                    .await;
            }
            Err(error) => return Err(error),
        };
        let route = observation_route(agentic, &access.route, &saved.snapshot.route);
        if let Err(reason) = self
            .check_route_mode(run, route_target, agentic, &saved.snapshot)
            .and_then(|()| check_saved(run, &target, route, &saved))
            .and_then(|()| check_rollout_snapshot(access.authorization.as_ref(), &saved.snapshot))
        {
            return self.reject(run, terminal, reason, None, None).await;
        }
        if check_admission(run, &saved.snapshot, &options).is_err() {
            return self
                .reject(
                    run,
                    terminal,
                    "prelaunch identity differs from admission",
                    None,
                    None,
                )
                .await;
        }

        // Observe the exact route captured at launch, not a newly selected
        // route. A positively observed missing/ambiguous or changing runtime
        // is a permanent mismatch; API transport failures remain retryable.
        let snapshot = match self
            .observe(run, target.choice, &access.route, agentic)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(
                thalamus_runtime::Error::Invalid(_)
                | thalamus_runtime::Error::Changed
                | thalamus_runtime::Error::Json(_),
            ) => {
                return self
                    .reject(
                        run,
                        terminal,
                        "live route evidence is incomplete, ambiguous, or changed",
                        None,
                        None,
                    )
                    .await;
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(reason) = self
            .check_route_mode(run, route_target, agentic, &snapshot)
            .and_then(|()| check_snapshot(run, &target, &snapshot, route))
            .and_then(|()| check_rollout_snapshot(access.authorization.as_ref(), &snapshot))
        {
            return self
                .reject(run, terminal, reason, Some(&snapshot), None)
                .await;
        }
        if check_admission(run, &snapshot, &options).is_err() {
            return self
                .reject(
                    run,
                    terminal,
                    "runtime or test identity changed since admission",
                    Some(&snapshot),
                    None,
                )
                .await;
        }
        if !saved.snapshot.same_identity(&snapshot) {
            return self
                .reject(
                    run,
                    terminal,
                    "route or runtime identity changed during the run",
                    Some(&snapshot),
                    None,
                )
                .await;
        }
        let runner = match self.observer.observe_runner(&run.job_name).await {
            Ok(runner) => runner,
            Err(
                thalamus_runtime::Error::Invalid(_)
                | thalamus_runtime::Error::Changed
                | thalamus_runtime::Error::Json(_),
            ) => {
                return self
                    .reject(
                        run,
                        terminal,
                        "live runner evidence is incomplete, ambiguous, or changed",
                        Some(&snapshot),
                        None,
                    )
                    .await;
            }
            Err(error) => return Err(error.into()),
        };
        // Each record independently checks fingerprint, snapshots, Job UID,
        // Pod ownership and pinned actual runner image. Failure evidence is
        // distinct and never counts as successful verification.
        match terminal {
            Terminal::Success => {
                self.records
                    .ingestion(run.id, &run.fingerprint, &snapshot, &runner)
                    .await?;
            }
            Terminal::Failure => {
                self.records
                    .failure(run.id, &run.fingerprint, &snapshot, &runner)
                    .await?;
            }
        }
        Ok(())
    }

    async fn reject(
        &self,
        run: &Run,
        terminal: Terminal,
        reason: &'static str,
        snapshot: Option<&RouteSnapshot>,
        runner: Option<&RunnerObservation>,
    ) -> Result<()> {
        match terminal {
            Terminal::Success => {
                self.records
                    .reject_ingestion(run.id, &run.fingerprint, reason, snapshot, runner)
                    .await?
            }
            Terminal::Failure => {
                self.records
                    .reject_failure(run.id, &run.fingerprint, reason, snapshot, runner)
                    .await?
            }
        }
        Err(RecordsError::Rejected(reason.into()).into())
    }

    fn check_job(&self, run: &Run, job: &Job, existing: bool) -> Result<()> {
        if run.fingerprint != run.request.fingerprint()?
            || run.job_name != thalamus_runs::job_name(run.id)
            || run.job_namespace.as_deref() != Some(self.config.namespace.as_str())
            || job.metadata.name.as_deref() != Some(run.job_name.as_str())
            || job.metadata.namespace.as_deref() != Some(self.config.namespace.as_str())
        {
            return Err("run or Job identity mismatch".into());
        }
        if existing {
            thalamus_runs::verify_recoverable_job(run, job)?;
        } else {
            // Before CREATE there is no UID or Kubernetes defaulting. Compare
            // against the exact persisted prepared spec, not new templates.
            let spec = job.spec.as_ref().ok_or("prepared Job has no spec")?;
            if run.job_spec.as_ref() != Some(&serde_json::to_value(spec)?) {
                return Err("prepared Job differs from persisted spec".into());
            }
        }
        Ok(())
    }

    async fn route_access(
        &self,
        run: &Run,
        choice: RouteChoice,
        require_catalog: bool,
        require_active: bool,
    ) -> Result<RouteAccess> {
        let routes = self
            .config
            .routes
            .get(&run.request.service)
            .ok_or_else(|| {
                RecordsError::Invalid("service has no runtime route configuration".into())
            })?;
        if routes.team_id.to_string() != run.team
            || routes.project_id != run.request.project_id
            || routes.service.name != run.request.service
        {
            return Err(RecordsError::Invalid(
                "run does not belong to configured service and project".into(),
            )
            .into());
        }
        let route = derived_route(
            &run.request.kind,
            choice,
            &routes.public,
            &run.request.deployment,
        )
        .map_err(|reason| RecordsError::Invalid(reason.into()))?;
        if choice == RouteChoice::Candidate && route == routes.public {
            return Err(RecordsError::Invalid(
                "candidate route conflicts with the public route".into(),
            )
            .into());
        }
        let validation = if run.request.kind == RunKind::Validation {
            self.changes
                .authorize_validation_run(run.id)
                .await
                .map_err(rollout_error)?
        } else {
            None
        };
        let candidate = self
            .changes
            .authorize_candidate_run(run.id)
            .await
            .map_err(rollout_error)?;
        let authorization = match (validation, candidate) {
            (Some(_), Some(_)) => {
                return Err(
                    RecordsError::Invalid("run has conflicting rollout bindings".into()).into(),
                );
            }
            (Some(auth), None) => {
                let stage_matches = matches!(
                    (auth.stage, choice),
                    (ValidationStage::Candidate, RouteChoice::Candidate)
                        | (ValidationStage::Public, RouteChoice::Public)
                );
                if auth.run_id != Some(run.id)
                    || auth.owner_team.to_string() != run.team
                    || auth.project_id != run.request.project_id
                    || auth.service != run.request.service
                    || auth.deployment != run.request.deployment
                    || !stage_matches
                    || (require_active && !auth.active)
                    || (choice == RouteChoice::Candidate
                        && auth.route.as_deref() != Some(route.as_str()))
                    || (choice == RouteChoice::Public && auth.route.is_some())
                {
                    return Err(RecordsError::Invalid(
                        "rollout does not authorize this run and route".into(),
                    )
                    .into());
                }
                Some(RolloutAuthorization::Validation(auth))
            }
            (None, Some(auth)) => {
                if auth.run_id != run.id
                    || auth.owner_team.to_string() != run.team
                    || auth.project_id != run.request.project_id
                    || auth.service != run.request.service
                    || auth.deployment != run.request.deployment
                    || choice != RouteChoice::Candidate
                    || auth.route != route
                    || (require_active && !auth.active)
                {
                    return Err(RecordsError::Invalid(
                        "candidate rollout does not authorize this run and route".into(),
                    )
                    .into());
                }
                Some(RolloutAuthorization::Candidate(auth))
            }
            (None, None) => None,
        };
        if authorization.is_none() && require_catalog {
            // Manual runs are authorized against pinned committed blobs, not
            // caller-provided route names or the mutable working tree.
            let policy = self.policy_source.load()?;
            let service = policy.service_id(&run.request.service)?.ok_or_else(|| {
                RecordsError::Invalid("service is absent from committed catalog".into())
            })?;
            if service.namespace != self.config.namespace
                || !policy.includes_deployment(&service, &run.request.deployment)?
            {
                return Err(RecordsError::Invalid(
                    "deployment is absent from committed service catalog".into(),
                )
                .into());
            }
        }
        Ok(RouteAccess {
            route,
            authorization,
        })
    }

    async fn observe(
        &self,
        run: &Run,
        choice: RouteChoice,
        route: &str,
        agentic: bool,
    ) -> thalamus_runtime::Result<RouteSnapshot> {
        match (&run.request.kind, choice, agentic) {
            (RunKind::Validation, RouteChoice::Candidate, true) => {
                self.observer
                    .observe_agentic_candidate(&run.request.service, &run.request.deployment)
                    .await
            }
            (RunKind::Validation, RouteChoice::Public, true) => {
                self.observer
                    .observe_agentic_public(&run.request.service)
                    .await
            }
            (RunKind::Validation, RouteChoice::Candidate, false) => {
                self.observer
                    .observe_candidate(&run.request.service, &run.request.deployment)
                    .await
            }
            (RunKind::Benchmark, RouteChoice::Candidate, false) => {
                self.observer
                    .observe_benchmark(&run.request.service, &run.request.deployment)
                    .await
            }
            (RunKind::Validation, RouteChoice::Public, false) => {
                self.observer
                    .observe_route(&run.request.service, route)
                    .await
            }
            _ => Err(thalamus_runtime::Error::Invalid(
                "unsupported run route target".into(),
            )),
        }
    }
}

/// The operator-approved Job's protocol options determine which independently
/// observed route carries it. Rollout authorization remains bound to the
/// canonical model deployment route, not to a caller-selected outer route.
fn agentic_mode(kind: &RunKind, options: &serde_json::Value) -> Result<bool> {
    thalamus_runtime::admission::requires_agentic(kind, options)
        .map_err(|_| RecordsError::Invalid("invalid runner protocol options".into()).into())
}

fn observation_route<'a>(agentic: bool, direct: &'a str, observed: &'a str) -> &'a str {
    if agentic {
        observed
    } else {
        direct
    }
}

fn rollout_error(error: thalamus_changes::Error) -> Box<dyn std::error::Error + Send + Sync> {
    match error {
        thalamus_changes::Error::Invalid(_)
        | thalamus_changes::Error::Conflict(_)
        | thalamus_changes::Error::IdempotencyConflict
        | thalamus_changes::Error::NotFound => {
            RecordsError::Invalid("invalid rollout run authorization".into()).into()
        }
        other => other.into(),
    }
}

fn route_choice(kind: &RunKind, route_target: &str) -> BindingResult<RouteChoice> {
    match (kind, route_target) {
        (RunKind::Validation, "candidate") | (RunKind::Benchmark, "candidate") => {
            Ok(RouteChoice::Candidate)
        }
        (RunKind::Validation, "public") => Ok(RouteChoice::Public),
        _ => Err("invalid trusted runner route target"),
    }
}

fn derived_route(
    kind: &RunKind,
    choice: RouteChoice,
    public: &str,
    deployment: &str,
) -> BindingResult<String> {
    if deployment.is_empty() {
        return Err("deployment identity is empty");
    }
    match (kind, choice) {
        (RunKind::Validation, RouteChoice::Candidate) => Ok(format!("{deployment}-candidate")),
        (RunKind::Benchmark, RouteChoice::Candidate) => Ok(format!("{deployment}-benchmark")),
        (RunKind::Validation, RouteChoice::Public) if !public.is_empty() => Ok(public.to_owned()),
        _ => Err("invalid trusted runner route target"),
    }
}

fn check_rollout_snapshot(
    auth: Option<&RolloutAuthorization>,
    snapshot: &RouteSnapshot,
) -> BindingResult<()> {
    let Some(auth) = auth else { return Ok(()) };
    if snapshot.deployments.len() != 1 {
        return Err("rollout route does not have one deployment");
    }
    let target = &snapshot.deployments[0];
    let (service, deployment, revision, commit) = match auth {
        RolloutAuthorization::Validation(auth) => (
            &auth.service,
            &auth.deployment,
            &auth.revision,
            &auth.catalog_commit,
        ),
        RolloutAuthorization::Candidate(auth) => (
            &auth.service,
            &auth.deployment,
            &auth.revision,
            &auth.catalog_commit,
        ),
    };
    if &snapshot.service != service
        || &target.deployment != deployment
        || &target.revision != revision
        || flux_commit(&target.flux_revision) != Some(commit.as_str())
    {
        return Err("observed rollout revision or catalog commit changed");
    }
    Ok(())
}

fn flux_commit(revision: &str) -> Option<&str> {
    let (reference, hash) = revision.rsplit_once('@')?;
    if reference.is_empty()
        || reference.trim() != reference
        || reference.chars().any(char::is_control)
    {
        return None;
    }
    let (algorithm, commit) = hash.split_once(':')?;
    if !matches!((algorithm, commit.len()), ("sha1", 40) | ("sha256", 64))
        || !commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some(commit)
}

// This compares the admitted plan with a *new independent* observation to
// detect drift; the request's claims are never stored as observed proof.
fn check_admission(run: &Run, snapshot: &RouteSnapshot, options: &serde_json::Value) -> Result<()> {
    thalamus_runtime::admission::validate_options_workload(
        &run.request.kind,
        options,
        &run.request.workload,
    )
    .map_err(|_| {
        RecordsError::Invalid("run workload does not match approved test options".into())
    })?;
    let observed = thalamus_runtime::admission::request_evidence(
        snapshot,
        &run.request.deployment,
        &run.request.runner,
        &run.request.workload,
        options,
    )
    .map_err(|_| RecordsError::Invalid("admission identity is incomplete".into()))?;
    if !thalamus_runtime::admission::same_request_evidence(&run.request.evidence, &observed) {
        return Err(RecordsError::Conflict(
            "runtime or test identity changed since admission".into(),
        )
        .into());
    }
    Ok(())
}

fn check_saved(
    run: &Run,
    target: &Target,
    route: &str,
    saved: &thalamus_runtime::records::PrelaunchEvidence,
) -> BindingResult<()> {
    if saved.run_id != run.id || saved.request_fingerprint != run.fingerprint {
        return Err("prelaunch run identity mismatch");
    }
    if saved.snapshot.route != route {
        return Err("prelaunch route is no longer configured for the run");
    }
    check_snapshot(run, target, &saved.snapshot, &saved.snapshot.route)
}

/// Validate a workflow gate's independently observed route against the
/// persisted validation Job configuration. The caller supplies the expected
/// route and backend from its trusted stage and independently checks the Job
/// against the persisted spec.
pub(crate) fn verify_validation_job(
    run: &Run,
    job: &Job,
    snapshot: &RouteSnapshot,
    route_target: &str,
    expected_route: &str,
    expected_backend: &str,
) -> Result<()> {
    if run.request.kind != RunKind::Validation || expected_backend.is_empty() {
        return Err("invalid validation gate identity".into());
    }
    let target = target_from_job(run, job, route_target)?;
    if target.expected_backend.as_deref() != Some(expected_backend) {
        return Err("validation gate backend assertion is missing or mismatched".into());
    }
    check_snapshot(run, &target, snapshot, expected_route)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failure_classification_keeps_infrastructure_retryable() {
        assert!(is_permanent_failure(&RecordsError::Rejected(
            "changed route".into()
        )));
        assert!(is_permanent_failure(&RecordsError::Replay));
        assert!(is_permanent_failure(&RecordsError::Missing(
            "prelaunch".into()
        )));
        assert!(is_permanent_failure(&thalamus_runs::Error::Invalid(
            "bad config".into()
        )));
        assert!(is_permanent_failure(&thalamus_runs::Error::ForeignJob));
        let transient = std::io::Error::other("unavailable");
        assert!(!is_permanent_failure(&transient));
    }

    #[test]
    fn internal_routes_follow_authorized_target_kind() {
        assert_eq!(
            derived_route(
                &RunKind::Validation,
                RouteChoice::Candidate,
                "public",
                "alpha"
            )
            .unwrap(),
            "alpha-candidate"
        );
        assert_eq!(
            derived_route(
                &RunKind::Benchmark,
                RouteChoice::Candidate,
                "public",
                "alpha"
            )
            .unwrap(),
            "alpha-benchmark"
        );
        assert_eq!(
            derived_route(&RunKind::Validation, RouteChoice::Public, "public", "alpha").unwrap(),
            "public"
        );
        assert!(
            derived_route(&RunKind::Benchmark, RouteChoice::Public, "public", "alpha").is_err()
        );
        assert!(derived_route(&RunKind::Validation, RouteChoice::Candidate, "public", "").is_err());
    }

    #[test]
    fn agentic_route_binding_uses_observed_outer_route() {
        assert_eq!(
            observation_route(false, "model-candidate", "agent-route"),
            "model-candidate"
        );
        assert_eq!(
            observation_route(true, "model-candidate", "agent-route"),
            "agent-route"
        );
    }

    #[test]
    fn flux_revision_requires_exact_valid_commit() {
        let commit = "a".repeat(40);
        let revision = format!("main@sha1:{commit}");
        assert_eq!(flux_commit(&revision), Some(commit.as_str()));
        assert!(flux_commit("@sha1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_none());
        assert!(flux_commit("main@sha1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").is_none());
    }
}
