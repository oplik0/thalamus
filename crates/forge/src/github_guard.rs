//! Guarded GitHub merge decisions. All policy and review reads are snapshots;
//! only GitHub itself enforces its current branch protections at merge time.
use super::*;
use chrono::{DateTime, FixedOffset};
use futures_core::Stream;
use std::collections::{HashMap, HashSet};
use std::future::poll_fn;

fn invalid(message: &str) -> Error {
    Error::InvalidInput(message.into())
}

fn validate_request(github: &Github, request: &GuardedQueueRequest) -> Result<()> {
    if request.repository != github.repo || request.repository.forge != ForgeKind::Github {
        return Err(invalid(
            "guard repository does not match configured repository",
        ));
    }
    if request.number == 0 || request.number > i32::MAX as u64 {
        return Err(invalid("invalid pull request number"));
    }
    sha(&request.head_sha)?;
    sha(&request.base_sha)?;
    if request.target_branch.is_empty() || request.target_branch.chars().any(char::is_control) {
        return Err(invalid("invalid target branch"));
    }
    if !request.trust_administrators {
        return Err(invalid("administrator trust must be explicitly configured"));
    }
    let mut contexts = HashSet::new();
    if request.required_checks.is_empty()
        || request.required_checks.iter().any(|check| {
            check.context.trim().is_empty()
                || check.context.chars().any(char::is_control)
                || check.app_id == 0
                || check.status_creator_id == Some(0)
                || !contexts.insert(check.context.as_str())
        })
    {
        return Err(invalid(
            "required checks must have unique names and pinned app IDs",
        ));
    }
    let mut approvers = HashSet::new();
    if request.required_approvers.is_empty()
        || request.required_approvers.iter().any(|login| {
            login.is_empty()
                || !login
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-[]".contains(&c))
                || !approvers.insert(login.to_ascii_lowercase())
        })
    {
        return Err(invalid("required approvers must have unique GitHub logins"));
    }
    Ok(())
}

fn validate_change(change: &ChangeRequest, request: &GuardedQueueRequest) -> Result<()> {
    if change.number != request.number
        || change.state != ChangeState::Open
        || change.draft
        || change.head_sha != request.head_sha
        || change.base_sha != request.base_sha
        || change.target_branch != request.target_branch
        || change.source_repository.as_ref() != Some(&request.repository)
    {
        return Err(invalid(
            "pull request no longer matches the approved repository, head, target and base",
        ));
    }
    if request
        .required_approvers
        .iter()
        .any(|login| login.eq_ignore_ascii_case(&change.author))
    {
        return Err(invalid(
            "a pull request author cannot approve their own change",
        ));
    }
    Ok(())
}

fn pinned_check(value: &Value, app_key: &str) -> Result<RequiredCheck> {
    let context = text(value, "context")?;
    let app_id = number(value, app_key)?;
    if context.trim().is_empty() || app_id == 0 {
        return Err(invalid("required check is not pinned to a GitHub App"));
    }
    Ok(RequiredCheck {
        context: context.into(),
        app_id,
        status_creator_id: None,
    })
}

fn collect_checks(items: &Value, app_key: &str, checks: &mut HashSet<RequiredCheck>) -> Result<()> {
    let items = items
        .as_array()
        .ok_or_else(|| invalid("required checks are unavailable"))?;
    for item in items {
        checks.insert(pinned_check(item, app_key)?);
    }
    Ok(())
}

fn no_legacy_bypass(reviews: &Value, personal_repository: bool) -> Result<()> {
    match reviews.get("bypass_pull_request_allowances") {
        Some(allowances) if allowances.is_object() => {
            for kind in ["users", "teams", "apps"] {
                if !allowances
                    .get(kind)
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                {
                    return Err(invalid(
                        "branch protection has bypass actors or incomplete bypass visibility",
                    ));
                }
            }
        }
        // GitHub only offers user, team, and app bypass restrictions on
        // organization-owned repositories; the field may be absent for a user.
        None if personal_repository => (),
        _ => {
            return Err(invalid(
                "branch protection bypass allowances are unavailable",
            ));
        }
    }
    Ok(())
}

/// Validate all applicable protections, and return every required pinned check.
/// A ruleset-only branch is accepted only when the active rules independently
/// provide the mandatory check and review protections with no bypass actors.
fn validate_policy(
    policy: &BranchPolicy,
    request: &GuardedQueueRequest,
    queue_enabled: bool,
    personal_repository: bool,
) -> Result<HashSet<RequiredCheck>> {
    if policy.branch != request.target_branch {
        return Err(invalid("branch policy is for a different target"));
    }
    if queue_enabled && !request.trust_merge_group_ci {
        return Err(invalid(
            "merge-group CI trust must be explicitly configured",
        ));
    }
    let mut required = HashSet::new();
    let mut has_reviews = false;
    let mut has_strict_checks = false;
    let mut has_queue_rule = false;

    if !policy.protection.is_null() {
        let protection = &policy.protection;
        if !protection.is_object() {
            return Err(invalid("branch protection is unavailable"));
        }
        let legacy_reviews = protection
            .get("required_pull_request_reviews")
            .filter(|v| !v.is_null());
        let legacy_checks = protection
            .get("required_status_checks")
            .filter(|v| !v.is_null());
        if (legacy_reviews.is_some() || legacy_checks.is_some())
            && protection["enforce_admins"]["enabled"].as_bool() != Some(true)
        {
            return Err(invalid(
                "branch protections must be enforced for administrators",
            ));
        }
        if let Some(reviews) = legacy_reviews {
            if reviews["dismiss_stale_reviews"].as_bool() != Some(true)
                || reviews["required_approving_review_count"]
                    .as_u64()
                    .unwrap_or(0)
                    == 0
            {
                return Err(invalid("branch protection must require fresh approvals"));
            }
            no_legacy_bypass(reviews, personal_repository)?;
            has_reviews = true;
        }
        if let Some(checks) = legacy_checks {
            let strict = checks["strict"].as_bool() == Some(true);
            if !strict && !queue_enabled {
                return Err(invalid("branch protection must require up-to-date checks"));
            }
            let before = required.len();
            collect_checks(&checks["checks"], "app_id", &mut required)?;
            let contexts = checks["contexts"]
                .as_array()
                .ok_or_else(|| invalid("required check contexts are unavailable"))?;
            for context in contexts {
                let context = context
                    .as_str()
                    .ok_or_else(|| invalid("invalid required check context"))?;
                if !required.iter().any(|check| check.context == context) {
                    return Err(invalid("required check context has no pinned app"));
                }
            }
            has_strict_checks |= required.len() > before;
        }
    }

    let details: HashMap<_, _> = policy
        .rulesets
        .iter()
        .map(|detail| Ok((number(detail, "id")?, detail)))
        .collect::<Result<_>>()?;
    for rule in &policy.rules {
        let id = number(rule, "ruleset_id")?;
        let detail = details
            .get(&id)
            .ok_or_else(|| invalid("applicable ruleset detail is unavailable"))?;
        if text(detail, "enforcement")? != "active"
            || !detail
                .get("bypass_actors")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            || detail
                .get("current_user_can_bypass")
                .is_some_and(|value| value.as_str() != Some("never"))
        {
            return Err(invalid("applicable ruleset is inactive or can be bypassed"));
        }
        match text(rule, "type")? {
            "pull_request" => {
                let p = nested(rule, "parameters")?;
                if p["dismiss_stale_reviews_on_push"].as_bool() != Some(true)
                    || p["required_approving_review_count"].as_u64().unwrap_or(0) == 0
                {
                    return Err(invalid("ruleset must require fresh approvals"));
                }
                has_reviews = true;
            }
            "required_status_checks" => {
                let p = nested(rule, "parameters")?;
                if p["strict_required_status_checks_policy"].as_bool() != Some(true)
                    && !queue_enabled
                {
                    return Err(invalid("ruleset must require up-to-date checks"));
                }
                let items = p["required_status_checks"]
                    .as_array()
                    .ok_or_else(|| invalid("ruleset checks are unavailable"))?;
                if !items.is_empty() {
                    has_strict_checks = true;
                }
                collect_checks(
                    &p["required_status_checks"],
                    "integration_id",
                    &mut required,
                )?;
            }
            "merge_queue" => {
                has_queue_rule = true;
                if rule["parameters"]["grouping_strategy"].as_str() != Some("ALLGREEN") {
                    return Err(invalid(
                        "merge queue must require checks for every queued change",
                    ));
                }
            }
            _ => (),
        }
    }
    if has_queue_rule && !queue_enabled {
        return Err(invalid("merge queue policy and pull request disagree"));
    }
    if !has_reviews || !has_strict_checks || required.is_empty() {
        return Err(invalid(
            "target lacks enforced fresh reviews and required up-to-date checks",
        ));
    }
    for check in &request.required_checks {
        if !required
            .iter()
            .any(|p| p.context == check.context && p.app_id == check.app_id)
        {
            return Err(invalid(
                "operator-pinned check is not required by upstream policy",
            ));
        }
    }
    if required.iter().any(|p| {
        !request
            .required_checks
            .iter()
            .any(|check| p.context == check.context && p.app_id == check.app_id)
    }) {
        return Err(invalid("an upstream required check is not operator-pinned"));
    }
    Ok(request.required_checks.iter().cloned().collect())
}

fn validate_reviews(reviews: &[Review], request: &GuardedQueueRequest) -> Result<()> {
    let mut latest: HashMap<String, (DateTime<FixedOffset>, u64, &Review)> = HashMap::new();
    for review in reviews {
        match review.state.as_str() {
            "PENDING" | "COMMENTED" => continue,
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED" => (),
            _ => return Err(invalid("unknown review state")),
        }
        let submitted = review
            .submitted_at
            .as_deref()
            .ok_or_else(|| invalid("review submission time unavailable"))?;
        let time = DateTime::parse_from_rfc3339(submitted)
            .map_err(|_| invalid("invalid review submission time"))?;
        let key = review.author.to_ascii_lowercase();
        match latest.get(&key) {
            Some((old_time, old_id, _)) if (*old_time, *old_id) >= (time, review.id) => (),
            _ => {
                latest.insert(key, (time, review.id, review));
            }
        }
    }
    for approver in &request.required_approvers {
        let review = latest
            .get(&approver.to_ascii_lowercase())
            .map(|(_, _, review)| *review)
            .ok_or_else(|| invalid("required approver has not reviewed the current head"))?;
        if review.state != "APPROVED"
            || review.commit_sha.as_deref() != Some(request.head_sha.as_str())
        {
            return Err(invalid(
                "required approver's latest review is not an approval of the current head",
            ));
        }
    }
    Ok(())
}

fn latest_status<'a>(statuses: &'a [Value], context: &str) -> Option<&'a Value> {
    statuses
        .iter()
        .find(|status| status.get("context").and_then(Value::as_str) == Some(context))
}

fn validate_check_results(
    head_runs: &[Value],
    head_statuses: &[Value],
    merge: Option<(&str, &[Value], &[Value])>,
    required: &HashSet<RequiredCheck>,
    head: &str,
) -> Result<()> {
    for required_check in required {
        let context = required_check.context.as_str();
        // A test-merge context takes precedence over the head context. Statuses
        // are newest-first; their creator is a bot user ID, not the App ID.
        if let Some(creator_id) = required_check.status_creator_id {
            let head_status = latest_status(head_statuses, context)
                .ok_or_else(|| invalid("required head commit status is unavailable"))?;
            let merge_status = merge.and_then(|(_, _, statuses)| latest_status(statuses, context));
            for status in std::iter::once(head_status).chain(merge_status) {
                if status.get("state").and_then(Value::as_str) != Some("success")
                    || status
                        .get("creator")
                        .and_then(|v| v.get("id"))
                        .and_then(Value::as_u64)
                        != Some(creator_id)
                    || status
                        .get("creator")
                        .and_then(|v| v.get("type"))
                        .and_then(Value::as_str)
                        != Some("Bot")
                {
                    return Err(invalid(
                        "latest required commit status did not succeed from the pinned bot",
                    ));
                }
            }
        }
        let matches = |run: &Value| {
            run.get("name").and_then(Value::as_str) == Some(context)
                && run
                    .get("app")
                    .and_then(|app| app.get("id"))
                    .and_then(Value::as_u64)
                    == Some(required_check.app_id)
        };
        let (commit, runs) = match merge {
            Some((sha, runs, _)) if runs.iter().any(&matches) => (sha, runs),
            _ => (head, head_runs),
        };
        let mut found_run = false;
        for run in runs.iter().filter(|run| matches(run)) {
            found_run = true;
            if text(run, "head_sha")? != commit
                || text(run, "status")? != "completed"
                || run.get("conclusion").and_then(Value::as_str) != Some("success")
            {
                return Err(invalid(
                    "required check has not succeeded on the current pull request",
                ));
            }
        }
        if !found_run && required_check.status_creator_id.is_none() {
            return Err(invalid(
                "required check run from the pinned app is unavailable",
            ));
        }
    }
    Ok(())
}

struct PullQueue {
    id: String,
    enabled: bool,
    entry: Option<QueueEntry>,
}

fn ensure_queue_allowed(enabled: bool, allow_queue: bool) -> Result<()> {
    if enabled && !allow_queue {
        return Err(Error::Unsupported(
            "scheduled in-place merge queue is incompatible".into(),
        ));
    }
    Ok(())
}

impl Github {
    async fn pull_queue(
        &self,
        number: u64,
        expected: Option<&GuardedQueueRequest>,
        inspect_queue_policy: bool,
    ) -> Result<PullQueue> {
        let query = if expected.is_some() && inspect_queue_policy {
            "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){nameWithOwner pullRequest(number:$number){id state isDraft baseRefName baseRefOid headRefOid isMergeQueueEnabled mergeQueue{configuration{mergingStrategy}} mergeQueueEntry{id}}}}"
        } else {
            "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){nameWithOwner pullRequest(number:$number){id state isDraft baseRefName baseRefOid headRefOid isMergeQueueEnabled mergeQueueEntry{id}}}}"
        };
        let data: Value = self.graphql(&json!({
            "query": query,
            "variables": {"owner": self.repo.namespace, "repo": self.repo.name, "number": number}
        })).await?;
        let repo = nested(&data, "repository")?;
        if text(repo, "nameWithOwner")? != format!("{}/{}", self.repo.namespace, self.repo.name) {
            return Err(invalid("GraphQL repository identity mismatch"));
        }
        let pull = nested(repo, "pullRequest")?;
        if let Some(expected) = expected
            && (text(pull, "state")? != "OPEN"
                || pull.get("isDraft").and_then(Value::as_bool) != Some(false)
                || text(pull, "baseRefName")? != expected.target_branch
                || text(pull, "baseRefOid")? != expected.base_sha
                || text(pull, "headRefOid")? != expected.head_sha)
        {
            return Err(invalid(
                "GraphQL pull request no longer matches the approved target and commits",
            ));
        }
        let id = text(pull, "id")?.to_owned();
        if id.is_empty() {
            return Err(Error::Malformed("empty pull request node ID".into()));
        }
        let enabled = pull
            .get("isMergeQueueEnabled")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::Malformed("merge queue state unavailable".into()))?;
        if expected.is_some()
            && inspect_queue_policy
            && enabled
            && pull["mergeQueue"]["configuration"]["mergingStrategy"].as_str() != Some("ALLGREEN")
        {
            return Err(invalid(
                "merge queue must require checks for every queued change",
            ));
        }
        let entry = match pull.get("mergeQueueEntry") {
            Some(Value::Null) => None,
            Some(entry) => {
                let id = text(entry, "id")?;
                if id.is_empty() {
                    return Err(Error::Malformed("empty merge queue entry ID".into()));
                }
                Some(QueueEntry { id: id.into() })
            }
            None => return Err(Error::Malformed("merge queue entry unavailable".into())),
        };
        Ok(PullQueue { id, enabled, entry })
    }

    async fn fresh_change(&self, request: &GuardedQueueRequest) -> Result<ChangeRequest> {
        let value = self
            .get(&self.route(&format!("/pulls/{}", request.number)))
            .await?;
        let base_repo = nested(nested(&value, "base")?, "repo")?;
        if text(base_repo, "full_name")? != format!("{}/{}", self.repo.namespace, self.repo.name) {
            return Err(invalid(
                "pull request target repository does not match configuration",
            ));
        }
        let current = change(&value, &self.repo.host)?;
        validate_change(&current, request)?;
        Ok(current)
    }

    async fn check_evidence(&self, commit: &str) -> Result<(Vec<Value>, Vec<Value>)> {
        sha(commit)?;
        let mut runs = Vec::new();
        let mut run_stream = pages(
            self,
            self.route(&format!("/commits/{commit}/check-runs?filter=latest")),
            Some("check_runs"),
            |v| Ok(v.clone()),
        );
        while let Some(item) = poll_fn(|cx| Stream::poll_next(run_stream.as_mut(), cx)).await {
            runs.push(item?);
        }
        let mut statuses = Vec::new();
        let mut status_stream = pages(
            self,
            self.route(&format!("/commits/{commit}/statuses")),
            None,
            |v| Ok(v.clone()),
        );
        while let Some(item) = poll_fn(|cx| Stream::poll_next(status_stream.as_mut(), cx)).await {
            statuses.push(item?);
        }
        Ok((runs, statuses))
    }

    async fn guarded_policy(
        &self,
        request: &GuardedQueueRequest,
        allow_queue: bool,
    ) -> Result<(HashSet<RequiredCheck>, PullQueue)> {
        let repository = self.get(&self.route("")).await?;
        if text(&repository, "full_name")? != format!("{}/{}", self.repo.namespace, self.repo.name)
        {
            return Err(invalid("repository identity mismatch"));
        }
        let personal_repository = match text(nested(&repository, "owner")?, "type")? {
            "User" => true,
            "Organization" => false,
            _ => return Err(invalid("unknown repository owner type")),
        };
        let policy = self.branch_policy(&request.target_branch).await?;
        let queue = self
            .pull_queue(request.number, Some(request), allow_queue)
            .await?;
        ensure_queue_allowed(queue.enabled || queue.entry.is_some(), allow_queue)?;
        let required = validate_policy(&policy, request, queue.enabled, personal_repository)?;
        Ok((required, queue))
    }
    async fn merge_guarded_inner(
        &self,
        request: &GuardedQueueRequest,
        allow_queue: bool,
    ) -> Result<GuardedMergeResult> {
        validate_request(self, request)?;
        let initial = self.fresh_change(request).await?;
        let (required, queue) = self.guarded_policy(request, allow_queue).await?;

        let (head_runs, head_statuses) = self.check_evidence(&request.head_sha).await?;
        let merge_evidence = match initial.merge_commit.as_deref() {
            Some(commit) if commit != request.head_sha => Some(self.check_evidence(commit).await?),
            _ => None,
        };
        let merge = initial
            .merge_commit
            .as_deref()
            .zip(merge_evidence.as_ref())
            .map(|(commit, (runs, statuses))| (commit, runs.as_slice(), statuses.as_slice()));
        validate_check_results(
            &head_runs,
            &head_statuses,
            merge,
            &required,
            &request.head_sha,
        )?;

        let mut reviews = Vec::new();
        let mut review_stream = self.list_reviews(request.number);
        while let Some(item) = poll_fn(|cx| Stream::poll_next(review_stream.as_mut(), cx)).await {
            reviews.push(item?);
        }
        validate_reviews(&reviews, request)?;

        // These reads are not locks. GitHub's head condition and its current
        // branch policy are the only upstream enforcement at the actual action.
        let current = self.fresh_change(request).await?;
        if current.merge_commit != initial.merge_commit {
            return Err(invalid(
                "pull request test merge commit changed while collecting evidence",
            ));
        }
        let queue_now = self
            .pull_queue(request.number, Some(request), allow_queue)
            .await?;
        ensure_queue_allowed(queue_now.enabled || queue_now.entry.is_some(), allow_queue)?;
        if queue_now.enabled != queue.enabled {
            return Err(invalid(
                "merge queue policy changed while collecting evidence",
            ));
        }
        let queue = queue_now;
        if queue.enabled {
            if !request.trust_merge_group_ci {
                return Err(invalid(
                    "merge-group CI trust must be explicitly configured",
                ));
            }
            if let Some(entry) = queue.entry {
                return Ok(GuardedMergeResult::Enqueued(entry));
            }
            let data: Value = self.graphql(&json!({
                "query": "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{id}}}",
                "variables": {"id": queue.id, "head": request.head_sha}
            })).await?;
            let entry = nested(nested(&data, "enqueuePullRequest")?, "mergeQueueEntry")?;
            let id = text(entry, "id")?;
            if id.is_empty() {
                return Err(Error::Malformed("empty merge queue entry ID".into()));
            }
            Ok(GuardedMergeResult::Enqueued(QueueEntry { id: id.into() }))
        } else {
            let value: Value = self
                .client
                .put(
                    self.route(&format!("/pulls/{}/merge", request.number)),
                    Some(&json!({"sha": request.head_sha})),
                )
                .await?;
            if value.get("merged").and_then(Value::as_bool) != Some(true) {
                return Err(Error::Malformed("merge was not performed".into()));
            }
            Ok(GuardedMergeResult::Merged {
                sha: text(&value, "sha")?.into(),
            })
        }
    }
}

#[async_trait]
impl GuardedQueue for Github {
    async fn verify_update_policy(&self, request: &GuardedQueueRequest) -> Result<()> {
        validate_request(self, request)?;
        self.fresh_change(request).await?;
        let (_, queue) = self.guarded_policy(request, true).await?;
        if queue.entry.is_some() {
            return Err(invalid("dequeue the pull request before updating it"));
        }
        self.fresh_change(request).await?;
        let current = self.pull_queue(request.number, Some(request), true).await?;
        if current.enabled != queue.enabled || current.entry.is_some() {
            return Err(invalid("merge queue changed while checking update policy"));
        }
        Ok(())
    }
    async fn merge_guarded_direct(
        &self,
        request: &GuardedQueueRequest,
    ) -> Result<GuardedMergeResult> {
        self.merge_guarded_inner(request, false).await
    }

    async fn merge_guarded(&self, request: &GuardedQueueRequest) -> Result<GuardedMergeResult> {
        self.merge_guarded_inner(request, true).await
    }

    async fn dequeue(&self, repository: &RepoRef, number: u64) -> Result<()> {
        if repository != &self.repo || number == 0 || number > i32::MAX as u64 {
            return Err(invalid(
                "dequeue repository or pull request does not match configuration",
            ));
        }
        let queue = self.pull_queue(number, None, false).await?;
        if queue.entry.is_none() {
            return Ok(());
        }
        let data: Value = self.graphql(&json!({
            "query": "mutation($id:ID!){dequeuePullRequest(input:{id:$id}){mergeQueueEntry{id}}}",
            "variables": {"id": queue.id}
        })).await?;
        let entry = nested(nested(&data, "dequeuePullRequest")?, "mergeQueueEntry")?;
        if text(entry, "id")?.is_empty() {
            return Err(Error::Malformed("empty dequeued entry ID".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const BASE: &str = "2222222222222222222222222222222222222222";
    const MERGE: &str = "3333333333333333333333333333333333333333";

    fn request() -> GuardedQueueRequest {
        GuardedQueueRequest {
            repository: RepoRef::parse(ForgeKind::Github, "https://github.com/team/catalog")
                .unwrap(),
            number: 42,
            target_branch: "main".into(),
            head_sha: HEAD.into(),
            base_sha: BASE.into(),
            required_checks: vec![
                RequiredCheck {
                    context: "build".into(),
                    app_id: 12,
                    status_creator_id: None,
                },
                RequiredCheck {
                    context: "thalamus/rollout-window".into(),
                    app_id: 34,
                    status_creator_id: Some(567),
                },
            ],
            required_approvers: vec!["owner".into()],
            trust_administrators: true,
            trust_merge_group_ci: true,
        }
    }

    fn legacy_policy() -> BranchPolicy {
        BranchPolicy {
            branch: "main".into(),
            protection: json!({
                "enforce_admins": {"enabled": true},
                "required_pull_request_reviews": {
                    "dismiss_stale_reviews": true,
                    "required_approving_review_count": 1,
                    "bypass_pull_request_allowances": {"users": [], "teams": [], "apps": []}
                },
                "required_status_checks": {
                    "strict": true,
                    "contexts": ["build", "thalamus/rollout-window"],
                    "checks": [
                        {"context": "build", "app_id": 12},
                        {"context": "thalamus/rollout-window", "app_id": 34}
                    ]
                }
            }),
            rules: vec![],
            rulesets: vec![],
        }
    }

    fn ruleset_policy() -> BranchPolicy {
        BranchPolicy {
            branch: "main".into(),
            protection: Value::Null,
            rules: vec![
                json!({"ruleset_id": 1, "type": "pull_request", "parameters": {
                    "dismiss_stale_reviews_on_push": true, "required_approving_review_count": 1
                }}),
                json!({"ruleset_id": 1, "type": "required_status_checks", "parameters": {
                    "strict_required_status_checks_policy": false,
                    "required_status_checks": [
                        {"context": "build", "integration_id": 12},
                        {"context": "thalamus/rollout-window", "integration_id": 34}
                    ]
                }}),
                json!({"ruleset_id": 1, "type": "merge_queue", "parameters": {
                    "grouping_strategy": "ALLGREEN"
                }}),
            ],
            rulesets: vec![json!({
                "id": 1, "enforcement": "active", "bypass_actors": [],
                "current_user_can_bypass": "never"
            })],
        }
    }

    fn reviewed(id: u64, state: &str, commit: &str, submitted: &str) -> Review {
        Review {
            id,
            author: "owner".into(),
            state: state.into(),
            commit_sha: Some(commit.into()),
            submitted_at: Some(submitted.into()),
            body: None,
            url: "https://github.com/team/catalog/pull/42".into(),
        }
    }

    fn run(commit: &str, app_id: u64, conclusion: &str) -> Value {
        json!({
            "name": "build", "head_sha": commit, "app": {"id": app_id},
            "status": "completed", "conclusion": conclusion
        })
    }

    fn status(creator: u64, state: &str) -> Value {
        json!({
            "context": "thalamus/rollout-window", "state": state,
            "creator": {"id": creator, "type": "Bot"}
        })
    }

    #[test]
    fn legacy_policy_requires_enforced_fresh_reviews_and_pinned_checks() {
        let mut policy = legacy_policy();
        let request = request();
        assert!(validate_policy(&policy, &request, false, false).is_ok());
        policy.protection["enforce_admins"]["enabled"] = json!(false);
        assert!(validate_policy(&policy, &request, false, false).is_err());
        policy = legacy_policy();
        policy.protection["required_pull_request_reviews"]["dismiss_stale_reviews"] = json!(false);
        assert!(validate_policy(&policy, &request, false, false).is_err());
        policy = legacy_policy();
        policy.protection["required_status_checks"]["strict"] = json!(false);
        assert!(validate_policy(&policy, &request, false, false).is_err());
        policy = legacy_policy();
        policy.protection["required_status_checks"]["checks"][0]["app_id"] = Value::Null;
        assert!(validate_policy(&policy, &request, false, false).is_err());
        policy = legacy_policy();
        policy.protection["required_pull_request_reviews"]["bypass_pull_request_allowances"]["apps"] =
            json!([{"id": 34}]);
        assert!(validate_policy(&policy, &request, false, false).is_err());
    }

    #[test]
    fn legacy_bypass_visibility_and_all_required_contexts_are_enforced() {
        let mut policy = legacy_policy();
        policy.protection["required_pull_request_reviews"]
            .as_object_mut()
            .unwrap()
            .remove("bypass_pull_request_allowances");
        assert!(validate_policy(&policy, &request(), false, false).is_err());
        assert!(validate_policy(&policy, &request(), false, true).is_ok());
        let mut configured = request();
        configured.required_checks.pop();
        assert!(validate_policy(&legacy_policy(), &configured, false, false).is_err());
        configured = request();
        configured.required_checks[0].app_id = 99;
        assert!(validate_policy(&legacy_policy(), &configured, false, false).is_err());
    }

    #[test]
    fn ruleset_queue_requires_trust_visible_no_bypass_and_all_green() {
        let mut policy = ruleset_policy();
        let mut request = request();
        assert!(validate_policy(&policy, &request, true, false).is_ok());
        assert!(validate_policy(&policy, &request, false, false).is_err());
        request.trust_merge_group_ci = false;
        assert!(validate_policy(&policy, &request, true, false).is_err());
        request.trust_merge_group_ci = true;
        policy.rulesets[0]["bypass_actors"] =
            json!([{"actor_type": "Integration", "actor_id": 99}]);
        assert!(validate_policy(&policy, &request, true, false).is_err());
        policy = ruleset_policy();
        policy.rulesets[0]["current_user_can_bypass"] = json!("always");
        assert!(validate_policy(&policy, &request, true, false).is_err());
        policy = ruleset_policy();
        policy.rules[2]["parameters"]["grouping_strategy"] = json!("HEADGREEN");
        assert!(validate_policy(&policy, &request, true, false).is_err());
        policy = ruleset_policy();
        policy.rulesets.clear();
        assert!(validate_policy(&policy, &request, true, false).is_err());
    }

    #[test]
    fn direct_guard_rejects_required_queue_without_enqueue() {
        assert!(ensure_queue_allowed(false, false).is_ok());
        assert!(ensure_queue_allowed(false, true).is_ok());
        assert!(ensure_queue_allowed(true, true).is_ok());
        assert!(matches!(
            ensure_queue_allowed(true, false),
            Err(Error::Unsupported(message)) if message == "scheduled in-place merge queue is incompatible"
        ));
    }

    #[tokio::test]
    async fn guard_rejects_untrusted_or_mismatched_request_and_pull() {
        let mut input = request();
        let github =
            Github::from_client(input.repository.clone(), octocrab::Octocrab::default()).unwrap();
        assert!(validate_request(&github, &input).is_ok());
        input.trust_administrators = false;
        assert!(validate_request(&github, &input).is_err());
        input.trust_administrators = true;
        input.required_checks[1].status_creator_id = Some(0);
        assert!(validate_request(&github, &input).is_err());
        input = request();
        let mut change = ChangeRequest {
            number: 42,
            title: "proposal".into(),
            body: None,
            source_branch: "feature".into(),
            target_branch: "main".into(),
            head_sha: HEAD.into(),
            base_sha: BASE.into(),
            state: ChangeState::Open,
            draft: false,
            mergeability: Mergeability::Mergeable,
            merge_commit: None,
            merged_at: None,
            source_repository: Some(input.repository.clone()),
            author: "author".into(),
            labels: vec![],
            url: "https://github.com/team/catalog/pull/42".into(),
        };
        assert!(validate_change(&change, &input).is_ok());
        change.target_branch = "other".into();
        assert!(validate_change(&change, &input).is_err());
        change.target_branch = "main".into();
        change.base_sha = HEAD.into();
        assert!(validate_change(&change, &input).is_err());
        change.base_sha = BASE.into();
        change.head_sha = BASE.into();
        assert!(validate_change(&change, &input).is_err());
        change.head_sha = HEAD.into();
        change.source_repository = None;
        assert!(validate_change(&change, &input).is_err());
    }

    #[test]
    fn latest_approval_must_cover_exact_head() {
        let request = request();
        let approval = reviewed(2, "APPROVED", HEAD, "2026-10-01T00:00:00Z");
        assert!(validate_reviews(std::slice::from_ref(&approval), &request).is_ok());
        let comment = reviewed(3, "COMMENTED", HEAD, "2026-10-02T00:00:00Z");
        assert!(validate_reviews(&[approval.clone(), comment], &request).is_ok());
        let dismissed = reviewed(4, "DISMISSED", HEAD, "2026-10-03T00:00:00Z");
        assert!(validate_reviews(&[approval.clone(), dismissed], &request).is_err());
        let changes = reviewed(5, "CHANGES_REQUESTED", HEAD, "2026-10-03T00:00:00Z");
        assert!(validate_reviews(&[changes, approval], &request).is_err());
        let stale = reviewed(6, "APPROVED", BASE, "2026-10-04T00:00:00Z");
        assert!(validate_reviews(&[stale], &request).is_err());
    }

    #[test]
    fn current_check_runs_and_latest_bot_status_are_required() {
        let checks: HashSet<_> = request().required_checks.into_iter().collect();
        let runs = vec![run(HEAD, 12, "success")];
        let statuses = vec![status(567, "success"), status(567, "failure")];
        assert!(validate_check_results(&runs, &statuses, None, &checks, HEAD).is_ok());
        assert!(
            validate_check_results(
                &runs,
                &[status(567, "pending"), status(567, "success")],
                None,
                &checks,
                HEAD
            )
            .is_err()
        );
        assert!(
            validate_check_results(&runs, &[status(99, "success")], None, &checks, HEAD).is_err()
        );
        assert!(
            validate_check_results(&[run(HEAD, 99, "success")], &statuses, None, &checks, HEAD)
                .is_err()
        );
        assert!(
            validate_check_results(&[run(HEAD, 12, "skipped")], &statuses, None, &checks, HEAD)
                .is_err()
        );
    }

    #[test]
    fn current_test_merge_check_takes_precedence_over_head() {
        let checks: HashSet<_> = request().required_checks.into_iter().collect();
        let head_runs = vec![run(HEAD, 12, "failure")];
        let merge_runs = vec![run(MERGE, 12, "success")];
        let statuses = vec![status(567, "success")];
        assert!(
            validate_check_results(
                &head_runs,
                &statuses,
                Some((MERGE, &merge_runs, &[])),
                &checks,
                HEAD
            )
            .is_ok()
        );
        let bad_merge = vec![run(MERGE, 12, "failure")];
        assert!(
            validate_check_results(
                &[run(HEAD, 12, "success")],
                &statuses,
                Some((MERGE, &bad_merge, &[])),
                &checks,
                HEAD
            )
            .is_err()
        );
        assert!(
            validate_check_results(
                &[run(HEAD, 12, "success")],
                &statuses,
                Some((MERGE, &[], &[status(567, "failure")])),
                &checks,
                HEAD
            )
            .is_err()
        );
    }
}
