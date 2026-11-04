//! Connect live route observation with execution records captured by the worker.
//!
//! Configuration selects resources and service ownership; it is not evidence of
//! their current state. Every call reads Kubernetes again and joins the result to
//! the latest durable functional-run observation. The observation is deliberately
//! independent of `RunRequest::evidence` and the runner's uploaded summary.
use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc};

use sqlx::PgPool;
use thalamus_core::{EvidenceIdentity, RunConditions, ServiceDeployment, TestIdentity};
use thalamus_runs::{Artifacts, RunKind, Store};
use thalamus_runtime::{
    admission, binding,
    records::{Records, VerifiedEvidence},
    DeploymentSnapshot, LiveObserver, RouteSnapshot, RuntimeConfig,
};
use uuid::Uuid;

use crate::{
    capability_evidence::{DeploymentObservation, ObservationSource, Provider, RoutingObservation},
    client_config::CapabilityEvidenceProvider,
};

/// Runtime-backed observation source. Ownership and descriptive service
/// properties are configured by an administrator; routing membership and
/// weights are constructed from the independently observed live route.
pub struct LiveObservationSource {
    db: PgPool,
    records: Records,
    observer: LiveObserver,
    config: RuntimeConfig,
}

impl LiveObservationSource {
    pub fn new(
        db: PgPool,
        observer: LiveObserver,
        config: RuntimeConfig,
    ) -> Result<Self, &'static str> {
        if config.routes.is_empty() {
            return Err("no runtime service bindings configured");
        }
        if observer.config() != &config {
            return Err("runtime observer configuration does not match service bindings");
        }
        for (name, binding) in &config.routes {
            if name.is_empty()
                || name != &binding.service.name
                || binding.team_id.is_nil()
                || binding.project_id.is_nil()
                || binding.public.is_empty()
            {
                return Err("invalid runtime service binding");
            }
        }
        Ok(Self {
            records: Records::new(db.clone()),
            db,
            observer,
            config,
        })
    }

    async fn collect(&self, service: &str) -> Option<RoutingObservation> {
        let binding = self.config.routes.get(service)?;
        let current = self
            .observer
            .observe_route(service, &binding.public)
            .await
            .ok()?;
        if current.service != service || current.route != binding.public {
            return None;
        }
        let actual: BTreeSet<_> = current
            .deployments
            .iter()
            .map(|d| d.deployment.as_str())
            .collect();
        let weighted: BTreeSet<_> = current.weights.keys().map(String::as_str).collect();
        if actual.is_empty() || actual.len() != current.deployments.len() || actual != weighted {
            return None;
        }
        let mut published = binding.service.clone();
        published.deployments = current
            .deployments
            .iter()
            .map(|d| ServiceDeployment {
                deployment: d.deployment.clone(),
                weight: current.weights[&d.deployment],
            })
            .collect();

        let mut agentic_current = None;
        let mut deployments = Vec::with_capacity(current.deployments.len());
        for target in &current.deployments {
            // Select first, then require evidence. Never skip a newer failed,
            // pending, unobserved or incompatible functional run.
            let id: Uuid = sqlx::query_scalar("SELECT id FROM thalamus_runs WHERE team=$1 AND request->>'project_id'=$2 AND request->>'service'=$3 AND request->>'deployment'=$4 AND request->>'kind'='validation' AND request->'workload'->>'name'='functional' ORDER BY created_at DESC,id DESC LIMIT 1")
                .bind(binding.team_id.to_string()).bind(binding.project_id.to_string())
                .bind(service).bind(&target.deployment).fetch_optional(&self.db).await.ok()??;
            let run = Store::new(self.db.clone())
                .get(&binding.team_id.to_string(), id)
                .await
                .ok()?;
            if run.state != "succeeded" || run.request.fingerprint().ok()? != run.fingerprint {
                return None;
            }
            let recorded = self.records.get_verified(id).await.ok()?;
            let suite = validation_suite(
                &run.state,
                recorded.as_ref(),
                &binding.public,
                &target.deployment,
            )?;
            let evidence = recorded?;
            if evidence.run_id != id
                || evidence.runner.pod_owner_job_uid != evidence.runner.job_uid
                || evidence.request_fingerprint != run.fingerprint
                || !evidence.prelaunch.same_identity(&evidence.ingestion)
                || evidence.runner.job != run.job_name
                || run.job_uid.as_deref() != Some(evidence.runner.job_uid.as_str())
                || evidence.runner.pod_uid.is_empty()
                || evidence.runner.image_id.is_empty()
            {
                return None;
            }
            if evidence.prelaunch.agentic && agentic_current.is_none() {
                agentic_current = Some(self.observer.observe_agentic_public(service).await.ok()?);
            }
            let current_target = if evidence.prelaunch.agentic {
                let agentic = agentic_current.as_ref()?;
                if !agentic.agentic
                    || agentic.service != service
                    || agentic.weights != current.weights
                {
                    return None;
                }
                let target_agentic = agentic
                    .deployments
                    .iter()
                    .find(|d| d.deployment == target.deployment)?;
                if !same_logical_model(target, target_agentic) {
                    return None;
                }
                target_agentic
            } else {
                target
            };
            let executed = singleton_execution(&evidence.prelaunch, service, current_target)?;
            let job = serde_json::from_value(serde_json::json!({
                "apiVersion": "batch/v1", "kind": "Job",
                "metadata": {"name": &run.job_name, "namespace": &run.job_namespace},
                "spec": run.job_spec.clone()?
            }))
            .ok()?;
            let target_binding = binding::target_from_job(&run, &job, suite).ok()?;
            binding::check_snapshot(
                &run,
                &target_binding,
                &evidence.prelaunch,
                &evidence.prelaunch.route,
            )
            .ok()?;
            let actual_options = admission::options_from_job(&run, &job).ok()?;
            let test_definition = admission::test_definition(
                &run.request.runner,
                &run.request.workload,
                &actual_options,
            )
            .ok()?;
            if test_definition != run.request.evidence.test_definition {
                return None;
            }

            // Conditions describe the past execution, not fabricated current
            // measurements. Node membership and replica count were observed;
            // cache, traffic and hardware/driver identity were not measured.
            let replicas = u32::try_from(executed.nodes.len())
                .ok()
                .filter(|n| *n > 0)?;
            let conditions = RunConditions {
                hardware: format!("not observed; serving nodes: {}", executed.nodes.join(",")),
                replicas,
                cache_state: "not observed".into(),
                competing_traffic: "not observed".into(),
                time: evidence.prelaunch.observed_at.to_rfc3339(),
            };
            // The definition is reconstructed from the verified Job, not from
            // the request's asserted evidence or from the result payload.
            let test = TestIdentity {
                suite: format!("functional/{suite}"),
                runner: evidence.runner.image_id.clone(),
                client: evidence.runner.image_id.clone(),
                workload: test_definition,
                seed: run.request.workload.seed,
            };
            let current_identity = EvidenceIdentity {
                desired: current_target.desired.clone(),
                runtime: current_target.runtime.clone(),
                test: test.clone(),
                conditions: conditions.clone(),
            };
            let run_identity = EvidenceIdentity {
                desired: executed.desired.clone(),
                runtime: executed.runtime.clone(),
                test,
                conditions,
            };
            deployments.push(DeploymentObservation {
                observed_at: agentic_current
                    .as_ref()
                    .filter(|_| evidence.prelaunch.agentic)
                    .map_or(current.observed_at, |snapshot| snapshot.observed_at),
                deployment: target.deployment.clone(),
                revision: target.revision.clone(),
                backend: target.backend.clone(),
                validation_suite: suite.into(),
                current_identity,
                run_identity,
                run_id: id,
                job_uid: evidence.runner.job_uid,
                runner_pod_uid: evidence.runner.pod_uid,
                runner_image: evidence.runner.image_id,
                run_request: run.request,
            });
        }
        deployments.sort_by(|a, b| a.deployment.cmp(&b.deployment));
        Some(RoutingObservation {
            observed_at: current.observed_at,
            team_id: binding.team_id,
            project_id: binding.project_id,
            service: published,
            deployments,
        })
    }
}

impl ObservationSource for LiveObservationSource {
    fn observe<'a>(
        &'a self,
        service: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<RoutingObservation>> + Send + 'a>> {
        Box::pin(self.collect(service))
    }
}

/// Construct an installable provider from the same configuration used by the
/// worker. Migrate `Records` after the run-store schema before installing it.
pub fn provider(
    db: PgPool,
    artifacts: Artifacts,
    observer: LiveObserver,
    config: RuntimeConfig,
    max_age_seconds: i64,
) -> Result<Arc<dyn CapabilityEvidenceProvider>, &'static str> {
    let source = Arc::new(LiveObservationSource::new(db.clone(), observer, config)?);
    Ok(Arc::new(Provider::new(
        db,
        artifacts,
        source,
        max_age_seconds,
    )?))
}

fn validation_suite(
    state: &str,
    recorded: Option<&VerifiedEvidence>,
    public_route: &str,
    deployment: &str,
) -> Option<&'static str> {
    if state != "succeeded" {
        return None;
    }
    let snapshot = &recorded?.prelaunch;
    ["public", "candidate"].into_iter().find(|suite| {
        admission::matches_route(
            snapshot,
            deployment,
            &RunKind::Validation,
            suite,
            public_route,
            snapshot.agentic,
        )
    })
}

/// The independently joined agentic route must describe the same underlying
/// public model targets as the separately observed direct public route.
fn same_logical_model(direct: &DeploymentSnapshot, agentic: &DeploymentSnapshot) -> bool {
    direct.deployment == agentic.deployment
        && direct.revision == agentic.revision
        && direct.flux_revision == agentic.flux_revision
        && direct.backend == agentic.backend
        && direct.engine_spec == agentic.engine_spec
        && direct.nodes == agentic.nodes
        && direct.runtime.image_digests == agentic.runtime.image_digests
        && direct.runtime.model_content == agentic.runtime.model_content
        && direct.runtime.tokenizer_content == agentic.runtime.tokenizer_content
}

fn singleton_execution<'a>(
    snapshot: &'a RouteSnapshot,
    service: &str,
    current: &DeploymentSnapshot,
) -> Option<&'a DeploymentSnapshot> {
    if snapshot.service != service || snapshot.deployments.len() != 1 {
        return None;
    }
    let executed = snapshot.deployments.first()?;
    // The candidate and public routes are intentionally different. Their sole
    // common backend must nevertheless be the identical observed deployment,
    // revision, runtime, immutable contents, configuration, and node membership.
    (executed == current).then_some(executed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use std::collections::BTreeMap;
    use thalamus_core::{DesiredIdentity, RuntimeIdentity};
    use thalamus_runtime::RouteListener;

    fn fixture() -> RouteSnapshot {
        let sha = format!("sha256:{}", "a".repeat(64));
        let target = DeploymentSnapshot {
            deployment: "dep".into(),
            revision: "rev".into(),
            flux_revision: "main@sha1:abcd".into(),
            backend: "svc/dep/rev".into(),
            desired: DesiredIdentity {
                rendered_resources: sha.clone(),
                resolved_dependencies: sha.clone(),
            },
            rollout_configuration: sha.clone(),
            runtime: RuntimeIdentity {
                image_digests: BTreeMap::from([("engine".into(), sha.clone())]),
                model_content: sha.clone(),
                tokenizer_content: sha.clone(),
                effective_configuration: sha.clone(),
                component_versions: BTreeMap::from([("engine".into(), sha)]),
            },
            underlying_model: None,
            engine_spec: json!({"image":"registry.example/model","args":["--served-model-name","svc"]}),
            nodes: vec!["node-a".into()],
        };
        RouteSnapshot {
            observed_at: Utc::now(),
            service: "svc".into(),
            route: "dep-candidate".into(),
            agentic: false,
            hostnames: vec!["candidate.example".into()],
            paths: vec!["/candidates/dep/v1/chat/completions".into()],
            ports: vec![443],
            listeners: vec![RouteListener {
                hostname: "candidate.example".into(),
                port: 443,
                protocol: "HTTPS".into(),
            }],
            weights: BTreeMap::from([("dep".into(), 1)]),
            deployments: vec![target],
        }
    }

    #[test]
    fn candidate_proof_rejects_changed_runtime_and_old_pool() {
        let snapshot = fixture();
        let current = &snapshot.deployments[0];
        assert!(singleton_execution(&snapshot, "svc", current).is_some());
        let mut changed = current.clone();
        changed.runtime.model_content = format!("sha256:{}", "b".repeat(64));
        assert!(singleton_execution(&snapshot, "svc", &changed).is_none());
        let mut ambiguous = snapshot.clone();
        let mut old = current.clone();
        old.deployment = "old".into();
        ambiguous.deployments.push(old);
        assert!(singleton_execution(&ambiguous, "svc", current).is_none());
    }

    #[test]
    fn latest_verified_public_run_replaces_candidate_after_promotion() {
        let candidate = fixture();
        let sha = format!("sha256:{}", "a".repeat(64));
        let evidence = |snapshot: RouteSnapshot| VerifiedEvidence {
            run_id: Uuid::from_u128(3),
            request_fingerprint: "a".repeat(64),
            prelaunch: snapshot.clone(),
            ingestion: snapshot,
            runner: thalamus_runtime::RunnerObservation {
                job: "job".into(),
                job_uid: "job-uid".into(),
                pod: "pod".into(),
                pod_uid: "pod-uid".into(),
                pod_owner_job_uid: "job-uid".into(),
                image: format!("registry.example/runner@{sha}"),
                image_id: sha.clone(),
            },
        };
        let candidate_run = evidence(candidate.clone());
        assert_eq!(
            validation_suite("succeeded", Some(&candidate_run), "svc", "dep"),
            Some("candidate")
        );
        assert!(
            singleton_execution(&candidate_run.prelaunch, "svc", &candidate.deployments[0])
                .is_some()
        );

        let mut promoted = candidate.clone();
        promoted.route = "svc".into();
        promoted.deployments[0].flux_revision = "main@sha1:new-commit".into();
        assert!(
            singleton_execution(&candidate_run.prelaunch, "svc", &promoted.deployments[0])
                .is_none()
        );
        let public_run = evidence(promoted.clone());
        assert_eq!(
            validation_suite("succeeded", Some(&public_run), "svc", "dep"),
            Some("public")
        );
        assert!(
            singleton_execution(&public_run.prelaunch, "svc", &promoted.deployments[0]).is_some()
        );
        // A newer failed or unavailable run must not fall back to candidate_run.
        assert_eq!(
            validation_suite("failed", Some(&public_run), "svc", "dep"),
            None
        );
        assert_eq!(validation_suite("succeeded", None, "svc", "dep"), None);
        assert_eq!(validation_suite("pending", None, "svc", "dep"), None);
    }
    #[test]
    fn agentic_runs_match_actual_outer_stage_and_same_logical_model() {
        let mut snapshot = fixture();
        snapshot.agentic = true;
        snapshot.route = format!("agentic-cand-{}", "a".repeat(32));
        let evidence = VerifiedEvidence {
            run_id: Uuid::from_u128(4),
            request_fingerprint: "a".repeat(64),
            prelaunch: snapshot.clone(),
            ingestion: snapshot.clone(),
            runner: thalamus_runtime::RunnerObservation {
                job: "job".into(),
                job_uid: "job-uid".into(),
                pod: "pod".into(),
                pod_uid: "pod-uid".into(),
                pod_owner_job_uid: "job-uid".into(),
                image: "registry.example/runner@sha256:abc".into(),
                image_id: "sha256:abc".into(),
            },
        };
        assert_eq!(
            validation_suite("succeeded", Some(&evidence), "svc", "dep"),
            Some("candidate")
        );
        let mut public = evidence.clone();
        public.prelaunch.route = "svc-assistant".into();
        public.ingestion.route = "svc-assistant".into();
        assert_eq!(
            validation_suite("succeeded", Some(&public), "svc", "dep"),
            Some("public")
        );
        assert!(same_logical_model(
            &snapshot.deployments[0],
            &public.prelaunch.deployments[0]
        ));
        public.prelaunch.deployments[0].runtime.model_content =
            format!("sha256:{}", "b".repeat(64));
        assert!(!same_logical_model(
            &snapshot.deployments[0],
            &public.prelaunch.deployments[0]
        ));
        public.prelaunch.agentic = false;
        assert_eq!(
            validation_suite("succeeded", Some(&public), "svc", "dep"),
            None
        );
    }
}
