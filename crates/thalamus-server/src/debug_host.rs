//! Server-owned integration for the restricted OpenCode proxy.
//!
//! The host never accepts a target or a credential from the browser. It reloads
//! the session, its current owner/team and both committed service grants, the
//! session-bound model key, and the actual workload resources on authorization.
//! Token consumption is enforced independently by the model gateway; a valid
//! key and configured budget are not evidence of remaining model tokens.

use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Utc};
use k8s_openapi::{
    api::{
        batch::v1::Job,
        core::v1::{Secret, Service},
    },
    apimachinery::pkg::{apis::meta::v1::ObjectMeta, util::intstr::IntOrString},
};
use kube::{Api, Client};
use serde_json::{json, Value};
use sqlx::PgPool;
use thalamus_access::PolicySource;
use thalamus_debug::{
    jobs::{debug_job_name, debug_server_secret_name},
    proxy::{
        Actor, AuthorizedSession, DebugProxy, HostError, Operation, ProxyError, ProxyHost,
        ProxyLimits, ServiceCidr, StoredEvent,
    },
    store::{
        DebugIdentity, DebugSessionAccess, DebugStore, NewDebugEvent, PromptClaim, PromptKind,
        SessionState, StoreError, WorkerSessionCursor, WorkerStore,
    },
};
use thalamus_identity::{KeyMetadata, WorkerIdentity};
use tokio::task::JoinHandle;
use uuid::Uuid;

const SESSION_LABEL: &str = "thalamus.dev/debug-session";
const OWNERSHIP_ANNOTATION: &str = "thalamus.dev/ownership-nonce";
const JOB_UID_ANNOTATION: &str = "thalamus.dev/job-uid";
const EXPIRY_ANNOTATION: &str = "thalamus.dev/credentials-expire-at";
const SWEEP_INTERVAL: Duration = Duration::from_secs(2);

/// The concrete proxy type stored by the server and used by the reply route.
pub type ServerDebugProxy = DebugProxy<ServerDebugHost>;

#[derive(Debug, thiserror::Error)]
pub enum DebugHostConfigError {
    #[error("invalid debug workload namespace")]
    InvalidNamespace,
    #[error(transparent)]
    Proxy(#[from] ProxyError),
}

/// Construct from operator configuration, never from a debug request. The
/// configured CIDRs must be the cluster's actual Kubernetes Service networks.
pub fn new_debug_proxy(
    db: PgPool,
    policy_source: Arc<PolicySource>,
    kube: Client,
    namespace: String,
    service_cidrs: Vec<String>,
) -> Result<Arc<ServerDebugProxy>, DebugHostConfigError> {
    if !valid_namespace(&namespace) {
        return Err(DebugHostConfigError::InvalidNamespace);
    }
    let service_cidrs = service_cidrs
        .iter()
        .map(|cidr| cidr.parse::<ServiceCidr>())
        .collect::<Result<Vec<_>, _>>()?;
    let host = Arc::new(ServerDebugHost {
        store: DebugStore::new(db.clone()),
        identity: WorkerIdentity::new(db),
        policy_source,
        kube,
        namespace,
    });
    // Both persisted seed objects may be up to 128 KiB. Do not truncate them.
    let limits = ProxyLimits {
        max_prompt_bytes: 1024 * 1024,
        ..ProxyLimits::default()
    };
    Ok(Arc::new(DebugProxy::new(host, limits, service_cidrs)?))
}

/// Holds only trusted server configuration and database/Kubernetes clients.
/// Instances are created by [`new_debug_proxy`].
pub struct ServerDebugHost {
    store: DebugStore,
    identity: WorkerIdentity,
    policy_source: Arc<PolicySource>,
    kube: Client,
    namespace: String,
}

fn valid_namespace(value: &str) -> bool {
    let bytes = value.as_bytes();
    let dns_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 63
        && dns_char(bytes[0])
        && dns_char(bytes[bytes.len() - 1])
        && bytes.iter().all(|&b| dns_char(b) || b == b'-')
}

fn identity(actor: &Actor) -> DebugIdentity {
    DebugIdentity {
        user_id: actor.user_id,
        team_id: actor.team_id,
    }
}

fn store_error(error: StoreError) -> HostError {
    match error {
        StoreError::Invalid(_) => HostError::InvalidInput,
        StoreError::Forbidden | StoreError::InvalidCredential => HostError::Unauthorized,
        StoreError::NotFound => HostError::NotFound,
        StoreError::BudgetExhausted => HostError::BudgetExhausted,
        StoreError::InitializationBusy => HostError::InitializationBusy,
        StoreError::IdentityProjectionStale
        | StoreError::Database(_)
        | StoreError::SequenceExhausted
        | StoreError::RandomUnavailable
        | StoreError::CorruptData => HostError::Unavailable,
        _ => HostError::Conflict,
    }
}

fn annotation<'a>(metadata: &'a ObjectMeta, name: &str) -> Option<&'a str> {
    metadata.annotations.as_ref()?.get(name).map(String::as_str)
}

fn uuid_matches(value: Option<&str>, id: Uuid) -> bool {
    let mut buffer = Uuid::encode_buffer();
    let expected: &str = id.hyphenated().encode_lower(&mut buffer);
    value == Some(expected)
}

fn owned_metadata(
    metadata: &ObjectMeta,
    namespace: &str,
    name: &str,
    session: Uuid,
    nonce: Uuid,
) -> bool {
    let Some(labels) = metadata.labels.as_ref() else {
        return false;
    };
    metadata.name.as_deref() == Some(name)
        && metadata.namespace.as_deref() == Some(namespace)
        && metadata.uid.as_deref().is_some_and(|s| !s.is_empty())
        && metadata
            .resource_version
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        && metadata.deletion_timestamp.is_none()
        && labels.get("app.kubernetes.io/name").map(String::as_str) == Some("thalamus-debug")
        && labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str)
            == Some("thalamus")
        && !nonce.is_nil()
        && uuid_matches(annotation(metadata, OWNERSHIP_ANNOTATION), nonce)
        && uuid_matches(labels.get(SESSION_LABEL).map(String::as_str), session)
}

fn expires_at(metadata: &ObjectMeta) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(annotation(metadata, EXPIRY_ANNOTATION)?)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

fn job_server_secret(job: &Job, name: &str, session: Uuid) -> bool {
    let Some(spec) = job.spec.as_ref() else {
        return false;
    };
    let Some(uid) = job.metadata.uid.as_deref() else {
        return false;
    };
    let selector = spec.selector.as_ref().is_some_and(|selector| {
        selector
            .match_expressions
            .as_ref()
            .is_none_or(Vec::is_empty)
            && selector.match_labels.as_ref().is_some_and(|labels| {
                labels.len() == 1
                    && (labels
                        .get("batch.kubernetes.io/controller-uid")
                        .map(String::as_str)
                        == Some(uid)
                        || labels.get("controller-uid").map(String::as_str) == Some(uid))
            })
    });
    let labels = spec
        .template
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.labels.as_ref());
    if spec.suspend.unwrap_or(false)
        || !selector
        || !uuid_matches(
            labels
                .and_then(|labels| labels.get(SESSION_LABEL))
                .map(String::as_str),
            session,
        )
        || spec.parallelism != Some(1)
        || spec.completions != Some(1)
        || spec.backoff_limit != Some(0)
    {
        return false;
    }
    let Some(pod) = spec.template.spec.as_ref() else {
        return false;
    };
    if pod.containers.len() != 1
        || pod.automount_service_account_token != Some(false)
        || pod.host_network.unwrap_or(false)
        || pod.host_pid.unwrap_or(false)
        || pod.host_ipc.unwrap_or(false)
    {
        return false;
    }
    let container = &pod.containers[0];
    if container.name != "opencode" {
        return false;
    }
    let Some(env) = container.env.as_ref() else {
        return false;
    };
    let mut selected = env.iter().filter(|v| v.name == "OPENCODE_SERVER_PASSWORD");
    let Some(variable) = selected.next() else {
        return false;
    };
    if selected.next().is_some() || variable.value.is_some() {
        return false;
    }
    variable
        .value_from
        .as_ref()
        .and_then(|source| source.secret_key_ref.as_ref())
        .is_some_and(|secret| {
            secret.name == name && secret.key == "password" && !secret.optional.unwrap_or(false)
        })
}

fn job_is_live(job: &Job) -> bool {
    !job.status.as_ref().is_some_and(|status| {
        status.completion_time.is_some()
            || status.conditions.as_ref().is_some_and(|conditions| {
                conditions.iter().any(|c| {
                    c.status == "True"
                        && matches!(c.type_.as_str(), "Complete" | "Failed" | "FailureTarget")
                })
            })
    })
}

fn service_is_owned(
    service: &Service,
    namespace: &str,
    name: &str,
    session: Uuid,
    nonce: Uuid,
    job_uid: &str,
) -> bool {
    if !owned_metadata(&service.metadata, namespace, name, session, nonce) {
        return false;
    }
    let owned = service
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|refs| {
            refs.len() == 1
                && refs[0].api_version == "batch/v1"
                && refs[0].kind == "Job"
                && refs[0].name == name
                && refs[0].uid == job_uid
        });
    let Some(spec) = service.spec.as_ref() else {
        return false;
    };
    let selector = spec.selector.as_ref().is_some_and(|selector| {
        selector.len() == 1
            && uuid_matches(selector.get(SESSION_LABEL).map(String::as_str), session)
    });
    let cluster_ip = spec
        .cluster_ip
        .as_deref()
        .and_then(|ip| ip.parse::<IpAddr>().ok());
    let addresses = spec.cluster_ips.as_ref().is_none_or(|ips| {
        !ips.is_empty()
            && ips.len() <= 2
            && ips.iter().all(|ip| ip.parse::<IpAddr>().is_ok())
            && ips.first().map(String::as_str) == spec.cluster_ip.as_deref()
    });
    let ports = spec.ports.as_ref().is_some_and(|ports| {
        ports.len() == 1
            && ports[0].name.as_deref() == Some("http")
            && ports[0].port == 4096
            && ports[0].protocol.as_deref().unwrap_or("TCP") == "TCP"
            && ports[0].target_port == Some(IntOrString::Int(4096))
            && ports[0].node_port.is_none()
    });
    owned
        && selector
        && cluster_ip.is_some()
        && addresses
        && ports
        && spec.type_.as_deref().unwrap_or("ClusterIP") == "ClusterIP"
        && spec.external_name.is_none()
        && spec.external_ips.as_ref().is_none_or(Vec::is_empty)
        && !spec.publish_not_ready_addresses.unwrap_or(false)
}

impl ServerDebugHost {
    async fn active_session(
        &self,
        actor: &Actor,
        id: Uuid,
    ) -> Result<(DebugSessionAccess, KeyMetadata), HostError> {
        if actor.user_id.is_nil() || actor.team_id.is_nil() || id.is_nil() {
            return Err(HostError::NotFound);
        }
        let who = identity(actor);
        let session = self.store.get_access(&who, id).await.map_err(store_error)?;
        if session.state != SessionState::Running
            || session.deadline <= Utc::now()
            || session.ownership_nonce.is_nil()
            || session.job_namespace != self.namespace
            || session.job_name != debug_job_name(id)
        {
            return Err(HostError::Conflict);
        }
        if session.tool_budget <= 0 || session.tool_calls_used >= session.tool_budget {
            return Err(HostError::BudgetExhausted);
        }
        let policy = self
            .policy_source
            .load()
            .map_err(|_| HostError::Unavailable)?;
        for name in std::iter::once(&session.service_id)
            .chain((session.model != session.service_id).then_some(&session.model))
        {
            let service = policy
                .service_id(name)
                .map_err(|_| HostError::Unavailable)?
                .ok_or(HostError::Unauthorized)?;
            if service.namespace != self.namespace
                || !policy
                    .team_allowed(&service, actor.team_id)
                    .map_err(|_| HostError::Unavailable)?
            {
                return Err(HostError::Unauthorized);
            }
        }
        let key = self
            .identity
            .active_debug_key(id)
            .await
            .map_err(|_| HostError::Unavailable)?
            .ok_or(HostError::Unauthorized)?;
        if key.id.is_nil()
            || key.user_id != actor.user_id
            || key.creator_id != actor.user_id
            || key.team_id != actor.team_id
            || key.project_id != Some(session.project_id)
            || key.kind != "debug"
            || key.allowed_services.len() != 1
            || key.allowed_services.first() != Some(&session.model)
            || key.budget != Some(session.model_budget)
            || key.revoked_at.is_some()
            || key
                .expires_at
                .is_none_or(|expiry| expiry > session.deadline || expiry <= Utc::now())
        {
            return Err(HostError::Unauthorized);
        }
        // Recheck membership and state after external policy/key reads.
        let current = self.store.get_access(&who, id).await.map_err(store_error)?;
        if current.state != SessionState::Running || current.deadline <= Utc::now() {
            return Err(HostError::Conflict);
        }
        if current.tool_calls_used >= current.tool_budget {
            return Err(HostError::BudgetExhausted);
        }
        Ok((current, key))
    }

    async fn password(
        &self,
        session: &DebugSessionAccess,
        key: &KeyMetadata,
        operation: Operation,
    ) -> Result<String, HostError> {
        let id = session.id;
        let job_name = debug_job_name(id);
        let secret_name = debug_server_secret_name(id);
        let jobs: Api<Job> = Api::namespaced(self.kube.clone(), &self.namespace);
        let services: Api<Service> = Api::namespaced(self.kube.clone(), &self.namespace);
        let secrets: Api<Secret> = Api::namespaced(self.kube.clone(), &self.namespace);
        let job = jobs
            .get_opt(&job_name)
            .await
            .map_err(|_| HostError::Unavailable)?
            .ok_or(HostError::NotFound)?;
        if !owned_metadata(
            &job.metadata,
            &self.namespace,
            &job_name,
            id,
            session.ownership_nonce,
        ) || job
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|refs| !refs.is_empty())
            || annotation(&job.metadata, "thalamus.dev/launch-state") != Some("active")
            || !uuid_matches(
                annotation(&job.metadata, "thalamus.dev/model-credential"),
                key.id,
            )
            || annotation(&job.metadata, "thalamus.dev/model-token-budget")
                .and_then(|value| value.parse::<i64>().ok())
                != Some(session.model_budget)
            || annotation(&job.metadata, "thalamus.dev/tool-call-budget")
                .and_then(|value| value.parse::<i64>().ok())
                != Some(session.tool_budget)
            || expires_at(&job.metadata) != Some(session.deadline)
            || !job_server_secret(&job, &secret_name, id)
            || !job_is_live(&job)
        {
            return Err(HostError::Conflict);
        }
        // The Job may have been resumed while checkout or OpenCode is still
        // starting. Do not reserve an irreversible create until a pod is ready.
        if operation == Operation::CreateSession
            && job
                .status
                .as_ref()
                .and_then(|status| status.ready)
                .unwrap_or(0)
                < 1
        {
            return Err(HostError::Unavailable);
        }
        let job_uid = job.metadata.uid.as_deref().ok_or(HostError::Conflict)?;
        if session.job_uid.as_deref() != Some(job_uid) {
            return Err(HostError::Conflict);
        }
        let service = services
            .get_opt(&job_name)
            .await
            .map_err(|_| HostError::Unavailable)?
            .ok_or(HostError::NotFound)?;
        if !service_is_owned(
            &service,
            &self.namespace,
            &job_name,
            id,
            session.ownership_nonce,
            job_uid,
        ) {
            return Err(HostError::Conflict);
        }
        // Per-session RBAC and Secret creation can propagate after the Job is
        // resumed. A missing or temporarily inaccessible Secret is retryable.
        let secret = secrets
            .get_opt(&secret_name)
            .await
            .map_err(|_| HostError::Unavailable)?
            .ok_or(HostError::Unavailable)?;
        if !owned_metadata(
            &secret.metadata,
            &self.namespace,
            &secret_name,
            id,
            session.ownership_nonce,
        ) || annotation(&secret.metadata, JOB_UID_ANNOTATION) != Some(job_uid)
            || expires_at(&secret.metadata) != Some(session.deadline)
            || secret
                .metadata
                .owner_references
                .as_ref()
                .is_some_and(|refs| !refs.is_empty())
            || secret.immutable != Some(true)
            || secret.type_.as_deref() != Some("Opaque")
        {
            return Err(HostError::Conflict);
        }
        let mut data = secret.data.ok_or(HostError::Conflict)?;
        if data.len() != 1 {
            return Err(HostError::Conflict);
        }
        let bytes = data.remove("password").ok_or(HostError::Conflict)?.0;
        if bytes.len() != 64
            || !bytes
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            return Err(HostError::Conflict);
        }
        String::from_utf8(bytes).map_err(|_| HostError::Conflict)
    }
}

fn basic_auth_token(password: &str) -> Result<[u8; 100], HostError> {
    if password.len() != 64 {
        return Err(HostError::Conflict);
    }
    let mut input = [0; 73];
    input[..9].copy_from_slice(b"opencode:");
    input[9..].copy_from_slice(password.as_bytes());
    let mut encoded = [0; 100];
    if STANDARD
        .encode_slice(input, &mut encoded)
        .map_err(|_| HostError::Unavailable)?
        != encoded.len()
    {
        return Err(HostError::Unavailable);
    }
    Ok(encoded)
}

fn contains_password(value: &Value, password: &str, basic: &str) -> bool {
    let sensitive = |text: &str| text.contains(password) || text.contains(basic);
    match value {
        Value::String(text) => sensitive(text),
        Value::Array(values) => values.iter().any(|v| contains_password(v, password, basic)),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| sensitive(key) || contains_password(value, password, basic)),
        _ => false,
    }
}

#[async_trait]
impl ProxyHost for ServerDebugHost {
    async fn authorize(
        &self,
        actor: &Actor,
        id: Uuid,
        operation: Operation,
    ) -> Result<AuthorizedSession, HostError> {
        let (session, key) = self.active_session(actor, id).await?;
        let password = self.password(&session, &key, operation).await?;
        let (current, current_key) = self.active_session(actor, id).await?;
        if current_key.id != key.id
            || current.deadline != session.deadline
            || current.ownership_nonce != session.ownership_nonce
            || current.job_uid != session.job_uid
            || current.upstream_session_id != session.upstream_session_id
        {
            return Err(HostError::Conflict);
        }
        Ok(AuthorizedSession {
            namespace: self.namespace.clone(),
            password,
            upstream_session_id: current.upstream_session_id,
            deadline: current
                .deadline
                .min(current_key.expires_at.ok_or(HostError::Unauthorized)?),
            budget_available: true,
        })
    }

    async fn reserve_initialization(
        &self,
        actor: &Actor,
        id: Uuid,
        claim: Uuid,
    ) -> Result<(), HostError> {
        self.active_session(actor, id).await?;
        self.store
            .reserve_initialization(&identity(actor), id, claim)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn bind_upstream_session(
        &self,
        actor: &Actor,
        id: Uuid,
        claim: Uuid,
        upstream_id: &str,
    ) -> Result<(), HostError> {
        self.store
            .bind_upstream_session_after_dispatch(&identity(actor), id, claim, upstream_id)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn release_initialization(
        &self,
        actor: &Actor,
        id: Uuid,
        claim: Uuid,
    ) -> Result<(), HostError> {
        self.store
            .release_initialization(&identity(actor), id, claim)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn load_seed(&self, actor: &Actor, id: Uuid) -> Result<String, HostError> {
        let (checked, _) = self.active_session(actor, id).await?;
        // Context and evidence are loaded only for the one-time seed, not for
        // every event authorization.
        let session = self
            .store
            .get(&identity(actor), id)
            .await
            .map_err(store_error)?;
        if session.state != SessionState::Running
            || session.deadline <= Utc::now()
            || session.ownership_nonce != checked.ownership_nonce
            || session.job_uid != checked.job_uid
        {
            return Err(HostError::Conflict);
        }
        if session.tool_calls_used >= session.tool_budget {
            return Err(HostError::BudgetExhausted);
        }
        Ok(format!(
            "Investigate this service and report your findings using the available authorized tools.\n\nService context (JSON):\n{}\n\nFailing evidence (JSON):\n{}",
            session.service_context, session.failing_evidence
        ))
    }

    async fn claim_prompt(
        &self,
        actor: &Actor,
        id: Uuid,
        kind: PromptKind,
        key: &str,
        text: &str,
        claim: Uuid,
    ) -> Result<PromptClaim, HostError> {
        let operation = match kind {
            PromptKind::Seed => Operation::Seed,
            PromptKind::Reply => Operation::Reply,
        };
        let grant = self.authorize(actor, id, operation).await?;
        let basic = basic_auth_token(&grant.password)?;
        let basic = std::str::from_utf8(&basic).map_err(|_| HostError::Unavailable)?;
        if text.contains(grant.password.as_str()) || text.contains(basic) {
            return Err(HostError::InvalidInput);
        }
        self.store
            .claim_prompt(&identity(actor), id, kind, key, text, claim)
            .await
            .map_err(store_error)
    }

    async fn accept_prompt(
        &self,
        actor: &Actor,
        id: Uuid,
        kind: PromptKind,
        key: &str,
        claim: Uuid,
    ) -> Result<(), HostError> {
        self.store
            .accept_prompt(&identity(actor), id, kind, key, claim)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn release_prompt(
        &self,
        actor: &Actor,
        id: Uuid,
        kind: PromptKind,
        key: &str,
        text: &str,
        claim: Uuid,
    ) -> Result<(), HostError> {
        self.store
            .release_prompt(&identity(actor), id, kind, key, text, claim)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn append_collector_event(
        &self,
        actor: &Actor,
        id: Uuid,
        generation: i64,
        event: StoredEvent,
    ) -> Result<(), HostError> {
        let grant = self.authorize(actor, id, Operation::Stream).await?;
        let basic = basic_auth_token(&grant.password)?;
        let basic = std::str::from_utf8(&basic).map_err(|_| HostError::Unavailable)?;
        if contains_password(&event.payload, &grant.password, basic) {
            return Err(HostError::Unauthorized);
        }
        self.store
            .append_collector_event(
                &identity(actor),
                id,
                generation,
                NewDebugEvent {
                    client_request_key: event.key.to_string(),
                    kind: event.kind.to_owned(),
                    payload: event.payload,
                },
            )
            .await
            .map_err(|error| match error {
                StoreError::Invalid("credentials must not be persisted") => HostError::Unauthorized,
                StoreError::Invalid(_) => HostError::Conflict,
                other => store_error(other),
            })?;
        Ok(())
    }

    async fn record_collector_gap(
        &self,
        actor: &Actor,
        id: Uuid,
        generation: i64,
    ) -> Result<(), HostError> {
        // This fixed, generation-fenced marker must survive termination and
        // owner offboarding. It does not expose transcript data or credentials.
        self.store
            .record_collector_gap(&identity(actor), id, generation)
            .await
            .map_err(store_error)
    }
}

// Dropping the supervisor also aborts its child collectors. Normal completion,
// offboarding and deadline handling remain in the proxy/store, which record a
// gap before disconnect and fence any stale writer after leadership changes.
struct CollectorTask(JoinHandle<()>);
impl Drop for CollectorTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Start the browser-independent collector supervisor. Keep the returned handle
/// for the lifetime of the server; abort it during shutdown. Each collector's
/// PostgreSQL session lock elects one leader across all server replicas.
pub fn spawn_debug_supervisor(proxy: Arc<ServerDebugProxy>, db: PgPool) -> JoinHandle<()> {
    tokio::spawn(async move {
        let worker = WorkerStore::new(db.clone());
        let store = DebugStore::new(db);
        let mut tasks: HashMap<Uuid, CollectorTask> = HashMap::new();
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            tasks.retain(|_, task| !task.0.is_finished());
            // Recovery includes terminal and offboarded sessions. A process can
            // crash while the feed is open and the session can expire before a
            // server restarts. Only the advisory-lock owner may mark that gap;
            // an active collector on any replica retains its own lock.
            let mut recovery_cursor = None;
            loop {
                let page = match store.list_collector_recovery(recovery_cursor, 100).await {
                    Ok(page) => page,
                    Err(_) => break,
                };
                let count = page.len();
                recovery_cursor = page.last().map(WorkerSessionCursor::from);
                for session in page {
                    if tasks.contains_key(&session.id) {
                        continue;
                    }
                    let lease = tokio::time::timeout(
                        Duration::from_secs(10),
                        store.try_collector_leadership(session.id),
                    )
                    .await;
                    if let Ok(Ok(Some(mut lease))) = lease {
                        let who = DebugIdentity {
                            user_id: session.user_id,
                            team_id: session.team_id,
                        };
                        let _ = tokio::time::timeout(
                            Duration::from_secs(10),
                            lease.record_abandoned_attempt(&who),
                        )
                        .await;
                        let _ =
                            tokio::time::timeout(Duration::from_secs(10), lease.release()).await;
                    }
                }
                if count < 100 {
                    break;
                }
            }
            let mut cursor = None;
            loop {
                let page = match worker.list_actionable(cursor, 100).await {
                    Ok(page) => page,
                    Err(_) => break,
                };
                let count = page.len();
                cursor = page.last().map(WorkerSessionCursor::from);
                for session in page {
                    if session.state != SessionState::Running
                        || session.deadline <= Utc::now()
                        || tasks.contains_key(&session.id)
                    {
                        continue;
                    }
                    let id = session.id;
                    let actor = Actor {
                        user_id: session.user_id,
                        team_id: session.team_id,
                    };
                    let proxy = Arc::clone(&proxy);
                    let store = store.clone();
                    let task = tokio::spawn(async move {
                        let result = proxy.run_collector(&actor, id, &store).await;
                        let reason = match result {
                            Err(ProxyError::InitializationIndeterminate { .. })
                            | Err(ProxyError::Host(HostError::InitializationBusy)) => {
                                Some("upstream_initialization_indeterminate")
                            }
                            Err(ProxyError::PromptIndeterminate) => {
                                Some("seed_outcome_indeterminate")
                            }
                            Err(ProxyError::InvalidMessage)
                            | Err(ProxyError::Host(HostError::InvalidInput)) => {
                                Some("seed_invalid")
                            }
                            Err(ProxyError::BudgetExhausted)
                            | Err(ProxyError::Host(HostError::BudgetExhausted)) => {
                                Some("budget_exhausted")
                            }
                            Err(ProxyError::Host(HostError::Unauthorized)) => {
                                Some("authorization_revoked")
                            }
                            _ => None,
                        };
                        if reason == Some("upstream_initialization_indeterminate") {
                            // The request may have created an upstream session
                            // before any subscription existed. This fixed marker
                            // is also recoverable after a crash or offboarding.
                            let _ = tokio::time::timeout(
                                Duration::from_secs(10),
                                store.record_initialization_gap(&identity(&actor), id),
                            )
                            .await;
                        }
                        if let Some(reason) = reason {
                            // An ambiguous upstream mutation must never be resent.
                            // Terminal transition also schedules credential cleanup.
                            let _ = store
                                .transition(
                                    &identity(&actor),
                                    id,
                                    SessionState::Running,
                                    SessionState::Failed,
                                    Some(json!({"reason": reason})),
                                )
                                .await;
                        }
                    });
                    tasks.insert(id, CollectorTask(task));
                }
                if count < 100 {
                    break;
                }
            }
        }
    })
}
