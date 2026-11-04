//! Required GitHub rollout-window status. The webhook, forge and database are
//! trusted; no GitHub Actions input or pull-request file is an authority.
//!
//! Require `thalamus/rollout-window` from the configured GitHub App, not from
//! GitHub Actions or any source. A successful status is a point-in-time decision,
//! not an atomic promise about the instant at which GitHub merges. Reconciliation
//! refreshes it, including at window entry/expiry, but cannot make a partitioned
//! forge or a delayed merge atomic with the controller's clock.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use forge::{ForgeKind, RepoRef};
use futures_util::{stream::FuturesUnordered, StreamExt};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use thalamus_changes::{CiStatus, MergeGroup, Rollout, RolloutMode, Store};
use url::Url;

use crate::{rollout, workflows, ApiError, AppState};

pub const REQUIRED_CONTEXT: &str = "thalamus/rollout-window";
const LOCK_KEY: i64 = 0x5448_414c_4349;
const PAGE_SIZE: usize = 100;

type Result<T, E = &'static str> = std::result::Result<T, E>;

/// Whether the operator configured an in-place controller and signed GitHub webhook.
pub fn enabled(state: &AppState) -> bool {
    state
        .config
        .workflow
        .as_ref()
        .is_some_and(workflows::WorkflowConfig::rollout_ci_enabled)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/webhooks/github/rollout-ci", post(webhook))
        .route("/webhooks/github/rollout-ci/decision", post(decision))
}

#[derive(Clone)]
struct Github {
    repo: RepoRef,
    api: Url,
    graphql: Url,
    app: Arc<forge::github::Github>,
    client: reqwest::Client,
}

impl Github {
    fn new(state: &AppState) -> Result<(Self, String)> {
        let config = workflows::gitops(state).map_err(|_| "GitOps is not configured")?;
        let config = config.github.as_ref().ok_or("GitHub is not configured")?;
        let repo = RepoRef::parse(ForgeKind::Github, &config.repository)
            .map_err(|_| "invalid configured repository")?;
        // The shared App client validates the API/repository binding and caches
        // renewable installation credentials. Never accept a token from a PR.
        let app = workflows::github_client(config).map_err(|_| "invalid configured GitHub App")?;
        let secret = config
            .webhook_secret
            .as_ref()
            .filter(|secret| secret.len() >= 32)
            .ok_or("GitHub webhook secret is required")?
            .clone();
        let mut api = Url::parse(&config.api_base).map_err(|_| "invalid GitHub API")?;
        if !api.path().ends_with('/') {
            let path = format!("{}/", api.path());
            api.set_path(&path);
        }
        let mut graphql = api.clone();
        let path = api.path().trim_end_matches('/');
        let path = if repo.host == "github.com" && path.is_empty() {
            "/graphql".to_owned()
        } else if let Some(prefix) = path.strip_suffix("/api/v3") {
            format!("{prefix}/api/graphql")
        } else {
            return Err("GitHub Enterprise API base must end in /api/v3");
        };
        graphql.set_path(&path);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .user_agent("thalamus-rollout-ci")
            .build()
            .map_err(|_| "cannot create GitHub client")?;
        Ok((
            Self {
                repo,
                api,
                graphql,
                app,
                client,
            },
            secret,
        ))
    }

    fn repo_url(&self, parts: &[&str]) -> Result<Url> {
        let mut url = self.api.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| "invalid GitHub API")?;
            path.pop_if_empty()
                .push("repos")
                .push(&self.repo.namespace)
                .push(&self.repo.name);
            for part in parts {
                path.push(part);
            }
        }
        Ok(url)
    }

    async fn get(&self, url: Url) -> Result<Value> {
        let token = self
            .app
            .installation_token()
            .await
            .map_err(|_| "GitHub App authentication failed")?;
        self.client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| "GitHub request failed")?
            .error_for_status()
            .map_err(|_| "GitHub request failed")?
            .json()
            .await
            .map_err(|_| "invalid GitHub response")
    }

    async fn post(&self, url: Url, body: &Value) -> Result<Value> {
        let token = self
            .app
            .installation_token()
            .await
            .map_err(|_| "GitHub App authentication failed")?;
        self.client
            .post(url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .json(body)
            .send()
            .await
            .map_err(|_| "GitHub request failed")?
            .error_for_status()
            .map_err(|_| "GitHub request failed")?
            .json()
            .await
            .map_err(|_| "invalid GitHub response")
    }

    async fn open_pulls(&self) -> Result<Vec<Pull>> {
        let mut result = Vec::new();
        let mut seen = BTreeSet::new();
        let mut page = 1_u64;
        loop {
            let mut url = self.repo_url(&["pulls"])?;
            url.query_pairs_mut()
                .append_pair("state", "open")
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());
            let value = self.get(url).await?;
            let entries = value.as_array().ok_or("invalid pull request page")?;
            for entry in entries {
                let pr = Pull::parse(entry, &self.repo)?;
                if !seen.insert(pr.number) {
                    return Err("duplicate pull request page entry");
                }
                result.push(pr);
            }
            if entries.len() < PAGE_SIZE {
                break;
            }
            page = page.checked_add(1).ok_or("too many pull request pages")?;
        }
        Ok(result)
    }

    async fn current_pull(&self, number: u64) -> Result<Pull> {
        let value = self
            .get(self.repo_url(&["pulls", &number.to_string()])?)
            .await?;
        Pull::parse(&value, &self.repo)
    }

    async fn queue(&self, branch: &str) -> Result<BTreeMap<u64, String>> {
        const QUERY: &str = "query($owner:String!,$name:String!,$branch:String!,$after:String){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100,after:$after){nodes{pullRequest{number headRefOid baseRefName state}} pageInfo{hasNextPage endCursor}}}}}";
        let mut after: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut pulls = BTreeMap::new();
        loop {
            let response = self.post(self.graphql.clone(), &json!({
                "query": QUERY,
                "variables": {"owner": self.repo.namespace, "name": self.repo.name, "branch": branch, "after": after}
            })).await?;
            match response.get("errors") {
                None | Some(Value::Null) => (),
                Some(Value::Array(errors)) if errors.is_empty() => (),
                _ => return Err("GitHub queue query failed"),
            }
            let entries = response
                .pointer("/data/repository/mergeQueue/entries")
                .filter(|v| !v.is_null())
                .ok_or("merge queue is unavailable")?;
            let nodes = entries
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or("invalid queue page")?;
            for node in nodes {
                let pr = node.get("pullRequest").ok_or("invalid queue entry")?;
                let number = pr
                    .get("number")
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0)
                    .ok_or("invalid queue PR")?;
                let head = text(pr, "headRefOid")?;
                if !valid_sha(head)
                    || text(pr, "baseRefName")? != branch
                    || text(pr, "state")? != "OPEN"
                    || pulls.insert(number, head.to_owned()).is_some()
                {
                    return Err("inconsistent queue entry");
                }
            }
            let info = entries.get("pageInfo").ok_or("invalid queue pagination")?;
            match info.get("hasNextPage").and_then(Value::as_bool) {
                Some(false) => break,
                Some(true) => {
                    let cursor = text(info, "endCursor")?.to_owned();
                    if cursor.is_empty() || !seen.insert(cursor.clone()) {
                        return Err("invalid queue cursor");
                    }
                    after = Some(cursor);
                }
                None => return Err("invalid queue pagination"),
            }
        }
        Ok(pulls)
    }

    // A signed event alone can be replayed. Require the exact live GitHub ref as
    // well as a currently queued anchor PR before accepting its provenance.
    async fn group_ref_live(&self, group: &MergeGroup) -> Result<bool> {
        let relative = group
            .head_ref
            .strip_prefix("refs/")
            .ok_or("invalid group ref")?;
        let mut parts = vec!["git", "ref"];
        parts.extend(relative.split('/'));
        let url = self.repo_url(&parts)?;
        let token = self
            .app
            .installation_token()
            .await
            .map_err(|_| "GitHub App authentication failed")?;
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| "GitHub ref request failed")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let value: Value = response
            .error_for_status()
            .map_err(|_| "GitHub ref request failed")?
            .json()
            .await
            .map_err(|_| "invalid GitHub ref")?;
        Ok(
            value.get("ref").and_then(Value::as_str) == Some(group.head_ref.as_str())
                && value.pointer("/object/sha").and_then(Value::as_str)
                    == Some(group.head_sha.as_str()),
        )
    }

    // A status is keyed by commit, not by event. Do not let an unmanaged PR
    // whose head is also a merge-group commit manufacture a successful group
    // status before the independently signed group event arrives.
    async fn queue_refs(&self) -> Result<BTreeMap<String, String>> {
        let value = self
            .get(self.repo_url(&["git", "matching-refs", "heads", "gh-readonly-queue"])?)
            .await?;
        let entries = value.as_array().ok_or("invalid queue references")?;
        let mut refs = BTreeMap::new();
        for entry in entries {
            let name = text(entry, "ref")?;
            if !name.starts_with("refs/heads/gh-readonly-queue/") {
                continue;
            }
            let sha = entry
                .pointer("/object/sha")
                .and_then(Value::as_str)
                .filter(|sha| valid_sha(sha))
                .ok_or("invalid queue reference")?;
            if refs.insert(name.to_owned(), sha.to_owned()).is_some() {
                return Err("duplicate queue reference");
            }
        }
        Ok(refs)
    }

    async fn latest_status(&self, sha: &str) -> Result<Option<(i64, String)>> {
        let mut page = 1_u64;
        loop {
            let mut url = self.repo_url(&["commits", sha, "statuses"])?;
            url.query_pairs_mut()
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());
            let value = self.get(url).await?;
            let statuses = value.as_array().ok_or("invalid commit statuses")?;
            for status in statuses {
                if status.get("context").and_then(Value::as_str) == Some(REQUIRED_CONTEXT) {
                    let id = status
                        .get("id")
                        .and_then(Value::as_i64)
                        .ok_or("invalid status id")?;
                    return Ok(Some((id, text(status, "state")?.to_owned())));
                }
            }
            if statuses.len() < PAGE_SIZE {
                return Ok(None);
            }
            page = page.checked_add(1).ok_or("too many status pages")?;
        }
    }

    async fn publish(&self, store: &Store, sha: &str, verdict: &Verdict) -> Result<()> {
        let verdict = verdict.clone().current();
        let previous = match store.ci_status(&self.repo, sha).await {
            Ok(previous) => previous,
            Err(_) if !verdict.allow => None,
            Err(_) => return Err("status database unavailable"),
        };
        let latest = match self.latest_status(sha).await {
            Ok(latest) => latest,
            Err(_) if !verdict.clone().current().allow => None,
            Err(error) => return Err(error),
        };
        let verdict = verdict.current();
        let desired = if verdict.allow {
            "success"
        } else if verdict.pending {
            "pending"
        } else {
            "failure"
        };
        // Checking the forge's latest ID as well as our durable ID avoids treating
        // another actor's same-named status as our App's status.
        if previous.as_ref().is_some_and(|p| {
            p.state == desired
                && latest
                    .as_ref()
                    .is_some_and(|(id, state)| *id == p.status_id && state == desired)
        }) {
            return Ok(());
        }
        let body =
            json!({"state": desired, "context": REQUIRED_CONTEXT, "description": verdict.reason});
        let result = self.post(self.repo_url(&["statuses", sha])?, &body).await?;
        let id = result
            .get("id")
            .and_then(Value::as_i64)
            .ok_or("invalid created status")?;
        store
            .put_ci_status(&CiStatus {
                repository: self.repo.clone(),
                sha: sha.to_owned(),
                status_id: id,
                state: desired.to_owned(),
            })
            .await
            .map_err(|_| "status database unavailable")
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("invalid GitHub payload")
}
fn valid_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|c| c.is_ascii_hexdigit())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pull {
    number: u64,
    head: String,
    base: String,
    target: String,
    branch: String,
    source: Option<RepoRef>,
}
impl Pull {
    fn parse(value: &Value, repository: &RepoRef) -> Result<Self> {
        let number = value
            .get("number")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .ok_or("invalid pull request")?;
        if text(value, "state")? != "open" {
            return Err("pull request is not open");
        }
        let head = value.get("head").ok_or("invalid pull request head")?;
        let base = value.get("base").ok_or("invalid pull request base")?;
        let head_sha = text(head, "sha")?;
        let base_sha = text(base, "sha")?;
        let base_repo = base.get("repo").ok_or("invalid pull request repository")?;
        let base_repo = RepoRef::parse(ForgeKind::Github, text(base_repo, "html_url")?)
            .map_err(|_| "invalid pull request repository")?;
        if base_repo != *repository || !valid_sha(head_sha) || !valid_sha(base_sha) {
            return Err("invalid pull request identity");
        }
        let source = match head.get("repo").filter(|v| !v.is_null()) {
            Some(repo) => Some(
                RepoRef::parse(ForgeKind::Github, text(repo, "html_url")?)
                    .map_err(|_| "invalid source repository")?,
            ),
            None => None,
        };
        Ok(Self {
            number,
            head: head_sha.to_owned(),
            base: base_sha.to_owned(),
            target: text(base, "ref")?.to_owned(),
            branch: text(head, "ref")?.to_owned(),
            source,
        })
    }
}

fn bound_to_change(pr: &Pull, change: &thalamus_changes::Change, repository: &RepoRef) -> bool {
    pr.source.as_ref() == Some(repository)
        && pr.branch == change.branch
        && pr.target == change.request.target_branch
        && change.head_sha.as_deref() == Some(pr.head.as_str())
        && change.request.base_sha == pr.base
}

// Pooled in-place changes also need a recent independent observation that the
// previous revision and zero-replica filler are safe. Call only for a trusted
// pooled rollout; missing, future or stale evidence never authorizes a status.
fn pooled_proof_deadline(
    previous_revision: Option<&str>,
    namespace: Option<&str>,
    verified_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if previous_revision.is_none_or(|value| value.trim().is_empty())
        || namespace.is_none_or(|value| value.trim().is_empty())
    {
        return None;
    }
    let verified_at = verified_at?;
    let deadline = verified_at.checked_add_signed(ChronoDuration::seconds(30))?;
    (verified_at <= now && now < deadline).then_some(deadline)
}

#[derive(Clone, Debug)]
struct Verdict {
    allow: bool,
    pending: bool,
    reason: &'static str,
    // Earliest window-margin or independently observed proof expiry.
    deadline: Option<DateTime<Utc>>,
    pending_on_expiry: bool,
}
impl Verdict {
    fn pass() -> Self {
        Self {
            allow: true,
            pending: false,
            reason: "Current rollout checks passed",
            deadline: None,
            pending_on_expiry: false,
        }
    }
    fn deny(reason: &'static str) -> Self {
        Self {
            allow: false,
            pending: false,
            reason,
            deadline: None,
            pending_on_expiry: false,
        }
    }
    fn pending(reason: &'static str) -> Self {
        Self {
            allow: false,
            pending: true,
            reason,
            deadline: None,
            pending_on_expiry: false,
        }
    }
    fn current(self) -> Self {
        if self.allow && self.deadline.is_some_and(|end| Utc::now() >= end) {
            if self.pending_on_expiry {
                Self::pending("Pooled filler safety proof needs refresh")
            } else {
                Self::deny("Rollout window or merge margin has elapsed")
            }
        } else {
            self
        }
    }
    fn include(&mut self, other: &Self) {
        if let Some(end) = other.deadline {
            match self.deadline {
                None => {
                    self.deadline = Some(end);
                    self.pending_on_expiry = other.pending_on_expiry;
                }
                Some(previous) if end < previous => {
                    self.deadline = Some(end);
                    self.pending_on_expiry = other.pending_on_expiry;
                }
                Some(previous) if end == previous => {
                    self.pending_on_expiry &= other.pending_on_expiry;
                }
                _ => (),
            }
        }
    }
}

async fn check_pull(
    state: &AppState,
    github: &Github,
    store: &Store,
    pr: &Pull,
) -> Result<Verdict> {
    let changes = store
        .by_pr_number(pr.number)
        .await
        .map_err(|_| "change database unavailable")?;
    // The publisher reserves this namespace before opening the PR. A webhook can
    // arrive before the database has recorded the newly created PR number.
    if changes.is_empty() && pr.branch.starts_with("thalamus/") {
        return Ok(Verdict::deny("Managed branch is not yet bound to a change"));
    }
    let mut result = Verdict::pass();
    for change in changes {
        if !matches!(&change.request.rollout, Rollout::InPlace { .. }) {
            continue;
        }
        if !bound_to_change(pr, &change, &github.repo) {
            return Ok(Verdict::deny(
                "Managed pull request identity or revision changed",
            ));
        }
        let policy = match rollout::policy_for_change(state, &change) {
            Ok(policy)
                if policy.mode == RolloutMode::InPlace && policy.repository == github.repo =>
            {
                policy
            }
            _ => return Ok(Verdict::deny("Managed rollout policy changed")),
        };
        if !store
            .approval_current(change.id, &policy, &pr.head, &pr.base, Utc::now())
            .await
            .map_err(|_| "approval lookup failed")?
        {
            return Ok(Verdict::deny(
                "Current owner approval and open window required",
            ));
        }
        let mut status = store
            .rollout(change.id)
            .await
            .map_err(|_| "rollout lookup failed")?;
        if !status.prepared {
            return Ok(Verdict::deny(
                "Rollout pool classification has not been prepared",
            ));
        }
        if status.in_place_pool {
            if status
                .previous_revision
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
                || status
                    .workload_namespace
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
            {
                return Ok(Verdict::deny(
                    "Pooled rollout revision or namespace is missing",
                ));
            }
            let mut proof_deadline = pooled_proof_deadline(
                status.previous_revision.as_deref(),
                status.workload_namespace.as_deref(),
                status.filler_verified_at,
                Utc::now(),
            );
            if proof_deadline.is_none() {
                // The worker clears its proof while performing a fresh live
                // observation. Avoid a fixed CI/worker phase alignment keeping
                // a healthy rollout pending forever, but never reuse stale proof.
                let refreshed = tokio::time::timeout(Duration::from_secs(8), async {
                    for _ in 0..5 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let next = store
                            .rollout(change.id)
                            .await
                            .map_err(|_| "rollout lookup failed")?;
                        if !next.prepared
                            || !next.in_place_pool
                            || next.previous_revision != status.previous_revision
                            || next.workload_namespace != status.workload_namespace
                        {
                            return Ok(None);
                        }
                        status = next;
                        if let Some(deadline) = pooled_proof_deadline(
                            status.previous_revision.as_deref(),
                            status.workload_namespace.as_deref(),
                            status.filler_verified_at,
                            Utc::now(),
                        ) {
                            return Ok(Some(deadline));
                        }
                    }
                    Ok::<Option<DateTime<Utc>>, &'static str>(None)
                })
                .await;
                proof_deadline = match refreshed {
                    Ok(Ok(deadline)) => deadline,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => None,
                };
            }
            let Some(proof_deadline) = proof_deadline else {
                return Ok(Verdict::pending(
                    "Recent pooled filler safety proof required",
                ));
            };
            // An owner may have rescheduled or revoked approval during the
            // bounded wait. The old authorization is not reusable.
            if !store
                .approval_current(change.id, &policy, &pr.head, &pr.base, Utc::now())
                .await
                .map_err(|_| "approval lookup failed")?
            {
                return Ok(Verdict::deny(
                    "Current owner approval and open window required",
                ));
            }
            result.include(&Verdict {
                deadline: Some(proof_deadline),
                pending_on_expiry: true,
                ..Verdict::pass()
            });
        }
        let Some(end) = status.window_end else {
            return Ok(Verdict::deny("Current rollout window required"));
        };
        let margin =
            i64::try_from(policy.merge_margin_seconds).map_err(|_| "invalid merge margin")?;
        let deadline = end
            .checked_sub_signed(ChronoDuration::seconds(margin))
            .ok_or("invalid rollout deadline")?;
        result.include(&Verdict {
            deadline: Some(deadline),
            ..Verdict::pass()
        });
    }
    // Statuses attach to SHA, but base/target/source may change without a new
    // head. Re-read the PR immediately after evaluating the durable approval.
    if &github.current_pull(pr.number).await? != pr {
        return Ok(Verdict::deny("Pull request changed during evaluation"));
    }
    Ok(result.current())
}

fn group_anchor(group: &MergeGroup) -> Result<(&str, u64)> {
    let branch = group
        .base_ref
        .strip_prefix("refs/heads/")
        .filter(|s| !s.is_empty())
        .ok_or("invalid group base")?;
    let prefix = format!("refs/heads/gh-readonly-queue/{branch}/pr-");
    let suffix = group
        .head_ref
        .strip_prefix(&prefix)
        .ok_or("invalid group head ref")?;
    let (number, discriminator) = suffix.split_once('-').ok_or("invalid group head ref")?;
    let number: u64 = number.parse().map_err(|_| "invalid group anchor")?;
    if number == 0
        || discriminator.is_empty()
        || !discriminator.bytes().all(|c| c.is_ascii_hexdigit())
        || !valid_sha(&group.head_sha)
        || !valid_sha(&group.base_sha)
    {
        return Err("invalid group identity");
    }
    Ok((branch, number))
}

fn queued_pull<'a>(pulls: &'a [Pull], number: u64, head: &str, branch: &str) -> Option<&'a Pull> {
    pulls
        .iter()
        .find(|pr| pr.number == number && pr.head == head && pr.target == branch)
}

async fn check_group(
    state: &AppState,
    github: &Github,
    store: &Store,
    group: &MergeGroup,
    pulls: &[Pull],
) -> Result<Verdict> {
    let (branch, anchor) = group_anchor(group)?;
    if group.repository != github.repo || !github.group_ref_live(group).await? {
        return Ok(Verdict::deny("Signed merge group is no longer current"));
    }
    let queue = github.queue(branch).await?;
    if !queue.contains_key(&anchor) {
        return Ok(Verdict::deny("Merge group anchor is not queued"));
    }
    for (number, head) in &queue {
        if queued_pull(pulls, *number, head, branch).is_none() {
            return Ok(Verdict::deny("Queue and current pull request disagree"));
        }
    }
    // A merge group may contain many queued entries. Bound concurrency so the
    // short-lived pooled-filler evidence cannot expire simply while every other
    // PR is checked serially, without issuing an unbounded burst to GitHub.
    let mut checks = FuturesUnordered::new();
    let mut result = Verdict::pass();
    for (number, head) in &queue {
        let pr = queued_pull(pulls, *number, head, branch).ok_or("queue PR disappeared")?;
        checks.push(check_pull(state, github, store, pr));
        if checks.len() == 16 {
            while let Some(verdict) = checks.next().await {
                let verdict = verdict?;
                if !verdict.allow {
                    return Ok(verdict);
                }
                result.include(&verdict);
            }
        }
    }
    while let Some(verdict) = checks.next().await {
        let verdict = verdict?;
        if !verdict.allow {
            return Ok(verdict);
        }
        result.include(&verdict);
    }
    // A changed queue or ref invalidates this snapshot. Check deadlines again
    // after these final forge requests; no prior proof survives its expiry.
    if queue != github.queue(branch).await? || !github.group_ref_live(group).await? {
        return Ok(Verdict::deny("Merge group changed during evaluation"));
    }
    Ok(result.current())
}

fn has_unverified_group(sha: &str, groups: &[MergeGroup], refs: &BTreeMap<String, String>) -> bool {
    refs.iter().any(|(name, head)| {
        head == sha
            && !groups
                .iter()
                .any(|group| group.head_sha == sha && group.head_ref.as_str() == name.as_str())
    })
}

async fn check_sha(
    state: &AppState,
    github: &Github,
    store: &Store,
    sha: &str,
    pulls: &[Pull],
    groups: &[MergeGroup],
    refs: &BTreeMap<String, String>,
) -> Result<Verdict> {
    let mut found = false;
    let mut result = Verdict::pass();
    if has_unverified_group(sha, groups, refs) {
        return Ok(Verdict::deny("Signed merge group evidence has not arrived"));
    }
    // Commit statuses are attached to a SHA, not a PR. All open PRs sharing
    // that SHA, and any live group sharing it, must pass before publishing it.
    for pr in pulls.iter().filter(|pr| pr.head == sha) {
        found = true;
        let verdict = check_pull(state, github, store, pr).await?;
        if !verdict.allow {
            return Ok(verdict);
        }
        result.include(&verdict);
    }
    for group in groups.iter().filter(|group| group.head_sha == sha) {
        found = true;
        let verdict = check_group(state, github, store, group, pulls).await?;
        if !verdict.allow {
            return Ok(verdict);
        }
        result.include(&verdict);
    }
    if !found {
        return Ok(Verdict::deny("No current pull request or signed group"));
    }
    Ok(result.current())
}

async fn lock(state: &AppState) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
    loop {
        let mut tx = state.db.begin().await.map_err(|_| "CI lock unavailable")?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(LOCK_KEY)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| "CI lock unavailable")?;
        if acquired {
            return Ok(tx);
        }
        // Do not occupy every pool connection while another replica holds the
        // lock across forge requests and needs a second connection for evidence.
        tx.rollback().await.map_err(|_| "CI lock unavailable")?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// If the evidence APIs fail but GitHub still accepts statuses, revoke prior
// successes rather than leaving them green until the next healthy pass. This
// is necessarily best-effort: a forge outage can also prevent the revocation.
async fn revoke_prior_successes(github: &Github, store: &Store) {
    if let Ok(statuses) = store.ci_statuses(&github.repo).await {
        for status in statuses
            .into_iter()
            .filter(|status| status.state == "success")
        {
            let _ = github
                .publish(
                    store,
                    &status.sha,
                    &Verdict::deny("Unable to verify current rollout evidence"),
                )
                .await;
        }
    }
}

async fn reconcile_locked(state: &AppState, github: &Github) -> Result<()> {
    let store = Store::new(state.db.clone());
    let pulls = match github.open_pulls().await {
        Ok(pulls) => pulls,
        Err(error) => {
            revoke_prior_successes(github, &store).await;
            return Err(error);
        }
    };
    let groups = match store.merge_groups(&github.repo).await {
        Ok(groups) => groups,
        Err(_) => {
            revoke_prior_successes(github, &store).await;
            return Err("group database unavailable");
        }
    };
    let refs = match github.queue_refs().await {
        Ok(refs) => refs,
        Err(error) => {
            revoke_prior_successes(github, &store).await;
            return Err(error);
        }
    };
    // Do not discard signed provenance on a single missing-ref response:
    // GitHub's ref can become visible after its webhook. The signed destroyed
    // event removes it; until then, a missing ref cannot pass.
    let mut failed = false;
    let mut shas = BTreeSet::new();
    shas.extend(pulls.iter().map(|p| p.head.clone()));
    shas.extend(groups.iter().map(|g| g.head_sha.clone()));
    // Revisit prior successes even after a PR closes or a signed group is
    // destroyed, so an old green status is not left on a formerly active SHA.
    let statuses = match store.ci_statuses(&github.repo).await {
        Ok(statuses) => statuses,
        Err(_) => {
            revoke_prior_successes(github, &store).await;
            return Err("status database unavailable");
        }
    };
    shas.extend(
        statuses
            .into_iter()
            .filter(|s| s.state == "success")
            .map(|s| s.sha),
    );
    for sha in shas {
        let verdict = match check_sha(state, github, &store, &sha, &pulls, &groups, &refs).await {
            Ok(verdict) => verdict,
            Err(_) => {
                failed = true;
                Verdict::deny("Unable to verify current rollout evidence")
            }
        };
        if github.publish(&store, &sha, &verdict).await.is_err() {
            failed = true;
        }
    }
    if failed {
        Err("one or more CI statuses could not be reconciled")
    } else {
        Ok(())
    }
}

/// Run periodically from the server process. This never enqueues or merges a PR.
/// Every reconciliation, whether periodic or webhook-triggered, is serialized.
pub async fn reconcile(state: Arc<AppState>) -> std::result::Result<(), String> {
    let (github, _) = Github::new(&state).map_err(str::to_owned)?;
    let tx = lock(&state).await.map_err(str::to_owned)?;
    let result = reconcile_locked(&state, &github)
        .await
        .map_err(str::to_owned);
    tx.commit()
        .await
        .map_err(|_| "CI lock unavailable".to_owned())?;
    result
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or("missing webhook header")?
        .to_str()
        .map_err(|_| "invalid webhook header")?;
    if values.next().is_some() {
        return Err("duplicate webhook header");
    }
    Ok(value)
}

fn verify_signature(secret: &[u8], signature: &str, body: &[u8]) -> bool {
    let Some(encoded) = signature.strip_prefix("sha256=") else {
        return false;
    };
    if encoded.len() != 64 || !encoded.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let mut decoded = [0_u8; 32];
    for (i, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| (b as char).to_digit(16);
        let (Some(a), Some(b)) = (digit(pair[0]), digit(pair[1])) else {
            return false;
        };
        decoded[i] = ((a << 4) | b) as u8;
    }
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&decoded).is_ok()
}

fn webhook_repository(value: &Value, expected: &RepoRef) -> Result<()> {
    let repo = value
        .get("repository")
        .ok_or("missing webhook repository")?;
    let observed = RepoRef::parse(ForgeKind::Github, text(repo, "html_url")?)
        .map_err(|_| "invalid webhook repository")?;
    if &observed != expected
        || text(repo, "full_name")? != format!("{}/{}", expected.namespace, expected.name)
    {
        return Err("webhook repository mismatch");
    }
    Ok(())
}

fn parse_group(value: &Value, repository: &RepoRef) -> Result<MergeGroup> {
    let group = value.get("merge_group").ok_or("missing merge group")?;
    let group = MergeGroup {
        repository: repository.clone(),
        base_ref: text(group, "base_ref")?.to_owned(),
        base_sha: text(group, "base_sha")?.to_owned(),
        head_ref: text(group, "head_ref")?.to_owned(),
        head_sha: text(group, "head_sha")?.to_owned(),
    };
    group_anchor(&group)?;
    Ok(group)
}

pub(crate) async fn webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<StatusCode, ApiError> {
    let (github, secret) = Github::new(&state)
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "CI gate unavailable"))?;
    let signature = single_header(&headers, "x-hub-signature-256")
        .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "invalid webhook signature"))?;
    if !verify_signature(secret.as_bytes(), signature, &body) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid webhook signature",
        ));
    }
    let event = single_header(&headers, "x-github-event")
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid event"))?;
    // App installations have one webhook URL and may send ping and unrelated
    // events. Verify their signatures, then let the shared dispatcher ignore them.
    if !matches!(event, "pull_request" | "merge_group") {
        return Ok(StatusCode::NO_CONTENT);
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid event"))?;
    webhook_repository(&value, &github.repo)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid repository"))?;
    let store = Store::new(state.db.clone());
    match event {
        "pull_request" => {
            if value
                .get("number")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .is_none()
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid pull request",
                ));
            }
        }
        "merge_group" => {
            let group = parse_group(&value, &github.repo)
                .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid merge group"))?;
            match text(&value, "action") {
                Ok("checks_requested") => store.put_merge_group(&group).await,
                Ok("destroyed") => {
                    store
                        .remove_merge_group(&github.repo, &group.head_sha)
                        .await
                }
                _ => {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid merge group action",
                    ))
                }
            }
            .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "CI gate unavailable"))?;
        }
        _ => unreachable!("other events were returned above"),
    }
    // Persist signed group evidence before acknowledging delivery. GitHub's
    // delivery timeout is short, so never perform full queue reconciliation in
    // the webhook request itself. The periodic loop also recovers failed work.
    tokio::spawn(async move {
        if let Err(error) = reconcile(state).await {
            eprintln!("rollout webhook reconciliation failed: {error}");
        }
    });
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionRequest {
    repository: String,
    event: String,
    sha: String,
    number: Option<u64>,
    base_ref: Option<String>,
    base_sha: Option<String>,
    head_ref: Option<String>,
}
#[derive(Serialize)]
struct DecisionResponse {
    verified: bool,
    allow: bool,
    reason: &'static str,
}

// No credential is accepted here: this endpoint reports an independently
// derived decision. It never writes a status or accepts a caller-supplied policy.
async fn decision(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DecisionRequest>,
) -> std::result::Result<Json<DecisionResponse>, ApiError> {
    let unavailable = || ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "CI gate unavailable");
    let (github, _) = Github::new(&state).map_err(|_| unavailable())?;
    if request.repository != format!("{}/{}", github.repo.namespace, github.repo.name)
        || !valid_sha(&request.sha)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid repository or revision",
        ));
    }
    let store = Store::new(state.db.clone());
    let pulls = github.open_pulls().await.map_err(|_| unavailable())?;
    let groups = store
        .merge_groups(&github.repo)
        .await
        .map_err(|_| unavailable())?;
    let refs = github.queue_refs().await.map_err(|_| unavailable())?;
    let matches_event = match request.event.as_str() {
        "pull_request" => request.number.is_some_and(|number| {
            pulls
                .iter()
                .any(|pr| pr.number == number && pr.head == request.sha)
        }),
        "merge_group" => groups.iter().any(|g| {
            g.head_sha == request.sha
                && request.base_ref.as_deref() == Some(&g.base_ref)
                && request.base_sha.as_deref() == Some(&g.base_sha)
                && request.head_ref.as_deref() == Some(&g.head_ref)
        }),
        _ => false,
    };
    if !matches_event {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "current signed event is not available",
        ));
    }
    let verdict = check_sha(
        &state,
        &github,
        &store,
        &request.sha,
        &pulls,
        &groups,
        &refs,
    )
    .await
    .map_err(|_| unavailable())?;
    Ok(Json(DecisionResponse {
        verified: true,
        allow: verdict.allow,
        reason: verdict.reason,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn repo() -> RepoRef {
        RepoRef::parse(ForgeKind::Github, "https://github.com/example/gitops").unwrap()
    }
    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }
    fn group() -> MergeGroup {
        MergeGroup {
            repository: repo(),
            base_ref: "refs/heads/release/prod".into(),
            base_sha: sha('a'),
            head_ref: "refs/heads/gh-readonly-queue/release/prod/pr-42-ab12".into(),
            head_sha: sha('b'),
        }
    }

    #[test]
    fn signature_binds_every_byte_and_requires_sha256() {
        let secret = b"01234567890123456789012345678901";
        let body = b"{\"action\":\"checks_requested\"}";
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        let signature = format!("sha256={:x}", mac.finalize().into_bytes());
        assert!(verify_signature(secret, &signature, body));
        assert!(!verify_signature(secret, &signature, b"{}"));
        assert!(!verify_signature(
            secret,
            &signature.replace("sha256=", "sha1="),
            body
        ));
        assert!(!verify_signature(secret, "sha256=00", body));
    }

    #[test]
    fn signed_group_requires_exact_base_ref_and_queue_anchor() {
        let mut g = group();
        assert_eq!(group_anchor(&g).unwrap(), ("release/prod", 42));
        g.head_ref = "refs/heads/gh-readonly-queue/main/pr-42-ab12".into();
        assert!(group_anchor(&g).is_err());
        g = group();
        g.head_sha = "abc".into();
        assert!(group_anchor(&g).is_err());
    }

    #[test]
    fn every_queued_head_must_match_the_current_pr_and_target() {
        let first = Pull {
            number: 42,
            head: sha('a'),
            base: sha('b'),
            target: "main".into(),
            branch: "thalamus/first".into(),
            source: Some(repo()),
        };
        let second = Pull {
            number: 43,
            head: sha('c'),
            base: sha('b'),
            target: "main".into(),
            branch: "feature/unmanaged".into(),
            source: Some(repo()),
        };
        let pulls = [first, second];
        assert!(queued_pull(&pulls, 42, &sha('a'), "main").is_some());
        assert!(queued_pull(&pulls, 43, &sha('c'), "main").is_some());
        assert!(queued_pull(&pulls, 43, &sha('a'), "main").is_none());
        assert!(queued_pull(&pulls, 42, &sha('a'), "other").is_none());
        assert!(queued_pull(&pulls, 44, &sha('a'), "main").is_none());
    }

    #[test]
    fn pooled_rollout_requires_fresh_nonfuture_filler_proof() {
        let now = DateTime::parse_from_rfc3339("2026-10-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let recent = now - ChronoDuration::seconds(29);
        assert_eq!(
            pooled_proof_deadline(Some("revision"), Some("workload"), Some(recent), now),
            Some(recent + ChronoDuration::seconds(30))
        );
        assert!(pooled_proof_deadline(None, Some("workload"), Some(recent), now).is_none());
        assert!(pooled_proof_deadline(Some("  "), Some("workload"), Some(recent), now).is_none());
        assert!(pooled_proof_deadline(Some("revision"), None, Some(recent), now).is_none());
        assert!(pooled_proof_deadline(Some("revision"), Some("workload"), None, now).is_none());
        assert!(pooled_proof_deadline(
            Some("revision"),
            Some("workload"),
            Some(now + ChronoDuration::seconds(1)),
            now
        )
        .is_none());
        assert!(pooled_proof_deadline(
            Some("revision"),
            Some("workload"),
            Some(now - ChronoDuration::seconds(30)),
            now
        )
        .is_none());
    }

    #[test]
    fn shortest_deadline_controls_combined_verdict() {
        let now = Utc::now();
        let mut combined = Verdict::pass();
        combined.include(&Verdict {
            deadline: Some(now + ChronoDuration::seconds(60)),
            ..Verdict::pass()
        });
        combined.include(&Verdict {
            deadline: Some(now - ChronoDuration::seconds(1)),
            ..Verdict::pass()
        });
        assert!(!combined.current().allow);
        let active = Verdict {
            deadline: Some(now + ChronoDuration::seconds(60)),
            ..Verdict::pass()
        };
        assert!(active.current().allow);
        assert!(!Verdict::deny("denied").current().allow);
        let proof_expired = Verdict {
            deadline: Some(now - ChronoDuration::seconds(1)),
            pending_on_expiry: true,
            ..Verdict::pass()
        }
        .current();
        assert!(!proof_expired.allow && proof_expired.pending);
        let window_expired = Verdict {
            deadline: Some(now - ChronoDuration::seconds(1)),
            ..Verdict::pass()
        }
        .current();
        assert!(!window_expired.allow && !window_expired.pending);
    }

    #[test]
    fn a_pr_cannot_authorize_an_unsigned_merge_group_with_the_same_sha() {
        let group = group();
        let mut refs = BTreeMap::new();
        refs.insert(group.head_ref.clone(), group.head_sha.clone());
        assert!(has_unverified_group(&group.head_sha, &[], &refs));
        assert!(!has_unverified_group(
            &group.head_sha,
            std::slice::from_ref(&group),
            &refs
        ));
        refs.insert(
            "refs/heads/gh-readonly-queue/main/pr-7-abcd".into(),
            group.head_sha.clone(),
        );
        assert!(has_unverified_group(
            &group.head_sha,
            std::slice::from_ref(&group),
            &refs
        ));
        assert!(!has_unverified_group(&sha('c'), &[], &refs));
    }

    #[test]
    fn repository_payload_is_bound_to_operator_configuration() {
        let payload = json!({"repository": {"full_name":"example/gitops", "html_url":"https://github.com/example/gitops"}});
        assert!(webhook_repository(&payload, &repo()).is_ok());
        let other = json!({"repository": {"full_name":"example/gitops", "html_url":"https://evil.example/example/gitops"}});
        assert!(webhook_repository(&other, &repo()).is_err());
    }

    #[test]
    fn parse_group_does_not_invent_pull_request_members() {
        let g = group();
        let payload = json!({"merge_group": {"base_ref":g.base_ref,"base_sha":g.base_sha,"head_ref":g.head_ref,"head_sha":g.head_sha}});
        let parsed = parse_group(&payload, &repo()).unwrap();
        assert_eq!(group_anchor(&parsed).unwrap().1, 42);
        assert_eq!(parsed.head_sha, sha('b'));
    }

    #[test]
    fn malformed_or_foreign_pull_request_fails_closed() {
        let value = json!({"number":42,"state":"open","head":{"sha":sha('a'),"ref":"thalamus/x","repo":{"html_url":"https://github.com/example/gitops"}},
            "base":{"sha":sha('b'),"ref":"main","repo":{"html_url":"https://github.com/example/gitops"}}});
        assert_eq!(Pull::parse(&value, &repo()).unwrap().head, sha('a'));
        let mut foreign = value.clone();
        foreign["base"]["repo"]["html_url"] = json!("https://github.com/example/other");
        assert!(Pull::parse(&foreign, &repo()).is_err());
        let mut malformed = value;
        malformed["head"]["sha"] = json!("abc");
        assert!(Pull::parse(&malformed, &repo()).is_err());
    }

    #[test]
    fn managed_binding_rejects_each_changed_identity_component() {
        let change: thalamus_changes::Change = serde_json::from_value(json!({
            "id": "00000000-0000-4000-8000-000000000001", "actor": "operator",
            "idempotency_key": "key", "state": "open", "branch": "thalamus/x",
            "head_sha": sha('a'), "pr_number": 42, "pr_url": null,
            "base_render_sha256": null, "proposed_render_sha256": null,
            "data_diff": null, "rendered_diff": null, "error": null,
            "request": {"service": "service", "base_sha": sha('b'), "target_branch": "main",
                "title": "change", "patches": [], "revert_commit": null, "validation_tier": "normal",
                "rollout": {"mode":"in_place", "window_start":"2026-01-01T00:00:00Z", "window_end":"2026-01-02T00:00:00Z"}}
        })).unwrap();
        let pr = Pull {
            number: 42,
            head: sha('a'),
            base: sha('b'),
            target: "main".into(),
            branch: "thalamus/x".into(),
            source: Some(repo()),
        };
        assert!(bound_to_change(&pr, &change, &repo()));
        let mut changed = pr.clone();
        changed.head = sha('c');
        assert!(!bound_to_change(&changed, &change, &repo()));
        let mut changed = pr.clone();
        changed.base = sha('c');
        assert!(!bound_to_change(&changed, &change, &repo()));
        let mut changed = pr.clone();
        changed.target = "other".into();
        assert!(!bound_to_change(&changed, &change, &repo()));
        let mut changed = pr.clone();
        changed.branch = "thalamus/other".into();
        assert!(!bound_to_change(&changed, &change, &repo()));
        let mut changed = pr;
        changed.source = None;
        assert!(!bound_to_change(&changed, &change, &repo()));
    }
}
