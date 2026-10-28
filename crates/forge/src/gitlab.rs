//! GitLab REST adapter, including self-hosted HTTPS installations.
use crate::*;
use ::gitlab::api::projects::{self, merge_requests as mr, repository::commits};
use ::gitlab::api::{self, AsyncQuery};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

pub struct Gitlab {
    client: ::gitlab::AsyncGitlab,
    repo: RepoRef,
    project: String,
}

fn api_error(e: impl std::fmt::Display) -> Error {
    Error::Malformed(format!("GitLab API: {e}"))
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Malformed(format!("missing or invalid {key}")))
}
fn number(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Malformed(format!("missing or invalid {key}")))
}
fn nested<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    v.get(key)
        .filter(|v| !v.is_null())
        .ok_or_else(|| Error::Malformed(format!("missing {key}")))
}
fn optional(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn sha(s: &str) -> Result<()> {
    if (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "expected full hexadecimal commit SHA".into(),
        ))
    }
}
impl Gitlab {
    /// `host` comes from the repository and is always contacted over HTTPS.
    pub async fn new(repo: RepoRef, token: String) -> Result<Self> {
        validate_repo(&repo)?;
        if token.is_empty() {
            return Err(Error::InvalidInput("empty GitLab token".into()));
        }
        let client = ::gitlab::GitlabBuilder::new(repo.host.as_str(), token)
            .build_async()
            .await
            .map_err(api_error)?;
        Self::from_client(repo, client)
    }
    /// The caller must ensure this client targets the repository's HTTPS host.
    pub fn from_client(repo: RepoRef, client: ::gitlab::AsyncGitlab) -> Result<Self> {
        validate_repo(&repo)?;
        let project = format!("{}/{}", repo.namespace, repo.name);
        Ok(Self {
            client,
            repo,
            project,
        })
    }
    pub fn client(&self) -> &::gitlab::AsyncGitlab {
        &self.client
    }
    pub fn repository(&self) -> &RepoRef {
        &self.repo
    }
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            change_requests: true,
            merge: false,
            comments: true,
            reviews: false,
            branch_policy_evidence: false,
            atomic_policy_guarded_merge: false,
            commit_statuses: true,
            check_runs: false,
            webhooks: true,
        }
    }
    async fn convert_change(&self, v: &Value) -> Result<ChangeRequest> {
        let source_repository = match v.get("source_project_id") {
            Some(Value::Null) => None,
            Some(id) => {
                let id = id
                    .as_u64()
                    .ok_or_else(|| Error::Malformed("invalid source_project_id".into()))?;
                let endpoint = projects::Project::builder()
                    .project(id)
                    .build()
                    .map_err(api_error)?;
                let project: Value = endpoint
                    .query_async(&self.client)
                    .await
                    .map_err(api_error)?;
                let url = text(&project, "web_url")?;
                let parsed = RepoRef::parse(ForgeKind::Gitlab, url)?;
                if Url::parse(url)
                    .map_err(|e| Error::InvalidRepo(e.to_string()))?
                    .scheme()
                    != "https"
                    || parsed.host != self.repo.host
                    || text(&project, "path_with_namespace")?
                        != format!("{}/{}", parsed.namespace, parsed.name)
                {
                    return Err(Error::InvalidRepo(
                        "source project host or path mismatch".into(),
                    ));
                }
                Some(parsed)
            }
            None => return Err(Error::Malformed("missing source_project_id".into())),
        };
        change(v, source_repository)
    }
}
fn validate_repo(repo: &RepoRef) -> Result<()> {
    let parsed = RepoRef::parse(
        ForgeKind::Gitlab,
        &format!("https://{}/{}/{}", repo.host, repo.namespace, repo.name),
    )?;
    if repo.forge != ForgeKind::Gitlab || parsed != *repo {
        return Err(Error::InvalidRepo("invalid GitLab repository".into()));
    }
    Ok(())
}
fn change(v: &Value, source_repository: Option<RepoRef>) -> Result<ChangeRequest> {
    let state = match text(v, "state")? {
        "opened" => ChangeState::Open,
        "closed" => ChangeState::Closed,
        "merged" => ChangeState::Merged,
        s => return Err(Error::Malformed(format!("unknown merge request state {s}"))),
    };
    let refs = nested(v, "diff_refs")?;
    let labels = nested(v, "labels")?
        .as_array()
        .ok_or_else(|| Error::Malformed("invalid labels".into()))?
        .iter()
        .map(|x| {
            x.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::Malformed("invalid label".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let head_sha = text(refs, "head_sha")?;
    let base_sha = text(refs, "start_sha")?;
    sha(head_sha).map_err(|_| Error::Malformed("invalid head SHA".into()))?;
    sha(base_sha).map_err(|_| Error::Malformed("invalid target SHA".into()))?;
    if text(v, "sha")? != head_sha {
        return Err(Error::Malformed(
            "merge request head and diff head disagree".into(),
        ));
    }
    Ok(ChangeRequest {
        number: number(v, "iid")?,
        title: text(v, "title")?.into(),
        body: optional(v, "description"),
        source_branch: text(v, "source_branch")?.into(),
        target_branch: text(v, "target_branch")?.into(),
        head_sha: head_sha.into(),
        base_sha: base_sha.into(),
        state,
        draft: v
            .get("draft")
            .or_else(|| v.get("work_in_progress"))
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::Malformed("missing draft state".into()))?,
        mergeability: match v.get("has_conflicts").and_then(Value::as_bool) {
            Some(true) => Mergeability::Conflicting,
            Some(false)
                if v.get("merge_status").and_then(Value::as_str) == Some("can_be_merged") =>
            {
                Mergeability::Mergeable
            }
            _ => Mergeability::Unknown,
        },
        merge_commit: optional(v, "merge_commit_sha"),
        source_repository,
        author: text(nested(v, "author")?, "username")?.into(),
        labels,
        url: text(v, "web_url")?.into(),
    })
}
fn check(v: &Value) -> Result<Check> {
    let (status, conclusion) = match text(v, "status")? {
        "created" | "pending" | "waiting_for_resource" | "preparing" | "scheduled" => {
            (CheckStatus::Queued, None)
        }
        "running" => (CheckStatus::InProgress, None),
        "success" => (CheckStatus::Completed, Some(Conclusion::Success)),
        "failed" => (CheckStatus::Completed, Some(Conclusion::Failure)),
        "canceled" => (CheckStatus::Completed, Some(Conclusion::Cancelled)),
        "skipped" => (CheckStatus::Completed, Some(Conclusion::Skipped)),
        "manual" => (CheckStatus::Completed, Some(Conclusion::ActionRequired)),
        s => return Err(Error::Malformed(format!("unknown GitLab status {s}"))),
    };
    Ok(Check {
        name: text(v, "name")?.into(),
        status,
        conclusion,
        description: optional(v, "description"),
        url: optional(v, "target_url"),
    })
}
fn comment(v: &Value, repo: &RepoRef, mr: u64) -> Result<Comment> {
    let id = number(v, "id")?;
    Ok(Comment {
        id,
        author: text(nested(v, "author")?, "username")?.into(),
        body: text(v, "body")?.into(),
        url: format!(
            "https://{}/{}/{}/-/merge_requests/{mr}#note_{id}",
            repo.host, repo.namespace, repo.name
        ),
    })
}
#[async_trait]
impl ChangeRequests for Gitlab {
    async fn get_change(&self, n: u64) -> Result<ChangeRequest> {
        let endpoint = mr::MergeRequest::builder()
            .project(self.project.as_str())
            .merge_request(n)
            .build()
            .map_err(api_error)?;
        let v: Value = endpoint
            .query_async(&self.client)
            .await
            .map_err(api_error)?;
        self.convert_change(&v).await
    }
    fn list_changes(&self) -> PageStream<'_, ChangeRequest> {
        Box::pin(async_stream::try_stream! {
            let endpoint = mr::MergeRequests::builder().project(self.project.as_str()).scope(mr::MergeRequestScope::All).build().map_err(api_error)?;
            let values: Vec<Value> = api::paged(endpoint, api::Pagination::All).query_async(&self.client).await.map_err(api_error)?;
            // List responses may omit diff_refs; fetch each complete request.
            for v in values { yield self.get_change(number(&v, "iid")?).await?; }
        })
    }
    async fn create_change(&self, r: &CreateChangeRequest) -> Result<ChangeRequest> {
        let title = if r.draft && !r.title.starts_with("Draft:") {
            format!("Draft: {}", r.title)
        } else {
            r.title.clone()
        };
        let mut builder = mr::CreateMergeRequest::builder();
        builder
            .project(self.project.as_str())
            .source_branch(r.source_branch.as_str())
            .target_branch(r.target_branch.as_str())
            .title(title.as_str());
        if let Some(body) = &r.body {
            builder.description(body.as_str());
        }
        let endpoint = builder.build().map_err(api_error)?;
        let v: Value = endpoint
            .query_async(&self.client)
            .await
            .map_err(api_error)?;
        self.get_change(number(&v, "iid")?).await
    }
    async fn update_change(&self, n: u64, r: &UpdateChangeRequest) -> Result<ChangeRequest> {
        let mut builder = mr::EditMergeRequest::builder();
        builder.project(self.project.as_str()).merge_request(n);
        if let Some(x) = &r.title {
            builder.title(x.as_str());
        }
        if let Some(x) = &r.body {
            builder.description(x.as_str());
        }
        if let Some(x) = &r.target_branch {
            builder.target_branch(x.as_str());
        }
        if let Some(state) = r.state {
            builder.state_event(match state {
                ChangeState::Open => mr::MergeRequestStateEvent::Reopen,
                ChangeState::Closed => mr::MergeRequestStateEvent::Close,
                ChangeState::Merged => {
                    return Err(Error::InvalidInput("use merge_change to merge".into()));
                }
            });
        }
        let endpoint = builder.build().map_err(api_error)?;
        let _: Value = endpoint
            .query_async(&self.client)
            .await
            .map_err(api_error)?;
        self.get_change(n).await
    }
    async fn merge_change(&self, _: u64, _: &str, _: &str, _: &str) -> Result<String> {
        Err(Error::Unsupported(
            "GitLab cannot atomically guard approved target and base at merge".into(),
        ))
    }
}
#[async_trait]
impl Checks for Gitlab {
    fn list_checks<'a>(&'a self, s: &'a str) -> PageStream<'a, Check> {
        Box::pin(async_stream::try_stream! {
            sha(s)?;
            let endpoint = commits::CommitStatuses::builder().project(self.project.as_str()).commit(s).build().map_err(api_error)?;
            let values: Vec<Value> = api::paged(endpoint, api::Pagination::All).query_async(&self.client).await.map_err(api_error)?;
            for v in values { yield check(&v)?; }
        })
    }
    async fn combined_state(&self, s: &str) -> Result<CombinedState> {
        use futures_core::Stream;
        use std::future::poll_fn;
        let mut checks = self.list_checks(s);
        let mut seen = false;
        let mut pending = false;
        while let Some(item) = poll_fn(|cx| Stream::poll_next(checks.as_mut(), cx)).await {
            let c = item?;
            seen = true;
            match (c.status, c.conclusion) {
                (
                    CheckStatus::Completed,
                    Some(Conclusion::Success | Conclusion::Neutral | Conclusion::Skipped),
                ) => (),
                (CheckStatus::Completed, Some(_)) => return Ok(CombinedState::Failure),
                _ => pending = true,
            }
        }
        Ok(if !seen || pending {
            CombinedState::Pending
        } else {
            CombinedState::Success
        })
    }
}
#[async_trait]
impl Comments for Gitlab {
    fn list_comments(&self, n: u64) -> PageStream<'_, Comment> {
        Box::pin(async_stream::try_stream! {
            let endpoint = mr::notes::MergeRequestNotes::builder().project(self.project.as_str()).merge_request(n).build().map_err(api_error)?;
            let values: Vec<Value> = api::paged(endpoint, api::Pagination::All).query_async(&self.client).await.map_err(api_error)?;
            for v in values { yield comment(&v, &self.repo, n)?; }
        })
    }
    async fn create_comment(&self, n: u64, body: &str) -> Result<Comment> {
        let endpoint = mr::notes::CreateMergeRequestNote::builder()
            .project(self.project.as_str())
            .merge_request(n)
            .body(body)
            .build()
            .map_err(api_error)?;
        let v: Value = endpoint
            .query_async(&self.client)
            .await
            .map_err(api_error)?;
        comment(&v, &self.repo, n)
    }
    fn list_reviews(&self, _: u64) -> PageStream<'_, Review> {
        Box::pin(
            async_stream::stream! { yield Err(Error::Unsupported("GitLab approvals do not provide commit-bound review submissions".into())); },
        )
    }
}
#[async_trait]
impl BranchPolicies for Gitlab {
    async fn branch_policy(&self, _: &str) -> Result<BranchPolicy> {
        Err(Error::Unsupported(
            "GitLab branch policy evidence is not implemented".into(),
        ))
    }
    async fn policy_actor(&self) -> Result<PolicyActor> {
        Err(Error::Unsupported(
            "GitLab bypass permissions cannot be established".into(),
        ))
    }
}
impl Webhooks for Gitlab {
    fn webhook(
        &self,
        event: &str,
        signature: &str,
        secret: &[u8],
        body: &[u8],
    ) -> Result<Option<WebhookEvent>> {
        parse_webhook(&self.repo, event, signature, secret, body)
    }
}
fn parse_webhook(
    repo: &RepoRef,
    event: &str,
    signature: &str,
    secret: &[u8],
    body: &[u8],
) -> Result<Option<WebhookEvent>> {
    if secret.is_empty() {
        return Err(Error::InvalidInput("empty webhook secret".into()));
    }
    // GitLab's X-Gitlab-Token is a shared secret, not a body signature. Compare
    // HMACs to avoid secret-dependent comparison timing.
    let mut expected =
        Hmac::<Sha256>::new_from_slice(secret).map_err(|_| Error::InvalidSignature)?;
    expected.update(b"gitlab webhook token");
    let tag = expected.finalize().into_bytes();
    let mut supplied = Hmac::<Sha256>::new_from_slice(signature.as_bytes())
        .map_err(|_| Error::InvalidSignature)?;
    supplied.update(b"gitlab webhook token");
    supplied
        .verify_slice(&tag)
        .map_err(|_| Error::InvalidSignature)?;
    if !matches!(
        event,
        "Merge Request Hook" | "Push Hook" | "Pipeline Hook" | "Job Hook"
    ) {
        return Ok(None);
    }
    let v: Value = serde_json::from_slice(body)?;
    let project = nested(&v, "project")?;
    let web_url = text(project, "web_url")?;
    let url = Url::parse(web_url).map_err(|e| Error::InvalidRepo(e.to_string()))?;
    if url.scheme() != "https" {
        return Err(Error::InvalidRepo("webhook project must use HTTPS".into()));
    }
    let repository = RepoRef::parse(ForgeKind::Gitlab, web_url)?;
    if repository != *repo
        || text(project, "path_with_namespace")? != format!("{}/{}", repo.namespace, repo.name)
    {
        return Err(Error::InvalidRepo(
            "webhook project does not match configured repository".into(),
        ));
    }
    Ok(match event {
        "Merge Request Hook" => {
            let attrs = nested(&v, "object_attributes")?;
            let action = match text(attrs, "action")? {
                "open" | "reopen" => ChangeAction::Opened,
                "update" | "approved" | "unapproved" | "approval" | "unapproval" => {
                    ChangeAction::Updated
                }
                "merge" => ChangeAction::Merged,
                "close" => ChangeAction::Closed,
                _ => return Ok(None),
            };
            Some(WebhookEvent::ChangeRequest {
                action,
                repository,
                number: number(attrs, "iid")?,
            })
        }
        "Push Hook" => Some(WebhookEvent::Push {
            repository,
            reference: text(&v, "ref")?.into(),
            after: text(&v, "after")?.into(),
        }),
        "Pipeline Hook" => Some(WebhookEvent::CheckUpdated {
            repository,
            sha: text(nested(&v, "object_attributes")?, "sha")?.into(),
        }),
        "Job Hook" => Some(WebhookEvent::CheckUpdated {
            repository,
            sha: text(nested(&v, "commit")?, "id")?.into(),
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn status_conclusions_are_not_in_progress() {
        let c = check(&json!({"name":"build","status":"failed"})).unwrap();
        assert!(matches!(c.status, CheckStatus::Completed));
        assert!(matches!(c.conclusion, Some(Conclusion::Failure)));
        let c = check(&json!({"name":"build","status":"running"})).unwrap();
        assert!(matches!(c.status, CheckStatus::InProgress));
        assert!(c.conclusion.is_none());
        assert!(check(&json!({"name":"build","status":"unknown"})).is_err());
    }
    #[test]
    fn merge_request_uses_target_start_sha_not_merge_base() {
        let head = "a".repeat(40);
        let target = "b".repeat(40);
        let base = "c".repeat(40);
        let v = json!({"iid":4,"title":"change","description":null,"state":"opened","source_branch":"feature","target_branch":"main","sha":head,"diff_refs":{"head_sha":head,"start_sha":target,"base_sha":base},"draft":false,"has_conflicts":false,"merge_status":"can_be_merged","author":{"username":"alice"},"labels":["review"],"web_url":"https://git.example/group/sub/project/-/merge_requests/4"});
        let c = change(&v, None).unwrap();
        assert_eq!(c.base_sha, target);
        assert_eq!(c.head_sha, head);
        assert_eq!(c.labels, vec!["review"]);
        let mut bad = v;
        bad["sha"] = json!(base);
        assert!(change(&bad, None).is_err());
    }
    #[test]
    fn validates_nested_namespace_and_rejects_forged_repo() {
        let repo =
            RepoRef::parse(ForgeKind::Gitlab, "https://git.example/group/sub/project").unwrap();
        assert!(validate_repo(&repo).is_ok());
        let mut bad = repo;
        bad.host = "attacker.example/path".into();
        assert!(validate_repo(&bad).is_err());
    }
    #[test]
    fn webhook_authenticates_and_binds_project() {
        let repo =
            RepoRef::parse(ForgeKind::Gitlab, "https://git.example/group/sub/project").unwrap();
        let mut payload = json!({
            "project": {"web_url":"https://git.example/group/sub/project","path_with_namespace":"group/sub/project"},
            "object_attributes": {"iid": 9, "action":"merge"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        assert!(matches!(
            parse_webhook(&repo, "Merge Request Hook", "secret", b"secret", &body).unwrap(),
            Some(WebhookEvent::ChangeRequest {
                action: ChangeAction::Merged,
                number: 9,
                ..
            })
        ));
        assert!(matches!(
            parse_webhook(&repo, "Merge Request Hook", "wrong", b"secret", &body),
            Err(Error::InvalidSignature)
        ));
        payload["project"]["web_url"] = json!("https://evil.example/group/sub/project");
        let body = serde_json::to_vec(&payload).unwrap();
        assert!(matches!(
            parse_webhook(&repo, "Merge Request Hook", "secret", b"secret", &body),
            Err(Error::InvalidRepo(_))
        ));
        assert!(matches!(
            parse_webhook(&repo, "unhandled", "wrong", b"secret", b"not json"),
            Err(Error::InvalidSignature)
        ));
    }
}
