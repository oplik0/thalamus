use chrono::{TimeZone, Utc};
use std::collections::BTreeMap;
use thalamus_runs::{
    build_job, job_name, verify_owned_job, verify_recoverable_job, EvidenceIdentity, JobTemplate,
    Run, RunConditions, RunKind, RunRequest, Templates, Workload,
};
use uuid::Uuid;

fn request() -> RunRequest {
    RunRequest {
        kind: RunKind::Benchmark,
        service: "coding".into(),
        deployment: "gpu-a".into(),
        runner: "perf".into(),
        workload: Workload {
            name: "shared-prefix".into(),
            seed: 1,
            concurrency: 8,
            duration_seconds: 60,
            warmup_seconds: 5,
            max_requests: 100,
            model_budget_tokens: 0,
            tool_budget_calls: 0,
        },
        evidence: EvidenceIdentity {
            desired_configuration: "sha256:desired".into(),
            observed_runtime: "sha256:runtime".into(),
            test_definition: "sha256:test".into(),
            conditions: RunConditions {
                hardware: "gpu-a".into(),
                nodes: vec!["node-a".into()],
                replicas: 1,
                cache_state: "warm".into(),
                competing_traffic: "low".into(),
                observed_at: Utc.timestamp_opt(100, 0).unwrap(),
            },
        },
    }
}
fn run() -> Run {
    let request = request();
    let id = Uuid::from_u128(1);
    let now = Utc.timestamp_opt(100, 0).unwrap();
    Run {
        id,
        team: "team".into(),
        actor: "user".into(),
        idempotency_key: "retry".into(),
        fingerprint: request.fingerprint().unwrap(),
        request,
        job_name: job_name(id),
        job_uid: None,
        ownership_nonce: Uuid::from_u128(2),
        job_spec: None,
        job_namespace: None,
        state: "pending".into(),
        artifacts: None,
        error: None,
        created_at: now,
        updated_at: now,
    }
}
fn templates() -> Templates {
    Templates::new(
        "inference".into(),
        BTreeMap::from([(
            "perf".into(),
            JobTemplate {
                kind: RunKind::Benchmark,
                image: format!("example/runner@sha256:{}", "a".repeat(64)),
                command: vec!["/runner".into()],
                workloads: vec!["shared-prefix".into()],
                service_account: "runner".into(),
                key_secret: None,
            },
        )]),
    )
    .unwrap()
}

#[test]
fn fingerprint_includes_conditions_and_workload_and_bounds_are_enforced() {
    let original = request();
    let mut changed = original.clone();
    changed.evidence.conditions.cache_state = "cold".into();
    assert_ne!(
        original.fingerprint().unwrap(),
        changed.fingerprint().unwrap()
    );
    changed = original.clone();
    changed.workload.seed += 1;
    assert_ne!(
        original.fingerprint().unwrap(),
        changed.fingerprint().unwrap()
    );
    changed.workload.concurrency = 257;
    assert!(changed.validate().is_err());
    changed = original;
    changed.workload.duration_seconds = 3601;
    assert!(changed.validate().is_err());
}

#[test]
fn jobs_are_bounded_and_foreign_jobs_cannot_be_adopted() {
    let mut run = run();
    let templates = templates();
    let mut job = build_job(&run, &templates).unwrap();
    let spec = job.spec.as_ref().unwrap();
    assert_eq!(spec.ttl_seconds_after_finished, Some(3600));
    assert_eq!(spec.active_deadline_seconds, Some(360));
    let pod = spec.template.spec.as_ref().unwrap();
    assert_eq!(pod.automount_service_account_token, Some(false));
    assert_eq!(
        pod.containers[0]
            .security_context
            .as_ref()
            .unwrap()
            .allow_privilege_escalation,
        Some(false)
    );
    assert!(pod.containers[0]
        .resources
        .as_ref()
        .unwrap()
        .limits
        .is_some());
    run.job_spec = Some(serde_json::to_value(&job.spec).unwrap());
    run.job_namespace = Some("inference".into());
    job.metadata.uid = Some("uid-1".into());
    assert!(verify_owned_job(&run, &job, "inference").is_err());
    assert!(verify_recoverable_job(&run, &job).is_ok());
    let mut foreign = job.clone();
    foreign
        .metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert("thalamus.io/ownership-nonce".into(), "wrong".into());
    assert!(verify_recoverable_job(&run, &foreign).is_err());
    let mut foreign = job.clone();
    foreign
        .spec
        .as_mut()
        .unwrap()
        .template
        .spec
        .as_mut()
        .unwrap()
        .containers[0]
        .image = Some("foreign".into());
    assert!(verify_recoverable_job(&run, &foreign).is_err());
    run.job_uid = Some("uid-1".into());
    assert!(verify_owned_job(&run, &job, "inference").is_ok());
    job.metadata.uid = Some("uid-2".into());
    assert!(verify_owned_job(&run, &job, "inference").is_err());
    run.request.workload.name = "unapproved".into();
    run.fingerprint = run.request.fingerprint().unwrap();
    assert!(build_job(&run, &templates).is_err());
}
