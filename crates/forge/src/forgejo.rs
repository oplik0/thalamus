//! Forgejo review API adapter. Merge and policy evidence fail closed: Forgejo's
//! merge endpoint cannot atomically guard the approved base, target and policy.
use crate::*;
use forgejo_api::{Auth, Forgejo as Client, structs};
use futures_core::Stream;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::future::poll_fn;

pub struct Forgejo {
    client: Client,
    repo: RepoRef,
}

impl Forgejo {
    /// The base is the Forgejo instance root, on the repository's HTTPS host.
    pub fn new(repo: RepoRef, api_base: &str, token: String) -> Result<Self> {
        let url = Url::parse(api_base).map_err(|e| Error::InvalidInput(e.to_string()))?;
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_owned(),
        };
        if repo.forge != ForgeKind::Forgejo
            || repo.namespace.contains('/')
            || RepoRef::parse(
                ForgeKind::Forgejo,
                &format!("https://{}/{}/{}", repo.host, repo.namespace, repo.name),
            )? != repo
        {
            return Err(Error::InvalidRepo("invalid Forgejo repository".into()));
        }
        if url.scheme() != "https"
            || host != repo.host
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::InvalidInput(
                "API base must be HTTPS on the repository host".into(),
            ));
        }
        if token.is_empty() {
            return Err(Error::InvalidInput("empty Forgejo token".into()));
        }
        let client = Client::new(Auth::Token(&token), url).map_err(api_error)?;
        Ok(Self { client, repo })
    }
    pub fn client(&self) -> &Client {
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
            reviews: true,
            branch_policy_evidence: false,
            atomic_policy_guarded_merge: false,
            commit_statuses: true,
            check_runs: false,
            webhooks: true,
        }
    }
}
fn api_error(e: forgejo_api::ForgejoError) -> Error {
    Error::Malformed(format!("Forgejo API: {e}"))
}
fn index(n: u64) -> Result<i64> {
    i64::try_from(n).map_err(|_| Error::InvalidInput("invalid change number".into()))
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Malformed(format!("missing {key}")))
}
fn nested<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    v.get(key)
        .filter(|x| !x.is_null())
        .ok_or_else(|| Error::Malformed(format!("missing {key}")))
}
fn number(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Malformed(format!("missing {key}")))
}
fn optional(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn sha(s: &str) -> Result<()> {
    if matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "expected full hexadecimal commit SHA".into(),
        ))
    }
}
fn value<T: serde::Serialize>(v: T) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}
fn change(v: &Value, host: &str) -> Result<ChangeRequest> {
    let head = nested(v, "head")?;
    let base = nested(v, "base")?;
    let source_repository = match head.get("repo").filter(|v| !v.is_null()) {
        Some(repo) => {
            let url = text(repo, "html_url")?;
            let source = RepoRef::parse(ForgeKind::Forgejo, url)?;
            if !url.starts_with("https://") || source.host != host {
                return Err(Error::Malformed("source repository host mismatch".into()));
            }
            Some(source)
        }
        None => None,
    };
    let state = match text(v, "state")? {
        "open" => ChangeState::Open,
        "closed" if v.get("merged").and_then(Value::as_bool) == Some(true) => ChangeState::Merged,
        "closed" => ChangeState::Closed,
        s => return Err(Error::Malformed(format!("unknown change state {s}"))),
    };
    let labels = nested(v, "labels")?
        .as_array()
        .ok_or_else(|| Error::Malformed("invalid labels".into()))?
        .iter()
        .map(|l| text(l, "name").map(str::to_owned))
        .collect::<Result<Vec<_>>>()?;
    Ok(ChangeRequest {
        number: number(v, "number")?,
        title: text(v, "title")?.into(),
        body: optional(v, "body"),
        source_branch: text(head, "ref")?.into(),
        target_branch: text(base, "ref")?.into(),
        head_sha: text(head, "sha")?.into(),
        base_sha: text(base, "sha")?.into(),
        state,
        draft: v
            .get("draft")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::Malformed("missing draft".into()))?,
        mergeability: match v.get("mergeable").and_then(Value::as_bool) {
            Some(true) => Mergeability::Mergeable,
            Some(false) => Mergeability::Conflicting,
            None => Mergeability::Unknown,
        },
        merge_commit: optional(v, "merge_commit_sha"),
        source_repository,
        author: text(nested(v, "user")?, "login")?.into(),
        labels,
        url: text(v, "html_url")?.into(),
    })
}
fn comment(v: &Value) -> Result<Comment> {
    Ok(Comment {
        id: number(v, "id")?,
        author: text(nested(v, "user")?, "login")?.into(),
        body: text(v, "body")?.into(),
        url: text(v, "html_url")?.into(),
    })
}
fn review(v: &Value) -> Result<Review> {
    let commit_sha = optional(v, "commit_id");
    if let Some(s) = &commit_sha {
        sha(s).map_err(|_| Error::Malformed("invalid reviewed commit".into()))?;
    }
    let state = if v.get("dismissed").and_then(Value::as_bool) == Some(true) {
        "DISMISSED"
    } else if v.get("stale").and_then(Value::as_bool) == Some(true) {
        "STALE"
    } else {
        text(v, "state")?
    };
    Ok(Review {
        id: number(v, "id")?,
        author: text(nested(v, "user")?, "login")?.into(),
        state: state.into(),
        commit_sha,
        submitted_at: optional(v, "submitted_at"),
        body: optional(v, "body"),
        url: text(v, "html_url")?.into(),
    })
}
fn check(v: &Value) -> Result<Check> {
    let (status, conclusion) = match text(v, "status")? {
        "pending" => (CheckStatus::InProgress, None),
        "success" => (CheckStatus::Completed, Some(Conclusion::Success)),
        "failure" | "error" => (CheckStatus::Completed, Some(Conclusion::Failure)),
        s => return Err(Error::Malformed(format!("unknown status {s}"))),
    };
    Ok(Check {
        name: text(v, "context")?.into(),
        status,
        conclusion,
        description: optional(v, "description"),
        url: optional(v, "target_url"),
    })
}

#[async_trait]
impl ChangeRequests for Forgejo {
    async fn get_change(&self, n: u64) -> Result<ChangeRequest> {
        let v = self
            .client
            .repo_get_pull_request(&self.repo.namespace, &self.repo.name, index(n)?)
            .await
            .map_err(api_error)?;
        change(&value(v)?, &self.repo.host)
    }
    fn list_changes(&self) -> PageStream<'_, ChangeRequest> {
        Box::pin(async_stream::try_stream! {
            let query = structs::RepoListPullRequestsQuery { state: Some(structs::RepoListPullRequestsQueryState::All), ..Default::default() };
            let mut stream = Box::pin(self.client.repo_list_pull_requests(&self.repo.namespace, &self.repo.name, query).stream());
            while let Some(item) = poll_fn(|cx| Stream::poll_next(stream.as_mut(), cx)).await {
                yield change(&value(item.map_err(api_error)?)?, &self.repo.host)?;
            }
        })
    }
    async fn create_change(&self, r: &CreateChangeRequest) -> Result<ChangeRequest> {
        if r.draft {
            return Err(Error::Unsupported(
                "Forgejo API cannot create draft pull requests directly".into(),
            ));
        }
        let body: structs::CreatePullRequestOption = serde_json::from_value(
            json!({"title":r.title,"body":r.body,"head":r.source_branch,"base":r.target_branch}),
        )?;
        let v = self
            .client
            .repo_create_pull_request(&self.repo.namespace, &self.repo.name, body)
            .await
            .map_err(api_error)?;
        change(&value(v)?, &self.repo.host)
    }
    async fn update_change(&self, n: u64, r: &UpdateChangeRequest) -> Result<ChangeRequest> {
        let state = match r.state {
            Some(ChangeState::Open) => Some("open"),
            Some(ChangeState::Closed) => Some("closed"),
            Some(ChangeState::Merged) => {
                return Err(Error::InvalidInput("use merge_change to merge".into()));
            }
            None => None,
        };
        let body: structs::EditPullRequestOption = serde_json::from_value(
            json!({"title":r.title,"body":r.body,"base":r.target_branch,"state":state}),
        )?;
        let v = self
            .client
            .repo_edit_pull_request(&self.repo.namespace, &self.repo.name, index(n)?, body)
            .await
            .map_err(api_error)?;
        change(&value(v)?, &self.repo.host)
    }
    async fn merge_change(
        &self,
        _n: u64,
        _head: &str,
        _target: &str,
        _base: &str,
    ) -> Result<String> {
        Err(Error::Unsupported(
            "Forgejo cannot atomically guard approved head, base, target and policy".into(),
        ))
    }
}
#[async_trait]
impl Comments for Forgejo {
    fn list_comments(&self, n: u64) -> PageStream<'_, Comment> {
        Box::pin(async_stream::try_stream! {
            let mut stream = Box::pin(self.client.issue_get_comments(&self.repo.namespace, &self.repo.name, index(n)?, Default::default()).stream());
            while let Some(item) = poll_fn(|cx| Stream::poll_next(stream.as_mut(), cx)).await { yield comment(&value(item.map_err(api_error)?)?)?; }
        })
    }
    async fn create_comment(&self, n: u64, body: &str) -> Result<Comment> {
        let v = self
            .client
            .issue_create_comment(
                &self.repo.namespace,
                &self.repo.name,
                index(n)?,
                structs::CreateIssueCommentOption {
                    body: body.into(),
                    updated_at: None,
                },
            )
            .await
            .map_err(api_error)?;
        comment(&value(v)?)
    }
    fn list_reviews(&self, n: u64) -> PageStream<'_, Review> {
        Box::pin(async_stream::try_stream! {
            let mut stream = Box::pin(self.client.repo_list_pull_reviews(&self.repo.namespace, &self.repo.name, index(n)?).stream());
            while let Some(item) = poll_fn(|cx| Stream::poll_next(stream.as_mut(), cx)).await { yield review(&value(item.map_err(api_error)?)?)?; }
        })
    }
}
#[async_trait]
impl Checks for Forgejo {
    fn list_checks<'a>(&'a self, s: &'a str) -> PageStream<'a, Check> {
        Box::pin(async_stream::try_stream! {
            sha(s)?;
            let mut stream = Box::pin(self.client.repo_list_statuses_by_ref(&self.repo.namespace, &self.repo.name, s, Default::default()).stream());
            while let Some(item) = poll_fn(|cx| Stream::poll_next(stream.as_mut(), cx)).await { yield check(&value(item.map_err(api_error)?)?)?; }
        })
    }
    async fn combined_state(&self, s: &str) -> Result<CombinedState> {
        sha(s)?;
        let (_, result) = self
            .client
            .repo_get_combined_status_by_ref(&self.repo.namespace, &self.repo.name, s)
            .await
            .map_err(api_error)?;
        let v = value(result)?;
        if number(&v, "total_count")? == 0 {
            return Ok(CombinedState::Pending);
        }
        match text(&v, "state")? {
            "success" => Ok(CombinedState::Success),
            "pending" => Ok(CombinedState::Pending),
            "failure" | "error" => Ok(CombinedState::Failure),
            s => Err(Error::Malformed(format!("unknown combined state {s}"))),
        }
    }
}
#[async_trait]
impl BranchPolicies for Forgejo {
    async fn branch_policy(&self, _branch: &str) -> Result<BranchPolicy> {
        Err(Error::Unsupported(
            "complete Forgejo policy evidence is unavailable".into(),
        ))
    }
    async fn policy_actor(&self) -> Result<PolicyActor> {
        Err(Error::Unsupported(
            "Forgejo bypass actor evidence is unavailable".into(),
        ))
    }
}
impl Webhooks for Forgejo {
    fn webhook(
        &self,
        event: &str,
        signature: &str,
        secret: &[u8],
        body: &[u8],
    ) -> Result<Option<WebhookEvent>> {
        if secret.is_empty() {
            return Err(Error::InvalidInput("empty webhook secret".into()));
        }
        let signature = hex::decode(signature).map_err(|_| Error::InvalidSignature)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret).map_err(|_| Error::InvalidSignature)?;
        mac.update(body);
        mac.verify_slice(&signature)
            .map_err(|_| Error::InvalidSignature)?;
        if !matches!(event, "pull_request" | "push") {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(body)?;
        let repo = nested(&v, "repository")?;
        let url = text(repo, "html_url")?;
        let observed = RepoRef::parse(ForgeKind::Forgejo, url)?;
        if !url.starts_with("https://")
            || observed != self.repo
            || text(repo, "full_name")? != format!("{}/{}", self.repo.namespace, self.repo.name)
        {
            return Err(Error::InvalidRepo(
                "webhook repository does not match configured repository".into(),
            ));
        }
        match event {
            "push" => Ok(Some(WebhookEvent::Push {
                repository: observed,
                reference: text(&v, "ref")?.into(),
                after: text(&v, "after")?.into(),
            })),
            "pull_request" => {
                let action = match text(&v, "action")? {
                    "opened" | "reopened" => ChangeAction::Opened,
                    "closed"
                        if nested(&v, "pull_request")?
                            .get("merged")
                            .and_then(Value::as_bool)
                            == Some(true) =>
                    {
                        ChangeAction::Merged
                    }
                    "closed" => ChangeAction::Closed,
                    "edited" | "synchronized" | "synchronize" | "label_updated"
                    | "label_cleared" => ChangeAction::Updated,
                    _ => return Ok(None),
                };
                Ok(Some(WebhookEvent::ChangeRequest {
                    action,
                    repository: observed,
                    number: number(&v, "number")?,
                }))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn change_preserves_reviewed_refs() {
        let v = json!({"number":7,"title":"test","body":null,"state":"open","draft":false,"mergeable":null,
            "head":{"ref":"proposal","sha":"head","repo":{"html_url":"https://forge.example/alice/repo"}},
            "base":{"ref":"main","sha":"base"},"user":{"login":"alice"},"labels":[],"html_url":"https://forge.example/team/repo/pulls/7"});
        let c = change(&v, "forge.example").unwrap();
        assert_eq!(
            (
                c.head_sha.as_str(),
                c.base_sha.as_str(),
                c.target_branch.as_str()
            ),
            ("head", "base", "main")
        );
        assert_eq!(c.source_repository.unwrap().namespace, "alice");
        assert!(change(&v, "other.example").is_err());
    }
    #[test]
    fn webhook_authenticates_raw_payload_and_origin() {
        let repo = RepoRef::parse(ForgeKind::Forgejo, "https://forge.example/team/repo").unwrap();
        let adapter = Forgejo::new(repo, "https://forge.example/", "token".into()).unwrap();
        let body = br#"{"repository":{"html_url":"https://forge.example/team/repo","full_name":"team/repo"},"ref":"refs/heads/main","after":"abc"}"#.to_vec();
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(&body);
        let sig = hex::encode(mac.finalize().into_bytes());
        assert!(matches!(
            adapter.webhook("push", &sig, b"secret", &body).unwrap(),
            Some(WebhookEvent::Push { .. })
        ));
        assert!(adapter.webhook("push", &sig, b"wrong", &body).is_err());
        let mut altered = body.clone();
        altered.push(b' ');
        assert!(adapter.webhook("push", &sig, b"secret", &altered).is_err());
        let other = Forgejo::new(
            RepoRef::parse(ForgeKind::Forgejo, "https://forge.example/team/other").unwrap(),
            "https://forge.example/",
            "token".into(),
        )
        .unwrap();
        assert!(other.webhook("push", &sig, b"secret", &body).is_err());
    }
    #[test]
    fn unknown_status_is_not_success() {
        assert!(check(&json!({"context":"ci","status":"unknown"})).is_err());
        assert!(matches!(
            check(&json!({"context":"ci","status":"error"}))
                .unwrap()
                .conclusion,
            Some(Conclusion::Failure)
        ));
    }
}
