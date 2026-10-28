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
    let other_tenant = ServiceId {
        namespace: "tenant-b".into(),
        name: "model".into(),
    };
    assert!(matches!(
        policy.includes_deployment(&other_tenant, "primary"),
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
