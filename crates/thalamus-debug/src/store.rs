//! Postgres-backed, owner- and team-isolated debug sessions.
//!
//! `DebugIdentity` must come from an authenticated trusted transport, never from
//! request JSON. Each operation checks the current enabled user and team role
//! while holding the identity user row lock, then checks the session owner and
//! team. The identity migrations must have been applied first. When using an
//! `IdentityService` authorization guard, use the `_in_transaction` methods on
//! that guard's transaction and commit only on success; calling a pool method
//! while holding the same user's guard in another transaction can deadlock.
//!
//! Context, evidence, outcomes and events can contain sensitive user data. Their
//! trusted producers must redact credentials and enforce service authorization;
//! this store does not accept or persist a model/forge credential. Model budgets
//! are metadata here and must be enforced by the scoped model gateway key.

use std::fmt::Write as _;
use std::io;

use chrono::{DateTime, Utc};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgRow, PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

const MAX_JSON_BYTES: usize = 128 * 1024;
const MAX_PAGE_SIZE: i64 = 1000;
const CREDENTIAL_PREFIX: &str = "thld_";

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid input: {0}")]
    Invalid(&'static str),
    #[error("principal is not authorized for this team")]
    Forbidden,
    #[error("session or credential not found")]
    NotFound,
    #[error("client request key was already used for different input")]
    IdempotencyConflict,
    #[error("invalid or stale session transition")]
    InvalidTransition,
    #[error("session is not active or has expired")]
    Inactive,
    #[error("tool budget exhausted")]
    BudgetExhausted,
    #[error("upstream initialization already reserved")]
    InitializationBusy,
    #[error("invalid, expired or revoked tool credential")]
    InvalidCredential,
    #[error("event sequence exhausted")]
    SequenceExhausted,
    #[error("secure random generator unavailable")]
    RandomUnavailable,
    #[error("invalid stored debug data")]
    CorruptData,
}

/// A server-authenticated user and a team currently authorized for that user.
/// Do not construct from untrusted request fields without authenticating them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebugIdentity {
    pub user_id: Uuid,
    pub team_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl SessionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "timed_out" => Ok(Self::TimedOut),
            _ => Err(StoreError::CorruptData),
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }

    fn permits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Pending,
                Self::Running | Self::Failed | Self::Cancelled | Self::TimedOut
            ) | (
                Self::Running,
                Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
            )
        )
    }
}

/// A fully resolved, authorized and credential-free session request.
/// `checkout_revision` is the immutable, lowercase full Git commit ID.
#[derive(Clone)]
pub struct NewDebugSession {
    pub client_request_key: String,
    pub project_id: Uuid,
    pub service_id: String,
    pub run_id: Option<Uuid>,
    pub model: String,
    pub checkout_revision: String,
    pub job_namespace: String,
    pub deadline: DateTime<Utc>,
    pub model_budget: i64,
    pub tool_budget: i64,
    pub service_context: Value,
    pub failing_evidence: Value,
}

#[derive(Clone, Serialize)]
pub struct DebugSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub project_id: Uuid,
    pub client_request_key: String,
    pub service_id: String,
    pub run_id: Option<Uuid>,
    pub model: String,
    pub checkout_revision: String,
    pub job_namespace: String,
    pub job_name: String,
    pub deadline: DateTime<Utc>,
    pub model_budget: i64,
    pub tool_budget: i64,
    pub tool_calls_used: i64,
    pub service_context: Value,
    pub failing_evidence: Value,
    pub state: SessionState,
    pub outcome: Option<Value>,
    pub cleanup_required: bool,
    pub upstream_session_id: Option<String>,
    pub next_event_sequence: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip)]
    initialization_claim: Option<Uuid>,
    #[serde(skip)]
    last_transition_from: Option<SessionState>,
}

#[derive(Clone)]
pub struct NewDebugEvent {
    pub client_request_key: String,
    pub kind: String,
    pub payload: Value,
}

#[derive(Clone, Serialize)]
pub struct DebugEvent {
    pub session_id: Uuid,
    pub sequence: i64,
    pub client_request_key: String,
    pub kind: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

/// Returned only at issuance. Never log, serialize, or persist `secret` in
/// Postgres. Deliver it through a trusted Kubernetes Secret and discard it.
pub struct IssuedToolCredential {
    pub id: Uuid,
    pub secret: String,
}

/// Server-side authenticated binding; credential IDs are not bearer secrets.
/// Do not accept a credential ID supplied by an unauthenticated client.
#[derive(Clone, Debug)]
pub struct ToolCredentialBinding {
    pub credential_id: Uuid,
    pub identity: DebugIdentity,
    pub session_id: Uuid,
    pub issuer: String,
    pub subject: String,
}

#[derive(Clone)]
pub struct DebugStore {
    pool: PgPool,
}

/// Metadata only, returned by the privileged worker sweep. No transcript,
/// service context, evidence, credentials or outcome are returned.
#[derive(Clone, Debug)]
pub struct WorkerSessionRef {
    pub id: Uuid,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub state: SessionState,
    pub deadline: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub job_namespace: String,
    pub job_name: String,
    pub cleanup_required: bool,
}

#[derive(Clone, Debug)]
pub struct WorkerSessionCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

impl From<&WorkerSessionRef> for WorkerSessionCursor {
    fn from(session: &WorkerSessionRef) -> Self {
        Self {
            created_at: session.created_at,
            id: session.id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerFinalizationReason {
    DeadlineExpired,
    OwnerDisabled,
    TeamMembershipRevoked,
}

/// Cross-tenant capability for the trusted worker process only. Never expose
/// its methods as user-facing endpoints. It returns only cleanup metadata and
/// can finalize solely after verifying expiry or loss of owner authorization.
#[derive(Clone)]
pub struct WorkerStore {
    pool: PgPool,
}

#[derive(Clone, Copy)]
struct CredentialOwner {
    id: Uuid,
    identity: DebugIdentity,
    session_id: Uuid,
}

fn non_nil(id: Uuid, name: &'static str) -> Result<()> {
    if id.is_nil() {
        Err(StoreError::Invalid(name))
    } else {
        Ok(())
    }
}

fn printable(value: &str, max: usize, name: &'static str) -> Result<()> {
    if value.is_empty()
        || value.len() > max
        || !value.bytes().all(|b| (b'!'..=b'~').contains(&b))
        || contains_known_credential(value)
    {
        return Err(StoreError::Invalid(name));
    }
    Ok(())
}

fn valid_namespace(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn valid_revision(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_upstream_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !contains_known_credential(value)
}

struct BoundedCounter(usize);

impl io::Write for BoundedCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let count = self
            .0
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "JSON too large"))?;
        if count > MAX_JSON_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "JSON too large"));
        }
        self.0 = count;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Defense in depth for the two credentials issued by Thalamus. The producer
// must still redact other secrets; arbitrary evidence cannot be proven clean.
fn contains_known_credential(text: &str) -> bool {
    let bytes = text.as_bytes();
    let model_key = text.match_indices("thl_").any(|(offset, _)| {
        bytes.get(offset + 4..offset + 47).is_some_and(|s| {
            s.iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'_' | b'-'))
        })
    });
    let tool_key = text.match_indices(CREDENTIAL_PREFIX).any(|(offset, _)| {
        bytes
            .get(offset + CREDENTIAL_PREFIX.len()..offset + CREDENTIAL_PREFIX.len() + 64)
            .is_some_and(|s| {
                s.iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            })
    });
    model_key || tool_key
}

fn contains_credential(value: &Value) -> bool {
    match value {
        Value::String(text) => contains_known_credential(text),
        Value::Array(items) => items.iter().any(contains_credential),
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| contains_known_credential(key) || contains_credential(value)),
        _ => false,
    }
}

fn json_object(value: &Value, name: &'static str) -> Result<()> {
    if value.as_object().is_none_or(|object| object.is_empty()) {
        return Err(StoreError::Invalid(name));
    }
    serde_json::to_writer(&mut BoundedCounter(0), value).map_err(|_| StoreError::Invalid(name))?;
    if contains_credential(value) {
        return Err(StoreError::Invalid("credentials must not be persisted"));
    }
    Ok(())
}

fn db_timestamp(value: DateTime<Utc>) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(value.timestamp_micros())
        .ok_or(StoreError::Invalid("timestamp out of range"))
}

fn validate_new(input: &NewDebugSession) -> Result<()> {
    printable(&input.client_request_key, 256, "client request key")?;
    printable(&input.service_id, 256, "service ID")?;
    printable(&input.model, 256, "model")?;
    non_nil(input.project_id, "project ID")?;
    if let Some(id) = input.run_id {
        non_nil(id, "run ID")?;
    }
    if !valid_namespace(&input.job_namespace) {
        return Err(StoreError::Invalid("job namespace"));
    }
    if !valid_revision(&input.checkout_revision) {
        return Err(StoreError::Invalid(
            "checkout revision must be a full lowercase Git commit",
        ));
    }
    if input.model_budget <= 0 || input.tool_budget <= 0 {
        return Err(StoreError::Invalid("budgets must be positive"));
    }
    json_object(&input.service_context, "service context")?;
    json_object(&input.failing_evidence, "failing evidence")?;
    Ok(())
}

fn validate_event(input: &NewDebugEvent) -> Result<()> {
    printable(&input.client_request_key, 256, "event request key")?;
    if input.kind.is_empty()
        || input.kind.len() > 80
        || !input.kind.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
    {
        return Err(StoreError::Invalid("event kind"));
    }
    json_object(&input.payload, "event payload")
}

fn decode_session(row: &PgRow) -> Result<DebugSession> {
    let from: Option<&str> = row.try_get("last_transition_from")?;
    Ok(DebugSession {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        team_id: row.try_get("team_id")?,
        project_id: row.try_get("project_id")?,
        client_request_key: row.try_get("client_request_key")?,
        service_id: row.try_get("service_id")?,
        run_id: row.try_get("run_id")?,
        model: row.try_get("model")?,
        checkout_revision: row.try_get("checkout_revision")?,
        job_namespace: row.try_get("job_namespace")?,
        job_name: row.try_get("job_name")?,
        deadline: row.try_get("deadline")?,
        model_budget: row.try_get("model_budget")?,
        tool_budget: row.try_get("tool_budget")?,
        tool_calls_used: row.try_get("tool_calls_used")?,
        service_context: row.try_get("service_context")?,
        failing_evidence: row.try_get("failing_evidence")?,
        state: SessionState::parse(row.try_get("state")?)?,
        outcome: row.try_get("outcome")?,
        cleanup_required: row.try_get("cleanup_required")?,
        upstream_session_id: row.try_get("upstream_session_id")?,
        next_event_sequence: row.try_get("next_event_sequence")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        initialization_claim: row.try_get("initialization_claim")?,
        last_transition_from: from.map(SessionState::parse).transpose()?,
    })
}

fn decode_event(row: &PgRow) -> Result<DebugEvent> {
    Ok(DebugEvent {
        session_id: row.try_get("session_id")?,
        sequence: row.try_get("sequence")?,
        client_request_key: row.try_get("client_request_key")?,
        kind: row.try_get("kind")?,
        payload: row.try_get("payload")?,
        created_at: row.try_get("created_at")?,
    })
}

fn same_request(session: &DebugSession, input: &NewDebugSession, deadline: DateTime<Utc>) -> bool {
    session.project_id == input.project_id
        && session.service_id == input.service_id
        && session.run_id == input.run_id
        && session.model == input.model
        && session.checkout_revision == input.checkout_revision
        && session.job_namespace == input.job_namespace
        && session.deadline == deadline
        && session.model_budget == input.model_budget
        && session.tool_budget == input.tool_budget
        && session.service_context == input.service_context
        && session.failing_evidence == input.failing_evidence
}

async fn authorize(tx: &mut Transaction<'_, Postgres>, identity: &DebugIdentity) -> Result<PgRow> {
    non_nil(identity.user_id, "user ID")?;
    non_nil(identity.team_id, "team ID")?;
    // Identity's membership/offboarding writers lock this same user row first.
    let row = sqlx::query("SELECT issuer, subject, platform_admin FROM identity_users WHERE id=$1 AND NOT disabled FOR SHARE")
        .bind(identity.user_id).fetch_optional(&mut **tx).await?.ok_or(StoreError::Forbidden)?;
    let admin: bool = row.try_get("platform_admin")?;
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM identity_teams t WHERE t.id=$2 AND ($3 OR EXISTS (SELECT 1 FROM identity_memberships m WHERE m.user_id=$1 AND m.team_id=t.id)))")
        .bind(identity.user_id).bind(identity.team_id).bind(admin).fetch_one(&mut **tx).await?;
    if !allowed {
        return Err(StoreError::Forbidden);
    }
    Ok(row)
}

async fn session_row(
    tx: &mut Transaction<'_, Postgres>,
    identity: &DebugIdentity,
    id: Uuid,
    lock: bool,
) -> Result<PgRow> {
    non_nil(id, "session ID")?;
    let sql = if lock {
        "SELECT *, deadline > clock_timestamp() AS before_deadline FROM thalamus_debug_sessions WHERE id=$1 AND user_id=$2 AND team_id=$3 FOR UPDATE"
    } else {
        "SELECT *, deadline > clock_timestamp() AS before_deadline FROM thalamus_debug_sessions WHERE id=$1 AND user_id=$2 AND team_id=$3"
    };
    sqlx::query(sql)
        .bind(id)
        .bind(identity.user_id)
        .bind(identity.team_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NotFound)
}

// Lock only the small mutable control fields when neither transcript nor
// evidence is needed (notably on every MCP dispatch).
async fn session_guard_row(
    tx: &mut Transaction<'_, Postgres>,
    identity: &DebugIdentity,
    id: Uuid,
) -> Result<PgRow> {
    non_nil(id, "session ID")?;
    sqlx::query("SELECT state,deadline,deadline > clock_timestamp() AS before_deadline,tool_calls_used,tool_budget,next_event_sequence FROM thalamus_debug_sessions WHERE id=$1 AND user_id=$2 AND team_id=$3 FOR UPDATE")
        .bind(id).bind(identity.user_id).bind(identity.team_id)
        .fetch_optional(&mut **tx).await?.ok_or(StoreError::NotFound)
}

fn active(row: &PgRow, state: SessionState) -> Result<bool> {
    Ok(
        matches!(state, SessionState::Pending | SessionState::Running)
            && row.try_get::<bool, _>("before_deadline")?,
    )
}

fn credential_digest(secret: &str) -> Result<[u8; 32]> {
    let suffix = secret
        .strip_prefix(CREDENTIAL_PREFIX)
        .ok_or(StoreError::InvalidCredential)?;
    if suffix.len() != 64
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(StoreError::InvalidCredential);
    }
    Ok(Sha256::digest(secret.as_bytes()).into())
}

fn new_credential_secret() -> Result<String> {
    let mut random = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| StoreError::RandomUnavailable)?;
    let mut result = String::with_capacity(CREDENTIAL_PREFIX.len() + 64);
    result.push_str(CREDENTIAL_PREFIX);
    for byte in random {
        write!(&mut result, "{byte:02x}").map_err(|_| StoreError::RandomUnavailable)?;
    }
    Ok(result)
}

fn decode_owner(row: &PgRow) -> Result<CredentialOwner> {
    Ok(CredentialOwner {
        id: row.try_get("id")?,
        identity: DebugIdentity {
            user_id: row.try_get("user_id")?,
            team_id: row.try_get("team_id")?,
        },
        session_id: row.try_get("session_id")?,
    })
}

async fn owner_by_digest(
    tx: &mut Transaction<'_, Postgres>,
    digest: &[u8; 32],
) -> Result<CredentialOwner> {
    let row = sqlx::query(
        "SELECT id,session_id,user_id,team_id FROM thalamus_debug_tool_credentials WHERE digest=$1",
    )
    .bind(digest.as_slice())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StoreError::InvalidCredential)?;
    decode_owner(&row)
}

async fn owner_by_id(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<CredentialOwner> {
    if id.is_nil() {
        return Err(StoreError::InvalidCredential);
    }
    let row = sqlx::query(
        "SELECT id,session_id,user_id,team_id FROM thalamus_debug_tool_credentials WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StoreError::InvalidCredential)?;
    decode_owner(&row)
}

// Lock in the same order as issue/revoke: identity, session, credential.
async fn check_credential(
    tx: &mut Transaction<'_, Postgres>,
    owner: CredentialOwner,
    audience: &str,
) -> Result<(PgRow, PgRow)> {
    printable(audience, 256, "credential audience").map_err(|_| StoreError::InvalidCredential)?;
    let user = match authorize(tx, &owner.identity).await {
        Ok(user) => user,
        Err(StoreError::Forbidden | StoreError::Invalid(_)) => {
            return Err(StoreError::InvalidCredential)
        }
        Err(e) => return Err(e),
    };
    let row = match session_guard_row(tx, &owner.identity, owner.session_id).await {
        Ok(row) => row,
        Err(StoreError::NotFound | StoreError::Invalid(_)) => {
            return Err(StoreError::InvalidCredential)
        }
        Err(e) => return Err(e),
    };
    let state = SessionState::parse(row.try_get("state")?)?;
    if state != SessionState::Running || !row.try_get::<bool, _>("before_deadline")? {
        return Err(StoreError::InvalidCredential);
    }
    let valid: Option<Uuid> = sqlx::query_scalar("SELECT id FROM thalamus_debug_tool_credentials WHERE id=$1 AND session_id=$2 AND user_id=$3 AND team_id=$4 AND audience=$5 AND revoked_at IS NULL AND expires_at > clock_timestamp() FOR UPDATE")
        .bind(owner.id).bind(owner.session_id).bind(owner.identity.user_id).bind(owner.identity.team_id).bind(audience)
        .fetch_optional(&mut **tx).await?;
    if valid.is_none() {
        return Err(StoreError::InvalidCredential);
    }
    Ok((user, row))
}

fn decode_worker_session(row: &PgRow) -> Result<WorkerSessionRef> {
    Ok(WorkerSessionRef {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        team_id: row.try_get("team_id")?,
        state: SessionState::parse(row.try_get("state")?)?,
        deadline: row.try_get("deadline")?,
        created_at: row.try_get("created_at")?,
        job_namespace: row.try_get("job_namespace")?,
        job_name: row.try_get("job_name")?,
        cleanup_required: row.try_get("cleanup_required")?,
    })
}

async fn revoke_session_credentials(
    tx: &mut Transaction<'_, Postgres>,
    identity: &DebugIdentity,
    id: Uuid,
) -> Result<()> {
    sqlx::query("UPDATE thalamus_debug_tool_credentials SET revoked_at=clock_timestamp() WHERE session_id=$1 AND user_id=$2 AND team_id=$3 AND revoked_at IS NULL")
        .bind(id).bind(identity.user_id).bind(identity.team_id).execute(&mut **tx).await?;
    Ok(())
}

impl WorkerStore {
    /// This capability is for a trusted worker only, never a tenant endpoint.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Pending/running sessions plus terminal sessions awaiting confirmed
    /// cleanup. Reset `after` to None at the beginning of each new sweep;
    /// records becoming actionable behind a cursor appear in the next sweep.
    pub async fn list_actionable(
        &self,
        after: Option<WorkerSessionCursor>,
        limit: u32,
    ) -> Result<Vec<WorkerSessionRef>> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::Invalid("worker page limit"));
        }
        if let Some(cursor) = &after {
            non_nil(cursor.id, "worker cursor ID")?;
        }
        let rows = sqlx::query("SELECT id,user_id,team_id,state,deadline,created_at,job_namespace,job_name,cleanup_required FROM thalamus_debug_sessions WHERE (state IN ('pending','running') OR cleanup_required) AND ($1::timestamptz IS NULL OR (created_at,id)>($1,$2)) ORDER BY created_at,id LIMIT $3")
            .bind(after.as_ref().map(|cursor| cursor.created_at))
            .bind(after.as_ref().map(|cursor| cursor.id))
            .bind(i64::from(limit))
            .fetch_all(&self.pool).await?;
        rows.iter().map(decode_worker_session).collect()
    }

    /// Atomically finalize only a genuinely expired or currently unauthorized
    /// owner. The supplied owner/team must match the session; the worker is the
    /// trusted caller, so this also works after the owner has been disabled.
    /// Terminal cleanup stays in the sweep until explicitly acknowledged.
    pub async fn finalize_expired_or_inactive(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
    ) -> Result<Option<(WorkerSessionRef, WorkerFinalizationReason)>> {
        non_nil(identity.user_id, "user ID")?;
        non_nil(identity.team_id, "team ID")?;
        let mut tx = self.pool.begin().await?;
        let user =
            sqlx::query("SELECT disabled,platform_admin FROM identity_users WHERE id=$1 FOR SHARE")
                .bind(identity.user_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?;
        let disabled: bool = user.try_get("disabled")?;
        let admin: bool = user.try_get("platform_admin")?;
        let allowed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM identity_teams t WHERE t.id=$2 AND ($3 OR EXISTS (SELECT 1 FROM identity_memberships m WHERE m.user_id=$1 AND m.team_id=t.id)))")
            .bind(identity.user_id).bind(identity.team_id).bind(admin).fetch_one(&mut *tx).await?;
        let row = session_guard_row(&mut tx, identity, id).await?;
        let state = SessionState::parse(row.try_get("state")?)?;
        if state.is_terminal() {
            tx.commit().await?;
            return Ok(None);
        }
        let reason = if disabled {
            Some(WorkerFinalizationReason::OwnerDisabled)
        } else if !allowed {
            Some(WorkerFinalizationReason::TeamMembershipRevoked)
        } else if !row.try_get::<bool, _>("before_deadline")? {
            Some(WorkerFinalizationReason::DeadlineExpired)
        } else {
            None
        };
        let Some(reason) = reason else {
            tx.commit().await?;
            return Ok(None);
        };
        let (next, label) = match reason {
            WorkerFinalizationReason::DeadlineExpired => {
                (SessionState::TimedOut, "deadline_expired")
            }
            WorkerFinalizationReason::OwnerDisabled => (SessionState::Failed, "owner_disabled"),
            WorkerFinalizationReason::TeamMembershipRevoked => {
                (SessionState::Failed, "team_membership_revoked")
            }
        };
        let outcome = serde_json::json!({"reason": label});
        let row = sqlx::query("UPDATE thalamus_debug_sessions SET last_transition_from=state,state=$4,outcome=$5,cleanup_required=true,updated_at=clock_timestamp() WHERE id=$1 AND user_id=$2 AND team_id=$3 AND state IN ('pending','running') RETURNING id,user_id,team_id,state,deadline,created_at,job_namespace,job_name,cleanup_required")
            .bind(id).bind(identity.user_id).bind(identity.team_id).bind(next.as_str()).bind(outcome)
            .fetch_optional(&mut *tx).await?.ok_or(StoreError::InvalidTransition)?;
        revoke_session_credentials(&mut tx, identity, id).await?;
        let session = decode_worker_session(&row)?;
        tx.commit().await?;
        Ok(Some((session, reason)))
    }

    /// Acknowledge only after the worker has actually confirmed deletion of
    /// the Job/Secrets and revocation of any model key. This is idempotent and
    /// retains the cleanup item after crashes or ambiguous external failures.
    pub async fn mark_cleanup_complete(&self, identity: &DebugIdentity, id: Uuid) -> Result<()> {
        non_nil(identity.user_id, "user ID")?;
        non_nil(identity.team_id, "team ID")?;
        let mut tx = self.pool.begin().await?;
        let user: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM identity_users WHERE id=$1 FOR SHARE")
                .bind(identity.user_id)
                .fetch_optional(&mut *tx)
                .await?;
        if user.is_none() {
            return Err(StoreError::NotFound);
        }
        let row = session_guard_row(&mut tx, identity, id).await?;
        let state = SessionState::parse(row.try_get("state")?)?;
        if !state.is_terminal() {
            return Err(StoreError::InvalidTransition);
        }
        sqlx::query("UPDATE thalamus_debug_sessions SET cleanup_required=false,updated_at=clock_timestamp() WHERE id=$1 AND user_id=$2 AND team_id=$3 AND cleanup_required")
            .bind(id).bind(identity.user_id).bind(identity.team_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

impl DebugStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Administrative startup operation, not a tenant-facing API. Apply the
    /// identity migrations first and run migrations under deployment control.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(include_str!("../migrations/0001_debug.sql"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn create(
        &self,
        identity: &DebugIdentity,
        input: NewDebugSession,
    ) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let session = self.create_in_transaction(&mut tx, identity, input).await?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn create_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        input: NewDebugSession,
    ) -> Result<DebugSession> {
        validate_new(&input)?;
        let deadline = db_timestamp(input.deadline)?;
        authorize(tx, identity).await?;
        let project_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM identity_projects WHERE id=$1 AND team_id=$2)",
        )
        .bind(input.project_id)
        .bind(identity.team_id)
        .fetch_one(&mut **tx)
        .await?;
        if !project_exists {
            return Err(StoreError::NotFound);
        }
        let id = Uuid::new_v4();
        let name = format!("thalamus-debug-{id}");
        let row = sqlx::query("INSERT INTO thalamus_debug_sessions (id,user_id,team_id,project_id,client_request_key,service_id,run_id,model,checkout_revision,job_namespace,job_name,deadline,model_budget,tool_budget,service_context,failing_evidence) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16 WHERE $12::timestamptz > clock_timestamp() ON CONFLICT (user_id,team_id,client_request_key) DO NOTHING RETURNING *")
            .bind(id).bind(identity.user_id).bind(identity.team_id).bind(input.project_id)
            .bind(&input.client_request_key).bind(&input.service_id).bind(input.run_id).bind(&input.model)
            .bind(&input.checkout_revision).bind(&input.job_namespace).bind(name).bind(deadline)
            .bind(input.model_budget).bind(input.tool_budget).bind(&input.service_context).bind(&input.failing_evidence)
            .fetch_optional(&mut **tx).await?;
        if let Some(row) = row {
            return decode_session(&row);
        }
        let row = sqlx::query("SELECT * FROM thalamus_debug_sessions WHERE user_id=$1 AND team_id=$2 AND client_request_key=$3")
            .bind(identity.user_id).bind(identity.team_id).bind(&input.client_request_key)
            .fetch_optional(&mut **tx).await?.ok_or(StoreError::Invalid("deadline must be in the future"))?;
        let session = decode_session(&row)?;
        if !same_request(&session, &input, deadline) {
            return Err(StoreError::IdempotencyConflict);
        }
        Ok(session)
    }

    pub async fn get(&self, identity: &DebugIdentity, id: Uuid) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let session = self.get_in_transaction(&mut tx, identity, id).await?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn get_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
    ) -> Result<DebugSession> {
        authorize(tx, identity).await?;
        decode_session(&session_row(tx, identity, id, false).await?)
    }

    /// Compare-and-set. Terminal outcomes are required, nonterminal outcomes are
    /// forbidden. Repeating the same successful immediately preceding transition
    /// and identical outcome is idempotent; all other stale transitions fail.
    pub async fn transition(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
        expected: SessionState,
        next: SessionState,
        outcome: Option<Value>,
    ) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let session = self
            .transition_in_transaction(&mut tx, identity, id, expected, next, outcome)
            .await?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn transition_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
        expected: SessionState,
        next: SessionState,
        outcome: Option<Value>,
    ) -> Result<DebugSession> {
        if !expected.permits(next) {
            return Err(StoreError::InvalidTransition);
        }
        if next.is_terminal() {
            json_object(
                outcome
                    .as_ref()
                    .ok_or(StoreError::Invalid("terminal outcome required"))?,
                "outcome",
            )?;
        } else if outcome.is_some() {
            return Err(StoreError::Invalid("nonterminal outcome forbidden"));
        }
        authorize(tx, identity).await?;
        let row = session_row(tx, identity, id, true).await?;
        let session = decode_session(&row)?;
        if session.state == next
            && session.last_transition_from == Some(expected)
            && session.outcome == outcome
        {
            return Ok(session);
        }
        if session.state != expected {
            return Err(StoreError::InvalidTransition);
        }
        if next == SessionState::Running && !row.try_get::<bool, _>("before_deadline")? {
            return Err(StoreError::Inactive);
        }
        let row = sqlx::query("UPDATE thalamus_debug_sessions SET state=$4,last_transition_from=$5,outcome=$6,cleanup_required=$7,updated_at=clock_timestamp() WHERE id=$1 AND user_id=$2 AND team_id=$3 AND state=$5 AND ($4 <> 'running' OR deadline > clock_timestamp()) RETURNING *")
            .bind(id).bind(identity.user_id).bind(identity.team_id).bind(next.as_str()).bind(expected.as_str()).bind(outcome).bind(next.is_terminal())
            .fetch_optional(&mut **tx).await?.ok_or(StoreError::Inactive)?;
        if next.is_terminal() {
            revoke_session_credentials(tx, identity, id).await?;
        }
        decode_session(&row)
    }

    /// Append one event, using a per-session request key for idempotency. The
    /// session row serializes sequence allocation and transition races. Retrying
    /// an existing identical event works even after session termination.
    pub async fn append_event(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
        input: NewDebugEvent,
    ) -> Result<DebugEvent> {
        let mut tx = self.pool.begin().await?;
        let event = self
            .append_event_in_transaction(&mut tx, identity, id, input)
            .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn append_event_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
        input: NewDebugEvent,
    ) -> Result<DebugEvent> {
        validate_event(&input)?;
        authorize(tx, identity).await?;
        let row = session_guard_row(tx, identity, id).await?;
        let state = SessionState::parse(row.try_get("state")?)?;
        let previous = sqlx::query(
            "SELECT * FROM thalamus_debug_events WHERE session_id=$1 AND client_request_key=$2",
        )
        .bind(id)
        .bind(&input.client_request_key)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = previous {
            let event = decode_event(&row)?;
            if event.kind != input.kind || event.payload != input.payload {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok(event);
        }
        if state.is_terminal() {
            return Err(StoreError::Inactive);
        }
        if row.try_get::<i64, _>("next_event_sequence")? == i64::MAX {
            return Err(StoreError::SequenceExhausted);
        }
        let row = sqlx::query("UPDATE thalamus_debug_sessions SET next_event_sequence=next_event_sequence+1,updated_at=clock_timestamp() WHERE id=$1 AND user_id=$2 AND team_id=$3 AND next_event_sequence < 9223372036854775807 RETURNING next_event_sequence-1 AS sequence")
            .bind(id).bind(identity.user_id).bind(identity.team_id).fetch_optional(&mut **tx).await?.ok_or(StoreError::SequenceExhausted)?;
        let sequence: i64 = row.try_get("sequence")?;
        let row = sqlx::query("INSERT INTO thalamus_debug_events(session_id,sequence,client_request_key,kind,payload) VALUES ($1,$2,$3,$4,$5) RETURNING *")
            .bind(id).bind(sequence).bind(&input.client_request_key).bind(&input.kind).bind(&input.payload)
            .fetch_one(&mut **tx).await?;
        decode_event(&row)
    }

    /// Ascending events strictly after `after_sequence`; zero starts at the first.
    pub async fn events(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
        after_sequence: i64,
        limit: i64,
    ) -> Result<Vec<DebugEvent>> {
        let mut tx = self.pool.begin().await?;
        let events = self
            .events_in_transaction(&mut tx, identity, id, after_sequence, limit)
            .await?;
        tx.commit().await?;
        Ok(events)
    }

    pub async fn events_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
        after_sequence: i64,
        limit: i64,
    ) -> Result<Vec<DebugEvent>> {
        if after_sequence < 0 || !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::Invalid("event cursor or limit"));
        }
        authorize(tx, identity).await?;
        non_nil(id, "session ID")?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM thalamus_debug_sessions WHERE id=$1 AND user_id=$2 AND team_id=$3)")
            .bind(id).bind(identity.user_id).bind(identity.team_id).fetch_one(&mut **tx).await?;
        if !exists {
            return Err(StoreError::NotFound);
        }
        let rows = sqlx::query("SELECT e.* FROM thalamus_debug_events e JOIN thalamus_debug_sessions s ON s.id=e.session_id WHERE s.id=$1 AND s.user_id=$2 AND s.team_id=$3 AND e.sequence>$4 ORDER BY e.sequence LIMIT $5")
            .bind(id).bind(identity.user_id).bind(identity.team_id).bind(after_sequence).bind(limit).fetch_all(&mut **tx).await?;
        rows.iter().map(decode_event).collect()
    }

    /// Reserve creation of the upstream session before the external request.
    /// An unresolved claim is deliberately not auto-expired: after an ambiguous
    /// network failure, blindly retrying could create a second upstream session.
    pub async fn reserve_initialization(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
        claim: Uuid,
    ) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let session = self
            .reserve_initialization_in_transaction(&mut tx, identity, id, claim)
            .await?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn reserve_initialization_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
        claim: Uuid,
    ) -> Result<DebugSession> {
        non_nil(claim, "initialization claim")?;
        authorize(tx, identity).await?;
        let row = session_row(tx, identity, id, true).await?;
        let session = decode_session(&row)?;
        if session.state != SessionState::Running || !row.try_get::<bool, _>("before_deadline")? {
            return Err(StoreError::Inactive);
        }
        if let Some(existing) = session.initialization_claim {
            return if existing == claim {
                Ok(session)
            } else {
                Err(StoreError::InitializationBusy)
            };
        }
        let row = sqlx::query("UPDATE thalamus_debug_sessions SET initialization_claim=$4,updated_at=clock_timestamp() WHERE id=$1 AND user_id=$2 AND team_id=$3 AND initialization_claim IS NULL AND state='running' AND deadline > clock_timestamp() RETURNING *")
            .bind(id).bind(identity.user_id).bind(identity.team_id).bind(claim).fetch_optional(&mut **tx).await?.ok_or(StoreError::Inactive)?;
        decode_session(&row)
    }

    /// Only the reservation owner can bind; recording the known upstream ID is
    /// allowed after terminal/deadline so an in-flight response is not lost.
    pub async fn bind_upstream_session(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
        claim: Uuid,
        upstream_id: &str,
    ) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let session = self
            .bind_upstream_session_in_transaction(&mut tx, identity, id, claim, upstream_id)
            .await?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn bind_upstream_session_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
        claim: Uuid,
        upstream_id: &str,
    ) -> Result<DebugSession> {
        non_nil(claim, "initialization claim")?;
        if !valid_upstream_id(upstream_id) {
            return Err(StoreError::Invalid("upstream session ID"));
        }
        authorize(tx, identity).await?;
        let row = session_row(tx, identity, id, true).await?;
        let session = decode_session(&row)?;
        if session.initialization_claim != Some(claim) {
            return Err(StoreError::InitializationBusy);
        }
        if let Some(existing) = &session.upstream_session_id {
            return if existing == upstream_id {
                Ok(session)
            } else {
                Err(StoreError::IdempotencyConflict)
            };
        }
        let row = sqlx::query("UPDATE thalamus_debug_sessions SET upstream_session_id=$5,updated_at=clock_timestamp() WHERE id=$1 AND user_id=$2 AND team_id=$3 AND initialization_claim=$4 AND upstream_session_id IS NULL RETURNING *")
            .bind(id).bind(identity.user_id).bind(identity.team_id).bind(claim).bind(upstream_id)
            .fetch_optional(&mut **tx).await?.ok_or(StoreError::InitializationBusy)?;
        decode_session(&row)
    }

    /// Release only when the host knows no upstream session was created. An
    /// ambiguous HTTP failure is not evidence that it is safe to release.
    pub async fn release_initialization(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
        claim: Uuid,
    ) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let session = self
            .release_initialization_in_transaction(&mut tx, identity, id, claim)
            .await?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn release_initialization_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
        claim: Uuid,
    ) -> Result<DebugSession> {
        non_nil(claim, "initialization claim")?;
        authorize(tx, identity).await?;
        let row = session_row(tx, identity, id, true).await?;
        let session = decode_session(&row)?;
        if session.initialization_claim != Some(claim) || session.upstream_session_id.is_some() {
            return Err(StoreError::InitializationBusy);
        }
        let row = sqlx::query("UPDATE thalamus_debug_sessions SET initialization_claim=NULL,updated_at=clock_timestamp() WHERE id=$1 AND user_id=$2 AND team_id=$3 AND initialization_claim=$4 AND upstream_session_id IS NULL RETURNING *")
            .bind(id).bind(identity.user_id).bind(identity.team_id).bind(claim).fetch_optional(&mut **tx).await?.ok_or(StoreError::InitializationBusy)?;
        decode_session(&row)
    }

    /// Trusted, already-authenticated dispatch only. Never expose this as a way
    /// for an MCP caller to identify itself; use the credential-bound methods.
    pub async fn consume_tool_call(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
    ) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let session = self
            .consume_tool_call_in_transaction(&mut tx, identity, id)
            .await?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn consume_tool_call_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
    ) -> Result<DebugSession> {
        authorize(tx, identity).await?;
        consume_locked(tx, identity, id, None).await
    }

    /// Issue/rotate the one live session credential. `secret` is returned once;
    /// only its SHA-256 digest is stored. Expiry cannot outlive the session.
    pub async fn issue_tool_credential(
        &self,
        identity: &DebugIdentity,
        id: Uuid,
        audience: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<IssuedToolCredential> {
        let mut tx = self.pool.begin().await?;
        let issued = self
            .issue_tool_credential_in_transaction(&mut tx, identity, id, audience, expires_at)
            .await?;
        tx.commit().await?;
        Ok(issued)
    }

    pub async fn issue_tool_credential_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        id: Uuid,
        audience: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<IssuedToolCredential> {
        printable(audience, 256, "credential audience")?;
        let expires_at = db_timestamp(expires_at)?;
        authorize(tx, identity).await?;
        let row = session_guard_row(tx, identity, id).await?;
        let state = SessionState::parse(row.try_get("state")?)?;
        if !active(&row, state)? {
            return Err(StoreError::Inactive);
        }
        if expires_at > row.try_get::<DateTime<Utc>, _>("deadline")? {
            return Err(StoreError::Invalid("credential exceeds session deadline"));
        }
        let future: bool = sqlx::query_scalar("SELECT $1::timestamptz > clock_timestamp()")
            .bind(expires_at)
            .fetch_one(&mut **tx)
            .await?;
        if !future {
            return Err(StoreError::Invalid(
                "credential expiration must be in the future",
            ));
        }
        let secret = new_credential_secret()?;
        let digest = credential_digest(&secret)?;
        let credential_id = Uuid::new_v4();
        sqlx::query("UPDATE thalamus_debug_tool_credentials SET revoked_at=clock_timestamp() WHERE session_id=$1 AND user_id=$2 AND team_id=$3 AND revoked_at IS NULL")
            .bind(id).bind(identity.user_id).bind(identity.team_id).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO thalamus_debug_tool_credentials (id,session_id,user_id,team_id,digest,audience,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(credential_id).bind(id).bind(identity.user_id).bind(identity.team_id).bind(digest.as_slice()).bind(audience).bind(expires_at)
            .execute(&mut **tx).await?;
        Ok(IssuedToolCredential {
            id: credential_id,
            secret,
        })
    }

    pub async fn revoke_tool_credential(
        &self,
        identity: &DebugIdentity,
        session_id: Uuid,
        credential_id: Uuid,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        self.revoke_tool_credential_in_transaction(&mut tx, identity, session_id, credential_id)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn revoke_tool_credential_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        identity: &DebugIdentity,
        session_id: Uuid,
        credential_id: Uuid,
    ) -> Result<()> {
        non_nil(credential_id, "credential ID")?;
        authorize(tx, identity).await?;
        session_guard_row(tx, identity, session_id).await?;
        let found: Option<Uuid> = sqlx::query_scalar("UPDATE thalamus_debug_tool_credentials SET revoked_at=COALESCE(revoked_at,clock_timestamp()) WHERE id=$1 AND session_id=$2 AND user_id=$3 AND team_id=$4 RETURNING id")
            .bind(credential_id).bind(session_id).bind(identity.user_id).bind(identity.team_id).fetch_optional(&mut **tx).await?;
        if found.is_none() {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Authenticate a bearer without spending a tool call, e.g. for tools/list.
    /// The returned binding belongs only in authenticated server request state.
    pub async fn lookup_tool_credential(
        &self,
        secret: &str,
        audience: &str,
    ) -> Result<ToolCredentialBinding> {
        let mut tx = self.pool.begin().await?;
        let binding = self
            .lookup_tool_credential_in_transaction(&mut tx, secret, audience)
            .await?;
        tx.commit().await?;
        Ok(binding)
    }

    pub async fn lookup_tool_credential_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        secret: &str,
        audience: &str,
    ) -> Result<ToolCredentialBinding> {
        let digest = credential_digest(secret)?;
        let owner = owner_by_digest(tx, &digest).await?;
        let (user, _) = check_credential(tx, owner, audience).await?;
        Ok(ToolCredentialBinding {
            credential_id: owner.id,
            identity: owner.identity,
            session_id: owner.session_id,
            issuer: user.try_get("issuer")?,
            subject: user.try_get("subject")?,
        })
    }

    /// Consume one call using a credential ID obtained exclusively from the
    /// authenticated request's lookup result, never from the caller. Each
    /// dispatch and retry consumes a call; no client nonce can bypass billing.
    pub async fn consume_tool_call_by_credential(
        &self,
        credential_id: Uuid,
        audience: &str,
    ) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let session = self
            .consume_tool_call_by_credential_in_transaction(&mut tx, credential_id, audience)
            .await?;
        tx.commit().await?;
        Ok(session)
    }

    pub async fn consume_tool_call_by_credential_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        credential_id: Uuid,
        audience: &str,
    ) -> Result<DebugSession> {
        let owner = owner_by_id(tx, credential_id).await?;
        let (_, row) = check_credential(tx, owner, audience).await?;
        consume_guarded(
            tx,
            &owner.identity,
            owner.session_id,
            &row,
            Some((owner.id, audience)),
        )
        .await
    }

    /// Bearer variant for a host which retains the authenticated credential only
    /// in memory. It performs lookup and budget consumption in one transaction.
    pub async fn consume_credentialed_tool_call(
        &self,
        secret: &str,
        audience: &str,
    ) -> Result<DebugSession> {
        let mut tx = self.pool.begin().await?;
        let digest = credential_digest(secret)?;
        let owner = owner_by_digest(&mut tx, &digest).await?;
        let (_, row) = check_credential(&mut tx, owner, audience).await?;
        let session = consume_guarded(
            &mut tx,
            &owner.identity,
            owner.session_id,
            &row,
            Some((owner.id, audience)),
        )
        .await?;
        tx.commit().await?;
        Ok(session)
    }
}

async fn consume_locked(
    tx: &mut Transaction<'_, Postgres>,
    identity: &DebugIdentity,
    id: Uuid,
    credential: Option<(Uuid, &str)>,
) -> Result<DebugSession> {
    let row = session_guard_row(tx, identity, id).await?;
    consume_guarded(tx, identity, id, &row, credential).await
}

async fn consume_guarded(
    tx: &mut Transaction<'_, Postgres>,
    identity: &DebugIdentity,
    id: Uuid,
    row: &PgRow,
    credential: Option<(Uuid, &str)>,
) -> Result<DebugSession> {
    let state = SessionState::parse(row.try_get("state")?)?;
    if state != SessionState::Running || !row.try_get::<bool, _>("before_deadline")? {
        return Err(StoreError::Inactive);
    }
    if row.try_get::<i64, _>("tool_calls_used")? >= row.try_get::<i64, _>("tool_budget")? {
        return Err(StoreError::BudgetExhausted);
    }
    let (credential_id, audience) = match credential {
        Some((id, audience)) => (Some(id), Some(audience)),
        None => (None, None),
    };
    let row = sqlx::query("UPDATE thalamus_debug_sessions s SET tool_calls_used=tool_calls_used+1,updated_at=clock_timestamp() WHERE s.id=$1 AND s.user_id=$2 AND s.team_id=$3 AND s.state='running' AND s.deadline>clock_timestamp() AND s.tool_calls_used<s.tool_budget AND ($4::uuid IS NULL OR EXISTS (SELECT 1 FROM thalamus_debug_tool_credentials c WHERE c.id=$4 AND c.session_id=s.id AND c.user_id=s.user_id AND c.team_id=s.team_id AND c.audience=$5 AND c.revoked_at IS NULL AND c.expires_at>clock_timestamp())) RETURNING s.*")
        .bind(id).bind(identity.user_id).bind(identity.team_id).bind(credential_id).bind(audience)
        .fetch_optional(&mut **tx).await?;
    match row {
        Some(row) => decode_session(&row),
        None if credential_id.is_some() => Err(StoreError::InvalidCredential),
        None => Err(StoreError::Inactive),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lifecycle_has_no_reopening_or_pending_success() {
        assert!(SessionState::Pending.permits(SessionState::Running));
        assert!(SessionState::Pending.permits(SessionState::Failed));
        assert!(SessionState::Running.permits(SessionState::Completed));
        assert!(!SessionState::Pending.permits(SessionState::Completed));
        assert!(!SessionState::Completed.permits(SessionState::Running));
        assert!(!SessionState::Failed.permits(SessionState::Failed));
        assert!(SessionState::TimedOut.is_terminal());
    }

    #[test]
    fn only_full_immutable_lowercase_commits_are_accepted() {
        assert!(valid_revision(&"a".repeat(40)));
        assert!(valid_revision(&"0".repeat(64)));
        assert!(!valid_revision("main"));
        assert!(!valid_revision(&"a".repeat(39)));
        assert!(!valid_revision(&"A".repeat(40)));
    }

    #[test]
    fn namespace_and_upstream_id_cannot_change_http_or_dns_targets() {
        assert!(valid_namespace("service-namespace"));
        assert!(!valid_namespace("-service"));
        assert!(!valid_namespace("Service"));
        assert!(!valid_namespace("service.example"));
        assert!(valid_upstream_id("ses_123-abc"));
        assert!(!valid_upstream_id("../other"));
        assert!(!valid_upstream_id("ses_1?x=2"));
    }

    #[test]
    fn events_require_bounded_objects_and_safe_keys() {
        let event = NewDebugEvent {
            client_request_key: "req-1".into(),
            kind: "opencode.event".into(),
            payload: json!({"type":"message"}),
        };
        assert!(validate_event(&event).is_ok());
        assert!(validate_event(&NewDebugEvent {
            payload: json!({}),
            ..event.clone()
        })
        .is_err());
        assert!(validate_event(&NewDebugEvent {
            kind: "bad/path".into(),
            ..event.clone()
        })
        .is_err());
        assert!(validate_event(&NewDebugEvent {
            client_request_key: "bad key".into(),
            ..event.clone()
        })
        .is_err());
        assert!(validate_event(&NewDebugEvent {
            payload: json!({"text":"x".repeat(MAX_JSON_BYTES)}),
            ..event
        })
        .is_err());
    }

    #[test]
    fn json_rejects_known_credentials_even_when_nested() {
        let model_key = format!("thl_{}", "a".repeat(43));
        let tool_key = format!("thld_{}", "b".repeat(64));
        assert!(json_object(
            &json!({"nested":[{"message":format!("Bearer {model_key}")}]}),
            "context"
        )
        .is_err());
        assert!(json_object(&json!({"nested":{(tool_key):"value"}}), "event").is_err());
        assert!(json_object(&json!({"message":"the thl_ prefix is reserved"}), "event").is_ok());
    }

    #[test]
    fn credential_format_is_strict_and_digest_is_deterministic() {
        let token = format!("{CREDENTIAL_PREFIX}{}", "a".repeat(64));
        let other = format!("{CREDENTIAL_PREFIX}{}", "b".repeat(64));
        assert_eq!(
            credential_digest(&token).unwrap(),
            credential_digest(&token).unwrap()
        );
        assert_ne!(
            credential_digest(&token).unwrap(),
            credential_digest(&other).unwrap()
        );
        assert!(credential_digest("thld_short").is_err());
        assert!(credential_digest(&format!("{CREDENTIAL_PREFIX}{}", "A".repeat(64))).is_err());
    }

    #[test]
    fn timestamps_are_normalized_to_database_microseconds() {
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 123_456_789).unwrap();
        assert_eq!(
            db_timestamp(time).unwrap().timestamp_subsec_nanos(),
            123_456_000
        );
    }

    async fn database_fixture(
        pool: &PgPool,
        tool_budget: i64,
    ) -> (DebugStore, DebugIdentity, NewDebugSession, DebugSession) {
        sqlx::raw_sql(include_str!(
            "../../thalamus-identity/migrations/0001_identity.sql"
        ))
        .execute(pool)
        .await
        .unwrap();
        let store = DebugStore::new(pool.clone());
        store.migrate().await.unwrap();
        let identity = DebugIdentity {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
        };
        let project_id = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,'https://issuer.example','subject-1')")
            .bind(identity.user_id).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
            .bind(identity.team_id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')",
        )
        .bind(identity.user_id)
        .bind(identity.team_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO identity_projects(id,team_id,name) VALUES ($1,$2,'project')")
            .bind(project_id)
            .bind(identity.team_id)
            .execute(pool)
            .await
            .unwrap();
        let input = NewDebugSession {
            client_request_key: "session-request".into(),
            project_id,
            service_id: "test-service".into(),
            run_id: Some(Uuid::new_v4()),
            model: "test-model".into(),
            checkout_revision: "a".repeat(40),
            job_namespace: "test-namespace".into(),
            deadline: Utc::now() + chrono::Duration::hours(1),
            model_budget: 1000,
            tool_budget,
            service_context: json!({"service":"test-service"}),
            failing_evidence: json!({"failure":"unhealthy"}),
        };
        let session = store.create(&identity, input.clone()).await.unwrap();
        (store, identity, input, session)
    }

    #[sqlx::test(migrations = false)]
    async fn postgres_idempotency_sequence_and_owner_isolation(pool: PgPool) {
        let (store, identity, input, session) = database_fixture(&pool, 3).await;
        assert_eq!(
            store.create(&identity, input.clone()).await.unwrap().id,
            session.id
        );
        let changed = NewDebugSession {
            model: "different".into(),
            ..input
        };
        assert!(matches!(
            store.create(&identity, changed).await,
            Err(StoreError::IdempotencyConflict)
        ));

        let event = NewDebugEvent {
            client_request_key: "event-1".into(),
            kind: "opencode.event".into(),
            payload: json!({"message":"first"}),
        };
        assert_eq!(
            store
                .append_event(&identity, session.id, event.clone())
                .await
                .unwrap()
                .sequence,
            1
        );
        assert_eq!(
            store
                .append_event(&identity, session.id, event.clone())
                .await
                .unwrap()
                .sequence,
            1
        );
        assert!(matches!(
            store
                .append_event(
                    &identity,
                    session.id,
                    NewDebugEvent {
                        payload: json!({"message":"changed"}),
                        ..event
                    }
                )
                .await,
            Err(StoreError::IdempotencyConflict)
        ));
        let event = NewDebugEvent {
            client_request_key: "event-2".into(),
            kind: "opencode.event".into(),
            payload: json!({"message":"second"}),
        };
        assert_eq!(
            store
                .append_event(&identity, session.id, event)
                .await
                .unwrap()
                .sequence,
            2
        );
        let page = store.events(&identity, session.id, 1, 10).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].sequence, 2);

        let other = DebugIdentity {
            user_id: Uuid::new_v4(),
            team_id: identity.team_id,
        };
        sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,'https://issuer.example','subject-2')")
            .bind(other.user_id).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')",
        )
        .bind(other.user_id)
        .bind(other.team_id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            store.get(&other, session.id).await,
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.events(&other, session.id, 0, 10).await,
            Err(StoreError::NotFound)
        ));
        sqlx::query("DELETE FROM identity_memberships WHERE user_id=$1 AND team_id=$2")
            .bind(identity.user_id)
            .bind(identity.team_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store.get(&identity, session.id).await,
            Err(StoreError::Forbidden)
        ));
    }

    #[sqlx::test(migrations = false)]
    async fn postgres_credentials_check_binding_rotation_expiry_and_membership(pool: PgPool) {
        let (store, identity, _, session) = database_fixture(&pool, 3).await;
        store
            .transition(
                &identity,
                session.id,
                SessionState::Pending,
                SessionState::Running,
                None,
            )
            .await
            .unwrap();
        let issued = store
            .issue_tool_credential(&identity, session.id, "thalamus-mcp", session.deadline)
            .await
            .unwrap();
        let binding = store
            .lookup_tool_credential(&issued.secret, "thalamus-mcp")
            .await
            .unwrap();
        assert_eq!(binding.credential_id, issued.id);
        assert_eq!(binding.session_id, session.id);
        assert_eq!(binding.identity, identity);
        assert_eq!(binding.issuer, "https://issuer.example");
        assert_eq!(binding.subject, "subject-1");
        assert!(matches!(
            store
                .lookup_tool_credential(&issued.secret, "other-audience")
                .await,
            Err(StoreError::InvalidCredential)
        ));
        let digest: Vec<u8> =
            sqlx::query_scalar("SELECT digest FROM thalamus_debug_tool_credentials WHERE id=$1")
                .bind(issued.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(digest, credential_digest(&issued.secret).unwrap().to_vec());

        let rotated = store
            .issue_tool_credential(&identity, session.id, "thalamus-mcp", session.deadline)
            .await
            .unwrap();
        assert!(matches!(
            store
                .lookup_tool_credential(&issued.secret, "thalamus-mcp")
                .await,
            Err(StoreError::InvalidCredential)
        ));
        assert!(matches!(
            store
                .consume_tool_call_by_credential(issued.id, "thalamus-mcp")
                .await,
            Err(StoreError::InvalidCredential)
        ));
        store
            .revoke_tool_credential(&identity, session.id, rotated.id)
            .await
            .unwrap();
        assert!(matches!(
            store
                .lookup_tool_credential(&rotated.secret, "thalamus-mcp")
                .await,
            Err(StoreError::InvalidCredential)
        ));

        let expired = store
            .issue_tool_credential(&identity, session.id, "thalamus-mcp", session.deadline)
            .await
            .unwrap();
        sqlx::query("UPDATE thalamus_debug_tool_credentials SET issued_at=clock_timestamp()-interval '2 minutes',expires_at=clock_timestamp()-interval '1 minute' WHERE id=$1")
            .bind(expired.id).execute(&pool).await.unwrap();
        assert!(matches!(
            store
                .lookup_tool_credential(&expired.secret, "thalamus-mcp")
                .await,
            Err(StoreError::InvalidCredential)
        ));

        let member = store
            .issue_tool_credential(&identity, session.id, "thalamus-mcp", session.deadline)
            .await
            .unwrap();
        sqlx::query("DELETE FROM identity_memberships WHERE user_id=$1 AND team_id=$2")
            .bind(identity.user_id)
            .bind(identity.team_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store
                .lookup_tool_credential(&member.secret, "thalamus-mcp")
                .await,
            Err(StoreError::InvalidCredential)
        ));
        assert!(matches!(
            store
                .consume_tool_call_by_credential(member.id, "thalamus-mcp")
                .await,
            Err(StoreError::InvalidCredential)
        ));
    }

    #[sqlx::test(migrations = false)]
    async fn postgres_concurrent_tool_calls_cannot_overspend(pool: PgPool) {
        let (store, identity, _, session) = database_fixture(&pool, 3).await;
        store
            .transition(
                &identity,
                session.id,
                SessionState::Pending,
                SessionState::Running,
                None,
            )
            .await
            .unwrap();
        let issued = store
            .issue_tool_credential(&identity, session.id, "thalamus-mcp", session.deadline)
            .await
            .unwrap();
        let attempts =
            (0..12).map(|_| store.consume_tool_call_by_credential(issued.id, "thalamus-mcp"));
        let results = futures::future::join_all(attempts).await;
        let succeeded = results.iter().filter(|result| result.is_ok()).count();
        let exhausted = results
            .iter()
            .filter(|result| matches!(result, Err(StoreError::BudgetExhausted)))
            .count();
        assert_eq!(succeeded, 3);
        assert_eq!(exhausted, 9);
        assert_eq!(
            store
                .get(&identity, session.id)
                .await
                .unwrap()
                .tool_calls_used,
            3
        );
    }

    #[sqlx::test(migrations = false)]
    async fn worker_recovers_revoked_membership_and_retains_cleanup_until_confirmed(pool: PgPool) {
        let (store, identity, mut input, session) = database_fixture(&pool, 3).await;
        store
            .transition(
                &identity,
                session.id,
                SessionState::Pending,
                SessionState::Running,
                None,
            )
            .await
            .unwrap();
        let issued = store
            .issue_tool_credential(&identity, session.id, "thalamus-mcp", session.deadline)
            .await
            .unwrap();
        input.client_request_key = "second-session".into();
        let second = store.create(&identity, input).await.unwrap();
        let worker = WorkerStore::new(pool.clone());
        let page1 = worker.list_actionable(None, 1).await.unwrap();
        let page2 = worker
            .list_actionable(Some(WorkerSessionCursor::from(&page1[0])), 1)
            .await
            .unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page2.len(), 1);
        assert_ne!(page1[0].id, page2[0].id);
        assert!(worker
            .finalize_expired_or_inactive(&identity, session.id)
            .await
            .unwrap()
            .is_none());

        sqlx::query("DELETE FROM identity_memberships WHERE user_id=$1 AND team_id=$2")
            .bind(identity.user_id)
            .bind(identity.team_id)
            .execute(&pool)
            .await
            .unwrap();
        let (finalized, reason) = worker
            .finalize_expired_or_inactive(&identity, session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reason, WorkerFinalizationReason::TeamMembershipRevoked);
        assert_eq!(finalized.state, SessionState::Failed);
        assert!(finalized.cleanup_required);
        let revoked: bool = sqlx::query_scalar(
            "SELECT revoked_at IS NOT NULL FROM thalamus_debug_tool_credentials WHERE id=$1",
        )
        .bind(issued.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(revoked);
        let outcome: Value =
            sqlx::query_scalar("SELECT outcome FROM thalamus_debug_sessions WHERE id=$1")
                .bind(session.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(outcome, json!({"reason":"team_membership_revoked"}));
        let pending_cleanup = worker.list_actionable(None, 100).await.unwrap();
        assert!(pending_cleanup
            .iter()
            .any(|item| item.id == session.id && item.cleanup_required));
        worker
            .mark_cleanup_complete(&identity, session.id)
            .await
            .unwrap();
        worker
            .mark_cleanup_complete(&identity, session.id)
            .await
            .unwrap();
        let remaining = worker.list_actionable(None, 100).await.unwrap();
        assert!(remaining.iter().all(|item| item.id != session.id));
        assert!(remaining.iter().any(|item| item.id == second.id));
    }

    #[sqlx::test(migrations = false)]
    async fn worker_finalizes_deadlines_as_timed_out_and_disabled_owners_as_failed(pool: PgPool) {
        let (store, identity, mut input, session) = database_fixture(&pool, 3).await;
        let worker = WorkerStore::new(pool.clone());
        sqlx::query("UPDATE thalamus_debug_sessions SET created_at=clock_timestamp()-interval '2 hours',deadline=clock_timestamp()-interval '1 hour' WHERE id=$1")
            .bind(session.id).execute(&pool).await.unwrap();
        let (expired, reason) = worker
            .finalize_expired_or_inactive(&identity, session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reason, WorkerFinalizationReason::DeadlineExpired);
        assert_eq!(expired.state, SessionState::TimedOut);

        input.client_request_key = "disabled-session".into();
        let disabled_session = store.create(&identity, input).await.unwrap();
        sqlx::query("UPDATE identity_users SET disabled=true WHERE id=$1")
            .bind(identity.user_id)
            .execute(&pool)
            .await
            .unwrap();
        let (disabled, reason) = worker
            .finalize_expired_or_inactive(&identity, disabled_session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reason, WorkerFinalizationReason::OwnerDisabled);
        assert_eq!(disabled.state, SessionState::Failed);
        assert!(disabled.cleanup_required);
    }
}
