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
