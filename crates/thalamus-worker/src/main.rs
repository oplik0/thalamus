use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use futures::StreamExt;
use k8s_openapi::{
    api::{batch::v1::Job, core::v1::Secret},
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
    ByteString,
};
use kube::{
    api::{DeleteParams, ListParams, PostParams, Preconditions},
    Api,
};
use object_store::{aws::AmazonS3, path::Path as ObjectPath, signer::Signer, ObjectStore};
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use std::{
    collections::{BTreeMap, HashMap},
    env,
    error::Error,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thalamus_identity::{BudgetCap, WorkerIdentity};
use thalamus_k8s::{K8s, ReadKind, WatchState};
use thalamus_runs::{
    Artifacts, JobTemplate, ResultUpload, RunCursor, RunKind, S3Config, Store, Templates,
    Transition, WorkerStore,
};
use thalamus_runtime::admission::{validate_profiles, RunnerProfiles};
use thalamus_usage::{PrometheusSource, UsageStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

mod debug;
mod idp_sync;
mod runtime_integration;
mod workflow_controller;
mod workflow_validation;

use debug::DebugReconciler;
use idp_sync::IdentitySync;
use runtime_integration::RuntimeIntegration;
use workflow_controller::WorkflowController;

type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
fn required(name: &str) -> Result<String> {
    let value = env::var(name)?;
    if value.trim().is_empty() {
        return Err(format!("{name} is empty").into());
    }
    Ok(value)
}

#[derive(Deserialize)]
struct Runner {
    kind: RunKind,
    image: String,
    command: Vec<String>,
    workloads: Vec<String>,
    service_account: String,
    key_secret: Option<String>,
    route_target: String,
}
struct UsageConfig {
    url: String,
    input: String,
    output: String,
    step: u32,
    start: chrono::NaiveDate,
    retention_days: u32,
}
struct Config {
    database: String,
    namespace: String,
    projection: String,
    labels: BTreeMap<String, String>,
    holder: String,
    lease: String,
    health: String,
    templates: Templates,
    test_profiles: RunnerProfiles,
    runner_targets: BTreeMap<String, String>,
    catalog: String,
    s3: S3Config,
    runtime_config: String,
    usage: UsageConfig,
}
impl Config {
    fn load() -> Result<Self> {
        let namespace = required("THALAMUS_K8S_NAMESPACE")?;
        let runners: BTreeMap<String, Runner> =
            serde_json::from_str(&required("THALAMUS_RUNNERS_JSON")?)?;
        let mut runner_targets = BTreeMap::new();
        let mut templates_by_name = BTreeMap::new();
        for (name, r) in runners {
            if !matches!(r.route_target.as_str(), "candidate" | "public")
                || (r.kind == RunKind::Benchmark && r.route_target != "candidate")
            {
                return Err("invalid runner route target".into());
            }
            runner_targets.insert(name.clone(), r.route_target);
            templates_by_name.insert(
                name,
                JobTemplate {
                    kind: r.kind,
                    image: r.image,
                    command: r.command,
                    workloads: r.workloads,
                    service_account: r.service_account,
                    key_secret: r.key_secret,
                },
            );
        }
        let labels: BTreeMap<String, String> =
            serde_json::from_str(&required("THALAMUS_GATEWAY_SELECTOR_LABELS")?)?;
        if labels.is_empty() {
            return Err("gateway selector labels must not be empty".into());
        }
        let s3_endpoint = required("THALAMUS_S3_ENDPOINT")?;
        if !s3_endpoint.starts_with("https://") {
            return Err("S3 endpoint must use HTTPS".into());
        }
        let s3_bucket = required("THALAMUS_S3_BUCKET")?;
        let s3_region = required("THALAMUS_S3_REGION")?;
        let upload = ResultUpload {
            endpoint: s3_endpoint,
            bucket: s3_bucket,
            region: s3_region,
            secret_prefix: required("THALAMUS_S3_RUN_SECRET_PREFIX")?,
        };
        let test_profiles: RunnerProfiles =
            serde_json::from_str(&required("THALAMUS_VALIDATION_CONFIGS_JSON")?)?;
        validate_profiles(&test_profiles)?;
        for runners in test_profiles.values() {
            for (name, profile) in runners {
                if runner_targets.get(name).map(String::as_str)
                    != Some(profile.route_target.as_str())
                    || templates_by_name.get(name).map(|runner| &runner.kind) != Some(&profile.kind)
                {
                    return Err("test profile does not match the trusted runner".into());
                }
            }
        }
        let templates = Templates::new(namespace.clone(), templates_by_name, upload)?;
        let s3 = S3Config::from_env()?.ok_or("S3 configuration is required")?;
        let usage = UsageConfig {
            url: required("THALAMUS_USAGE_PROMETHEUS_URL")?,
            input: required("THALAMUS_USAGE_INPUT_COUNTER")?,
            output: required("THALAMUS_USAGE_OUTPUT_COUNTER")?,
            step: required("THALAMUS_USAGE_STEP_SECONDS")?.parse()?,
            start: required("THALAMUS_USAGE_START_DATE")?.parse()?,
            retention_days: required("THALAMUS_USAGE_RETENTION_DAYS")?.parse()?,
        };
        if usage.start > Utc::now().date_naive() {
            return Err("usage start date is in the future".into());
        }
        if !(1..=36500).contains(&usage.retention_days) {
            return Err("invalid usage retention days".into());
        }
        Ok(Self {
            database: required("DATABASE_URL")?,
            templates,
            test_profiles,
            runner_targets,
            namespace,
            projection: required("THALAMUS_GATEWAY_CONFIGMAP")?,
            labels,
            holder: required("THALAMUS_WORKER_ID")?,
            lease: required("THALAMUS_LEASE_NAME")?,
            health: required("THALAMUS_WORKER_HEALTH_ADDR")?,
            catalog: required("THALAMUS_CATALOG_DIR")?,
            s3,
            runtime_config: required("THALAMUS_RUNTIME_CONFIG")?,
            usage,
        })
    }
}

#[derive(Default)]
struct Health {
    last_success: Option<Instant>,
    error: Option<String>,
    watches: HashMap<String, WatchState>,
    usage_ready: bool,
}

async fn watch(
    k8s: K8s,
    health: Arc<Mutex<Health>>,
    label: &'static str,
    kind: ReadKind,
    version: String,
) {
    loop {
        {
            let mut h = health.lock().unwrap();
            h.watches.entry(label.into()).or_default().invalidate();
        }
        if let Ok(stream) = k8s.watch(kind, &version) {
            futures::pin_mut!(stream);
            // Quiet streams are periodically restarted for an authoritative relist.
            let deadline = tokio::time::sleep(Duration::from_secs(60));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    event = stream.next() => match event {
                        Some(Ok(event)) => health.lock().unwrap().watches.entry(label.into()).or_default().apply(event, Instant::now()),
                        _ => break,
                    },
                    _ = &mut deadline => break,
                }
            }
        }
        health
            .lock()
            .unwrap()
            .watches
            .entry(label.into())
            .or_default()
            .invalidate();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn health_server(
    listener: tokio::net::TcpListener,
    health: Arc<Mutex<Health>>,
) -> Result<()> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let health = health.clone();
        tokio::spawn(async move {
            let mut request = [0u8; 1024];
            let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_secs(2), socket.read(&mut request)).await
            else {
                return;
            };
            let path = std::str::from_utf8(&request[..n])
                .ok()
                .and_then(|s| s.lines().next())
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");
            let (status, body) = {
                let h = health.lock().unwrap();
                let watches_fresh = h.watches.len() == 4
                    && h.watches
                        .values()
                        .all(|w| w.is_fresh(Instant::now(), Duration::from_secs(90)));
                let ready = watches_fresh
                    && h.usage_ready
                    && h.error.is_none()
                    && h.last_success
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(90));
                if path == "/livez" {
                    ("200 OK", "ok".to_string())
                } else if path == "/readyz" && ready {
                    ("200 OK", "ready".to_string())
                } else if path == "/readyz" && !h.usage_ready {
                    (
                        "503 Service Unavailable",
                        "usage ingestion unavailable or catching up".to_string(),
                    )
                } else if path == "/readyz" {
                    ("503 Service Unavailable", "not ready".to_string())
                } else {
                    ("404 Not Found", "not found".to_string())
                }
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapFile {
    teams: Vec<CapEntry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapEntry {
    team_id: Uuid,
    max_tokens: i64,
    unit: String,
    window: String,
    #[serde(default = "default_allowed_classes")]
    allowed_classes: Vec<String>,
}

fn default_allowed_classes() -> Vec<String> {
    vec!["standard".into()]
}

fn git_output(catalog: &str, args: &[&str]) -> Result<Vec<u8>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(catalog)
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err("catalog git read failed".into());
    }
    Ok(out.stdout)
}

fn caps_object(commit: &str, prefix: &str) -> Result<String> {
    if !prefix.is_empty()
        && (!prefix.ends_with('/')
            || prefix[..prefix.len() - 1].split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || part.bytes().any(|byte| {
                        !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                    })
            }))
    {
        return Err("invalid catalog Git prefix".into());
    }
    Ok(format!("{commit}:{prefix}budget-caps.json"))
}

/// Read only a tracked file from a fixed commit, never a dirty worktree.
async fn sync_caps(db: &PgPool, identity: &WorkerIdentity, catalog: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind("thalamus/budget-caps")
        .execute(&mut *tx)
        .await?;
    let commit = String::from_utf8(git_output(
        catalog,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )?)?;
    let commit = commit.trim();
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("invalid catalog commit".into());
    }
    let prefix = String::from_utf8(git_output(catalog, &["rev-parse", "--show-prefix"])?)?;
    let prefix = prefix
        .strip_suffix('\n')
        .ok_or("invalid catalog Git prefix")?;
    let object = caps_object(commit, prefix)?;
    let size = String::from_utf8(git_output(catalog, &["cat-file", "-s", &object])?)?
        .trim()
        .parse::<usize>()?;
    if size > 1024 * 1024 {
        return Err("budget caps too large".into());
    }
    let source = git_output(catalog, &["show", &object])?;
    if source.len() != size {
        return Err("invalid budget caps object".into());
    }
    let file: CapFile = serde_json::from_slice(&source)?;
    let caps: Vec<BudgetCap> = file
        .teams
        .into_iter()
        .map(|c| BudgetCap {
            team_id: c.team_id,
            max_tokens: c.max_tokens,
            unit: c.unit,
            window: c.window,
            allowed_classes: c.allowed_classes,
        })
        .collect();
    if String::from_utf8(git_output(
        catalog,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )?)?
    .trim()
        != commit
    {
        return Err("catalog changed during cap read".into());
    }
    identity.sync_budget_caps(commit, &caps).await?;
    tx.commit().await?;
    Ok(())
}

async fn project(db: &PgPool, identity: &WorkerIdentity, k8s: &K8s, config: &Config) -> Result<()> {
    // Serializing the read and write across replicas prevents an older snapshot
    // from winning after a newer one. The Lease alone does not fence a writer.
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "thalamus/projection/{}/{}",
            config.namespace, config.projection
        ))
        .execute(&mut *tx)
        .await?;
    let keys = identity.active_projection().await?;
    let entries = thalamus_worker::gateway_entries(&keys, Utc::now())?;
    let ids: Vec<Uuid> = entries
        .keys()
        .map(|id| Uuid::parse_str(id))
        .collect::<std::result::Result<_, _>>()?;
    let digest = thalamus_worker::projection_digest(&entries);
    let applied = k8s
        .apply_projection_with_labels(&config.projection, entries, config.labels.clone())
        .await?;
    sqlx::query("INSERT INTO thalamus_worker_projection(namespace,name,generation,digest,resource_version,observed_at,error) VALUES ($1,$2,1,$3,$4,now(),NULL) ON CONFLICT(namespace,name) DO UPDATE SET generation=thalamus_worker_projection.generation + CASE WHEN thalamus_worker_projection.digest IS DISTINCT FROM EXCLUDED.digest THEN 1 ELSE 0 END,digest=EXCLUDED.digest,resource_version=EXCLUDED.resource_version,observed_at=now(),error=NULL")
        .bind(&config.namespace).bind(&config.projection).bind(digest).bind(applied.metadata.resource_version).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO thalamus_worker_revocations(namespace,name,key_id,removed_at) SELECT $1,$2,k.id,now() FROM identity_keys k WHERE k.revoked_at IS NOT NULL AND NOT (k.id = ANY($3)) ON CONFLICT(namespace,name,key_id) DO NOTHING").bind(&config.namespace).bind(&config.projection).bind(&ids).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn record_projection_error(db: &PgPool, config: &Config) {
    // Never persist error bodies, which may contain credentials or object data.
    let _ = sqlx::query("INSERT INTO thalamus_worker_projection(namespace,name,generation,error) VALUES ($1,$2,0,'reconciliation failed') ON CONFLICT(namespace,name) DO UPDATE SET error='reconciliation failed'") .bind(&config.namespace).bind(&config.projection).execute(db).await;
}
/// Extract only the trusted runner's declared key reference.
fn job_secret(job: &Job) -> Result<Option<String>> {
    let mut result = None;
    let pod = job
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .ok_or("job pod missing")?;
    for env in pod.containers.iter().flat_map(|c| c.env.iter().flatten()) {
        if env.name == "THALAMUS_API_KEY" {
            let reference = env
                .value_from
                .as_ref()
                .and_then(|v| v.secret_key_ref.as_ref())
                .ok_or("invalid key reference")?;
            if reference.key != "key"
                || reference.name.is_empty()
                || !reference.name.starts_with("thalamus-")
                || result.replace(reference.name.clone()).is_some()
            {
                return Err("invalid key secret".into());
            }
        }
    }
    Ok(result)
}
fn upload_secret_name(job: &Job, run_id: Uuid) -> Result<String> {
    let pod = job
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .ok_or("job pod missing")?;
    let volumes = pod.volumes.as_ref().ok_or("missing upload volume")?;
    let volume = volumes
        .iter()
        .find(|v| v.name == "result-upload")
        .ok_or("missing upload volume")?;
    let sources = volume
        .projected
        .as_ref()
        .and_then(|p| p.sources.as_ref())
        .ok_or("missing upload projection")?;
    if sources.len() != 2 {
        return Err("invalid upload projection".into());
    }
    let secret = sources[0].secret.as_ref().ok_or("missing upload secret")?;
    let items = secret.items.as_ref().ok_or("missing upload item")?;
    let ca = sources[1]
        .secret
        .as_ref()
        .ok_or("missing upload CA projection")?;
    let ca_items = ca.items.as_ref().ok_or("missing upload CA item")?;
    if items.len() != 1
        || items[0].key != "url"
        || items[0].path != "url"
        || secret.optional == Some(true)
        || ca.name != secret.name
        || ca.optional != Some(true)
        || ca_items.len() != 1
        || ca_items[0].key != "ca.crt"
        || ca_items[0].path != "ca.pem"
        || !secret.name.starts_with("thalamus-")
        || !secret.name.ends_with(&format!("-{run_id}"))
    {
        return Err("invalid upload projection".into());
    }
    Ok(secret.name.clone())
}

fn verify_upload_secret(secret: &Secret, run: &thalamus_runs::Run, name: &str) -> Result<()> {
    let meta = &secret.metadata;
    if meta.name.as_deref() != Some(name)
        || meta
            .labels
            .as_ref()
            .and_then(|l| l.get("app.kubernetes.io/managed-by"))
            .map(String::as_str)
            != Some("thalamus")
        || meta
            .labels
            .as_ref()
            .and_then(|l| l.get("thalamus.io/run-id"))
            .map(String::as_str)
            != Some(run.id.to_string().as_str())
        || meta
            .annotations
            .as_ref()
            .and_then(|a| a.get("thalamus.io/ownership-nonce"))
            .map(String::as_str)
            != Some(run.ownership_nonce.to_string().as_str())
    {
        return Err("foreign upload secret".into());
    }
    let data = secret.data.as_ref().ok_or("missing upload data")?;
    if !(1..=2).contains(&data.len())
        || data
            .get("ca.crt")
            .is_some_and(|ca| ca.0.is_empty() || ca.0.len() > 1024 * 1024)
        || data.keys().any(|key| key != "url" && key != "ca.crt")
    {
        return Err("invalid upload data".into());
    }
    let url = reqwest::Url::parse(std::str::from_utf8(
        &data.get("url").ok_or("missing upload url")?.0,
    )?)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_none()
        || !url
            .path()
            .ends_with(&format!("/runs/{}/result/output.json", run.id))
    {
        return Err("invalid upload url".into());
    }
    Ok(())
}

async fn provision_upload_secret(
    db: &PgPool,
    client: &kube::Client,
    namespace: &str,
    job: &Job,
    run: &thalamus_runs::Run,
    objects: &AmazonS3,
    ca_pem: Option<&[u8]>,
) -> Result<()> {
    let name = upload_secret_name(job, run.id)?;
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("thalamus/run-key/{}", run.id))
        .execute(&mut *tx)
        .await?;
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let existing = api.get_opt(&name).await?;
    if let Some(secret) = &existing {
        verify_upload_secret(secret, run, &name)?;
        let expires = secret
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("thalamus.io/expires-at"))
            .ok_or("missing upload expiry")?
            .parse::<chrono::DateTime<Utc>>()?;
        let current_ca = secret
            .data
            .as_ref()
            .and_then(|data| data.get("ca.crt"))
            .map(|ca| ca.0.as_slice());
        if expires > Utc::now() + chrono::Duration::seconds(180) && current_ca == ca_pem {
            tx.commit().await?;
            return Ok(());
        }
    }
    let deadline = job
        .spec
        .as_ref()
        .and_then(|s| s.active_deadline_seconds)
        .ok_or("job deadline missing")?;
    if !(1..=86400).contains(&deadline) {
        return Err("invalid job deadline".into());
    }
    let path = ObjectPath::from(format!("runs/{}/result/output.json", run.id));
    let url = objects
        .signed_url(
            reqwest::Method::PUT,
            &path,
            Duration::from_secs(deadline as u64),
        )
        .await?;
    let expires = Utc::now() + chrono::Duration::seconds(deadline);
    let mut data = BTreeMap::from([("url".into(), ByteString(url.as_str().as_bytes().to_vec()))]);
    if let Some(ca) = ca_pem {
        data.insert("ca.crt".into(), ByteString(ca.to_vec()));
    }
    let mut s = Secret {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(namespace.into()),
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/managed-by".into(), "thalamus".into()),
                ("thalamus.io/run-id".into(), run.id.to_string()),
            ])),
            annotations: Some(BTreeMap::from([
                (
                    "thalamus.io/ownership-nonce".into(),
                    run.ownership_nonce.to_string(),
                ),
                ("thalamus.io/expires-at".into(), expires.to_rfc3339()),
            ])),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("Opaque".into()),
        ..Default::default()
    };
    match existing {
        Some(previous) => {
            s.metadata.resource_version = Some(
                previous
                    .metadata
                    .resource_version
                    .ok_or("secret resource version missing")?,
            );
            api.replace(&name, &PostParams::default(), &s).await?;
        }
        None => {
            api.create(&PostParams::default(), &s).await?;
        }
    };
    tx.commit().await?;
    Ok(())
}

fn verify_secret(
    secret: &Secret,
    run: &thalamus_runs::Run,
    name: &str,
) -> Result<(String, chrono::DateTime<Utc>)> {
    let meta = &secret.metadata;
    if meta.name.as_deref() != Some(name)
        || meta
            .labels
            .as_ref()
            .and_then(|l| l.get("app.kubernetes.io/managed-by"))
            .map(String::as_str)
            != Some("thalamus")
        || meta
            .labels
            .as_ref()
            .and_then(|l| l.get("thalamus.io/run-id"))
            .map(String::as_str)
            != Some(run.id.to_string().as_str())
        || meta
            .annotations
            .as_ref()
            .and_then(|a| a.get("thalamus.io/ownership-nonce"))
            .map(String::as_str)
            != Some(run.ownership_nonce.to_string().as_str())
    {
        return Err("foreign job secret".into());
    }
    let data = secret.data.as_ref().ok_or("missing secret data")?;
    if data.len() != 1 {
        return Err("invalid job secret data".into());
    }
    let key = std::str::from_utf8(&data.get("key").ok_or("missing job key")?.0)?;
    let encoded = key.strip_prefix("thl_").ok_or("invalid job key")?;
    if URL_SAFE_NO_PAD.decode(encoded)?.len() != 32 {
        return Err("invalid job key".into());
    }
    let expires = meta
        .annotations
        .as_ref()
        .and_then(|a| a.get("thalamus.io/expires-at"))
        .ok_or("missing key expiry")?
        .parse::<chrono::DateTime<Utc>>()?;
    Ok((key.to_owned(), expires))
}

async fn provision_secret(
    db: &PgPool,
    identity: &WorkerIdentity,
    client: &kube::Client,
    config: &Config,
    run: &thalamus_runs::Run,
    name: &str,
) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("thalamus/run-key/{}", run.id))
        .execute(&mut *tx)
        .await?;
    let api: Api<Secret> = Api::namespaced(client.clone(), &config.namespace);
    let existing = api.get_opt(name).await?;
    let existing = match existing {
        Some(mut s) => {
            let (_, expires) = verify_secret(&s, run, name)?;
            if expires <= Utc::now() {
                if run.job_uid.is_some() {
                    return Err("cannot rotate key for a recorded Job".into());
                }
                // Caller holds the Job-create lock and checked that no Job exists.
                let mut random = [0u8; 32];
                OsRng.fill_bytes(&mut random);
                let key = format!("thl_{}", URL_SAFE_NO_PAD.encode(random));
                let expires = chrono::DateTime::<Utc>::from_timestamp_micros(
                    (Utc::now()
                        + chrono::Duration::seconds(
                            i64::from(run.request.workload.duration_seconds) + 1800,
                        ))
                    .timestamp_micros(),
                )
                .ok_or("invalid expiry")?;
                s.metadata
                    .resource_version
                    .as_ref()
                    .ok_or("secret resource version missing")?;
                s.metadata
                    .annotations
                    .as_mut()
                    .ok_or("secret annotations missing")?
                    .insert("thalamus.io/expires-at".into(), expires.to_rfc3339());
                s.data = Some(BTreeMap::from([(
                    "key".into(),
                    ByteString(key.into_bytes()),
                )]));
                s = api.replace(name, &PostParams::default(), &s).await?;
            }
            Some(s)
        }
        None => None,
    };
    let secret = match existing {
        Some(s) => s,
        None => {
            let mut random = [0u8; 32];
            OsRng.fill_bytes(&mut random);
            let key = format!("thl_{}", URL_SAFE_NO_PAD.encode(random));
            let expires = chrono::DateTime::<Utc>::from_timestamp_micros(
                (Utc::now()
                    + chrono::Duration::seconds(
                        i64::from(run.request.workload.duration_seconds) + 1800,
                    ))
                .timestamp_micros(),
            )
            .ok_or("invalid expiry")?;
            let secret = Secret {
                metadata: ObjectMeta {
                    name: Some(name.into()),
                    namespace: Some(config.namespace.clone()),
                    labels: Some(BTreeMap::from([
                        ("app.kubernetes.io/managed-by".into(), "thalamus".into()),
                        ("thalamus.io/run-id".into(), run.id.to_string()),
                    ])),
                    annotations: Some(BTreeMap::from([
                        (
                            "thalamus.io/ownership-nonce".into(),
                            run.ownership_nonce.to_string(),
                        ),
                        ("thalamus.io/expires-at".into(), expires.to_rfc3339()),
                    ])),
                    ..Default::default()
                },
                data: Some(BTreeMap::from([(
                    "key".into(),
                    ByteString(key.into_bytes()),
                )])),
                type_: Some("Opaque".into()),
                ..Default::default()
            };
            match api.create(&PostParams::default(), &secret).await {
                Ok(s) => s,
                Err(kube::Error::Api(e)) if e.code == 409 => api.get(name).await?,
                Err(e) => return Err(e.into()),
            }
        }
    };
    let (key, expires) = verify_secret(&secret, run, name)?;
    if expires
        <= Utc::now()
            + chrono::Duration::seconds(i64::from(run.request.workload.duration_seconds) + 300)
    {
        return Err("job key has insufficient remaining lifetime".into());
    }
    let user = Uuid::parse_str(&run.actor)?;
    let team = Uuid::parse_str(&run.team)?;
    let budget = i64::from(run.request.workload.model_budget_tokens);
    let request = thalamus_identity::RunKeyRequest {
        run_id: run.id,
        user_id: user,
        team_id: team,
        project_id: run.request.project_id,
        service: &run.request.service,
        deployment: &run.request.deployment,
        run_kind: match &run.request.kind {
            RunKind::Validation => "validation",
            RunKind::Benchmark => "benchmark",
            RunKind::Debug => return Err("debug jobs require the debug reconciler".into()),
        },
        request_budget: (run.request.kind == RunKind::Benchmark)
            .then_some(i64::from(run.request.workload.max_requests)),
        expires_at: expires,
        secret: &key,
        budget,
    };
    match identity.register_run_key(request).await {
        Ok(_) => {}
        Err(thalamus_identity::Error::Invalid("run key registration conflict")) => {
            identity.replace_expired_run_key(request).await?;
        }
        Err(error) => return Err(error.into()),
    }
    tx.commit().await?;
    Ok(())
}

async fn cleanup_secret(
    db: &PgPool,
    identity: &WorkerIdentity,
    client: &kube::Client,
    config: &Config,
    run: &thalamus_runs::Run,
    job: &Job,
) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("thalamus/run-key/{}", run.id))
        .execute(&mut *tx)
        .await?;
    let api: Api<Secret> = Api::namespaced(client.clone(), &config.namespace);
    let key = job_secret(job)?;
    if key.is_some() {
        match identity.revoke_run_key(run.id).await {
            Ok(()) | Err(thalamus_identity::Error::NotFound) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let upload = upload_secret_name(job, run.id)?;
    for (name, is_upload) in key
        .into_iter()
        .map(|n| (n, false))
        .chain(std::iter::once((upload, true)))
    {
        if let Some(secret) = api.get_opt(&name).await? {
            if is_upload {
                verify_upload_secret(&secret, run, &name)?;
            } else {
                verify_secret(&secret, run, &name)?;
            }
            let uid = secret.metadata.uid.ok_or("secret UID missing")?;
            let rv = secret
                .metadata
                .resource_version
                .ok_or("secret resource version missing")?;
            api.delete(
                &name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(uid),
                        resource_version: Some(rv),
                    }),
                    ..Default::default()
                },
            )
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}
/// Delete expired owned Secrets for terminal or deleted runs. Active runs
/// retain and rotate their Secrets; reserved names require RBAC/admission.
async fn gc_secrets(
    db: &PgPool,
    identity: &WorkerIdentity,
    client: &kube::Client,
    namespace: &str,
) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    for secret in api
        .list(&ListParams::default().labels("app.kubernetes.io/managed-by=thalamus"))
        .await?
        .items
    {
        let meta = &secret.metadata;
        let Some(id) = meta
            .labels
            .as_ref()
            .and_then(|l| l.get("thalamus.io/run-id"))
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        let Some(name) = meta.name.as_deref() else {
            continue;
        };
        if !name.starts_with("thalamus-") || !name.ends_with(&format!("-{id}")) {
            continue;
        }
        let Some(expiry) = meta
            .annotations
            .as_ref()
            .and_then(|a| a.get("thalamus.io/expires-at"))
            .and_then(|s| s.parse::<chrono::DateTime<Utc>>().ok())
        else {
            continue;
        };
        if expiry > Utc::now() {
            continue;
        }
        let mut tx = db.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("thalamus/run-key/{id}"))
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT state,ownership_nonce FROM thalamus_runs WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let annotated = meta
            .annotations
            .as_ref()
            .and_then(|a| a.get("thalamus.io/ownership-nonce"))
            .and_then(|s| Uuid::parse_str(s).ok());
        let Some(annotated) = annotated else {
            continue;
        };
        if let Some(row) = row {
            let state: String = row.try_get("state")?;
            let nonce: Uuid = row.try_get("ownership_nonce")?;
            if !matches!(
                state.as_str(),
                "failed" | "succeeded" | "ingesting" | "ingestion_failed"
            ) || annotated != nonce
            {
                continue;
            }
        } else {
            let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
            if jobs.get_opt(&thalamus_runs::job_name(id)).await?.is_some() {
                continue;
            }
        }
        if secret
            .data
            .as_ref()
            .is_some_and(|data| data.contains_key("key"))
        {
            match identity.revoke_run_key(id).await {
                Ok(()) | Err(thalamus_identity::Error::NotFound) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let uid = meta.uid.clone().ok_or("secret UID missing")?;
        let rv = meta
            .resource_version
            .clone()
            .ok_or("secret resource version missing")?;
        match api
            .delete(
                name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(uid),
                        resource_version: Some(rv),
                    }),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => return Err(e.into()),
        }
        tx.commit().await?;
    }
    Ok(())
}

async fn ingest_result(
    objects: &dyn ObjectStore,
    artifacts: &Artifacts,
    id: Uuid,
) -> Result<Option<thalamus_runs::ArtifactRef>> {
    let path = ObjectPath::from(format!("runs/{id}/result/output.json"));
    let result = match objects.get(&path).await {
        Ok(result) => result,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if result.meta.size > 64 * 1024 * 1024 {
        return Err("result too large".into());
    }
    let mut stream = result.into_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if data.len().saturating_add(chunk.len()) > 64 * 1024 * 1024 {
            return Err("result too large".into());
        }
        data.extend_from_slice(&chunk);
    }
    if !serde_json::from_slice::<serde_json::Value>(&data)?.is_object() {
        return Err("result must be a JSON object".into());
    }
    Ok(Some(artifacts.put(id, data.into()).await?))
}

fn failed_job_transition(artifact: Option<thalamus_runs::ArtifactRef>) -> Transition {
    let error = "Kubernetes Job failed".to_owned();
    match artifact {
        Some(artifact) => Transition::JobFailedWithArtifacts {
            error,
            artifacts: vec![artifact],
        },
        None => Transition::JobFailed { error },
    }
}

fn require_generic_runner(kind: &RunKind) -> Result<()> {
    if matches!(kind, RunKind::Debug) {
        return Err("debug runs require an integrated debug launch authority".into());
    }
    Ok(())
}

struct RunServices<'a> {
    store: &'a Store,
    k8s: &'a K8s,
    client: &'a kube::Client,
    db: &'a PgPool,
    identity: &'a WorkerIdentity,
    config: &'a Config,
    objects: &'a AmazonS3,
    artifacts: &'a Artifacts,
    runtime: &'a RuntimeIntegration,
}

async fn fail_pending(store: &Store, run: &thalamus_runs::Run, reason: &str) -> Result<()> {
    store
        .transition(
            &run.team,
            "thalamus-worker",
            run.id,
            Transition::JobFailed {
                error: reason.into(),
            },
        )
        .await?;
    Ok(())
}

async fn fail_ingestion(store: &Store, run: &thalamus_runs::Run, reason: &str) -> Result<()> {
    store
        .transition(
            &run.team,
            "thalamus-worker",
            run.id,
            Transition::IngestionFailed {
                error: reason.into(),
            },
        )
        .await?;
    Ok(())
}

/// Stop a rejected launch without allowing a crash to cause a second Job.
/// The Job has already passed the persisted ownership check if it exists.
async fn reject_prelaunch(
    services: &RunServices<'_>,
    run: &thalamus_runs::Run,
    job: &Job,
    existing: bool,
) -> Result<()> {
    cleanup_secret(
        services.db,
        services.identity,
        services.client,
        services.config,
        run,
        job,
    )
    .await?;
    project(
        services.db,
        services.identity,
        services.k8s,
        services.config,
    )
    .await?;
    services
        .store
        .transition(
            &run.team,
            "thalamus-worker",
            run.id,
            Transition::JobFailed {
                error: "runtime evidence could not be established before launch".into(),
            },
        )
        .await?;
    if existing {
        let uid = job.metadata.uid.clone().ok_or("Job UID missing")?;
        let jobs: Api<Job> = Api::namespaced(services.client.clone(), &services.config.namespace);
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid),
                resource_version: None,
            }),
            ..Default::default()
        };
        match jobs.delete(&run.job_name, &params).await {
            Ok(_) => {}
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn run_one(run: thalamus_runs::Run, services: &RunServices<'_>) -> Result<()> {
    let &RunServices {
        store,
        k8s,
        client,
        db,
        identity,
        config,
        objects,
        artifacts,
        runtime,
    } = services;
    const ACTOR: &str = "thalamus-worker";
    require_generic_runner(&run.request.kind)?;
    let route_target = config
        .runner_targets
        .get(&run.request.runner)
        .ok_or("unknown runner route target")?;
    let mut run = run;
    if run.state == "pending" {
        // Lease expiry cannot fence a delayed worker. Serialize absence check,
        // key replacement and Job CREATE across all worker replicas.
        let mut create_lock = db.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("thalamus/job-create/{}", run.id))
            .execute(&mut *create_lock)
            .await?;
        run = store.get(&run.team, run.id).await?;
        if run.state != "pending" {
            create_lock.commit().await?;
            return Ok(());
        }
        let resolved = if run.job_spec.is_none() {
            let profile = config
                .test_profiles
                .get(&run.request.service)
                .and_then(|runners| runners.get(&run.request.runner));
            let Some(profile) = profile else {
                fail_pending(store, &run, "trusted test profile is unavailable").await?;
                create_lock.commit().await?;
                return Ok(());
            };
            if profile.kind != run.request.kind
                || profile.route_target.as_str() != route_target.as_str()
            {
                fail_pending(store, &run, "trusted test profile does not match the run").await?;
                create_lock.commit().await?;
                return Ok(());
            }
            if thalamus_runtime::admission::validate_test_workload(profile, &run.request.workload)
                .is_err()
            {
                fail_pending(
                    store,
                    &run,
                    "workload does not match the trusted test profile",
                )
                .await?;
                create_lock.commit().await?;
                return Ok(());
            }
            match runtime
                .resolve_config(&run, route_target, &profile.options)
                .await
            {
                Ok(Some(config)) => Some(config),
                Ok(None) => return Err("resolved Job configuration missing".into()),
                Err(error) if runtime_integration::is_permanent_failure(error.as_ref()) => {
                    fail_pending(store, &run, "runtime identity changed before launch").await?;
                    create_lock.commit().await?;
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let job = match store
            .prepare_job(
                &run.team,
                ACTOR,
                run.id,
                &config.templates,
                resolved.as_ref(),
            )
            .await
        {
            Ok(job) => job,
            Err(error)
                if run.job_spec.is_none() && runtime_integration::is_permanent_failure(&error) =>
            {
                fail_pending(store, &run, "runner configuration could not prepare a Job").await?;
                create_lock.commit().await?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        // prepare_job persists the spec; refresh so conflict recovery verifies it.
        run = store.get(&run.team, run.id).await?;
        let job = if let Some(existing) = k8s.get_job(&run.job_name).await? {
            thalamus_runs::verify_recoverable_job(&run, &existing)?;
            if let Err(error) = runtime.prelaunch(&run, &existing, true, route_target).await {
                if runtime_integration::is_permanent_failure(error.as_ref()) {
                    reject_prelaunch(services, &run, &existing, true).await?;
                    create_lock.commit().await?;
                    return Ok(());
                }
                return Err(error);
            }
            existing
        } else {
            if run.job_uid.is_some() {
                reject_prelaunch(services, &run, &job, false).await?;
                create_lock.commit().await?;
                return Ok(());
            }
            if let Err(error) = runtime.prelaunch(&run, &job, false, route_target).await {
                if runtime_integration::is_permanent_failure(error.as_ref()) {
                    reject_prelaunch(services, &run, &job, false).await?;
                    create_lock.commit().await?;
                    return Ok(());
                }
                return Err(error);
            }
            provision_upload_secret(
                db,
                client,
                &config.namespace,
                &job,
                &run,
                objects,
                config.s3.ca_pem(),
            )
            .await?;
            if let Some(name) = job_secret(&job)? {
                provision_secret(db, identity, client, config, &run, &name).await?;
                project(db, identity, k8s, config).await?;
            }
            match k8s.create_job(job).await {
                Ok(job) => job,
                Err(thalamus_k8s::Error::Kube(kube::Error::Api(response)))
                    if response.code == 409 =>
                {
                    k8s.get_job(&run.job_name)
                        .await?
                        .ok_or("conflicting job vanished")?
                }
                Err(e) => return Err(e.into()),
            }
        };
        run = store
            .record_created_job(&run.team, ACTOR, run.id, &job)
            .await?;
        run = store
            .transition(&run.team, ACTOR, run.id, Transition::Started)
            .await?;
        create_lock.commit().await?;
    }
    if run.state == "running" {
        let recorded_failure = if run.request.kind == RunKind::Validation {
            match runtime.verified_failure(&run, route_target).await {
                Ok(recorded) => recorded,
                Err(error) if runtime_integration::is_permanent_failure(error.as_ref()) => false,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        if recorded_failure {
            // Failure evidence is already durable even if the finished Job/Pod
            // expired while the worker was down.
            let artifact = ingest_result(objects, artifacts, run.id).await?;
            let prepared = store
                .prepare_job(&run.team, ACTOR, run.id, &config.templates, None)
                .await?;
            cleanup_secret(db, identity, client, config, &run, &prepared).await?;
            store
                .transition(&run.team, ACTOR, run.id, failed_job_transition(artifact))
                .await?;
            return Ok(());
        }
        let Some(job) = k8s.get_job(&run.job_name).await? else {
            // A deleted or TTL-expired Job cannot be treated as a success.
            let prepared = store
                .prepare_job(&run.team, ACTOR, run.id, &config.templates, None)
                .await?;
            cleanup_secret(db, identity, client, config, &run, &prepared).await?;
            store
                .transition(
                    &run.team,
                    ACTOR,
                    run.id,
                    Transition::JobFailed {
                        error: "Kubernetes Job disappeared before completion was recorded".into(),
                    },
                )
                .await?;
            return Ok(());
        };
        thalamus_runs::verify_owned_job(&run, &job, &config.namespace)?;
        if let Some(success) = thalamus_worker::job_outcome(&job) {
            let transition = if success {
                Transition::JobSucceeded
            } else if run.request.kind == RunKind::Validation {
                let artifact = match runtime.failure(&run, &job, route_target).await {
                    Ok(()) => ingest_result(objects, artifacts, run.id).await?,
                    Err(error) if runtime_integration::is_permanent_failure(error.as_ref()) => None,
                    Err(error) => return Err(error),
                };
                failed_job_transition(artifact)
            } else {
                failed_job_transition(None)
            };
            cleanup_secret(db, identity, client, config, &run, &job).await?;
            run = store
                .transition(&run.team, ACTOR, run.id, transition)
                .await?;
        } else {
            provision_upload_secret(
                db,
                client,
                &config.namespace,
                &job,
                &run,
                objects,
                config.s3.ca_pem(),
            )
            .await?;
        }
    }
    if run.state == "ingesting" {
        let recorded = match runtime.verified_ingestion(&run, route_target).await {
            Ok(recorded) => recorded,
            Err(error) if runtime_integration::is_permanent_failure(error.as_ref()) => {
                fail_ingestion(store, &run, "runtime evidence could not be verified").await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if !recorded {
            let Some(job) = k8s.get_job(&run.job_name).await? else {
                fail_ingestion(
                    store,
                    &run,
                    "Kubernetes Job disappeared before evidence was recorded",
                )
                .await?;
                return Ok(());
            };
            thalamus_runs::verify_owned_job(&run, &job, &config.namespace)?;
            if let Err(error) = runtime.ingestion(&run, &job, route_target).await {
                if runtime_integration::is_permanent_failure(error.as_ref()) {
                    fail_ingestion(store, &run, "runtime evidence could not be verified").await?;
                    return Ok(());
                }
                return Err(error);
            }
        }
        let Some(artifact) = ingest_result(objects, artifacts, run.id).await? else {
            fail_ingestion(store, &run, "result object missing").await?;
            return Ok(());
        };
        store
            .transition(
                &run.team,
                ACTOR,
                run.id,
                Transition::IngestionSucceeded {
                    artifacts: vec![artifact],
                },
            )
            .await?;
    }
    Ok(())
}

fn usage_day(next: chrono::NaiveDate, today: chrono::NaiveDate) -> Option<chrono::NaiveDate> {
    (next < today).then_some(next)
}

fn usage_retry_start(today: chrono::NaiveDate, retention_days: u32) -> Option<chrono::NaiveDate> {
    // Do not retry a UTC day whose first samples may already have expired.
    let oldest =
        today.checked_sub_days(chrono::Days::new(u64::from(retention_days.checked_sub(1)?)))?;
    (oldest < today).then_some(oldest)
}

async fn reconcile_usage(
    db: &PgPool,
    store: &UsageStore,
    source: &PrometheusSource,
    config: &UsageConfig,
) -> Result<bool> {
    // Bind the checkpoint to the source configuration without persisting its URL.
    let identity = serde_json::to_vec(&(
        &config.url,
        &config.input,
        &config.output,
        config.step,
        config.start,
    ))?;
    let digest = Sha256::digest(identity).to_vec();
    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO thalamus_worker_usage_checkpoint(id,source_digest,next_day) VALUES (true,$1,$2) ON CONFLICT (id) DO NOTHING")
        .bind(&digest).bind(config.start).execute(&mut *tx).await?;
    let row = sqlx::query("SELECT source_digest,next_day FROM thalamus_worker_usage_checkpoint WHERE id=true FOR UPDATE").fetch_one(&mut *tx).await?;
    let stored: Vec<u8> = row.try_get("source_digest")?;
    if stored != digest {
        return Err("usage source configuration differs from checkpoint".into());
    }
    let next: chrono::NaiveDate = row.try_get("next_day")?;
    let today = Utc::now().date_naive();
    let Some(day) = usage_day(next, today) else {
        if let Some(oldest) = usage_retry_start(today, config.retention_days) {
            if let Some(day) = store
                .next_incomplete_day(oldest, today, Utc::now() - chrono::Duration::hours(1))
                .await?
            {
                store.ingest_day(source, day).await?;
            }
        }
        tx.commit().await?;
        return Ok(true);
    };
    // Empty or incomplete telemetry is recorded as unknown, never as zero.
    store.ingest_day(source, day).await?;
    let following = day.succ_opt().ok_or("usage date overflow")?;
    sqlx::query("UPDATE thalamus_worker_usage_checkpoint SET next_day=$1 WHERE id=true")
        .bind(following)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(usage_day(following, today).is_none())
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database)
        .await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS thalamus_worker_projection (namespace text NOT NULL,name text NOT NULL,generation bigint NOT NULL,digest text,resource_version text,observed_at timestamptz,error text,PRIMARY KEY(namespace,name))").execute(&db).await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS thalamus_worker_revocations (namespace text NOT NULL,name text NOT NULL,key_id uuid NOT NULL,removed_at timestamptz NOT NULL,verified_at timestamptz,PRIMARY KEY(namespace,name,key_id))").execute(&db).await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS thalamus_worker_usage_checkpoint (id boolean PRIMARY KEY CHECK (id),source_digest bytea NOT NULL,next_day date NOT NULL)").execute(&db).await?;
    let usage_source = PrometheusSource::new(
        &config.usage.url,
        &config.usage.input,
        &config.usage.output,
        config.usage.step,
    )?;
    let usage_store = UsageStore::new(db.clone());
    let identity = WorkerIdentity::new(db.clone());
    let idp_sync = IdentitySync::from_env()?;
    let client = thalamus_k8s::default_client().await?;
    let k8s = K8s::new(client.clone(), &config.namespace)?;
    let store = Store::new(db.clone());
    let worker_store = WorkerStore::new(db.clone());
    let debug = DebugReconciler::new(
        db.clone(),
        client.clone(),
        config.namespace.clone(),
        config.namespace.clone(),
        config.projection.clone(),
    )?;
    let policy_source = thalamus_access::PolicySource::new(
        required("THALAMUS_ACCESS_REPOSITORY")?,
        required("THALAMUS_ACCESS_CATALOG_PATH")?,
        required("THALAMUS_ACCESS_COMMIT")?,
    );
    let runtime = RuntimeIntegration::new(
        &config.runtime_config,
        client.clone(),
        db.clone(),
        policy_source,
    )?;
    if runtime.config().namespace != config.namespace {
        return Err("runtime and worker namespaces differ".into());
    }
    let objects = config.s3.store()?;
    let artifacts = Artifacts::new(objects.clone());
    let workflows = WorkflowController::from_env(
        db.clone(),
        client.clone(),
        artifacts.clone(),
        &config.runtime_config,
        &config.runner_targets,
        &config.templates,
        &config.test_profiles,
    )?;
    let health = Arc::new(Mutex::new(Health::default()));
    for (label, kind, version) in [
        (
            "flux",
            ReadKind::GitRepository,
            runtime.config().source_version.clone(),
        ),
        (
            "helm",
            ReadKind::HelmRelease,
            runtime.config().helm_version.clone(),
        ),
        (
            "serving",
            ReadKind::LlmInferenceService,
            runtime.config().serving_version.clone(),
        ),
        (
            "pool",
            ReadKind::InferencePool,
            runtime.config().pool_version.clone(),
        ),
    ] {
        tokio::spawn(watch(k8s.clone(), health.clone(), label, kind, version));
    }
    let listener = tokio::net::TcpListener::bind(&config.health).await?;
    let server = tokio::spawn(health_server(listener, health.clone()));
    let run_services = RunServices {
        store: &store,
        k8s: &k8s,
        client: &client,
        db: &db,
        identity: &identity,
        config: &config,
        objects: objects.as_ref(),
        artifacts: &artifacts,
        runtime: &runtime,
    };
    loop {
        let cycle = async {
            k8s.acquire_lease(&config.lease, &config.holder, 30, Utc::now())
                .await?;
            if let Some(sync) = &idp_sync {
                if let Err(error) = sync.reconcile(&identity).await {
                    // A configured but unavailable collector must also remove
                    // stale managed-issuer keys from the gateway projection.
                    if project(&db, &identity, &k8s, &config).await.is_err() {
                        record_projection_error(&db, &config).await;
                    }
                    return Err(error);
                }
            }
            if let Err(e) = project(&db, &identity, &k8s, &config).await {
                record_projection_error(&db, &config).await;
                return Err(e);
            }
            let mut cursor: Option<RunCursor> = None;
            let mut failed = sync_caps(&db, &identity, &config.catalog).await.is_err();
            loop {
                let runs = worker_store.list_actionable(cursor, 100).await?;
                if runs.is_empty() {
                    break;
                }
                let count = runs.len();
                cursor = runs.last().map(RunCursor::from);
                for run in runs {
                    if run_one(run, &run_services).await.is_err() {
                        failed = true;
                    }
                }
                if count < 100 {
                    break;
                }
            }
            if gc_secrets(&db, &identity, &client, &config.namespace)
                .await
                .is_err()
            {
                failed = true;
            }
            if debug.reconcile().await.is_err() {
                failed = true;
            }
            if let Some(workflows) = &workflows {
                if workflows.reconcile().await.is_err() {
                    failed = true;
                }
            }
            let usage_ready = matches!(
                reconcile_usage(&db, &usage_store, &usage_source, &config.usage).await,
                Ok(true)
            );
            health.lock().unwrap().usage_ready = usage_ready;
            if !usage_ready {
                failed = true;
            }
            if failed {
                return Err("one or more runs failed to reconcile".into());
            }
            Ok::<(), Box<dyn Error + Send + Sync>>(())
        }
        .await;
        {
            let mut h = health.lock().unwrap();
            match cycle {
                Ok(()) => {
                    h.last_success = Some(Instant::now());
                    h.error = None;
                }
                Err(_) => {
                    h.error = Some("reconciliation failed".into());
                }
            }
        }
        if server.is_finished() {
            return Err("health server stopped".into());
        }
        tokio::select! { _ = tokio::time::sleep(Duration::from_secs(10)) => {}, _ = tokio::signal::ctrl_c() => { let _ = k8s.release_lease(&config.lease, &config.holder).await; return Ok(()); } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generic_worker_rejects_debug_without_launch_authority() {
        assert!(require_generic_runner(&RunKind::Debug).is_err());
        assert!(require_generic_runner(&RunKind::Validation).is_ok());
        assert!(require_generic_runner(&RunKind::Benchmark).is_ok());
    }

    #[test]
    fn usage_recovery_processes_only_closed_utc_days_in_order() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 28).unwrap();
        let previous = today.pred_opt().unwrap();
        let older = previous.pred_opt().unwrap();
        assert_eq!(usage_day(older, today), Some(older));
        assert_eq!(usage_day(previous, today), Some(previous));
        assert_eq!(usage_day(today, today), None);
        assert_eq!(usage_day(today.succ_opt().unwrap(), today), None);
        assert_eq!(usage_retry_start(today, 1), None);
        assert_eq!(usage_retry_start(today, 2), Some(previous));
        assert_eq!(usage_retry_start(today, 3), Some(older));
    }
    #[test]
    fn caps_input_rejects_unrecognized_fields() {
        assert!(serde_json::from_str::<CapFile>(r#"{"teams":[],"unexpected":1}"#).is_err());
        let parsed: CapFile = serde_json::from_str(r#"{"teams":[{"team_id":"00000000-0000-0000-0000-000000000001","max_tokens":100,"unit":"tokens","window":"day"}]}"#).unwrap();
        assert_eq!(parsed.teams[0].max_tokens, 100);
        assert_eq!(parsed.teams[0].allowed_classes, vec!["standard"]);
    }

    #[test]
    fn budget_caps_follow_the_configured_catalog_directory() {
        assert_eq!(
            caps_object("abc", "catalog/").unwrap(),
            "abc:catalog/budget-caps.json"
        );
        assert_eq!(
            caps_object("abc", "nested/catalog/").unwrap(),
            "abc:nested/catalog/budget-caps.json"
        );
        assert_eq!(caps_object("abc", "").unwrap(), "abc:budget-caps.json");
        assert!(caps_object("abc", "../").is_err());
        assert!(caps_object("abc", "catalog//").is_err());
    }
    #[test]
    fn job_secret_requires_reference_and_reserved_name() {
        let mut job: Job = serde_json::from_value(serde_json::json!({"spec":{"template":{"spec":{"containers":[{"name":"runner","env":[{"name":"THALAMUS_API_KEY","valueFrom":{"secretKeyRef":{"name":"thalamus-key-123","key":"key"}}}]}],"restartPolicy":"Never"}}}})).unwrap();
        assert_eq!(
            job_secret(&job).unwrap().as_deref(),
            Some("thalamus-key-123")
        );
        let env = &mut job
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .containers[0]
            .env
            .as_mut()
            .unwrap()[0];
        env.value_from
            .as_mut()
            .unwrap()
            .secret_key_ref
            .as_mut()
            .unwrap()
            .name = "foreign".into();
        assert!(job_secret(&job).is_err());
    }

    #[test]
    fn upload_projection_requires_shared_optional_ca() {
        let id = Uuid::new_v4();
        let name = format!("thalamus-upload-{id}");
        let mut job: Job = serde_json::from_value(serde_json::json!({
            "spec":{"template":{"spec":{"restartPolicy":"Never","containers":[{"name":"runner"}],
                "volumes":[{"name":"result-upload","projected":{"sources":[
                    {"secret":{"name":name,"items":[{"key":"url","path":"url"}]}},
                    {"secret":{"name":name,"optional":true,"items":[{"key":"ca.crt","path":"ca.pem"}]}}
                ]}}]}}}
        })).unwrap();
        assert_eq!(upload_secret_name(&job, id).unwrap(), name);
        let ca = &mut job
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .volumes
            .as_mut()
            .unwrap()[0]
            .projected
            .as_mut()
            .unwrap()
            .sources
            .as_mut()
            .unwrap()[1]
            .secret
            .as_mut()
            .unwrap();
        ca.optional = Some(false);
        assert!(upload_secret_name(&job, id).is_err());
    }
    #[tokio::test]
    async fn failed_job_retains_uploaded_summary_even_if_it_claims_success() {
        let objects = Arc::new(object_store::memory::InMemory::new());
        let artifacts = Artifacts::new(objects.clone());
        let id = Uuid::new_v4();
        assert!(matches!(
            failed_job_transition(
                ingest_result(objects.as_ref(), &artifacts, id)
                    .await
                    .unwrap()
            ),
            Transition::JobFailed { .. }
        ));
        let path = ObjectPath::from(format!("runs/{id}/result/output.json"));
        objects
            .put(
                &path,
                bytes::Bytes::from_static(br#"{"passed":true}"#).into(),
            )
            .await
            .unwrap();
        let artifact = ingest_result(objects.as_ref(), &artifacts, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            artifacts.read(id, &artifact).await.unwrap(),
            bytes::Bytes::from_static(br#"{"passed":true}"#)
        );
        assert!(matches!(
            failed_job_transition(Some(artifact)),
            Transition::JobFailedWithArtifacts { .. }
        ));
        objects
            .put(&path, bytes::Bytes::from_static(b"invalid").into())
            .await
            .unwrap();
        assert!(ingest_result(objects.as_ref(), &artifacts, id)
            .await
            .is_err());
    }
}
