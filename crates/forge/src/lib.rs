//! Review objects and webhook notifications for Git forges. Git commits and pushes
//! deliberately remain the caller's responsibility.
use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use url::Url;

#[cfg(feature = "forgejo")]
pub mod forgejo;
#[cfg(feature = "github")]
pub mod github;
#[cfg(feature = "gitlab")]
pub mod gitlab;

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid repository: {0}")]
    InvalidRepo(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid webhook signature")]
    InvalidSignature,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("malformed forge response: {0}")]
    Malformed(String),
    #[cfg(feature = "github")]
    #[error("GitHub API: {0}")]
    Github(#[from] octocrab::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}
pub type Result<T, E = Error> = std::result::Result<T, E>;
pub type PageStream<'a, T> = Pin<Box<dyn Stream<Item = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    Github,
    Gitlab,
    Forgejo,
}

/// Forge kind must be supplied explicitly: an enterprise hostname does not identify its software.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub forge: ForgeKind,
    pub host: String,
    pub namespace: String,
    pub name: String,
}
impl RepoRef {
    pub fn parse(forge: ForgeKind, input: &str) -> Result<Self> {
        let normalized = if !input.contains("://") {
            let (authority, path) = input
                .split_once(':')
                .ok_or_else(|| Error::InvalidRepo("expected a URL or SSH clone address".into()))?;
            format!("ssh://{}/{}", authority, path)
        } else {
            input.to_owned()
        };
        let url = Url::parse(&normalized).map_err(|e| Error::InvalidRepo(e.to_string()))?;
        if !matches!(url.scheme(), "https" | "ssh")
            || url.password().is_some()
            || (url.scheme() == "https" && !url.username().is_empty())
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::InvalidRepo(
                "expected an HTTPS or SSH URL without credentials, query, or fragment".into(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::InvalidRepo("missing host".into()))?;
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let segments: Vec<_> = url.path().trim_matches('/').split('/').collect();
        if segments.len() < 2
            || segments.iter().any(|s| {
                s.is_empty()
                    || *s == "."
                    || *s == ".."
                    || !s
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            })
        {
            return Err(Error::InvalidRepo(
                "invalid namespace or repository path".into(),
            ));
        }
        if forge == ForgeKind::Github && segments.len() != 2 {
            return Err(Error::InvalidRepo(
                "GitHub repositories require one owner".into(),
            ));
        }
        let name = segments
            .last()
            .unwrap()
            .strip_suffix(".git")
            .unwrap_or(segments.last().unwrap());
        if name.is_empty() {
            return Err(Error::InvalidRepo("empty repository name".into()));
        }
        Ok(Self {
            forge,
            host,
            namespace: segments[..segments.len() - 1].join("/"),
            name: name.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    Open,
    Merged,
    Closed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRequest {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub source_branch: String,
    pub target_branch: String,
    pub head_sha: String,
    pub state: ChangeState,
    pub draft: bool,
    pub mergeability: Mergeability,
    pub base_sha: String,
    pub merge_commit: Option<String>,
    pub source_repository: Option<RepoRef>,
    pub author: String,
    pub labels: Vec<String>,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChangeRequest {
    pub title: String,
    pub body: Option<String>,
    pub source_branch: String,
    pub target_branch: String,
    pub draft: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateChangeRequest {
    pub title: Option<String>,
    pub body: Option<String>,
    pub target_branch: Option<String>,
    pub state: Option<ChangeState>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub conclusion: Option<Conclusion>,
    pub description: Option<String>,
    pub url: Option<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CombinedState {
    Success,
    Failure,
    Pending,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub id: u64,
    pub author: String,
    pub state: String,
    /// Commit actually reviewed, as reported by the forge; absent for an unsubmitted review.
    pub commit_sha: Option<String>,
    /// Submission timestamp reported by the forge; absent for a pending review.
    pub submitted_at: Option<String>,
    pub body: Option<String>,
    pub url: String,
}
/// An upstream policy snapshot, not a guarantee that a later merge is protected.
/// Values are unmodified GitHub REST responses, including unknown fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchPolicy {
    pub branch: String,
    pub protection: serde_json::Value,
    pub rules: Vec<serde_json::Value>,
    /// All repository and inherited ruleset details, not just those applying to this branch.
    /// Missing bypass visibility or an inaccessible detail makes retrieval fail.
    pub rulesets: Vec<serde_json::Value>,
}
/// Evidence for an authenticated GitHub user. Installation tokens do not identify
/// a user through this API and must not be treated as non-bypassing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyActor {
    pub user: serde_json::Value,
    pub repository_permission: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAction {
    Opened,
    Updated,
    Merged,
    Closed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebhookEvent {
    ChangeRequest {
        action: ChangeAction,
        repository: RepoRef,
        number: u64,
    },
    Push {
        repository: RepoRef,
        reference: String,
        after: String,
    },
    CheckUpdated {
        repository: RepoRef,
        sha: String,
    },
}

/// Static adapter support; successful calls still depend on permissions, server version and repository settings.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub change_requests: bool,
    pub merge: bool,
    pub comments: bool,
    pub reviews: bool,
    pub branch_policy_evidence: bool,
    /// Whether merge atomically guards target, base and policy as well as head.
    pub atomic_policy_guarded_merge: bool,
    pub commit_statuses: bool,
    pub check_runs: bool,
    pub webhooks: bool,
}
#[async_trait]
pub trait ChangeRequests: Send + Sync {
    async fn get_change(&self, number: u64) -> Result<ChangeRequest>;
    fn list_changes(&self) -> PageStream<'_, ChangeRequest>;
    async fn create_change(&self, request: &CreateChangeRequest) -> Result<ChangeRequest>;
    async fn update_change(
        &self,
        number: u64,
        request: &UpdateChangeRequest,
    ) -> Result<ChangeRequest>;
    /// Recheck the approved head, target branch and base SHA immediately before merging.
    ///
    /// GitHub's merge API only atomically checks the head SHA, not the target or base.
    /// A retarget or base update can race the read and merge. Protect all possible target
    /// branches with appropriate required reviews and status checks, restrict retargeting
    /// and bypass permissions, and do not treat this method as an atomic guarantee.
    async fn merge_change(
        &self,
        number: u64,
        expected_head_sha: &str,
        expected_target_branch: &str,
        expected_base_sha: &str,
    ) -> Result<String>;
}

#[cfg(any(feature = "github", test))]
async fn ensure_approved_change(
    forge: &impl ChangeRequests,
    number: u64,
    expected_head_sha: &str,
    expected_target_branch: &str,
    expected_base_sha: &str,
) -> Result<()> {
    let change = forge.get_change(number).await?;
    if change.head_sha != expected_head_sha
        || change.target_branch != expected_target_branch
        || change.base_sha != expected_base_sha
    {
        return Err(Error::InvalidInput(
            "change no longer matches approved head, target, and base".into(),
        ));
    }
    Ok(())
}
#[async_trait]
pub trait Checks: Send + Sync {
    fn list_checks<'a>(&'a self, sha: &'a str) -> PageStream<'a, Check>;
    async fn combined_state(&self, sha: &str) -> Result<CombinedState>;
}
#[async_trait]
pub trait Comments: Send + Sync {
    fn list_comments(&self, number: u64) -> PageStream<'_, Comment>;
    async fn create_comment(&self, number: u64, body: &str) -> Result<Comment>;
    fn list_reviews(&self, number: u64) -> PageStream<'_, Review>;
}
/// Branch policy evidence is a point-in-time observation. It cannot guard a merge
/// against subsequent policy changes, bypass permissions, retargeting or base changes.
#[async_trait]
pub trait BranchPolicies: Send + Sync {
    async fn branch_policy(&self, branch: &str) -> Result<BranchPolicy>;
    async fn policy_actor(&self) -> Result<PolicyActor>;
}

pub trait Webhooks: Send + Sync {
    /// Verify the exact raw request body before parsing; unknown events return None.
    fn webhook(
        &self,
        event: &str,
        signature: &str,
        secret: &[u8],
        body: &[u8],
    ) -> Result<Option<WebhookEvent>>;
}

/// Dispatch over adapters compiled into this binary.
#[cfg(any(feature = "github", feature = "gitlab", feature = "forgejo"))]
pub enum Forge {
    #[cfg(feature = "github")]
    Github(github::Github),
    #[cfg(feature = "gitlab")]
    Gitlab(gitlab::Gitlab),
    #[cfg(feature = "forgejo")]
    Forgejo(forgejo::Forgejo),
}
#[cfg(any(feature = "github", feature = "gitlab", feature = "forgejo"))]
impl Forge {
    pub fn capabilities(&self) -> Capabilities {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.capabilities(),
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.capabilities(),
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.capabilities(),
        }
    }
}

// The enum delegates resource traits without exposing forge-specific branches to callers.
#[cfg(any(feature = "github", feature = "gitlab", feature = "forgejo"))]
#[async_trait]
impl ChangeRequests for Forge {
    async fn get_change(&self, n: u64) -> Result<ChangeRequest> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.get_change(n).await,
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.get_change(n).await,
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.get_change(n).await,
        }
    }
    fn list_changes(&self) -> PageStream<'_, ChangeRequest> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.list_changes(),
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.list_changes(),
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.list_changes(),
        }
    }
    async fn create_change(&self, r: &CreateChangeRequest) -> Result<ChangeRequest> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.create_change(r).await,
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.create_change(r).await,
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.create_change(r).await,
        }
    }
    async fn update_change(&self, n: u64, r: &UpdateChangeRequest) -> Result<ChangeRequest> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.update_change(n, r).await,
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.update_change(n, r).await,
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.update_change(n, r).await,
        }
    }
    async fn merge_change(&self, n: u64, head: &str, target: &str, base: &str) -> Result<String> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.merge_change(n, head, target, base).await,
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.merge_change(n, head, target, base).await,
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.merge_change(n, head, target, base).await,
        }
    }
}
#[cfg(any(feature = "github", feature = "gitlab", feature = "forgejo"))]
#[async_trait]
impl Checks for Forge {
    fn list_checks<'a>(&'a self, s: &'a str) -> PageStream<'a, Check> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.list_checks(s),
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.list_checks(s),
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.list_checks(s),
        }
    }
    async fn combined_state(&self, s: &str) -> Result<CombinedState> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.combined_state(s).await,
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.combined_state(s).await,
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.combined_state(s).await,
        }
    }
}
#[cfg(any(feature = "github", feature = "gitlab", feature = "forgejo"))]
#[async_trait]
impl Comments for Forge {
    fn list_comments(&self, n: u64) -> PageStream<'_, Comment> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.list_comments(n),
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.list_comments(n),
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.list_comments(n),
        }
    }
    async fn create_comment(&self, n: u64, b: &str) -> Result<Comment> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.create_comment(n, b).await,
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.create_comment(n, b).await,
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.create_comment(n, b).await,
        }
    }
    fn list_reviews(&self, n: u64) -> PageStream<'_, Review> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.list_reviews(n),
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.list_reviews(n),
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.list_reviews(n),
        }
    }
}
#[cfg(any(feature = "github", feature = "gitlab", feature = "forgejo"))]
#[async_trait]
impl BranchPolicies for Forge {
    async fn branch_policy(&self, branch: &str) -> Result<BranchPolicy> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.branch_policy(branch).await,
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.branch_policy(branch).await,
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.branch_policy(branch).await,
        }
    }
    async fn policy_actor(&self) -> Result<PolicyActor> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.policy_actor().await,
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.policy_actor().await,
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.policy_actor().await,
        }
    }
}
#[cfg(any(feature = "github", feature = "gitlab", feature = "forgejo"))]
impl Webhooks for Forge {
    fn webhook(&self, e: &str, s: &str, k: &[u8], b: &[u8]) -> Result<Option<WebhookEvent>> {
        match self {
            #[cfg(feature = "github")]
            Self::Github(g) => g.webhook(e, s, k, b),
            #[cfg(feature = "gitlab")]
            Self::Gitlab(g) => g.webhook(e, s, k, b),
            #[cfg(feature = "forgejo")]
            Self::Forgejo(g) => g.webhook(e, s, k, b),
        }
    }
}
