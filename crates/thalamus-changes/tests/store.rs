use sqlx::postgres::PgPoolOptions;
use thalamus_changes::{Error, Proposal, Rollout, Store};
use thalamus_gitops::{Operation, Patch};
use uuid::Uuid;

fn proposal() -> Proposal {
    Proposal {
        service: "example".into(),
        base_sha: "a".repeat(40),
        target_branch: "main".into(),
        title: "Update example".into(),
        validation_tier: "expected".into(),
        rollout: Rollout::Rolling,
        patches: vec![Patch {
            file: "catalog/services/example.yaml".into(),
            path: vec!["image".into()],
            operation: Operation::Remove,
        }],
        revert_commit: None,
    }
}

/// Requires a disposable PostgreSQL database supplied explicitly by the test runner.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL pointing to a disposable PostgreSQL database"]
async fn concurrent_retries_share_a_record_and_conflicts_are_rejected() {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is required");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("change_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let selected = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |connection, _| {
            let selected = selected.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {selected}"))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0001_changes.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let store = Store::new(pool.clone());
    let (a, b) = tokio::join!(
        store.propose("alice", "same-key", proposal()),
        store.propose("alice", "same-key", proposal())
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a.state, "proposed");
    assert_eq!(store.list(10).await.unwrap().len(), 1);
    let mut changed = proposal();
    changed.title = "Different".into();
    assert!(matches!(
        store.propose("alice", "same-key", changed).await,
        Err(Error::IdempotencyConflict)
    ));
    let other = store.propose("bob", "same-key", proposal()).await.unwrap();
    assert_ne!(a.id, other.id);
    drop(store);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL pointing to a disposable PostgreSQL database"]
async fn refresh_regenerates_preview_and_requires_revalidation_on_same_change() {
    use std::{fs, path::Path, process::Command};
    use thalamus_changes::PreviewConfig;
    use thalamus_gitops::{GitOpsRepo, RenderCommand};

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
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

    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is required");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("change_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let selected = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |connection, _| {
            let selected = selected.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {selected}"))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0001_changes.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let root = std::env::temp_dir().join(format!("thalamus-refresh-{}", Uuid::new_v4()));
    let source = root.join("source");
    fs::create_dir_all(source.join("catalog/services")).unwrap();
    fs::write(
        source.join("catalog/services/example.yaml"),
        "image: old\nreplicas: 1\n",
    )
    .unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.test"]);
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);
    let base = git(&source, &["rev-parse", "HEAD"]);
    let repo = GitOpsRepo::clone_bare(source.as_os_str(), &root.join("bare.git")).unwrap();
    let config = PreviewConfig {
        worktree_root: root.join("worktrees"),
        render: RenderCommand {
            executable: "cat".into(),
            args: vec!["catalog/services/example.yaml".into()],
        },
    };
    fs::create_dir_all(&config.worktree_root).unwrap();
    let store = Store::new(pool.clone());
    let mut request = proposal();
    request.base_sha = base;
    let original = store
        .propose("alice", "same", request.clone())
        .await
        .unwrap();
    let validated = store
        .validate(original.id, "alice", &repo, &config)
        .await
        .unwrap();
    assert_eq!(validated.state, "validated");
    fs::write(
        source.join("catalog/services/example.yaml"),
        "image: old\nreplicas: 2\n",
    )
    .unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "move target"]);
    let moved = git(&source, &["rev-parse", "HEAD"]);
    let refreshed = store
        .refresh(original.id, "bob", &repo, &config, &moved)
        .await
        .unwrap();
    assert_eq!(refreshed.id, original.id);
    assert_eq!(refreshed.request.base_sha, moved);
    assert_eq!(refreshed.state, "proposed");
    assert_ne!(refreshed.base_render_sha256, validated.base_render_sha256);
    assert!(refreshed
        .data_diff
        .as_deref()
        .unwrap()
        .contains("replicas: 2"));
    assert_eq!(
        store.propose("alice", "same", request).await.unwrap().id,
        original.id
    );
    let events: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM thalamus_change_audit WHERE change_id=$1 ORDER BY id",
    )
    .bind(original.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(events.contains(&("bob".into(), "base_refreshed_review_required".into())));
    let renewed = store
        .validate(original.id, "bob", &repo, &config)
        .await
        .unwrap();
    assert_eq!(renewed.state, "validated");
    assert_eq!(renewed.request.base_sha, moved);
    let validation_actors: Vec<String> = sqlx::query_scalar("SELECT actor FROM thalamus_change_audit WHERE change_id=$1 AND action='validated' ORDER BY id").bind(original.id).fetch_all(&pool).await.unwrap();
    assert_eq!(validation_actors, vec!["alice", "bob"]);
    sqlx::query("UPDATE thalamus_changes SET state='open',pr_number=1,head_sha=$2 WHERE id=$1")
        .bind(original.id)
        .bind("a".repeat(40))
        .execute(&pool)
        .await
        .unwrap();
    fs::write(
        source.join("catalog/services/example.yaml"),
        "image: old\nreplicas: 3\n",
    )
    .unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "move again"]);
    let latest = git(&source, &["rev-parse", "HEAD"]);
    assert!(matches!(
        store
            .refresh(original.id, "bob", &repo, &config, &latest)
            .await,
        Err(Error::Invalid(_))
    ));
    let unchanged = store.get(original.id).await.unwrap();
    assert_eq!(unchanged.state, "open");
    assert_eq!(unchanged.request.base_sha, moved);
    drop(store);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    fs::remove_dir_all(root).unwrap();
}
