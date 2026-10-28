//! OIDC identity and Postgres-backed authorization. The caller must protect browser
//! cookies (Secure, HttpOnly, SameSite) and bind the returned login state to the browser.
use std::collections::HashMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use openidconnect::{
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    reqwest::async_http_client,
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

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

/// Exact group names and existing team IDs. Team-admin membership takes precedence.
#[derive(Clone, Default)]
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
    pub session_signing_key: Vec<u8>,
    pub session_lifetime: Duration,
    pub groups: GroupMapping,
}

pub struct IdentityService {
    db: PgPool,
    client: CoreClient,
    issuer: String,
    signing_key: Vec<u8>,
    session_lifetime: Duration,
    groups: GroupMapping,
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
}
impl KeyKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Project => "project",
            Self::Job => "job",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewKey {
    pub team_id: Uuid,
    pub project_id: Option<Uuid>,
    pub kind: KeyKind,
    pub allowed_services: Vec<String>,
    pub budget: Option<i64>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct IssuedKey {
    pub key: KeyMetadata,
    pub secret: String,
}

#[derive(Debug, Serialize)]
pub struct KeyMetadata {
    pub id: Uuid,
    pub user_id: Uuid,
    pub creator_id: Uuid,
    pub team_id: Uuid,
    pub project_id: Option<Uuid>,
    pub kind: String,
    pub allowed_services: Vec<String>,
    pub budget: Option<i64>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Privileged worker-only projection; never return this type from a public listing.
#[derive(Debug)]
pub struct ProjectionKey {
    pub metadata: KeyMetadata,
    pub digest: Vec<u8>,
    pub cost_centre: Option<String>,
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
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_sessions s JOIN identity_users u ON u.id=s.user_id WHERE s.id=$1 AND s.user_id=$2 AND s.expires_at>now() AND NOT u.disabled)")
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
    let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$2 WHERE u.id=$1 AND NOT u.disabled AND (u.platform_admin OR (m.role IS NOT NULL AND (NOT $3 OR m.role='team_admin'))))")
        .bind(user).bind(team).bind(admin_required).fetch_one(&mut **tx).await?;
    if allowed {
        Ok(())
    } else {
        Err(Error::Forbidden)
    }
}

async fn disable_subject(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    subject: &str,
) -> Result<()> {
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
        let metadata = CoreProviderMetadata::discover_async(issuer, async_http_client)
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
            .request_async(async_http_client)
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
        sqlx::query_scalar("SELECT s.user_id FROM identity_sessions s JOIN identity_users u ON u.id=s.user_id WHERE s.id=$1 AND s.expires_at>now() AND NOT u.disabled")
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
        let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$2 WHERE u.id=$1 AND NOT u.disabled AND (u.platform_admin OR (m.role IS NOT NULL AND (NOT $3 OR m.role='team_admin'))))")
            .bind(user).bind(team).bind(admin_required).fetch_one(&self.db).await?;
        if allowed {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
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
        let user = self.authenticate(session).await?;
        let rows = sqlx::query("SELECT t.id,t.name,CASE WHEN u.platform_admin THEN 'platform_admin' ELSE m.role END AS role FROM identity_users u JOIN identity_teams t ON true LEFT JOIN identity_memberships m ON m.team_id=t.id AND m.user_id=u.id WHERE u.id=$1 AND NOT u.disabled AND (u.platform_admin OR m.role IS NOT NULL) ORDER BY t.name")
            .bind(user).fetch_all(&self.db).await?;
        rows.into_iter()
            .map(|r| {
                Ok(Team {
                    id: r.try_get("id")?,
                    name: r.try_get("name")?,
                    role: r.try_get("role")?,
                })
            })
            .collect()
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
        agent: Option<&str>,
    ) -> Result<Uuid> {
        let mut tx = self.db.begin().await?;
        let user = lock_session(&mut tx, &self.signing_key, session).await?;
        authorize_locked(&mut tx, user, team, true).await?;
        if name.trim().is_empty() {
            return Err(Error::Invalid("empty project name"));
        }
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO identity_projects(id,team_id,name,cost_centre) VALUES ($1,$2,$3,$4)",
        )
        .bind(id)
        .bind(team)
        .bind(name)
        .bind(cost_centre)
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
        let user = self.authenticate(session).await?;
        self.authorize(user, team, false).await?;
        let rows = sqlx::query("SELECT id,team_id,name,cost_centre FROM identity_projects WHERE team_id=$1 ORDER BY name").bind(team).fetch_all(&self.db).await?;
        rows.into_iter()
            .map(|r| {
                Ok(Project {
                    id: r.try_get("id")?,
                    team_id: r.try_get("team_id")?,
                    name: r.try_get("name")?,
                    cost_centre: r.try_get("cost_centre")?,
                })
            })
            .collect()
    }

    pub async fn issue_key(
        &self,
        session: &str,
        input: NewKey,
        agent: Option<&str>,
    ) -> Result<IssuedKey> {
        let mut tx = self.db.begin().await?;
        let user = lock_session(&mut tx, &self.signing_key, session).await?;
        authorize_locked(&mut tx, user, input.team_id, false).await?;
        if input.expires_at <= Utc::now() {
            return Err(Error::Invalid("expiry must be in the future"));
        }
        if input.kind != KeyKind::Personal && input.project_id.is_none() {
            return Err(Error::Invalid("project required"));
        }
        if input.budget.is_some_and(|v| v < 0)
            || input.allowed_services.iter().any(|s| s.trim().is_empty())
        {
            return Err(Error::Invalid("invalid key metadata"));
        }
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let secret = format!("thl_{}", URL_SAFE_NO_PAD.encode(random));
        let digest = Sha256::digest(secret.as_bytes());
        let id = Uuid::new_v4();
        let row = sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at) VALUES ($1,$2,$3,$3,$4,$5,$6,$7,$8,$9) RETURNING id,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,revoked_at,created_at")
            .bind(id).bind(digest.as_slice()).bind(user).bind(input.team_id).bind(input.project_id).bind(input.kind.as_str()).bind(&input.allowed_services).bind(input.budget).bind(input.expires_at).fetch_one(&mut *tx).await?;
        let key = key_metadata(row)?;
        audit(&mut tx, Some(user), agent, "key.issue", &id.to_string()).await?;
        tx.commit().await?;
        Ok(IssuedKey { key, secret })
    }

    pub async fn list_keys(&self, session: &str, team: Uuid) -> Result<Vec<KeyMetadata>> {
        let user = self.authenticate(session).await?;
        self.authorize(user, team, false).await?;
        let rows = sqlx::query("SELECT id,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at,revoked_at,created_at FROM identity_keys WHERE team_id=$1 AND (user_id=$2 OR EXISTS (SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$1 WHERE u.id=$2 AND (u.platform_admin OR m.role='team_admin'))) ORDER BY created_at DESC")
            .bind(team).bind(user).fetch_all(&self.db).await?;
        rows.into_iter().map(key_metadata).collect()
    }

    pub async fn revoke_key(
        &self,
        session: &str,
        team: Uuid,
        id: Uuid,
        agent: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let user = lock_session(&mut tx, &self.signing_key, session).await?;
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
async fn active_projection(db: &PgPool) -> Result<Vec<ProjectionKey>> {
    let rows = sqlx::query("SELECT k.id,k.user_id,k.creator_id,k.team_id,k.project_id,k.kind,k.allowed_services,k.budget,k.expires_at,k.revoked_at,k.created_at,k.digest,p.cost_centre FROM identity_keys k JOIN identity_users u ON u.id=k.user_id LEFT JOIN identity_projects p ON p.id=k.project_id AND p.team_id=k.team_id WHERE k.revoked_at IS NULL AND k.expires_at>now() AND NOT u.disabled AND (u.platform_admin OR EXISTS (SELECT 1 FROM identity_memberships m WHERE m.user_id=u.id AND m.team_id=k.team_id)) ORDER BY k.id").fetch_all(db).await?;
    rows.into_iter()
        .map(|r| {
            Ok(ProjectionKey {
                digest: r.try_get("digest")?,
                cost_centre: r.try_get("cost_centre")?,
                metadata: key_metadata(r)?,
            })
        })
        .collect()
}
fn key_metadata(r: sqlx::postgres::PgRow) -> Result<KeyMetadata> {
    Ok(KeyMetadata {
        id: r.try_get("id")?,
        user_id: r.try_get("user_id")?,
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
    fn session_signature_rejects_tampering_and_wrong_key() {
        let id = Uuid::new_v4();
        let token = sign(&[1; 32], id);
        assert_eq!(verify(&[1; 32], &token).unwrap(), id);
        assert!(verify(&[2; 32], &token).is_err());
        assert!(verify(&[1; 32], &format!("{}x", token)).is_err());
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
