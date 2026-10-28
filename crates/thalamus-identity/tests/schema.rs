use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn project_team_cannot_be_forged_and_digest_must_be_sha256(pool: PgPool) {
    let user = Uuid::new_v4();
    let team = Uuid::new_v4();
    let other_team = Uuid::new_v4();
    let project = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO identity_users(id,issuer,subject) VALUES ($1,'https://id.example','subject')",
    )
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
    sqlx::query("INSERT INTO identity_projects(id,team_id,name) VALUES ($1,$2,'project')")
        .bind(project)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    let insert = "INSERT INTO identity_keys(id,digest,user_id,creator_id,team_id,project_id,kind,allowed_services,expires_at) VALUES ($1,$2,$3,$3,$4,$5,'project',ARRAY['service'],now()+interval '1 hour')";
    let wrong_team = sqlx::query(insert)
        .bind(Uuid::new_v4())
        .bind(vec![1u8; 32])
        .bind(user)
        .bind(other_team)
        .bind(project)
        .execute(&pool)
        .await;
    assert!(wrong_team.is_err());
    let wrong_digest = sqlx::query(insert)
        .bind(Uuid::new_v4())
        .bind(vec![1u8; 31])
        .bind(user)
        .bind(team)
        .bind(project)
        .execute(&pool)
        .await;
    assert!(wrong_digest.is_err());
    sqlx::query(insert)
        .bind(Uuid::new_v4())
        .bind(vec![1u8; 32])
        .bind(user)
        .bind(team)
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    let duplicate = sqlx::query(insert)
        .bind(Uuid::new_v4())
        .bind(vec![1u8; 32])
        .bind(user)
        .bind(team)
        .bind(project)
        .execute(&pool)
        .await;
    assert!(duplicate.is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn worker_registration_is_scoped_idempotent_and_revocable(pool: PgPool) {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use chrono::{Duration, Utc};
    use sha2::{Digest, Sha256};
    use thalamus_identity::{Error, RunKeyRequest, WorkerIdentity};

    let (user, team, project, run) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,'issuer','user')")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_projects(id,team_id,name) VALUES ($1,$2,'project')")
        .bind(project)
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
    let worker = WorkerIdentity::new(pool.clone());
    worker
        .sync_budget_caps(
            &"a".repeat(40),
            &[thalamus_identity::BudgetCap {
                team_id: team,
                max_tokens: 100,
                unit: "tokens".into(),
                window: "day".into(),
            }],
        )
        .await
        .unwrap();
    let secret = format!("thl_{}", URL_SAFE_NO_PAD.encode([7u8; 32]));
    let expires = Utc::now() + Duration::hours(1);
    let request = RunKeyRequest {
        run_id: run,
        user_id: user,
        team_id: team,
        project_id: project,
        service: "model",
        expires_at: expires,
        secret: &secret,
        budget: 10,
    };
    let first = worker.register_run_key(request).await.unwrap();
    let retry = worker.register_run_key(request).await.unwrap();
    assert_eq!(first.id, retry.id);
    assert!(worker
        .register_run_key(RunKeyRequest {
            service: "other",
            ..request
        })
        .await
        .is_err());
    let projection = worker.active_projection().await.unwrap();
    assert_eq!(projection.len(), 1);
    assert_eq!(
        projection[0].digest,
        Sha256::digest(secret.as_bytes()).to_vec()
    );
    assert_eq!(projection[0].metadata.allowed_services, vec!["model"]);
    let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM identity_audit WHERE actor_id=$1 AND agent='thalamus-worker' AND details->>'run_id'=$2").bind(user).bind(run.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(audit_count, 1);
    worker.revoke_run_key(run).await.unwrap();
    worker.revoke_run_key(run).await.unwrap();
    assert!(worker.active_projection().await.unwrap().is_empty());
    assert!(worker.register_run_key(request).await.is_err());
    sqlx::query("DELETE FROM identity_memberships WHERE user_id=$1 AND team_id=$2")
        .bind(user)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        worker
            .register_run_key(RunKeyRequest {
                run_id: Uuid::new_v4(),
                ..request
            })
            .await,
        Err(Error::Forbidden)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn worker_rejects_unbounded_or_cross_team_keys(pool: PgPool) {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use chrono::{Duration, Utc};
    use thalamus_identity::{RunKeyRequest, WorkerIdentity};
    let (user, team, other, project) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO identity_users(id,issuer,subject) VALUES ($1,'issuer','user')")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'one'),($2,'two')")
        .bind(team)
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_projects(id,team_id,name) VALUES ($1,$2,'project')")
        .bind(project)
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'member')")
        .bind(user)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    let worker = WorkerIdentity::new(pool);
    let secret = format!("thl_{}", URL_SAFE_NO_PAD.encode([9u8; 32]));
    assert!(worker
        .register_run_key(RunKeyRequest {
            run_id: Uuid::new_v4(),
            user_id: user,
            team_id: team,
            project_id: project,
            service: "model",
            expires_at: Utc::now() + Duration::hours(1),
            secret: &secret,
            budget: 10,
        })
        .await
        .is_err());
    assert!(worker
        .register_run_key(RunKeyRequest {
            run_id: Uuid::new_v4(),
            user_id: user,
            team_id: team,
            project_id: project,
            service: "model",
            expires_at: Utc::now() + Duration::hours(25),
            secret: &secret,
            budget: 10,
        })
        .await
        .is_err());
    assert!(worker
        .register_run_key(RunKeyRequest {
            run_id: Uuid::new_v4(),
            user_id: user,
            team_id: team,
            project_id: project,
            service: "model",
            expires_at: Utc::now() + Duration::hours(1),
            secret: "bad",
            budget: 10,
        })
        .await
        .is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn budget_projection_preserves_windows_and_rolls_back_failed_refresh(pool: PgPool) {
    use thalamus_identity::{BudgetCap, WorkerIdentity};

    let team = Uuid::new_v4();
    sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'team')")
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    let worker = WorkerIdentity::new(pool.clone());
    let cap = BudgetCap {
        team_id: team,
        max_tokens: 100,
        unit: "tokens".into(),
        window: "day".into(),
    };
    let first_commit = "a".repeat(40);
    worker
        .sync_budget_caps(&first_commit, std::slice::from_ref(&cap))
        .await
        .unwrap();

    let stored: (i64, String, String) = sqlx::query_as(
        "SELECT max_tokens,unit,budget_window FROM identity_budget_caps WHERE team_id=$1",
    )
    .bind(team)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, (100, "tokens".into(), "day".into()));

    let mut month = cap.clone();
    month.window = "month".into();
    worker
        .sync_budget_caps(&first_commit, &[month])
        .await
        .unwrap();
    let stored_window: String =
        sqlx::query_scalar("SELECT budget_window FROM identity_budget_caps WHERE team_id=$1")
            .bind(team)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_window, "month");

    let invalid_window =
        sqlx::query("UPDATE identity_budget_caps SET budget_window='week' WHERE team_id=$1")
            .bind(team)
            .execute(&pool)
            .await;
    assert!(invalid_window.is_err());

    let missing_team = BudgetCap {
        team_id: Uuid::new_v4(),
        ..cap
    };
    assert!(worker
        .sync_budget_caps(&"b".repeat(40), &[missing_team])
        .await
        .is_err());
    let source: String = sqlx::query_scalar(
        "SELECT source_commit FROM identity_budget_projection WHERE singleton=true",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(source, first_commit);
    let stored: (i64, String) = sqlx::query_as(
        "SELECT max_tokens,budget_window FROM identity_budget_caps WHERE team_id=$1",
    )
    .bind(team)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, (100, "month".into()));
}
