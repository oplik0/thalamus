use sqlx::postgres::PgPoolOptions;
use thalamus_changes::{Error, Proposal, Rollout, Store};
use thalamus_gitops::{Operation, Patch};
use uuid::Uuid;

struct FollowForge(std::sync::Mutex<forge::ChangeRequest>);

#[async_trait::async_trait]
impl forge::ChangeRequests for FollowForge {
    async fn get_change(&self, number: u64) -> forge::Result<forge::ChangeRequest> {
        let pr = self.0.lock().unwrap().clone();
        if pr.number != number {
            return Err(forge::Error::InvalidInput("unexpected PR number".into()));
        }
        Ok(pr)
    }
    fn list_changes(&self) -> forge::PageStream<'_, forge::ChangeRequest> {
        Box::pin(futures_util::stream::empty())
    }
    async fn create_change(
        &self,
        _: &forge::CreateChangeRequest,
    ) -> forge::Result<forge::ChangeRequest> {
        Err(forge::Error::Unsupported("fixture is read-only".into()))
    }
    async fn update_change(
        &self,
        _: u64,
        _: &forge::UpdateChangeRequest,
    ) -> forge::Result<forge::ChangeRequest> {
        Err(forge::Error::Unsupported("fixture is read-only".into()))
    }
    async fn merge_change(&self, _: u64, _: &str, _: &str, _: &str) -> forge::Result<String> {
        Err(forge::Error::Unsupported("fixture is read-only".into()))
    }
}

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
    sqlx::raw_sql(include_str!("../migrations/0002_rollouts.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let legacy_id = Uuid::new_v4();
    let legacy_observation = chrono::DateTime::parse_from_rfc3339("2026-10-28T07:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    sqlx::query("INSERT INTO thalamus_changes(id,actor,idempotency_key,fingerprint,request,state,branch) VALUES($1,'legacy','legacy','legacy',$2,'merged','legacy')")
        .bind(legacy_id).bind(serde_json::to_value(proposal()).unwrap()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO thalamus_rollouts(change_id,stage,merge_completed_at) VALUES($1,'scheduled_pr',$2)")
        .bind(legacy_id).bind(legacy_observation).execute(&pool).await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0003_merge_timestamps.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let (observed, upstream): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT merge_observed_at,upstream_merged_at FROM thalamus_rollouts WHERE change_id=$1",
    )
    .bind(legacy_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(observed, Some(legacy_observation));
    assert_eq!(upstream, None);
    sqlx::query("DELETE FROM thalamus_rollouts WHERE change_id=$1")
        .bind(legacy_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM thalamus_changes WHERE id=$1")
        .bind(legacy_id)
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
    sqlx::raw_sql(include_str!("../migrations/0002_rollouts.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0003_merge_timestamps.sql"))
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
    let bare = root.join("bare.git");
    let unbound = GitOpsRepo::clone_bare(source.as_os_str(), &bare).unwrap();
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let ssh = root.join("ssh-transport");
    fs::write(&ssh, format!(
        "#!/bin/sh\nunset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX GIT_COMMON_DIR\ncase \"$*\" in\n *git-upload-pack*) exec git upload-pack {} ;;\n *git-receive-pack*) exec git receive-pack {} ;;\n *) exit 2 ;;\nesac\n",
        quote(source.join(".git").to_str().unwrap()), quote(source.join(".git").to_str().unwrap())
    )).unwrap();
    git(
        &bare,
        &[
            "config",
            "core.sshCommand",
            &format!("sh {}", quote(ssh.to_str().unwrap())),
        ],
    );
    git(&bare, &["config", "ssh.variant", "ssh"]);
    let origin = "ssh://git@example.invalid/team/repository.git";
    git(&bare, &["remote", "set-url", "origin", origin]);
    let repo = unbound.bind_origin(origin, "main").unwrap();
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
    fs::write(
        source.join("catalog/services/example.yaml"),
        "image: changed-by-someone-else\nreplicas: 2\n",
    )
    .unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "conflicting image"]);
    let conflicting = git(&source, &["rev-parse", "HEAD"]);
    assert!(matches!(
        store
            .refresh(original.id, "bob", &repo, &config, &conflicting)
            .await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        store.get(original.id).await.unwrap().request.base_sha,
        moved
    );
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

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL pointing to a disposable PostgreSQL database"]
async fn owner_approval_and_rollout_run_bindings_are_durable_and_revision_scoped() {
    use chrono::{Duration, Utc};
    use thalamus_changes::{
        InPlaceSafetyRequirement, RequiredCheckPin, RolloutMode, RolloutPolicy,
        ValidationRequirement, ValidationStage,
    };

    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is required");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("rollout_test_{}", Uuid::new_v4().simple());
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
    sqlx::raw_sql(include_str!(
        "../../thalamus-identity/migrations/0001_identity.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../../thalamus-runs/migrations/0001_runs.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0001_changes.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0002_rollouts.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0003_merge_timestamps.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let owner = Uuid::new_v4();
    let outsider = Uuid::new_v4();
    let team = Uuid::new_v4();
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO identity_users(id,issuer,subject,platform_admin) VALUES ($1,'issuer','owner',false),($2,'issuer','admin',true)")
        .bind(owner).bind(outsider).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO identity_teams(id,name) VALUES ($1,'owners')")
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO identity_memberships(user_id,team_id,role) VALUES ($1,$2,'team_admin')",
    )
    .bind(owner)
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
    let policy = RolloutPolicy {
        service: "example".into(),
        repository: forge::RepoRef::parse(
            forge::ForgeKind::Github,
            "https://github.example/team/catalog",
        )
        .unwrap(),
        target_branch: "main".into(),
        owner_team: team,
        validation_tier: "expected".into(),
        mode: RolloutMode::InPlace,
        required_checks: vec![RequiredCheckPin {
            context: "ci".into(),
            app_id: 42,
            status_creator_id: None,
        }],
        required_approvers: vec!["owner".into()],
        trust_administrators: true,
        trust_merge_group_ci: false,
        merge_margin_seconds: 50,
        request_timeout_seconds: 10,
    };
    let store = Store::new(pool.clone());
    let now = Utc::now();
    let start = now - Duration::minutes(1);
    let end = now + Duration::hours(1);
    let mut request = proposal();
    request.rollout = Rollout::InPlace {
        window_start: start,
        window_end: end,
    };
    let change = store
        .propose(&owner.to_string(), "approval", request)
        .await
        .unwrap();
    let head = "b".repeat(40);
    let base = "a".repeat(40);
    sqlx::query("UPDATE thalamus_changes SET state='open',pr_number=7,head_sha=$2 WHERE id=$1")
        .bind(change.id)
        .bind(&head)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE thalamus_rollouts SET prepared=true,in_place_pool=true,owner_team=$2,policy_digest=$3,old_deployment='old',previous_revision=$4,workload_namespace='ns' WHERE change_id=$1")
        .bind(change.id).bind(team).bind(policy.digest().unwrap()).bind("e".repeat(64)).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        store
            .approve_in_transaction(&mut tx, change.id, outsider, &policy)
            .await,
        Err(Error::Invalid(_))
    ));
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let approved = store
        .approve_in_transaction(&mut tx, change.id, owner, &policy)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(approved.head_sha, head);
    let safety = InPlaceSafetyRequirement {
        change_id: change.id,
        owner_team: team,
        service: "example".into(),
        deployment: "old".into(),
        namespace: "ns".into(),
        base_commit: base.clone(),
        revision: "e".repeat(64),
    };
    assert!(store.authorize_in_place_safety(&safety).await.unwrap());
    let mut wrong = safety.clone();
    wrong.revision = "f".repeat(64);
    assert!(!store.authorize_in_place_safety(&wrong).await.unwrap());
    assert!(!store
        .approval_current(
            change.id,
            &policy,
            &head,
            &base,
            start - Duration::seconds(1)
        )
        .await
        .unwrap());
    assert!(store
        .approval_current(change.id, &policy, &head, &base, start)
        .await
        .unwrap());
    assert!(!store
        .approval_current(change.id, &policy, &head, &base, end)
        .await
        .unwrap());
    assert!(!store
        .approval_current(change.id, &policy, &"c".repeat(40), &base, start)
        .await
        .unwrap());
    assert!(!store
        .approval_current(change.id, &policy, &head, &"c".repeat(40), start)
        .await
        .unwrap());
    let mut changed_policy = policy.clone();
    changed_policy.required_checks[0].app_id = 99;
    assert!(!store
        .approval_current(change.id, &changed_policy, &head, &base, start)
        .await
        .unwrap());
    sqlx::query("UPDATE identity_users SET disabled=true WHERE id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!store
        .approval_current(change.id, &policy, &head, &base, start)
        .await
        .unwrap());
    sqlx::query("UPDATE identity_users SET disabled=false WHERE id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE identity_memberships SET role='member' WHERE user_id=$1 AND team_id=$2")
        .bind(owner)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!store
        .approval_current(change.id, &policy, &head, &base, start)
        .await
        .unwrap());
    sqlx::query(
        "UPDATE identity_memberships SET role='team_admin' WHERE user_id=$1 AND team_id=$2",
    )
    .bind(owner)
    .bind(team)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE thalamus_rollouts SET merge_observed_at=$2,upstream_merged_at=$2 WHERE change_id=$1")
        .bind(change.id).bind(now).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store
        .schedule_in_transaction(
            &mut tx,
            change.id,
            owner,
            start + Duration::minutes(1),
            end,
            &policy,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(store.rollout(change.id).await.unwrap().approvals.is_empty());
    let rescheduled = store.rollout(change.id).await.unwrap();
    assert!(rescheduled.merge_observed_at.is_none());
    assert!(rescheduled.upstream_merged_at.is_none());

    // A successful direct response gives a local observation, not an upstream
    // timestamp. A later forge read can supply that independently.
    let observed_at = Utc::now();
    sqlx::query("UPDATE thalamus_rollouts SET merge_observed_at=$2 WHERE change_id=$1")
        .bind(change.id)
        .bind(observed_at)
        .execute(&pool)
        .await
        .unwrap();
    let forge = FollowForge(std::sync::Mutex::new(forge::ChangeRequest {
        number: 7,
        title: "fixture".into(),
        body: None,
        source_branch: change.branch.clone(),
        target_branch: "main".into(),
        head_sha: head.clone(),
        state: forge::ChangeState::Merged,
        draft: false,
        mergeability: forge::Mergeability::Unknown,
        base_sha: base.clone(),
        merge_commit: Some("c".repeat(40)),
        merged_at: None,
        source_repository: Some(policy.repository.clone()),
        author: "bot".into(),
        labels: Vec::new(),
        url: "https://github.example/team/catalog/pull/7".into(),
    }));
    {
        let mut pr = forge.0.lock().unwrap();
        pr.head_sha = "f".repeat(40);
        pr.merged_at = Some("2026-10-28T09:00:00Z".into());
    }
    assert!(matches!(
        store
            .follow(change.id, &forge, &policy.repository, "main")
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(store
        .rollout(change.id)
        .await
        .unwrap()
        .upstream_merged_at
        .is_none());
    {
        let mut pr = forge.0.lock().unwrap();
        pr.head_sha = head.clone();
        pr.merged_at = None;
    }
    store
        .follow(change.id, &forge, &policy.repository, "main")
        .await
        .unwrap();
    let first = store.rollout(change.id).await.unwrap();
    assert_eq!(
        first.merge_observed_at.unwrap().timestamp_micros(),
        observed_at.timestamp_micros()
    );
    assert_eq!(first.upstream_merged_at, None);
    let actual = end + Duration::minutes(1);
    forge.0.lock().unwrap().merged_at = Some(actual.to_rfc3339());
    store
        .follow(change.id, &forge, &policy.repository, "main")
        .await
        .unwrap();
    let second = store.rollout(change.id).await.unwrap();
    assert_eq!(
        second.merge_observed_at.unwrap().timestamp_micros(),
        observed_at.timestamp_micros()
    );
    assert_eq!(
        second.upstream_merged_at.unwrap().timestamp_micros(),
        actual.timestamp_micros()
    );
    assert!(second.upstream_merged_at.unwrap() > second.window_end.unwrap());
    forge.0.lock().unwrap().merged_at = Some("not-an-rfc3339-time".into());
    assert!(matches!(
        store
            .follow(change.id, &forge, &policy.repository, "main")
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        store
            .rollout(change.id)
            .await
            .unwrap()
            .upstream_merged_at
            .unwrap()
            .timestamp_micros(),
        actual.timestamp_micros()
    );

    let mut other_proposal = proposal();
    other_proposal.rollout = Rollout::InPlace {
        window_start: start,
        window_end: end,
    };
    let other = store
        .propose(&owner.to_string(), "non-github", other_proposal)
        .await
        .unwrap();
    sqlx::query("UPDATE thalamus_changes SET state='open',pr_number=8,head_sha=$2 WHERE id=$1")
        .bind(other.id)
        .bind(&head)
        .execute(&pool)
        .await
        .unwrap();
    let other_repo = forge::RepoRef::parse(
        forge::ForgeKind::Gitlab,
        "https://gitlab.example/team/catalog",
    )
    .unwrap();
    {
        let mut pr = forge.0.lock().unwrap();
        pr.number = 8;
        pr.source_branch = other.branch.clone();
        pr.source_repository = Some(other_repo.clone());
        pr.merged_at = Some(actual.to_rfc3339());
    }
    store
        .follow(other.id, &forge, &other_repo, "main")
        .await
        .unwrap();
    let other_status = store.rollout(other.id).await.unwrap();
    assert!(other_status.merge_observed_at.is_some());
    assert!(other_status.upstream_merged_at.is_none());

    let mut gated = proposal();
    gated.rollout = Rollout::Gated;
    let candidate = store
        .propose(&owner.to_string(), "candidate", gated)
        .await
        .unwrap();
    let commit = "d".repeat(40);
    let revision = "e".repeat(64);
    let deployment = "candidate-123";
    sqlx::query("UPDATE thalamus_rollouts SET stage='candidate_validation',owner_team=$2,policy_digest='fixture',candidate_deployment=$3,candidate_revision=$4,old_deployment='old' WHERE change_id=$1")
        .bind(candidate.id).bind(team).bind(deployment).bind(&revision).execute(&pool).await.unwrap();
    sqlx::query("UPDATE thalamus_rollout_stages SET merged_commit=$2 WHERE change_id=$1 AND stage='candidate_pr'")
        .bind(candidate.id).bind(&commit).execute(&pool).await.unwrap();
    let requirement = ValidationRequirement {
        change_id: candidate.id,
        owner_team: team,
        actor: owner.to_string(),
        stage: ValidationStage::Candidate,
        service: "example".into(),
        deployment: deployment.into(),
        revision,
        catalog_commit: commit.clone(),
        candidate_run_id: None,
        candidate_catalog_commit: None,
    };
    let authorized = store
        .authorize_validation(&requirement, project)
        .await
        .unwrap();
    assert_eq!(authorized.route.as_deref(), Some("candidate-123-candidate"));
    let run = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO thalamus_runs(id,team,actor,idempotency_key,fingerprint,request,job_name,state,ownership_nonce) VALUES ($1,$2,$3,$4,'fixture',$5,$6,'pending',$7)")
        .bind(run).bind(team.to_string()).bind(owner.to_string())
        .bind(format!("rollout:{}:candidate:{}",candidate.id,commit))
        .bind(serde_json::json!({"project_id":project,"kind":"validation","service":"example","deployment":deployment}))
        .bind(format!("job-{run}")).bind(Uuid::new_v4()).execute(&mut *tx).await.unwrap();
    store
        .bind_validation_run_in_transaction(&mut tx, &requirement, project, run)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        store
            .authorize_validation_run(run)
            .await
            .unwrap()
            .unwrap()
            .active
    );
    let admission = store
        .authorize_active_candidate("example", deployment, team, project)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(admission.catalog_commit, commit);
    assert!(store
        .authorize_active_candidate("example", deployment, team, Uuid::new_v4())
        .await
        .unwrap()
        .is_none());
    let manual_run = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO thalamus_runs(id,team,actor,idempotency_key,fingerprint,request,job_name,state,ownership_nonce) VALUES ($1,$2,$3,'manual-benchmark','fixture',$4,$5,'pending',$6)")
        .bind(manual_run).bind(team.to_string()).bind(owner.to_string())
        .bind(serde_json::json!({"project_id":project,"kind":"benchmark","service":"example","deployment":deployment}))
        .bind(format!("job-{manual_run}")).bind(Uuid::new_v4()).execute(&mut *tx).await.unwrap();
    let bound = store
        .bind_candidate_run_in_transaction(&mut tx, manual_run, &admission)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(bound.route, "candidate-123-benchmark");
    assert!(
        store
            .authorize_candidate_run(manual_run)
            .await
            .unwrap()
            .unwrap()
            .active
    );
    sqlx::query("UPDATE thalamus_rollouts SET stage='promotion_pr' WHERE change_id=$1")
        .bind(candidate.id)
        .execute(&pool)
        .await
        .unwrap();
    let historical = store.authorize_validation_run(run).await.unwrap().unwrap();
    assert!(!historical.active);
    assert_eq!(historical.old_deployment.as_deref(), Some("old"));
    assert!(store
        .authorize_validation(&requirement, project)
        .await
        .is_err());
    sqlx::query("UPDATE thalamus_rollouts SET stage='completed' WHERE change_id=$1")
        .bind(candidate.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !store
            .authorize_candidate_run(manual_run)
            .await
            .unwrap()
            .unwrap()
            .active
    );
    drop(store);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
}
