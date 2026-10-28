use std::{fs, path::Path, process::Command};
use tempfile::TempDir;
use thalamus_access::{Error, PolicySource, ServiceId};

const TEAM: &str = "a4db7aae-fd26-4f09-b652-e3d75cd4ac21";

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn repo() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(dir.path(), &["config", "user.name", "Test"]);
    fs::create_dir_all(dir.path().join("catalog/services")).unwrap();
    dir
}
fn commit(dir: &TempDir) -> String {
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "publish"]);
    git(dir.path(), &["rev-parse", "HEAD"])
}
fn source(dir: &TempDir, commit: &str) -> PolicySource {
    PolicySource::new(dir.path(), "catalog", commit)
}
fn write_service(dir: &TempDir, body: &str) {
    fs::write(dir.path().join("catalog/services/model.yaml"), body).unwrap();
}
fn valid() -> String {
    format!(
        "name: model\nnamespace: tenant-a\ndeployments:\n  - name: primary\naccess:\n  teams: [{TEAM}]\n"
    )
}

// This loopback endpoint only bootstraps the real IdentityService via OIDC
// discovery. Authorization below is exercised against its actual PostgreSQL
// state and the real committed Git policy, not a mocked authorization result.
// The bundled private key is public test material and must never be deployed.
async fn identity_fixture(
    pool: sqlx::PgPool,
) -> (
    thalamus_identity::IdentityService,
    String,
    tokio::task::JoinHandle<()>,
) {
    use std::{io::Cursor, sync::Arc};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{Certificate, PrivateKey, ServerConfig},
    };

    let certificates = rustls_pemfile::certs(&mut Cursor::new(
        include_bytes!("fixtures/oidc-test-server.pem").as_slice(),
    ))
    .unwrap()
    .into_iter()
    .map(Certificate)
    .collect();
    let key = rustls_pemfile::pkcs8_private_keys(&mut Cursor::new(
        include_bytes!("fixtures/oidc-test-server-key.pem").as_slice(),
    ))
    .unwrap()
    .into_iter()
    .next()
    .map(PrivateKey)
    .unwrap();
    let tls = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("https://{}", listener.local_addr().unwrap());
    let metadata = format!(
        r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/authorize","token_endpoint":"{issuer}/token","jwks_uri":"{issuer}/jwks","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}}"#
    );
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let task = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let metadata = metadata.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(socket).await else {
                    return;
                };
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let Ok(read) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 || request.len() + read > 8192 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("");
                let (status, body) = if path.ends_with("/.well-known/openid-configuration") {
                    ("200 OK", metadata.as_str())
                } else if path == "/jwks" {
                    ("200 OK", r#"{"keys":[]}"#)
                } else {
                    ("404 Not Found", "{}")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    let identity = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        thalamus_identity::IdentityService::discover(
            pool,
            thalamus_identity::Config {
                issuer: issuer.clone(),
                client_id: "access-test".into(),
                client_secret: None,
                redirect_uri: format!("{issuer}/callback"),
                ca_pem: Some(include_bytes!("fixtures/oidc-test-ca.pem").to_vec()),
                session_signing_key: vec![7; 32],
                session_lifetime: chrono::Duration::hours(1),
                groups: Default::default(),
            },
        ),
    )
    .await
    .expect("local OIDC discovery timed out")
    .unwrap();
    (identity, issuer, task)
}

fn verified<'a>(issuer: &'a str, subject: &'a str) -> thalamus_identity::VerifiedPrincipal<'a> {
    thalamus_identity::VerifiedPrincipal::Oidc { issuer, subject }
}

fn signed_session(id: uuid::Uuid) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&[7; 32]).unwrap();
    mac.update(id.to_string().as_bytes());
    format!(
        "{id}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

#[sqlx::test(migrations = false)]
async fn authorization_uses_current_identity_and_exact_committed_team_grants(pool: sqlx::PgPool) {
    use thalamus_identity::{Error as IdentityError, VerifiedPrincipal};
    use uuid::Uuid;

    for migration in [
        thalamus_identity::INITIAL_MIGRATION,
        thalamus_identity::WORKER_KEYS_MIGRATION,
        thalamus_identity::BUDGET_CAPS_MIGRATION,
        thalamus_identity::IDENTITY_BINDING_MIGRATION,
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }
    let (identity, issuer, discovery) = identity_fixture(pool.clone()).await;
    let team_a = Uuid::parse_str(TEAM).unwrap();
    let team_b = Uuid::from_u128(2);
    let foreign = Uuid::from_u128(3);
    for (id, name) in [(team_a, "alpha"), (team_b, "beta"), (foreign, "foreign")] {
        sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,$2)")
            .bind(id)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
    }
    let admin = Uuid::from_u128(101);
    let member = Uuid::from_u128(102);
    let dual_member = Uuid::from_u128(103);
    let outsider = Uuid::from_u128(104);
    let unassigned = Uuid::from_u128(105);
    let disabled = Uuid::from_u128(106);
    for (id, subject, platform_admin) in [
        (admin, "admin", true),
        (member, "member", false),
        (dual_member, "dual", false),
        (outsider, "outsider", false),
        (unassigned, "unassigned", false),
        (disabled, "disabled", false),
    ] {
        sqlx::query(
            "INSERT INTO identity_users(id,issuer,subject,platform_admin) VALUES ($1,$2,$3,$4)",
        )
        .bind(id)
        .bind(&issuer)
        .bind(subject)
        .bind(platform_admin)
        .execute(&pool)
        .await
        .unwrap();
    }
    for (user, team, role) in [
        (member, team_a, "member"),
        (member, foreign, "team_admin"),
        (dual_member, team_a, "member"),
        (dual_member, team_b, "team_admin"),
        (dual_member, foreign, "member"),
        (outsider, foreign, "member"),
        (disabled, team_a, "member"),
    ] {
        sqlx::query("INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,$3)")
            .bind(user)
            .bind(team)
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
    }

    let dir = repo();
    write_service(
        &dir,
        &format!(
            "name: model\nnamespace: tenant-a\ndeployments: []\naccess: {{teams: [{team_a}, {team_b}]}}\n"
        ),
    );
    fs::write(
        dir.path().join("catalog/services/empty.yaml"),
        "name: empty\nnamespace: tenant-a\ndeployments: []\naccess: {teams: []}\n",
    )
    .unwrap();
    let revision = commit(&dir);
    let policy = source(&dir, &revision).load().unwrap();
    let service = ServiceId {
        namespace: "tenant-a".into(),
        name: "model".into(),
    };
    let no_team_service = ServiceId {
        namespace: "tenant-a".into(),
        name: "empty".into(),
    };
    let unknown_service = ServiceId {
        namespace: "tenant-a".into(),
        name: "unknown".into(),
    };

    // Platform administrators need no team membership, but even their list
    // of usable teams is exactly the catalog's committed grant set.
    policy
        .authorize(&identity, verified(&issuer, "admin"), &service)
        .await
        .unwrap();
    let mut admin_teams = policy
        .authorized_teams(&identity, verified(&issuer, "admin"), &service)
        .await
        .unwrap();
    admin_teams.sort();
    let mut expected = vec![team_a, team_b];
    expected.sort();
    assert_eq!(admin_teams, expected);
    policy
        .authorize(&identity, verified(&issuer, "admin"), &no_team_service)
        .await
        .unwrap();
    assert!(matches!(
        policy
            .authorized_teams(&identity, verified(&issuer, "admin"), &no_team_service)
            .await,
        Err(Error::Forbidden)
    ));
    assert!(matches!(
        policy
            .authorize(&identity, verified(&issuer, "admin"), &unknown_service)
            .await,
        Err(Error::Forbidden)
    ));

    policy
        .authorize(&identity, verified(&issuer, "member"), &service)
        .await
        .unwrap();
    assert_eq!(
        policy
            .authorized_teams(&identity, verified(&issuer, "member"), &service)
            .await
            .unwrap(),
        vec![team_a]
    );
    policy
        .authorize(&identity, verified(&issuer, "dual"), &service)
        .await
        .unwrap();
    let mut dual_teams = policy
        .authorized_teams(&identity, verified(&issuer, "dual"), &service)
        .await
        .unwrap();
    dual_teams.sort();
    assert_eq!(dual_teams, expected);
    for subject in ["outsider", "unassigned"] {
        assert!(matches!(
            policy
                .authorize(&identity, verified(&issuer, subject), &service)
                .await,
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            policy
                .authorized_teams(&identity, verified(&issuer, subject), &service)
                .await,
            Err(Error::Forbidden)
        ));
    }
    assert!(matches!(
        policy
            .authorize(&identity, verified(&issuer, "missing"), &service)
            .await,
        Err(Error::Identity(IdentityError::Unauthorized))
    ));
    assert!(matches!(
        policy
            .authorize(
                &identity,
                verified("https://other.example", "member"),
                &service
            )
            .await,
        Err(Error::Identity(IdentityError::Unauthorized))
    ));

    // Session revocation and user disable take effect even if the subject was
    // previously allowed and the catalog revision itself has not changed.
    let session = Uuid::from_u128(201);
    sqlx::query("INSERT INTO identity_sessions(id,user_id,expires_at) VALUES ($1,$2,now()+interval '1 hour')")
        .bind(session).bind(member).execute(&pool).await.unwrap();
    let cookie = signed_session(session);
    policy
        .authorize(&identity, VerifiedPrincipal::Session(&cookie), &service)
        .await
        .unwrap();
    sqlx::query("DELETE FROM identity_sessions WHERE id=$1")
        .bind(session)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        policy
            .authorize(&identity, VerifiedPrincipal::Session(&cookie), &service)
            .await,
        Err(Error::Identity(IdentityError::Unauthorized))
    ));
    policy
        .authorize(&identity, verified(&issuer, "disabled"), &service)
        .await
        .unwrap();
    sqlx::query("UPDATE identity_users SET disabled=true WHERE id=$1")
        .bind(disabled)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        policy
            .authorize(&identity, verified(&issuer, "disabled"), &service)
            .await,
        Err(Error::Identity(IdentityError::Unauthorized))
    ));
    assert!(matches!(
        policy
            .authorized_teams(&identity, verified(&issuer, "disabled"), &service)
            .await,
        Err(Error::Identity(IdentityError::Unauthorized))
    ));

    sqlx::query("DELETE FROM identity_memberships WHERE user_id=$1 AND team_id=$2")
        .bind(member)
        .bind(team_a)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        policy
            .authorize(&identity, verified(&issuer, "member"), &service)
            .await,
        Err(Error::Forbidden)
    ));
    assert!(matches!(
        policy
            .authorized_teams(&identity, verified(&issuer, "member"), &service)
            .await,
        Err(Error::Forbidden)
    ));
    sqlx::query("INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')")
        .bind(member)
        .bind(team_b)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        policy
            .authorized_teams(&identity, verified(&issuer, "member"), &service)
            .await
            .unwrap(),
        vec![team_b]
    );

    // Neither an old snapshot nor a dirty worktree can authorize against a
    // newly published revision.
    write_service(
        &dir,
        "name: model\nnamespace: tenant-a\ndeployments: []\naccess: {teams: []}\n",
    );
    commit(&dir);
    assert!(matches!(
        policy
            .authorize(&identity, verified(&issuer, "admin"), &service)
            .await,
        Err(Error::Source)
    ));
    assert!(matches!(
        policy
            .authorized_teams(&identity, verified(&issuer, "member"), &service)
            .await,
        Err(Error::Source)
    ));
    discovery.abort();
}

#[test]
fn requires_published_revision_and_ignores_worktree_edits() {
    let dir = repo();
    write_service(&dir, &valid());
    fs::write(
        dir.path().join("catalog/hardware.yaml"),
        "published: true\n",
    )
    .unwrap();
    let revision = commit(&dir);
    let configured = source(&dir, &revision);
    let policy = configured.load().unwrap();
    let id = ServiceId {
        namespace: "tenant-a".into(),
        name: "model".into(),
    };
    assert_eq!(policy.service_id("model").unwrap(), Some(id.clone()));
    assert_eq!(policy.service_id("other").unwrap(), None);
    assert!(policy.includes_deployment(&id, "primary").unwrap());
    assert!(!policy.includes_deployment(&id, "other").unwrap());
    let allowed_team = uuid::Uuid::parse_str(TEAM).unwrap();
    assert!(policy.team_allowed(&id, allowed_team).unwrap());
    assert!(
        !policy
            .team_allowed(
                &id,
                uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap()
            )
            .unwrap()
    );
    let other_tenant = ServiceId {
        namespace: "tenant-b".into(),
        name: "model".into(),
    };
    assert!(matches!(
        policy.includes_deployment(&other_tenant, "primary"),
        Err(Error::Forbidden)
    ));
    assert!(matches!(
        policy.team_allowed(&other_tenant, allowed_team),
        Err(Error::Forbidden)
    ));
    write_service(&dir, "not: valid\n");
    fs::write(
        dir.path().join("catalog/hardware.yaml"),
        "published: false\n",
    )
    .unwrap();
    assert_eq!(
        policy.committed_catalog_blob("hardware.yaml").unwrap(),
        b"published: true\n"
    );
    assert!(matches!(
        policy.committed_catalog_blob("../hardware.yaml"),
        Err(Error::Source)
    ));
    assert!(matches!(
        policy.committed_catalog_blob("missing.yaml"),
        Err(Error::Source)
    ));
    let blobs = policy.committed_services().unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[0].0, "model.yaml");
    assert_eq!(blobs[0].1, valid().as_bytes());
    assert!(
        configured.load().is_ok(),
        "policy must use committed blobs, not worktree"
    );
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "advance"]);
    assert!(
        matches!(configured.load(), Err(Error::Source)),
        "old revision must fail closed"
    );
    assert!(matches!(
        policy.includes_deployment(&id, "primary"),
        Err(Error::Source)
    ));
    assert!(matches!(policy.committed_services(), Err(Error::Source)));
    assert!(matches!(policy.service_id("model"), Err(Error::Source)));
    assert!(matches!(
        policy.team_allowed(&id, allowed_team),
        Err(Error::Source)
    ));
    assert!(matches!(
        policy.committed_catalog_blob("hardware.yaml"),
        Err(Error::Source)
    ));
    assert!(matches!(
        source(&dir, &revision[..12]).load(),
        Err(Error::Source)
    ));
}

#[test]
fn rejects_missing_or_invalid_access_policy() {
    for body in [
        "name: model\nnamespace: tenant-a\ndeployments: []\n",
        "name: model\nnamespace: tenant-a\ndeployments: []\naccess: {}\n",
        "name: model\nnamespace: tenant-a\ndeployments: []\naccess: {teams: [not-a-uuid]}\n",
        "name: model\nnamespace: tenant-a\ndeployments: []\naccess: {teams: [00000000-0000-0000-0000-000000000000]}\n",
        "name: model\nnamespace: tenant-a\ndeployments: []\naccess: {teams: [], public: true}\n",
    ] {
        let dir = repo();
        write_service(&dir, body);
        let revision = commit(&dir);
        assert!(
            matches!(source(&dir, &revision).load(), Err(Error::Source)),
            "accepted {body}"
        );
    }
}

#[test]
fn rejects_duplicate_names_and_policy_team_ids() {
    let dir = repo();
    write_service(&dir, &valid());
    fs::write(
        dir.path().join("catalog/services/other.yml"),
        valid().replace("tenant-a", "tenant-b"),
    )
    .unwrap();
    let revision = commit(&dir);
    assert!(matches!(source(&dir, &revision).load(), Err(Error::Source)));
    fs::remove_file(dir.path().join("catalog/services/other.yml")).unwrap();
    write_service(
        &dir,
        &format!(
            "name: model\nnamespace: tenant-a\ndeployments: []\naccess: {{teams: [{TEAM}, {TEAM}]}}\n"
        ),
    );
    let revision = commit(&dir);
    assert!(matches!(source(&dir, &revision).load(), Err(Error::Source)));
}

#[test]
fn missing_catalog_or_untrusted_path_fail_closed() {
    let dir = repo();
    write_service(&dir, &valid());
    let revision = commit(&dir);
    assert!(matches!(
        PolicySource::new(dir.path(), "missing", &revision).load(),
        Err(Error::Source)
    ));
    assert!(matches!(
        PolicySource::new(dir.path(), "../catalog", &revision).load(),
        Err(Error::Source)
    ));
    assert!(matches!(
        PolicySource::new(dir.path(), "catalog", "HEAD").load(),
        Err(Error::Source)
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_service_blob() {
    use std::os::unix::fs::symlink;
    let dir = repo();
    symlink(
        "../../other",
        dir.path().join("catalog/services/model.yaml"),
    )
    .unwrap();
    let revision = commit(&dir);
    assert!(matches!(source(&dir, &revision).load(), Err(Error::Source)));
}
