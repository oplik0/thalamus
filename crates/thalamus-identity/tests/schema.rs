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
