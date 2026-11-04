use super::*;
use tempfile::TempDir;

const FORGE_REPOSITORY: &str = "ssh://git@example.invalid/team/repository.git";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn git_test(dir: &Path, arguments: &[&str]) -> String {
    git(dir, &args(arguments)).unwrap()
}

fn fixture() -> (TempDir, GitOpsRepo, String) {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("catalog")).unwrap();
    fs::write(
        source.join("catalog/service.yaml"),
        "service:\n  image: old\n  replicas: 1\n",
    )
    .unwrap();
    fs::write(source.join("template.k"), "value = 1\n").unwrap();
    git_test(&source, &["init"]);
    git_test(&source, &["config", "user.name", "Fixture"]);
    git_test(&source, &["config", "user.email", "fixture@example.test"]);
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "fixture base"]);
    let base = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    let unbound =
        GitOpsRepo::clone_bare(source.as_os_str(), &temp.path().join("bare.git")).unwrap();
    // Exercise real Git transport and refs without connecting to an external
    // forge. The configured SSH identity still goes through origin binding.
    let ssh = temp.path().join("ssh-transport");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX GIT_COMMON_DIR\n\
             case \"$*\" in\n\
               *git-upload-pack*) exec git upload-pack {} ;;\n\
               *git-receive-pack*) exec git receive-pack {} ;;\n\
               *) exit 2 ;;\n\
             esac\n",
            shell_quote(source.join(".git").to_str().unwrap()),
            shell_quote(source.join(".git").to_str().unwrap()),
        ),
    )
    .unwrap();
    let command = format!("sh {}", shell_quote(ssh.to_str().unwrap()));
    git_test(&unbound.bare, &["config", "core.sshCommand", &command]);
    git_test(&unbound.bare, &["config", "ssh.variant", "ssh"]);
    git_test(
        &unbound.bare,
        &["remote", "set-url", "origin", FORGE_REPOSITORY],
    );
    let branch = git_test(&source, &["branch", "--show-current"]);
    let repo = unbound
        .bind_origin(FORGE_REPOSITORY, branch.trim())
        .unwrap();
    (temp, repo, base)
}

fn patch(file: &str, value: &str) -> Patch {
    Patch {
        file: file.into(),
        path: vec!["service".into(), "image".into()],
        operation: Operation::Set(Value::String(value.into())),
    }
}

fn render() -> RenderCommand {
    RenderCommand {
        executable: "cat".into(),
        args: vec!["catalog/service.yaml".into()],
    }
}

#[test]
fn moved_base_regenerates_preview_and_updates_published_branch() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    let base_branch = git_test(&source, &["branch", "--show-current"]);
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    let change_id = "moved-base-change";
    let message = "update image";
    assert_eq!(repo.resolve_base(base_branch.trim()).unwrap(), base);

    let mut first = repo
        .create_worktree(&base, &temp.path().join("first"))
        .unwrap();
    let original = first
        .preview(&[patch("catalog/service.yaml", "desired")], &render())
        .unwrap();
    assert!(original.base_render.contains("image: old"));
    assert!(original.proposed_render.contains("image: desired"));
    let published = first.commit_and_push(change_id, message, &author).unwrap();

    // A worktree created before fetching continues to use its pinned base.
    let mut pinned = repo
        .create_worktree(&base, &temp.path().join("pinned"))
        .unwrap();
    fs::write(
        source.join("catalog/service.yaml"),
        "service:\n  image: upstream\n  replicas: 2\n  region: west\n",
    )
    .unwrap();
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "move base"]);
    repo.fetch().unwrap();
    let moved = repo.resolve_base(base_branch.trim()).unwrap();
    assert_ne!(base, moved);
    let still_old = pinned
        .preview(&[patch("catalog/service.yaml", "desired")], &render())
        .unwrap();
    assert_eq!(pinned.base(), base);
    assert!(still_old.base_render.contains("image: old"));
    assert!(still_old.proposed_render.contains("replicas: 1"));

    let mut fresh = repo
        .create_worktree(&moved, &temp.path().join("fresh"))
        .unwrap();
    let regenerated = fresh
        .preview(&[patch("catalog/service.yaml", "desired")], &render())
        .unwrap();
    assert_eq!(fresh.base(), moved);
    assert!(regenerated.base_render.contains("image: upstream"));
    assert!(regenerated.proposed_render.contains("image: desired"));
    assert!(regenerated.proposed_render.contains("replicas: 2"));
    assert!(regenerated.proposed_render.contains("region: west"));
    assert!(regenerated.data_diff.contains("-  image: upstream"));
    assert!(regenerated.data_diff.contains("+  image: desired"));
    assert!(regenerated.rendered_diff.contains("-  image: upstream"));
    assert!(regenerated.rendered_diff.contains("+  image: desired"));

    // The initial publication path must not overwrite an existing branch.
    assert!(matches!(
        fresh.commit_and_push(change_id, message, &author),
        Err(Error::Invalid(_))
    ));
    let branch_ref = format!("refs/heads/{}", published.branch);
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        published.commit
    );

    let updated = fresh
        .commit_and_push_update(change_id, message, &author, &published.commit)
        .unwrap();
    assert_eq!(updated.branch, published.branch);
    assert_ne!(updated.commit, published.commit);
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        updated.commit
    );
    assert_eq!(
        git_test(&source, &["show", "-s", "--format=%P", &updated.commit]).trim(),
        moved
    );
    let contents = git_test(
        &source,
        &["show", &format!("{}:catalog/service.yaml", updated.commit)],
    );
    assert!(contents.contains("image: desired"));
    assert!(contents.contains("replicas: 2"));
    assert!(contents.contains("region: west"));

    // A push that succeeded before its caller saved the result is retryable
    // without another push, even if the receive service is unavailable.
    fs::write(
        temp.path().join("ssh-transport"),
        format!(
            "#!/bin/sh\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX GIT_COMMON_DIR\n\
             case \"$*\" in\n\
               *git-upload-pack*) exec git upload-pack {} ;;\n\
               *git-receive-pack*) exit 1 ;;\n\
               *) exit 2 ;;\n\
             esac\n",
            shell_quote(source.join(".git").to_str().unwrap()),
        ),
    )
    .unwrap();
    let retried = fresh
        .commit_and_push_update(change_id, message, &author, &published.commit)
        .unwrap();
    assert_eq!(retried.commit, updated.commit);
    assert_eq!(retried.branch, updated.branch);
    repo.discard_worktree(first).unwrap();
    repo.discard_worktree(pinned).unwrap();
    repo.discard_worktree(fresh).unwrap();
}

#[test]
fn stale_lease_cannot_overwrite_or_create_a_branch() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    let change_id = "lease-change";
    let message = "update image";
    let mut first = repo
        .create_worktree(&base, &temp.path().join("first"))
        .unwrap();
    first
        .preview(&[patch("catalog/service.yaml", "first")], &render())
        .unwrap();
    let published = first.commit_and_push(change_id, message, &author).unwrap();
    let branch_ref = format!("refs/heads/{}", published.branch);

    let tree = git_test(
        &source,
        &["rev-parse", &format!("{}^{{tree}}", published.commit)],
    );
    let competing = git_test(
        &source,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            &published.commit,
            "-m",
            "concurrent update",
        ],
    )
    .trim()
    .to_owned();
    git_test(
        &source,
        &["update-ref", &branch_ref, &competing, &published.commit],
    );
    git_test(&source, &["branch", "external-branch", &base]);

    let mut replacement = repo
        .create_worktree(&base, &temp.path().join("replacement"))
        .unwrap();
    replacement
        .preview(&[patch("catalog/service.yaml", "replacement")], &render())
        .unwrap();
    assert!(matches!(
        replacement.commit_and_push_update(change_id, message, &author, &published.commit),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        competing
    );

    // Reject malformed IDs and zero (which Git treats as an absent-branch
    // lease), rather than allowing them to bypass the existing-branch rule.
    let short = &base[..base.len() - 1];
    for invalid in [
        String::new(),
        short.to_owned(),
        "0".repeat(base.len()),
        "g".repeat(base.len()),
        "a".repeat(if base.len() == 40 { 64 } else { 40 }),
    ] {
        assert!(
            matches!(
                replacement.commit_and_push_update(change_id, message, &author, &invalid),
                Err(Error::Invalid(_))
            ),
            "{invalid}"
        );
    }
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        competing
    );

    // A caller-supplied name is always hashed into the Thalamus namespace;
    // it can neither address another branch nor create an absent branch.
    assert!(matches!(
        replacement.commit_and_push_update("external-branch", message, &author, &base),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        git_test(&source, &["rev-parse", "refs/heads/external-branch"]).trim(),
        base
    );
    let missing_ref = format!(
        "refs/heads/{}",
        branch_for_change("external-branch").unwrap()
    );
    assert!(git(&source, &args(&["rev-parse", "--verify", &missing_ref])).is_err());

    repo.fetch().unwrap();
    let resumed = repo
        .resume_committed_worktree(&base, replacement.path())
        .unwrap();
    let updated = resumed
        .commit_and_push_update(change_id, message, &author, &competing)
        .unwrap();
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        updated.commit
    );
    git_test(&source, &["update-ref", "-d", &branch_ref, &updated.commit]);
    assert!(matches!(
        resumed.commit_and_push_update(change_id, message, &author, &updated.commit),
        Err(Error::Invalid(_))
    ));
    assert!(git(&source, &args(&["rev-parse", "--verify", &branch_ref])).is_err());
}

#[test]
fn origin_binding_rejects_changed_and_multiple_destinations() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    let mut first = repo
        .create_worktree(&base, &temp.path().join("first"))
        .unwrap();
    first
        .preview(&[patch("catalog/service.yaml", "first")], &render())
        .unwrap();
    let published = first
        .commit_and_push("push-url", "update image", &author)
        .unwrap();
    let branch_ref = format!("refs/heads/{}", published.branch);
    let mut replacement = repo
        .create_worktree(&base, &temp.path().join("replacement"))
        .unwrap();
    replacement
        .preview(&[patch("catalog/service.yaml", "replacement")], &render())
        .unwrap();

    // An otherwise reachable push host must not be substituted for the
    // configured forge, even when the fetch URL is still correct.
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "ssh://git@evil.invalid/team/repository.git",
        ],
    );
    assert!(matches!(repo.fetch(), Err(Error::Invalid(_))));
    assert!(matches!(
        first.commit_and_push("push-url", "update image", &author),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        replacement.commit_and_push("new-change", "update image", &author),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        replacement.commit_and_push_update("push-url", "update image", &author, &published.commit),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        git_test(replacement.path(), &["rev-parse", "HEAD"]).trim(),
        base
    );

    // Equivalent SCP syntax denotes the same identity, but even two
    // equivalent push URLs are forbidden because Git can partially push.
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "git@example.invalid:team/repository",
        ],
    );
    repo.fetch().unwrap();
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "--add",
            "--push",
            "origin",
            FORGE_REPOSITORY,
        ],
    );
    assert!(matches!(repo.fetch(), Err(Error::Invalid(_))));
    assert!(matches!(
        replacement.commit_and_push("new-change", "update image", &author),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        replacement.commit_and_push_update("push-url", "update image", &author, &published.commit),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        published.commit
    );

    // A malicious fetch origin cannot be hidden behind a trusted push URL.
    git_test(
        &repo.bare,
        &["config", "--unset-all", "remote.origin.pushurl"],
    );
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://git@example.invalid.evil/team/repository.git",
        ],
    );
    git_test(
        &repo.bare,
        &["remote", "set-url", "--push", "origin", FORGE_REPOSITORY],
    );
    assert!(matches!(repo.fetch(), Err(Error::Invalid(_))));
    assert!(matches!(
        replacement.commit_and_push_update("push-url", "update image", &author, &published.commit),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        published.commit
    );

    // Every configured fetch URL is checked even though Git uses only the
    // first one, and a sole valid push URL cannot hide an unexpected second.
    git_test(
        &repo.bare,
        &["remote", "set-url", "origin", FORGE_REPOSITORY],
    );
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "--add",
            "origin",
            "ssh://git@evil.invalid/team/repository.git",
        ],
    );
    assert!(matches!(repo.fetch(), Err(Error::Invalid(_))));
    assert!(matches!(
        first.commit_and_push("push-url", "update image", &author),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn configured_repository_identity_is_strict_and_branch_is_bound() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    let branch = git_test(&source, &["branch", "--show-current"]);
    let unbound = GitOpsRepo::open(&repo.bare).unwrap();
    assert!(
        unbound
            .bind_origin("https://EXAMPLE.invalid/team/repository", branch.trim())
            .is_ok()
    );
    for invalid in [
        "https://user@example.invalid/team/repository",
        "ssh://git:password@example.invalid/team/repository",
        "https://example.invalid/team/../team/repository",
        "https://example.invalid/team/%72epository",
        "https://example.invalid/team/repository?x=1",
        "https://example.invalid/team/repository#x",
        "file:///team/repository",
        "https://example.invalid/team//repository",
        "https://example.invalid/team/repository/",
        "https://example.invalid/team/repository.git/extra",
        "https://example.invalid.evil/team/repository",
        "https://example.invalid/other/repository",
        "https://example.invalid:8443/team/repository",
    ] {
        assert!(
            unbound.bind_origin(invalid, branch.trim()).is_err(),
            "{invalid}"
        );
    }
    assert!(repo.resolve_base("another-branch").is_err());
    assert_eq!(repo.resolve_base(branch.trim()).unwrap(), base);
    assert!(unbound.bind_origin(FORGE_REPOSITORY, "../bad").is_err());
    assert!(
        unbound
            .bind_origin(FORGE_REPOSITORY, "refs/heads/main")
            .is_err()
    );

    let mut tree = unbound
        .create_worktree(&base, &temp.path().join("unbound"))
        .unwrap();
    tree.preview(&[patch("catalog/service.yaml", "new")], &render())
        .unwrap();
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    assert!(matches!(
        tree.commit_and_push("unbound", "update image", &author),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn equivalent_scp_push_destination_publishes_to_the_bound_repository() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "git@example.invalid:team/repository",
        ],
    );
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("scp"))
        .unwrap();
    tree.preview(&[patch("catalog/service.yaml", "new")], &render())
        .unwrap();
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    let published = tree.commit_and_push("scp", "update", &author).unwrap();
    assert_eq!(
        git_test(
            &source,
            &["rev-parse", &format!("refs/heads/{}", published.branch)]
        )
        .trim(),
        published.commit
    );
}

#[test]
fn https_origin_cannot_override_redirect_prohibition() {
    let (_temp, repo, _) = fixture();
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "origin",
            "https://example.invalid/team/repository.git",
        ],
    );
    let key = "http.https://example.invalid/.followRedirects";
    let unbound = GitOpsRepo::open(&repo.bare).unwrap();
    let target = repo
        .binding
        .as_ref()
        .unwrap()
        .target_ref
        .trim_start_matches("refs/heads/");
    git_test(&repo.bare, &["config", key, "true"]);
    assert!(matches!(
        unbound.bind_origin(FORGE_REPOSITORY, target),
        Err(Error::Invalid(_))
    ));
    git_test(&repo.bare, &["config", key, "false"]);
    assert!(unbound.bind_origin(FORGE_REPOSITORY, target).is_ok());
}

#[test]
fn origin_rewrite_cannot_hide_a_second_transport_rewrite() {
    let (_temp, repo, _) = fixture();
    git_test(
        &repo.bare,
        &["remote", "set-url", "origin", "alias:team/repository"],
    );
    git_test(
        &repo.bare,
        &[
            "config",
            "url.ssh://git@example.invalid/.insteadOf",
            "alias:",
        ],
    );
    // The first expansion is the configured repository and is safe by itself.
    repo.fetch().unwrap();
    git_test(
        &repo.bare,
        &[
            "config",
            "url.ssh://git@evil.invalid/.insteadOf",
            "ssh://git@example.invalid/",
        ],
    );
    // A direct push or fetch of the already-expanded URL would be rewritten
    // again by Git, so it must be refused before contacting the remote.
    assert!(matches!(repo.fetch(), Err(Error::Invalid(_))));
}

#[test]
fn initial_publication_rejects_existing_fast_forwardable_branch() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    let reference = format!("refs/heads/{}", branch_for_change("existing").unwrap());
    git_test(&source, &["update-ref", &reference, &base]);
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("tree"))
        .unwrap();
    tree.preview(&[patch("catalog/service.yaml", "new")], &render())
        .unwrap();
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    assert!(matches!(
        tree.commit_and_push("existing", "update image", &author),
        Err(Error::Invalid(_))
    ));
    assert_eq!(git_test(&source, &["rev-parse", &reference]).trim(), base);
}

#[test]
fn pull_head_fetch_uses_bound_origin_and_returns_current_commit() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    git_test(&source, &["update-ref", "refs/pull/42/head", &base]);
    assert_eq!(repo.fetch_pull_head(42).unwrap(), base);
    assert!(repo.fetch_pull_head(0).is_err());
    assert!(
        GitOpsRepo::open(&repo.bare)
            .unwrap()
            .fetch_pull_head(42)
            .is_err()
    );

    fs::write(
        source.join("catalog/service.yaml"),
        "service:\n  image: fork\n  replicas: 1\n",
    )
    .unwrap();
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "fork head"]);
    let moved = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    git_test(&source, &["update-ref", "refs/pull/42/head", &moved, &base]);
    assert_eq!(repo.fetch_pull_head(42).unwrap(), moved);
    assert_eq!(
        git_test(&repo.bare, &["rev-parse", "refs/thalamus/pull/42"]).trim(),
        moved
    );

    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://git@evil.invalid/team/repository.git",
        ],
    );
    assert!(matches!(repo.fetch_pull_head(42), Err(Error::Invalid(_))));
    assert_eq!(
        git_test(&repo.bare, &["rev-parse", "refs/thalamus/pull/42"]).trim(),
        moved
    );
}

#[cfg(unix)]
#[test]
fn initial_absent_branch_lease_rejects_a_racing_creation() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    let reference = format!("refs/heads/{}", branch_for_change("racing-create").unwrap());
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("worktree"))
        .unwrap();
    tree.preview(&[patch("catalog/service.yaml", "new")], &render())
        .unwrap();
    let ssh = temp.path().join("create-race-transport");
    let source_git = shell_quote(source.join(".git").to_str().unwrap());
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n\
             unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX GIT_COMMON_DIR\n\
             case \"$*\" in\n\
               *git-upload-pack*) exec git upload-pack {source_git} ;;\n\
               *git-receive-pack*)\n\
                 git -C {} update-ref {reference} {base} || exit 1\n\
                 exec git receive-pack {source_git} ;;\n\
               *) exit 2 ;;\n\
             esac\n",
            shell_quote(source.to_str().unwrap()),
        ),
    )
    .unwrap();
    let command = format!("sh {}", shell_quote(ssh.to_str().unwrap()));
    git_test(&repo.bare, &["config", "core.sshCommand", &command]);
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    assert!(matches!(
        tree.commit_and_push("racing-create", "update image", &author),
        Err(Error::Process { .. })
    ));
    assert_eq!(git_test(&source, &["rev-parse", &reference]).trim(), base);
}

#[cfg(unix)]
#[test]
fn lease_rejects_a_change_after_preflight_before_push() {
    use std::os::unix::fs::PermissionsExt;

    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    let mut first = repo
        .create_worktree(&base, &temp.path().join("first"))
        .unwrap();
    first
        .preview(&[patch("catalog/service.yaml", "first")], &render())
        .unwrap();
    let published = first
        .commit_and_push("racing-lease", "update image", &author)
        .unwrap();
    let branch_ref = format!("refs/heads/{}", published.branch);
    let tree = git_test(
        &source,
        &["rev-parse", &format!("{}^{{tree}}", published.commit)],
    );
    let competing = git_test(
        &source,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            &published.commit,
            "-m",
            "racing update",
        ],
    )
    .trim()
    .to_owned();

    let mut replacement = repo
        .create_worktree(&base, &temp.path().join("replacement"))
        .unwrap();
    replacement
        .preview(&[patch("catalog/service.yaml", "replacement")], &render())
        .unwrap();

    // The local SSH transport presents the real remote and changes its ref
    // exactly after ls-remote preflight but before receive-pack advertises it.
    let ssh = temp.path().join("ssh-transport");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n\
         unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX GIT_COMMON_DIR\n\
         case \"$*\" in\n\
           *git-upload-pack*) exec git upload-pack ../source/.git ;;\n\
           *git-receive-pack*)\n\
             git -C ../source update-ref {branch_ref} {competing} {} || exit 1\n\
             exec git receive-pack ../source/.git ;;\n\
           *) exit 2 ;;\n\
         esac\n",
            published.commit,
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let command = format!("'{}'", ssh.to_string_lossy().replace('\'', "'\\''"));
    git_test(&repo.bare, &["config", "core.sshCommand", &command]);
    git_test(&repo.bare, &["config", "ssh.variant", "ssh"]);

    assert!(matches!(
        replacement.commit_and_push_update(
            "racing-lease",
            "update image",
            &author,
            &published.commit
        ),
        Err(Error::Process { .. })
    ));
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        competing
    );
}

#[test]
fn service_file_is_resolved_from_pinned_regular_catalog_blobs() {
    let (temp, repo, _) = fixture();
    let source = temp.path().join("source");
    let services = source.join("catalog/services");
    fs::create_dir_all(&services).unwrap();
    fs::write(services.join("primary.yaml"), "name: alpha\nreplicas: 1\n").unwrap();
    fs::write(services.join("other.yml"), "name: beta\n").unwrap();
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "add services"]);
    let pinned = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    repo.fetch().unwrap();
    assert_eq!(
        repo.service_file(&pinned, "alpha").unwrap(),
        PathBuf::from("catalog/services/primary.yaml")
    );
    assert_eq!(
        repo.service_file(&pinned, "beta").unwrap(),
        PathBuf::from("catalog/services/other.yml")
    );
    assert!(matches!(
        repo.service_file(&pinned, "missing"),
        Err(Error::ServiceNotFound)
    ));
    assert!(matches!(
        repo.service_file(&pinned, "../alpha"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        repo.service_file("not-a-commit", "alpha"),
        Err(Error::Invalid(_))
    ));

    git_test(&source, &["tag", "-a", "annotated", "-m", "annotated tag"]);
    repo.fetch().unwrap();
    let tag = git_test(&repo.bare, &["rev-parse", "refs/tags/annotated"]);
    assert!(matches!(
        repo.service_file(tag.trim(), "alpha"),
        Err(Error::Invalid(_))
    ));

    fs::write(services.join("primary.yaml"), "name: renamed\n").unwrap();
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "rename service"]);
    let moved = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    repo.fetch().unwrap();
    assert_eq!(
        repo.service_file(&pinned, "alpha").unwrap(),
        PathBuf::from("catalog/services/primary.yaml")
    );
    assert!(matches!(
        repo.service_file(&moved, "alpha"),
        Err(Error::ServiceNotFound)
    ));
    assert_eq!(
        repo.service_file(&moved, "renamed").unwrap(),
        PathBuf::from("catalog/services/primary.yaml")
    );

    fs::write(services.join("duplicate.yaml"), "name: renamed\n").unwrap();
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "duplicate service"]);
    let duplicate = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    repo.fetch().unwrap();
    assert!(matches!(
        repo.service_file(&duplicate, "renamed"),
        Err(Error::Invalid(_))
    ));
    #[cfg(unix)]
    {
        fs::remove_file(services.join("duplicate.yaml")).unwrap();
        std::os::unix::fs::symlink("../service.yaml", services.join("link.yml")).unwrap();
        git_test(&source, &["add", "-A"]);
        git_test(&source, &["commit", "-m", "symlink service"]);
        let symlink = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
        repo.fetch().unwrap();
        assert!(matches!(
            repo.service_file(&symlink, "renamed"),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn render_ref_is_bound_and_discards_worktrees_on_both_outcomes() {
    let (temp, repo, base) = fixture();
    let path = temp.path().join("render");
    assert!(
        repo.render_ref(&base, &path, &render())
            .unwrap()
            .contains("image: old")
    );
    assert!(!path.exists());
    let bad = RenderCommand {
        executable: "false".into(),
        args: vec![],
    };
    assert!(repo.render_ref(&base, &path, &bad).is_err());
    assert!(!path.exists());
    let unbound = GitOpsRepo::open(&repo.bare).unwrap();
    assert!(matches!(
        unbound.render_ref(&base, &path, &render()),
        Err(Error::Invalid(_))
    ));
    assert!(!path.exists());
}

#[test]
fn ancestry_checks_the_bound_fetched_target_without_replacement_objects() {
    let (temp, repo, base) = fixture();
    let source = temp.path().join("source");
    let branch = git_test(&source, &["branch", "--show-current"]);
    let branch = branch.trim();
    let tree = git_test(&source, &["rev-parse", "HEAD^{tree}"]);
    let other = git_test(&source, &["commit-tree", tree.trim(), "-m", "unrelated"]);
    let other = other.trim();
    fs::write(
        source.join("catalog/service.yaml"),
        "service:\n  image: replaced\n  replicas: 1\n",
    )
    .unwrap();
    git_test(&source, &["add", "catalog/service.yaml"]);
    let replacement_tree = git_test(&source, &["write-tree"]);
    let replacement = git_test(
        &source,
        &[
            "commit-tree",
            replacement_tree.trim(),
            "-p",
            other,
            "-m",
            "replacement",
        ],
    );
    let replacement = replacement.trim();
    git_test(&source, &["update-ref", "refs/heads/unrelated", other]);
    git_test(
        &source,
        &["update-ref", "refs/heads/replacement", replacement],
    );
    repo.fetch().unwrap();
    assert!(repo.is_ancestor(&base, branch).unwrap());
    assert!(!repo.is_ancestor(other, branch).unwrap());
    assert!(matches!(
        repo.is_ancestor(&base, "unrelated"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        repo.is_ancestor("abc", branch),
        Err(Error::Invalid(_))
    ));

    // Replacements must not make an unrelated real commit appear as an
    // ancestor of the trusted target.
    git_test(&repo.bare, &["replace", &base, replacement]);
    assert!(!repo.is_ancestor(other, branch).unwrap());
    let rendered = repo
        .render_ref(&base, &temp.path().join("replace-render"), &render())
        .unwrap();
    assert!(rendered.contains("image: old"));
    assert!(!rendered.contains("image: replaced"));
    let unbound = GitOpsRepo::open(&repo.bare).unwrap();
    assert!(matches!(
        unbound.is_ancestor(&base, branch),
        Err(Error::Invalid(_))
    ));
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "ssh://git@evil.invalid/team/repository.git",
        ],
    );
    assert!(matches!(
        repo.is_ancestor(&base, branch),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn compare_refs_cleans_up_on_success_and_error() {
    let (temp, repo, base) = fixture();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    let preview = repo.preview_refs(&base, &base, &a, &b, &render()).unwrap();
    assert_eq!(preview.base_render, preview.proposed_render);
    assert!(!a.exists() && !b.exists());
    let bad = RenderCommand {
        executable: "false".into(),
        args: vec![],
    };
    assert!(repo.preview_refs(&base, &base, &a, &b, &bad).is_err());
    assert!(!a.exists() && !b.exists());
}

#[test]
fn revert_previews_real_inverse_and_rejects_templates() {
    let (temp, repo, _) = fixture();
    let source = temp.path().join("source");
    fs::write(
        source.join("catalog/service.yaml"),
        "service:\n  image: changed\n",
    )
    .unwrap();
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "change catalog"]);
    let change = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    repo.fetch().unwrap();
    let mut tree = repo
        .create_worktree(&change, &temp.path().join("revert"))
        .unwrap();
    let preview = tree.preview_revert(&change, &render()).unwrap();
    assert!(preview.base_render.contains("changed"));
    assert!(preview.proposed_render.contains("old"));
    assert!(preview.data_diff.contains("old"));
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    let published = tree
        .commit_and_push("revert", "revert catalog", &author)
        .unwrap();
    let inverse = git_test(
        &source,
        &[
            "show",
            &format!("{}:catalog/service.yaml", published.commit),
        ],
    );
    assert!(inverse.contains("old"));
    repo.discard_worktree(tree).unwrap();
    fs::write(source.join("template.k"), "value = 2\n").unwrap();
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "change template"]);
    let bad = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    repo.fetch().unwrap();
    let mut tree = repo
        .create_worktree(&bad, &temp.path().join("bad"))
        .unwrap();
    assert!(tree.preview_revert(&bad, &render()).is_err());
    repo.discard_worktree(tree).unwrap();
}

#[test]
fn independent_previews_commit_and_retry() {
    let (temp, repo, base) = fixture();
    let mut first = repo
        .create_worktree(&base, &temp.path().join("one"))
        .unwrap();
    let mut second = repo
        .create_worktree(&base, &temp.path().join("two"))
        .unwrap();
    let preview = first
        .preview(&[patch("catalog/service.yaml", "new")], &render())
        .unwrap();
    assert!(preview.base_render.contains("old"));
    assert!(preview.proposed_render.contains("new"));
    assert!(preview.data_diff.contains("+  image: new"));
    assert!(preview.rendered_diff.contains("+  image: new"));
    assert!(
        fs::read_to_string(second.path().join("catalog/service.yaml"))
            .unwrap()
            .contains("old")
    );
    let author = CommitAuthor {
        name: "Thalamus".into(),
        email: "thalamus@example.test".into(),
    };
    let published = first
        .commit_and_push("change-123", "update image", &author)
        .unwrap();
    let resumed = repo.resume_committed_worktree(&base, first.path()).unwrap();
    let repeated = resumed
        .commit_and_push("change-123", "update image", &author)
        .unwrap();
    assert_eq!(published.commit, repeated.commit);
    assert_eq!(published.branch, repeated.branch);
    second
        .preview(&[patch("catalog/service.yaml", "new")], &render())
        .unwrap();
    let recreated = second
        .commit_and_push("change-123", "update image", &author)
        .unwrap();
    assert_eq!(published.commit, recreated.commit);
    assert_eq!(
        git_test(
            &temp.path().join("source"),
            &["rev-parse", &format!("refs/heads/{}", published.branch)]
        )
        .trim(),
        published.commit
    );
}

#[test]
fn refuses_templates_traversal_symlinks_and_missing_keys() {
    let (temp, repo, base) = fixture();
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("tree"))
        .unwrap();
    for file in [
        "template.k",
        "catalog/../template.k",
        "catalog/template.k",
        "/tmp/service.yaml",
    ] {
        assert!(
            tree.preview(&[patch(file, "new")], &render()).is_err(),
            "{file}"
        );
    }
    let bad_key = Patch {
        file: "catalog/service.yaml".into(),
        path: vec!["missing".into(), "image".into()],
        operation: Operation::Remove,
    };
    assert!(tree.preview(&[bad_key], &render()).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("service.yaml", tree.path().join("catalog/link.yaml")).unwrap();
        assert!(validate_file(tree.path(), Path::new("catalog/link.yaml")).is_err());
    }
    assert!(
        fs::read_to_string(tree.path().join("catalog/service.yaml"))
            .unwrap()
            .contains("old")
    );
}

#[test]
fn render_errors_are_reported_and_cannot_be_committed() {
    let (temp, repo, base) = fixture();
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("tree"))
        .unwrap();
    let render = RenderCommand {
        executable: "cat".into(),
        args: vec!["no-such-file".into()],
    };
    assert!(matches!(
        tree.preview(&[patch("catalog/service.yaml", "new")], &render),
        Err(Error::Process { .. })
    ));
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    assert!(tree.commit_and_push("id", "message", &author).is_err());
}

#[test]
fn kcl_cli_renders() {
    let (temp, repo, base) = fixture();
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("tree"))
        .unwrap();
    let command = RenderCommand {
        executable: "kcl".into(),
        args: vec!["template.k".into()],
    };
    let preview = tree
        .preview(&[patch("catalog/service.yaml", "new")], &command)
        .unwrap();
    assert!(preview.base_render.contains("value: 1"));
    assert_eq!(preview.base_render, preview.proposed_render);
    assert!(preview.data_diff.contains("image: new"));
}

#[test]
fn real_catalog_publication_commits_preview_and_refreshes_with_a_lease() {
    const ORIGINAL_IMAGE: &str = "vllm/vllm-openai:v0.23.0";
    const PROPOSED_IMAGE: &str = "vllm/vllm-openai:v0.23.1";

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                assert!(
                    kind.is_file(),
                    "unexpected fixture entry {:?}",
                    entry.path()
                );
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn resource<'a>(render: &'a Value, kind: &str, name: &str) -> &'a Value {
        render["items"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|item| {
                item["kind"].as_str() == Some(kind)
                    && item["metadata"]["name"].as_str() == Some(name)
            })
            .unwrap_or_else(|| panic!("missing {kind} {name}"))
    }

    fn workload_image(render: &Value) -> &str {
        resource(render, "LLMInferenceService", "example-deployment")["spec"]["template"]
            ["containers"][0]["image"]
            .as_str()
            .unwrap()
    }

    fn resource_key(item: &Value) -> (String, String, String, String) {
        (
            item["apiVersion"].as_str().unwrap().to_owned(),
            item["kind"].as_str().unwrap().to_owned(),
            item["metadata"]["namespace"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            item["metadata"]["name"].as_str().unwrap().to_owned(),
        )
    }

    // Compare every resource in both Git trees with the actual preview and
    // the bytes left by the renderer, including each Kustomize inventory.
    fn assert_committed_render(source: &Path, worktree: &Path, commit: &str, preview: &str) {
        let rendered: Value = serde_yaml::from_str(preview).unwrap();
        assert_eq!(rendered["apiVersion"].as_str(), Some("v1"));
        assert_eq!(rendered["kind"].as_str(), Some("List"));
        let mut expected = BTreeMap::new();
        for item in rendered["items"].as_sequence().unwrap() {
            assert!(expected.insert(resource_key(item), item.clone()).is_none());
        }
        for root in ["rendered/inference", "rendered/pools"] {
            let entries = git_test(
                source,
                &["ls-tree", "-r", "--name-only", commit, "--", root],
            );
            let prefix = format!("{root}/");
            let files: Vec<_> = entries
                .lines()
                .map(|path| path.strip_prefix(&prefix).unwrap())
                .collect();
            let resources: Vec<_> = files
                .iter()
                .copied()
                .filter(|name| name.ends_with(".json"))
                .collect();
            assert!(!resources.is_empty(), "no resources in {root}");
            assert_eq!(files.len(), resources.len() + 1);
            assert!(files.contains(&"kustomization.yaml"));

            let inventory_path = format!("{root}/kustomization.yaml");
            let inventory = git_test(source, &["show", &format!("{commit}:{inventory_path}")]);
            assert_eq!(
                inventory,
                fs::read_to_string(worktree.join(&inventory_path)).unwrap()
            );
            let inventory: Value = serde_yaml::from_str(&inventory).unwrap();
            let listed: Vec<_> = inventory["resources"]
                .as_sequence()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap())
                .collect();
            assert_eq!(listed, resources);

            for name in resources {
                let path = format!("{root}/{name}");
                let contents = git_test(source, &["show", &format!("{commit}:{path}")]);
                assert_eq!(contents, fs::read_to_string(worktree.join(&path)).unwrap());
                let item: Value = serde_yaml::from_str(&contents).unwrap();
                assert_eq!(expected.remove(&resource_key(&item)), Some(item), "{path}");
            }
        }
        assert!(
            expected.is_empty(),
            "preview resources missing from Git: {expected:?}"
        );
    }

    fn assert_catalog(
        source: &Path,
        commit: &str,
        hostname: &str,
        image: &str,
        replicas: i64,
        total_gpu: i64,
    ) {
        let service = git_test(
            source,
            &[
                "show",
                &format!("{commit}:catalog/services/example-service.yaml"),
            ],
        );
        let service: Value = serde_yaml::from_str(&service).unwrap();
        assert_eq!(
            service["deployments"][0]["replicas"].as_i64(),
            Some(replicas)
        );
        assert_eq!(service["deployments"][0]["image"].as_str(), Some(image));
        assert_eq!(service["gateway"]["hostname"].as_str(), Some(hostname));
        let pool = git_test(
            source,
            &["show", &format!("{commit}:catalog/pools/example-pool.yaml")],
        );
        let pool: Value = serde_yaml::from_str(&pool).unwrap();
        assert_eq!(pool["totalGpu"].as_i64(), Some(total_gpu));
    }

    fn assert_committed_image(source: &Path, commit: &str, path: &str, image: &str) {
        let blob = git_test(source, &["show", &format!("{commit}:{path}")]);
        let workload: Value = serde_yaml::from_str(&blob).unwrap();
        assert_eq!(workload["kind"].as_str(), Some("LLMInferenceService"));
        assert_eq!(
            workload["spec"]["template"]["containers"][0]["image"].as_str(),
            Some(image)
        );
    }

    fn output_path(worktree: &Path, root: &str, prefix: &str) -> String {
        let matches: Vec<_> = fs::read_dir(worktree.join(root))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect();
        assert_eq!(matches.len(), 1, "expected one generated file for {prefix}");
        matches[0]
            .strip_prefix(worktree)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    let (temp, repo, _) = fixture();
    let source = temp.path().join("source");
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    copy_tree(&project.join("catalog"), &source.join("catalog"));
    copy_tree(&project.join("templates"), &source.join("templates"));
    fs::create_dir_all(source.join("scripts")).unwrap();
    for path in [
        "main.k",
        "limits.k",
        "pool.k",
        "kcl.mod",
        "kcl.mod.lock",
        "scripts/render-catalog.py",
    ] {
        fs::copy(project.join(path), source.join(path)).unwrap();
    }
    let renderer = RenderCommand {
        executable: "python3".into(),
        args: vec![
            "-I".into(),
            fs::canonicalize(project.join("scripts/render-catalog.py"))
                .unwrap()
                .into_os_string(),
            "--write".into(),
        ],
    };
    run(&renderer.executable, &renderer.args, Some(&source), &[]).unwrap();
    let stale = "rendered/inference/obsolete.json";
    fs::write(
        source.join(stale),
        r#"{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"obsolete"}}"#,
    )
    .unwrap();
    git_test(&source, &["add", "."]);
    git_test(&source, &["commit", "-m", "seed real catalog"]);
    let base = git_test(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    let target = git_test(&source, &["branch", "--show-current"]);
    repo.fetch().unwrap();
    assert_eq!(repo.resolve_base(target.trim()).unwrap(), base);
    assert_eq!(
        git_test(
            &source,
            &["ls-tree", "-r", "--name-only", &base, "--", stale]
        )
        .trim(),
        stale
    );

    // An unmerged checkout can replace its copy of the script. Reference
    // previews must still run the separately trusted real renderer above.
    let unmerged = temp.path().join("unmerged-source");
    git_test(
        &source,
        &[
            "worktree",
            "add",
            "--detach",
            unmerged.to_str().unwrap(),
            &base,
        ],
    );
    let marker = temp.path().join("unmerged-script-executed");
    let script_path = unmerged.join("scripts/render-catalog.py");
    let checked_in_script = fs::read_to_string(&script_path).unwrap();
    fs::write(
        &script_path,
        format!(
            "from pathlib import Path\nPath({:?}).write_text('executed')\n{}",
            marker.to_str().unwrap(),
            checked_in_script
        ),
    )
    .unwrap();
    let service_path = unmerged.join("catalog/services/example-service.yaml");
    let mut service: Value = serde_yaml::from_slice(&fs::read(&service_path).unwrap()).unwrap();
    service["deployments"][0]["image"] = Value::String(PROPOSED_IMAGE.into());
    fs::write(&service_path, serde_yaml::to_string(&service).unwrap()).unwrap();
    git_test(
        &unmerged,
        &[
            "add",
            "--",
            "scripts/render-catalog.py",
            "catalog/services/example-service.yaml",
        ],
    );
    git_test(&unmerged, &["commit", "-m", "unmerged checkout script"]);
    let unmerged_head = git_test(&unmerged, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_test(
        &source,
        &["update-ref", "refs/heads/unmerged-script", &unmerged_head],
    );
    repo.fetch().unwrap();
    let reference_base = temp.path().join("unmerged-preview-base");
    let reference_head = temp.path().join("unmerged-preview-head");
    let reference_preview = repo
        .preview_refs(
            &base,
            &unmerged_head,
            &reference_base,
            &reference_head,
            &renderer,
        )
        .unwrap();
    let base_render: Value = serde_yaml::from_str(&reference_preview.base_render).unwrap();
    let head_render: Value = serde_yaml::from_str(&reference_preview.proposed_render).unwrap();
    assert_eq!(workload_image(&base_render), ORIGINAL_IMAGE);
    assert_eq!(workload_image(&head_render), PROPOSED_IMAGE);
    assert!(!marker.exists());
    assert!(!reference_base.exists());
    assert!(!reference_head.exists());

    // This is illustrative desired state, not evidence that the image exists
    // or has been validated or deployed.
    let patches = [
        Patch {
            file: "catalog/services/example-service.yaml".into(),
            path: vec!["deployments".into(), "0".into(), "image".into()],
            operation: Operation::Set(Value::String(PROPOSED_IMAGE.into())),
        },
        Patch {
            file: "catalog/services/example-service.yaml".into(),
            path: vec!["deployments".into(), "0".into(), "replicas".into()],
            operation: Operation::Set(Value::Number(2.into())),
        },
        Patch {
            file: "catalog/pools/example-pool.yaml".into(),
            path: vec!["totalGpu".into()],
            operation: Operation::Set(Value::Number(4.into())),
        },
    ];
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    let change_id = "real-catalog-publication";
    let message = "update catalog resources";
    let mut first = repo
        .create_worktree(&base, &temp.path().join("catalog-first"))
        .unwrap();
    let preview = first.preview(&patches, &renderer).unwrap();
    let base_render: Value = serde_yaml::from_str(&preview.base_render).unwrap();
    let proposed: Value = serde_yaml::from_str(&preview.proposed_render).unwrap();
    assert_eq!(workload_image(&base_render), ORIGINAL_IMAGE);
    assert_eq!(workload_image(&proposed), PROPOSED_IMAGE);
    assert_eq!(
        resource(&base_render, "LLMInferenceService", "example-deployment")["spec"]["replicas"]
            .as_i64(),
        Some(1)
    );
    assert_eq!(
        resource(&proposed, "LLMInferenceService", "example-deployment")["spec"]["replicas"]
            .as_i64(),
        Some(2)
    );
    let queue = "example-pool-example-pool-deployment";
    assert_eq!(
        resource(&base_render, "ClusterQueue", queue)["spec"]["resourceGroups"][0]["flavors"][0]
            ["resources"][0]["nominalQuota"]
            .as_str(),
        Some("2")
    );
    assert_eq!(
        resource(&proposed, "ClusterQueue", queue)["spec"]["resourceGroups"][0]["flavors"][0]
            ["resources"][0]["nominalQuota"]
            .as_str(),
        Some("4")
    );
    assert!(preview.data_diff.contains(PROPOSED_IMAGE));
    assert!(preview.rendered_diff.contains(PROPOSED_IMAGE));
    assert!(preview.data_diff.contains("replicas: 2"));
    assert!(preview.data_diff.contains("totalGpu: 4"));
    assert!(preview.rendered_diff.contains("\"replicas\": 2"));
    assert!(!first.path().join(stale).exists());

    let workload_path = output_path(
        first.path(),
        "rendered/inference",
        "llminferenceservice-example-deployment-",
    );
    let queue_path = output_path(
        first.path(),
        "rendered/pools",
        "clusterqueue-example-pool-example-pool-deployment-",
    );
    let workload = fs::read_to_string(first.path().join(&workload_path)).unwrap();
    assert!(workload.contains("\"replicas\": 2"));
    fs::write(
        first.path().join(&workload_path),
        workload.replacen("\"replicas\": 2", "\"replicas\": 99", 1),
    )
    .unwrap();
    assert!(matches!(
        first.commit_and_push(change_id, message, &author),
        Err(Error::Invalid(_))
    ));
    assert_eq!(git_test(first.path(), &["rev-parse", "HEAD"]).trim(), base);
    let branch_ref = format!("refs/heads/{}", branch_for_change(change_id).unwrap());
    assert!(git(&source, &args(&["rev-parse", "--verify", &branch_ref])).is_err());
    fs::write(first.path().join(&workload_path), workload).unwrap();
    fs::write(first.path().join("unrelated.txt"), "not reviewed\n").unwrap();
    let published = first.commit_and_push(change_id, message, &author).unwrap();
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        published.commit
    );
    assert_eq!(
        git_test(&source, &["show", "-s", "--format=%P", &published.commit]).trim(),
        base
    );
    assert_catalog(
        &source,
        &published.commit,
        "inference.example.invalid",
        PROPOSED_IMAGE,
        2,
        4,
    );
    assert_committed_image(&source, &published.commit, &workload_path, PROPOSED_IMAGE);
    assert_committed_render(
        &source,
        first.path(),
        &published.commit,
        &preview.proposed_render,
    );
    let changes = git_test(
        &source,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            &published.commit,
        ],
    );
    for path in [
        "catalog/services/example-service.yaml",
        "catalog/pools/example-pool.yaml",
        &workload_path,
        &queue_path,
    ] {
        assert!(
            changes.lines().any(|line| line == format!("M\t{path}")),
            "{path}"
        );
    }
    assert!(changes.lines().any(|line| line == format!("D\t{stale}")));
    assert!(
        git_test(
            &source,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                &published.commit,
                "--",
                "unrelated.txt"
            ]
        )
        .is_empty()
    );

    // Advance the target without merging the change branch. The refreshed
    // preview must combine that new catalog value with the original patches.
    let service_path = source.join("catalog/services/example-service.yaml");
    let mut service: Value = serde_yaml::from_slice(&fs::read(&service_path).unwrap()).unwrap();
    service["gateway"]["hostname"] = Value::String("moved.example.invalid".into());
    fs::write(&service_path, serde_yaml::to_string(&service).unwrap()).unwrap();
    git_test(
        &source,
        &["add", "--", "catalog/services/example-service.yaml"],
    );
    git_test(&source, &["commit", "-m", "move target"]);
    repo.fetch().unwrap();
    let moved = repo.resolve_base(target.trim()).unwrap();
    assert_ne!(moved, base);
    let mut fresh = repo
        .create_worktree(&moved, &temp.path().join("catalog-fresh"))
        .unwrap();
    let refreshed = fresh.preview(&patches, &renderer).unwrap();
    assert_eq!(fresh.base(), moved);
    let refreshed_base: Value = serde_yaml::from_str(&refreshed.base_render).unwrap();
    let refreshed_proposed: Value = serde_yaml::from_str(&refreshed.proposed_render).unwrap();
    assert_eq!(workload_image(&refreshed_base), ORIGINAL_IMAGE);
    assert_eq!(workload_image(&refreshed_proposed), PROPOSED_IMAGE);
    for render in [&refreshed_base, &refreshed_proposed] {
        assert_eq!(
            resource(render, "HTTPRoute", "example-service")["spec"]["hostnames"][0].as_str(),
            Some("moved.example.invalid")
        );
    }
    assert_eq!(
        resource(&refreshed_base, "LLMInferenceService", "example-deployment")["spec"]["replicas"]
            .as_i64(),
        Some(1)
    );
    assert_eq!(
        resource(
            &refreshed_proposed,
            "LLMInferenceService",
            "example-deployment"
        )["spec"]["replicas"]
            .as_i64(),
        Some(2)
    );
    assert!(!fresh.path().join(stale).exists());

    // A mismatched old head cannot replace the branch; retry the same
    // deterministic commit with the recorded published head as the lease.
    assert!(matches!(
        fresh.commit_and_push_update(change_id, message, &author, &base),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        published.commit
    );
    let updated = fresh
        .commit_and_push_update(change_id, message, &author, &published.commit)
        .unwrap();
    assert_eq!(updated.branch, published.branch);
    assert_ne!(updated.commit, published.commit);
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        updated.commit
    );
    assert_eq!(
        git_test(&source, &["show", "-s", "--format=%P", &updated.commit]).trim(),
        moved
    );
    assert_catalog(
        &source,
        &updated.commit,
        "moved.example.invalid",
        PROPOSED_IMAGE,
        2,
        4,
    );
    assert_committed_image(&source, &updated.commit, &workload_path, PROPOSED_IMAGE);
    assert_committed_render(
        &source,
        fresh.path(),
        &updated.commit,
        &refreshed.proposed_render,
    );
    let changes = git_test(
        &source,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            &updated.commit,
        ],
    );
    for path in [
        "catalog/services/example-service.yaml",
        "catalog/pools/example-pool.yaml",
        &workload_path,
        &queue_path,
    ] {
        assert!(
            changes.lines().any(|line| line == format!("M\t{path}")),
            "{path}"
        );
    }
    assert!(changes.lines().any(|line| line == format!("D\t{stale}")));
    assert!(
        git_test(
            &source,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                &updated.commit,
                "--",
                "unrelated.txt"
            ]
        )
        .is_empty()
    );

    // Reverting a published commit must invert catalog sources and regenerate
    // both output trees, rather than trying to revert the generated blobs.
    let mut inverse = repo
        .create_worktree(&updated.commit, &temp.path().join("catalog-inverse"))
        .unwrap();
    let reverted_preview = inverse.preview_revert(&updated.commit, &renderer).unwrap();
    assert_eq!(reverted_preview.base_render, refreshed.proposed_render);
    let reverted_render: Value = serde_yaml::from_str(&reverted_preview.proposed_render).unwrap();
    assert_eq!(workload_image(&reverted_render), ORIGINAL_IMAGE);
    assert_eq!(
        resource(
            &reverted_render,
            "LLMInferenceService",
            "example-deployment"
        )["spec"]["replicas"]
            .as_i64(),
        Some(1)
    );
    assert_eq!(
        resource(&reverted_render, "ClusterQueue", queue)["spec"]["resourceGroups"][0]["flavors"]
            [0]["resources"][0]["nominalQuota"]
            .as_str(),
        Some("2")
    );
    assert_eq!(
        resource(&reverted_render, "HTTPRoute", "example-service")["spec"]["hostnames"][0].as_str(),
        Some("moved.example.invalid")
    );
    assert!(!reverted_preview.data_diff.contains("rendered/"));
    assert!(reverted_preview.rendered_diff.contains("\"replicas\": 1"));
    let reverted = inverse
        .commit_and_push("real-catalog-revert", "revert catalog resources", &author)
        .unwrap();
    assert_eq!(
        git_test(
            &source,
            &["rev-parse", &format!("refs/heads/{}", reverted.branch)]
        )
        .trim(),
        reverted.commit
    );
    assert_eq!(
        git_test(&source, &["show", "-s", "--format=%P", &reverted.commit]).trim(),
        updated.commit
    );
    assert_catalog(
        &source,
        &reverted.commit,
        "moved.example.invalid",
        ORIGINAL_IMAGE,
        1,
        2,
    );
    assert_committed_image(&source, &reverted.commit, &workload_path, ORIGINAL_IMAGE);
    assert_committed_render(
        &source,
        inverse.path(),
        &reverted.commit,
        &reverted_preview.proposed_render,
    );
    let changes = git_test(
        &source,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            &reverted.commit,
        ],
    );
    for path in [
        "catalog/services/example-service.yaml",
        "catalog/pools/example-pool.yaml",
        &workload_path,
        &queue_path,
    ] {
        assert!(
            changes.lines().any(|line| line == format!("M\t{path}")),
            "{path}"
        );
    }
    assert!(
        git_test(
            &source,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                &reverted.commit,
                "--",
                stale
            ]
        )
        .is_empty()
    );
    repo.discard_worktree(inverse).unwrap();
    repo.discard_worktree(first).unwrap();
    repo.discard_worktree(fresh).unwrap();
}

#[test]
fn previewed_data_cannot_be_modified_before_commit() {
    let (temp, repo, base) = fixture();
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("tree"))
        .unwrap();
    tree.preview(&[patch("catalog/service.yaml", "new")], &render())
        .unwrap();
    fs::write(
        tree.path().join("catalog/service.yaml"),
        "service:\n  image: unreviewed\n",
    )
    .unwrap();
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    assert!(tree.commit_and_push("id", "message", &author).is_err());
}

#[test]
fn nested_sequence_patches_change_and_remove_items() {
    let mut value: Value = serde_yaml::from_str("items:\n  - name: a\n  - name: b\n").unwrap();
    apply(
        &mut value,
        &["items".into(), "0".into(), "name".into()],
        &Operation::Set(Value::String("c".into())),
    )
    .unwrap();
    apply(
        &mut value,
        &["items".into(), "1".into()],
        &Operation::Remove,
    )
    .unwrap();
    let expected: Value = serde_yaml::from_str("items:\n  - name: c\n").unwrap();
    assert_eq!(value, expected);
    assert!(
        apply(
            &mut value,
            &["items".into(), "2".into()],
            &Operation::Remove
        )
        .is_err()
    );
}

#[test]
fn creates_new_service_from_structured_yaml_and_pushes_it() {
    let (temp, repo, base) = fixture();
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("tree"))
        .unwrap();
    let service: Value =
        serde_yaml::from_str("name: example\ndeployments:\n  - name: gpu\n").unwrap();
    let patch = Patch {
        file: "catalog/services/example.yaml".into(),
        path: Vec::new(),
        operation: Operation::Set(service.clone()),
    };
    let preview = tree.preview(&[patch], &render()).unwrap();
    assert!(preview.data_diff.contains("new file mode"));
    assert!(preview.data_diff.contains("+name: example"));
    assert_eq!(
        serde_yaml::from_slice::<Value>(
            &fs::read(tree.path().join("catalog/services/example.yaml")).unwrap()
        )
        .unwrap(),
        service
    );
    let author = CommitAuthor {
        name: "Test".into(),
        email: "test@example.test".into(),
    };
    let published = tree
        .commit_and_push("create-service", "add service", &author)
        .unwrap();
    let source = temp.path().join("source");
    assert!(
        git_test(
            &source,
            &[
                "show",
                &format!("{}:catalog/services/example.yaml", published.commit)
            ]
        )
        .contains("name: example")
    );
}

#[test]
fn rejects_invalid_new_service_paths_and_non_mapping_creation() {
    let (temp, repo, base) = fixture();
    let mut tree = repo
        .create_worktree(&base, &temp.path().join("tree"))
        .unwrap();
    let mapping: Value = serde_yaml::from_str("name: example").unwrap();
    for file in [
        "catalog/new.yaml",
        "catalog/services/../escape.yaml",
        "catalog/services/deep/x.yaml",
        "catalog/services/new.k",
    ] {
        let patch = Patch {
            file: file.into(),
            path: Vec::new(),
            operation: Operation::Set(mapping.clone()),
        };
        assert!(tree.preview(&[patch], &render()).is_err(), "{file}");
    }
    let patch = Patch {
        file: "catalog/services/new.yaml".into(),
        path: Vec::new(),
        operation: Operation::Set(Value::String("not a service".into())),
    };
    assert!(tree.preview(&[patch], &render()).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.path(), tree.path().join("catalog/services")).unwrap();
        assert!(validate_new_service(tree.path(), Path::new("catalog/services/new.yaml")).is_err());
    }
}
