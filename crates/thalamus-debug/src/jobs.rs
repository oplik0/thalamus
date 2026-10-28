//! Construction and admission-gated creation of debug-session workloads.
//!
//! The configuration and [`DebugLaunchAuthority`] are **trusted control-plane
//! inputs**, not request data. There is deliberately no default authority: a
//! deployment without the documented enforcement and credential issuer must
//! pass `None` and cannot launch a workload. Rendering is not authorization.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::IpAddr,
    pin::Pin,
};

use chrono::{DateTime, Utc};
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Service, ServiceAccount},
    networking::v1::NetworkPolicy,
};
use kube::{
    api::{DeleteParams, Patch, PatchParams, PostParams, Preconditions},
    core::{ApiResource, DynamicObject, GroupVersionKind},
    Api, Client,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use url::{Host, Url};
use uuid::Uuid;

const SESSION_LABEL: &str = "thalamus.dev/debug-session";
const LAUNCH_ANNOTATION: &str = "thalamus.dev/launch-state";
const PORT: u16 = 4096;
const MAX_DURATION_SECONDS: i64 = 24 * 60 * 60;
const MAX_TTL_SECONDS: i32 = 7 * 24 * 60 * 60;

/// An administrator-controlled HTTPS endpoint and the exact network destination
/// to which its hostname resolves. No DNS egress is granted to the workload.
/// For Kubernetes service addresses, the CNI must enforce the specified egress
/// restriction correctly after service address translation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedEndpoint {
    pub url: String,
    pub address: IpAddr,
}

/// Trusted deployment configuration; never populate this directly from a user
/// request. Both MCP URLs must have the same HTTPS origin and destination: they
/// are two paths on the authenticated, budget-enforcing Thalamus mediator,
/// *not* a direct connection to the Flux MCP server.
///
/// The pinned images must be audited for this use. In particular, they must
/// contain no extra credentials, OpenCode plugins, agents or managed settings;
/// the runtime image must provide `sh`, `date`, GNU `timeout`, `opencode` and
/// its `@ai-sdk/openai-compatible` provider without downloading dependencies.
/// Its pinned OpenCode version must support the generated provider/MCP and
/// file-substitution configuration, deny built-in tools as configured, and
/// run with a read-only project tree. Those properties require image QA.
/// The checkout image must provide `sh`, `date`, GNU `timeout`, `git`, `find`
/// and `rm`. Only anonymous HTTPS Git checkout is supported.
/// Images and deployment logging must not print resolved model/MCP credentials,
/// authorization headers or the resolved OpenCode config.
#[derive(Clone, Debug)]
pub struct DebugJobConfig {
    pub namespace: String,
    pub service_account: String,
    pub opencode_image: String,
    pub checkout_image: String,
    pub git_repository: TrustedEndpoint,
    pub model_gateway: TrustedEndpoint,
    pub thalamus_mcp: TrustedEndpoint,
    pub flux_mcp: TrustedEndpoint,
    pub proxy_namespace: String,
    /// All labels must match the trusted control-plane proxy pods.
    pub proxy_pod_labels: BTreeMap<String, String>,
    pub max_duration_seconds: i64,
    pub ttl_seconds_after_finished: i32,
    pub max_model_tokens: u64,
    pub max_tool_calls: u32,
}

/// Per-session inputs. Namespace and model selection are still subject to the
/// trusted configuration and authority; no image, URL or Secret is supplied by
/// the caller. `created_at` must be the persisted session creation time so that
/// retries construct the same active deadline.
#[derive(Clone, Debug)]
pub struct DebugJobRequest {
    pub session_id: Uuid,
    pub namespace: String,
    pub git_revision: String,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
    pub model_token_budget: u64,
    pub tool_call_budget: u32,
}

/// Result of the trusted authority's verification, not user-supplied evidence.
/// The opaque IDs identify actual, independently verifiable issuer and budget
/// records, not labels on the Kubernetes Job. The authority must check their
/// state on *every* launch/retry; this type itself does not enforce budgets.
#[derive(Clone, Debug)]
pub struct DebugAdmission {
    pub session_id: Uuid,
    pub namespace: String,
    pub git_revision: String,
    pub model: String,
    pub deadline: DateTime<Utc>,
    pub model_token_budget: u64,
    pub tool_call_budget: u32,
    pub model_budget_lease_id: Uuid,
    pub tool_budget_lease_id: Uuid,
    pub model_credential_id: Uuid,
    pub mcp_credential_id: Uuid,
    /// Expiry verified against both the model credential and the MCP credential.
    pub credentials_expire_at: DateTime<Utc>,
}

/// Trusted, concrete integration boundary. The calling application must
/// authenticate and authorize the actor for this session and bind that actor
/// into the authority's context; `DebugJobRequest` is not an identity assertion.
/// Implementations must verify the identity and membership of that actor and
/// provision and verify the three per-session Secrets named below. Provisioning
/// must be idempotent and return the same lease and credential IDs on retry.
/// The caller/issuer must revoke and clean up credentials after failed starts,
/// expiry or session termination. The authority must also verify:
///
/// * The model key was issued by Thalamus for this user, session, model and
///   token budget, is short-lived, and the selected gateway actively enforces
///   that budget (including its failure policy and accounting semantics).
/// * The MCP credential acts as that user, is bound to this session, and both
///   MCP paths pass **every** `tools/call` through the same durable atomic
///   session tool counter before dispatch. Flux has read-only authorization.
/// * The control-plane proxy authenticates users and only forwards safe session,
///   message and event operations: it must reject shell/command, file, find,
///   config, provider, auth and MCP registration APIs, arbitrary directory,
///   agent, model, system prompt and tool overrides.
/// * The service account has only read access in this service's namespace.
///   The CNI actually enforces these NetworkPolicies (including service DNAT);
///   no overlapping policy grants broader ingress or egress. Namespace RBAC
///   prevents untrusted pods from impersonating the selector or mutating/deleting
///   the resources and Secret contents. Admission must not unsuspend the staged
///   Job or inject containers, credentials, altered security settings or other
///   bypasses into its template or resulting pods. The Git endpoint is anonymous
///   and does not require forge auth.
///
/// Secret values used for file-based OpenCode `{env:...}` substitution must be
/// JSON-safe ASCII credentials; the server password must be high entropy and
/// unique per session. Never log or return Secret values. An implementation
/// that merely echoes request fields, labels, booleans or lease IDs does *not*
/// satisfy this contract. No production implementation is provided here.
pub trait DebugLaunchAuthority: Send + Sync {
    fn verify<'a>(
        &'a self,
        config: &'a DebugJobConfig,
        request: &'a DebugJobRequest,
    ) -> Pin<Box<dyn Future<Output = Result<DebugAdmission, DebugJobError>> + Send + 'a>>;

    /// After the session policy and Service have been created, prove that the
    /// actual cluster/CNI will enforce this policy before any workload packet
    /// can flow, including on pod creation and across service DNAT. Verify no
    /// broader policy selects these pods and that the listed resources and
    /// admission rules remain protected from untrusted mutation. If the CNI
    /// cannot provide this guarantee, return an error rather than guessing.
    fn verify_isolation<'a>(
        &'a self,
        config: &'a DebugJobConfig,
        request: &'a DebugJobRequest,
        job: &'a Job,
        policy: &'a NetworkPolicy,
        service: &'a Service,
    ) -> Pin<Box<dyn Future<Output = Result<(), DebugJobError>> + Send + 'a>>;
}

#[derive(Debug, Error)]
pub enum DebugJobError {
    #[error("invalid debug job configuration or request: {0}")]
    Invalid(&'static str),
    #[error("trusted debug launch admission is unavailable")]
    AdmissionUnavailable,
    #[error("debug launch admission denied or does not bind this session")]
    AdmissionDenied,
    #[error("conflicting debug {0} already exists")]
    Conflict(&'static str),
    #[error("debug workload service account does not exist")]
    ServiceAccountMissing,
    #[error("Kubernetes debug workload operation failed")]
    Kubernetes(#[source] kube::Error),
    #[error("debug workload serialization failed")]
    Serialization(#[source] serde_json::Error),
}

impl From<kube::Error> for DebugJobError {
    fn from(error: kube::Error) -> Self {
        Self::Kubernetes(error)
    }
}

impl From<serde_json::Error> for DebugJobError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

/// The Job, Service and NetworkPolicy use the same deterministic name.
pub fn debug_job_name(session_id: Uuid) -> String {
    format!("thalamus-debug-{session_id}")
}

/// The Secret must contain a random `password` key for Basic user `opencode`.
pub fn debug_server_secret_name(session_id: Uuid) -> String {
    format!("{}-server", debug_job_name(session_id))
}

/// The session-scoped model credential is in Secret key `key`.
pub fn debug_model_secret_name(session_id: Uuid) -> String {
    format!("{}-model", debug_job_name(session_id))
}

/// The user/session-scoped MCP credential is in Secret key `token`.
pub fn debug_mcp_secret_name(session_id: Uuid) -> String {
    format!("{}-mcp", debug_job_name(session_id))
}

fn dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.bytes()
            .last()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

fn dns_subdomain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.split('.').all(dns_label)
}

fn label_key(s: &str) -> bool {
    fn part(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 63
            && s.bytes().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.bytes().last().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    }
    match s.split_once('/') {
        Some((prefix, name)) => dns_subdomain(prefix) && part(name),
        None => part(s),
    }
}

fn label_value(s: &str) -> bool {
    s.is_empty()
        || (s.len() <= 63
            && s.bytes().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.bytes().last().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)))
}

fn pinned_image(s: &str) -> bool {
    let Some((name, digest)) = s.split_once("@sha256:") else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/.:_-".contains(&c))
        && digest.len() == 64
        && digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn endpoint(endpoint: &TrustedEndpoint) -> Result<Url, DebugJobError> {
    if endpoint
        .url
        .bytes()
        .any(|c| c.is_ascii_control() || c.is_ascii_whitespace() || c == b'{' || c == b'}')
    {
        return Err(DebugJobError::Invalid("invalid endpoint URL"));
    }
    let url =
        Url::parse(&endpoint.url).map_err(|_| DebugJobError::Invalid("invalid endpoint URL"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
        || url.port_or_known_default().is_none()
    {
        return Err(DebugJobError::Invalid(
            "endpoints must be credential-free HTTPS URLs",
        ));
    }
    let ip = endpoint.address;
    let unsafe_address = match ip {
        IpAddr::V4(ip) => {
            ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || ip.is_link_local()
                || ip.is_broadcast()
        }
        IpAddr::V6(ip) => {
            ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || ip.is_unicast_link_local()
        }
    };
    if unsafe_address {
        return Err(DebugJobError::Invalid("unsafe endpoint address"));
    }
    match url.host() {
        Some(Host::Domain(host)) if dns_subdomain(host) => {}
        Some(Host::Ipv4(host)) if IpAddr::V4(host) == ip => {}
        Some(Host::Ipv6(host)) if IpAddr::V6(host) == ip => {}
        _ => {
            return Err(DebugJobError::Invalid(
                "endpoint hostname or address mismatch",
            ))
        }
    }
    Ok(url)
}

fn validate(
    config: &DebugJobConfig,
    request: &DebugJobRequest,
    now: DateTime<Utc>,
) -> Result<(), DebugJobError> {
    if !dns_label(&config.namespace)
        || request.namespace != config.namespace
        || !dns_subdomain(&config.service_account)
        || !dns_label(&config.proxy_namespace)
    {
        return Err(DebugJobError::Invalid("namespace or service account"));
    }
    if config.proxy_pod_labels.is_empty()
        || config
            .proxy_pod_labels
            .iter()
            .any(|(k, v)| !label_key(k) || !label_value(v))
    {
        return Err(DebugJobError::Invalid("proxy pod selector"));
    }
    if !pinned_image(&config.opencode_image) || !pinned_image(&config.checkout_image) {
        return Err(DebugJobError::Invalid(
            "images must be pinned by SHA-256 digest",
        ));
    }
    if request.session_id.is_nil()
        || !(request.git_revision.len() == 40 || request.git_revision.len() == 64)
        || !request
            .git_revision
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(DebugJobError::Invalid("session or full Git commit"));
    }
    if request.model.is_empty()
        || request.model.len() > 200
        || !request
            .model
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
    {
        return Err(DebugJobError::Invalid("model identifier"));
    }
    let duration = request
        .deadline
        .timestamp()
        .checked_sub(request.created_at.timestamp())
        .ok_or(DebugJobError::Invalid("deadline"))?;
    if config.max_duration_seconds <= 0
        || config.max_duration_seconds > MAX_DURATION_SECONDS
        || duration <= 0
        || duration > config.max_duration_seconds
        || request.created_at > now
        || request.deadline <= now
        || request.deadline.timestamp() <= now.timestamp()
    {
        return Err(DebugJobError::Invalid("deadline"));
    }
    if !(60..=MAX_TTL_SECONDS).contains(&config.ttl_seconds_after_finished)
        || config.max_model_tokens == 0
        || config.max_tool_calls == 0
        || request.model_token_budget == 0
        || request.model_token_budget > config.max_model_tokens
        || request.tool_call_budget == 0
        || request.tool_call_budget > config.max_tool_calls
    {
        return Err(DebugJobError::Invalid("TTL or budgets"));
    }
    let git = endpoint(&config.git_repository)?;
    let model = endpoint(&config.model_gateway)?;
    let thalamus = endpoint(&config.thalamus_mcp)?;
    let flux = endpoint(&config.flux_mcp)?;
    if thalamus.origin() != flux.origin()
        || config.thalamus_mcp.address != config.flux_mcp.address
        || thalamus.path() == flux.path()
    {
        return Err(DebugJobError::Invalid(
            "MCP endpoints must be distinct paths on the same mediator",
        ));
    }
    let mut hosts = BTreeMap::new();
    for (url, address) in [
        (&git, config.git_repository.address),
        (&model, config.model_gateway.address),
        (&thalamus, config.thalamus_mcp.address),
        (&flux, config.flux_mcp.address),
    ] {
        if let Some(Host::Domain(host)) = url.host() {
            if hosts
                .insert(host.to_owned(), address)
                .is_some_and(|previous| previous != address)
            {
                return Err(DebugJobError::Invalid("inconsistent endpoint addresses"));
            }
        }
    }
    Ok(())
}

fn validate_admission(
    request: &DebugJobRequest,
    admission: &DebugAdmission,
) -> Result<(), DebugJobError> {
    if admission.session_id != request.session_id
        || admission.namespace != request.namespace
        || admission.git_revision != request.git_revision
        || admission.model != request.model
        || admission.deadline != request.deadline
        || admission.model_token_budget != request.model_token_budget
        || admission.tool_call_budget != request.tool_call_budget
        || admission.model_budget_lease_id.is_nil()
        || admission.tool_budget_lease_id.is_nil()
        || admission.model_credential_id.is_nil()
        || admission.mcp_credential_id.is_nil()
        || admission.credentials_expire_at < request.deadline
        || admission
            .credentials_expire_at
            .timestamp()
            .saturating_sub(request.deadline.timestamp())
            > 60
    {
        return Err(DebugJobError::AdmissionDenied);
    }
    Ok(())
}

fn labels(session_id: Uuid) -> Value {
    json!({"app.kubernetes.io/name":"thalamus-debug", "app.kubernetes.io/managed-by":"thalamus", SESSION_LABEL:session_id.to_string()})
}

fn secret_env(name: &str, secret: String, key: &str) -> Value {
    json!({"name":name,"valueFrom":{"secretKeyRef":{"name":secret,"key":key,"optional":false}}})
}

fn endpoint_aliases(config: &DebugJobConfig) -> Result<Vec<Value>, DebugJobError> {
    let mut aliases: BTreeMap<IpAddr, BTreeSet<String>> = BTreeMap::new();
    for item in [
        &config.git_repository,
        &config.model_gateway,
        &config.thalamus_mcp,
        &config.flux_mcp,
    ] {
        if let Some(Host::Domain(host)) = endpoint(item)?.host() {
            aliases
                .entry(item.address)
                .or_default()
                .insert(host.to_owned());
        }
    }
    Ok(aliases
        .into_iter()
        .map(|(ip, hosts)| json!({"ip":ip.to_string(),"hostnames":hosts}))
        .collect())
}

const CHECKOUT_SCRIPT: &str = r#"set -eu
remaining=$(($1 - $(date -u +%s)))
[ "$remaining" -gt 0 ] || exit 1
exec timeout -s KILL "${remaining}s" /bin/sh -ceu '
  git -c credential.helper= -c credential.interactive=never -c http.followRedirects=false -c protocol.file.allow=never -c protocol.ext.allow=never -c core.hooksPath=/dev/null clone --no-checkout --no-tags -- "$1" /workspace/repo
  [ "$(git -C /workspace/repo rev-parse --verify "$2^{commit}")" = "$2" ]
  git -C /workspace/repo -c core.hooksPath=/dev/null checkout --detach "$2" --
  find /workspace/repo -depth \( -name .opencode -o -name opencode.json -o -name opencode.jsonc \) -exec rm -rf -- {} +
' checkout "$2" "$3""#;

const SERVE_SCRIPT: &str = r#"set -eu
remaining=$(($1 - $(date -u +%s)))
[ "$remaining" -gt 0 ] || exit 1
umask 077
printf '%s' "$THALAMUS_OPENCODE_CONFIG" > /home/opencode/config.json
unset THALAMUS_OPENCODE_CONFIG
export OPENCODE_CONFIG=/home/opencode/config.json
exec timeout -s KILL "${remaining}s" opencode serve --hostname 0.0.0.0 --port 4096"#;

/// Render a *suspended* Job from trusted configuration and an already verified
/// admission. This is useful for inspection/tests; rendering is not permission
/// to run. Use [`create_debug_job`] to perform admission and safe creation.
pub fn render_debug_job(
    config: &DebugJobConfig,
    request: &DebugJobRequest,
    admission: &DebugAdmission,
) -> Result<Job, DebugJobError> {
    validate(config, request, Utc::now())?;
    validate_admission(request, admission)?;
    let name = debug_job_name(request.session_id);
    let model = &request.model;
    let opencode_config = json!({
        "autoupdate":false,
        "enabled_providers":["thalamus"],
        "model":format!("thalamus/{model}"),
        "provider":{"thalamus":{
            "npm":"@ai-sdk/openai-compatible", "name":"Thalamus",
            "options":{"baseURL":config.model_gateway.url, "apiKey":"{env:DEBUG_MODEL_KEY}"},
            "models":{model:{"name":model}}
        }},
        "mcp":{
            "thalamus":{"type":"remote","enabled":true,"oauth":false,"url":config.thalamus_mcp.url,
                "headers":{"Authorization":"Bearer {env:DEBUG_MCP_TOKEN}"}},
            "flux":{"type":"remote","enabled":true,"oauth":false,"url":config.flux_mcp.url,
                "headers":{"Authorization":"Bearer {env:DEBUG_MCP_TOKEN}"}}
        },
        "permission":{"*":"deny","thalamus_*":"allow","flux_*":"allow"},
        "lsp":false,
        "formatter":false
    })
    .to_string();
    let hardening = json!({
        "allowPrivilegeEscalation":false,"privileged":false,"readOnlyRootFilesystem":true,
        "capabilities":{"drop":["ALL"]},"runAsNonRoot":true,"runAsUser":10001,"runAsGroup":10001,
        "seccompProfile":{"type":"RuntimeDefault"}
    });
    let deadline = request.deadline.timestamp().to_string();
    let checkout = json!({
        "name":"checkout","image":config.checkout_image,"imagePullPolicy":"IfNotPresent",
        "workingDir":"/tmp","command":["/bin/sh","-ceu",CHECKOUT_SCRIPT,"checkout",deadline,config.git_repository.url,request.git_revision],
        "env":[{"name":"HOME","value":"/tmp"},{"name":"GIT_CONFIG_GLOBAL","value":"/dev/null"},
            {"name":"GIT_CONFIG_NOSYSTEM","value":"1"},{"name":"GIT_TERMINAL_PROMPT","value":"0"},
            {"name":"GIT_ASKPASS","value":"/bin/false"},{"name":"SSH_ASKPASS","value":"/bin/false"}],
        "resources":{"requests":{"cpu":"100m","memory":"128Mi","ephemeral-storage":"256Mi"},
            "limits":{"cpu":"1","memory":"1Gi","ephemeral-storage":"2Gi"}},
        "securityContext":hardening,
        "volumeMounts":[{"name":"workspace","mountPath":"/workspace"},{"name":"tmp","mountPath":"/tmp"}]
    });
    let opencode = json!({
        "name":"opencode","image":config.opencode_image,"imagePullPolicy":"IfNotPresent",
        "workingDir":"/workspace/repo","command":["/bin/sh","-ceu",SERVE_SCRIPT,"serve",deadline],
        "ports":[{"name":"http","containerPort":PORT,"protocol":"TCP"}],
        "env":[{"name":"HOME","value":"/home/opencode"},
            {"name":"OPENCODE_SERVER_USERNAME","value":"opencode"},
            secret_env("OPENCODE_SERVER_PASSWORD",debug_server_secret_name(request.session_id),"password"),
            secret_env("DEBUG_MODEL_KEY",debug_model_secret_name(request.session_id),"key"),
            secret_env("DEBUG_MCP_TOKEN",debug_mcp_secret_name(request.session_id),"token"),
            {"name":"THALAMUS_OPENCODE_CONFIG","value":opencode_config},
            {"name":"OPENCODE_DISABLE_AUTOUPDATE","value":"true"},
            {"name":"OPENCODE_DISABLE_MODELS_FETCH","value":"true"}],
        "resources":{"requests":{"cpu":"250m","memory":"512Mi","ephemeral-storage":"256Mi"},
            "limits":{"cpu":"2","memory":"2Gi","ephemeral-storage":"2Gi"}},
        "securityContext":hardening,
        "volumeMounts":[{"name":"workspace","mountPath":"/workspace","readOnly":true},
            {"name":"tmp","mountPath":"/tmp"},{"name":"home","mountPath":"/home/opencode"}]
    });
    let mut pod_spec = json!({
        "restartPolicy":"Never","serviceAccountName":config.service_account,
        "automountServiceAccountToken":false,"enableServiceLinks":false,
        "hostNetwork":false,"hostPID":false,"hostIPC":false,
        "terminationGracePeriodSeconds":0,"dnsPolicy":"None",
        "dnsConfig":{"nameservers":["127.0.0.1"]},"hostAliases":endpoint_aliases(config)?,
        "securityContext":{"runAsNonRoot":true,"runAsUser":10001,"runAsGroup":10001,
            "fsGroup":10001,"seccompProfile":{"type":"RuntimeDefault"}},
        "initContainers":[],"containers":[],
        "volumes":[{"name":"workspace","emptyDir":{"sizeLimit":"2Gi"}},
            {"name":"tmp","emptyDir":{"sizeLimit":"1Gi"}},{"name":"home","emptyDir":{"sizeLimit":"512Mi"}}]
    });
    pod_spec["initContainers"] = Value::Array(vec![checkout]);
    pod_spec["containers"] = Value::Array(vec![opencode]);
    let mut job = json!({
        "apiVersion":"batch/v1","kind":"Job",
        "metadata":{"name":name,"namespace":config.namespace,"labels":labels(request.session_id),
            "annotations":{LAUNCH_ANNOTATION:"staging",
                "thalamus.dev/model-token-budget":request.model_token_budget.to_string(),
                "thalamus.dev/tool-call-budget":request.tool_call_budget.to_string(),
                "thalamus.dev/model-budget-lease":admission.model_budget_lease_id.to_string(),
                "thalamus.dev/tool-budget-lease":admission.tool_budget_lease_id.to_string(),
                "thalamus.dev/model-credential":admission.model_credential_id.to_string(),
                "thalamus.dev/mcp-credential":admission.mcp_credential_id.to_string(),
                "thalamus.dev/credentials-expire-at":admission.credentials_expire_at.to_rfc3339()}},
        "spec":{
            "suspend":true,"parallelism":1,"completions":1,"completionMode":"NonIndexed",
            "backoffLimit":0,"activeDeadlineSeconds":request.deadline.timestamp() - request.created_at.timestamp(),
            "ttlSecondsAfterFinished":config.ttl_seconds_after_finished,
            "template":{"metadata":{"labels":labels(request.session_id)},"spec":{}}
        }
    });
    job["spec"]["template"]["spec"] = pod_spec;
    let job: Job = serde_json::from_value(job)?;
    Ok(job)
}

fn owned_metadata(config: &DebugJobConfig, request: &DebugJobRequest, uid: &str) -> Value {
    json!({"name":debug_job_name(request.session_id),"namespace":config.namespace,"labels":labels(request.session_id),
        "ownerReferences":[{"apiVersion":"batch/v1","kind":"Job","name":debug_job_name(request.session_id),
            "uid":uid,"controller":false,"blockOwnerDeletion":false}]})
}

fn render_network_policy(
    config: &DebugJobConfig,
    request: &DebugJobRequest,
    uid: &str,
) -> Result<NetworkPolicy, DebugJobError> {
    let mut destinations = BTreeSet::new();
    for item in [
        &config.git_repository,
        &config.model_gateway,
        &config.thalamus_mcp,
        &config.flux_mcp,
    ] {
        let url = endpoint(item)?;
        destinations.insert((
            item.address,
            url.port_or_known_default()
                .ok_or(DebugJobError::Invalid("endpoint port"))?,
        ));
    }
    let egress: Vec<_> = destinations.into_iter().map(|(ip, port)| {
        let bits = if ip.is_ipv4() { 32 } else { 128 };
        json!({"to":[{"ipBlock":{"cidr":format!("{ip}/{bits}")}}],"ports":[{"protocol":"TCP","port":port}]})
    }).collect();
    Ok(serde_json::from_value(
        json!({"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy",
        "metadata":owned_metadata(config, request, uid),"spec":{
            "podSelector":{"matchLabels":{SESSION_LABEL:request.session_id.to_string()}},
            "policyTypes":["Ingress","Egress"],
            "ingress":[{"from":[{"namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":config.proxy_namespace}},
                "podSelector":{"matchLabels":config.proxy_pod_labels}}],"ports":[{"protocol":"TCP","port":PORT}]}],
            "egress":egress
        }}),
    )?)
}

fn render_service(
    config: &DebugJobConfig,
    request: &DebugJobRequest,
    uid: &str,
) -> Result<Service, DebugJobError> {
    Ok(serde_json::from_value(
        json!({"apiVersion":"v1","kind":"Service",
        "metadata":owned_metadata(config, request, uid),"spec":{"type":"ClusterIP","sessionAffinity":"None",
            "selector":{SESSION_LABEL:request.session_id.to_string()},
            "ports":[{"name":"http","port":PORT,"targetPort":PORT,"protocol":"TCP"}]}}),
    )?)
}

// Strict recursive comparison. Only explicitly recognized API-server defaults
// are ignored. Unknown additions (notably injected containers, volumes, env,
// selectors and security settings) cause a conflict rather than being adopted.
fn equivalent(expected: &Value, actual: &Value, path: &str, kind: &str) -> bool {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            e.iter().all(|(k, v)| match a.get(k) {
                Some(found) => equivalent(v, found, &format!("{path}/{k}"), kind),
                None => allowed_missing(path, k, v, kind),
            }) && a
                .iter()
                .all(|(k, v)| e.contains_key(k) || allowed_default(path, k, v, kind))
        }
        (Value::Array(e), Value::Array(a)) => {
            e.len() == a.len()
                && e.iter()
                    .zip(a)
                    .all(|(x, y)| equivalent(x, y, &format!("{path}/[]"), kind))
        }
        _ => expected == actual,
    }
}

fn allowed_missing(path: &str, key: &str, value: &Value, kind: &str) -> bool {
    // These are non-pointer Go bool fields with omitempty in the apiserver.
    kind == "job"
        && path == "/spec/template/spec"
        && matches!(key, "hostNetwork" | "hostPID" | "hostIPC")
        && value == false
}

fn allowed_default(path: &str, key: &str, value: &Value, kind: &str) -> bool {
    if kind == "service" && path == "/spec" {
        return match key {
            "clusterIP" => value
                .as_str()
                .and_then(|v| v.parse::<IpAddr>().ok())
                .is_some(),
            "clusterIPs" => value.as_array().is_some_and(|ips| {
                !ips.is_empty()
                    && ips.len() <= 2
                    && ips
                        .iter()
                        .all(|v| v.as_str().and_then(|s| s.parse::<IpAddr>().ok()).is_some())
            }),
            "ipFamilies" => value.as_array().is_some_and(|a| {
                !a.is_empty() && a.len() <= 2 && a.iter().all(|v| v == "IPv4" || v == "IPv6")
            }),
            "ipFamilyPolicy" => {
                value == "SingleStack" || value == "PreferDualStack" || value == "RequireDualStack"
            }
            "internalTrafficPolicy" => value == "Cluster",
            _ => false,
        };
    }
    if kind == "job" {
        if path == "/spec/template/metadata" && key == "creationTimestamp" {
            return value.is_null();
        }
        if path == "/spec" {
            return match key {
                "manualSelector" => value == false,
                "podReplacementPolicy" => value == "TerminatingOrFailed",
                _ => false,
            };
        }
        if path == "/spec/template/spec" {
            return match key {
                "schedulerName" => value == "default-scheduler",
                "priority" => value == 0,
                "preemptionPolicy" => value == "PreemptLowerPriority",
                "shareProcessNamespace" | "setHostnameAsFQDN" => value == false,
                // The deprecated field is checked separately against serviceAccountName.
                "serviceAccount" => value.is_string(),
                _ => false,
            };
        }
        if path.ends_with("/containers/[]") || path.ends_with("/initContainers/[]") {
            return match key {
                "terminationMessagePath" => value == "/dev/termination-log",
                "terminationMessagePolicy" => value == "File",
                "stdin" | "stdinOnce" | "tty" => value == false,
                _ => false,
            };
        }
        if path.ends_with("/emptyDir") && key == "medium" {
            return value == "";
        }
    }
    false
}

fn metadata_matches(expected: &kube::api::ObjectMeta, actual: &kube::api::ObjectMeta) -> bool {
    expected.name == actual.name
        && expected.namespace == actual.namespace
        && expected.labels.as_ref().is_some_and(|wanted| {
            actual
                .labels
                .as_ref()
                .is_some_and(|got| wanted.iter().all(|(k, v)| got.get(k) == Some(v)))
        })
        && expected.owner_references == actual.owner_references
        && actual.deletion_timestamp.is_none()
}

fn check_job(expected: &Job, actual: &DynamicObject) -> Result<bool, DebugJobError> {
    if !metadata_matches(&expected.metadata, &actual.metadata) {
        return Err(DebugJobError::Conflict("Job"));
    }
    let uid = actual
        .metadata
        .uid
        .as_deref()
        .filter(|u| !u.is_empty())
        .ok_or(DebugJobError::Conflict("Job"))?;
    let annotations = actual
        .metadata
        .annotations
        .as_ref()
        .ok_or(DebugJobError::Conflict("Job"))?;
    if !expected
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|expected_annotations| {
            expected_annotations
                .iter()
                .all(|(key, value)| key == LAUNCH_ANNOTATION || annotations.get(key) == Some(value))
        })
    {
        return Err(DebugJobError::Conflict("Job"));
    }
    let suspended = actual
        .data
        .pointer("/spec/suspend")
        .and_then(Value::as_bool)
        .ok_or(DebugJobError::Conflict("Job"))?;
    let state = annotations.get(LAUNCH_ANNOTATION).map(String::as_str);
    if !matches!(
        (suspended, state),
        (true, Some("staging")) | (false, Some("active"))
    ) {
        return Err(DebugJobError::Conflict("Job"));
    }
    let mut want = serde_json::to_value(expected)?;
    let mut got = serde_json::to_value(actual)?;
    want["spec"]["suspend"] = json!(suspended);
    let spec = got
        .get_mut("spec")
        .and_then(Value::as_object_mut)
        .ok_or(DebugJobError::Conflict("Job"))?;
    // An automatic selector must select this Job's own controller UID.
    let selector = spec
        .remove("selector")
        .ok_or(DebugJobError::Conflict("Job"))?;
    let matches = selector
        .get("matchLabels")
        .and_then(Value::as_object)
        .ok_or(DebugJobError::Conflict("Job"))?;
    if selector.as_object().is_none_or(|m| m.len() != 1)
        || matches.len() != 1
        || !(matches.get("batch.kubernetes.io/controller-uid") == Some(&json!(uid))
            || matches.get("controller-uid") == Some(&json!(uid)))
    {
        return Err(DebugJobError::Conflict("Job"));
    }
    let pod_labels = got
        .pointer_mut("/spec/template/metadata/labels")
        .and_then(Value::as_object_mut)
        .ok_or(DebugJobError::Conflict("Job"))?;
    for (key, expected_value) in [
        ("controller-uid", uid),
        ("batch.kubernetes.io/controller-uid", uid),
        (
            "job-name",
            expected.metadata.name.as_deref().unwrap_or_default(),
        ),
        (
            "batch.kubernetes.io/job-name",
            expected.metadata.name.as_deref().unwrap_or_default(),
        ),
    ] {
        if let Some(value) = pod_labels.remove(key) {
            if value != expected_value {
                return Err(DebugJobError::Conflict("Job"));
            }
        }
    }
    if let Some(service_account) = got.pointer("/spec/template/spec/serviceAccount") {
        if service_account != &want["spec"]["template"]["spec"]["serviceAccountName"] {
            return Err(DebugJobError::Conflict("Job"));
        }
    }
    if !equivalent(&want["spec"], &got["spec"], "/spec", "job") {
        return Err(DebugJobError::Conflict("Job"));
    }
    Ok(suspended)
}

fn check_service(expected: &Service, actual: &DynamicObject) -> Result<(), DebugJobError> {
    let wanted = serde_json::to_value(&expected.spec)?;
    if !metadata_matches(&expected.metadata, &actual.metadata)
        || !actual
            .data
            .get("spec")
            .is_some_and(|spec| equivalent(&wanted, spec, "/spec", "service"))
    {
        return Err(DebugJobError::Conflict("Service"));
    }
    Ok(())
}

fn check_policy(expected: &NetworkPolicy, actual: &DynamicObject) -> Result<(), DebugJobError> {
    let wanted = serde_json::to_value(&expected.spec)?;
    if !metadata_matches(&expected.metadata, &actual.metadata)
        || !actual
            .data
            .get("spec")
            .is_some_and(|spec| equivalent(&wanted, spec, "/spec", "network policy"))
    {
        return Err(DebugJobError::Conflict("NetworkPolicy"));
    }
    Ok(())
}

fn dynamic_api(
    client: &Client,
    namespace: &str,
    group: &str,
    kind: &str,
    plural: &str,
) -> Api<DynamicObject> {
    let gvk = GroupVersionKind::gvk(group, "v1", kind);
    let resource = ApiResource::from_gvk_with_plural(&gvk, plural);
    Api::namespaced_with(client.clone(), namespace, &resource)
}

fn as_dynamic<T: Serialize>(object: &T) -> Result<DynamicObject, DebugJobError> {
    Ok(serde_json::from_value(serde_json::to_value(object)?)?)
}

fn as_typed<T: DeserializeOwned>(object: &DynamicObject) -> Result<T, DebugJobError> {
    Ok(serde_json::from_value(serde_json::to_value(object)?)?)
}

fn require_authority(
    authority: Option<&dyn DebugLaunchAuthority>,
) -> Result<&dyn DebugLaunchAuthority, DebugJobError> {
    authority.ok_or(DebugJobError::AdmissionUnavailable)
}

/// Create or adopt this session's Job and its owned NetworkPolicy and Service.
/// A new Job is first created suspended, then its policy and service are
/// installed, and only then is it resumed with a resource-version precondition.
/// The per-session Service is `{job-name}.{namespace}.svc.cluster.local:4096`.
/// Existing resources are accepted only when their security-relevant specs and
/// ownership match, not merely when their names or labels match.
///
/// Passing `None` always fails before any Kubernetes write. A verifier must not
/// return `Ok` without the real, active enforcement described on the trait.
pub async fn create_debug_job(
    client: &Client,
    config: &DebugJobConfig,
    request: &DebugJobRequest,
    authority: Option<&dyn DebugLaunchAuthority>,
) -> Result<Job, DebugJobError> {
    validate(config, request, Utc::now())?;
    let authority = require_authority(authority)?;
    let service_accounts: Api<ServiceAccount> = Api::namespaced(client.clone(), &config.namespace);
    let account = service_accounts
        .get_opt(&config.service_account)
        .await?
        .ok_or(DebugJobError::ServiceAccountMissing)?;
    if account.metadata.deletion_timestamp.is_some() {
        return Err(DebugJobError::ServiceAccountMissing);
    }
    let admission = authority.verify(config, request).await?;
    validate_admission(request, &admission)?;
    let wanted = render_debug_job(config, request, &admission)?;
    let name = debug_job_name(request.session_id);
    let jobs = dynamic_api(client, &config.namespace, "batch", "Job", "jobs");
    let (job, created) = match jobs
        .create(&PostParams::default(), &as_dynamic(&wanted)?)
        .await
    {
        Ok(job) => (job, true),
        Err(kube::Error::Api(error)) if error.code == 409 => (jobs.get(&name).await?, false),
        Err(error) => return Err(error.into()),
    };
    let result = finish_launch(
        client, config, request, &admission, authority, &wanted, &job,
    )
    .await;
    if result.is_err() && created {
        // The controller may update a suspended Job's resourceVersion. Read it
        // again, and only delete the same still-staged Job with a precondition;
        // a concurrent successful retry or a replacement must not be deleted.
        if let Ok(latest) = jobs.get(&name).await {
            if latest.metadata.uid == job.metadata.uid
                && matches!(check_job(&wanted, &latest), Ok(true))
            {
                if let (Some(uid), Some(resource_version)) =
                    (&latest.metadata.uid, &latest.metadata.resource_version)
                {
                    let params = DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: Some(uid.clone()),
                            resource_version: Some(resource_version.clone()),
                        }),
                        ..DeleteParams::background()
                    };
                    let _ = jobs.delete(&name, &params).await;
                }
            }
        }
    }
    result
}

async fn finish_launch(
    client: &Client,
    config: &DebugJobConfig,
    request: &DebugJobRequest,
    admission: &DebugAdmission,
    authority: &dyn DebugLaunchAuthority,
    wanted: &Job,
    job: &DynamicObject,
) -> Result<Job, DebugJobError> {
    let suspended = check_job(wanted, job)?;
    let uid = job
        .metadata
        .uid
        .as_deref()
        .ok_or(DebugJobError::Conflict("Job"))?;
    let name = debug_job_name(request.session_id);
    let desired_policy = render_network_policy(config, request, uid)?;
    let policies = dynamic_api(
        client,
        &config.namespace,
        "networking.k8s.io",
        "NetworkPolicy",
        "networkpolicies",
    );
    let policy = match policies
        .create(&PostParams::default(), &as_dynamic(&desired_policy)?)
        .await
    {
        Ok(policy) => policy,
        Err(kube::Error::Api(error)) if error.code == 409 => policies.get(&name).await?,
        Err(error) => return Err(error.into()),
    };
    check_policy(&desired_policy, &policy)?;
    let desired_service = render_service(config, request, uid)?;
    let services = dynamic_api(client, &config.namespace, "", "Service", "services");
    let service = match services
        .create(&PostParams::default(), &as_dynamic(&desired_service)?)
        .await
    {
        Ok(service) => service,
        Err(kube::Error::Api(error)) if error.code == 409 => services.get(&name).await?,
        Err(error) => return Err(error.into()),
    };
    check_service(&desired_service, &service)?;
    let typed_job = as_typed(job)?;
    let typed_policy = as_typed(&policy)?;
    let typed_service = as_typed(&service)?;
    authority
        .verify_isolation(config, request, &typed_job, &typed_policy, &typed_service)
        .await?;
    if !suspended {
        return Ok(typed_job);
    }
    let jobs = dynamic_api(client, &config.namespace, "batch", "Job", "jobs");
    let mut current = job.clone();
    // The Job controller and concurrent identical launches can update the
    // resourceVersion between the read and patch. Re-read and revalidate on a
    // conflict, and accept a matching Job already resumed by another caller.
    for _ in 0..4 {
        validate(config, request, Utc::now())?;
        validate_admission(request, admission)?;
        if current.metadata.uid.as_deref() != Some(uid) {
            return Err(DebugJobError::Conflict("Job"));
        }
        if !check_job(wanted, &current)? {
            return as_typed(&current);
        }
        let resource_version = current
            .metadata
            .resource_version
            .as_deref()
            .ok_or(DebugJobError::Conflict("Job"))?;
        let patch = json!({"metadata":{"resourceVersion":resource_version,"annotations":{LAUNCH_ANNOTATION:"active"}},
            "spec":{"suspend":false}});
        match jobs
            .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Ok(active) => {
                if check_job(wanted, &active)? {
                    return Err(DebugJobError::Conflict("Job"));
                }
                return as_typed(&active);
            }
            Err(kube::Error::Api(error)) if error.code == 409 => current = jobs.get(&name).await?,
            Err(error) => return Err(error.into()),
        }
    }
    Err(DebugJobError::Conflict("Job"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn example() -> (DebugJobConfig, DebugJobRequest, DebugAdmission) {
        let now = Utc::now();
        let id = Uuid::from_u128(1);
        let endpoint = |host: &str, address: &str| TrustedEndpoint {
            url: host.into(),
            address: address.parse().unwrap(),
        };
        let config = DebugJobConfig {
            namespace: "service-ns".into(),
            service_account: "debug-reader".into(),
            opencode_image: format!("registry.example/opencode@sha256:{}", "a".repeat(64)),
            checkout_image: format!("registry.example/git@sha256:{}", "b".repeat(64)),
            git_repository: endpoint("https://git.example/repo.git", "10.1.0.1"),
            model_gateway: endpoint("https://model.example/v1", "10.1.0.2"),
            thalamus_mcp: endpoint("https://mediator.example/thalamus", "10.1.0.3"),
            flux_mcp: endpoint("https://mediator.example/flux", "10.1.0.3"),
            proxy_namespace: "control-plane".into(),
            proxy_pod_labels: BTreeMap::from([(
                "app.kubernetes.io/name".into(),
                "thalamus".into(),
            )]),
            max_duration_seconds: 3600,
            ttl_seconds_after_finished: 600,
            max_model_tokens: 10_000,
            max_tool_calls: 100,
        };
        let request = DebugJobRequest {
            session_id: id,
            namespace: "service-ns".into(),
            git_revision: "a".repeat(40),
            model: "my-model".into(),
            created_at: now,
            deadline: now + Duration::minutes(10),
            model_token_budget: 1000,
            tool_call_budget: 10,
        };
        let admission = DebugAdmission {
            session_id: id,
            namespace: request.namespace.clone(),
            git_revision: request.git_revision.clone(),
            model: request.model.clone(),
            deadline: request.deadline,
            model_token_budget: request.model_token_budget,
            tool_call_budget: request.tool_call_budget,
            model_budget_lease_id: Uuid::from_u128(2),
            tool_budget_lease_id: Uuid::from_u128(3),
            model_credential_id: Uuid::from_u128(4),
            mcp_credential_id: Uuid::from_u128(5),
            credentials_expire_at: request.deadline,
        };
        (config, request, admission)
    }

    #[test]
    fn job_is_bounded_hardened_and_never_contains_secret_values() {
        let (config, request, admission) = example();
        let job = render_debug_job(&config, &request, &admission).unwrap();
        let value = serde_json::to_value(job).unwrap();
        assert_eq!(
            value["metadata"]["name"],
            debug_job_name(request.session_id)
        );
        assert_eq!(value["spec"]["suspend"], true);
        assert_eq!(value["spec"]["activeDeadlineSeconds"], 600);
        assert_eq!(value["spec"]["ttlSecondsAfterFinished"], 600);
        let pod = &value["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(
            pod["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
            true
        );
        assert_eq!(
            pod["containers"][0]["securityContext"]["capabilities"]["drop"][0],
            "ALL"
        );
        assert_eq!(pod["containers"][0]["volumeMounts"][0]["readOnly"], true);
        let serialized = value.to_string();
        assert!(serialized.contains(&debug_model_secret_name(request.session_id)));
        assert!(serialized.contains(&debug_server_secret_name(request.session_id)));
        assert!(serialized.contains("{env:DEBUG_MODEL_KEY}"));
        assert!(!serialized.contains("thl_secret"));
        assert!(!serialized.contains("forge-token"));
        assert!(serialized.contains("credential.helper="));
        assert!(serialized.contains("GIT_TERMINAL_PROMPT"));
        assert!(serialized.contains("\\\"*\\\":\\\"deny\\\""));
    }

    #[test]
    fn policy_only_allows_pinned_endpoints_and_authenticated_proxy() {
        let (config, request, _) = example();
        let policy = render_network_policy(&config, &request, "job-uid").unwrap();
        let value = serde_json::to_value(policy).unwrap();
        assert_eq!(value["spec"]["policyTypes"], json!(["Ingress", "Egress"]));
        let egress = value["spec"]["egress"].as_array().unwrap();
        assert_eq!(egress.len(), 3);
        assert!(egress
            .iter()
            .all(|rule| rule["ports"][0]["protocol"] == "TCP"));
        assert!(egress.iter().all(|rule| rule["to"][0]["ipBlock"]["cidr"]
            .as_str()
            .unwrap()
            .ends_with("/32")));
        assert_eq!(
            value["spec"]["ingress"][0]["from"][0]["namespaceSelector"]["matchLabels"]
                ["kubernetes.io/metadata.name"],
            "control-plane"
        );
        let service =
            serde_json::to_value(render_service(&config, &request, "job-uid").unwrap()).unwrap();
        assert_eq!(service["spec"]["ports"][0]["port"], PORT);
        assert_eq!(service["metadata"]["ownerReferences"][0]["uid"], "job-uid");
    }

    #[test]
    fn rejects_untrusted_inputs_and_unbound_grants() {
        let (mut config, mut request, mut admission) = example();
        config.opencode_image = "opencode:latest".into();
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::Invalid(_))
        ));
        config.opencode_image = format!("registry/opencode@sha256:{}", "a".repeat(64));
        request.namespace = "other".into();
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::Invalid(_))
        ));
        request.namespace = config.namespace.clone();
        request.git_revision = "main".into();
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::Invalid(_))
        ));
        request.git_revision = admission.git_revision.clone();
        config.git_repository.url = "https://user:secret@git.example/repo.git".into();
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::Invalid(_))
        ));
        config.git_repository.url = "https://git.example/repo.git".into();
        config.flux_mcp.url = "https://direct-flux.example/mcp".into();
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::Invalid(_))
        ));
        config.flux_mcp.url = "https://mediator.example/flux".into();
        admission.tool_budget_lease_id = Uuid::nil();
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::AdmissionDenied)
        ));
        admission.tool_budget_lease_id = Uuid::from_u128(3);
        admission.model_token_budget += 1;
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::AdmissionDenied)
        ));
    }

    #[test]
    fn rejects_expired_overlong_and_unbounded_requests() {
        let (mut config, mut request, mut admission) = example();
        request.deadline = request.created_at - Duration::seconds(1);
        admission.deadline = request.deadline;
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::Invalid(_))
        ));
        request.deadline = request.created_at + Duration::hours(2);
        admission.deadline = request.deadline;
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::Invalid(_))
        ));
        request.deadline = request.created_at + Duration::minutes(10);
        admission.deadline = request.deadline;
        admission.credentials_expire_at = request.deadline + Duration::minutes(5);
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::AdmissionDenied)
        ));
        admission.credentials_expire_at = request.deadline;
        config.max_tool_calls = 0;
        assert!(matches!(
            render_debug_job(&config, &request, &admission),
            Err(DebugJobError::Invalid(_))
        ));
    }

    #[test]
    fn missing_trusted_authority_is_rejected() {
        assert!(matches!(
            require_authority(None),
            Err(DebugJobError::AdmissionUnavailable)
        ));
    }

    #[test]
    fn conflicts_are_not_accepted_by_name_or_labels_alone() {
        let (config, request, admission) = example();
        let wanted = render_debug_job(&config, &request, &admission).unwrap();
        let mut value = serde_json::to_value(&wanted).unwrap();
        value["metadata"]["uid"] = json!("job-uid");
        value["spec"]["selector"] =
            json!({"matchLabels":{"batch.kubernetes.io/controller-uid":"job-uid"}});
        value["spec"]["manualSelector"] = json!(false);
        value["spec"]["podReplacementPolicy"] = json!("TerminatingOrFailed");
        value["spec"]["template"]["spec"]["schedulerName"] = json!("default-scheduler");
        value["spec"]["template"]["metadata"]["labels"]["batch.kubernetes.io/controller-uid"] =
            json!("job-uid");
        value["spec"]["template"]["metadata"]["labels"]["batch.kubernetes.io/job-name"] =
            json!(debug_job_name(request.session_id));
        value["spec"]["template"]["metadata"]["creationTimestamp"] = Value::Null;
        let pod = value["spec"]["template"]["spec"].as_object_mut().unwrap();
        pod.remove("hostNetwork");
        pod.remove("hostPID");
        pod.remove("hostIPC");
        let existing: DynamicObject = serde_json::from_value(value.clone()).unwrap();
        assert!(check_job(&wanted, &existing).unwrap());
        let mut altered_budget = value.clone();
        altered_budget["metadata"]["annotations"]["thalamus.dev/tool-call-budget"] = json!("999");
        let altered_budget: DynamicObject = serde_json::from_value(altered_budget).unwrap();
        assert!(matches!(
            check_job(&wanted, &altered_budget),
            Err(DebugJobError::Conflict("Job"))
        ));
        let mut unknown = value.clone();
        unknown["spec"]["template"]["spec"]["futurePrivilegeSwitch"] = json!(true);
        let unknown: DynamicObject = serde_json::from_value(unknown).unwrap();
        assert!(matches!(
            check_job(&wanted, &unknown),
            Err(DebugJobError::Conflict("Job"))
        ));
        let mut active = value.clone();
        active["spec"]["suspend"] = json!(false);
        active["metadata"]["annotations"][LAUNCH_ANNOTATION] = json!("active");
        let active: DynamicObject = serde_json::from_value(active).unwrap();
        assert!(!check_job(&wanted, &active).unwrap());
        let mut wrong_controller = value.clone();
        wrong_controller["spec"]["selector"]["matchLabels"]["batch.kubernetes.io/controller-uid"] =
            json!("other-uid");
        let wrong_controller: DynamicObject = serde_json::from_value(wrong_controller).unwrap();
        assert!(matches!(
            check_job(&wanted, &wrong_controller),
            Err(DebugJobError::Conflict("Job"))
        ));
        value["spec"]["template"]["spec"]["containers"][0]["securityContext"]["privileged"] =
            json!(true);
        let altered: DynamicObject = serde_json::from_value(value).unwrap();
        assert!(matches!(
            check_job(&wanted, &altered),
            Err(DebugJobError::Conflict("Job"))
        ));
        let wanted_service = render_service(&config, &request, "job-uid").unwrap();
        let mut altered_service = serde_json::to_value(&wanted_service).unwrap();
        altered_service["spec"]["selector"] = json!({});
        let altered_service: DynamicObject = serde_json::from_value(altered_service).unwrap();
        assert!(matches!(
            check_service(&wanted_service, &altered_service),
            Err(DebugJobError::Conflict("Service"))
        ));
    }
}
