//! Opt-in test for the disposable Keycloak fixture. The fixture provisions a
//! browser user first and disables that user in Keycloak before invoking this.
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use thalamus_identity::{
    keycloak::{KeycloakCollector, KeycloakConfig},
    GroupMapping, WorkerIdentity,
};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires the disposable OIDC fixture and an upstream-disabled user"]
async fn keycloak_offboarding_revokes_existing_keys() {
    let issuer = std::env::var("THALAMUS_OIDC_ISSUER").expect("issuer");
    let subject = std::env::var("THALAMUS_SMOKE_OFFBOARD_SUBJECT").expect("subject");
    let mapping: GroupMapping =
        serde_json::from_str(&std::env::var("THALAMUS_GROUP_MAPPING").expect("group mapping"))
            .expect("valid group mapping");
    let ca_pem =
        std::env::var_os("THALAMUS_OIDC_CA_FILE").map(|path| std::fs::read(path).expect("CA file"));
    let collector = KeycloakCollector::new(KeycloakConfig {
        issuer: issuer.clone(),
        client_id: std::env::var("THALAMUS_IDP_KEYCLOAK_CLIENT_ID").expect("collector client id"),
        client_secret: std::env::var("THALAMUS_IDP_KEYCLOAK_CLIENT_SECRET")
            .expect("collector client secret"),
        ca_pem,
    })
    .expect("collector configuration");
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").expect("database URL"))
        .await
        .expect("database connection");
    let row = sqlx::query("SELECT u.id,m.team_id FROM identity_users u JOIN identity_memberships m ON m.user_id=u.id WHERE u.issuer=$1 AND u.subject=$2 AND NOT u.disabled LIMIT 1")
        .bind(&issuer).bind(&subject).fetch_one(&pool).await.expect("locally active provisioned user");
    let user: Uuid = row.get("id");
    let team: Uuid = row.get("team_id");
    let key_id = Uuid::new_v4();
    let digest = Sha256::digest(Uuid::new_v4().as_bytes());
    sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,kind,allowed_services,expires_at,name) VALUES ($1,$2,$3,$3,$4,'personal',ARRAY['smoke-model'],clock_timestamp()+interval '1 hour','keycloak-offboarding-smoke')")
        .bind(key_id).bind(digest.as_slice()).bind(user).bind(team)
        .execute(&pool).await.expect("local key");

    let snapshot = collector
        .snapshot()
        .await
        .expect("complete Keycloak snapshot");
    assert!(snapshot
        .subjects
        .iter()
        .any(|item| item.subject == subject && !item.active));
    let identity = WorkerIdentity::new(pool.clone());
    identity
        .sync_idp_snapshot(&issuer, &mapping, &snapshot)
        .await
        .expect("trusted IdP sync");

    let disabled: bool = sqlx::query_scalar("SELECT disabled FROM identity_users WHERE id=$1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM identity_keys WHERE id=$1")
            .bind(key_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let memberships: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identity_memberships WHERE user_id=$1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identity_sessions WHERE user_id=$1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(disabled && revoked);
    assert_eq!(memberships, 0);
    assert_eq!(sessions, 0);
    assert!(identity
        .active_projection()
        .await
        .unwrap()
        .iter()
        .all(|entry| entry.metadata.id != key_id));
}
