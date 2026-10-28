//! A deliberately narrow client for a single debug Job's OpenCode server.
//!
//! The caller authenticates the principal and implements [`ProxyHost`].  The host
//! must authorize the session owner, current team membership, session state and
//! live budgets on every call.  It must resolve credentials from trusted Job
//! configuration, not from a request.  Neither arbitrary URLs nor OpenCode's
//! general HTTP API are exposed here.
//!
//! `/event` is a hot, instance-wide SSE feed, not a replayable session feed.
//! Run exactly one continuously polled collector per Job, connect it after
//! `create_session` and before `send_seed`, and serve UI subscribers from the
//! durable event store.  A disconnect or process crash can lose events: OpenCode
//! does not document an event cursor.  Gateway/tool budget enforcement and Job
//! deadlines must independently stop the agent even if this collector is not
//! running; the proxy's abort requests are only best effort.

use std::{io, net::IpAddr, str::FromStr, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{header, Client, Response, Url};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::time::{self, Instant};
use uuid::Uuid;

const OPENCODE_PORT: u16 = 4096;
const MAX_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_STREAM_BYTES: usize = 128 * 1024 * 1024;
const MAX_STREAM_EVENTS: usize = 1_000_000;
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REAUTHORIZE_INTERVAL: Duration = Duration::from_secs(5);

/// An identity obtained from the host's authenticated principal, never from an
/// unauthenticated HTTP parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Actor {
    pub user_id: Uuid,
    pub team_id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    CreateSession,
    Seed,
    Reply,
    Stream,
}

/// A fresh, authoritative authorization decision.  Do not log or serialize it:
/// it contains the OpenCode server password.
///
/// `budget_available` must reflect *both* model and tool budgets, not merely the
/// original configured limits.  The target is constructed by this module from
/// `namespace` and the Thalamus session ID; the host must load the namespace and
/// password from trusted session/Job state.  `upstream_session_id` is absent
/// only before creation and must come from the durable session record thereafter.
pub struct AuthorizedSession {
    pub namespace: String,
    pub password: String,
    pub upstream_session_id: Option<String>,
    pub deadline: DateTime<Utc>,
    pub budget_available: bool,
}

/// A record to durably append.  Neither this type nor upstream events implement
/// `Debug`, to discourage accidental logging of prompts or tool output.
pub struct StoredEvent {
    pub key: Uuid,
    pub kind: &'static str,
    pub payload: Value,
}

/// Host failures intentionally contain no underlying error messages, credentials,
/// or upstream bodies.  Map detailed errors to these variants at the host edge.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HostError {
    #[error("access denied")]
    Unauthorized,
    #[error("session not found")]
    NotFound,
    #[error("session state conflict")]
    Conflict,
    #[error("session budget exhausted")]
    BudgetExhausted,
    #[error("host service unavailable")]
    Unavailable,
}

/// Trusted integration boundary.  Every method must fail closed on authentication,
/// current membership, owner/team mismatch and relevant state/budget changes.
///
/// `reserve_initialization` must atomically reserve an uninitialized session;
/// `bind_upstream_session` must atomically check that claim and persist the ID.
/// `release_initialization` is called only when the proxy knows no create request
/// was sent.  If a create request may have reached OpenCode, its reservation is
/// deliberately retained for reconciliation rather than allowing a duplicate.
///
/// `load_seed` must construct text exclusively from persisted service context and
/// failing evidence, excluding credentials; never use caller-provided seed text.
/// `append_event` must commit before returning, and may map `key` to the store's
/// client request key.  Event payloads are sensitive transcript data.
#[async_trait]
pub trait ProxyHost: Send + Sync {
    async fn authorize(
        &self,
        actor: &Actor,
        session: Uuid,
        operation: Operation,
    ) -> Result<AuthorizedSession, HostError>;

    async fn reserve_initialization(
        &self,
        actor: &Actor,
        session: Uuid,
        claim: Uuid,
    ) -> Result<(), HostError>;

    async fn bind_upstream_session(
        &self,
        actor: &Actor,
        session: Uuid,
        claim: Uuid,
        upstream_id: &str,
    ) -> Result<(), HostError>;

    async fn release_initialization(
        &self,
        actor: &Actor,
        session: Uuid,
        claim: Uuid,
    ) -> Result<(), HostError>;

    async fn load_seed(&self, actor: &Actor, session: Uuid) -> Result<String, HostError>;

    async fn append_event(
        &self,
        actor: &Actor,
        session: Uuid,
        event: StoredEvent,
    ) -> Result<(), HostError>;
}

/// Per-connection resource limits.  Values above the hard ceilings are rejected.
#[derive(Clone, Debug)]
pub struct ProxyLimits {
    pub max_prompt_bytes: usize,
    pub max_event_bytes: usize,
    pub max_response_bytes: usize,
    pub max_stream_bytes: usize,
    pub max_stream_events: usize,
    pub request_timeout: Duration,
    pub reauthorize_interval: Duration,
}

impl Default for ProxyLimits {
    fn default() -> Self {
        Self {
            max_prompt_bytes: 64 * 1024,
            max_event_bytes: 128 * 1024,
            max_response_bytes: 64 * 1024,
            max_stream_bytes: 32 * 1024 * 1024,
            max_stream_events: 100_000,
            request_timeout: Duration::from_secs(10),
            reauthorize_interval: Duration::from_secs(2),
        }
    }
}

impl ProxyLimits {
    fn validate(&self) -> Result<(), ProxyError> {
        if self.max_prompt_bytes == 0
            || self.max_prompt_bytes > MAX_PROMPT_BYTES
            || self.max_event_bytes == 0
            || self.max_event_bytes > MAX_EVENT_BYTES
            || self.max_response_bytes == 0
            || self.max_response_bytes > MAX_RESPONSE_BYTES
            || self.max_stream_bytes == 0
            || self.max_stream_bytes > MAX_STREAM_BYTES
            || self.max_stream_events == 0
            || self.max_stream_events > MAX_STREAM_EVENTS
            || self.request_timeout.is_zero()
            || self.request_timeout > MAX_REQUEST_TIMEOUT
            || self.reauthorize_interval.is_zero()
            || self.reauthorize_interval > MAX_REAUTHORIZE_INTERVAL
        {
            return Err(ProxyError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error(transparent)]
    Host(#[from] HostError),
    #[error("invalid proxy limits")]
    InvalidLimits,
    #[error("invalid or missing trusted Kubernetes service CIDR configuration")]
    InvalidServiceCidrs,
    #[error("invalid trusted debug target")]
    InvalidTarget,
    #[error("debug session has expired")]
    DeadlineExceeded,
    #[error("session budget exhausted")]
    BudgetExhausted,
    #[error("the session has not been initialized")]
    NotInitialized,
    #[error("the session is already initialized")]
    AlreadyInitialized,
    #[error("authorized session target or identity changed")]
    AuthorizationChanged,
    #[error("empty or oversized message")]
    InvalidMessage,
    #[error("OpenCode request failed")]
    Upstream,
    #[error("OpenCode returned HTTP {0}")]
    UpstreamStatus(u16),
    #[error("OpenCode returned an invalid response")]
    InvalidUpstreamResponse,
    #[error("OpenCode stream exceeded a configured limit")]
    StreamLimit,
    #[error("OpenCode event stream closed")]
    StreamClosed,
    #[error("initialization outcome is uncertain; reconcile claim {claim}")]
    InitializationIndeterminate {
        claim: Uuid,
        upstream_session_id: Option<String>,
    },
}

struct Target {
    namespace: String,
    password: String,
    base: Url,
}

impl Target {
    fn from_grant(session: Uuid, grant: &AuthorizedSession, port: u16) -> Result<Self, ProxyError> {
        if !valid_namespace(&grant.namespace)
            || grant.password.is_empty()
            || grant.password.len() > 4096
            || port == 0
        {
            return Err(ProxyError::InvalidTarget);
        }
        let base = Url::parse(&format!(
            "http://thalamus-debug-{session}.{}.svc.cluster.local:{port}/",
            grant.namespace
        ))
        .map_err(|_| ProxyError::InvalidTarget)?;
        Ok(Self {
            namespace: grant.namespace.clone(),
            password: grant.password.clone(),
            base,
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, ProxyError> {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| ProxyError::InvalidTarget)?;
        path.clear();
        for segment in segments {
            path.push(segment);
        }
        drop(path);
        Ok(url)
    }

    fn matches(&self, grant: &AuthorizedSession, upstream_id: &str) -> bool {
        self.namespace == grant.namespace
            && self.password == grant.password
            && grant.upstream_session_id.as_deref() == Some(upstream_id)
    }
}

fn valid_namespace(namespace: &str) -> bool {
    let bytes = namespace.as_bytes();
    let is_alphanumeric = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 63
        && is_alphanumeric(bytes[0])
        && is_alphanumeric(bytes[bytes.len() - 1])
        && bytes.iter().all(|&b| is_alphanumeric(b) || b == b'-')
}

/// An operator-configured Kubernetes Service network, not request input.
/// Network addresses must be canonical and wildcard (/0) networks are rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceCidr {
    network: IpAddr,
    prefix: u8,
}

impl ServiceCidr {
    pub fn new(network: IpAddr, prefix: u8) -> Result<Self, ProxyError> {
        let canonical = match network {
            IpAddr::V4(ip) if (1..=32).contains(&prefix) => {
                let mask = u32::MAX << (32 - prefix);
                u32::from(ip) & mask == u32::from(ip)
            }
            IpAddr::V6(ip) if (1..=128).contains(&prefix) => {
                let mask = u128::MAX << (128 - prefix);
                u128::from(ip) & mask == u128::from(ip)
            }
            _ => false,
        };
        if !canonical {
            return Err(ProxyError::InvalidServiceCidrs);
        }
        Ok(Self { network, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                let mask = u32::MAX << (32 - self.prefix);
                u32::from(ip) & mask == u32::from(network)
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                let mask = u128::MAX << (128 - self.prefix);
                u128::from(ip) & mask == u128::from(network)
            }
            _ => false,
        }
    }
}

impl FromStr for ServiceCidr {
    type Err = ProxyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (network, prefix) = value
            .split_once('/')
            .ok_or(ProxyError::InvalidServiceCidrs)?;
        let network = network
            .parse()
            .map_err(|_| ProxyError::InvalidServiceCidrs)?;
        let prefix = prefix
            .parse()
            .map_err(|_| ProxyError::InvalidServiceCidrs)?;
        Self::new(network, prefix)
    }
}

// Each DNS answer is checked before handing these same socket addresses to the
// connector. The connector does not perform a second, rebindable DNS lookup.
struct ClusterDns {
    service_cidrs: Arc<[ServiceCidr]>,
}

fn valid_service_hostname(host: &str) -> bool {
    let Some(prefix) = host.strip_suffix(".svc.cluster.local") else {
        return false;
    };
    let Some((name, namespace)) = prefix.split_once('.') else {
        return false;
    };
    let Some(id) = name.strip_prefix("thalamus-debug-") else {
        return false;
    };
    if !valid_namespace(namespace) || id.len() != 36 {
        return false;
    }
    Uuid::parse_str(id)
        .map(|uuid| {
            let mut buffer = Uuid::encode_buffer();
            uuid.hyphenated().encode_lower(&mut buffer) == id
        })
        .unwrap_or(false)
}

fn permitted_service_ip(ip: IpAddr, cidrs: &[ServiceCidr]) -> bool {
    let forbidden = match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip == std::net::Ipv4Addr::BROADCAST
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.segments()[0] & 0xffc0 == 0xfe80
                || ip.to_ipv4_mapped().is_some()
        }
    };
    !forbidden && cidrs.iter().any(|cidr| cidr.contains(ip))
}

impl reqwest::dns::Resolve for ClusterDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let hostname = name.as_str().to_owned();
        let service_cidrs = Arc::clone(&self.service_cidrs);
        Box::pin(async move {
            if !valid_service_hostname(&hostname) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "invalid service host",
                )
                .into());
            }
            let addresses: Vec<_> = tokio::net::lookup_host((hostname.as_str(), 0))
                .await?
                .take(65)
                .collect();
            if addresses.is_empty()
                || addresses.len() > 64
                || addresses
                    .iter()
                    .any(|addr| !permitted_service_ip(addr.ip(), &service_cidrs))
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "service address outside trusted CIDRs",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn valid_upstream_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn remaining(deadline: DateTime<Utc>) -> Result<Duration, ProxyError> {
    let duration = deadline
        .signed_duration_since(Utc::now())
        .to_std()
        .map_err(|_| ProxyError::DeadlineExceeded)?;
    if duration.is_zero() {
        return Err(ProxyError::DeadlineExceeded);
    }
    Ok(duration)
}

async fn authorized<H: ProxyHost + ?Sized>(
    host: &H,
    actor: &Actor,
    session: Uuid,
    operation: Operation,
    timeout: Duration,
) -> Result<AuthorizedSession, ProxyError> {
    let grant = time::timeout(timeout, host.authorize(actor, session, operation))
        .await
        .map_err(|_| ProxyError::Host(HostError::Unavailable))??;
    remaining(grant.deadline)?;
    if !grant.budget_available {
        return Err(ProxyError::BudgetExhausted);
    }
    Ok(grant)
}

/// The only HTTP client for a Job.  Redirects and environment-configured HTTP
/// proxies are disabled, and endpoint paths are selected internally.
pub struct DebugProxy<H: ProxyHost + ?Sized> {
    host: Arc<H>,
    client: Client,
    limits: ProxyLimits,
    #[cfg(test)]
    test_port: Option<u16>,
}

impl<H: ProxyHost + ?Sized + 'static> DebugProxy<H> {
    pub fn new(
        host: Arc<H>,
        limits: ProxyLimits,
        service_cidrs: Vec<ServiceCidr>,
    ) -> Result<Self, ProxyError> {
        limits.validate()?;
        if service_cidrs.is_empty() || service_cidrs.len() > 64 {
            return Err(ProxyError::InvalidServiceCidrs);
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(ClusterDns {
                service_cidrs: service_cidrs.into(),
            }))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| ProxyError::Upstream)?;
        Ok(Self {
            host,
            client,
            limits,
            #[cfg(test)]
            test_port: None,
        })
    }

    fn port(&self) -> u16 {
        #[cfg(test)]
        if let Some(port) = self.test_port {
            return port;
        }
        OPENCODE_PORT
    }

    async fn grant(
        &self,
        actor: &Actor,
        session: Uuid,
        operation: Operation,
    ) -> Result<AuthorizedSession, ProxyError> {
        authorized(
            self.host.as_ref(),
            actor,
            session,
            operation,
            self.limits.request_timeout,
        )
        .await
    }

    async fn release_before_send(&self, actor: &Actor, session: Uuid, claim: Uuid) -> bool {
        matches!(
            time::timeout(
                self.limits.request_timeout,
                self.host.release_initialization(actor, session, claim),
            )
            .await,
            Ok(Ok(()))
        )
    }

    /// Reserve and create exactly one upstream session.  An indeterminate create
    /// keeps its claim: the host must reconcile it before retrying.
    pub async fn create_session(&self, actor: &Actor, session: Uuid) -> Result<String, ProxyError> {
        let initial = self.grant(actor, session, Operation::CreateSession).await?;
        if initial.upstream_session_id.is_some() {
            return Err(ProxyError::AlreadyInitialized);
        }
        let initial_target = Target::from_grant(session, &initial, self.port())?;
        let claim = Uuid::new_v4();
        time::timeout(
            self.limits
                .request_timeout
                .min(remaining(initial.deadline)?),
            self.host.reserve_initialization(actor, session, claim),
        )
        .await
        .map_err(|_| ProxyError::InitializationIndeterminate {
            claim,
            upstream_session_id: None,
        })??;

        let next = self.grant(actor, session, Operation::CreateSession).await;
        let next = match next {
            Ok(next)
                if next.upstream_session_id.is_none()
                    && next.namespace == initial_target.namespace
                    && next.password == initial_target.password =>
            {
                next
            }
            result => {
                let error = match result {
                    Err(error) => error,
                    Ok(_) => ProxyError::AuthorizationChanged,
                };
                if self.release_before_send(actor, session, claim).await {
                    return Err(error);
                }
                return Err(ProxyError::InitializationIndeterminate {
                    claim,
                    upstream_session_id: None,
                });
            }
        };
        let deadline = initial.deadline.min(next.deadline);
        let request_timeout = match remaining(deadline) {
            Ok(duration) => duration.min(self.limits.request_timeout),
            Err(error) => {
                if self.release_before_send(actor, session, claim).await {
                    return Err(error);
                }
                return Err(ProxyError::InitializationIndeterminate {
                    claim,
                    upstream_session_id: None,
                });
            }
        };
        let url = initial_target.endpoint(&["session"])?;
        let response = self
            .client
            .post(url)
            .basic_auth("opencode", Some(&initial_target.password))
            .timeout(request_timeout)
            .json(&json!({}))
            .send()
            .await;
        // Even an error response may follow a committed create, so do not retry.
        let mut response = match response {
            Ok(response) if response.status().is_success() => response,
            _ => {
                return Err(ProxyError::InitializationIndeterminate {
                    claim,
                    upstream_session_id: None,
                })
            }
        };
        let body = match read_bounded(&mut response, self.limits.max_response_bytes).await {
            Ok(body) => body,
            Err(_) => {
                return Err(ProxyError::InitializationIndeterminate {
                    claim,
                    upstream_session_id: None,
                })
            }
        };
        let upstream_id = match serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned))
            .filter(|id| valid_upstream_id(id))
        {
            Some(id) => id,
            None => {
                return Err(ProxyError::InitializationIndeterminate {
                    claim,
                    upstream_session_id: None,
                })
            }
        };
        let bound = time::timeout(
            self.limits
                .request_timeout
                .min(remaining(deadline).unwrap_or(Duration::ZERO)),
            self.host
                .bind_upstream_session(actor, session, claim, &upstream_id),
        )
        .await;
        if !matches!(bound, Ok(Ok(()))) {
            best_effort_abort(&self.client, &initial_target, &upstream_id).await;
            return Err(ProxyError::InitializationIndeterminate {
                claim,
                upstream_session_id: Some(upstream_id),
            });
        }
        Ok(upstream_id)
    }

    /// Submit the host's persisted service context and evidence, not caller text.
    /// The host must make seeding a one-time state transition.
    pub async fn send_seed(&self, actor: &Actor, session: Uuid) -> Result<(), ProxyError> {
        let initial = self.grant(actor, session, Operation::Seed).await?;
        let seed = time::timeout(
            self.limits
                .request_timeout
                .min(remaining(initial.deadline)?),
            self.host.load_seed(actor, session),
        )
        .await
        .map_err(|_| ProxyError::Host(HostError::Unavailable))??;
        self.send_text(actor, session, Operation::Seed, &seed, initial)
            .await
    }

    /// Send text only.  Model, agent, tools, directory, command and shell
    /// overrides are intentionally not exposed.
    pub async fn send_reply(
        &self,
        actor: &Actor,
        session: Uuid,
        text: &str,
    ) -> Result<(), ProxyError> {
        let initial = self.grant(actor, session, Operation::Reply).await?;
        self.send_text(actor, session, Operation::Reply, text, initial)
            .await
    }

    async fn send_text(
        &self,
        actor: &Actor,
        session: Uuid,
        operation: Operation,
        text: &str,
        initial: AuthorizedSession,
    ) -> Result<(), ProxyError> {
        if text.trim().is_empty() || text.len() > self.limits.max_prompt_bytes {
            return Err(ProxyError::InvalidMessage);
        }
        let upstream_id = initial
            .upstream_session_id
            .as_deref()
            .ok_or(ProxyError::NotInitialized)?;
        if !valid_upstream_id(upstream_id) {
            return Err(ProxyError::InvalidTarget);
        }
        let target = Target::from_grant(session, &initial, self.port())?;
        let current = match self.grant(actor, session, operation).await {
            Ok(current) => current,
            Err(error) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(error);
            }
        };
        if !target.matches(&current, upstream_id) {
            best_effort_abort(&self.client, &target, upstream_id).await;
            return Err(ProxyError::AuthorizationChanged);
        }
        let mut deadline = initial.deadline.min(current.deadline);
        let key = Uuid::new_v4();
        let kind = if operation == Operation::Seed {
            "seed.requested"
        } else {
            "reply.requested"
        };
        let record_timeout = match remaining(deadline) {
            Ok(duration) => duration.min(self.limits.request_timeout),
            Err(error) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(error);
            }
        };
        let recorded = time::timeout(
            record_timeout,
            self.host.append_event(
                actor,
                session,
                StoredEvent {
                    key,
                    kind,
                    payload: json!({"text": text}),
                },
            ),
        )
        .await;
        match recorded {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(ProxyError::Host(error));
            }
            Err(_) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(ProxyError::Host(HostError::Unavailable));
            }
        }

        // Do not send after revocation or a budget/state change during persistence.
        let current = match self.grant(actor, session, operation).await {
            Ok(current) => current,
            Err(error) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(error);
            }
        };
        if !target.matches(&current, upstream_id) {
            best_effort_abort(&self.client, &target, upstream_id).await;
            return Err(ProxyError::AuthorizationChanged);
        }
        deadline = deadline.min(current.deadline);
        let request_timeout = match remaining(deadline) {
            Ok(duration) => duration.min(self.limits.request_timeout),
            Err(error) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(error);
            }
        };
        let url = target.endpoint(&["session", upstream_id, "prompt_async"])?;
        let result = self
            .client
            .post(url)
            .basic_auth("opencode", Some(&target.password))
            .timeout(request_timeout)
            .json(&json!({"parts": [{"type": "text", "text": text}]}))
            .send()
            .await;
        let response = match result {
            Ok(response) if response.status().as_u16() == 204 => response,
            Ok(response) => {
                let status = response.status().as_u16();
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(ProxyError::UpstreamStatus(status));
            }
            Err(_) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(ProxyError::Upstream);
            }
        };
        drop(response);
        let kind = if operation == Operation::Seed {
            "seed.accepted"
        } else {
            "reply.accepted"
        };
        let saved = time::timeout(
            self.limits
                .request_timeout
                .min(remaining(deadline).unwrap_or(Duration::ZERO)),
            self.host.append_event(
                actor,
                session,
                StoredEvent {
                    key: Uuid::new_v4(),
                    kind,
                    payload: json!({"request_key": key}),
                },
            ),
        )
        .await;
        match saved {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                Err(ProxyError::Host(error))
            }
            Err(_) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                Err(ProxyError::Host(HostError::Unavailable))
            }
        }
    }

    /// Connect the single host-owned event collector before sending the seed.
    /// Poll `next_event` continuously even when no UI client is connected.
    pub async fn open_events(
        &self,
        actor: &Actor,
        session: Uuid,
    ) -> Result<EventStream<H>, ProxyError> {
        let grant = self.grant(actor, session, Operation::Stream).await?;
        let upstream_id = grant
            .upstream_session_id
            .as_deref()
            .ok_or(ProxyError::NotInitialized)?;
        if !valid_upstream_id(upstream_id) {
            return Err(ProxyError::InvalidTarget);
        }
        let target = Target::from_grant(session, &grant, self.port())?;
        let url = target.endpoint(&["event"])?;
        let request_timeout = match remaining(grant.deadline) {
            Ok(duration) => duration.min(self.limits.request_timeout),
            Err(error) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(error);
            }
        };
        let response = match time::timeout(
            request_timeout,
            self.client
                .get(url)
                .basic_auth("opencode", Some(&target.password))
                .header(header::ACCEPT, "text/event-stream")
                .send(),
        )
        .await
        {
            Ok(Ok(response)) => response,
            _ => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(ProxyError::Upstream);
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            best_effort_abort(&self.client, &target, upstream_id).await;
            return Err(ProxyError::UpstreamStatus(status));
        }
        let is_sse = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
            .unwrap_or(false);
        if !is_sse {
            best_effort_abort(&self.client, &target, upstream_id).await;
            return Err(ProxyError::InvalidUpstreamResponse);
        }
        let deadline = match remaining(grant.deadline) {
            Ok(duration) => Instant::now() + duration,
            Err(error) => {
                best_effort_abort(&self.client, &target, upstream_id).await;
                return Err(error);
            }
        };
        let mut stream = EventStream {
            host: Arc::clone(&self.host),
            client: self.client.clone(),
            actor: *actor,
            session,
            target,
            upstream_id: upstream_id.to_owned(),
            deadline,
            reauthorize_at: Instant::now(),
            response,
            decoder: SseDecoder::new(self.limits.max_event_bytes),
            limits: self.limits.clone(),
            stream_bytes: 0,
            stream_events: 0,
            closed: false,
        };
        if let Err(error) = stream.refresh().await {
            stream.abort().await;
            return Err(error);
        }
        Ok(stream)
    }
}

async fn read_bounded(response: &mut Response, max_bytes: usize) -> Result<Vec<u8>, ProxyError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(ProxyError::InvalidUpstreamResponse);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ProxyError::Upstream)? {
        if chunk.len() > max_bytes.saturating_sub(body.len()) {
            return Err(ProxyError::InvalidUpstreamResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn best_effort_abort(client: &Client, target: &Target, upstream_id: &str) {
    if let Ok(url) = target.endpoint(&["session", upstream_id, "abort"]) {
        // Failure here is never treated as budget enforcement: the gateway and
        // Job deadline must independently stop an autonomous upstream agent.
        let _ = time::timeout(
            Duration::from_secs(3),
            client
                .post(url)
                .basic_auth("opencode", Some(&target.password))
                .send(),
        )
        .await;
    }
}

/// One actual upstream bus event, already durably appended before delivery.
pub struct ProxyEvent {
    pub key: Uuid,
    pub payload: Value,
}

pub struct EventStream<H: ProxyHost + ?Sized> {
    host: Arc<H>,
    client: Client,
    actor: Actor,
    session: Uuid,
    target: Target,
    upstream_id: String,
    deadline: Instant,
    reauthorize_at: Instant,
    response: Response,
    decoder: SseDecoder,
    limits: ProxyLimits,
    stream_bytes: usize,
    stream_events: usize,
    closed: bool,
}

impl<H: ProxyHost + ?Sized + 'static> EventStream<H> {
    async fn abort(&mut self) {
        if !self.closed {
            self.closed = true;
            best_effort_abort(&self.client, &self.target, &self.upstream_id).await;
        }
    }

    fn remaining(&self) -> Result<Duration, ProxyError> {
        let duration = self.deadline.saturating_duration_since(Instant::now());
        if duration.is_zero() {
            Err(ProxyError::DeadlineExceeded)
        } else {
            Ok(duration)
        }
    }

    async fn refresh(&mut self) -> Result<(), ProxyError> {
        let timeout = self.limits.request_timeout.min(self.remaining()?);
        let grant = authorized(
            self.host.as_ref(),
            &self.actor,
            self.session,
            Operation::Stream,
            timeout,
        )
        .await?;
        if !self.target.matches(&grant, &self.upstream_id) {
            return Err(ProxyError::AuthorizationChanged);
        }
        self.deadline = self
            .deadline
            .min(Instant::now() + remaining(grant.deadline)?);
        self.remaining()?;
        self.reauthorize_at = Instant::now() + self.limits.reauthorize_interval;
        Ok(())
    }

    /// Wait for one session-scoped event.  Persistence succeeds before the
    /// event is returned; a persistence, authorization, budget or stream error
    /// closes this collector and attempts to abort the upstream session.
    pub async fn next_event(&mut self) -> Result<ProxyEvent, ProxyError> {
        if self.closed {
            return Err(ProxyError::StreamClosed);
        }
        let result = self.next_event_inner().await;
        if result.is_err() {
            self.abort().await;
        }
        result
    }

    async fn next_event_inner(&mut self) -> Result<ProxyEvent, ProxyError> {
        loop {
            self.remaining()?;
            if Instant::now() >= self.reauthorize_at {
                self.refresh().await?;
            }
            if let Some(data) = self.decoder.next()? {
                self.stream_events = self
                    .stream_events
                    .checked_add(1)
                    .ok_or(ProxyError::StreamLimit)?;
                if self.stream_events > self.limits.max_stream_events {
                    return Err(ProxyError::StreamLimit);
                }
                let payload: Value = serde_json::from_slice(&data)
                    .map_err(|_| ProxyError::InvalidUpstreamResponse)?;
                if !event_belongs_to_session(&payload, &self.upstream_id)? {
                    continue;
                }
                self.refresh().await?;
                let key = Uuid::new_v4();
                time::timeout(
                    self.limits.request_timeout.min(self.remaining()?),
                    self.host.append_event(
                        &self.actor,
                        self.session,
                        StoredEvent {
                            key,
                            kind: "opencode.event",
                            payload: payload.clone(),
                        },
                    ),
                )
                .await
                .map_err(|_| ProxyError::Host(HostError::Unavailable))??;
                // Membership or budgets may have changed while the write waited.
                self.refresh().await?;
                return Ok(ProxyEvent { key, payload });
            }
            let wait = self
                .reauthorize_at
                .saturating_duration_since(Instant::now())
                .min(self.remaining()?);
            tokio::select! {
                chunk = self.response.chunk() => {
                    let chunk = chunk.map_err(|_| ProxyError::Upstream)?.ok_or(ProxyError::StreamClosed)?;
                    self.stream_bytes = self.stream_bytes.checked_add(chunk.len()).ok_or(ProxyError::StreamLimit)?;
                    if self.stream_bytes > self.limits.max_stream_bytes {
                        return Err(ProxyError::StreamLimit);
                    }
                    self.decoder.feed(&chunk)?;
                }
                _ = time::sleep(wait) => self.refresh().await?,
            }
        }
    }
}

// OpenCode bus events identify the session in one of these documented fields.
// Unscoped server/heartbeat events and events for any other session are dropped.
// If multiple recognized session IDs occur, all must match.
fn event_belongs_to_session(event: &Value, upstream_id: &str) -> Result<bool, ProxyError> {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .ok_or(ProxyError::InvalidUpstreamResponse)?;
    let properties = event
        .get("properties")
        .and_then(Value::as_object)
        .ok_or(ProxyError::InvalidUpstreamResponse)?;
    let mut found = false;
    let mut matches = true;
    let mut check = |value: Option<&Value>| -> Result<(), ProxyError> {
        if let Some(value) = value {
            let id = value.as_str().ok_or(ProxyError::InvalidUpstreamResponse)?;
            found = true;
            matches &= id == upstream_id;
        }
        Ok(())
    };
    check(properties.get("sessionID"))?;
    if let Some(info) = properties.get("info").and_then(Value::as_object) {
        check(info.get("sessionID"))?;
        if matches!(
            kind,
            "session.created" | "session.updated" | "session.deleted"
        ) {
            check(info.get("id"))?;
        }
    }
    if let Some(part) = properties.get("part").and_then(Value::as_object) {
        check(part.get("sessionID"))?;
    }
    Ok(found && matches)
}

// Incremental SSE decoder.  It accepts CR, LF and CRLF, multiline data fields
// and comments; partial frames remain bounded and are never emitted at EOF.
struct SseDecoder {
    buffer: Vec<u8>,
    offset: usize,
    skip_lf: bool,
    data: Vec<u8>,
    has_data: bool,
    frame_bytes: usize,
    max_event_bytes: usize,
}

impl SseDecoder {
    fn new(max_event_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            offset: 0,
            skip_lf: false,
            data: Vec::new(),
            has_data: false,
            frame_bytes: 0,
            max_event_bytes,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), ProxyError> {
        if self.offset > 0 {
            self.buffer.drain(..self.offset);
            self.offset = 0;
        }
        // A chunk can contain more than one event, but buffering is still bounded.
        let cap = self.max_event_bytes.saturating_mul(2);
        if bytes.len() > cap.saturating_sub(self.buffer.len()) {
            return Err(ProxyError::StreamLimit);
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Vec<u8>>, ProxyError> {
        loop {
            if self.skip_lf {
                if self.offset == self.buffer.len() {
                    return Ok(None);
                }
                if self.buffer[self.offset] == b'\n' {
                    self.offset += 1;
                }
                self.skip_lf = false;
            }
            let mut end = self.offset;
            while end < self.buffer.len() && self.buffer[end] != b'\r' && self.buffer[end] != b'\n'
            {
                end += 1;
            }
            if end == self.buffer.len() {
                if self.buffer.len().saturating_sub(self.offset)
                    > self.max_event_bytes.saturating_sub(self.frame_bytes)
                {
                    return Err(ProxyError::StreamLimit);
                }
                return Ok(None);
            }
            let delimiter = if self.buffer[end] == b'\r' && self.buffer.get(end + 1) == Some(&b'\n')
            {
                2
            } else {
                1
            };
            self.skip_lf = self.buffer[end] == b'\r' && end + 1 == self.buffer.len();
            let consumed = end + delimiter - self.offset;
            self.frame_bytes = self
                .frame_bytes
                .checked_add(consumed)
                .ok_or(ProxyError::StreamLimit)?;
            if self.frame_bytes > self.max_event_bytes {
                return Err(ProxyError::StreamLimit);
            }
            let line = &self.buffer[self.offset..end];
            self.offset = end + delimiter;
            if line.is_empty() {
                self.frame_bytes = 0;
                if self.has_data {
                    self.has_data = false;
                    return Ok(Some(std::mem::take(&mut self.data)));
                }
                continue;
            }
            if line[0] == b':' {
                continue;
            }
            let colon = line.iter().position(|&b| b == b':').unwrap_or(line.len());
            if &line[..colon] == b"data" {
                let mut value = if colon == line.len() {
                    &[][..]
                } else {
                    &line[colon + 1..]
                };
                if value.first() == Some(&b' ') {
                    value = &value[1..];
                }
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
                self.has_data = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[test]
    fn target_is_derived_only_from_session_and_private_service_dns() {
        let session = Uuid::parse_str("4145a0b8-181d-461a-b13f-407d850d8782").unwrap();
        let grant = AuthorizedSession {
            namespace: "debug-team".into(),
            password: "not-a-real-secret".into(),
            upstream_session_id: Some("ses_test-1".into()),
            deadline: Utc::now() + chrono::Duration::minutes(1),
            budget_available: true,
        };
        let target = Target::from_grant(session, &grant, OPENCODE_PORT).unwrap();
        assert_eq!(target.endpoint(&["session", "ses_test-1", "prompt_async"]).unwrap().as_str(),
            "http://thalamus-debug-4145a0b8-181d-461a-b13f-407d850d8782.debug-team.svc.cluster.local:4096/session/ses_test-1/prompt_async");
        assert!(!valid_namespace("evil.example"));
        assert!(!valid_namespace("../../etc"));
        assert!(!valid_namespace("-debug"));
        assert!(!valid_namespace("Team"));
        assert!(!valid_upstream_id("../shell"));
        assert!(!valid_upstream_id("ses_1?directory=/"));
    }

    #[test]
    fn cidr_allowlist_is_required_canonical_and_rejects_special_addresses() {
        let cidr: ServiceCidr = "10.2.0.0/16".parse().unwrap();
        let ipv6: ServiceCidr = "fd12:3456::/32".parse().unwrap();
        assert!(permitted_service_ip("10.2.3.4".parse().unwrap(), &[cidr]));
        assert!(!permitted_service_ip("10.3.3.4".parse().unwrap(), &[cidr]));
        assert!(permitted_service_ip(
            "fd12:3456::42".parse().unwrap(),
            &[ipv6]
        ));
        assert!(!permitted_service_ip(
            "fd12:9999::42".parse().unwrap(),
            &[ipv6]
        ));
        assert!("0.0.0.0/0".parse::<ServiceCidr>().is_err());
        assert!("10.2.3.4/16".parse::<ServiceCidr>().is_err());
        assert!("fd12:3456::1/32".parse::<ServiceCidr>().is_err());
        assert!("::/0".parse::<ServiceCidr>().is_err());
        let loopback: ServiceCidr = "127.0.0.0/8".parse().unwrap();
        let link_local: ServiceCidr = "169.254.0.0/16".parse().unwrap();
        assert!(!permitted_service_ip(
            "127.0.0.1".parse().unwrap(),
            &[loopback]
        ));
        assert!(!permitted_service_ip(
            "169.254.169.254".parse().unwrap(),
            &[link_local]
        ));
        assert!(valid_service_hostname(
            "thalamus-debug-4145a0b8-181d-461a-b13f-407d850d8782.team.svc.cluster.local"
        ));
        assert!(!valid_service_hostname("thalamus-debug-4145a0b8-181d-461a-b13f-407d850d8782.team.svc.cluster.local.attacker.test"));
        assert!(!valid_service_hostname("arbitrary.team.svc.cluster.local"));
    }

    #[test]
    fn sse_handles_fragmentation_multiline_comments_and_crlf() {
        let mut decoder = SseDecoder::new(512);
        decoder
            .feed(b": ping\r\ndata: {\"type\":\"message.updated\",\r")
            .unwrap();
        assert!(decoder.next().unwrap().is_none());
        decoder
            .feed(b"\ndata: \"properties\":{\"sessionID\":\"ses_1\"}}\r\n\r")
            .unwrap();
        let value: Value = serde_json::from_slice(&decoder.next().unwrap().unwrap()).unwrap();
        decoder.feed(b"\n").unwrap();
        assert!(event_belongs_to_session(&value, "ses_1").unwrap());
        assert!(decoder.next().unwrap().is_none());
    }

    #[test]
    fn sse_bounds_partial_frames_and_never_emits_unterminated_data() {
        let mut decoder = SseDecoder::new(16);
        decoder.feed(b"data: unfinished").unwrap();
        assert!(decoder.next().unwrap().is_none());
        decoder.feed(b" even more").unwrap();
        assert!(matches!(decoder.next(), Err(ProxyError::StreamLimit)));
        assert!(matches!(
            SseDecoder::new(8).feed(&[b'x'; 17]),
            Err(ProxyError::StreamLimit)
        ));
    }

    #[test]
    fn event_filter_requires_matching_session_on_all_known_fields() {
        let message =
            json!({"type":"message.part.updated", "properties":{"part":{"sessionID":"ses_1"}}});
        assert!(event_belongs_to_session(&message, "ses_1").unwrap());
        assert!(!event_belongs_to_session(&message, "ses_2").unwrap());
        let mixed = json!({"type":"message.part.updated", "properties":{"sessionID":"ses_1","part":{"sessionID":"ses_2"}}});
        assert!(!event_belongs_to_session(&mixed, "ses_1").unwrap());
        assert!(!event_belongs_to_session(
            &json!({"type":"server.connected","properties":{}}),
            "ses_1"
        )
        .unwrap());
        let session = json!({"type":"session.updated","properties":{"info":{"id":"ses_1"}}});
        assert!(event_belongs_to_session(&session, "ses_1").unwrap());
        assert!(event_belongs_to_session(
            &json!({"type":"x","properties":{"sessionID":3}}),
            "ses_1"
        )
        .is_err());
    }

    struct DeniedHost {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl ProxyHost for DeniedHost {
        async fn authorize(
            &self,
            _: &Actor,
            _: Uuid,
            _: Operation,
        ) -> Result<AuthorizedSession, HostError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(HostError::Unauthorized)
        }
        async fn reserve_initialization(
            &self,
            _: &Actor,
            _: Uuid,
            _: Uuid,
        ) -> Result<(), HostError> {
            panic!("must not reserve")
        }
        async fn bind_upstream_session(
            &self,
            _: &Actor,
            _: Uuid,
            _: Uuid,
            _: &str,
        ) -> Result<(), HostError> {
            panic!("must not bind")
        }
        async fn release_initialization(
            &self,
            _: &Actor,
            _: Uuid,
            _: Uuid,
        ) -> Result<(), HostError> {
            panic!("must not release")
        }
        async fn load_seed(&self, _: &Actor, _: Uuid) -> Result<String, HostError> {
            panic!("must not load")
        }
        async fn append_event(&self, _: &Actor, _: Uuid, _: StoredEvent) -> Result<(), HostError> {
            panic!("must not append")
        }
    }

    #[tokio::test]
    async fn every_host_facing_operation_fails_before_io_when_denied() {
        let host = Arc::new(DeniedHost {
            calls: AtomicUsize::new(0),
        });
        let proxy = DebugProxy::new(
            Arc::clone(&host),
            ProxyLimits::default(),
            vec!["10.0.0.0/8".parse().unwrap()],
        )
        .unwrap();
        let actor = Actor {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
        };
        let session = Uuid::new_v4();
        assert!(matches!(
            proxy.create_session(&actor, session).await,
            Err(ProxyError::Host(HostError::Unauthorized))
        ));
        assert!(matches!(
            proxy.send_seed(&actor, session).await,
            Err(ProxyError::Host(HostError::Unauthorized))
        ));
        assert!(matches!(
            proxy.send_reply(&actor, session, "hello").await,
            Err(ProxyError::Host(HostError::Unauthorized))
        ));
        assert!(matches!(
            proxy.open_events(&actor, session).await,
            Err(ProxyError::Host(HostError::Unauthorized))
        ));
        assert_eq!(host.calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn unsafe_limits_are_rejected() {
        let limits = ProxyLimits {
            reauthorize_interval: Duration::from_secs(6),
            ..ProxyLimits::default()
        };
        assert!(matches!(limits.validate(), Err(ProxyError::InvalidLimits)));
        let limits = ProxyLimits {
            max_event_bytes: usize::MAX,
            ..ProxyLimits::default()
        };
        assert!(matches!(limits.validate(), Err(ProxyError::InvalidLimits)));
    }

    struct TestState {
        upstream_id: Option<String>,
        claim: Option<Uuid>,
        events: Vec<StoredEvent>,
    }

    struct TestHost {
        actor: Actor,
        session: Uuid,
        state: Mutex<TestState>,
        budget: AtomicBool,
        fail_append: AtomicBool,
    }

    impl TestHost {
        fn new(initialized: bool) -> Self {
            Self {
                actor: Actor {
                    user_id: Uuid::new_v4(),
                    team_id: Uuid::new_v4(),
                },
                session: Uuid::new_v4(),
                state: Mutex::new(TestState {
                    upstream_id: initialized.then(|| "ses_1".to_owned()),
                    claim: None,
                    events: Vec::new(),
                }),
                budget: AtomicBool::new(true),
                fail_append: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl ProxyHost for TestHost {
        async fn authorize(
            &self,
            actor: &Actor,
            session: Uuid,
            _: Operation,
        ) -> Result<AuthorizedSession, HostError> {
            if *actor != self.actor || session != self.session {
                return Err(HostError::Unauthorized);
            }
            Ok(AuthorizedSession {
                namespace: "test-team".into(),
                password: "secret".into(),
                upstream_session_id: self.state.lock().unwrap().upstream_id.clone(),
                deadline: Utc::now() + chrono::Duration::minutes(1),
                budget_available: self.budget.load(Ordering::SeqCst),
            })
        }

        async fn reserve_initialization(
            &self,
            _: &Actor,
            _: Uuid,
            claim: Uuid,
        ) -> Result<(), HostError> {
            let mut state = self.state.lock().unwrap();
            if state.claim.is_some() || state.upstream_id.is_some() {
                return Err(HostError::Conflict);
            }
            state.claim = Some(claim);
            Ok(())
        }

        async fn bind_upstream_session(
            &self,
            _: &Actor,
            _: Uuid,
            claim: Uuid,
            id: &str,
        ) -> Result<(), HostError> {
            let mut state = self.state.lock().unwrap();
            if state.claim != Some(claim) || state.upstream_id.is_some() {
                return Err(HostError::Conflict);
            }
            state.upstream_id = Some(id.to_owned());
            state.claim = None;
            Ok(())
        }

        async fn release_initialization(
            &self,
            _: &Actor,
            _: Uuid,
            claim: Uuid,
        ) -> Result<(), HostError> {
            let mut state = self.state.lock().unwrap();
            if state.claim != Some(claim) {
                return Err(HostError::Conflict);
            }
            state.claim = None;
            Ok(())
        }

        async fn load_seed(&self, _: &Actor, _: Uuid) -> Result<String, HostError> {
            Ok("persisted service evidence".to_owned())
        }

        async fn append_event(
            &self,
            _: &Actor,
            _: Uuid,
            event: StoredEvent,
        ) -> Result<(), HostError> {
            if self.fail_append.load(Ordering::SeqCst) {
                return Err(HostError::Unavailable);
            }
            self.state.lock().unwrap().events.push(event);
            Ok(())
        }
    }

    // The production resolver cannot reach loopback; only this test-only client
    // overrides DNS and the port to exercise real HTTP/SSE with a local listener.
    fn test_proxy(host: Arc<TestHost>, listener: &TcpListener) -> DebugProxy<TestHost> {
        let address = listener.local_addr().unwrap();
        let domain = format!(
            "thalamus-debug-{}.test-team.svc.cluster.local",
            host.session
        );
        let mut proxy = DebugProxy::new(
            host,
            ProxyLimits::default(),
            vec!["10.0.0.0/8".parse().unwrap()],
        )
        .unwrap();
        proxy.client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .resolve(&domain, address)
            .build()
            .unwrap();
        proxy.test_port = Some(address.port());
        proxy
    }

    async fn request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 2048];
        loop {
            let n = stream.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                let header_end = end + 4;
                let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if bytes.len() >= header_end + length {
                    return String::from_utf8(bytes).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn create_binds_upstream_id_to_reserved_session() {
        let host = Arc::new(TestHost::new(false));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy = test_proxy(Arc::clone(&host), &listener);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = request(&mut stream).await;
            assert!(request.starts_with("POST /session HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: basic b3blbmnvzgu6c2vjcmv0"));
            let body = "{\"id\":\"ses_1\"}";
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        assert_eq!(
            proxy
                .create_session(&host.actor, host.session)
                .await
                .unwrap(),
            "ses_1"
        );
        assert_eq!(
            host.state.lock().unwrap().upstream_id.as_deref(),
            Some("ses_1")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn seed_and_reply_use_text_only_and_persist_before_sending() {
        let host = Arc::new(TestHost::new(true));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy = test_proxy(Arc::clone(&host), &listener);
        let server_host = Arc::clone(&host);
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for expected in ["seed.requested", "reply.requested"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = request(&mut stream).await;
                assert!(request.starts_with("POST /session/ses_1/prompt_async HTTP/1.1"));
                assert_eq!(
                    server_host
                        .state
                        .lock()
                        .unwrap()
                        .events
                        .last()
                        .unwrap()
                        .kind,
                    expected
                );
                let (_, body) = request.split_once("\r\n\r\n").unwrap();
                bodies.push(serde_json::from_str::<Value>(body).unwrap());
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            }
            bodies
        });
        proxy.send_seed(&host.actor, host.session).await.unwrap();
        proxy
            .send_reply(&host.actor, host.session, "user reply")
            .await
            .unwrap();
        let bodies = server.await.unwrap();
        assert_eq!(
            bodies[0],
            json!({"parts":[{"type":"text","text":"persisted service evidence"}]})
        );
        assert_eq!(
            bodies[1],
            json!({"parts":[{"type":"text","text":"user reply"}]})
        );
        let state = host.state.lock().unwrap();
        assert_eq!(
            state.events.iter().map(|e| e.kind).collect::<Vec<_>>(),
            [
                "seed.requested",
                "seed.accepted",
                "reply.requested",
                "reply.accepted"
            ]
        );
    }

    #[tokio::test]
    async fn real_sse_is_scoped_and_persisted_before_delivery() {
        let host = Arc::new(TestHost::new(true));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy = test_proxy(Arc::clone(&host), &listener);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = request(&mut stream).await;
            assert!(request.starts_with("GET /event HTTP/1.1"));
            let body = concat!(
                "data: {\"type\":\"server.connected\",\"properties\":{}}\n\n",
                "data: {\"type\":\"message.updated\",\"properties\":{\"info\":{\"sessionID\":\"ses_other\"}}}\n\n",
                "data: {\"type\":\"message.part.updated\",\"properties\":{\"part\":{\"sessionID\":\"ses_1\",\"text\":\"real event\"}}}\n\n"
            );
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let mut events = proxy.open_events(&host.actor, host.session).await.unwrap();
        let event = events.next_event().await.unwrap();
        {
            let state = host.state.lock().unwrap();
            assert_eq!(state.events.len(), 1);
            assert_eq!(state.events[0].key, event.key);
            assert_eq!(state.events[0].kind, "opencode.event");
            assert_eq!(state.events[0].payload, event.payload);
            assert_eq!(event.payload["properties"]["part"]["text"], "real event");
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn exhausted_budget_prevents_upstream_and_transcript_writes() {
        let host = Arc::new(TestHost::new(true));
        host.budget.store(false, Ordering::SeqCst);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy = test_proxy(Arc::clone(&host), &listener);
        assert!(matches!(
            proxy.send_reply(&host.actor, host.session, "hello").await,
            Err(ProxyError::BudgetExhausted)
        ));
        assert!(matches!(
            proxy.open_events(&host.actor, host.session).await,
            Err(ProxyError::BudgetExhausted)
        ));
        assert!(host.state.lock().unwrap().events.is_empty());
    }

    #[tokio::test]
    async fn upstream_failure_is_not_recorded_as_an_accepted_reply() {
        let host = Arc::new(TestHost::new(true));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy = test_proxy(Arc::clone(&host), &listener);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let prompt = request(&mut stream).await;
            assert!(prompt.starts_with("POST /session/ses_1/prompt_async HTTP/1.1"));
            stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let abort = request(&mut stream).await;
            assert!(abort.starts_with("POST /session/ses_1/abort HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        assert!(matches!(
            proxy.send_reply(&host.actor, host.session, "hello").await,
            Err(ProxyError::UpstreamStatus(500))
        ));
        {
            let state = host.state.lock().unwrap();
            assert_eq!(state.events.len(), 1);
            assert_eq!(state.events[0].kind, "reply.requested");
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn event_is_not_delivered_when_persistence_fails() {
        let host = Arc::new(TestHost::new(true));
        host.fail_append.store(true, Ordering::SeqCst);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy = test_proxy(Arc::clone(&host), &listener);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = request(&mut stream).await;
            let body = "data: {\"type\":\"message.updated\",\"properties\":{\"info\":{\"sessionID\":\"ses_1\"}}}\n\n";
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let abort = request(&mut stream).await;
            assert!(abort.starts_with("POST /session/ses_1/abort HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let mut events = proxy.open_events(&host.actor, host.session).await.unwrap();
        assert!(matches!(
            events.next_event().await,
            Err(ProxyError::Host(HostError::Unavailable))
        ));
        assert!(host.state.lock().unwrap().events.is_empty());
        server.await.unwrap();
    }
}
