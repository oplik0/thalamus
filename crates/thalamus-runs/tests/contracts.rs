use chrono::{TimeZone, Utc};
use std::collections::BTreeMap;
use thalamus_runs::{
    build_job, job_name, verify_owned_job, verify_recoverable_job, EvidenceIdentity, JobTemplate,
    ResultUpload, Run, RunConditions, RunKind, RunRequest, Templates, Workload,
};
use uuid::Uuid;

fn request() -> RunRequest {
    RunRequest {
        project_id: Uuid::from_u128(100),
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
        ResultUpload {
            endpoint: "https://objects.example".into(),
            bucket: "run-results".into(),
            region: "us-east-1".into(),
            secret_prefix: "upload".into(),
        },
    )
    .unwrap()
}

#[test]
fn fingerprint_includes_conditions_and_workload_and_bounds_are_enforced() {
    let original = request();
    let mut other_project = original.clone();
    other_project.project_id = Uuid::from_u128(101);
    assert_ne!(
        original.fingerprint().unwrap(),
        other_project.fingerprint().unwrap()
    );
    other_project.project_id = Uuid::nil();
    assert!(other_project.validate().is_err());
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
    let env = pod.containers[0].env.as_ref().unwrap();
    let volume = pod
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .find(|v| v.name == "result-upload")
        .unwrap();
    let projected = volume.projected.as_ref().unwrap();
    let secret = projected.sources.as_ref().unwrap()[0]
        .secret
        .as_ref()
        .unwrap();
    assert_eq!(secret.name, format!("upload-{}", run.id));
    let item = &secret.items.as_ref().unwrap()[0];
    assert_eq!(item.key, "url");
    assert_eq!(item.path, "url");
    let mount = pod.containers[0]
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.name == "result-upload")
        .unwrap();
    assert_eq!(mount.mount_path, "/var/run/thalamus/result-upload");
    assert_eq!(mount.read_only, Some(true));
    assert!(mount.sub_path.is_none());
    for forbidden in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "THALAMUS_RESULT_UPLOAD_URL",
        "THALAMUS_RESULT_KEY",
        "THALAMUS_RESULT_ENDPOINT",
        "THALAMUS_RESULT_BUCKET",
        "THALAMUS_RESULT_PREFIX",
    ] {
        assert!(!env.iter().any(|entry| entry.name == forbidden));
    }
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

#[sqlx::test(migrations = "./migrations")]
async fn worker_pages_across_teams_and_ingestion_is_durable(pool: sqlx::PgPool) {
    use thalamus_runs::{ArtifactRef, Error, RunCursor, Store, Transition, WorkerStore};

    let store = Store::new(pool.clone());
    let worker = WorkerStore::new(pool.clone());
    let first = store
        .create("team-a", "actor", "one", &request())
        .await
        .unwrap();
    let second = store
        .create("team-b", "actor", "two", &request())
        .await
        .unwrap();
    let third = store
        .create("team-a", "actor", "three", &request())
        .await
        .unwrap();
    assert!(matches!(
        store.get("team-b", first.id).await,
        Err(Error::NotFound)
    ));
    assert!(worker.list_actionable(None, 0).await.is_err());
    assert!(worker.list_actionable(None, 101).await.is_err());

    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = worker.list_actionable(cursor, 1).await.unwrap();
        if page.is_empty() {
            break;
        }
        cursor = Some(RunCursor::from(&page[0]));
        ids.push(page[0].id);
    }
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&first.id) && ids.contains(&second.id) && ids.contains(&third.id));

    // Simulate the already-verified job identity before observing completion.
    sqlx::query("UPDATE thalamus_runs SET job_uid='job-1' WHERE id=$1")
        .bind(first.id)
        .execute(&pool)
        .await
        .unwrap();
    store
        .transition("team-a", "worker", first.id, Transition::Started)
        .await
        .unwrap();
    store
        .transition("team-a", "worker", first.id, Transition::Started)
        .await
        .unwrap();
    store
        .transition("team-a", "worker", first.id, Transition::JobSucceeded)
        .await
        .unwrap();
    assert!(worker
        .list_actionable(None, 100)
        .await
        .unwrap()
        .iter()
        .any(|run| run.id == first.id && run.state == "ingesting"));
    let artifact = ArtifactRef {
        sha256: "a".repeat(64),
        path: format!("runs/{}/{}", first.id, "a".repeat(64)),
        size: 6,
    };
    store
        .transition(
            "team-a",
            "worker",
            first.id,
            Transition::IngestionSucceeded {
                artifacts: vec![artifact.clone()],
            },
        )
        .await
        .unwrap();
    store
        .transition(
            "team-a",
            "worker",
            first.id,
            Transition::IngestionSucceeded {
                artifacts: vec![artifact.clone()],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.get("team-a", first.id).await.unwrap().artifacts,
        Some(vec![artifact])
    );
    assert!(!worker
        .list_actionable(None, 100)
        .await
        .unwrap()
        .iter()
        .any(|run| run.id == first.id));
    assert!(matches!(
        store
            .transition(
                "team-a",
                "worker",
                first.id,
                Transition::IngestionFailed {
                    error: "late".into()
                }
            )
            .await,
        Err(Error::InvalidTransition)
    ));
    sqlx::query("UPDATE thalamus_runs SET job_uid='job-2' WHERE id=$1")
        .bind(second.id)
        .execute(&pool)
        .await
        .unwrap();
    store
        .transition("team-b", "worker", second.id, Transition::Started)
        .await
        .unwrap();
    let artifact = ArtifactRef {
        sha256: "b".repeat(64),
        path: format!("runs/{}/{}", second.id, "b".repeat(64)),
        size: 15,
    };
    let failure = Transition::JobFailedWithArtifacts {
        error: "Kubernetes Job failed".into(),
        artifacts: vec![artifact.clone()],
    };
    store
        .transition("team-b", "worker", second.id, failure.clone())
        .await
        .unwrap();
    let result = store
        .transition("team-b", "worker", second.id, failure)
        .await
        .unwrap();
    assert_eq!(result.state, "failed");
    assert_eq!(result.artifacts, Some(vec![artifact]));
    assert_eq!(result.error.as_deref(), Some("Kubernetes Job failed"));
}

#[test]
fn validation_job_uses_only_operator_service_deployment_config() {
    let mut run = run();
    run.request.kind = RunKind::Validation;
    run.request.workload.model_budget_tokens = 1000;
    run.fingerprint = run.request.fingerprint().unwrap();
    let runner = JobTemplate {
        kind: RunKind::Validation,
        image: format!("example/runner@sha256:{}", "a".repeat(64)),
        command: vec!["/runner".into()],
        workloads: vec![run.request.workload.name.clone()],
        service_account: "runner".into(),
        key_secret: Some("api".into()),
    };
    let upload = ResultUpload {
        endpoint: "https://objects.example".into(),
        bucket: "run-results".into(),
        region: "us-east-1".into(),
        secret_prefix: "upload".into(),
    };
    let templates = Templates::new(
        "inference".into(),
        BTreeMap::from([("perf".into(), runner)]),
        upload,
    )
    .unwrap();
    assert!(build_job(&run, &templates).is_err());
    let config = serde_json::json!({"base_url":"https://candidate.example/v1", "model":"allowed", "responses":true, "anthropic":false});
    let templates = templates
        .with_validation_configs(BTreeMap::from([(
            run.request.service.clone(),
            BTreeMap::from([(run.request.deployment.clone(), config.clone())]),
        )]))
        .unwrap();
    let job = build_job(&run, &templates).unwrap();
    let env = job.spec.unwrap().template.spec.unwrap().containers[0]
        .env
        .clone()
        .unwrap();
    let value = env
        .iter()
        .find(|e| e.name == "THALAMUS_VALIDATION_CONFIG")
        .unwrap()
        .value
        .as_ref()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(value).unwrap(),
        config
    );
    run.request.workload.model_budget_tokens = 0;
    run.fingerprint = run.request.fingerprint().unwrap();
    assert!(build_job(&run, &templates).is_err());
    run.request.workload.model_budget_tokens = 1000;
    run.request.deployment = "unconfigured".into();
    run.fingerprint = run.request.fingerprint().unwrap();
    assert!(build_job(&run, &templates).is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn transaction_creation_obeys_caller_rollback(pool: sqlx::PgPool) {
    let store = thalamus_runs::Store::new(pool.clone());
    let mut tx = pool.begin().await.unwrap();
    let run = store
        .create_in_transaction(&mut tx, "team", "actor", "key", &request())
        .await
        .unwrap();
    assert_eq!(
        store
            .get_in_transaction(&mut tx, "team", run.id)
            .await
            .unwrap()
            .id,
        run.id
    );
    assert!(matches!(
        store.get_in_transaction(&mut tx, "other", run.id).await,
        Err(thalamus_runs::Error::NotFound)
    ));
    assert_eq!(
        store
            .list_in_transaction(&mut tx, "team", 10)
            .await
            .unwrap()[0]
            .id,
        run.id
    );
    assert!(store
        .list_in_transaction(&mut tx, "other", 10)
        .await
        .unwrap()
        .is_empty());
    assert!(store.list_in_transaction(&mut tx, "team", 0).await.is_err());
    tx.rollback().await.unwrap();
    assert!(matches!(
        store.get("team", run.id).await,
        Err(thalamus_runs::Error::NotFound)
    ));
    let audit_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM thalamus_run_audit WHERE run_id=$1")
            .bind(run.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit_count, 0);
}
