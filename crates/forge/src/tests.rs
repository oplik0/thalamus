use crate::*;

#[test]
fn parses_clone_urls_and_rejects_unsafe_paths() {
    let a = RepoRef::parse(ForgeKind::Github, "git@github.example:team/catalog.git").unwrap();
    let b = RepoRef::parse(ForgeKind::Github, "https://github.example/team/catalog").unwrap();
    assert_eq!(a, b);
    assert_eq!(
        RepoRef::parse(ForgeKind::Gitlab, "https://git.example/a/b/catalog.git")
            .unwrap()
            .namespace,
        "a/b"
    );
    assert!(
        RepoRef::parse(
            ForgeKind::Github,
            "https://user:password@github.example/team/catalog"
        )
        .is_err()
    );
    assert!(
        RepoRef::parse(
            ForgeKind::Github,
            "https://github.example/team/catalog?token=secret"
        )
        .is_err()
    );
    assert!(RepoRef::parse(ForgeKind::Github, "https://github.example/a/b/catalog").is_err());
    assert!(
        RepoRef::parse(
            ForgeKind::Github,
            "https://github.example/team/repo%2Fother"
        )
        .is_err()
    );
}

#[cfg(feature = "github")]
#[tokio::test]
async fn verifies_webhook_bytes_before_decoding_and_checks_repository() {
    use crate::github::Github;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let repo = RepoRef::parse(ForgeKind::Github, "https://github.example/team/catalog").unwrap();
    let g = Github::from_client(repo, octocrab::Octocrab::default()).unwrap();
    // RFC 4231 HMAC-SHA256 test case 1; unknown events are ignored only after verification.
    let sig = "sha256=b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
    assert!(
        g.webhook("unknown", sig, &[0x0b; 20], b"Hi There")
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        g.webhook("unknown", sig, &[0x0b; 20], b"Hi There!"),
        Err(Error::InvalidSignature)
    ));
    let body=br#"{"repository":{"full_name":"team/catalog","html_url":"https://github.example/team/catalog"},"number":42,"action":"closed","pull_request":{"merged":true}}"#;
    let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
    mac.update(body);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    assert!(matches!(
        g.webhook("pull_request", &signature, b"secret", body)
            .unwrap(),
        Some(WebhookEvent::ChangeRequest {
            action: ChangeAction::Merged,
            number: 42,
            ..
        })
    ));
    assert!(
        g.webhook("pull_request", &signature, b"wrong", body)
            .is_err()
    );
    let spoof = br#"{"repository":{"full_name":"team/catalog","html_url":"https://evil.example/team/catalog"},"number":42,"action":"closed","pull_request":{"merged":true}}"#;
    let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
    mac.update(spoof);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    assert!(matches!(
        g.webhook("pull_request", &signature, b"secret", spoof),
        Err(Error::InvalidRepo(_))
    ));
}

#[tokio::test]
async fn merge_rejects_retarget_or_base_change_without_merging() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ForgeStub {
        change: ChangeRequest,
        merges: AtomicUsize,
    }
    #[async_trait]
    impl ChangeRequests for ForgeStub {
        async fn get_change(&self, _: u64) -> Result<ChangeRequest> {
            Ok(self.change.clone())
        }
        fn list_changes(&self) -> PageStream<'_, ChangeRequest> {
            panic!("not used")
        }
        async fn create_change(&self, _: &CreateChangeRequest) -> Result<ChangeRequest> {
            panic!("not used")
        }
        async fn update_change(&self, _: u64, _: &UpdateChangeRequest) -> Result<ChangeRequest> {
            panic!("not used")
        }
        async fn merge_change(
            &self,
            number: u64,
            head: &str,
            target: &str,
            base: &str,
        ) -> Result<String> {
            ensure_approved_change(self, number, head, target, base).await?;
            self.merges.fetch_add(1, Ordering::SeqCst);
            Ok(head.into())
        }
    }
    let mut forge = ForgeStub {
        change: ChangeRequest {
            number: 42,
            title: "change".into(),
            body: None,
            source_branch: "feature".into(),
            target_branch: "main".into(),
            head_sha: "head".into(),
            base_sha: "base".into(),
            state: ChangeState::Open,
            draft: false,
            mergeability: Mergeability::Mergeable,
            merge_commit: None,
            source_repository: None,
            author: "alice".into(),
            labels: vec![],
            url: "https://github.example/team/catalog/pull/42".into(),
        },
        merges: AtomicUsize::new(0),
    };
    forge.change.target_branch = "other".into();
    assert!(
        forge
            .merge_change(42, "head", "main", "base")
            .await
            .is_err()
    );
    forge.change.target_branch = "main".into();
    forge.change.base_sha = "new-base".into();
    assert!(
        forge
            .merge_change(42, "head", "main", "base")
            .await
            .is_err()
    );
    forge.change.base_sha = "base".into();
    forge.change.head_sha = "new-head".into();
    assert!(
        forge
            .merge_change(42, "head", "main", "base")
            .await
            .is_err()
    );
    assert_eq!(forge.merges.load(Ordering::SeqCst), 0);
    forge.change.head_sha = "head".into();
    assert_eq!(
        forge
            .merge_change(42, "head", "main", "base")
            .await
            .unwrap(),
        "head"
    );
    assert_eq!(forge.merges.load(Ordering::SeqCst), 1);
}
