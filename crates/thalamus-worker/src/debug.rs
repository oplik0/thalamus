//! Worker-owned debug workloads and their short-lived credentials.
//!
//! The installation contract is deliberately explicit: trusted administrators
//! install and protect the admission/RBAC rules, the reviewed gateway policy,
//! and a CNI that enforces NetworkPolicy before pods start, including Service
//! DNAT. Kubernetes does not expose an API that can attest CNI enforcement.
//! Live API checks below supplement that contract; they do not replace it.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use k8s_openapi::{
    api::{
        batch::v1::Job,
        core::v1::{ConfigMap, Endpoints, Namespace, Pod, Secret, Service, ServiceAccount},
        networking::v1::NetworkPolicy,
        rbac::v1::{Role, RoleBinding},
    },
    apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta},
    ByteString,
};
use kube::{
    api::{
        DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions, PropagationPolicy,
    },
    core::{ApiResource, DynamicObject, GroupVersionKind},
    Api, Client,
};
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thalamus_debug::{
    jobs::{
        create_debug_job, debug_job_name, debug_mcp_secret_name, debug_model_secret_name,
        debug_server_secret_name, DebugAdmission, DebugJobConfig, DebugJobError, DebugJobRequest,
        DebugLaunchAuthority, TrustedEndpoint,
    },
    store::{
        DebugIdentity, DebugSession, DebugStore, SessionLock, SessionState, WorkerSessionCursor,
        WorkerSessionRef, WorkerStore,
    },
};
use thalamus_identity::{DebugKeyRequest, WorkerIdentity};
use uuid::Uuid;

use crate::Result;

const SESSION_LABEL: &str = "thalamus.dev/debug-session";
const EXPIRY: &str = "thalamus.dev/credentials-expire-at";
const JOB_UID: &str = "thalamus.dev/job-uid";
const OWNERSHIP: &str = "thalamus.dev/ownership-nonce";
const CNI_CONTRACT: &str = "admin-verified-networkpolicy-service-dnat";
const ADMISSION_NAME: &str = "live-ownership.thalamus.dev";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointConfig {
    url: String,
    address: IpAddr,
}
impl From<EndpointConfig> for TrustedEndpoint {
    fn from(value: EndpointConfig) -> Self {
        Self {
            url: value.url,
            address: value.address,
        }
    }
}

/// All values come from the operator, never from a debug request. The policy
/// digest is SHA-256 of the canonical JSON serialization of the reviewed live
/// AgentgatewayPolicy `spec`, indexed by service ID. The administrator must
/// compare it to the Git-rendered policy for the installed gateway version.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    opencode_image: String,
    checkout_image: String,
    checkout_revision: String,
    allowed_models: Vec<String>,
    /// One configured gateway URL is one route, not a generic model router.
    gateway_model: String,
    model_policy_namespaces: BTreeMap<String, String>,
    git_repository: EndpointConfig,
    model_gateway: EndpointConfig,
    thalamus_mcp: EndpointConfig,
    flux_mcp: EndpointConfig,
    mcp_audience: String,
    proxy_namespace: String,
    proxy_pod_labels: BTreeMap<String, String>,
    max_duration_seconds: i64,
    ttl_seconds_after_finished: i32,
    max_model_tokens: u64,
    max_tool_calls: u32,
    trusted_policy_sha256: BTreeMap<String, String>,
    cni_contract: String,
    gateway_namespace: String,
    gateway_name: String,
    gateway_service_namespace: String,
    gateway_service_name: String,
    gateway_endpoint_port: u16,
    rate_limit_namespace: String,
    rate_limit_service: String,
    rate_limit_domain: String,
    rate_limit_deployment: String,
    rate_limit_configmap: String,
    trusted_rate_limit_config_sha256: String,
}

pub struct DebugReconciler {
    client: Client,
    store: DebugStore,
    worker: WorkerStore,
    identity: WorkerIdentity,
    namespace: String,
    projection_namespace: String,
    projection_name: String,
    config: Option<Config>,
}

impl DebugReconciler {
    pub fn new(
        db: PgPool,
        client: Client,
        namespace: String,
        projection_namespace: String,
        projection_name: String,
    ) -> Result<Self> {
        let config = match env::var("THALAMUS_DEBUG_CONFIG_JSON") {
            Ok(value) if !value.trim().is_empty() => {
                let config: Config = serde_json::from_str(&value)?;
                if config.cni_contract != CNI_CONTRACT
                    || config.allowed_models.len() != 1
                    || config.allowed_models.first() != Some(&config.gateway_model)
                    || config.gateway_model.is_empty()
                    || config
                        .model_policy_namespaces
                        .get(&config.gateway_model)
                        .is_none_or(String::is_empty)
                    || config.model_policy_namespaces.get(&config.gateway_model) != Some(&namespace)
                    || projection_namespace != namespace
                    || config.mcp_audience.is_empty()
                    || config.proxy_namespace != "thalamus-system"
                    || config.proxy_pod_labels
                        != BTreeMap::from([(
                            "app.kubernetes.io/name".to_owned(),
                            "thalamus-server".to_owned(),
                        )])
                    || config.gateway_namespace.is_empty()
                    || config.gateway_name.is_empty()
                    || config.gateway_service_namespace.is_empty()
                    || config.gateway_service_name.is_empty()
                    || config.gateway_endpoint_port == 0
                    || config.rate_limit_namespace.is_empty()
                    || config.rate_limit_service.is_empty()
                    || config.rate_limit_domain.is_empty()
                    || config.rate_limit_deployment.is_empty()
                    || config.rate_limit_configmap.is_empty()
                    || config.trusted_rate_limit_config_sha256.len() != 64
                    || !config
                        .trusted_rate_limit_config_sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    || !matches!(config.checkout_revision.len(), 40 | 64)
                    || !config
                        .checkout_revision
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    || config.trusted_policy_sha256.is_empty()
                    || config.trusted_policy_sha256.values().any(|v| {
                        v.len() != 64
                            || !v
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                {
                    return Err("invalid trusted debug configuration".into());
                }
                let model = reqwest::Url::parse(&config.model_gateway.url)?;
                let thalamus = reqwest::Url::parse(&config.thalamus_mcp.url)?;
                let flux = reqwest::Url::parse(&config.flux_mcp.url)?;
                if model.scheme() != "https"
                    || model
                        .host_str()
                        .is_none_or(|host| host.parse::<IpAddr>().is_ok())
                    || model.path().trim_end_matches('/') != "/v1"
                    || !model.username().is_empty()
                    || model.password().is_some()
                    || model.query().is_some()
                    || model.fragment().is_some()
                    || thalamus.path() != "/mcp"
                    || flux.path() != "/flux-mcp"
                    || config.mcp_audience != config.thalamus_mcp.url
                {
                    return Err("invalid trusted debug endpoint".into());
                }
                Some(config)
            }
            Ok(_) | Err(env::VarError::NotPresent) => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            client,
            store: DebugStore::new(db.clone()),
            worker: WorkerStore::new(db.clone()),
            identity: WorkerIdentity::new(db),
            namespace,
            projection_namespace,
            projection_name,
            config,
        })
    }
    /// The same complete key snapshot must be projected into every namespace
    /// whose AgentgatewayPolicy selects it.
    pub fn projection_namespaces(&self) -> BTreeSet<String> {
        let mut namespaces = BTreeSet::from([self.projection_namespace.clone()]);
        if let Some(config) = &self.config {
            namespaces.extend(config.model_policy_namespaces.values().cloned());
        }
        namespaces
    }

    fn job_config(&self, config: &Config) -> DebugJobConfig {
        let endpoint = |e: &EndpointConfig| TrustedEndpoint {
            url: e.url.clone(),
            address: e.address,
        };
        DebugJobConfig {
            namespace: self.namespace.clone(),
            service_account: "thalamus-debug".into(),
            opencode_image: config.opencode_image.clone(),
            checkout_image: config.checkout_image.clone(),
            git_repository: endpoint(&config.git_repository),
            model_gateway: endpoint(&config.model_gateway),
            thalamus_mcp: endpoint(&config.thalamus_mcp),
            flux_mcp: endpoint(&config.flux_mcp),
            proxy_namespace: config.proxy_namespace.clone(),
            proxy_pod_labels: config.proxy_pod_labels.clone(),
            max_duration_seconds: config.max_duration_seconds,
            ttl_seconds_after_finished: config.ttl_seconds_after_finished,
            max_model_tokens: config.max_model_tokens,
            max_tool_calls: config.max_tool_calls,
        }
    }

    pub async fn reconcile(&self) -> Result<()> {
        let mut cursor = None;
        let mut failed = false;
        loop {
            let sessions = self.worker.list_actionable(cursor, 100).await?;
            if sessions.is_empty() {
                break;
            }
            let count = sessions.len();
            cursor = sessions.last().map(WorkerSessionCursor::from);
            for item in sessions {
                if self.reconcile_one(item).await.is_err() {
                    failed = true;
                }
            }
            if count < 100 {
                break;
            }
        }
        if failed {
            Err("debug reconciliation incomplete".into())
        } else {
            Ok(())
        }
    }

    async fn reconcile_one(&self, item: WorkerSessionRef) -> Result<()> {
        let Some(mut lock) = self.worker.try_lock_session(item.id).await? else {
            return Ok(());
        };
        let actor = DebugIdentity {
            user_id: item.user_id,
            team_id: item.team_id,
        };
        lock.check().await?;
        if let Some((terminal, _)) = self
            .worker
            .finalize_expired_or_inactive(&actor, item.id)
            .await?
        {
            let result = self.cleanup(&terminal, &mut lock).await;
            lock.release().await?;
            return result;
        }
        // Re-read after acquiring the lock: the API can cancel concurrently.
        let session = match self.store.get(&actor, item.id).await {
            Ok(session) => session,
            Err(_error) if item.cleanup_required => {
                let result = self.cleanup(&item, &mut lock).await;
                lock.release().await?;
                return result;
            }
            Err(error) => return Err(error.into()),
        };
        if session.state.is_terminal() {
            if session.cleanup_required {
                self.cleanup(&item, &mut lock).await?;
            }
            lock.release().await?;
            return Ok(());
        }
        let Some(config) = &self.config else {
            // Debug creation is disabled without operator config. Existing
            // sessions are failed and remain on the ordinary cleanup path.
            self.fail(&session, "debug_not_configured").await?;
            lock.release().await?;
            return Ok(());
        };
        lock.check().await?;
        let result = self.launch_or_monitor(config, &session, &mut lock).await;
        if result.is_err() {
            // Never persist upstream error bodies or credential-bearing details.
            lock.check().await?;
            let _ = self.fail(&session, "debug_workload_failed").await;
        }
        lock.release().await?;
        result
    }

    async fn fail(&self, session: &DebugSession, reason: &'static str) -> Result<()> {
        let actor = DebugIdentity {
            user_id: session.user_id,
            team_id: session.team_id,
        };
        self.store
            .transition(
                &actor,
                session.id,
                session.state,
                SessionState::Failed,
                Some(json!({"reason":reason})),
            )
            .await?;
        Ok(())
    }

    fn request(&self, config: &Config, session: &DebugSession) -> Result<DebugJobRequest> {
        if session.job_namespace != self.namespace
            || session.job_name != debug_job_name(session.id)
            || session.checkout_revision != config.checkout_revision
            || !config.allowed_models.contains(&session.model)
            || config.gateway_model != session.model
            || !config.model_policy_namespaces.contains_key(&session.model)
            || !config.trusted_policy_sha256.contains_key(&session.model)
        {
            return Err("debug session does not match installation contract".into());
        }
        Ok(DebugJobRequest {
            session_id: session.id,
            ownership_nonce: session.ownership_nonce,
            namespace: session.job_namespace.clone(),
            git_revision: session.checkout_revision.clone(),
            model: session.model.clone(),
            created_at: session.created_at,
            deadline: session.deadline,
            model_token_budget: session.model_budget.try_into()?,
            tool_call_budget: session.tool_budget.try_into()?,
        })
    }

    async fn launch_or_monitor(
        &self,
        config: &Config,
        session: &DebugSession,
        lock: &mut SessionLock,
    ) -> Result<()> {
        let request = self.request(config, session)?;
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        if session.state == SessionState::Running {
            let job = jobs
                .get_opt(&session.job_name)
                .await?
                .ok_or("running debug Job is missing")?;
            if !resource_owned(
                &job.metadata,
                session.id,
                session.ownership_nonce,
                &self.namespace,
                &session.job_name,
            ) {
                return Err("foreign debug Job".into());
            }
            if let Some(success) = thalamus_worker::job_outcome(&job) {
                let actor = DebugIdentity {
                    user_id: session.user_id,
                    team_id: session.team_id,
                };
                let state = if success {
                    SessionState::Completed
                } else {
                    SessionState::Failed
                };
                let reason = if success {
                    "job_completed"
                } else {
                    "job_failed"
                };
                lock.check().await?;
                self.store
                    .transition(
                        &actor,
                        session.id,
                        SessionState::Running,
                        state,
                        Some(json!({"reason":reason})),
                    )
                    .await?;
                return Ok(());
            }
        }
        // Check the installed protection before creating credentials or grants,
        // not only after the staged Job and NetworkPolicy exist.
        self.verify_admission().await?;
        self.provision(config, session, lock).await?;
        let key = self
            .identity
            .debug_key(session.id)
            .await?
            .ok_or("debug model key missing")?;
        if !self.projection_has_key(config, &key).await? {
            // The outer worker projects before this sweep. Registration and
            // projection are separate; retry after the next complete snapshot.
            return Ok(());
        }
        self.verify_gateway_policy(config, session).await?;
        let model = self
            .secret(session, &debug_model_secret_name(session.id), "key")
            .await?
            .ok_or("debug model Secret missing")?;
        if !self
            .probe_gateway(config, &session.model, &model, true)
            .await?
        {
            if session.state == SessionState::Pending {
                return Ok(());
            }
            return Err("running debug key is no longer active at every gateway".into());
        }
        lock.check().await?;
        let job = {
            let authority = LaunchAuthority {
                reconciler: self,
                config,
                session,
                lock: tokio::sync::Mutex::new(&mut *lock),
            };
            create_debug_job(
                &self.client,
                &self.job_config(config),
                &request,
                Some(&authority),
            )
            .await?
        };
        let uid = job
            .metadata
            .uid
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or("debug Job UID missing")?;
        lock.check().await?;
        self.bind_secrets(session, uid).await?;
        if session.state == SessionState::Pending {
            let actor = DebugIdentity {
                user_id: session.user_id,
                team_id: session.team_id,
            };
            lock.check().await?;
            self.store
                .transition(
                    &actor,
                    session.id,
                    SessionState::Pending,
                    SessionState::Running,
                    None,
                )
                .await?;
        }
        Ok(())
    }

    async fn provision(
        &self,
        config: &Config,
        session: &DebugSession,
        lock: &mut SessionLock,
    ) -> Result<()> {
        let actor = DebugIdentity {
            user_id: session.user_id,
            team_id: session.team_id,
        };
        lock.check().await?;
        self.ensure_server_access(session).await?;
        let server_name = debug_server_secret_name(session.id);
        if self
            .secret(session, &server_name, "password")
            .await?
            .is_none()
        {
            let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
            if session.state != SessionState::Pending
                || jobs.get_opt(&session.job_name).await?.is_some()
            {
                return Err("debug server Secret disappeared after staging".into());
            }
            let mut bytes = [0u8; 32];
            OsRng.try_fill_bytes(&mut bytes)?;
            let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            lock.check().await?;
            self.create_secret(session, &server_name, "password", &password)
                .await?;
        }
        let model_name = debug_model_secret_name(session.id);
        let model = match self.secret(session, &model_name, "key").await? {
            Some(value) => value,
            None => {
                // Never replace a missing already-issued key: it would reset a
                // live budget and make revocation verification impossible.
                if self.identity.debug_key(session.id).await?.is_some() {
                    return Err("issued debug model Secret is missing".into());
                }
                let mut bytes = [0u8; 32];
                OsRng.try_fill_bytes(&mut bytes)?;
                let value = format!("thl_{}", URL_SAFE_NO_PAD.encode(bytes));
                lock.check().await?;
                self.create_secret(session, &model_name, "key", &value)
                    .await?;
                value
            }
        };
        lock.check().await?;
        self.identity
            .register_debug_key(DebugKeyRequest {
                session_id: session.id,
                user_id: session.user_id,
                team_id: session.team_id,
                project_id: session.project_id,
                service: &session.service_id,
                model: &session.model,
                expires_at: session.deadline,
                secret: &model,
                budget: session.model_budget,
            })
            .await?;
        let mcp_name = debug_mcp_secret_name(session.id);
        let token = match self.secret(session, &mcp_name, "token").await? {
            Some(value) => value,
            None => {
                // An issuance whose Secret write did not complete can safely be
                // rotated: no Job has been resumed with that lost credential.
                let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
                if session.state != SessionState::Pending
                    || jobs.get_opt(&session.job_name).await?.is_some()
                {
                    return Err("debug MCP Secret disappeared after staging".into());
                }
                lock.check().await?;
                let issued = self
                    .store
                    .issue_tool_credential(
                        &actor,
                        session.id,
                        &config.mcp_audience,
                        session.deadline,
                    )
                    .await?;
                lock.check().await?;
                self.create_secret(session, &mcp_name, "token", &issued.secret)
                    .await?;
                issued.secret
            }
        };
        self.store
            .verify_tool_credential_for_launch(
                &actor,
                session.id,
                &token,
                &config.mcp_audience,
                session.deadline,
            )
            .await?;
        Ok(())
    }

    async fn secret(
        &self,
        session: &DebugSession,
        name: &str,
        key: &str,
    ) -> Result<Option<String>> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(secret) = api.get_opt(name).await? else {
            return Ok(None);
        };
        if !secret_owned(
            &secret,
            session.id,
            session.ownership_nonce,
            &self.namespace,
            name,
            session.deadline,
        ) || secret.metadata.deletion_timestamp.is_some()
        {
            return Err("foreign debug Secret".into());
        }
        let data = secret.data.as_ref().ok_or("debug Secret data missing")?;
        if data.len() != 1 {
            return Err("invalid debug Secret data".into());
        }
        let value =
            std::str::from_utf8(&data.get(key).ok_or("debug Secret key missing")?.0)?.to_owned();
        let valid = match key {
            "password" => {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }
            "key" => {
                value.len() == 47
                    && value.starts_with("thl_")
                    && value[4..]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            }
            "token" => {
                value.len() == 69
                    && value.starts_with("thld_")
                    && value[5..]
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }
            _ => false,
        };
        if !valid {
            return Err("invalid debug Secret credential".into());
        }
        Ok(Some(value))
    }

    async fn create_secret(
        &self,
        session: &DebugSession,
        name: &str,
        key: &str,
        value: &str,
    ) -> Result<()> {
        let config = self.config.as_ref().ok_or("debug configuration missing")?;
        let policy_namespace = config
            .model_policy_namespaces
            .get(&session.model)
            .ok_or("model policy namespace missing")?;
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels(session.id)),
                annotations: Some(BTreeMap::from([
                    (EXPIRY.into(), session.deadline.to_rfc3339()),
                    (OWNERSHIP.into(), session.ownership_nonce.to_string()),
                    (
                        "thalamus.dev/model-policy-namespace".into(),
                        policy_namespace.clone(),
                    ),
                ])),
                ..Default::default()
            },
            immutable: Some(true),
            type_: Some("Opaque".into()),
            data: Some(BTreeMap::from([(
                key.into(),
                ByteString(value.as_bytes().to_vec()),
            )])),
            ..Default::default()
        };
        api.create(&PostParams::default(), &secret).await?;
        Ok(())
    }

    async fn bind_secrets(&self, session: &DebugSession, uid: &str) -> Result<()> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        for (name, key) in [
            (debug_server_secret_name(session.id), "password"),
            (debug_model_secret_name(session.id), "key"),
            (debug_mcp_secret_name(session.id), "token"),
        ] {
            self.secret(session, &name, key)
                .await?
                .ok_or("debug Secret missing")?;
            let secret = api.get(&name).await?;
            if !secret_owned(
                &secret,
                session.id,
                session.ownership_nonce,
                &self.namespace,
                &name,
                session.deadline,
            ) {
                return Err("foreign debug Secret".into());
            }
            if let Some(existing) = secret
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(JOB_UID))
            {
                if existing != uid {
                    return Err("debug Secret Job binding conflict".into());
                }
                continue;
            }
            let rv = secret
                .metadata
                .resource_version
                .as_deref()
                .ok_or("debug Secret resource version missing")?;
            api.patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(
                    json!({"metadata":{"resourceVersion":rv,"annotations":{(JOB_UID):uid}}}),
                ),
            )
            .await?;
        }
        Ok(())
    }

    /// Grant the API only the one server password Secret, never model or MCP
    /// credentials. These objects are protected by the installation's RBAC
    /// admission policy and are removed on every terminal cleanup.
    async fn ensure_server_access(&self, session: &DebugSession) -> Result<()> {
        let name = &session.job_name;
        let roles: Api<Role> = Api::namespaced(self.client.clone(), &self.namespace);
        let bindings: Api<RoleBinding> = Api::namespaced(self.client.clone(), &self.namespace);
        let role: Role = serde_json::from_value(json!({
            "apiVersion":"rbac.authorization.k8s.io/v1", "kind":"Role",
            "metadata":{"name":name,"namespace":self.namespace,"labels":labels(session.id),
                "annotations":{(OWNERSHIP):session.ownership_nonce.to_string()}},
            "rules":[{"apiGroups":[""],"resources":["secrets"],
                "resourceNames":[debug_server_secret_name(session.id)],"verbs":["get"]}]
        }))?;
        let binding: RoleBinding = serde_json::from_value(json!({
            "apiVersion":"rbac.authorization.k8s.io/v1", "kind":"RoleBinding",
            "metadata":{"name":name,"namespace":self.namespace,"labels":labels(session.id),
                "annotations":{(OWNERSHIP):session.ownership_nonce.to_string()}},
            "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":name},
            "subjects":[{"kind":"ServiceAccount","name":"thalamus-server","namespace":"thalamus-system"}]
        }))?;
        let actual_role = match roles.create(&PostParams::default(), &role).await {
            Ok(role) => role,
            Err(kube::Error::Api(error)) if error.code == 409 => roles.get(name).await?,
            Err(error) => return Err(error.into()),
        };
        if !resource_owned(
            &actual_role.metadata,
            session.id,
            session.ownership_nonce,
            &self.namespace,
            name,
        ) || actual_role.metadata.deletion_timestamp.is_some()
            || actual_role
                .metadata
                .owner_references
                .as_ref()
                .is_some_and(|v| !v.is_empty())
            || serde_json::to_value(&actual_role.rules)? != serde_json::to_value(&role.rules)?
        {
            return Err("foreign debug server Role".into());
        }
        let actual_binding = match bindings.create(&PostParams::default(), &binding).await {
            Ok(binding) => binding,
            Err(kube::Error::Api(error)) if error.code == 409 => bindings.get(name).await?,
            Err(error) => return Err(error.into()),
        };
        if !resource_owned(
            &actual_binding.metadata,
            session.id,
            session.ownership_nonce,
            &self.namespace,
            name,
        ) || actual_binding.metadata.deletion_timestamp.is_some()
            || actual_binding
                .metadata
                .owner_references
                .as_ref()
                .is_some_and(|v| !v.is_empty())
            || serde_json::to_value(&actual_binding.role_ref)?
                != serde_json::to_value(&binding.role_ref)?
            || serde_json::to_value(&actual_binding.subjects)?
                != serde_json::to_value(&binding.subjects)?
        {
            return Err("foreign debug server RoleBinding".into());
        }
        Ok(())
    }

    async fn projection_has_key(
        &self,
        config: &Config,
        key: &thalamus_identity::DebugKeyRegistration,
    ) -> Result<bool> {
        let namespace = config
            .model_policy_namespaces
            .get(&key.model)
            .ok_or("model policy namespace missing")?;
        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
        let Some(map) = maps.get_opt(&self.projection_name).await? else {
            return Ok(false);
        };
        let Some(value) = map
            .data
            .as_ref()
            .and_then(|d| d.get(&key.metadata.id.to_string()))
        else {
            return Ok(false);
        };
        let entry: Value = serde_json::from_str(value)?;
        let digest: String = key.digest.iter().map(|b| format!("{b:02x}")).collect();
        let meta = &entry["metadata"];
        if entry["keyHash"] != format!("sha256:{digest}")
            || meta["key_id"] != key.metadata.id.to_string()
            || meta["debug_session_id"] != key.session_id.to_string()
            || meta["debug_model"] != key.model
            || meta["user"] != key.metadata.user_id.to_string()
            || meta["team"] != key.metadata.team_id.to_string()
            || meta["project"]
                != key
                    .metadata
                    .project_id
                    .map(|v| v.to_string())
                    .ok_or("debug project missing")?
            || meta["kind"] != "debug"
            || meta["allowed_services"] != json!(key.metadata.allowed_services)
            || meta["budget"] != json!(key.metadata.budget)
            || meta["expiry"] != json!(key.metadata.expires_at.map(|v| v.to_rfc3339()))
            || meta["team_budget"].as_i64().is_none_or(|n| n <= 0)
            || !matches!(meta["budget_window"].as_str(), Some("day" | "month"))
            || (!meta["project_budget"].is_null()
                && meta["project_budget"].as_i64().is_none_or(|n| n <= 0))
        {
            return Err("debug gateway projection does not bind the key".into());
        }
        Ok(true)
    }

    async fn verify_gateway_policy(&self, config: &Config, session: &DebugSession) -> Result<()> {
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("agentgateway.dev", "v1alpha1", "AgentgatewayPolicy"),
            "agentgatewaypolicies",
        );
        let policy_namespace = config
            .model_policy_namespaces
            .get(&session.model)
            .ok_or("model policy namespace missing")?;
        let policies: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), policy_namespace, &resource);
        let policy = policies.get(&format!("{}-auth", session.model)).await?;
        if policy.metadata.deletion_timestamp.is_some() {
            return Err("gateway policy is being deleted".into());
        }
        let spec = policy
            .data
            .get("spec")
            .ok_or("gateway policy spec missing")?;
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(spec)?));
        if config.trusted_policy_sha256.get(&session.model) != Some(&digest) {
            return Err("gateway policy differs from reviewed policy".into());
        }
        let targets = spec
            .get("targetRefs")
            .and_then(Value::as_array)
            .ok_or("gateway target missing")?;
        if targets.len() != 1
            || targets[0]["group"] != "gateway.networking.k8s.io"
            || targets[0]["kind"] != "HTTPRoute"
            || targets[0]["name"] != session.model
            || spec.pointer("/traffic/apiKeyAuthentication/mode") != Some(&json!("Strict"))
            || spec.pointer("/traffic/authorization/action") != Some(&json!("Require"))
            || spec.pointer("/traffic/rateLimit/global/failureMode") != Some(&json!("FailClosed"))
        {
            return Err("gateway policy does not enforce the debug contract".into());
        }
        let selectors = spec
            .pointer("/traffic/apiKeyAuthentication/configMapSelector/matchLabels")
            .and_then(Value::as_object)
            .ok_or("gateway key selector missing")?;
        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), policy_namespace);
        let map = maps.get(&self.projection_name).await?;
        let labels = map
            .metadata
            .labels
            .as_ref()
            .ok_or("projection labels missing")?;
        if selectors.is_empty()
            || !selectors.iter().all(|(k, v)| {
                v.as_str()
                    .is_some_and(|s| labels.get(k).is_some_and(|x| x == s))
            })
        {
            return Err("gateway does not select the projection".into());
        }
        let descriptors = spec
            .pointer("/traffic/rateLimit/global/descriptors")
            .and_then(Value::as_array)
            .ok_or("gateway token descriptors missing")?;
        for key in ["key_id", "project", "team"] {
            if !descriptors.iter().any(|d| {
                d["unit"] == "Tokens"
                    && d.get("limitOverride").is_some()
                    && d["entries"].as_array().is_some_and(|entries| {
                        entries.iter().any(|e| {
                            e["name"] == key
                                && e["expression"]
                                    .as_str()
                                    .is_some_and(|expr| expr.contains(&format!("apiKey.{key}")))
                        })
                    })
            }) {
                return Err("gateway token allowance descriptor missing".into());
            }
        }
        let generation = policy
            .metadata
            .generation
            .ok_or("gateway policy generation missing")?;
        let ancestors = policy
            .data
            .pointer("/status/ancestors")
            .and_then(Value::as_array)
            .ok_or("gateway policy status missing")?;
        let matching: Vec<_> = ancestors
            .iter()
            .filter(|a| {
                a["ancestorRef"]["kind"] == "Gateway"
                    && a["ancestorRef"]["name"] == config.gateway_name
                    && a["ancestorRef"]["namespace"]
                        .as_str()
                        .unwrap_or(policy_namespace)
                        == config.gateway_namespace
            })
            .collect();
        if matching.len() != 1 {
            return Err("gateway policy ancestor is ambiguous".into());
        }
        let conditions = matching[0]["conditions"]
            .as_array()
            .ok_or("gateway policy conditions missing")?;
        for kind in ["Accepted", "Attached"] {
            let matching: Vec<_> = conditions
                .iter()
                .filter(|c| {
                    c["type"] == kind
                        && c["status"] == "True"
                        && c["observedGeneration"] == generation
                        && c["reason"] != "PartiallyValid"
                })
                .collect();
            if matching.len() != 1 {
                return Err("gateway policy is not fully accepted and attached".into());
            }
        }
        let key = self
            .identity
            .debug_key(session.id)
            .await?
            .ok_or("model key missing")?;
        let projected = map
            .data
            .as_ref()
            .and_then(|data| data.get(&key.metadata.id.to_string()))
            .ok_or("model key projection missing")?;
        let projected: Value = serde_json::from_str(projected)?;
        let window = projected
            .pointer("/metadata/budget_window")
            .and_then(Value::as_str)
            .ok_or("budget window missing")?;
        self.verify_limiter(config, spec, window).await?;
        self.verify_model_route(config, session, policy_namespace)
            .await?;
        Ok(())
    }

    /// The gateway's FailClosed setting alone cannot detect an RLS with an
    /// unknown domain, which the pinned Envoy service treats as unlimited.
    /// Inspect the installed, reviewed configuration and its mounted backend.
    /// The administrators remain responsible for the pinned service, shared
    /// Redis, network protection, and functioning token accounting.
    async fn verify_limiter(
        &self,
        config: &Config,
        policy_spec: &Value,
        window: &str,
    ) -> Result<()> {
        let global = policy_spec
            .pointer("/traffic/rateLimit/global")
            .ok_or("rate limit policy missing")?;
        if global["domain"] != config.rate_limit_domain
            || global["backendRef"]["namespace"] != config.rate_limit_namespace
            || global["backendRef"]["name"] != config.rate_limit_service
        {
            return Err("rate limit backend does not match the installed contract".into());
        }
        let port = global["backendRef"]["port"]
            .as_i64()
            .filter(|p| (1..=65535).contains(p))
            .ok_or("rate limit port missing")?;
        let maps: Api<ConfigMap> =
            Api::namespaced(self.client.clone(), &config.rate_limit_namespace);
        let map_name = &config.rate_limit_configmap;
        let map = maps.get(map_name).await?;
        let yaml = map
            .data
            .as_ref()
            .and_then(|d| d.get("config.yaml"))
            .ok_or("rate limit config missing")?;
        if map.metadata.deletion_timestamp.is_some()
            || format!("{:x}", Sha256::digest(yaml.as_bytes()))
                != config.trusted_rate_limit_config_sha256
        {
            return Err("rate limit config differs from reviewed config".into());
        }
        let parsed: Value = serde_yaml::from_str(yaml)?;
        if parsed["domain"] != config.rate_limit_domain {
            return Err("rate limit domain missing".into());
        }
        let descriptors = parsed["descriptors"]
            .as_array()
            .ok_or("rate limit descriptors missing")?;
        if descriptors.len() != 4 {
            return Err("rate limit descriptors are not exclusive".into());
        }
        for key in ["key_id", "project", "team", "benchmark_key"] {
            if descriptors
                .iter()
                .filter(|d| {
                    d["key"] == key
                        && d["rate_limit"]["unit"] == window
                        && d["rate_limit"]["requests_per_unit"] == 1
                        && d.get("value").is_none()
                })
                .count()
                != 1
            {
                return Err("rate limit descriptor is not enforced".into());
            }
        }
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("apps", "v1", "Deployment"),
            "deployments",
        );
        let deployments: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &config.rate_limit_namespace, &resource);
        let deployment = deployments.get(&config.rate_limit_deployment).await?;
        let generation = deployment
            .metadata
            .generation
            .ok_or("rate limit deployment generation missing")?;
        let desired = deployment
            .data
            .pointer("/spec/replicas")
            .and_then(Value::as_i64)
            .unwrap_or(1);
        if deployment.metadata.deletion_timestamp.is_some()
            || desired <= 0
            || deployment
                .data
                .pointer("/spec/template/metadata/annotations/checksum~1config")
                .and_then(Value::as_str)
                != Some(config.trusted_rate_limit_config_sha256.as_str())
            || deployment.data.pointer("/status/observedGeneration") != Some(&json!(generation))
            || deployment
                .data
                .pointer("/status/updatedReplicas")
                .and_then(Value::as_i64)
                != Some(desired)
            || deployment
                .data
                .pointer("/status/availableReplicas")
                .and_then(Value::as_i64)
                != Some(desired)
        {
            return Err("rate limit deployment is not fully ready".into());
        }
        let pod = deployment
            .data
            .pointer("/spec/template/spec")
            .ok_or("rate limit pod template missing")?;
        let volume_name = pod["volumes"]
            .as_array()
            .and_then(|volumes| {
                volumes
                    .iter()
                    .find(|v| v["configMap"]["name"] == map_name.as_str())
            })
            .and_then(|v| v["name"].as_str())
            .ok_or("rate limit config volume missing")?;
        let mounted = pod["containers"].as_array().is_some_and(|containers| {
            containers.iter().any(|c| {
                let mounted = c["volumeMounts"].as_array().is_some_and(|mounts| {
                    mounts.iter().any(|m| {
                        m["name"] == volume_name
                            && m["mountPath"] == "/data/ratelimit/config"
                            && m.get("subPath").is_none()
                            && m.get("subPathExpr").is_none()
                    })
                });
                let env = c["env"].as_array();
                mounted
                    && env.is_some_and(|e| {
                        e.iter()
                            .any(|v| v["name"] == "RUNTIME_ROOT" && v["value"] == "/data")
                            && e.iter().any(|v| {
                                v["name"] == "RUNTIME_SUBDIRECTORY" && v["value"] == "ratelimit"
                            })
                            && e.iter()
                                .any(|v| v["name"] == "RUNTIME_WATCH_ROOT" && v["value"] == "false")
                            && e.iter().any(|v| {
                                v["name"] == "RUNTIME_IGNOREDOTFILES" && v["value"] == "true"
                            })
                            && e.iter()
                                .any(|v| v["name"] == "REDIS_SOCKET_TYPE" && v["value"] == "tcp")
                            && e.iter()
                                .any(|v| v["name"] == "REDIS_TLS" && v["value"] == "true")
                            && e.iter().any(|v| {
                                v["name"] == "HEALTHY_WITH_AT_LEAST_ONE_CONFIG_LOADED"
                                    && v["value"] == "true"
                            })
                            && e.iter().any(|v| {
                                v["name"] == "REDIS_HEALTH_CHECK_ACTIVE_CONNECTION"
                                    && v["value"] == "true"
                            })
                    })
                    && c.pointer("/readinessProbe/httpGet/path") == Some(&json!("/healthcheck"))
                    && c.pointer("/readinessProbe/httpGet/port") == Some(&json!(8080))
            })
        });
        if !mounted {
            return Err("rate limit deployment does not mount its reviewed config".into());
        }
        let services: Api<Service> =
            Api::namespaced(self.client.clone(), &config.rate_limit_namespace);
        let service = services.get(&config.rate_limit_service).await?;
        let service_spec = service
            .spec
            .as_ref()
            .ok_or("rate limit Service spec missing")?;
        let selector = service_spec
            .selector
            .as_ref()
            .filter(|s| !s.is_empty())
            .ok_or("rate limit Service selector missing")?;
        let pod_labels = deployment
            .data
            .pointer("/spec/template/metadata/labels")
            .and_then(Value::as_object)
            .ok_or("rate limit pod labels missing")?;
        if service.metadata.deletion_timestamp.is_some()
            || !selector
                .iter()
                .all(|(k, v)| pod_labels.get(k).and_then(Value::as_str) == Some(v.as_str()))
            || !service_spec
                .ports
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|p| i64::from(p.port) == port)
        {
            return Err("rate limit Service does not select the configured backend".into());
        }
        let endpoints: Api<Endpoints> =
            Api::namespaced(self.client.clone(), &config.rate_limit_namespace);
        let endpoints = endpoints.get(&config.rate_limit_service).await?;
        if endpoints.metadata.deletion_timestamp.is_some()
            || !endpoints
                .subsets
                .unwrap_or_default()
                .iter()
                .any(|s| s.addresses.as_ref().is_some_and(|a| !a.is_empty()))
        {
            return Err("rate limit service has no ready endpoints".into());
        }
        Ok(())
    }

    async fn verify_model_route(
        &self,
        config: &Config,
        session: &DebugSession,
        namespace: &str,
    ) -> Result<()> {
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute"),
            "httproutes",
        );
        let routes: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), namespace, &resource);
        let route = routes.get(&session.model).await?;
        let spec = route.data.get("spec").ok_or("model route spec missing")?;
        let url = reqwest::Url::parse(&config.model_gateway.url)?;
        let host = url.host_str().ok_or("model gateway hostname missing")?;
        if route.metadata.deletion_timestamp.is_some()
            || !spec["hostnames"]
                .as_array()
                .is_some_and(|hosts| hosts.iter().any(|value| value == host))
            || !spec["parentRefs"].as_array().is_some_and(|refs| {
                refs.iter().any(|r| {
                    r["name"] == config.gateway_name
                        && r["namespace"].as_str().unwrap_or(namespace) == config.gateway_namespace
                        && r["kind"].as_str().unwrap_or("Gateway") == "Gateway"
                        && r["group"].as_str().unwrap_or("gateway.networking.k8s.io")
                            == "gateway.networking.k8s.io"
                })
            })
            || !spec["rules"].as_array().is_some_and(|rules| {
                rules.iter().any(|r| {
                    r["matches"].as_array().is_some_and(|matches| {
                        matches.iter().any(|m| {
                            m["path"]["type"] == "Exact"
                                && m["path"]["value"] == "/v1/chat/completions"
                                && m["method"].as_str().is_none_or(|method| method == "POST")
                                && m["headers"].as_array().is_none_or(Vec::is_empty)
                                && m["queryParams"].as_array().is_none_or(Vec::is_empty)
                        })
                    })
                })
            })
        {
            return Err("configured model gateway does not match the model route".into());
        }
        let generation = route
            .metadata
            .generation
            .ok_or("model route generation missing")?;
        let parents = route
            .data
            .pointer("/status/parents")
            .and_then(Value::as_array)
            .ok_or("model route status missing")?;
        let parent = parents
            .iter()
            .find(|p| {
                p["parentRef"]["name"] == config.gateway_name
                    && p["parentRef"]["namespace"].as_str().unwrap_or(namespace)
                        == config.gateway_namespace
                    && p["parentRef"]["kind"].as_str().unwrap_or("Gateway") == "Gateway"
                    && p["parentRef"]["group"]
                        .as_str()
                        .unwrap_or("gateway.networking.k8s.io")
                        == "gateway.networking.k8s.io"
            })
            .ok_or("model route gateway status missing")?;
        let conditions = parent["conditions"]
            .as_array()
            .ok_or("model route conditions missing")?;
        for kind in ["Accepted", "ResolvedRefs"] {
            if !conditions.iter().any(|c| {
                c["type"] == kind && c["status"] == "True" && c["observedGeneration"] == generation
            }) {
                return Err("model route is not accepted and resolved".into());
            }
        }
        Ok(())
    }

    /// Probe every ready endpoint directly with TLS authenticated as the
    /// configured gateway hostname. The pinned gateway returns exactly this
    /// 401/body before forwarding an unauthenticated request. Administrators
    /// must ensure the Service selects every serving gateway and that no
    /// intermediary/backend can forge the gateway's rejection response.
    async fn probe_gateway(
        &self,
        config: &Config,
        model: &str,
        key: &str,
        active: bool,
    ) -> Result<bool> {
        let gateway_services: Api<Service> =
            Api::namespaced(self.client.clone(), &config.gateway_service_namespace);
        let gateway = gateway_services.get(&config.gateway_service_name).await?;
        let gateway_spec = gateway
            .spec
            .as_ref()
            .ok_or("gateway Service spec missing")?;
        let expected_ip = config.model_gateway.address.to_string();
        let service_ip = gateway_spec.cluster_ip.as_deref() == Some(expected_ip.as_str())
            || gateway_spec
                .cluster_ips
                .as_ref()
                .is_some_and(|ips| ips.contains(&expected_ip));
        let load_balancer_ip = gateway
            .status
            .as_ref()
            .and_then(|s| s.load_balancer.as_ref())
            .and_then(|lb| lb.ingress.as_ref())
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry.ip.as_deref() == Some(expected_ip.as_str()))
            });
        let base = reqwest::Url::parse(&config.model_gateway.url)?;
        let port = base
            .port_or_known_default()
            .ok_or("gateway Service port missing")?;
        if gateway.metadata.deletion_timestamp.is_some()
            || !(service_ip || load_balancer_ip)
            || gateway_spec.publish_not_ready_addresses == Some(true)
            || !gateway_spec
                .ports
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|p| p.port == i32::from(port))
        {
            return Err(
                "debug egress address does not identify the configured gateway Service".into(),
            );
        }
        let api: Api<Endpoints> =
            Api::namespaced(self.client.clone(), &config.gateway_service_namespace);
        let endpoints = api.get(&config.gateway_service_name).await?;
        if endpoints.metadata.deletion_timestamp.is_some()
            || endpoints
                .metadata
                .annotations
                .as_ref()
                .is_some_and(|a| a.contains_key("endpoints.kubernetes.io/over-capacity"))
        {
            return Err("gateway endpoints are incomplete".into());
        }
        let version = endpoints
            .metadata
            .resource_version
            .clone()
            .filter(|v| !v.is_empty())
            .ok_or("gateway endpoints resource version missing")?;
        let uid = endpoints
            .metadata
            .uid
            .clone()
            .filter(|v| !v.is_empty())
            .ok_or("gateway endpoints UID missing")?;
        let mut addresses = BTreeSet::new();
        for subset in endpoints.subsets.unwrap_or_default() {
            if !subset
                .ports
                .unwrap_or_default()
                .iter()
                .any(|p| p.port == i32::from(config.gateway_endpoint_port))
            {
                continue;
            }
            for address in subset.addresses.unwrap_or_default() {
                if !address.target_ref.as_ref().is_some_and(|r| {
                    r.kind.as_deref() == Some("Pod")
                        && r.uid.as_deref().is_some_and(|u| !u.is_empty())
                }) {
                    return Err("gateway endpoint has no Pod identity".into());
                }
                addresses.insert(address.ip.parse::<IpAddr>()?);
            }
        }
        if addresses.is_empty() {
            return Err("no ready gateway endpoints".into());
        }
        let mut url = reqwest::Url::parse(&config.model_gateway.url)?;
        let host = url.host_str().ok_or("gateway hostname missing")?.to_owned();
        url.set_port(Some(config.gateway_endpoint_port))
            .map_err(|_| "invalid gateway port")?;
        url.set_path("/v1/chat/completions");
        let mut unknown = [0u8; 32];
        OsRng.try_fill_bytes(&mut unknown)?;
        let unknown = format!("thl_{}", URL_SAFE_NO_PAD.encode(unknown));
        for ip in addresses {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .resolve(&host, SocketAddr::new(ip, config.gateway_endpoint_port))
                .build()?;
            if !gateway_rejects(&client, &url, model, &unknown).await? {
                return Err("gateway replica does not reject unknown keys".into());
            }
            let rejected = gateway_rejects(&client, &url, model, key).await?;
            if rejected == active {
                return Ok(false);
            }
        }
        let current = api.get(&config.gateway_service_name).await?;
        if current.metadata.uid.as_deref() != Some(uid.as_str())
            || current.metadata.resource_version.as_deref() != Some(version.as_str())
        {
            return Err("gateway endpoints changed during verification".into());
        }
        Ok(true)
    }

    async fn cleanup(&self, session: &WorkerSessionRef, lock: &mut SessionLock) -> Result<()> {
        if session.job_namespace != self.namespace || session.job_name != debug_job_name(session.id)
        {
            return Err("invalid debug resource identity".into());
        }
        let actor = DebugIdentity {
            user_id: session.user_id,
            team_id: session.team_id,
        };
        lock.check().await?;
        self.identity.revoke_debug_key(session.id).await?;
        let key = self.identity.debug_key(session.id).await?;
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let policies: Api<NetworkPolicy> = Api::namespaced(self.client.clone(), &self.namespace);
        let job = jobs.get_opt(&session.job_name).await?;
        let uid = if let Some(job) = &job {
            if !resource_owned(
                &job.metadata,
                session.id,
                session.ownership_nonce,
                &self.namespace,
                &session.job_name,
            ) {
                return Err("foreign debug Job".into());
            }
            Some(
                job.metadata
                    .uid
                    .as_deref()
                    .ok_or("debug Job UID missing")?
                    .to_owned(),
            )
        } else {
            None
        };
        if session
            .job_uid
            .as_deref()
            .zip(uid.as_deref())
            .is_some_and(|(stored, current)| stored != current)
        {
            return Err("debug Job UID differs from its durable binding".into());
        }
        let expected_uid = session.job_uid.as_deref().or(uid.as_deref());
        if let Some(job) = &job {
            lock.check().await?;
            delete_exact(&jobs, &session.job_name, &job.metadata, true).await?;
        }
        if let Some(child) = services.get_opt(&session.job_name).await? {
            if !child_owned(&child.metadata, session, expected_uid) {
                return Err("foreign debug Service".into());
            }
            lock.check().await?;
            delete_exact(&services, &session.job_name, &child.metadata, false).await?;
        }
        if let Some(child) = policies.get_opt(&session.job_name).await? {
            if !child_owned(&child.metadata, session, expected_uid) {
                return Err("foreign debug NetworkPolicy".into());
            }
            lock.check().await?;
            delete_exact(&policies, &session.job_name, &child.metadata, false).await?;
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_selector = ListParams::default().labels(&format!("{SESSION_LABEL}={}", session.id));
        let selected_pods = pods.list(&pod_selector).await?.items;
        if !selected_pods.is_empty() {
            self.verify_pod_admission().await?;
            let job_uid = expected_uid.ok_or("debug Pod has no verified Job UID")?;
            for pod in selected_pods {
                let name = pod
                    .metadata
                    .name
                    .as_deref()
                    .ok_or("debug Pod name missing")?;
                let owned = name.starts_with(&format!("{}-", session.job_name))
                    && name.len() > session.job_name.len() + 1
                    && resource_owned(
                        &pod.metadata,
                        session.id,
                        session.ownership_nonce,
                        &self.namespace,
                        name,
                    )
                    && pod
                        .metadata
                        .owner_references
                        .as_ref()
                        .is_some_and(|owners| {
                            owners.len() == 1 && {
                                let owner = &owners[0];
                                owner.api_version == "batch/v1"
                                    && owner.kind == "Job"
                                    && owner.name == session.job_name
                                    && owner.uid == job_uid
                                    && owner.controller == Some(true)
                            }
                        });
                if !owned {
                    return Err("foreign debug Pod".into());
                }
                lock.check().await?;
                delete_exact(&pods, name, &pod.metadata, false).await?;
            }
        }
        let bindings: Api<RoleBinding> = Api::namespaced(self.client.clone(), &self.namespace);
        let roles: Api<Role> = Api::namespaced(self.client.clone(), &self.namespace);
        if let Some(binding) = bindings.get_opt(&session.job_name).await? {
            if !resource_owned(
                &binding.metadata,
                session.id,
                session.ownership_nonce,
                &self.namespace,
                &session.job_name,
            ) {
                return Err("foreign debug server RoleBinding".into());
            }
            lock.check().await?;
            delete_exact(&bindings, &session.job_name, &binding.metadata, false).await?;
        }
        if let Some(role) = roles.get_opt(&session.job_name).await? {
            if !resource_owned(
                &role.metadata,
                session.id,
                session.ownership_nonce,
                &self.namespace,
                &session.job_name,
            ) {
                return Err("foreign debug server Role".into());
            }
            lock.check().await?;
            delete_exact(&roles, &session.job_name, &role.metadata, false).await?;
        }
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        for name in [
            debug_server_secret_name(session.id),
            debug_mcp_secret_name(session.id),
        ] {
            if let Some(secret) = secrets.get_opt(&name).await? {
                if !secret_owned(
                    &secret,
                    session.id,
                    session.ownership_nonce,
                    &self.namespace,
                    &name,
                    session.deadline,
                ) {
                    return Err("foreign debug Secret".into());
                }
                lock.check().await?;
                delete_exact(&secrets, &name, &secret.metadata, false).await?;
            }
        }
        let model_name = debug_model_secret_name(session.id);
        let model_secret = secrets.get_opt(&model_name).await?;
        if let Some(secret) = &model_secret {
            if !secret_owned(
                secret,
                session.id,
                session.ownership_nonce,
                &self.namespace,
                &model_name,
                session.deadline,
            ) {
                return Err("foreign debug model Secret".into());
            }
        }
        if let Some(key) = key {
            if key.metadata.revoked_at.is_none() {
                return Err("debug model key revocation pending".into());
            }
            let mut namespaces = self.projection_namespaces();
            if let Some(secret) = &model_secret {
                if let Some(namespace) = secret
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|a| a.get("thalamus.dev/model-policy-namespace"))
                {
                    namespaces.insert(namespace.clone());
                }
            }
            for namespace in namespaces {
                let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &namespace);
                let map = maps.get(&self.projection_name).await?;
                if map
                    .data
                    .as_ref()
                    .is_some_and(|d| d.contains_key(&key.metadata.id.to_string()))
                {
                    return Err("debug model key projection removal pending".into());
                }
            }
            // Retain the plaintext Secret only until rejection is proven. If it
            // was lost, the trusted expiry policy bounds any stale projection.
            if Utc::now() < session.deadline {
                let config = self
                    .config
                    .as_ref()
                    .ok_or("gateway revocation verification unavailable")?;
                let secret = model_secret
                    .as_ref()
                    .ok_or("debug model Secret missing before expiry")?;
                let bytes = &secret
                    .data
                    .as_ref()
                    .and_then(|d| d.get("key"))
                    .ok_or("debug model key missing")?
                    .0;
                let value = std::str::from_utf8(bytes)?;
                if Sha256::digest(value.as_bytes()).as_slice() != key.digest.as_slice() {
                    return Err("debug model key digest mismatch".into());
                }
                if !self.probe_gateway(config, &key.model, value, false).await? {
                    return Err("debug key revocation has not reached every gateway".into());
                }
            }
        }
        if let Some(secret) = &model_secret {
            lock.check().await?;
            delete_exact(&secrets, &model_name, &secret.metadata, false).await?;
        }
        if jobs.get_opt(&session.job_name).await?.is_some()
            || services.get_opt(&session.job_name).await?.is_some()
            || policies.get_opt(&session.job_name).await?.is_some()
            || bindings.get_opt(&session.job_name).await?.is_some()
            || roles.get_opt(&session.job_name).await?.is_some()
            || !pods.list(&pod_selector).await?.items.is_empty()
            || secrets.get_opt(&model_name).await?.is_some()
            || secrets
                .get_opt(&debug_server_secret_name(session.id))
                .await?
                .is_some()
            || secrets
                .get_opt(&debug_mcp_secret_name(session.id))
                .await?
                .is_some()
        {
            return Err("debug resource deletion pending".into());
        }
        lock.check().await?;
        self.worker
            .mark_cleanup_complete(&actor, session.id)
            .await?;
        Ok(())
    }
}

struct LaunchAuthority<'a> {
    reconciler: &'a DebugReconciler,
    config: &'a Config,
    session: &'a DebugSession,
    lock: tokio::sync::Mutex<&'a mut SessionLock>,
}

struct IsolationResources<'a> {
    job: &'a Job,
    policy: &'a NetworkPolicy,
    service: &'a Service,
}
impl DebugLaunchAuthority for LaunchAuthority<'_> {
    fn verify<'a>(
        &'a self,
        _config: &'a DebugJobConfig,
        request: &'a DebugJobRequest,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<DebugAdmission, DebugJobError>> + Send + 'a>>
    {
        Box::pin(async move {
            {
                let mut lock = self.lock.lock().await;
                lock.check()
                    .await
                    .map_err(|_| DebugJobError::AdmissionDenied)?;
            }
            let result: Result<DebugAdmission> = async {
                let actor = DebugIdentity {
                    user_id: self.session.user_id,
                    team_id: self.session.team_id,
                };
                let session = self
                    .reconciler
                    .store
                    .get(&actor, request.session_id)
                    .await?;
                let expected = self.reconciler.request(self.config, &session)?;
                if session.state.is_terminal()
                    || session.cleanup_required
                    || request.namespace != expected.namespace
                    || request.ownership_nonce != expected.ownership_nonce
                    || request.git_revision != expected.git_revision
                    || request.model != expected.model
                    || request.deadline != expected.deadline
                    || request.created_at != expected.created_at
                    || request.model_token_budget != expected.model_token_budget
                    || request.tool_call_budget != expected.tool_call_budget
                {
                    return Err("debug admission session mismatch".into());
                }
                let model = self
                    .reconciler
                    .secret(&session, &debug_model_secret_name(session.id), "key")
                    .await?
                    .ok_or("model Secret missing")?;
                let mcp = self
                    .reconciler
                    .secret(&session, &debug_mcp_secret_name(session.id), "token")
                    .await?
                    .ok_or("MCP Secret missing")?;
                self.reconciler
                    .secret(&session, &debug_server_secret_name(session.id), "password")
                    .await?
                    .ok_or("server Secret missing")?;
                let key = self
                    .reconciler
                    .identity
                    .verify_debug_key(DebugKeyRequest {
                        session_id: session.id,
                        user_id: session.user_id,
                        team_id: session.team_id,
                        project_id: session.project_id,
                        service: &session.service_id,
                        model: &session.model,
                        expires_at: session.deadline,
                        secret: &model,
                        budget: session.model_budget,
                    })
                    .await?;
                let mcp_id = self
                    .reconciler
                    .store
                    .verify_tool_credential_for_launch(
                        &actor,
                        session.id,
                        &mcp,
                        &self.config.mcp_audience,
                        session.deadline,
                    )
                    .await?;
                let registration = self
                    .reconciler
                    .identity
                    .debug_key(session.id)
                    .await?
                    .ok_or("model registration missing")?;
                if registration.metadata.id != key.id
                    || !self
                        .reconciler
                        .projection_has_key(self.config, &registration)
                        .await?
                {
                    return Err("model key not projected".into());
                }
                self.reconciler
                    .verify_gateway_policy(self.config, &session)
                    .await?;
                Ok(DebugAdmission {
                    session_id: session.id,
                    namespace: session.job_namespace,
                    git_revision: session.checkout_revision,
                    model: session.model,
                    deadline: session.deadline,
                    model_token_budget: request.model_token_budget,
                    tool_call_budget: request.tool_call_budget,
                    model_budget_lease_id: key.id,
                    tool_budget_lease_id: mcp_id,
                    model_credential_id: key.id,
                    mcp_credential_id: mcp_id,
                    credentials_expire_at: session.deadline,
                })
            }
            .await;
            result.map_err(|_| DebugJobError::AdmissionDenied)
        })
    }

    fn verify_isolation<'a>(
        &'a self,
        config: &'a DebugJobConfig,
        request: &'a DebugJobRequest,
        job: &'a Job,
        policy: &'a NetworkPolicy,
        service: &'a Service,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), DebugJobError>> + Send + 'a>> {
        Box::pin(async move {
            let mut lock = self.lock.lock().await;
            lock.check()
                .await
                .map_err(|_| DebugJobError::AdmissionDenied)?;
            self.reconciler
                .verify_isolation(
                    config,
                    request,
                    IsolationResources {
                        job,
                        policy,
                        service,
                    },
                    self.session,
                    &mut lock,
                )
                .await
                .map_err(|_| DebugJobError::AdmissionDenied)
        })
    }
}

impl DebugReconciler {
    async fn verify_isolation(
        &self,
        config: &DebugJobConfig,
        request: &DebugJobRequest,
        resources: IsolationResources<'_>,
        session: &DebugSession,
        lock: &mut SessionLock,
    ) -> Result<()> {
        let IsolationResources {
            job,
            policy,
            service,
        } = resources;
        let name = debug_job_name(request.session_id);
        let uid = job.metadata.uid.as_deref().ok_or("debug Job UID missing")?;
        let item = WorkerSessionRef {
            id: request.session_id,
            ownership_nonce: request.ownership_nonce,
            job_uid: Some(uid.to_owned()),
            user_id: session.user_id,
            team_id: session.team_id,
            state: SessionState::Pending,
            deadline: request.deadline,
            created_at: request.created_at,
            job_namespace: self.namespace.clone(),
            job_name: name.clone(),
            cleanup_required: false,
        };
        if !resource_owned(
            &job.metadata,
            request.session_id,
            request.ownership_nonce,
            &self.namespace,
            &name,
        ) || !child_owned(&policy.metadata, &item, Some(uid))
            || !child_owned(&service.metadata, &item, Some(uid))
        {
            return Err("debug workload ownership mismatch".into());
        }
        let accounts: Api<ServiceAccount> = Api::namespaced(self.client.clone(), &self.namespace);
        let account = accounts.get(&config.service_account).await?;
        if account.automount_service_account_token != Some(false)
            || account.metadata.deletion_timestamp.is_some()
        {
            return Err("debug service account contract missing".into());
        }
        self.verify_admission().await?;
        let wanted = policy.spec.as_ref().ok_or("debug policy spec missing")?;
        let labels = job
            .spec
            .as_ref()
            .and_then(|s| s.template.metadata.as_ref())
            .and_then(|m| m.labels.as_ref())
            .ok_or("debug pod labels missing")?;
        let policies: Api<NetworkPolicy> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut found = false;
        for other in policies.list(&ListParams::default()).await?.items {
            let spec = other.spec.as_ref().ok_or("network policy spec missing")?;
            if !selector_matches(&spec.pod_selector, labels)? {
                continue;
            }
            if other.metadata.name.as_deref() == Some(&name) {
                if other.metadata.uid != policy.metadata.uid {
                    return Err("debug policy changed during admission".into());
                }
                found = true;
            }
            // Policies are additive. Exact desired rules and an empty rule set
            // are safe; an unknown or wider peer/port cannot be adopted.
            if !rules_subset(&spec.ingress, &wanted.ingress)?
                || !rules_subset(&spec.egress, &wanted.egress)?
            {
                return Err("another NetworkPolicy broadens debug peers".into());
            }
        }
        if !found {
            return Err("debug policy missing from live policy list".into());
        }
        // Bind the exact Job UID durably while the Job is still suspended.
        // A crash after activation cannot make orphan-Pod ownership ambiguous.
        let actor = DebugIdentity {
            user_id: session.user_id,
            team_id: session.team_id,
        };
        lock.check().await?;
        self.worker
            .bind_job_uid(&actor, session.id, session.ownership_nonce, uid)
            .await?;
        Ok(())
    }

    async fn verify_admission(&self) -> Result<()> {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        let ns = namespaces.get(&self.namespace).await?;
        if ns.metadata.deletion_timestamp.is_some()
            || ns
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("thalamus.dev/live-admission"))
                .map(String::as_str)
                != Some("enforced")
        {
            return Err("debug namespace admission is not enabled".into());
        }
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk(
                "admissionregistration.k8s.io",
                "v1",
                "ValidatingAdmissionPolicy",
            ),
            "validatingadmissionpolicies",
        );
        let policies: Api<DynamicObject> = Api::all_with(self.client.clone(), &resource);
        let policy = policies.get(ADMISSION_NAME).await?;
        let spec = policy
            .data
            .get("spec")
            .ok_or("admission policy spec missing")?;
        if policy.metadata.deletion_timestamp.is_some()
            || spec["failurePolicy"] != "Fail"
            || spec.pointer(
                "/matchConstraints/namespaceSelector/matchLabels/thalamus.dev~1live-admission",
            ) != Some(&json!("enforced"))
            || spec["validations"].as_array().is_none_or(Vec::is_empty)
        {
            return Err("debug ownership admission contract missing".into());
        }
        let rules = spec
            .pointer("/matchConstraints/resourceRules")
            .and_then(Value::as_array)
            .ok_or("admission resource rules missing")?;
        for (group, resource) in [
            ("", "secrets"),
            ("", "services"),
            ("batch", "jobs"),
            ("networking.k8s.io", "networkpolicies"),
            ("rbac.authorization.k8s.io", "roles"),
            ("rbac.authorization.k8s.io", "rolebindings"),
        ] {
            if !rules.iter().any(|r| {
                contains(&r["apiGroups"], group)
                    && contains(&r["resources"], resource)
                    && ["CREATE", "UPDATE", "DELETE"]
                        .iter()
                        .all(|op| contains(&r["operations"], op))
            }) {
                return Err("debug admission does not protect all resources".into());
            }
        }
        let variables = spec["variables"]
            .as_array()
            .ok_or("admission ownership variables missing")?;
        let worker = "system:serviceaccount:thalamus-system:thalamus-worker";
        if !variables.iter().any(|v| {
            v["name"] == "worker" && v["expression"].as_str().is_some_and(|e| e.contains(worker))
        }) || !variables.iter().any(|v| v["name"] == "newReserved")
            || !variables.iter().any(|v| v["name"] == "oldReserved")
        {
            return Err("debug admission worker binding missing".into());
        }
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk(
                "admissionregistration.k8s.io",
                "v1",
                "ValidatingAdmissionPolicyBinding",
            ),
            "validatingadmissionpolicybindings",
        );
        let bindings: Api<DynamicObject> = Api::all_with(self.client.clone(), &resource);
        let binding = bindings.get(ADMISSION_NAME).await?;
        if binding.metadata.deletion_timestamp.is_some()
            || binding.data.pointer("/spec/policyName") != Some(&json!(ADMISSION_NAME))
            || !binding
                .data
                .pointer("/spec/validationActions")
                .is_some_and(|v| contains(v, "Deny"))
        {
            return Err("debug admission binding missing".into());
        }
        let rbac_name = "live-debug-rbac.thalamus.dev";
        let rbac = policies.get(rbac_name).await?;
        let rbac_spec = rbac
            .data
            .get("spec")
            .ok_or("debug RBAC admission missing")?;
        let rbac_binding = bindings.get(rbac_name).await?;
        if rbac.metadata.deletion_timestamp.is_some()
            || rbac_spec["failurePolicy"] != "Fail"
            || rbac_spec.pointer(
                "/matchConstraints/namespaceSelector/matchLabels/thalamus.dev~1live-admission",
            ) != Some(&json!("enforced"))
            || rbac_spec["validations"]
                .as_array()
                .is_none_or(Vec::is_empty)
            || rbac_spec["variables"].as_array().is_none_or(|variables| {
                !["worker", "metadataValid", "roleValid", "bindingValid"]
                    .iter()
                    .all(|name| variables.iter().any(|v| v["name"] == *name))
            })
            || rbac_binding.metadata.deletion_timestamp.is_some()
            || rbac_binding.data.pointer("/spec/policyName") != Some(&json!(rbac_name))
            || !rbac_binding
                .data
                .pointer("/spec/validationActions")
                .is_some_and(|v| contains(v, "Deny"))
        {
            return Err("debug RBAC admission contract missing".into());
        }
        let jobs_name = "live-worker-jobs.thalamus.dev";
        let jobs_policy = policies.get(jobs_name).await?;
        let jobs_spec = jobs_policy
            .data
            .get("spec")
            .ok_or("worker Job admission missing")?;
        let jobs_binding = bindings.get(jobs_name).await?;
        let jobs_rules = jobs_spec
            .pointer("/matchConstraints/resourceRules")
            .and_then(Value::as_array)
            .ok_or("worker Job admission rules missing")?;
        if jobs_policy.metadata.deletion_timestamp.is_some()
            || jobs_spec["failurePolicy"] != "Fail"
            || jobs_spec.pointer(
                "/matchConstraints/namespaceSelector/matchLabels/thalamus.dev~1live-admission",
            ) != Some(&json!("enforced"))
            || jobs_spec["validations"]
                .as_array()
                .is_none_or(Vec::is_empty)
            || !jobs_rules.iter().any(|rule| {
                contains(&rule["apiGroups"], "batch")
                    && contains(&rule["resources"], "jobs")
                    && contains(&rule["operations"], "CREATE")
                    && contains(&rule["operations"], "UPDATE")
            })
            || jobs_binding.metadata.deletion_timestamp.is_some()
            || jobs_binding.data.pointer("/spec/policyName") != Some(&json!(jobs_name))
            || !jobs_binding
                .data
                .pointer("/spec/validationActions")
                .is_some_and(|value| contains(value, "Deny"))
        {
            return Err("worker Job admission contract missing".into());
        }
        self.verify_pod_admission().await?;
        Ok(())
    }

    async fn verify_pod_admission(&self) -> Result<()> {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        let namespace = namespaces.get(&self.namespace).await?;
        if namespace.metadata.deletion_timestamp.is_some()
            || namespace
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("thalamus.dev/live-admission"))
                .map(String::as_str)
                != Some("enforced")
        {
            return Err("debug Pod admission namespace is not enabled".into());
        }
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk(
                "admissionregistration.k8s.io",
                "v1",
                "ValidatingAdmissionPolicy",
            ),
            "validatingadmissionpolicies",
        );
        let policies: Api<DynamicObject> = Api::all_with(self.client.clone(), &resource);
        let name = "live-debug-pods.thalamus.dev";
        let policy = policies.get(name).await?;
        let spec = policy
            .data
            .get("spec")
            .ok_or("debug Pod admission spec missing")?;
        let rules = spec
            .pointer("/matchConstraints/resourceRules")
            .and_then(Value::as_array)
            .ok_or("debug Pod admission rules missing")?;
        let worker = "system:serviceaccount:thalamus-system:thalamus-worker";
        if policy.metadata.deletion_timestamp.is_some()
            || spec["failurePolicy"] != "Fail"
            || spec.pointer(
                "/matchConstraints/namespaceSelector/matchLabels/thalamus.dev~1live-admission",
            ) != Some(&json!("enforced"))
            || spec["validations"].as_array().is_none_or(Vec::is_empty)
            || !rules.iter().any(|rule| {
                contains(&rule["apiGroups"], "")
                    && contains(&rule["resources"], "pods")
                    && contains(&rule["operations"], "DELETE")
            })
            || !spec["variables"].as_array().is_some_and(|vars| {
                vars.iter().any(|v| {
                    v["name"] == "worker"
                        && v["expression"]
                            .as_str()
                            .is_some_and(|expr| expr.contains(worker))
                })
            })
        {
            return Err("debug Pod admission policy missing".into());
        }
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk(
                "admissionregistration.k8s.io",
                "v1",
                "ValidatingAdmissionPolicyBinding",
            ),
            "validatingadmissionpolicybindings",
        );
        let bindings: Api<DynamicObject> = Api::all_with(self.client.clone(), &resource);
        let binding = bindings.get(name).await?;
        if binding.metadata.deletion_timestamp.is_some()
            || binding.data.pointer("/spec/policyName") != Some(&json!(name))
            || !binding
                .data
                .pointer("/spec/validationActions")
                .is_some_and(|value| contains(value, "Deny"))
        {
            return Err("debug Pod admission binding missing".into());
        }
        Ok(())
    }
}

fn contains(value: &Value, text: &str) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().any(|v| v == text))
}

fn labels(id: Uuid) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "thalamus-debug".into()),
        ("app.kubernetes.io/managed-by".into(), "thalamus".into()),
        (SESSION_LABEL.into(), id.to_string()),
    ])
}
fn resource_owned(meta: &ObjectMeta, id: Uuid, nonce: Uuid, namespace: &str, name: &str) -> bool {
    !nonce.is_nil()
        && meta.name.as_deref() == Some(name)
        && meta.namespace.as_deref() == Some(namespace)
        && meta
            .labels
            .as_ref()
            .is_some_and(|actual| labels(id).iter().all(|(k, v)| actual.get(k) == Some(v)))
        && meta
            .annotations
            .as_ref()
            .and_then(|a| a.get(OWNERSHIP))
            .is_some_and(|v| v == &nonce.to_string())
        && meta.uid.as_deref().is_some_and(|v| !v.is_empty())
        && meta
            .resource_version
            .as_deref()
            .is_some_and(|v| !v.is_empty())
}
fn secret_owned(
    secret: &Secret,
    id: Uuid,
    nonce: Uuid,
    namespace: &str,
    name: &str,
    deadline: chrono::DateTime<Utc>,
) -> bool {
    resource_owned(&secret.metadata, id, nonce, namespace, name)
        && secret.type_.as_deref() == Some("Opaque")
        && secret.immutable == Some(true)
        && secret
            .metadata
            .owner_references
            .as_ref()
            .is_none_or(Vec::is_empty)
        && secret
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(EXPIRY))
            .and_then(|v| v.parse::<chrono::DateTime<Utc>>().ok())
            == Some(deadline)
}
fn child_owned(meta: &ObjectMeta, session: &WorkerSessionRef, expected_uid: Option<&str>) -> bool {
    resource_owned(
        meta,
        session.id,
        session.ownership_nonce,
        &session.job_namespace,
        &session.job_name,
    ) && meta.owner_references.as_ref().is_some_and(|refs| {
        refs.len() == 1 && {
            let r = &refs[0];
            r.api_version == "batch/v1"
                && r.kind == "Job"
                && r.name == session.job_name
                && !r.uid.is_empty()
                && expected_uid.is_none_or(|uid| r.uid == uid)
                && r.controller == Some(false)
                && r.block_owner_deletion == Some(false)
        }
    })
}
async fn delete_exact<K>(
    api: &Api<K>,
    name: &str,
    metadata: &ObjectMeta,
    foreground: bool,
) -> Result<()>
where
    K: kube::Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    if metadata.deletion_timestamp.is_some() {
        return Ok(());
    }
    let params = DeleteParams {
        propagation_policy: foreground.then_some(PropagationPolicy::Foreground),
        preconditions: Some(Preconditions {
            uid: Some(
                metadata
                    .uid
                    .clone()
                    .filter(|v| !v.is_empty())
                    .ok_or("resource UID missing")?,
            ),
            resource_version: Some(
                metadata
                    .resource_version
                    .clone()
                    .filter(|v| !v.is_empty())
                    .ok_or("resource version missing")?,
            ),
        }),
        ..Default::default()
    };
    match api.delete(name, &params).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(error.into()),
    }
}
fn selector_matches(selector: &LabelSelector, labels: &BTreeMap<String, String>) -> Result<bool> {
    if selector
        .match_labels
        .as_ref()
        .is_some_and(|m| m.iter().any(|(k, v)| labels.get(k) != Some(v)))
    {
        return Ok(false);
    }
    for expression in selector.match_expressions.as_deref().unwrap_or_default() {
        let value = labels.get(&expression.key);
        let values = expression.values.as_deref().unwrap_or_default();
        let matches = match expression.operator.as_str() {
            "In" if !values.is_empty() => value.is_some_and(|v| values.contains(v)),
            "NotIn" if !values.is_empty() => value.is_none_or(|v| !values.contains(v)),
            "Exists" if values.is_empty() => value.is_some(),
            "DoesNotExist" if values.is_empty() => value.is_none(),
            _ => return Err("invalid NetworkPolicy pod selector".into()),
        };
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}
fn rules_subset<T: serde::Serialize>(
    actual: &Option<Vec<T>>,
    wanted: &Option<Vec<T>>,
) -> Result<bool> {
    let expected: Vec<Value> = wanted
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()?;
    for rule in actual.as_deref().unwrap_or_default() {
        if !expected.contains(&serde_json::to_value(rule)?) {
            return Ok(false);
        }
    }
    Ok(true)
}
async fn gateway_rejects(
    client: &reqwest::Client,
    url: &reqwest::Url,
    model: &str,
    key: &str,
) -> Result<bool> {
    // A correctly authorized model request with no messages must fail backend
    // validation without invoking inference or spending tokens.
    let host = url.host_str().ok_or("gateway hostname missing")?;
    let mut response = client
        .post(url.clone())
        .header(reqwest::header::HOST, host)
        .bearer_auth(key)
        .json(&json!({"model":model,"messages":[]}))
        .send()
        .await?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 4096 {
            return Err("gateway probe response too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED && bytes.as_slice() == b"authentication failed" {
        return Ok(true);
    }
    if status == reqwest::StatusCode::BAD_REQUEST
        || status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
    {
        return Ok(false);
    }
    Err("ambiguous gateway authentication probe".into())
}
