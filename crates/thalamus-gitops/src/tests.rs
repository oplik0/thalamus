use super::*;
use tempfile::TempDir;

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
    let repo = GitOpsRepo::clone_bare(source.as_os_str(), &temp.path().join("bare.git")).unwrap();
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

    // The ordinary publication path must not force this non-fast-forward.
    assert!(matches!(
        fresh.commit_and_push(change_id, message, &author),
        Err(Error::Process { .. })
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
    git_test(
        &repo.bare,
        &["config", "remote.origin.receivepack", "false"],
    );
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
fn update_checks_actual_push_destination_not_fetch_destination() {
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

    // The separate push destination initially has the old branch head.
    let push_repo = temp.path().join("push.git");
    git_test(
        &source,
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            push_repo.to_str().unwrap(),
        ],
    );
    let mut replacement = repo
        .create_worktree(&base, &temp.path().join("replacement"))
        .unwrap();
    replacement
        .preview(&[patch("catalog/service.yaml", "replacement")], &render())
        .unwrap();
    assert!(
        replacement
            .commit_and_push("push-url", "update image", &author)
            .is_err()
    );
    let new_head = git_test(replacement.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    // Independently advance the fetch destination to the new commit. Merely
    // inspecting it would incorrectly conclude that the update was done.
    git_test(
        replacement.path(),
        &["push", "--force", "origin", &format!("HEAD:{branch_ref}")],
    );
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            push_repo.to_str().unwrap(),
        ],
    );
    let updated = replacement
        .commit_and_push_update("push-url", "update image", &author, &published.commit)
        .unwrap();
    assert_eq!(updated.commit, new_head);
    assert_eq!(
        git_test(&push_repo, &["rev-parse", &branch_ref]).trim(),
        new_head
    );
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        new_head
    );

    // A multi-destination push is refused rather than partially updating refs.
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "--add",
            "--push",
            "origin",
            source.to_str().unwrap(),
        ],
    );
    assert!(matches!(
        replacement.commit_and_push_update("push-url", "update image", &author, &published.commit),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        git_test(&push_repo, &["rev-parse", &branch_ref]).trim(),
        new_head
    );
    assert_eq!(
        git_test(&source, &["rev-parse", &branch_ref]).trim(),
        new_head
    );
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
    git_test(
        &repo.bare,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://git@example.invalid/repository",
        ],
    );

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
fn kcl_cli_renders_when_installed() {
    if Command::new("kcl").arg("--version").output().is_err() {
        return;
    }
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
