use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thalamus_identity::{
    BudgetCap, DebugKeyRequest, GroupMapping, IdpSnapshot, IdpSubjectSnapshot, RunKeyRequest,
    WorkerIdentity,
};
use uuid::Uuid;

const ISSUER: &str = "https://idp.example/realms/thalamus";
const OTHER_ISSUER: &str = "https://other-idp.example/realms/thalamus";
const TARGET: &str = "payments-api";
const MODEL: &str = "inference-model";
const DEBUG_BUDGET: i64 = 50;

struct Fixture {
    worker: WorkerIdentity,
    user: Uuid,
    team: Uuid,
    project: Uuid,
    session: Uuid,
    deadline: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl Fixture {
    fn request<'a>(&self, secret: &'a str) -> DebugKeyRequest<'a> {
        DebugKeyRequest {
            session_id: self.session,
            user_id: self.user,
            team_id: self.team,
            project_id: self.project,
            service: TARGET,
            model: MODEL,
            expires_at: self.expires_at,
            secret,
            budget: DEBUG_BUDGET,
        }
    }
}

fn now_micros() -> DateTime<Utc> {
    DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap()
}

fn secret(byte: u8) -> String {
    format!("thl_{}", URL_SAFE_NO_PAD.encode([byte; 32]))
}

fn cap(team: Uuid, classes: &[&str]) -> BudgetCap {
    BudgetCap {
        team_id: team,
        max_tokens: 100,
        unit: "tokens".into(),
        window: "day".into(),
        allowed_classes: classes.iter().map(|class| (*class).to_owned()).collect(),
    }
}

async fn insert_debug_session(pool: &PgPool, fixture: &Fixture, id: Uuid, budget: i64) {
    sqlx::query(
        "INSERT INTO thalamus_debug_sessions \
         (id,user_id,team_id,project_id,client_request_key,service_id,model,checkout_revision,\
          job_namespace,job_name,deadline,model_budget,tool_budget,service_context,failing_evidence) \
         VALUES ($1,$2,$3,$4,$1::text,$5,$6,$7,'debug-jobs','thalamus-debug-' || $1::text,\
                 $8,$9,10,'{\"service\":\"payments\"}'::jsonb,'{\"failure\":\"timeout\"}'::jsonb)",
    )
    .bind(id)
    .bind(fixture.user)
    .bind(fixture.team)
    .bind(fixture.project)
    .bind(TARGET)
    .bind(MODEL)
    .bind("a".repeat(40))
    .bind(fixture.deadline)
    .bind(budget)
    .execute(pool)
    .await
    .unwrap();
}

async fn fixture(pool: &PgPool) -> Fixture {
    // Identity and debug are deployed as separate migrations, not one test schema.
    sqlx::raw_sql(include_str!(
        "../../thalamus-debug/migrations/0001_debug.sql"
    ))
    .execute(pool)
    .await
    .unwrap();
    let user = Uuid::new_v4();
    let team = Uuid::new_v4();
    let project = Uuid::new_v4();
    let session = Uuid::new_v4();
    let now = now_micros();
    sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,$2,'alice')")
        .bind(user)
        .bind(ISSUER)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'payments')")
        .bind(team)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')")
        .bind(user)
        .bind(team)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_projects(id,team_id,name,cost_centre,budget,traffic_class) VALUES ($1,$2,'checkout','cost-payments',70,'interactive')")
        .bind(project)
        .bind(team)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_sessions(id,user_id,expires_at) VALUES ($1,$2,now()+interval '2 hours')")
        .bind(Uuid::new_v4())
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    let worker = WorkerIdentity::new(pool.clone());
    worker
        .sync_budget_caps(&"a".repeat(40), &[cap(team, &["standard", "interactive"])])
        .await
        .unwrap();
    let fixture = Fixture {
        worker,
        user,
        team,
        project,
        session,
        deadline: now + Duration::hours(1),
        expires_at: now + Duration::minutes(40),
    };
    insert_debug_session(pool, &fixture, session, DEBUG_BUDGET).await;
    fixture
}

async fn seed_member(pool: &PgPool, team: Uuid, issuer: &str, subject: &str) -> Uuid {
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,$2,$3)")
        .bind(user)
        .bind(issuer)
        .bind(subject)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')")
        .bind(user)
        .bind(team)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_sessions(id,user_id,expires_at) VALUES ($1,$2,now()+interval '2 hours')")
        .bind(Uuid::new_v4())
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    user
}

async fn seed_ordinary_keys(
    pool: &PgPool,
    fixture: &Fixture,
    user: Uuid,
    digest: u8,
) -> (Uuid, Uuid) {
    let project_key = Uuid::new_v4();
    let personal_key = Uuid::new_v4();
    sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,project_id,kind,allowed_services,budget,expires_at) VALUES ($1,$2,$3,$3,$4,$5,'project',ARRAY['inference-model'],35,now()+interval '2 hours')")
        .bind(project_key)
        .bind(vec![digest; 32])
        .bind(user)
        .bind(fixture.team)
        .bind(fixture.project)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,kind,allowed_services) VALUES ($1,$2,$3,$3,$4,'personal',ARRAY['inference-model'])")
        .bind(personal_key)
        .bind(vec![digest + 1; 32])
        .bind(user)
        .bind(fixture.team)
        .execute(pool)
        .await
        .unwrap();
    (project_key, personal_key)
}

fn group_mapping(team: Uuid) -> GroupMapping {
    let mut mapping = GroupMapping {
        platform_admin_group: Some("platform-admins".into()),
        ..GroupMapping::default()
    };
    mapping.member_groups.insert("members".into(), team);
    mapping.team_admin_groups.insert("owners".into(), team);
    mapping
}

fn subject(name: &str, active: bool, groups: &[&str]) -> IdpSubjectSnapshot {
    IdpSubjectSnapshot {
        subject: name.into(),
        active,
        groups: groups.iter().map(|group| (*group).to_owned()).collect(),
    }
}

async fn snapshot_record(pool: &PgPool) -> (DateTime<Utc>, Vec<u8>) {
    sqlx::query_as("SELECT observed_at,digest FROM identity_idp_projection WHERE issuer=$1")
        .bind(ISSUER)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn snapshot_audits(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM identity_audit WHERE action='identity.snapshot' AND target=$1",
    )
    .bind(ISSUER)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn debug_audits(pool: &PgPool, action: &str, session: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM identity_audit WHERE agent='thalamus-worker' AND action=$1 AND details->>'debug_session_id'=$2")
        .bind(action)
        .bind(session.to_string())
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn assert_live_user(
    pool: &PgPool,
    user: Uuid,
    team: Uuid,
    admin: bool,
    role: &str,
    observed: Option<DateTime<Utc>>,
    key_count: i64,
) {
    let state: (bool, bool, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT disabled,platform_admin,groups_issued_at FROM identity_users WHERE id=$1",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(state, (false, admin, observed));
    let stored_role: String =
        sqlx::query_scalar("SELECT role FROM identity_memberships WHERE user_id=$1 AND team_id=$2")
            .bind(user)
            .bind(team)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(stored_role, role);
    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identity_sessions WHERE user_id=$1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap();
    let keys: (i64, i64) = sqlx::query_as("SELECT count(*),count(*) FILTER (WHERE revoked_at IS NULL) FROM identity_keys WHERE user_id=$1")
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(sessions, 1);
    assert_eq!(keys, (key_count, key_count));
}

async fn assert_offboarded(pool: &PgPool, user: Uuid, key_count: i64) {
    let state: (bool, bool) =
        sqlx::query_as("SELECT disabled,platform_admin FROM identity_users WHERE id=$1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap();
    let memberships: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identity_memberships WHERE user_id=$1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap();
    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM identity_sessions WHERE user_id=$1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap();
    let keys: (i64, i64) = sqlx::query_as("SELECT count(*),count(*) FILTER (WHERE revoked_at IS NULL) FROM identity_keys WHERE user_id=$1")
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, (true, false));
    assert_eq!(memberships, 0);
    assert_eq!(sessions, 0);
    assert_eq!(keys, (key_count, 0));
}

async fn assert_debug_not_projected(worker: &WorkerIdentity, session: Uuid) {
    assert!(worker.active_debug_key(session).await.unwrap().is_none());
    assert!(!worker
        .active_projection()
        .await
        .unwrap()
        .iter()
        .any(|key| key.debug_session_id == Some(session)));
}

#[sqlx::test(migrations = "./migrations")]
async fn debug_registration_and_recovery_are_bound_to_the_complete_live_session(pool: PgPool) {
    let fixture = fixture(&pool).await;
    let worker = &fixture.worker;
    let debug_secret = secret(1);
    let request = fixture.request(&debug_secret);

    assert!(worker.debug_key(fixture.session).await.unwrap().is_none());
    assert!(worker.verify_debug_key(request).await.is_err());
    assert!(worker.debug_key(fixture.session).await.unwrap().is_none());
    let first = worker.register_debug_key(request).await.unwrap();
    let retry = worker.register_debug_key(request).await.unwrap();
    let verified = worker.verify_debug_key(request).await.unwrap();
    assert_eq!(first.id, retry.id);
    assert_eq!(first.id, verified.id);
    assert_eq!(first.kind, "debug");
    assert_eq!(first.user_id, fixture.user);
    assert_eq!(first.creator_id, fixture.user);
    assert_eq!(first.team_id, fixture.team);
    assert_eq!(first.project_id, Some(fixture.project));
    assert_eq!(first.allowed_services, vec![MODEL]);
    assert_eq!(first.budget, Some(DEBUG_BUDGET));
    assert_eq!(first.expires_at, Some(fixture.expires_at));
    assert!(first.revoked_at.is_none());

    let digest = Sha256::digest(debug_secret.as_bytes()).to_vec();
    let recovered = worker.debug_key(fixture.session).await.unwrap().unwrap();
    assert_eq!(recovered.metadata.id, first.id);
    assert_eq!(recovered.digest, digest);
    assert_eq!(recovered.session_id, fixture.session);
    assert_eq!(recovered.service, TARGET);
    assert_eq!(recovered.model, MODEL);
    assert_eq!(
        worker
            .active_debug_key(fixture.session)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    let projection = worker.active_projection().await.unwrap();
    assert_eq!(projection.len(), 1);
    let key = &projection[0];
    assert_eq!(key.digest, digest);
    assert_eq!(key.metadata.allowed_services, vec![MODEL]);
    assert!(!key
        .metadata
        .allowed_services
        .iter()
        .any(|service| service == TARGET));
    assert_eq!(key.metadata.budget, Some(DEBUG_BUDGET));
    assert_eq!(key.metadata.expires_at, Some(fixture.expires_at));
    assert_eq!(key.debug_session_id, Some(fixture.session));
    assert_eq!(key.debug_model.as_deref(), Some(MODEL));
    assert_eq!(key.run_id, None);
    assert_eq!(key.deployment, None);
    assert_eq!(key.run_kind, None);
    assert_eq!(key.request_budget, None);
    assert_eq!(key.class, "interactive");
    assert_eq!(key.cost_centre.as_deref(), Some("cost-payments"));
    assert_eq!(key.project_budget, Some(70));
    assert_eq!(key.team_budget, Some(100));
    assert_eq!(key.budget_window.as_deref(), Some("day"));
    assert_eq!(debug_audits(&pool, "key.issue", fixture.session).await, 1);

    // Give alternate principals and scopes valid independent authorization, so
    // these failures exercise the durable debug binding rather than missing rows.
    let other_user = seed_member(&pool, fixture.team, ISSUER, "other-user").await;
    let other_team = Uuid::new_v4();
    let other_project = Uuid::new_v4();
    sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'other-team')")
        .bind(other_team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')")
        .bind(fixture.user)
        .bind(other_team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_projects(id,team_id,name) VALUES ($1,$2,'other-project')")
        .bind(other_project)
        .bind(fixture.team)
        .execute(&pool)
        .await
        .unwrap();
    let different_secret = secret(2);
    let short_secret = format!("thl_{}", URL_SAFE_NO_PAD.encode([3u8; 31]));
    let padded_secret = format!("{debug_secret}=");
    let mismatches = [
        (
            "model",
            DebugKeyRequest {
                model: "other-model",
                ..request
            },
        ),
        (
            "target",
            DebugKeyRequest {
                service: "other-target",
                ..request
            },
        ),
        (
            "user",
            DebugKeyRequest {
                user_id: other_user,
                ..request
            },
        ),
        (
            "team",
            DebugKeyRequest {
                team_id: other_team,
                ..request
            },
        ),
        (
            "project",
            DebugKeyRequest {
                project_id: other_project,
                ..request
            },
        ),
        (
            "budget",
            DebugKeyRequest {
                budget: DEBUG_BUDGET + 1,
                ..request
            },
        ),
        (
            "zero budget",
            DebugKeyRequest {
                budget: 0,
                ..request
            },
        ),
        (
            "different expiry",
            DebugKeyRequest {
                expires_at: fixture.expires_at - Duration::seconds(1),
                ..request
            },
        ),
        (
            "after deadline",
            DebugKeyRequest {
                expires_at: fixture.deadline + Duration::seconds(1),
                ..request
            },
        ),
        (
            "different digest",
            DebugKeyRequest {
                secret: &different_secret,
                ..request
            },
        ),
        (
            "invalid prefix",
            DebugKeyRequest {
                secret: "invalid",
                ..request
            },
        ),
        (
            "short secret",
            DebugKeyRequest {
                secret: &short_secret,
                ..request
            },
        ),
        (
            "noncanonical secret",
            DebugKeyRequest {
                secret: &padded_secret,
                ..request
            },
        ),
    ];
    for (label, invalid) in mismatches {
        assert!(
            worker.register_debug_key(invalid).await.is_err(),
            "registered {label}"
        );
        assert!(
            worker.verify_debug_key(invalid).await.is_err(),
            "verified {label}"
        );
    }

    let second_session = Uuid::new_v4();
    insert_debug_session(&pool, &fixture, second_session, DEBUG_BUDGET).await;
    let duplicate = DebugKeyRequest {
        session_id: second_session,
        ..request
    };
    assert!(worker.register_debug_key(duplicate).await.is_err());
    assert!(worker.verify_debug_key(duplicate).await.is_err());
    assert!(worker.debug_key(second_session).await.unwrap().is_none());

    let over_cap_session = Uuid::new_v4();
    insert_debug_session(&pool, &fixture, over_cap_session, 101).await;
    let over_cap_secret = secret(4);
    let over_cap = DebugKeyRequest {
        session_id: over_cap_session,
        budget: 101,
        secret: &over_cap_secret,
        ..request
    };
    assert!(worker.register_debug_key(over_cap).await.is_err());
    assert!(worker.debug_key(over_cap_session).await.unwrap().is_none());
    sqlx::query("UPDATE identity_budget_projection SET refreshed_at=clock_timestamp()-interval '61 seconds'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(worker.register_debug_key(request).await.is_err());
    assert!(worker.verify_debug_key(request).await.is_err());
    worker
        .sync_budget_caps(
            &"a".repeat(40),
            &[cap(fixture.team, &["standard", "interactive"])],
        )
        .await
        .unwrap();
    assert_eq!(worker.verify_debug_key(request).await.unwrap().id, first.id);
    assert_eq!(debug_audits(&pool, "key.issue", fixture.session).await, 1);
    assert_eq!(
        worker
            .debug_key(fixture.session)
            .await
            .unwrap()
            .unwrap()
            .digest,
        digest
    );

    // A class removed from the trusted cap or a non-canonical project limit
    // must suppress an already-issued key instead of publishing unsafe limits.
    worker
        .sync_budget_caps(&"b".repeat(40), &[cap(fixture.team, &["standard"])])
        .await
        .unwrap();
    assert_debug_not_projected(worker, fixture.session).await;
    worker
        .sync_budget_caps(
            &"c".repeat(40),
            &[cap(fixture.team, &["standard", "interactive"])],
        )
        .await
        .unwrap();
    sqlx::query("UPDATE identity_projects SET budget=70.5 WHERE id=$1")
        .bind(fixture.project)
        .execute(&pool)
        .await
        .unwrap();
    assert_debug_not_projected(worker, fixture.session).await;
    sqlx::query("UPDATE identity_projects SET budget=70 WHERE id=$1")
        .bind(fixture.project)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        worker
            .active_debug_key(fixture.session)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn debug_projection_and_cleanup_follow_session_and_key_lifecycle(pool: PgPool) {
    let fixture = fixture(&pool).await;
    let worker = &fixture.worker;
    let absent = Uuid::new_v4();
    worker.revoke_debug_key(absent).await.unwrap();
    worker.revoke_debug_key(absent).await.unwrap();
    assert!(worker.debug_key(absent).await.unwrap().is_none());
    assert_debug_not_projected(worker, absent).await;
    assert_eq!(debug_audits(&pool, "key.revoke", absent).await, 0);

    let main_secret = secret(10);
    let main = fixture.request(&main_secret);
    let main_id = worker.register_debug_key(main).await.unwrap().id;
    sqlx::query("UPDATE thalamus_debug_sessions SET state='running' WHERE id=$1")
        .bind(fixture.session)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        worker
            .active_debug_key(fixture.session)
            .await
            .unwrap()
            .unwrap()
            .id,
        main_id
    );

    for (index, state) in ["completed", "failed", "cancelled", "timed_out"]
        .into_iter()
        .enumerate()
    {
        let session = Uuid::new_v4();
        insert_debug_session(&pool, &fixture, session, DEBUG_BUDGET).await;
        let session_secret = secret(11 + index as u8);
        let request = DebugKeyRequest {
            session_id: session,
            secret: &session_secret,
            ..main
        };
        worker.register_debug_key(request).await.unwrap();
        sqlx::query("UPDATE thalamus_debug_sessions SET state=$2,outcome='{\"result\":\"finished\"}'::jsonb WHERE id=$1")
            .bind(session)
            .bind(state)
            .execute(&pool)
            .await
            .unwrap();
        assert_debug_not_projected(worker, session).await;
        assert!(worker.register_debug_key(request).await.is_err());
        assert!(worker.verify_debug_key(request).await.is_err());
        assert!(worker
            .debug_key(session)
            .await
            .unwrap()
            .unwrap()
            .metadata
            .revoked_at
            .is_none());
        worker.revoke_debug_key(session).await.unwrap();
        worker.revoke_debug_key(session).await.unwrap();
        assert!(worker
            .debug_key(session)
            .await
            .unwrap()
            .unwrap()
            .metadata
            .revoked_at
            .is_some());
        assert_eq!(debug_audits(&pool, "key.revoke", session).await, 1);
    }

    let deadline_session = Uuid::new_v4();
    insert_debug_session(&pool, &fixture, deadline_session, DEBUG_BUDGET).await;
    let deadline_secret = secret(20);
    let deadline_request = DebugKeyRequest {
        session_id: deadline_session,
        secret: &deadline_secret,
        ..main
    };
    worker.register_debug_key(deadline_request).await.unwrap();
    // A shortened, still-future deadline also invalidates an overlong key.
    sqlx::query("UPDATE thalamus_debug_sessions SET deadline=clock_timestamp()+interval '5 minutes' WHERE id=$1")
        .bind(deadline_session)
        .execute(&pool)
        .await
        .unwrap();
    assert_debug_not_projected(worker, deadline_session).await;
    assert!(worker.verify_debug_key(deadline_request).await.is_err());
    sqlx::query("UPDATE thalamus_debug_sessions SET created_at=clock_timestamp()-interval '2 hours',deadline=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(deadline_session)
        .execute(&pool)
        .await
        .unwrap();
    assert_debug_not_projected(worker, deadline_session).await;
    assert!(worker.register_debug_key(deadline_request).await.is_err());
    assert!(worker.debug_key(deadline_session).await.unwrap().is_some());
    worker.revoke_debug_key(deadline_session).await.unwrap();

    let expired_session = Uuid::new_v4();
    insert_debug_session(&pool, &fixture, expired_session, DEBUG_BUDGET).await;
    let expired_secret = secret(21);
    let expired = DebugKeyRequest {
        session_id: expired_session,
        secret: &expired_secret,
        ..main
    };
    worker.register_debug_key(expired).await.unwrap();
    sqlx::query("UPDATE identity_keys SET expires_at=clock_timestamp()-interval '1 second' WHERE debug_session_id=$1")
        .bind(expired_session)
        .execute(&pool)
        .await
        .unwrap();
    assert_debug_not_projected(worker, expired_session).await;
    assert!(worker.debug_key(expired_session).await.unwrap().is_some());
    assert!(worker.verify_debug_key(expired).await.is_err());
    worker.revoke_debug_key(expired_session).await.unwrap();

    let removed_session = Uuid::new_v4();
    insert_debug_session(&pool, &fixture, removed_session, DEBUG_BUDGET).await;
    let removed_secret = secret(22);
    let removed = DebugKeyRequest {
        session_id: removed_session,
        secret: &removed_secret,
        ..main
    };
    worker.register_debug_key(removed).await.unwrap();
    sqlx::query("DELETE FROM thalamus_debug_sessions WHERE id=$1")
        .bind(removed_session)
        .execute(&pool)
        .await
        .unwrap();
    assert_debug_not_projected(worker, removed_session).await;
    worker.revoke_debug_key(removed_session).await.unwrap();
    worker.revoke_debug_key(removed_session).await.unwrap();
    assert!(worker
        .debug_key(removed_session)
        .await
        .unwrap()
        .unwrap()
        .metadata
        .revoked_at
        .is_some());

    worker.revoke_debug_key(fixture.session).await.unwrap();
    worker.revoke_debug_key(fixture.session).await.unwrap();
    assert_debug_not_projected(worker, fixture.session).await;
    let recovered = worker.debug_key(fixture.session).await.unwrap().unwrap();
    assert_eq!(recovered.metadata.id, main_id);
    assert!(recovered.metadata.revoked_at.is_some());
    assert!(worker.verify_debug_key(main).await.is_err());
    assert!(worker.register_debug_key(main).await.is_err());
    assert_eq!(debug_audits(&pool, "key.revoke", fixture.session).await, 1);
    assert!(worker.active_projection().await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn debug_session_ids_do_not_alias_personal_keys_or_run_ids(pool: PgPool) {
    let fixture = fixture(&pool).await;
    let worker = &fixture.worker;
    // The personal key ID and the job's run ID deliberately equal the debug
    // session ID, although all three identify different resources.
    sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,kind,allowed_services) VALUES ($1,$2,$3,$3,$4,'personal',ARRAY['inference-model'])")
        .bind(fixture.session)
        .bind(vec![240u8; 32])
        .bind(fixture.user)
        .bind(fixture.team)
        .execute(&pool)
        .await
        .unwrap();
    let run_secret = secret(30);
    let run = RunKeyRequest {
        run_id: fixture.session,
        deployment: "revision-a",
        run_kind: "benchmark",
        request_budget: Some(17),
        user_id: fixture.user,
        team_id: fixture.team,
        project_id: fixture.project,
        service: MODEL,
        expires_at: fixture.expires_at,
        secret: &run_secret,
        budget: 20,
    };
    let job = worker.register_run_key(run).await.unwrap();
    assert!(worker.debug_key(fixture.session).await.unwrap().is_none());
    assert_debug_not_projected(worker, fixture.session).await;
    worker.revoke_debug_key(fixture.session).await.unwrap();
    let projection = worker.active_projection().await.unwrap();
    assert_eq!(projection.len(), 2);
    let personal = projection
        .iter()
        .find(|key| key.metadata.id == fixture.session)
        .unwrap();
    assert_eq!(personal.metadata.kind, "personal");
    assert_eq!(personal.class, "standard");
    assert_eq!(personal.cost_centre, None);
    assert_eq!(personal.project_budget, None);
    assert_eq!(personal.team_budget, Some(100));
    let job_projection = projection
        .iter()
        .find(|key| key.metadata.id == job.id)
        .unwrap();
    assert_eq!(job_projection.run_id, Some(fixture.session));
    assert_eq!(job_projection.deployment.as_deref(), Some("revision-a"));
    assert_eq!(job_projection.run_kind.as_deref(), Some("benchmark"));
    assert_eq!(job_projection.request_budget, Some(17));
    assert_eq!(job_projection.debug_session_id, None);
    assert_eq!(job_projection.class, "interactive");
    assert_eq!(job_projection.cost_centre.as_deref(), Some("cost-payments"));
    assert_eq!(job_projection.project_budget, Some(70));
    assert_eq!(job_projection.team_budget, Some(100));
    assert_eq!(job_projection.budget_window.as_deref(), Some("day"));

    // A digest already owned by a job cannot be registered in the debug namespace.
    assert!(worker
        .register_debug_key(fixture.request(&run_secret))
        .await
        .is_err());
    assert!(worker.debug_key(fixture.session).await.unwrap().is_none());
    let debug_secret = secret(31);
    let debug = fixture.request(&debug_secret);
    let debug_key = worker.register_debug_key(debug).await.unwrap();
    assert_eq!(worker.active_projection().await.unwrap().len(), 3);
    worker.revoke_run_key(fixture.session).await.unwrap();
    assert_eq!(
        worker.verify_debug_key(debug).await.unwrap().id,
        debug_key.id
    );
    assert_eq!(
        worker
            .active_debug_key(fixture.session)
            .await
            .unwrap()
            .unwrap()
            .id,
        debug_key.id
    );
    worker.revoke_debug_key(fixture.session).await.unwrap();
    let projection = worker.active_projection().await.unwrap();
    assert_eq!(projection.len(), 1);
    assert_eq!(projection[0].metadata.id, fixture.session);
    assert_eq!(projection[0].metadata.kind, "personal");
}

#[sqlx::test(migrations = "./migrations")]
async fn ordinary_keys_remain_usable_without_the_separate_debug_migration(pool: PgPool) {
    let user = Uuid::new_v4();
    let team = Uuid::new_v4();
    let key_id = Uuid::new_v4();
    sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,$2,'ordinary')")
        .bind(user)
        .bind(ISSUER)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')")
        .bind(user)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,kind,allowed_services) VALUES ($1,$2,$3,$3,$4,'personal',ARRAY['model'])")
        .bind(key_id)
        .bind(vec![90u8; 32])
        .bind(user)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    let worker = WorkerIdentity::new(pool);
    assert!(worker.debug_key(key_id).await.unwrap().is_none());
    assert_debug_not_projected(&worker, key_id).await;
    worker.revoke_debug_key(key_id).await.unwrap();
    let projection = worker.active_projection().await.unwrap();
    assert_eq!(projection.len(), 1);
    assert_eq!(projection[0].metadata.id, key_id);
}

#[sqlx::test(migrations = "./migrations")]
async fn idp_snapshots_are_ordered_idempotent_and_bound_key_expiry(pool: PgPool) {
    let fixture = fixture(&pool).await;
    let worker = &fixture.worker;
    let debug_secret = secret(40);
    let debug = fixture.request(&debug_secret);
    let debug_id = worker.register_debug_key(debug).await.unwrap().id;
    let (project_key, personal_key) = seed_ordinary_keys(&pool, &fixture, fixture.user, 41).await;
    let other = seed_member(&pool, fixture.team, OTHER_ISSUER, "alice").await;
    let (other_project, other_personal) = seed_ordinary_keys(&pool, &fixture, other, 43).await;
    assert_eq!(worker.active_projection().await.unwrap().len(), 5);
    worker.enable_idp_sync(ISSUER).await.unwrap();
    let sentinel = snapshot_record(&pool).await;
    assert!(sentinel.0 < now_micros() - Duration::minutes(10));
    assert_eq!(sentinel.1.len(), 32);
    worker.enable_idp_sync(ISSUER).await.unwrap();
    assert_eq!(snapshot_record(&pool).await, sentinel);
    assert_eq!(snapshot_audits(&pool).await, 0);
    let projection = worker.active_projection().await.unwrap();
    assert_eq!(projection.len(), 2);
    assert!(projection.iter().all(|key| key.metadata.user_id == other));
    assert!(worker.active_debug_key(fixture.session).await.is_err());
    assert!(worker.verify_debug_key(debug).await.is_err());
    assert!(worker.debug_key(fixture.session).await.unwrap().is_some());
    let mapping = group_mapping(fixture.team);
    let observed = now_micros() - Duration::minutes(2);
    let initial = IdpSnapshot {
        issuer: ISSUER.into(),
        observed_at: observed,
        subjects: vec![subject(
            "alice",
            true,
            &["members", "owners", "platform-admins"],
        )],
    };
    worker
        .sync_idp_snapshot(ISSUER, &mapping, &initial)
        .await
        .unwrap();
    assert_live_user(
        &pool,
        fixture.user,
        fixture.team,
        true,
        "team_admin",
        Some(observed),
        3,
    )
    .await;
    assert_live_user(&pool, other, fixture.team, false, "member", None, 2).await;
    let initial_record = snapshot_record(&pool).await;
    assert_eq!(initial_record.0, observed);
    assert_eq!(initial_record.1.len(), 32);
    assert_eq!(snapshot_audits(&pool).await, 1);
    worker.enable_idp_sync(ISSUER).await.unwrap();
    assert_eq!(snapshot_record(&pool).await, initial_record);
    assert_eq!(snapshot_audits(&pool).await, 1);

    let projection = worker.active_projection().await.unwrap();
    for id in [debug_id, project_key, personal_key] {
        let key = projection.iter().find(|key| key.metadata.id == id).unwrap();
        assert_eq!(
            key.metadata.expires_at,
            Some(observed + Duration::minutes(10))
        );
    }
    assert_eq!(
        worker
            .active_debug_key(fixture.session)
            .await
            .unwrap()
            .unwrap()
            .expires_at,
        Some(observed + Duration::minutes(10))
    );
    assert_eq!(
        worker
            .debug_key(fixture.session)
            .await
            .unwrap()
            .unwrap()
            .metadata
            .expires_at,
        Some(fixture.expires_at)
    );
    let unmanaged_project_expiry: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM identity_keys WHERE id=$1")
            .bind(other_project)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        projection
            .iter()
            .find(|key| key.metadata.id == other_project)
            .unwrap()
            .metadata
            .expires_at,
        unmanaged_project_expiry
    );
    assert_eq!(
        projection
            .iter()
            .find(|key| key.metadata.id == other_personal)
            .unwrap()
            .metadata
            .expires_at,
        None
    );
    let short_expiry = observed + Duration::minutes(4);
    sqlx::query("UPDATE identity_keys SET expires_at=$2 WHERE id=$1")
        .bind(project_key)
        .bind(short_expiry)
        .execute(&pool)
        .await
        .unwrap();
    let projection = worker.active_projection().await.unwrap();
    assert_eq!(
        projection
            .iter()
            .find(|key| key.metadata.id == project_key)
            .unwrap()
            .metadata
            .expires_at,
        Some(short_expiry)
    );

    worker
        .sync_idp_snapshot(ISSUER, &mapping, &initial)
        .await
        .unwrap();
    assert_eq!(snapshot_record(&pool).await, initial_record);
    assert_eq!(snapshot_audits(&pool).await, 1);
    assert_live_user(
        &pool,
        fixture.user,
        fixture.team,
        true,
        "team_admin",
        Some(observed),
        3,
    )
    .await;

    let mut conflict = initial.clone();
    conflict.subjects[0].active = false;
    let mut duplicate_subject = initial.clone();
    duplicate_subject.observed_at += Duration::seconds(1);
    duplicate_subject
        .subjects
        .push(duplicate_subject.subjects[0].clone());
    let mut duplicate_group = initial.clone();
    duplicate_group.observed_at += Duration::seconds(1);
    duplicate_group.subjects[0].groups.push("members".into());
    let mut old = initial.clone();
    old.observed_at -= Duration::seconds(1);
    let mut stale = initial.clone();
    stale.observed_at = now_micros() - Duration::minutes(11);
    let mut future = initial.clone();
    future.observed_at = now_micros() + Duration::minutes(1);
    let mut wrong_issuer = initial.clone();
    wrong_issuer.issuer = OTHER_ISSUER.into();
    for (label, invalid) in [
        ("equal-time conflict", conflict),
        ("duplicate subject", duplicate_subject),
        ("duplicate group", duplicate_group),
        ("out-of-order snapshot", old),
        ("stale snapshot", stale),
        ("future snapshot", future),
        ("wrong issuer", wrong_issuer),
    ] {
        assert!(
            worker
                .sync_idp_snapshot(ISSUER, &mapping, &invalid)
                .await
                .is_err(),
            "accepted {label}"
        );
        assert_eq!(
            snapshot_record(&pool).await,
            initial_record,
            "changed source on {label}"
        );
        assert_eq!(snapshot_audits(&pool).await, 1);
        assert_live_user(
            &pool,
            fixture.user,
            fixture.team,
            true,
            "team_admin",
            Some(observed),
            3,
        )
        .await;
        assert_eq!(
            worker
                .active_debug_key(fixture.session)
                .await
                .unwrap()
                .unwrap()
                .id,
            debug_id
        );
    }
    let mut changed_mapping = mapping.clone();
    changed_mapping.platform_admin_group = Some("different-admins".into());
    assert!(worker
        .sync_idp_snapshot(ISSUER, &changed_mapping, &initial)
        .await
        .is_err());
    assert_eq!(snapshot_record(&pool).await, initial_record);
    assert_live_user(
        &pool,
        fixture.user,
        fixture.team,
        true,
        "team_admin",
        Some(observed),
        3,
    )
    .await;

    let newer = IdpSnapshot {
        observed_at: observed + Duration::seconds(1),
        subjects: vec![subject("alice", true, &["members"])],
        ..initial.clone()
    };
    worker
        .sync_idp_snapshot(ISSUER, &mapping, &newer)
        .await
        .unwrap();
    let newer_record = snapshot_record(&pool).await;
    assert_eq!(newer_record.0, newer.observed_at);
    assert_eq!(snapshot_audits(&pool).await, 2);
    assert_live_user(
        &pool,
        fixture.user,
        fixture.team,
        false,
        "member",
        Some(newer.observed_at),
        3,
    )
    .await;
    assert!(worker
        .sync_idp_snapshot(ISSUER, &mapping, &initial)
        .await
        .is_err());
    assert_eq!(snapshot_record(&pool).await, newer_record);
    assert_eq!(snapshot_audits(&pool).await, 2);
    assert_live_user(
        &pool,
        fixture.user,
        fixture.team,
        false,
        "member",
        Some(newer.observed_at),
        3,
    )
    .await;

    // Simulate passage of the freshness window without a sleep. Existing
    // records remain available for cleanup but cannot authorize or project.
    sqlx::query("UPDATE identity_idp_projection SET observed_at=clock_timestamp()-interval '11 minutes' WHERE issuer=$1")
        .bind(ISSUER)
        .execute(&pool)
        .await
        .unwrap();
    let projection = worker.active_projection().await.unwrap();
    assert!(!projection
        .iter()
        .any(|key| key.metadata.user_id == fixture.user));
    assert!(projection
        .iter()
        .any(|key| key.metadata.id == other_project));
    assert!(projection
        .iter()
        .any(|key| key.metadata.id == other_personal));
    assert!(worker.active_debug_key(fixture.session).await.is_err());
    assert!(worker.verify_debug_key(debug).await.is_err());
    assert!(worker.register_debug_key(debug).await.is_err());
    assert!(worker
        .debug_key(fixture.session)
        .await
        .unwrap()
        .unwrap()
        .metadata
        .revoked_at
        .is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn inactive_and_missing_idp_subjects_are_offboarded_without_cross_issuer_effects(
    pool: PgPool,
) {
    let fixture = fixture(&pool).await;
    let worker = &fixture.worker;
    let bob = seed_member(&pool, fixture.team, ISSUER, "bob").await;
    let other = seed_member(&pool, fixture.team, OTHER_ISSUER, "alice").await;
    seed_ordinary_keys(&pool, &fixture, fixture.user, 50).await;
    seed_ordinary_keys(&pool, &fixture, bob, 52).await;
    let (other_project, other_personal) = seed_ordinary_keys(&pool, &fixture, other, 54).await;
    let debug_secret = secret(56);
    let debug = fixture.request(&debug_secret);
    worker.register_debug_key(debug).await.unwrap();
    let run_secret = secret(57);
    worker
        .register_run_key(RunKeyRequest {
            run_id: Uuid::new_v4(),
            deployment: "revision-a",
            run_kind: "validation",
            request_budget: None,
            user_id: fixture.user,
            team_id: fixture.team,
            project_id: fixture.project,
            service: MODEL,
            expires_at: fixture.expires_at,
            secret: &run_secret,
            budget: 20,
        })
        .await
        .unwrap();
    let mapping = group_mapping(fixture.team);
    let observed = now_micros() - Duration::minutes(2);
    let active = IdpSnapshot {
        issuer: ISSUER.into(),
        observed_at: observed,
        subjects: vec![
            subject("alice", true, &["members", "platform-admins"]),
            subject("bob", true, &["members"]),
        ],
    };
    worker
        .sync_idp_snapshot(ISSUER, &mapping, &active)
        .await
        .unwrap();
    assert_live_user(
        &pool,
        fixture.user,
        fixture.team,
        true,
        "member",
        Some(observed),
        4,
    )
    .await;
    assert_live_user(&pool, bob, fixture.team, false, "member", Some(observed), 2).await;
    assert_eq!(worker.active_projection().await.unwrap().len(), 8);

    let offboard = IdpSnapshot {
        issuer: ISSUER.into(),
        observed_at: observed + Duration::seconds(1),
        // Alice is explicitly inactive; Bob is absent from this complete snapshot.
        subjects: vec![subject("alice", false, &[])],
    };
    worker
        .sync_idp_snapshot(ISSUER, &mapping, &offboard)
        .await
        .unwrap();
    worker
        .sync_idp_snapshot(ISSUER, &mapping, &offboard)
        .await
        .unwrap();
    assert_eq!(snapshot_audits(&pool).await, 2);
    assert_offboarded(&pool, fixture.user, 4).await;
    assert_offboarded(&pool, bob, 2).await;
    assert_live_user(&pool, other, fixture.team, false, "member", None, 2).await;
    assert!(worker
        .debug_key(fixture.session)
        .await
        .unwrap()
        .unwrap()
        .metadata
        .revoked_at
        .is_some());
    assert_debug_not_projected(worker, fixture.session).await;
    assert!(worker.verify_debug_key(debug).await.is_err());
    let projection = worker.active_projection().await.unwrap();
    assert_eq!(projection.len(), 2);
    assert!(projection
        .iter()
        .any(|key| key.metadata.id == other_project));
    assert!(projection
        .iter()
        .any(|key| key.metadata.id == other_personal));
    assert!(!projection
        .iter()
        .any(|key| key.metadata.user_id == fixture.user || key.metadata.user_id == bob));

    // A later active observation cannot resurrect an offboarded local user.
    let resurrect = IdpSnapshot {
        observed_at: observed + Duration::seconds(2),
        subjects: vec![
            subject("alice", true, &["members", "platform-admins"]),
            subject("bob", true, &["members"]),
        ],
        ..active
    };
    worker
        .sync_idp_snapshot(ISSUER, &mapping, &resurrect)
        .await
        .unwrap();
    assert_offboarded(&pool, fixture.user, 4).await;
    assert_offboarded(&pool, bob, 2).await;
    assert_eq!(worker.active_projection().await.unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_idp_mapping_and_missing_team_roll_back_the_entire_snapshot(pool: PgPool) {
    let fixture = fixture(&pool).await;
    let worker = &fixture.worker;
    let bob = seed_member(&pool, fixture.team, ISSUER, "bob").await;
    seed_ordinary_keys(&pool, &fixture, fixture.user, 60).await;
    seed_ordinary_keys(&pool, &fixture, bob, 62).await;
    let mapping = group_mapping(fixture.team);
    let observed = now_micros() - Duration::minutes(2);
    let baseline = IdpSnapshot {
        issuer: ISSUER.into(),
        observed_at: observed,
        subjects: vec![
            subject("alice", true, &["members"]),
            subject("bob", true, &["members"]),
        ],
    };
    worker
        .sync_idp_snapshot(ISSUER, &mapping, &baseline)
        .await
        .unwrap();
    let original_record = snapshot_record(&pool).await;
    let attempted = IdpSnapshot {
        observed_at: observed + Duration::seconds(1),
        subjects: vec![
            subject("alice", true, &["owners", "platform-admins"]),
            subject("bob", true, &["missing-team"]),
        ],
        ..baseline.clone()
    };
    let mut invalid_mapping = mapping.clone();
    invalid_mapping
        .member_groups
        .insert("invalid".into(), Uuid::nil());
    assert!(worker
        .sync_idp_snapshot(ISSUER, &invalid_mapping, &attempted)
        .await
        .is_err());
    let mut missing_team = mapping.clone();
    missing_team
        .member_groups
        .insert("missing-team".into(), Uuid::new_v4());
    // Alice's promotion is processed first; Bob's FK failure must roll it back.
    assert!(worker
        .sync_idp_snapshot(ISSUER, &missing_team, &attempted)
        .await
        .is_err());
    assert_eq!(snapshot_record(&pool).await, original_record);
    assert_eq!(snapshot_audits(&pool).await, 1);
    assert_live_user(
        &pool,
        fixture.user,
        fixture.team,
        false,
        "member",
        Some(observed),
        2,
    )
    .await;
    assert_live_user(&pool, bob, fixture.team, false, "member", Some(observed), 2).await;
    assert_eq!(worker.active_projection().await.unwrap().len(), 4);

    // The failed observation did not consume its ordering timestamp either.
    let corrected = IdpSnapshot {
        subjects: vec![
            subject("alice", true, &["owners", "platform-admins"]),
            subject("bob", true, &["members"]),
        ],
        ..attempted
    };
    worker
        .sync_idp_snapshot(ISSUER, &mapping, &corrected)
        .await
        .unwrap();
    assert_eq!(snapshot_record(&pool).await.0, corrected.observed_at);
    assert_eq!(snapshot_audits(&pool).await, 2);
    assert_live_user(
        &pool,
        fixture.user,
        fixture.team,
        true,
        "team_admin",
        Some(corrected.observed_at),
        2,
    )
    .await;
    assert_live_user(
        &pool,
        bob,
        fixture.team,
        false,
        "member",
        Some(corrected.observed_at),
        2,
    )
    .await;
    assert_eq!(worker.active_projection().await.unwrap().len(), 4);
}
