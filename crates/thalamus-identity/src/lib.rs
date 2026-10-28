//! OIDC identity and Postgres-backed authorization. The caller must protect browser
//! cookies (Secure, HttpOnly, SameSite) and bind the returned login state to the browser.
use std::collections::HashMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use openidconnect::{
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, HttpRequest, HttpResponse, IssuerUrl,
    Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

pub mod keycloak;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("OIDC error: {0}")]
    Oidc(String),
    #[error("invalid or expired credentials")]
    Unauthorized,
    #[error("permission denied")]
    Forbidden,
    #[error("invalid input: {0}")]
    Invalid(&'static str),
    #[error("not found")]
    NotFound,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Apply this migration with the application's normal migration runner.
pub const INITIAL_MIGRATION: &str = include_str!("../migrations/0001_identity.sql");
/// Apply after `INITIAL_MIGRATION`.
pub const WORKER_KEYS_MIGRATION: &str = include_str!("../migrations/0002_worker_keys.sql");
/// Apply after `WORKER_KEYS_MIGRATION`.
pub const BUDGET_CAPS_MIGRATION: &str = include_str!("../migrations/0003_budget_caps.sql");
/// Apply after `BUDGET_CAPS_MIGRATION`.
pub const IDENTITY_BINDING_MIGRATION: &str =
    include_str!("../migrations/0004_identity_binding.sql");

/// Exact group names and existing team IDs. Team-admin membership takes precedence.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct GroupMapping {
    pub platform_admin_group: Option<String>,
    pub team_admin_groups: HashMap<String, Uuid>,
    pub member_groups: HashMap<String, Uuid>,
}

pub struct Config {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    /// Optional PEM CA bundle added to the built-in public roots.
    pub ca_pem: Option<Vec<u8>>,
    pub session_signing_key: Vec<u8>,
    pub session_lifetime: Duration,
    pub groups: GroupMapping,
}

pub struct IdentityService {
    db: PgPool,
    client: CoreClient,
    http_client: reqwest::Client,
    issuer: String,
    signing_key: Vec<u8>,
    session_lifetime: Duration,
    groups: GroupMapping,
}
/// Only pass OIDC identities after successful resource-token verification at the
/// trusted transport boundary. Subjects are never provisioned by this API.
pub enum VerifiedPrincipal<'a> {
    Session(&'a str),
    Oidc { issuer: &'a str, subject: &'a str },
}

/// Holds the identity row lock until the authorized mutation and its audit commit.
pub struct AuthorizedIdentity<'a> {
    tx: Transaction<'a, Postgres>,
    actor: Uuid,
}

impl<'a> AuthorizedIdentity<'a> {
    pub fn actor_id(&self) -> Uuid {
        self.actor
    }
    pub fn transaction(&mut self) -> &mut Transaction<'a, Postgres> {
        &mut self.tx
    }
    pub async fn audit(&mut self, agent: Option<&str>, action: &str, target: &str) -> Result<()> {
        audit(&mut self.tx, Some(self.actor), agent, action, target).await
    }
    /// Issue only after the caller has checked the committed service policy.
    /// The guard retains the identity lock and uncommitted audit until the
    /// caller rechecks that policy and commits.
    pub async fn issue_key(&mut self, input: NewKey, agent: Option<&str>) -> Result<IssuedKey> {
        issue_key_locked(&mut self.tx, self.actor, input, agent).await
    }
    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}

pub struct LoginStart {
    pub authorization_url: String,
    /// Store in a short-lived, signed browser cookie and supply it to callback.
    pub state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    Personal,
    Project,
    Job,
    Debug,
}
impl KeyKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Project => "project",
            Self::Job => "job",
            Self::Debug => "debug",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewKey {
    pub name: String,
    pub team_id: Uuid,
    pub project_id: Option<Uuid>,
    pub kind: KeyKind,
    pub allowed_services: Vec<String>,
    pub budget: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct IssuedKey {
    pub key: KeyMetadata,
    pub secret: String,
}

#[derive(Debug, Serialize)]
pub struct KeyMetadata {
    pub id: Uuid,
    pub name: String,
    pub user_id: Uuid,
    pub creator_id: Uuid,
    pub team_id: Uuid,
    pub project_id: Option<Uuid>,
    pub kind: String,
    pub allowed_services: Vec<String>,
    pub budget: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Privileged worker-only projection; never return this type from a public listing.
#[derive(Debug)]
pub struct ProjectionKey {
    pub metadata: KeyMetadata,
    pub digest: Vec<u8>,
    pub cost_centre: Option<String>,
    pub class: String,
    pub project_budget: Option<i64>,
    pub team_budget: Option<i64>,
    pub budget_window: Option<String>,
    pub run_id: Option<Uuid>,
    pub deployment: Option<String>,
    pub run_kind: Option<String>,
    pub request_budget: Option<i64>,
    pub debug_session_id: Option<Uuid>,
    pub debug_model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct Project {
    pub id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    pub cost_centre: Option<String>,
    pub budget: Option<f64>,
    pub traffic_class: String,
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    actor: Option<Uuid>,
    agent: Option<&str>,
    action: &str,
    target: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO identity_audit(actor_id,agent,action,target) VALUES ($1,$2,$3,$4)")
        .bind(actor)
        .bind(agent)
        .bind(action)
        .bind(target)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn sign(key: &[u8], id: Uuid) -> String {
    let value = id.to_string();
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(value.as_bytes());
    format!(
        "{}.{}",
        value,
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}
fn verify(key: &[u8], token: &str) -> Result<Uuid> {
    let (id, tag) = token.split_once('.').ok_or(Error::Unauthorized)?;
    let tag = URL_SAFE_NO_PAD
        .decode(tag)
        .map_err(|_| Error::Unauthorized)?;
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| Error::Unauthorized)?;
    mac.update(id.as_bytes());
    mac.verify_slice(&tag).map_err(|_| Error::Unauthorized)?;
    Uuid::parse_str(id).map_err(|_| Error::Unauthorized)
}
fn token_payload(token: &str) -> Result<serde_json::Value> {
    let part = token.split('.').nth(1).ok_or(Error::Unauthorized)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| Error::Unauthorized)?;
    serde_json::from_slice(&bytes).map_err(|_| Error::Unauthorized)
}
fn role_mapping(mapping: &GroupMapping, groups: &[String]) -> (bool, HashMap<Uuid, &'static str>) {
    let admin = mapping
        .platform_admin_group
        .as_ref()
        .is_some_and(|g| groups.contains(g));
    let mut teams = HashMap::new();
    for group in groups {
        if let Some(team) = mapping.member_groups.get(group) {
            teams.entry(*team).or_insert("member");
        }
        if let Some(team) = mapping.team_admin_groups.get(group) {
            teams.insert(*team, "team_admin");
        }
    }
    (admin, teams)
}

async fn lock_issuer(tx: &mut Transaction<'_, Postgres>, issuer: &str) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("thalamus/idp/{issuer}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// Every authorization-changing operation locks the user row first. The unique
// issuer/subject upsert also serializes first login against a disable tombstone.
async fn apply_login_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    subject: &str,
    issued_at: DateTime<Utc>,
    admin: bool,
    teams: HashMap<Uuid, &'static str>,
) -> Result<Uuid> {
    // PostgreSQL stores timestamps at microsecond precision. Compare at that
    // same precision so sub-microsecond differences cannot cross the fence.
    let issued_at =
        DateTime::from_timestamp_micros(issued_at.timestamp_micros()).ok_or(Error::Unauthorized)?;
    lock_issuer(tx, issuer).await?;
    let snapshot_time: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT observed_at FROM identity_idp_projection WHERE issuer=$1")
            .bind(issuer)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(observed) = snapshot_time.filter(|observed| issued_at <= *observed) {
        // ID-token iat has second precision. A token from the same second as a
        // newer authoritative observation may authenticate the subject, but
        // must never restore its older groups. Require a live user whose roles
        // were applied at or after that observation, and leave them unchanged.
        if observed <= Utc::now() - Duration::minutes(10) {
            return Err(Error::Unauthorized);
        }
        let row = sqlx::query("SELECT id,disabled,groups_issued_at FROM identity_users WHERE issuer=$1 AND subject=$2 FOR UPDATE")
            .bind(issuer).bind(subject).fetch_optional(&mut **tx).await?
            .ok_or(Error::Unauthorized)?;
        let previous: Option<DateTime<Utc>> = row.try_get("groups_issued_at")?;
        if row.try_get::<bool, _>("disabled")? || previous.is_none_or(|time| time < observed) {
            return Err(Error::Unauthorized);
        }
        return Ok(row.try_get("id")?);
    }
    sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,$2,$3) ON CONFLICT (issuer,subject) DO NOTHING")
        .bind(Uuid::new_v4()).bind(issuer).bind(subject).execute(&mut **tx).await?;
    let row = sqlx::query("SELECT id,disabled,groups_issued_at FROM identity_users WHERE issuer=$1 AND subject=$2 FOR UPDATE")
        .bind(issuer).bind(subject).fetch_one(&mut **tx).await?;
    let user: Uuid = row.try_get("id")?;
    let previous: Option<DateTime<Utc>> = row.try_get("groups_issued_at")?;
    if row.try_get::<bool, _>("disabled")? || previous.is_some_and(|time| time > issued_at) {
        return Err(Error::Unauthorized);
    }
    // Equal-second snapshots have no trustworthy ordering. Clear privileges
    // rather than allow either snapshot to restore privileges from the other.
    let equal = previous == Some(issued_at);
    sqlx::query("UPDATE identity_users SET platform_admin=$2,groups_issued_at=$3 WHERE id=$1")
        .bind(user)
        .bind(admin && !equal)
        .bind(issued_at)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM identity_memberships WHERE user_id=$1")
        .bind(user)
        .execute(&mut **tx)
        .await?;
    if !equal {
        for (team, role) in teams {
            sqlx::query("INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,$3)")
                .bind(user)
                .bind(team)
                .bind(role)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(user)
}

async fn lock_session(
    tx: &mut Transaction<'_, Postgres>,
    signing_key: &[u8],
    session: &str,
) -> Result<Uuid> {
    let id = verify(signing_key, session)?;
    // Lock only the user, before touching sessions, memberships, or keys. Recheck
    // the session after the lock, since a concurrent logout may have removed it.
    let user: Uuid = sqlx::query_scalar("SELECT u.id FROM identity_users u JOIN identity_sessions s ON s.user_id=u.id WHERE s.id=$1 FOR UPDATE OF u")
        .bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::Unauthorized)?;
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_sessions s JOIN identity_users u ON u.id=s.user_id WHERE s.id=$1 AND s.user_id=$2 AND s.expires_at>now() AND NOT u.disabled AND NOT EXISTS (SELECT 1 FROM identity_idp_projection p WHERE p.issuer=u.issuer AND p.observed_at<=clock_timestamp()-interval '10 minutes'))")
        .bind(id).bind(user).fetch_one(&mut **tx).await?;
    if !valid {
        return Err(Error::Unauthorized);
    }
    Ok(user)
}

async fn authorize_locked(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    team: Uuid,
    admin_required: bool,
) -> Result<()> {
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$2 WHERE u.id=$1 AND NOT u.disabled AND NOT EXISTS (SELECT 1 FROM identity_idp_projection p WHERE p.issuer=u.issuer AND p.observed_at<=clock_timestamp()-interval '10 minutes') AND (u.platform_admin OR (m.role IS NOT NULL AND (NOT $3 OR m.role='team_admin'))))")
        .bind(user).bind(team).bind(admin_required).fetch_one(&mut **tx).await?;
    if allowed {
        Ok(())
    } else {
        Err(Error::Forbidden)
    }
}

async fn require_platform_admin_locked(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<()> {
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_users u WHERE u.id=$1 AND NOT u.disabled AND u.platform_admin AND NOT EXISTS (SELECT 1 FROM identity_idp_projection p WHERE p.issuer=u.issuer AND p.observed_at<=clock_timestamp()-interval '10 minutes'))")
        .bind(user).fetch_one(&mut **tx).await?;
    if allowed {
        Ok(())
    } else {
        Err(Error::Forbidden)
    }
}
async fn check_budget_cap(
    tx: &mut Transaction<'_, Postgres>,
    team: Uuid,
    budget: i64,
) -> Result<()> {
    if budget < 0 {
        return Err(Error::Invalid("negative budget"));
    }
    // Lock the singleton before reading the caps; replacement takes the same
    // lock, so authorization and the write cannot race a cap update.
    let fresh: Option<bool> = sqlx::query_scalar("SELECT refreshed_at > clock_timestamp() - interval '60 seconds' FROM identity_budget_projection WHERE singleton=true FOR SHARE")
        .fetch_optional(&mut **tx).await?;
    if fresh != Some(true) {
        return Err(Error::Invalid("budget cap unavailable or stale"));
    }
    let cap: Option<i64> =
        sqlx::query_scalar("SELECT max_tokens FROM identity_budget_caps WHERE team_id=$1")
            .bind(team)
            .fetch_optional(&mut **tx)
            .await?;
    if cap.is_none_or(|cap| budget > cap) {
        return Err(Error::Invalid("budget exceeds or lacks team cap"));
    }
    Ok(())
}

fn valid_traffic_class(class: &str) -> bool {
    !class.is_empty()
        && class.len() <= 63
        && class
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && class
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

async fn check_class_cap(
    tx: &mut Transaction<'_, Postgres>,
    team: Uuid,
    class: &str,
) -> Result<()> {
    if !valid_traffic_class(class) {
        return Err(Error::Invalid("invalid traffic class"));
    }
    // Acquires the same snapshot lock used by cap replacement.
    check_budget_cap(tx, team, 0).await?;
    let allowed: bool = sqlx::query_scalar(
        "SELECT $2 = ANY(allowed_classes) FROM identity_budget_caps WHERE team_id=$1",
    )
    .bind(team)
    .bind(class)
    .fetch_one(&mut **tx)
    .await?;
    if !allowed {
        return Err(Error::Forbidden);
    }
    Ok(())
}

async fn issue_key_locked(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    input: NewKey,
    agent: Option<&str>,
) -> Result<IssuedKey> {
    authorize_locked(tx, user, input.team_id, false).await?;
    if input.expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
        return Err(Error::Invalid("expiry must be in the future"));
    }
    if input.name.trim().is_empty()
        || input.name.len() > 255
        || input.name.chars().any(char::is_control)
    {
        return Err(Error::Invalid("invalid key name"));
    }
    if matches!(input.kind, KeyKind::Job | KeyKind::Debug) {
        return Err(Error::Forbidden);
    }
    if input.kind != KeyKind::Personal && input.project_id.is_none() {
        return Err(Error::Invalid("project required"));
    }
    if input.budget.is_some_and(|v| v < 0)
        || input.allowed_services.iter().any(|s| s.trim().is_empty())
    {
        return Err(Error::Invalid("invalid key metadata"));
    }
    if let Some(budget) = input.budget {
        check_budget_cap(tx, input.team_id, budget).await?;
    }
    let mut random = [0u8; 32];
    OsRng.fill_bytes(&mut random);
    let secret = format!("thl_{}", URL_SAFE_NO_PAD.encode(random));
    let digest = Sha256::digest(secret.as_bytes());
    let id = Uuid::new_v4();
    let row = sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,name) VALUES ($1,$2,$3,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,revoked_at,created_at,name")
        .bind(id).bind(digest.as_slice()).bind(user).bind(input.team_id).bind(input.project_id).bind(input.kind.as_str()).bind(&input.allowed_services).bind(input.budget).bind(input.expires_at).bind(&input.name).fetch_one(&mut **tx).await?;
    let key = key_metadata(row)?;
    audit(tx, Some(user), agent, "key.issue", &id.to_string()).await?;
    Ok(IssuedKey { key, secret })
}

async fn disable_subject(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    subject: &str,
) -> Result<()> {
    lock_issuer(tx, issuer).await?;
    let user: Option<Uuid> = sqlx::query_scalar("INSERT INTO identity_users(id,issuer,subject,disabled) VALUES ($1,$2,$3,true) ON CONFLICT (issuer,subject) DO UPDATE SET disabled=true,platform_admin=false WHERE NOT identity_users.disabled RETURNING id")
        .bind(Uuid::new_v4()).bind(issuer).bind(subject).fetch_optional(&mut **tx).await?;
    if let Some(user) = user {
        sqlx::query("DELETE FROM identity_memberships WHERE user_id=$1")
            .bind(user)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM identity_sessions WHERE user_id=$1")
            .bind(user)
            .execute(&mut **tx)
            .await?;
        sqlx::query(
            "UPDATE identity_keys SET revoked_at=now() WHERE user_id=$1 AND revoked_at IS NULL",
        )
        .bind(user)
        .execute(&mut **tx)
        .await?;
        audit(tx, None, None, "user.offboard", &user.to_string()).await?;
    }
    Ok(())
}

fn oidc_http_client(ca_pem: Option<&[u8]>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(pem) = ca_pem {
        let certificates = reqwest::Certificate::from_pem_bundle(pem)
            .map_err(|_| Error::Invalid("invalid OIDC CA PEM bundle"))?;
        if certificates.is_empty() {
            return Err(Error::Invalid("invalid OIDC CA PEM bundle"));
        }
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    builder
        .build()
        .map_err(|e| Error::Oidc(format!("cannot construct OIDC HTTPS client: {e}")))
}

// The same client is used for discovery, JWKS, and the authorization-code exchange.
async fn oidc_request(
    client: &reqwest::Client,
    request: HttpRequest,
) -> std::result::Result<HttpResponse, openidconnect::reqwest::AsyncHttpClientError> {
    use openidconnect::reqwest::Error as HttpError;
    if request.url.scheme() != "https" {
        return Err(HttpError::Other("OIDC endpoints must use HTTPS".into()));
    }
    let response = client
        .request(request.method, request.url)
        .headers(request.headers)
        .body(request.body)
        .send()
        .await
        .map_err(HttpError::Reqwest)?;
    let status_code = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await.map_err(HttpError::Reqwest)?.to_vec();
    Ok(HttpResponse {
        status_code,
        headers,
        body,
    })
}

impl IdentityService {
    /// Discover the configured trusted provider. Use the same signing key on all replicas.
    pub async fn discover(db: PgPool, config: Config) -> Result<Self> {
        if config.session_signing_key.len() < 32 {
            return Err(Error::Invalid(
                "session signing key must be at least 32 bytes",
            ));
        }
        if config.session_lifetime <= Duration::zero() {
            return Err(Error::Invalid("session lifetime must be positive"));
        }
        let issuer =
            IssuerUrl::new(config.issuer.clone()).map_err(|e| Error::Oidc(e.to_string()))?;
        if !config.issuer.starts_with("https://") || !config.redirect_uri.starts_with("https://") {
            return Err(Error::Invalid("issuer and redirect URI must use HTTPS"));
        }
        let http_client = oidc_http_client(config.ca_pem.as_deref())?;
        let metadata = CoreProviderMetadata::discover_async(issuer, |request| {
            oidc_request(&http_client, request)
        })
        .await
        .map_err(|e| Error::Oidc(e.to_string()))?;
        let redirect =
            RedirectUrl::new(config.redirect_uri).map_err(|e| Error::Oidc(e.to_string()))?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.client_id),
            config.client_secret.map(ClientSecret::new),
        )
        .set_redirect_uri(redirect);
        Ok(Self {
            db,
            client,
            http_client,
            issuer: config.issuer,
            signing_key: config.session_signing_key,
            session_lifetime: config.session_lifetime,
            groups: config.groups,
        })
    }

    pub async fn begin_login(&self) -> Result<LoginStart> {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("groups".into()))
            .set_pkce_challenge(challenge)
            .url();
        sqlx::query("INSERT INTO identity_login_attempts(state,nonce,pkce_verifier,expires_at) VALUES ($1,$2,$3,now()+interval '10 minutes')")
            .bind(state.secret()).bind(nonce.secret()).bind(verifier.secret()).execute(&self.db).await?;
        Ok(LoginStart {
            authorization_url: url.to_string(),
            state: state.secret().to_owned(),
        })
    }

    /// The browser's independently stored state must match the callback's state.
    /// Attempts are consumed even if token exchange fails, preventing replay.
    /// Snapshot ordering relies on the provider's `iat` tracking group changes;
    /// equal issuance times conservatively clear privileges.
    pub async fn complete_login(
        &self,
        browser_state: &str,
        callback_state: &str,
        code: &str,
    ) -> Result<String> {
        if browser_state.is_empty() || browser_state != callback_state {
            return Err(Error::Unauthorized);
        }
        let row = sqlx::query("DELETE FROM identity_login_attempts WHERE state=$1 AND expires_at > now() RETURNING nonce,pkce_verifier")
            .bind(callback_state).fetch_optional(&self.db).await?.ok_or(Error::Unauthorized)?;
        let nonce = Nonce::new(row.try_get("nonce")?);
        let verifier = PkceCodeVerifier::new(row.try_get("pkce_verifier")?);
        let response = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .set_pkce_verifier(verifier)
            .request_async(|request| oidc_request(&self.http_client, request))
            .await
            .map_err(|e| Error::Oidc(e.to_string()))?;
        let token = response.id_token().ok_or(Error::Unauthorized)?;
        let claims = token
            .claims(&self.client.id_token_verifier(), &nonce)
            .map_err(|_| Error::Unauthorized)?;
        let subject = claims.subject().as_str().to_owned();
        // Read extension claims only after successful cryptographic validation.
        let payload = token_payload(&token.to_string())?;
        let groups: Vec<String> = match payload.get("groups") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|_| Error::Unauthorized)?,
            None => Vec::new(),
        };
        match payload.get("active") {
            Some(serde_json::Value::Bool(false)) => {
                self.sync_disabled(&self.issuer, &subject).await?;
                return Err(Error::Unauthorized);
            }
            Some(serde_json::Value::Bool(true)) | None => {}
            Some(_) => return Err(Error::Unauthorized),
        }
        let (admin, teams) = role_mapping(&self.groups, &groups);
        let mut tx = self.db.begin().await?;
        let user = apply_login_snapshot(
            &mut tx,
            &self.issuer,
            &subject,
            claims.issue_time(),
            admin,
            teams,
        )
        .await?;
        let session = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_sessions(id,user_id,expires_at) VALUES ($1,$2,$3)")
            .bind(session)
            .bind(user)
            .bind(Utc::now() + self.session_lifetime)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, Some(user), None, "login", &session.to_string()).await?;
        tx.commit().await?;
        Ok(sign(&self.signing_key, session))
    }

    pub async fn authenticate(&self, session: &str) -> Result<Uuid> {
        let id = verify(&self.signing_key, session)?;
        sqlx::query_scalar("SELECT s.user_id FROM identity_sessions s JOIN identity_users u ON u.id=s.user_id WHERE s.id=$1 AND s.expires_at>now() AND NOT u.disabled AND NOT EXISTS (SELECT 1 FROM identity_idp_projection p WHERE p.issuer=u.issuer AND p.observed_at<=clock_timestamp()-interval '10 minutes')")
            .bind(id).fetch_optional(&self.db).await?.ok_or(Error::Unauthorized)
    }

    pub async fn logout(&self, session: &str) -> Result<()> {
        let id = verify(&self.signing_key, session)?;
        let mut tx = self.db.begin().await?;
        let user = lock_session(&mut tx, &self.signing_key, session).await?;
        sqlx::query("DELETE FROM identity_sessions WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, Some(user), None, "logout", &id.to_string()).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn authorize(&self, user: Uuid, team: Uuid, admin_required: bool) -> Result<()> {
        let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$2 WHERE u.id=$1 AND NOT u.disabled AND NOT EXISTS (SELECT 1 FROM identity_idp_projection p WHERE p.issuer=u.issuer AND p.observed_at<=clock_timestamp()-interval '10 minutes') AND (u.platform_admin OR (m.role IS NOT NULL AND (NOT $3 OR m.role='team_admin'))))")
            .bind(user).bind(team).bind(admin_required).fetch_one(&self.db).await?;
        if allowed {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }

    async fn lock_principal(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        principal: VerifiedPrincipal<'_>,
    ) -> Result<Uuid> {
        match principal {
            VerifiedPrincipal::Session(session) => {
                lock_session(tx, &self.signing_key, session).await
            }
            VerifiedPrincipal::Oidc { issuer, subject } => {
                if issuer != self.issuer || subject.is_empty() {
                    return Err(Error::Unauthorized);
                }
                sqlx::query_scalar("SELECT id FROM identity_users u WHERE issuer=$1 AND subject=$2 AND NOT disabled AND NOT EXISTS (SELECT 1 FROM identity_idp_projection p WHERE p.issuer=u.issuer AND p.observed_at<=clock_timestamp()-interval '10 minutes') FOR UPDATE")
                    .bind(issuer).bind(subject).fetch_optional(&mut **tx).await?.ok_or(Error::Unauthorized)
            }
        }
    }

    /// Authenticate a verified principal for an unscoped read.
    pub async fn authenticate_principal(&self, principal: VerifiedPrincipal<'_>) -> Result<Uuid> {
        let mut tx = self.db.begin().await?;
        let actor = self.lock_principal(&mut tx, principal).await?;
        tx.commit().await?;
        Ok(actor)
    }

    /// Authorize a catalog-wide mutation without requiring a team.
    pub async fn begin_platform_admin(
        &self,
        principal: VerifiedPrincipal<'_>,
    ) -> Result<AuthorizedIdentity<'_>> {
        let mut tx = self.db.begin().await?;
        let actor = self.lock_principal(&mut tx, principal).await?;
        let admin: bool = sqlx::query_scalar(
            "SELECT platform_admin FROM identity_users WHERE id=$1 AND NOT disabled",
        )
        .bind(actor)
        .fetch_one(&mut *tx)
        .await?;
        if !admin {
            return Err(Error::Forbidden);
        }
        Ok(AuthorizedIdentity { tx, actor })
    }

    /// The caller must keep this guard until its database mutation and audit
    /// have committed, and use its transaction for both.
    pub async fn begin_authorized(
        &self,
        principal: VerifiedPrincipal<'_>,
        team: Uuid,
        admin_required: bool,
    ) -> Result<AuthorizedIdentity<'_>> {
        let mut tx = self.db.begin().await?;
        let actor = self.lock_principal(&mut tx, principal).await?;
        authorize_locked(&mut tx, actor, team, admin_required).await?;
        Ok(AuthorizedIdentity { tx, actor })
    }

    /// Authorize a request in another service using current database membership.
    pub async fn authorize_team(
        &self,
        session: &str,
        team: Uuid,
        admin_required: bool,
    ) -> Result<Uuid> {
        let user = self.authenticate(session).await?;
        self.authorize(user, team, admin_required).await?;
        Ok(user)
    }

    pub async fn list_teams(&self, session: &str) -> Result<Vec<Team>> {
        self.list_teams_principal(VerifiedPrincipal::Session(session))
            .await
    }

    pub async fn list_teams_principal(
        &self,
        principal: VerifiedPrincipal<'_>,
    ) -> Result<Vec<Team>> {
        let mut tx = self.db.begin().await?;
        let user = self.lock_principal(&mut tx, principal).await?;
        let rows = sqlx::query("SELECT t.id,t.name,CASE WHEN u.platform_admin THEN 'platform_admin' ELSE m.role END AS role FROM identity_users u JOIN identity_teams t ON true LEFT JOIN identity_memberships m ON m.team_id=t.id AND m.user_id=u.id WHERE u.id=$1 AND NOT u.disabled AND (u.platform_admin OR m.role IS NOT NULL) ORDER BY t.name")
            .bind(user).fetch_all(&mut *tx).await?;
        let teams = rows
            .into_iter()
            .map(|r| {
                Ok(Team {
                    id: r.try_get("id")?,
                    name: r.try_get("name")?,
                    role: r.try_get("role")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(teams)
    }

    pub async fn create_team(
        &self,
        session: &str,
        name: &str,
        agent: Option<&str>,
    ) -> Result<Uuid> {
        let mut tx = self.db.begin().await?;
        let user = lock_session(&mut tx, &self.signing_key, session).await?;
        let admin: bool =
            sqlx::query_scalar("SELECT platform_admin FROM identity_users WHERE id=$1")
                .bind(user)
                .fetch_one(&mut *tx)
                .await?;
        if !admin {
            return Err(Error::Forbidden);
        }
        if name.trim().is_empty() {
            return Err(Error::Invalid("empty team name"));
        }
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,$2)")
            .bind(id)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, Some(user), agent, "team.create", &id.to_string()).await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn create_project(
        &self,
        session: &str,
        team: Uuid,
        name: &str,
        cost_centre: Option<&str>,
        budget: Option<f64>,
        agent: Option<&str>,
    ) -> Result<Uuid> {
        self.create_project_principal(
            VerifiedPrincipal::Session(session),
            team,
            name,
            cost_centre,
            budget,
            agent,
        )
        .await
    }

    pub async fn create_project_principal(
        &self,
        principal: VerifiedPrincipal<'_>,
        team: Uuid,
        name: &str,
        cost_centre: Option<&str>,
        budget: Option<f64>,
        agent: Option<&str>,
    ) -> Result<Uuid> {
        let mut tx = self.db.begin().await?;
        let user = self.lock_principal(&mut tx, principal).await?;
        authorize_locked(&mut tx, user, team, true).await?;
        if name.trim().is_empty() {
            return Err(Error::Invalid("empty project name"));
        }
        if cost_centre.is_some_and(|value| {
            value.is_empty()
                || value.len() > 128
                || value.trim() != value
                || value.chars().any(char::is_control)
        }) {
            return Err(Error::Invalid("invalid cost centre"));
        }
        if let Some(value) = budget {
            if !value.is_finite()
                || value < 0.0
                || value.fract() != 0.0
                || value > 9_007_199_254_740_991.0
            {
                return Err(Error::Invalid("project budget must be whole tokens"));
            }
            check_budget_cap(&mut tx, team, value as i64).await?;
        }
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO identity_projects(id,team_id,name,cost_centre,budget) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(id)
        .bind(team)
        .bind(name)
        .bind(cost_centre)
        .bind(budget)
        .execute(&mut *tx)
        .await?;
        audit(
            &mut tx,
            Some(user),
            agent,
            "project.create",
            &id.to_string(),
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn list_projects(&self, session: &str, team: Uuid) -> Result<Vec<Project>> {
        self.list_projects_principal(VerifiedPrincipal::Session(session), team)
            .await
    }

    pub async fn list_projects_principal(
        &self,
        principal: VerifiedPrincipal<'_>,
        team: Uuid,
    ) -> Result<Vec<Project>> {
        let mut tx = self.db.begin().await?;
        let user = self.lock_principal(&mut tx, principal).await?;
        authorize_locked(&mut tx, user, team, false).await?;
        let rows = sqlx::query("SELECT id,team_id,name,cost_centre,budget,traffic_class FROM identity_projects WHERE team_id=$1 ORDER BY name").bind(team).fetch_all(&mut *tx).await?;
        let projects = rows
            .into_iter()
            .map(|r| {
                Ok(Project {
                    id: r.try_get("id")?,
                    team_id: r.try_get("team_id")?,
                    name: r.try_get("name")?,
                    cost_centre: r.try_get("cost_centre")?,
                    budget: r.try_get("budget")?,
                    traffic_class: r.try_get("traffic_class")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(projects)
    }

    /// Only platform administrators may assign a class, and only from the
    /// current reviewed cap for the project's team.
    pub async fn set_project_traffic_class_principal(
        &self,
        principal: VerifiedPrincipal<'_>,
        team: Uuid,
        project: Uuid,
        class: &str,
        agent: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let user = self.lock_principal(&mut tx, principal).await?;
        require_platform_admin_locked(&mut tx, user).await?;
        check_class_cap(&mut tx, team, class).await?;
        let changed =
            sqlx::query("UPDATE identity_projects SET traffic_class=$3 WHERE id=$1 AND team_id=$2")
                .bind(project)
                .bind(team)
                .bind(class)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if changed == 0 {
            return Err(Error::NotFound);
        }
        audit(
            &mut tx,
            Some(user),
            agent,
            "project.class",
            &project.to_string(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// List only the reviewed classes assignable to this project. This uses the
    /// same platform-admin and freshness checks as the mutation.
    pub async fn allowed_project_traffic_classes_principal(
        &self,
        principal: VerifiedPrincipal<'_>,
        team: Uuid,
        project: Uuid,
    ) -> Result<Vec<String>> {
        let mut tx = self.db.begin().await?;
        let user = self.lock_principal(&mut tx, principal).await?;
        require_platform_admin_locked(&mut tx, user).await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM identity_projects WHERE id=$1 AND team_id=$2)",
        )
        .bind(project)
        .bind(team)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            return Err(Error::NotFound);
        }
        check_budget_cap(&mut tx, team, 0).await?;
        let classes: Vec<String> =
            sqlx::query_scalar("SELECT allowed_classes FROM identity_budget_caps WHERE team_id=$1")
                .bind(team)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(classes)
    }

    pub async fn issue_key(
        &self,
        session: &str,
        input: NewKey,
        agent: Option<&str>,
    ) -> Result<IssuedKey> {
        self.issue_key_principal(VerifiedPrincipal::Session(session), input, agent)
            .await
    }

    pub async fn issue_key_principal(
        &self,
        principal: VerifiedPrincipal<'_>,
        input: NewKey,
        agent: Option<&str>,
    ) -> Result<IssuedKey> {
        let mut tx = self.db.begin().await?;
        let user = self.lock_principal(&mut tx, principal).await?;
        let issued = issue_key_locked(&mut tx, user, input, agent).await?;
        tx.commit().await?;
        Ok(issued)
    }

    pub async fn list_keys(&self, session: &str, team: Uuid) -> Result<Vec<KeyMetadata>> {
        self.list_keys_principal(VerifiedPrincipal::Session(session), team)
            .await
    }

    pub async fn list_keys_principal(
        &self,
        principal: VerifiedPrincipal<'_>,
        team: Uuid,
    ) -> Result<Vec<KeyMetadata>> {
        let mut tx = self.db.begin().await?;
        let user = self.lock_principal(&mut tx, principal).await?;
        authorize_locked(&mut tx, user, team, false).await?;
        let rows = sqlx::query("SELECT id,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,revoked_at,created_at,name FROM identity_keys WHERE team_id=$1 AND (user_id=$2 OR EXISTS (SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$1 WHERE u.id=$2 AND (u.platform_admin OR m.role='team_admin'))) ORDER BY created_at DESC")
            .bind(team).bind(user).fetch_all(&mut *tx).await?;
        let keys = rows
            .into_iter()
            .map(key_metadata)
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(keys)
    }

    pub async fn revoke_key(
        &self,
        session: &str,
        team: Uuid,
        id: Uuid,
        agent: Option<&str>,
    ) -> Result<()> {
        self.revoke_key_principal(VerifiedPrincipal::Session(session), team, id, agent)
            .await
    }

    pub async fn revoke_key_principal(
        &self,
        principal: VerifiedPrincipal<'_>,
        team: Uuid,
        id: Uuid,
        agent: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let user = self.lock_principal(&mut tx, principal).await?;
        authorize_locked(&mut tx, user, team, false).await?;
        let changed = sqlx::query("UPDATE identity_keys SET revoked_at=now() WHERE id=$1 AND team_id=$2 AND revoked_at IS NULL AND (user_id=$3 OR EXISTS (SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$2 WHERE u.id=$3 AND (u.platform_admin OR m.role='team_admin')))")
            .bind(id).bind(team).bind(user).execute(&mut *tx).await?.rows_affected();
        if changed == 0 {
            return Err(Error::NotFound);
        }
        audit(&mut tx, Some(user), agent, "key.revoke", &id.to_string()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Trusted worker operation: only call after an authoritative provider reports disabled.
    /// All keys, including project keys, are revoked rather than silently reassigned.
    pub async fn sync_disabled(&self, issuer: &str, subject: &str) -> Result<()> {
        if issuer != self.issuer {
            return Err(Error::Invalid("issuer mismatch"));
        }
        let mut tx = self.db.begin().await?;
        disable_subject(&mut tx, issuer, subject).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Periodic maintenance; key expiry is enforced by active_projection without deletion.
    pub async fn prune_expired_auth(&self) -> Result<()> {
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM identity_sessions WHERE expires_at <= now()")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM identity_login_attempts WHERE expires_at <= now()")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Trusted worker operation; contains hashes and must never be exposed to end users.
    pub async fn active_projection(&self) -> Result<Vec<ProjectionKey>> {
        active_projection(&self.db).await
    }
}
/// Database-only API for the authenticated internal worker. The caller must
/// authenticate the worker and supply the actual run owner and scope from its
/// durable run record, never from an untrusted request.
#[derive(Clone)]
pub struct WorkerIdentity {
    db: PgPool,
}
/// A verified Git catalog entry. Only the internal catalog reconciler may
/// supply these; request handlers must never treat user input as a cap.
#[derive(Clone, Debug, Deserialize)]
pub struct BudgetCap {
    pub team_id: Uuid,
    pub max_tokens: i64,
    pub unit: String,
    pub window: String,
    /// Classes this team may select, from the reviewed Git snapshot.
    pub allowed_classes: Vec<String>,
}

/// A complete authoritative observation from the configured trusted IdP.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdpSnapshot {
    pub issuer: String,
    pub observed_at: DateTime<Utc>,
    pub subjects: Vec<IdpSubjectSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdpSubjectSnapshot {
    pub subject: String,
    pub active: bool,
    pub groups: Vec<String>,
}

/// Scope and secret for a worker-managed run key.
#[derive(Clone, Copy)]
pub struct RunKeyRequest<'a> {
    pub run_id: Uuid,
    pub deployment: &'a str,
    pub run_kind: &'a str,
    pub request_budget: Option<i64>,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub project_id: Uuid,
    pub service: &'a str,
    pub expires_at: DateTime<Utc>,
    pub secret: &'a str,
    pub budget: i64,
}

fn valid_run_budgets(run_kind: &str, tokens: i64, requests: Option<i64>) -> bool {
    match run_kind {
        "validation" => (1..=1_000_000).contains(&tokens) && requests.is_none(),
        "benchmark" => {
            (0..=1_000_000).contains(&tokens)
                && requests.is_some_and(|n| (1..=1_000_000).contains(&n))
        }
        _ => false,
    }
}

/// Trusted worker input, resolved from the durable debug session and its Secret.
#[derive(Clone, Copy)]
pub struct DebugKeyRequest<'a> {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub project_id: Uuid,
    pub service: &'a str,
    pub model: &'a str,
    pub expires_at: DateTime<Utc>,
    pub secret: &'a str,
    pub budget: i64,
}

/// Privileged cleanup/recovery lookup, including revoked and expired records.
#[derive(Debug)]
pub struct DebugKeyRegistration {
    pub metadata: KeyMetadata,
    pub digest: Vec<u8>,
    pub session_id: Uuid,
    pub service: String,
    pub model: String,
}

fn debug_digest(request: DebugKeyRequest<'_>) -> Result<[u8; 32]> {
    if request.session_id.is_nil()
        || request.user_id.is_nil()
        || request.team_id.is_nil()
        || request.project_id.is_nil()
        || request.service.is_empty()
        || request.service.len() > 256
        || !request.service.bytes().all(|b| (b'!'..=b'~').contains(&b))
        || request.model.is_empty()
        || request.model.len() > 256
        || !request.model.bytes().all(|b| (b'!'..=b'~').contains(&b))
        || request.budget <= 0
        || request.expires_at <= Utc::now()
    {
        return Err(Error::Invalid("invalid debug key scope"));
    }
    let encoded = request
        .secret
        .strip_prefix("thl_")
        .ok_or(Error::Invalid("invalid debug secret"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| Error::Invalid("invalid debug secret"))?;
    if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != encoded {
        return Err(Error::Invalid("invalid debug secret"));
    }
    Ok(Sha256::digest(request.secret.as_bytes()).into())
}

async fn lock_debug_scope(
    tx: &mut Transaction<'_, Postgres>,
    request: DebugKeyRequest<'_>,
) -> Result<()> {
    // The same user-then-session ordering is used by the debug store and
    // offboarding, so terminal transitions cannot race a registration.
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM identity_users WHERE id=$1 FOR UPDATE")
            .bind(request.user_id)
            .fetch_optional(&mut **tx)
            .await?;
    if owner.is_none() {
        return Err(Error::Forbidden);
    }
    authorize_locked(tx, request.user_id, request.team_id, false).await?;
    let session: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM thalamus_debug_sessions WHERE id=$1 AND user_id=$2 AND team_id=$3 AND project_id=$4 AND service_id=$5 AND model=$6 AND model_budget=$7 AND state IN ('pending','running') AND deadline>clock_timestamp() AND $8::timestamptz<=deadline AND $8::timestamptz>clock_timestamp() FOR UPDATE",
    )
    .bind(request.session_id).bind(request.user_id).bind(request.team_id)
    .bind(request.project_id).bind(request.service).bind(request.model)
    .bind(request.budget).bind(request.expires_at)
    .fetch_optional(&mut **tx).await?;
    if session.is_none() {
        return Err(Error::Invalid("debug session scope mismatch or inactive"));
    }
    check_budget_cap(tx, request.team_id, request.budget).await
}

impl WorkerIdentity {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }

    /// Declare that this issuer requires a fresh IdP snapshot. Until the first
    /// successful synchronization the stale sentinel denies its credentials.
    /// This is idempotent and never overwrites an existing observation.
    pub async fn enable_idp_sync(&self, issuer: &str) -> Result<()> {
        let url =
            reqwest::Url::parse(issuer).map_err(|_| Error::Invalid("invalid identity issuer"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || issuer.chars().any(char::is_whitespace)
        {
            return Err(Error::Invalid("invalid identity issuer"));
        }
        let mut tx = self.db.begin().await?;
        lock_issuer(&mut tx, issuer).await?;
        sqlx::query("INSERT INTO identity_idp_projection(issuer,observed_at,digest) VALUES ($1,'epoch'::timestamptz,decode(repeat('00',32),'hex')) ON CONFLICT (issuer) DO NOTHING")
            .bind(issuer).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Apply a complete, trusted IdP snapshot. The transport must authenticate
    /// the source and guarantee completeness; request handlers must never call
    /// this with caller-supplied identities or groups.
    pub async fn sync_idp_snapshot(
        &self,
        expected_issuer: &str,
        mapping: &GroupMapping,
        snapshot: &IdpSnapshot,
    ) -> Result<()> {
        let observed = DateTime::from_timestamp_micros(snapshot.observed_at.timestamp_micros())
            .ok_or(Error::Invalid("invalid identity snapshot time"))?;
        let now = Utc::now();
        if expected_issuer.is_empty()
            || snapshot.issuer != expected_issuer
            || !expected_issuer.starts_with("https://")
            || observed > now
            || observed <= now - Duration::minutes(10)
        {
            return Err(Error::Invalid("identity snapshot issuer or time invalid"));
        }
        let valid_group = |group: &str| {
            !group.is_empty() && group.len() <= 1024 && !group.chars().any(char::is_control)
        };
        if mapping
            .platform_admin_group
            .as_deref()
            .is_some_and(|g| !valid_group(g))
            || mapping
                .team_admin_groups
                .iter()
                .chain(mapping.member_groups.iter())
                .any(|(group, team)| !valid_group(group) || team.is_nil())
        {
            return Err(Error::Invalid("invalid identity group mapping"));
        }
        let encoded = serde_json::to_vec(snapshot)
            .map_err(|_| Error::Invalid("invalid identity snapshot"))?;
        if encoded.len() > 8 * 1024 * 1024 || snapshot.subjects.len() > 100_000 {
            return Err(Error::Invalid("identity snapshot too large"));
        }
        let mut seen = std::collections::HashSet::new();
        for subject in &snapshot.subjects {
            if subject.subject.is_empty()
                || subject.subject.len() > 255
                || subject.subject.chars().any(char::is_control)
                || !seen.insert(subject.subject.as_str())
                || subject.groups.len() > 10_000
            {
                return Err(Error::Invalid("invalid or duplicate identity subject"));
            }
            let mut groups = std::collections::HashSet::new();
            if subject.groups.iter().any(|group| {
                group.is_empty()
                    || group.len() > 1024
                    || group.chars().any(char::is_control)
                    || !groups.insert(group)
            }) {
                return Err(Error::Invalid("invalid or duplicate identity group"));
            }
        }
        let mut fingerprint = Sha256::new();
        fingerprint.update(&encoded);
        if let Some(group) = &mapping.platform_admin_group {
            fingerprint.update((group.len() as u64).to_be_bytes());
            fingerprint.update(group.as_bytes());
        } else {
            fingerprint.update(0u64.to_be_bytes());
        }
        for groups in [&mapping.team_admin_groups, &mapping.member_groups] {
            let ordered: std::collections::BTreeMap<_, _> = groups.iter().collect();
            fingerprint.update((ordered.len() as u64).to_be_bytes());
            for (group, team) in ordered {
                fingerprint.update((group.len() as u64).to_be_bytes());
                fingerprint.update(group.as_bytes());
                fingerprint.update(team.as_bytes());
            }
        }
        let digest = fingerprint.finalize();
        let mut tx = self.db.begin().await?;
        lock_issuer(&mut tx, expected_issuer).await?;
        let previous = sqlx::query(
            "SELECT observed_at,digest FROM identity_idp_projection WHERE issuer=$1 FOR UPDATE",
        )
        .bind(expected_issuer)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = previous {
            let previous_time: DateTime<Utc> = row.try_get("observed_at")?;
            let previous_digest: Vec<u8> = row.try_get("digest")?;
            if observed < previous_time
                || (observed == previous_time && previous_digest.as_slice() != digest.as_slice())
            {
                return Err(Error::Invalid("identity snapshot replay or conflict"));
            }
            if observed == previous_time {
                tx.commit().await?;
                return Ok(());
            }
        }
        let active: Vec<String> = snapshot
            .subjects
            .iter()
            .filter(|s| s.active)
            .map(|s| s.subject.clone())
            .collect();
        for subject in &snapshot.subjects {
            let row = sqlx::query("SELECT disabled,groups_issued_at FROM identity_users WHERE issuer=$1 AND subject=$2 FOR UPDATE")
                .bind(expected_issuer).bind(&subject.subject).fetch_optional(&mut *tx).await?;
            let Some(row) = row else {
                if subject.active {
                    let (admin, teams) = role_mapping(mapping, &subject.groups);
                    apply_login_snapshot(
                        &mut tx,
                        expected_issuer,
                        &subject.subject,
                        observed,
                        admin,
                        teams,
                    )
                    .await?;
                }
                continue;
            };
            let disabled: bool = row.try_get("disabled")?;
            let issued_at: Option<DateTime<Utc>> = row.try_get("groups_issued_at")?;
            if disabled || issued_at.is_some_and(|time| time > observed) {
                continue;
            }
            if !subject.active {
                disable_subject(&mut tx, expected_issuer, &subject.subject).await?;
            } else {
                let (admin, teams) = role_mapping(mapping, &subject.groups);
                apply_login_snapshot(
                    &mut tx,
                    expected_issuer,
                    &subject.subject,
                    observed,
                    admin,
                    teams,
                )
                .await?;
            }
        }
        let missing: Vec<String> = sqlx::query_scalar("SELECT subject FROM identity_users WHERE issuer=$1 AND NOT disabled AND (groups_issued_at IS NULL OR groups_issued_at<=$2) AND NOT (subject=ANY($3)) ORDER BY subject")
            .bind(expected_issuer).bind(observed).bind(&active).fetch_all(&mut *tx).await?;
        for subject in missing {
            disable_subject(&mut tx, expected_issuer, &subject).await?;
        }
        sqlx::query("INSERT INTO identity_idp_projection(issuer,observed_at,digest) VALUES ($1,$2,$3) ON CONFLICT (issuer) DO UPDATE SET observed_at=EXCLUDED.observed_at,digest=EXCLUDED.digest")
            .bind(expected_issuer).bind(observed).bind(digest.as_slice()).execute(&mut *tx).await?;
        audit(
            &mut tx,
            None,
            Some("thalamus-worker"),
            "identity.snapshot",
            expected_issuer,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    /// Atomically replace the projection with a fully verified Git snapshot.
    /// Refresh only after the source commit and clean tracked file are verified.
    pub async fn sync_budget_caps(&self, source_commit: &str, caps: &[BudgetCap]) -> Result<()> {
        if !matches!(source_commit.len(), 40 | 64)
            || !source_commit
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Error::Invalid("invalid budget source commit"));
        }
        let mut seen = std::collections::HashSet::new();
        if caps.iter().any(|cap| {
            cap.max_tokens < 0
                || cap.max_tokens > i64::from(u32::MAX)
                || cap.unit != "tokens"
                || !matches!(cap.window.as_str(), "day" | "month")
                || !seen.insert(cap.team_id)
                || !cap.allowed_classes.iter().any(|class| class == "standard")
                || {
                    let mut classes = std::collections::HashSet::new();
                    cap.allowed_classes
                        .iter()
                        .any(|class| !valid_traffic_class(class) || !classes.insert(class))
                }
        }) {
            return Err(Error::Invalid("invalid budget caps"));
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO identity_budget_projection(singleton,source_commit,refreshed_at) VALUES (true,$1,clock_timestamp()) ON CONFLICT (singleton) DO UPDATE SET source_commit=EXCLUDED.source_commit,refreshed_at=EXCLUDED.refreshed_at")
            .bind(source_commit).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM identity_budget_caps")
            .execute(&mut *tx)
            .await?;
        for cap in caps {
            sqlx::query("INSERT INTO identity_budget_caps(team_id,max_tokens,unit,budget_window,allowed_classes) VALUES ($1,$2,$3,$4,$5)")
                .bind(cap.team_id).bind(cap.max_tokens).bind(&cap.unit).bind(&cap.window).bind(&cap.allowed_classes).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The returned digests are privileged gateway configuration, not public metadata.
    pub async fn active_projection(&self) -> Result<Vec<ProjectionKey>> {
        active_projection(&self.db).await
    }

    /// Register a Secret already owned by this debug session. A retry is
    /// accepted only when the entire live binding and digest match.
    pub async fn register_debug_key(&self, request: DebugKeyRequest<'_>) -> Result<KeyMetadata> {
        let digest = debug_digest(request)?;
        let mut tx = self.db.begin().await?;
        lock_debug_scope(&mut tx, request).await?;
        let id = Uuid::new_v4();
        let name = format!("debug-{}", request.session_id);
        let row = sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,name,debug_session_id,debug_model,debug_target_service) VALUES ($1,$2,$3,$3,$4,$5,'debug',$6,$7,$8,$9,$10,$11,$12) ON CONFLICT (debug_session_id) DO NOTHING RETURNING *")
            .bind(id).bind(digest.as_slice()).bind(request.user_id).bind(request.team_id)
            .bind(request.project_id).bind(vec![request.model]).bind(request.budget)
            .bind(request.expires_at).bind(name).bind(request.session_id)
            .bind(request.model).bind(request.service).fetch_optional(&mut *tx).await?;
        let key = if let Some(row) = row {
            sqlx::query("INSERT INTO identity_audit(actor_id,agent,action,target,details) VALUES ($1,'thalamus-worker','key.issue',$2,jsonb_build_object('debug_session_id',$3::text))")
                .bind(request.user_id).bind(id.to_string()).bind(request.session_id.to_string())
                .execute(&mut *tx).await?;
            key_metadata(row)?
        } else {
            matching_debug_key(&mut tx, request, &digest).await?
        };
        tx.commit().await?;
        Ok(key)
    }

    /// Verify an existing registration without ever creating one.
    pub async fn verify_debug_key(&self, request: DebugKeyRequest<'_>) -> Result<KeyMetadata> {
        let digest = debug_digest(request)?;
        let mut tx = self.db.begin().await?;
        lock_debug_scope(&mut tx, request).await?;
        let key = matching_debug_key(&mut tx, request, &digest).await?;
        tx.commit().await?;
        Ok(key)
    }

    /// Find even expired or revoked registrations for trusted cleanup.
    pub async fn debug_key(&self, session_id: Uuid) -> Result<Option<DebugKeyRegistration>> {
        let row =
            sqlx::query("SELECT * FROM identity_keys WHERE kind='debug' AND debug_session_id=$1")
                .bind(session_id)
                .fetch_optional(&self.db)
                .await?;
        row.map(|row| {
            Ok(DebugKeyRegistration {
                digest: row.try_get("digest")?,
                session_id: row.try_get("debug_session_id")?,
                service: row.try_get("debug_target_service")?,
                model: row.try_get("debug_model")?,
                metadata: key_metadata(row)?,
            })
        })
        .transpose()
    }

    /// Check the same authoritative conditions used by gateway projection.
    pub async fn active_debug_key(&self, session_id: Uuid) -> Result<Option<KeyMetadata>> {
        let key = query_active_projection(&self.db, Some(session_id))
            .await?
            .into_iter()
            .next()
            .map(|key| key.metadata);
        if key.is_none() {
            let stale: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_keys k JOIN identity_users u ON u.id=k.user_id JOIN identity_idp_projection p ON p.issuer=u.issuer WHERE k.kind='debug' AND k.debug_session_id=$1 AND p.observed_at<=clock_timestamp()-interval '10 minutes')")
                .bind(session_id).fetch_one(&self.db).await?;
            if stale {
                return Err(Error::Invalid("identity snapshot unavailable or stale"));
            }
        }
        Ok(key)
    }

    /// Missing and already-revoked keys are successful cleanup retries.
    pub async fn revoke_debug_key(&self, session_id: Uuid) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let row = sqlx::query("UPDATE identity_keys SET revoked_at=clock_timestamp() WHERE kind='debug' AND debug_session_id=$1 AND revoked_at IS NULL RETURNING id,user_id")
            .bind(session_id).fetch_optional(&mut *tx).await?;
        if let Some(row) = row {
            let id: Uuid = row.try_get("id")?;
            let user: Uuid = row.try_get("user_id")?;
            sqlx::query("INSERT INTO identity_audit(actor_id,agent,action,target,details) VALUES ($1,'thalamus-worker','key.revoke',$2,jsonb_build_object('debug_session_id',$3::text))")
                .bind(user).bind(id.to_string()).bind(session_id.to_string())
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Register the secret already durably stored by the worker. Only its
    /// canonical SHA-256 digest is persisted, and retries must match exactly.
    pub async fn register_run_key(&self, request: RunKeyRequest<'_>) -> Result<KeyMetadata> {
        let RunKeyRequest {
            run_id,
            deployment,
            run_kind,
            request_budget,
            user_id,
            team_id,
            project_id,
            service,
            expires_at,
            secret,
            budget,
        } = request;
        let now = Utc::now();
        if expires_at <= now || expires_at > now + Duration::hours(24) {
            return Err(Error::Invalid("run key expiry must be within 24 hours"));
        }
        if !valid_run_budgets(run_kind, budget, request_budget) {
            return Err(Error::Invalid("invalid run budgets"));
        }
        if run_id.is_nil()
            || deployment.trim().is_empty()
            || deployment.len() > 1024
            || deployment.chars().any(char::is_control)
            || service.trim().is_empty()
            || service.len() > 1024
            || service.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid run scope"));
        }
        let encoded = secret
            .strip_prefix("thl_")
            .ok_or(Error::Invalid("invalid run secret"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| Error::Invalid("invalid run secret"))?;
        if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != encoded {
            return Err(Error::Invalid("invalid run secret"));
        }
        let mut tx = self.db.begin().await?;
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM identity_users WHERE id=$1 FOR UPDATE")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
        if owner.is_none() {
            return Err(Error::Forbidden);
        }
        authorize_locked(&mut tx, user_id, team_id, false).await?;
        check_budget_cap(&mut tx, team_id, budget).await?;
        let project_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM identity_projects WHERE id=$1 AND team_id=$2)",
        )
        .bind(project_id)
        .bind(team_id)
        .fetch_one(&mut *tx)
        .await?;
        if !project_exists {
            return Err(Error::NotFound);
        }
        // The worker is responsible for generating this value with a CSPRNG.
        let digest = Sha256::digest(secret.as_bytes());
        let id = Uuid::new_v4();
        let name = format!("run-{run_id}");
        let row = sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,project_id,kind,allowed_services,expires_at,name,run_id,budget,deployment,run_kind,request_budget) VALUES ($1,$2,$3,$3,$4,$5,'job',$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT (run_id) DO NOTHING RETURNING id,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,revoked_at,created_at,name")
            .bind(id).bind(digest.as_slice()).bind(user_id).bind(team_id).bind(project_id)
            .bind(vec![service]).bind(expires_at).bind(name).bind(run_id).bind(budget).bind(deployment).bind(run_kind).bind(request_budget)
            .fetch_optional(&mut *tx).await?;
        let key = if let Some(row) = row {
            let key = key_metadata(row)?;
            sqlx::query("INSERT INTO identity_audit(actor_id,agent,action,target,details) VALUES ($1,'thalamus-worker','key.issue',$2,jsonb_build_object('run_id',$3::text))")
                .bind(user_id).bind(id.to_string()).bind(run_id.to_string()).execute(&mut *tx).await?;
            key
        } else {
            let row = sqlx::query("SELECT id,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,revoked_at,created_at,name FROM identity_keys WHERE run_id=$1 AND user_id=$2 AND creator_id=$2 AND team_id=$3 AND project_id=$4 AND kind='job' AND allowed_services=$5 AND expires_at=$6 AND digest=$7 AND budget=$8 AND deployment=$9 AND run_kind=$10 AND request_budget IS NOT DISTINCT FROM $11 AND revoked_at IS NULL AND expires_at>clock_timestamp() FOR UPDATE")
                .bind(run_id).bind(user_id).bind(team_id).bind(project_id).bind(vec![service])
                .bind(expires_at).bind(digest.as_slice()).bind(budget).bind(deployment).bind(run_kind).bind(request_budget).fetch_optional(&mut *tx).await?
                .ok_or(Error::Invalid("run key registration conflict"))?;
            key_metadata(row)?
        };
        tx.commit().await?;
        Ok(key)
    }

    /// Replace only an expired, non-revoked run key. The trusted worker must
    /// serialize this operation and prove that no Job exists before replacing
    /// its owned Secret; a Job that has ever started must never be rotated.
    pub async fn replace_expired_run_key(&self, request: RunKeyRequest<'_>) -> Result<KeyMetadata> {
        let RunKeyRequest {
            run_id,
            deployment,
            run_kind,
            request_budget,
            user_id,
            team_id,
            project_id,
            service,
            expires_at,
            secret,
            budget,
        } = request;
        let now = Utc::now();
        if expires_at <= now || expires_at > now + Duration::hours(24) {
            return Err(Error::Invalid("run key expiry must be within 24 hours"));
        }
        if !valid_run_budgets(run_kind, budget, request_budget)
            || run_id.is_nil()
            || deployment.trim().is_empty()
            || deployment.len() > 1024
            || deployment.chars().any(char::is_control)
            || service.trim().is_empty()
            || service.len() > 1024
            || service.chars().any(char::is_control)
        {
            return Err(Error::Invalid("invalid run key scope"));
        }
        let encoded = secret
            .strip_prefix("thl_")
            .ok_or(Error::Invalid("invalid run secret"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| Error::Invalid("invalid run secret"))?;
        if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != encoded {
            return Err(Error::Invalid("invalid run secret"));
        }
        let mut tx = self.db.begin().await?;
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM identity_users WHERE id=$1 FOR UPDATE")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
        if owner.is_none() {
            return Err(Error::Forbidden);
        }
        authorize_locked(&mut tx, user_id, team_id, false).await?;
        check_budget_cap(&mut tx, team_id, budget).await?;
        let digest = Sha256::digest(secret.as_bytes());
        let row = sqlx::query("UPDATE identity_keys SET digest=$1,expires_at=$2 WHERE run_id=$3 AND user_id=$4 AND creator_id=$4 AND team_id=$5 AND project_id=$6 AND kind='job' AND allowed_services=$7 AND budget=$8 AND deployment=$9 AND run_kind=$10 AND request_budget IS NOT DISTINCT FROM $11 AND revoked_at IS NULL AND expires_at <= clock_timestamp() RETURNING id,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,revoked_at,created_at,name")
            .bind(digest.as_slice()).bind(expires_at).bind(run_id).bind(user_id).bind(team_id)
            .bind(project_id).bind(vec![service]).bind(budget).bind(deployment).bind(run_kind).bind(request_budget).fetch_optional(&mut *tx).await?
            .ok_or(Error::Invalid("run key cannot be replaced"))?;
        let key = key_metadata(row)?;
        sqlx::query("INSERT INTO identity_audit(actor_id,agent,action,target,details) VALUES ($1,'thalamus-worker','key.rotate',$2,jsonb_build_object('run_id',$3::text))")
            .bind(user_id).bind(key.id.to_string()).bind(run_id.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(key)
    }

    /// Idempotently revoke a previously issued run key.
    pub async fn revoke_run_key(&self, run_id: Uuid) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let row = sqlx::query(
            "SELECT id,user_id,revoked_at FROM identity_keys WHERE run_id=$1 FOR UPDATE",
        )
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
        let id: Uuid = row.try_get("id")?;
        let user: Uuid = row.try_get("user_id")?;
        let revoked: Option<DateTime<Utc>> = row.try_get("revoked_at")?;
        if revoked.is_none() {
            sqlx::query("UPDATE identity_keys SET revoked_at=now() WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO identity_audit(actor_id,agent,action,target,details) VALUES ($1,'thalamus-worker','key.revoke',$2,jsonb_build_object('run_id',$3::text))")
                .bind(user).bind(id.to_string()).bind(run_id.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn matching_debug_key(
    tx: &mut Transaction<'_, Postgres>,
    request: DebugKeyRequest<'_>,
    digest: &[u8; 32],
) -> Result<KeyMetadata> {
    let row = sqlx::query("SELECT * FROM identity_keys WHERE kind='debug' AND debug_session_id=$1 AND user_id=$2 AND creator_id=$2 AND team_id=$3 AND project_id=$4 AND debug_target_service=$5 AND debug_model=$6 AND allowed_services=$7 AND budget=$8 AND expires_at=$9 AND digest=$10 AND revoked_at IS NULL AND expires_at>clock_timestamp() FOR UPDATE")
        .bind(request.session_id).bind(request.user_id).bind(request.team_id)
        .bind(request.project_id).bind(request.service).bind(request.model)
        .bind(vec![request.model]).bind(request.budget).bind(request.expires_at)
        .bind(digest.as_slice()).fetch_optional(&mut **tx).await?
        .ok_or(Error::Invalid("debug key registration conflict"))?;
    key_metadata(row)
}

async fn active_projection(db: &PgPool) -> Result<Vec<ProjectionKey>> {
    query_active_projection(db, None).await
}

async fn query_active_projection(
    db: &PgPool,
    debug_session: Option<Uuid>,
) -> Result<Vec<ProjectionKey>> {
    // Identity can run before the separate debug migration. In that case no
    // debug key can be registered or projected; ordinary keys remain usable.
    let has_debug: bool =
        sqlx::query_scalar("SELECT to_regclass('thalamus_debug_sessions') IS NOT NULL")
            .fetch_one(db)
            .await?;
    let debug_filter = if has_debug {
        " AND (k.kind <> 'debug' OR EXISTS (SELECT 1 FROM thalamus_debug_sessions s WHERE s.id=k.debug_session_id AND s.user_id=k.user_id AND s.team_id=k.team_id AND s.project_id=k.project_id AND s.service_id=k.debug_target_service AND s.model=k.debug_model AND s.model_budget=k.budget AND s.state IN ('pending','running') AND s.deadline>clock_timestamp() AND k.expires_at<=s.deadline))"
    } else {
        " AND k.kind <> 'debug'"
    };
    let sql = format!("SELECT k.*,p.cost_centre,COALESCE(p.traffic_class,'standard') AS traffic_class,CASE WHEN p.budget BETWEEN 0 AND 9007199254740991 AND p.budget=trunc(p.budget) THEN p.budget::bigint ELSE NULL END AS project_budget,c.max_tokens AS team_budget,c.budget_window,CASE WHEN ip.issuer IS NULL THEN k.expires_at WHEN k.expires_at IS NULL THEN ip.observed_at+interval '10 minutes' ELSE LEAST(k.expires_at,ip.observed_at+interval '10 minutes') END AS projected_expiry FROM identity_keys k JOIN identity_users u ON u.id=k.user_id LEFT JOIN identity_projects p ON p.id=k.project_id AND p.team_id=k.team_id LEFT JOIN identity_budget_caps c ON c.team_id=k.team_id LEFT JOIN identity_idp_projection ip ON ip.issuer=u.issuer WHERE k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()) AND NOT u.disabled AND (ip.issuer IS NULL OR ip.observed_at>clock_timestamp()-interval '10 minutes') AND (u.platform_admin OR EXISTS (SELECT 1 FROM identity_memberships m WHERE m.user_id=u.id AND m.team_id=k.team_id)) AND (p.budget IS NULL OR (p.budget BETWEEN 0 AND 9007199254740991 AND p.budget=trunc(p.budget))) AND (p.traffic_class IS NULL OR p.traffic_class='standard' OR p.traffic_class=ANY(c.allowed_classes)) AND ($1::uuid IS NULL OR k.debug_session_id=$1){debug_filter} ORDER BY k.id");
    let rows = sqlx::query(&sql).bind(debug_session).fetch_all(db).await?;
    rows.into_iter()
        .map(|r| {
            Ok(ProjectionKey {
                digest: r.try_get("digest")?,
                cost_centre: r.try_get("cost_centre")?,
                class: r.try_get("traffic_class")?,
                project_budget: r.try_get("project_budget")?,
                team_budget: r.try_get("team_budget")?,
                budget_window: r.try_get("budget_window")?,
                run_id: r.try_get("run_id")?,
                deployment: r.try_get("deployment")?,
                run_kind: r.try_get("run_kind")?,
                request_budget: r.try_get("request_budget")?,
                debug_session_id: r.try_get("debug_session_id")?,
                debug_model: r.try_get("debug_model")?,
                metadata: {
                    let expiry = r.try_get("projected_expiry")?;
                    let mut metadata = key_metadata(r)?;
                    metadata.expires_at = expiry;
                    metadata
                },
            })
        })
        .collect()
}
fn key_metadata(r: sqlx::postgres::PgRow) -> Result<KeyMetadata> {
    Ok(KeyMetadata {
        id: r.try_get("id")?,
        user_id: r.try_get("user_id")?,
        name: r.try_get("name")?,
        creator_id: r.try_get("creator_id")?,
        team_id: r.try_get("team_id")?,
        project_id: r.try_get("project_id")?,
        kind: r.try_get("kind")?,
        allowed_services: r.try_get("allowed_services")?,
        budget: r.try_get("budget")?,
        expires_at: r.try_get("expires_at")?,
        revoked_at: r.try_get("revoked_at")?,
        created_at: r.try_get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oidc_trust_rejects_an_empty_or_invalid_pem_bundle() {
        assert!(oidc_http_client(None).is_ok());
        for pem in [&b""[..], &b"not a certificate"[..]] {
            assert!(matches!(
                oidc_http_client(Some(pem)),
                Err(Error::Invalid(_))
            ));
        }
    }

    #[test]
    fn session_signature_rejects_tampering_and_wrong_key() {
        let id = Uuid::new_v4();
        let token = sign(&[1; 32], id);
        assert_eq!(verify(&[1; 32], &token).unwrap(), id);
        assert!(verify(&[2; 32], &token).is_err());
        assert!(verify(&[1; 32], &format!("{}x", token)).is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn personal_keys_are_projectless_and_cannot_be_issued_as_job_or_debug(pool: PgPool) {
        let user = Uuid::new_v4();
        let team = Uuid::new_v4();
        let other_team = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,'issuer','subject')")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'one'),($2,'two')")
            .bind(team)
            .bind(other_team)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')",
        )
        .bind(user)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
        let input = NewKey {
            name: "personal".into(),
            team_id: team,
            project_id: None,
            kind: KeyKind::Personal,
            allowed_services: vec!["model".into()],
            budget: None,
            expires_at: None,
        };
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM identity_users WHERE id=$1 FOR UPDATE")
            .bind(user)
            .execute(&mut *tx)
            .await
            .unwrap();
        let issued = issue_key_locked(&mut tx, user, input.clone(), None)
            .await
            .unwrap();
        assert_eq!(issued.key.project_id, None);
        assert_eq!(issued.key.kind, "personal");
        tx.commit().await.unwrap();
        let projection = active_projection(&pool).await.unwrap();
        assert_eq!(projection.len(), 1);
        assert_eq!(projection[0].metadata.id, issued.key.id);
        assert_eq!(projection[0].metadata.project_id, None);
        assert_eq!(projection[0].class, "standard");
        for kind in [KeyKind::Job, KeyKind::Debug] {
            let mut tx = pool.begin().await.unwrap();
            let forged = NewKey {
                kind,
                ..input.clone()
            };
            assert!(matches!(
                issue_key_locked(&mut tx, user, forged, None).await,
                Err(Error::Forbidden)
            ));
            tx.rollback().await.unwrap();
        }
        let mut tx = pool.begin().await.unwrap();
        let cross_team = NewKey {
            team_id: other_team,
            ..input
        };
        assert!(matches!(
            issue_key_locked(&mut tx, user, cross_team, None).await,
            Err(Error::Forbidden)
        ));
        tx.rollback().await.unwrap();
    }
    #[test]
    fn group_mapping_prefers_team_admin() {
        let team = Uuid::new_v4();
        let mut mapping = GroupMapping {
            platform_admin_group: Some("platform".into()),
            ..Default::default()
        };
        mapping.team_admin_groups.insert("owners".into(), team);
        mapping.member_groups.insert("members".into(), team);
        let (admin, teams) = role_mapping(
            &mapping,
            &["owners".into(), "members".into(), "platform".into()],
        );
        assert!(admin);
        assert_eq!(teams.get(&team), Some(&"team_admin"));
    }
    #[test]
    fn extension_payload_rejects_invalid_encoding() {
        assert!(token_payload("a.!.c").is_err());
        let payload = URL_SAFE_NO_PAD.encode(br#"{"groups":["owners"],"active":false}"#);
        assert_eq!(
            token_payload(&format!("a.{payload}.c")).unwrap()["active"],
            false
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn snapshot_provisions_subject_and_old_login_cannot_restore_old_roles(pool: PgPool) {
        let team = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
            .bind(team)
            .execute(&pool)
            .await
            .unwrap();
        let mut mapping = GroupMapping::default();
        mapping.member_groups.insert("/members".into(), team);
        let observed =
            DateTime::from_timestamp_micros((Utc::now() - Duration::seconds(1)).timestamp_micros())
                .unwrap()
                + Duration::nanoseconds(400);
        let snapshot = IdpSnapshot {
            issuer: "https://id.example".into(),
            observed_at: observed,
            subjects: vec![IdpSubjectSnapshot {
                subject: "new-user".into(),
                active: true,
                groups: vec!["/members".into()],
            }],
        };
        WorkerIdentity::new(pool.clone())
            .sync_idp_snapshot(&snapshot.issuer, &mapping, &snapshot)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let user = apply_login_snapshot(
            &mut tx,
            &snapshot.issuer,
            "new-user",
            observed - Duration::seconds(1),
            true,
            HashMap::from([(team, "team_admin")]),
        )
        .await
        .unwrap();
        let same_microsecond = apply_login_snapshot(
            &mut tx,
            &snapshot.issuer,
            "new-user",
            observed + Duration::nanoseconds(500),
            true,
            HashMap::from([(team, "team_admin")]),
        )
        .await
        .unwrap();
        assert_eq!(same_microsecond, user);
        tx.commit().await.unwrap();
        let admin: bool =
            sqlx::query_scalar("SELECT platform_admin FROM identity_users WHERE id=$1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        let role: String = sqlx::query_scalar(
            "SELECT role FROM identity_memberships WHERE user_id=$1 AND team_id=$2",
        )
        .bind(user)
        .bind(team)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!admin);
        assert_eq!(role, "member");
        let mut tx = pool.begin().await.unwrap();
        assert!(matches!(
            apply_login_snapshot(
                &mut tx,
                &snapshot.issuer,
                "absent-user",
                observed,
                true,
                HashMap::new()
            )
            .await,
            Err(Error::Unauthorized)
        ));
        assert!(matches!(
            apply_login_snapshot(
                &mut tx,
                &snapshot.issuer,
                "another-absent-user",
                observed + Duration::nanoseconds(500),
                true,
                HashMap::new()
            )
            .await,
            Err(Error::Unauthorized)
        ));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn class_assignment_requires_platform_admin_and_reviewed_cap(pool: PgPool) {
        let team = Uuid::new_v4();
        let team_admin = Uuid::new_v4();
        let platform_admin = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
            .bind(team)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO identity_users(id,issuer,subject,platform_admin) VALUES ($1,'issuer','team-admin',false),($2,'issuer','platform-admin',true)")
            .bind(team_admin).bind(platform_admin).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'team_admin')",
        )
        .bind(team_admin)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
        let worker = WorkerIdentity::new(pool.clone());
        worker
            .sync_budget_caps(
                &"a".repeat(40),
                &[BudgetCap {
                    team_id: team,
                    max_tokens: 100,
                    unit: "tokens".into(),
                    window: "day".into(),
                    allowed_classes: vec!["standard".into(), "interactive".into()],
                }],
            )
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM identity_users WHERE id=$1 FOR UPDATE")
            .bind(team_admin)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(authorize_locked(&mut tx, team_admin, team, true)
            .await
            .is_ok());
        assert!(matches!(
            require_platform_admin_locked(&mut tx, team_admin).await,
            Err(Error::Forbidden)
        ));
        tx.rollback().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM identity_users WHERE id=$1 FOR UPDATE")
            .bind(platform_admin)
            .execute(&mut *tx)
            .await
            .unwrap();
        require_platform_admin_locked(&mut tx, platform_admin)
            .await
            .unwrap();
        check_class_cap(&mut tx, team, "interactive").await.unwrap();
        assert!(matches!(
            check_class_cap(&mut tx, team, "privileged").await,
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            check_class_cap(&mut tx, team, "Interactive").await,
            Err(Error::Invalid(_))
        ));
        tx.rollback().await.unwrap();
        sqlx::query("UPDATE identity_budget_projection SET refreshed_at=clock_timestamp()-interval '2 minutes'")
            .execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(matches!(
            check_class_cap(&mut tx, team, "interactive").await,
            Err(Error::Invalid(_))
        ));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn snapshots_are_ordered_and_equal_times_fail_closed(pool: PgPool) {
        let team = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
            .bind(team)
            .execute(&pool)
            .await
            .unwrap();
        let old = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let new = old + Duration::seconds(1);
        let mut tx = pool.begin().await.unwrap();
        let user = apply_login_snapshot(
            &mut tx,
            "issuer",
            "subject",
            old,
            true,
            HashMap::from([(team, "team_admin")]),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        apply_login_snapshot(&mut tx, "issuer", "subject", new, false, HashMap::new())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(matches!(
            apply_login_snapshot(
                &mut tx,
                "issuer",
                "subject",
                old,
                true,
                HashMap::from([(team, "team_admin")])
            )
            .await,
            Err(Error::Unauthorized)
        ));
        tx.rollback().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        apply_login_snapshot(
            &mut tx,
            "issuer",
            "subject",
            new,
            true,
            HashMap::from([(team, "team_admin")]),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let row = sqlx::query("SELECT platform_admin,(SELECT count(*) FROM identity_memberships WHERE user_id=$1) AS memberships FROM identity_users WHERE id=$1").bind(user).fetch_one(&pool).await.unwrap();
        assert!(!row.get::<bool, _>("platform_admin"));
        assert_eq!(row.get::<i64, _>("memberships"), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn tombstone_blocks_first_login_and_existing_access(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        disable_subject(&mut tx, "issuer", "new-subject")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(matches!(
            apply_login_snapshot(
                &mut tx,
                "issuer",
                "new-subject",
                Utc::now(),
                true,
                HashMap::new()
            )
            .await,
            Err(Error::Unauthorized)
        ));
        tx.rollback().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        let user = apply_login_snapshot(
            &mut tx,
            "issuer",
            "existing",
            Utc::now(),
            false,
            HashMap::new(),
        )
        .await
        .unwrap();
        let session = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_sessions(id,user_id,expires_at) VALUES ($1,$2,now()+interval '1 hour')").bind(session).bind(user).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        disable_subject(&mut tx, "issuer", "existing")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(matches!(
            lock_session(&mut tx, &[1; 32], &sign(&[1; 32], session)).await,
            Err(Error::Unauthorized)
        ));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn first_login_and_new_subject_tombstone_serialize(pool: PgPool) {
        let mut disable = pool.begin().await.unwrap();
        disable_subject(&mut disable, "issuer", "subject")
            .await
            .unwrap();
        let mut login = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *login)
            .await
            .unwrap();
        assert!(matches!(
            apply_login_snapshot(
                &mut login,
                "issuer",
                "subject",
                Utc::now(),
                true,
                HashMap::new()
            )
            .await,
            Err(Error::Database(_))
        ));
        login.rollback().await.unwrap();
        disable.commit().await.unwrap();
        let mut login = pool.begin().await.unwrap();
        assert!(matches!(
            apply_login_snapshot(
                &mut login,
                "issuer",
                "subject",
                Utc::now(),
                true,
                HashMap::new()
            )
            .await,
            Err(Error::Unauthorized)
        ));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn authorization_lock_serializes_demotion_and_offboarding(pool: PgPool) {
        let team = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
            .bind(team)
            .execute(&pool)
            .await
            .unwrap();
        let time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut setup = pool.begin().await.unwrap();
        let user = apply_login_snapshot(
            &mut setup,
            "issuer",
            "subject",
            time,
            false,
            HashMap::from([(team, "team_admin")]),
        )
        .await
        .unwrap();
        let session = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_sessions(id,user_id,expires_at) VALUES ($1,$2,now()+interval '1 hour')").bind(session).bind(user).execute(&mut *setup).await.unwrap();
        setup.commit().await.unwrap();
        let token = sign(&[1; 32], session);
        let mut writer = pool.begin().await.unwrap();
        assert_eq!(
            lock_session(&mut writer, &[1; 32], &token).await.unwrap(),
            user
        );
        authorize_locked(&mut writer, user, team, true)
            .await
            .unwrap();
        let mut demotion = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *demotion)
            .await
            .unwrap();
        assert!(matches!(
            apply_login_snapshot(
                &mut demotion,
                "issuer",
                "subject",
                time + Duration::seconds(1),
                false,
                HashMap::new()
            )
            .await,
            Err(Error::Database(_))
        ));
        demotion.rollback().await.unwrap();
        let mut offboard = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *offboard)
            .await
            .unwrap();
        assert!(matches!(
            disable_subject(&mut offboard, "issuer", "subject").await,
            Err(Error::Database(_))
        ));
        offboard.rollback().await.unwrap();
        writer.commit().await.unwrap();
        let mut demotion = pool.begin().await.unwrap();
        apply_login_snapshot(
            &mut demotion,
            "issuer",
            "subject",
            time + Duration::seconds(1),
            false,
            HashMap::new(),
        )
        .await
        .unwrap();
        demotion.commit().await.unwrap();
        let mut writer = pool.begin().await.unwrap();
        lock_session(&mut writer, &[1; 32], &token).await.unwrap();
        assert!(matches!(
            authorize_locked(&mut writer, user, team, false).await,
            Err(Error::Forbidden)
        ));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn projection_requires_current_membership_or_platform_admin(pool: PgPool) {
        let team = Uuid::new_v4();
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
            .bind(team)
            .execute(&pool)
            .await
            .unwrap();
        let time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut tx = pool.begin().await.unwrap();
        let user = apply_login_snapshot(
            &mut tx,
            "issuer",
            "subject",
            time,
            false,
            HashMap::from([(team, "member")]),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,kind,allowed_services,expires_at) VALUES ($1,$2,$3,$3,$4,'personal',ARRAY['service'],now()+interval '1 hour')").bind(Uuid::new_v4()).bind(vec![1u8;32]).bind(user).bind(team).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(active_projection(&pool).await.unwrap().len(), 1);
        let mut tx = pool.begin().await.unwrap();
        apply_login_snapshot(
            &mut tx,
            "issuer",
            "subject",
            time + Duration::seconds(1),
            false,
            HashMap::new(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(active_projection(&pool).await.unwrap().is_empty());
        let mut tx = pool.begin().await.unwrap();
        apply_login_snapshot(
            &mut tx,
            "issuer",
            "subject",
            time + Duration::seconds(2),
            true,
            HashMap::new(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(active_projection(&pool).await.unwrap().len(), 1);
        let mut tx = pool.begin().await.unwrap();
        disable_subject(&mut tx, "issuer", "subject").await.unwrap();
        tx.commit().await.unwrap();
        assert!(active_projection(&pool).await.unwrap().is_empty());
    }
}
