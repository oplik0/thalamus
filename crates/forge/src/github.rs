//! GitHub.com and GitHub Enterprise Server review API adapter.
use crate::*;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub struct Github {
    client: octocrab::Octocrab,
    repo: RepoRef,
}
impl Github {
    /// `api_base` is the actual GitHub API root, e.g. https://github.example.com/api/v3.
    /// HTTPS is required; credentials never travel to a different repository host.
    pub fn new(repo: RepoRef, api_base: &str, token: String) -> Result<Self> {
        let u = Url::parse(api_base).map_err(|e| Error::InvalidInput(e.to_string()))?;
        let host = match u.port() {
            Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
            None => u.host_str().unwrap_or_default().to_owned(),
        };
        if u.scheme() != "https"
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || !(host == repo.host || (repo.host == "github.com" && host == "api.github.com"))
        {
            return Err(Error::InvalidInput(
                "API base must be HTTPS on the repository host".into(),
            ));
        }
        if token.is_empty() {
            return Err(Error::InvalidInput("empty GitHub token".into()));
        }
        let client = octocrab::Octocrab::builder()
            .base_uri(api_base)?
            .personal_token(token)
            .build()?;
        Self::from_client(repo, client)
    }
    /// Accept a preconfigured client, including GitHub App installation authentication.
    /// The caller is responsible for binding its base URL to the intended host.
    pub fn from_client(repo: RepoRef, client: octocrab::Octocrab) -> Result<Self> {
        let parsed = RepoRef::parse(
            ForgeKind::Github,
            &format!("https://{}/{}/{}", repo.host, repo.namespace, repo.name),
        )?;
        if repo.forge != ForgeKind::Github || parsed != repo {
            return Err(Error::InvalidRepo("invalid GitHub repository".into()));
        }
        Ok(Self { client, repo })
    }
    pub fn client(&self) -> &octocrab::Octocrab {
        &self.client
    }
    pub fn repository(&self) -> &RepoRef {
        &self.repo
    }
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            change_requests: true,
            merge: true,
            comments: true,
            reviews: true,
            commit_statuses: true,
            check_runs: true,
            webhooks: true,
        }
    }
    fn route(&self, path: &str) -> String {
        format!("/repos/{}/{}{}", self.repo.namespace, self.repo.name, path)
    }
    async fn get(&self, route: &str) -> Result<Value> {
        Ok(self.client.get(route, None::<&()>).await?)
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Malformed(format!("missing {key}")))
}
fn number(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Malformed(format!("missing {key}")))
}
fn nested<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    v.get(key)
        .filter(|v| !v.is_null())
        .ok_or_else(|| Error::Malformed(format!("missing {key}")))
}
fn optional(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn change(v: &Value, host: &str) -> Result<ChangeRequest> {
    let head = nested(v, "head")?;
    let base = nested(v, "base")?;
    let state = match text(v, "state")? {
        "open" => ChangeState::Open,
        "closed"
            if v.get("merged").and_then(Value::as_bool).unwrap_or(false)
                || v.get("merged_at").is_some_and(|x| !x.is_null()) =>
        {
            ChangeState::Merged
        }
        "closed" => ChangeState::Closed,
        s => return Err(Error::Malformed(format!("unknown change state {s}"))),
    };
    let source_repository = match head.get("repo").filter(|v| !v.is_null()) {
        Some(r) => {
            let owner = text(nested(r, "owner")?, "login")?;
            Some(RepoRef::parse(
                ForgeKind::Github,
                &format!("https://{host}/{owner}/{}", text(r, "name")?),
            )?)
        }
        None => None,
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
        draft: v.get("draft").and_then(Value::as_bool).unwrap_or(false),
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
    Ok(Review {
        id: number(v, "id")?,
        author: text(nested(v, "user")?, "login")?.into(),
        state: text(v, "state")?.into(),
        body: optional(v, "body"),
        url: text(v, "html_url")?.into(),
    })
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
fn check(v: &Value, run: bool) -> Result<Check> {
    let (name, status, conclusion, description, url) = if run {
        let status = match text(v, "status")? {
            "queued" | "waiting" | "pending" | "requested" => CheckStatus::Queued,
            "in_progress" => CheckStatus::InProgress,
            "completed" => CheckStatus::Completed,
            s => return Err(Error::Malformed(format!("unknown check status {s}"))),
        };
        let conclusion = match v.get("conclusion").and_then(Value::as_str) {
            None => None,
            Some("success") => Some(Conclusion::Success),
            Some("failure" | "startup_failure" | "stale") => Some(Conclusion::Failure),
            Some("neutral") => Some(Conclusion::Neutral),
            Some("cancelled") => Some(Conclusion::Cancelled),
            Some("skipped") => Some(Conclusion::Skipped),
            Some("timed_out") => Some(Conclusion::TimedOut),
            Some("action_required") => Some(Conclusion::ActionRequired),
            Some(s) => return Err(Error::Malformed(format!("unknown conclusion {s}"))),
        };
        (
            text(v, "name")?,
            status,
            conclusion,
            v.get("output").and_then(|o| optional(o, "summary")),
            optional(v, "html_url"),
        )
    } else {
        let (status, conclusion) = match text(v, "state")? {
            "pending" => (CheckStatus::InProgress, None),
            "success" => (CheckStatus::Completed, Some(Conclusion::Success)),
            "failure" | "error" => (CheckStatus::Completed, Some(Conclusion::Failure)),
            s => return Err(Error::Malformed(format!("unknown status {s}"))),
        };
        (
            text(v, "context")?,
            status,
            conclusion,
            optional(v, "description"),
            optional(v, "target_url"),
        )
    };
    Ok(Check {
        name: name.into(),
        status,
        conclusion,
        description,
        url,
    })
}
// Page numbers rather than server-supplied links prevent credential forwarding to arbitrary hosts.
fn pages<'a, T: Send + 'a>(
    github: &'a Github,
    path: String,
    field: Option<&'static str>,
    convert: impl Fn(&Value) -> Result<T> + Send + 'a,
) -> PageStream<'a, T> {
    Box::pin(async_stream::try_stream! {
        let mut page = 1u64;
        loop {
            let sep = if path.contains('?') { '&' } else { '?' };
            let value = github.get(&format!("{path}{sep}per_page=100&page={page}")).await?;
            let items = match field { Some(f) => nested(&value,f)?, None => &value }.as_array().ok_or_else(|| Error::Malformed("expected page array".into()))?;
            for item in items { yield convert(item)?; }
            if items.len() < 100 { break; }
            page = page.checked_add(1).ok_or_else(|| Error::Malformed("pagination overflow".into()))?;
        }
    })
}
#[async_trait]
impl ChangeRequests for Github {
    async fn get_change(&self, n: u64) -> Result<ChangeRequest> {
        change(
            &self.get(&self.route(&format!("/pulls/{n}"))).await?,
            &self.repo.host,
        )
    }
    fn list_changes(&self) -> PageStream<'_, ChangeRequest> {
        pages(self, self.route("/pulls?state=all"), None, |v| {
            change(v, &self.repo.host)
        })
    }
    async fn create_change(&self, r: &CreateChangeRequest) -> Result<ChangeRequest> {
        let v:Value=self.client.post(self.route("/pulls"),Some(&json!({"title":r.title,"body":r.body,"head":r.source_branch,"base":r.target_branch,"draft":r.draft}))).await?;
        change(&v, &self.repo.host)
    }
    async fn update_change(&self, n: u64, r: &UpdateChangeRequest) -> Result<ChangeRequest> {
        let mut b = serde_json::Map::new();
        if let Some(v) = &r.title {
            b.insert("title".into(), json!(v));
        }
        if let Some(v) = &r.body {
            b.insert("body".into(), json!(v));
        }
        if let Some(v) = &r.target_branch {
            b.insert("base".into(), json!(v));
        }
        if let Some(s) = r.state {
            b.insert(
                "state".into(),
                json!(match s {
                    ChangeState::Open => "open",
                    ChangeState::Closed => "closed",
                    ChangeState::Merged =>
                        return Err(Error::InvalidInput("use merge_change to merge".into())),
                }),
            );
        }
        let v: Value = self
            .client
            .patch(self.route(&format!("/pulls/{n}")), Some(&b))
            .await?;
        change(&v, &self.repo.host)
    }
    async fn merge_change(&self, n: u64, s: &str, target: &str, base: &str) -> Result<String> {
        sha(s)?;
        ensure_approved_change(self, n, s, target, base).await?;
        let v: Value = self
            .client
            .put(
                self.route(&format!("/pulls/{n}/merge")),
                Some(&json!({"sha":s})),
            )
            .await?;
        if v.get("merged").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Malformed("merge was not performed".into()));
        }
        Ok(text(&v, "sha")?.into())
    }
}
#[async_trait]
impl Comments for Github {
    fn list_comments(&self, n: u64) -> PageStream<'_, Comment> {
        pages(
            self,
            self.route(&format!("/issues/{n}/comments")),
            None,
            comment,
        )
    }
    async fn create_comment(&self, n: u64, b: &str) -> Result<Comment> {
        let v: Value = self
            .client
            .post(
                self.route(&format!("/issues/{n}/comments")),
                Some(&json!({"body":b})),
            )
            .await?;
        comment(&v)
    }
    fn list_reviews(&self, n: u64) -> PageStream<'_, Review> {
        pages(
            self,
            self.route(&format!("/pulls/{n}/reviews")),
            None,
            review,
        )
    }
}
#[async_trait]
impl Checks for Github {
    fn list_checks<'a>(&'a self, s: &'a str) -> PageStream<'a, Check> {
        Box::pin(async_stream::try_stream! {
            use futures_core::Stream;
            use std::future::poll_fn;
            sha(s)?;
            for (path,field,run) in [(format!("/commits/{s}/statuses"),None,false),(format!("/commits/{s}/check-runs"),Some("check_runs"),true)] {
                let mut stream=pages(self,self.route(&path),field,move |v| check(v,run));
                while let Some(item)=poll_fn(|cx| Stream::poll_next(stream.as_mut(),cx)).await { yield item?; }
            }
        })
    }
    async fn combined_state(&self, s: &str) -> Result<CombinedState> {
        use futures_core::Stream;
        use std::future::poll_fn;
        sha(s)?;
        let v = self
            .get(&self.route(&format!("/commits/{s}/status")))
            .await?;
        let count = number(&v, "total_count")?;
        let mut seen = count > 0;
        let mut pending = false;
        if seen {
            match text(&v, "state")? {
                "failure" => return Ok(CombinedState::Failure),
                "pending" => pending = true,
                "success" => (),
                state => return Err(Error::Malformed(format!("unknown combined state {state}"))),
            }
        }
        let mut runs = pages(
            self,
            self.route(&format!("/commits/{s}/check-runs")),
            Some("check_runs"),
            |v| check(v, true),
        );
        while let Some(item) = poll_fn(|cx| Stream::poll_next(runs.as_mut(), cx)).await {
            let c = item?;
            seen = true;
            match (c.status, c.conclusion) {
                (
                    CheckStatus::Completed,
                    Some(
                        Conclusion::Failure
                        | Conclusion::Cancelled
                        | Conclusion::TimedOut
                        | Conclusion::ActionRequired,
                    ),
                ) => return Ok(CombinedState::Failure),
                (
                    CheckStatus::Completed,
                    Some(Conclusion::Success | Conclusion::Neutral | Conclusion::Skipped),
                ) => (),
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
impl Webhooks for Github {
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
        let encoded = signature
            .strip_prefix("sha256=")
            .ok_or(Error::InvalidSignature)?;
        let sig = hex::decode(encoded).map_err(|_| Error::InvalidSignature)?;
        let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| Error::InvalidSignature)?;
        mac.update(body);
        mac.verify_slice(&sig)
            .map_err(|_| Error::InvalidSignature)?;
        if !matches!(event, "pull_request" | "push" | "check_run" | "status") {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(body)?;
        let repo = nested(&v, "repository")?;
        let full = text(repo, "full_name")?;
        let repository_url = text(repo, "html_url")?;
        let url = Url::parse(repository_url).map_err(|e| Error::InvalidRepo(e.to_string()))?;
        if url.scheme() != "https" {
            return Err(Error::InvalidRepo(
                "webhook repository URL must use HTTPS".into(),
            ));
        }
        let observed = RepoRef::parse(ForgeKind::Github, repository_url)?;
        if observed != self.repo || full != format!("{}/{}", observed.namespace, observed.name) {
            return Err(Error::InvalidRepo(
                "webhook repository does not match configured repository".into(),
            ));
        }
        match event {
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
                    "edited" | "synchronize" | "ready_for_review" | "converted_to_draft"
                    | "labeled" | "unlabeled" => ChangeAction::Updated,
                    _ => return Ok(None),
                };
                Ok(Some(WebhookEvent::ChangeRequest {
                    action,
                    repository: observed,
                    number: number(&v, "number")?,
                }))
            }
            "push" => Ok(Some(WebhookEvent::Push {
                repository: observed,
                reference: text(&v, "ref")?.into(),
                after: text(&v, "after")?.into(),
            })),
            "check_run" => Ok(Some(WebhookEvent::CheckUpdated {
                repository: observed,
                sha: text(nested(&v, "check_run")?, "head_sha")?.into(),
            })),
            "status" => Ok(Some(WebhookEvent::CheckUpdated {
                repository: observed,
                sha: text(&v, "sha")?.into(),
            })),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_example_pull_and_check_responses() {
        let pull = json!({
            "number": 23, "title": "Example", "body": null, "state": "closed",
            "merged_at": "2026-01-01T00:00:00Z", "draft": false, "mergeable": null,
            "merge_commit_sha": "merge", "html_url": "https://github.example/team/catalog/pull/23",
            "user": {"login": "alice"}, "labels": [{"name": "review"}],
            "head": {"ref": "proposal", "sha": "head", "repo": {"name": "catalog", "owner": {"login": "alice"}}},
            "base": {"ref": "main", "sha": "base"}
        });
        let result = change(&pull, "github.example").unwrap();
        assert_eq!(result.state, ChangeState::Merged);
        assert_eq!(result.mergeability, Mergeability::Unknown);
        assert_eq!(result.source_repository.unwrap().namespace, "alice");
        assert_eq!(result.labels, vec!["review"]);
        let run = check(&json!({"name": "CI", "status": "completed", "conclusion": "timed_out", "output": {"summary": "late"}}), true).unwrap();
        assert!(matches!(run.status, CheckStatus::Completed));
        assert!(matches!(run.conclusion, Some(Conclusion::TimedOut)));
        let status = check(&json!({"context": "render", "state": "pending"}), false).unwrap();
        assert!(matches!(status.status, CheckStatus::InProgress));
        assert!(status.conclusion.is_none());
        assert!(
            check(
                &json!({"context": "render", "state": "unrecognized"}),
                false
            )
            .is_err()
        );
    }
}
