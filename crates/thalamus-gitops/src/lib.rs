//! Git-backed previews and changes to YAML catalog source files.
//!
//! Git and the operator-configured catalog renderer are invoked directly,
//! without a shell. The renderer runs in each worktree and owns the derived
//! inference and pool output trees. The caller supplies credentials, manages
//! worktree lifecycle, and serializes access to a worktree. Publication
//! requires an operator-configured forge repository and target branch binding.
//!
//! To regenerate a preview after the base moves, fetch and resolve the base,
//! create a new worktree at that commit, and reapply the requested patches.
//! Existing worktrees remain pinned to their original bases.

use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use thiserror::Error;
use url::{Host, Url};

#[cfg(test)]
mod tests;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("{program} failed ({status}): {stderr}")]
    Process {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("service not found in pinned catalog")]
    ServiceNotFound,
    #[error("command output was not UTF-8")]
    Utf8(#[from] std::string::FromUtf8Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A path is a sequence of YAML mapping keys or decimal sequence indices.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Patch {
    pub file: PathBuf,
    pub path: Vec<String>,
    pub operation: Operation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", content = "value", rename_all = "snake_case")]
pub enum Operation {
    Set(Value),
    Remove,
}

/// Renderer executable and arguments, run from the root of each worktree.
#[derive(Clone, Debug)]
pub struct RenderCommand {
    pub executable: OsString,
    pub args: Vec<OsString>,
}

#[derive(Debug)]
pub struct Preview {
    pub base_render: String,
    pub proposed_render: String,
    pub data_diff: String,
    pub rendered_diff: String,
}

#[derive(Clone, Debug)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
}

#[derive(Debug)]
pub struct PublishedCommit {
    pub branch: String,
    pub commit: String,
}

#[derive(Clone, Debug)]
pub struct GitOpsRepo {
    bare: PathBuf,
    binding: Option<OriginBinding>,
}

#[derive(Debug)]
pub struct ChangeWorktree {
    path: PathBuf,
    base: String,
    changed_files: Vec<PathBuf>,
    preview_hashes: BTreeMap<PathBuf, Option<[u8; 32]>>,
    generated_hashes: BTreeMap<PathBuf, [u8; 32]>,
    repository: PathBuf,
    binding: Option<OriginBinding>,
    owned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RepositoryIdentity {
    host: String,
    port: Option<u16>,
    path: String,
}

#[derive(Clone, Debug)]
struct OriginBinding {
    repository: RepositoryIdentity,
    target_ref: String,
}

struct RemoteUrls {
    fetch: String,
    push: String,
}

fn run(
    program: &OsStr,
    args: &[OsString],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
) -> Result<Output> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(Error::Process {
            program: program.to_string_lossy().into_owned(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output)
}

fn git(cwd: &Path, args: &[OsString]) -> Result<String> {
    Ok(String::from_utf8(
        run(
            OsStr::new("git"),
            args,
            Some(cwd),
            &[("GIT_NO_REPLACE_OBJECTS", "1")],
        )?
        .stdout,
    )?)
}

// Apply an inverse source-only patch with Git's three-way conflict checks.
// Derived files are regenerated rather than reverted as independent inputs.
fn git_apply_inverse(cwd: &Path, patch: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new("git")
        .args(["apply", "--3way", "--index", "-"])
        .current_dir(cwd)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Invalid("cannot pipe inverse patch to Git".into()))?;
    let (write_result, output) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(patch));
        let output = child.wait_with_output();
        (writer.join(), output)
    });
    let output = output?;
    if !output.status.success() {
        return Err(Error::Process {
            program: "git".into(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    write_result.map_err(|_| Error::Invalid("inverse patch writer failed".into()))??;
    Ok(())
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn verify_commit(cwd: &Path, commit: &str) -> Result<()> {
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid(
            "commit must be a full hexadecimal ID".into(),
        ));
    }
    let resolved = run(
        OsStr::new("git"),
        &args(&["rev-parse", "--verify", &format!("{commit}^{{commit}}")]),
        Some(cwd),
        &[("GIT_NO_REPLACE_OBJECTS", "1")],
    )?;
    if !String::from_utf8(resolved.stdout)?
        .trim()
        .eq_ignore_ascii_case(commit)
    {
        return Err(Error::Invalid(
            "ID must identify a commit, not a tag".into(),
        ));
    }
    Ok(())
}

// Match the forge's host, optional port and repository path, not merely a
// suffix of a URL. Reject URL normalization and credential ambiguities.
fn repository_identity(input: &str) -> Result<RepositoryIdentity> {
    let invalid = || Error::Invalid("invalid forge repository URL".into());
    if input.is_empty()
        || !input.is_ascii()
        || input
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b"%\\?#".contains(&b))
    {
        return Err(invalid());
    }
    let address = if input.starts_with("https://") || input.starts_with("ssh://") {
        Cow::Borrowed(input)
    } else if !input.contains("://") {
        let (authority, path) = input.split_once(':').ok_or_else(invalid)?;
        if authority.is_empty() || path.is_empty() || path.starts_with('/') {
            return Err(invalid());
        }
        Cow::Owned(format!("ssh://{authority}/{path}"))
    } else {
        return Err(invalid());
    };
    let (scheme, rest) = address.split_once("://").ok_or_else(invalid)?;
    let (authority, path) = rest.split_once('/').ok_or_else(invalid)?;
    if authority.is_empty() || path.is_empty() || authority.matches('@').count() > 1 {
        return Err(invalid());
    }
    if let Some((user, host)) = authority.split_once('@')
        && (scheme != "ssh"
            || user.is_empty()
            || host.is_empty()
            || !user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
    {
        return Err(invalid());
    }
    if !path.contains('/')
        || path.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err(invalid());
    }
    let canonical_path = path.strip_suffix(".git").unwrap_or(path);
    let name = canonical_path.rsplit('/').next().unwrap_or("");
    if name.is_empty() || name == "." || name == ".." {
        return Err(invalid());
    }
    let url = Url::parse(&address).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "https" | "ssh")
        || url.password().is_some()
        || (url.scheme() == "https" && !url.username().is_empty())
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().strip_prefix('/') != Some(path)
    {
        return Err(invalid());
    }
    match url.host().ok_or_else(invalid)? {
        Host::Domain(host) => {
            if host.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }) {
                return Err(invalid());
            }
        }
        Host::Ipv4(_) | Host::Ipv6(_) => {}
    }
    Ok(RepositoryIdentity {
        host: url.host_str().ok_or_else(invalid)?.to_ascii_lowercase(),
        port: url.port(),
        path: canonical_path.to_owned(),
    })
}

// Git expands insteadOf/pushInsteadOf when resolving origin. We use the
// resolved URL as an explicit transport argument, so it must not itself be
// rewritten a second time. Also forbid per-URL redirect settings that could
// override the redirect-disabled command-line setting.
fn ensure_stable_transport_urls(cwd: &Path, urls: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args([
            "config",
            "--null",
            "--get-regexp",
            r"^(url\..*\.(insteadof|pushinsteadof)|http\..*\.followredirects)$",
        ])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stdout.is_empty() {
            return Ok(());
        }
        return Err(Error::Process {
            program: "git".into(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let config = String::from_utf8(output.stdout)?;
    for entry in config.split_terminator('\0') {
        let (key, value) = entry
            .split_once('\n')
            .ok_or_else(|| Error::Invalid("invalid Git URL configuration".into()))?;
        if key.starts_with("url.") {
            if value.is_empty() || urls.iter().any(|url| url.starts_with(value)) {
                return Err(Error::Invalid(
                    "resolved Git URL would be rewritten again".into(),
                ));
            }
        } else if !value.eq_ignore_ascii_case("false")
            && urls.iter().any(|url| url.starts_with("https://"))
        {
            return Err(Error::Invalid(
                "per-URL Git HTTP redirects must be disabled".into(),
            ));
        }
    }
    Ok(())
}

fn verified_origin(cwd: &Path, binding: &OriginBinding) -> Result<RemoteUrls> {
    let fetch = git(cwd, &args(&["remote", "get-url", "--all", "origin"]))?;
    let push = git(
        cwd,
        &args(&["remote", "get-url", "--push", "--all", "origin"]),
    )?;
    let fetch = fetch
        .strip_suffix('\n')
        .ok_or_else(|| Error::Invalid("missing origin fetch URL".into()))?;
    let push = push
        .strip_suffix('\n')
        .ok_or_else(|| Error::Invalid("missing origin push URL".into()))?;
    let mut fetch_urls = fetch.split('\n');
    let mut push_urls = push.split('\n');
    let fetch_url = fetch_urls.next().unwrap_or("");
    let push_url = push_urls.next().unwrap_or("");
    let matches_repository =
        |url: &str| repository_identity(url).ok().as_ref() == Some(&binding.repository);
    if !matches_repository(fetch_url)
        || fetch_urls.any(|url| !matches_repository(url))
        || push_urls.next().is_some()
        || !matches_repository(push_url)
    {
        return Err(Error::Invalid(
            "origin fetch and sole push URL must match the configured forge repository".into(),
        ));
    }
    ensure_stable_transport_urls(cwd, &[fetch_url, push_url])?;
    Ok(RemoteUrls {
        fetch: fetch_url.to_owned(),
        push: push_url.to_owned(),
    })
}

fn transport_git(cwd: &Path, command: &[OsString]) -> Result<String> {
    let mut args = args(&["-c", "http.followRedirects=false"]);
    args.extend_from_slice(command);
    git(cwd, &args)
}

impl GitOpsRepo {
    /// Clone an actual configured repository. Destination must not already exist.
    pub fn clone_bare(remote: &OsStr, destination: &Path) -> Result<Self> {
        let destination = absolute(destination)?;
        run(
            OsStr::new("git"),
            &[
                "clone".into(),
                "--bare".into(),
                "--".into(),
                remote.into(),
                destination.as_os_str().into(),
            ],
            None,
            &[],
        )?;
        Self::open(&destination)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let bare = fs::canonicalize(path)?;
        if git(&bare, &args(&["rev-parse", "--is-bare-repository"]))?.trim() != "true" {
            return Err(Error::Invalid("repository must be bare".into()));
        }
        Ok(Self {
            bare,
            binding: None,
        })
    }

    /// Bind this handle to operator-owned forge configuration. Both effective
    /// origin fetch URLs and exactly one push URL must identify this repository.
    /// The returned handle and its worktrees recheck the binding before network
    /// operations; a handle opened or cloned without binding cannot publish.
    pub fn bind_origin(&self, repository_url: &str, target_branch: &str) -> Result<Self> {
        if target_branch.is_empty()
            || target_branch.starts_with('-')
            || target_branch.starts_with("refs/heads/")
        {
            return Err(Error::Invalid("invalid configured target branch".into()));
        }
        let target_ref = format!("refs/heads/{target_branch}");
        git(&self.bare, &args(&["check-ref-format", &target_ref]))?;
        let binding = OriginBinding {
            repository: repository_identity(repository_url)?,
            target_ref,
        };
        verified_origin(&self.bare, &binding)?;
        Ok(Self {
            bare: self.bare.clone(),
            binding: Some(binding),
        })
    }

    /// Refresh refs before choosing a base. Existing worktrees remain pinned.
    pub fn fetch(&self) -> Result<()> {
        let destination = match &self.binding {
            Some(binding) => verified_origin(&self.bare, binding)?.fetch,
            None => "origin".to_owned(),
        };
        transport_git(
            &self.bare,
            &args(&[
                "fetch",
                "--prune",
                &destination,
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ]),
        )?;
        Ok(())
    }

    /// Fetch a GitHub pull request's synthetic head from the bound origin.
    /// Compare the returned commit with fresh forge evidence before rendering.
    pub fn fetch_pull_head(&self, number: u64) -> Result<String> {
        if number == 0 {
            return Err(Error::Invalid("invalid pull request number".into()));
        }
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| Error::Invalid("repository is not bound to a forge".into()))?;
        let destination = verified_origin(&self.bare, binding)?.fetch;
        let reference = format!("refs/thalamus/pull/{number}");
        let refspec = format!("+refs/pull/{number}/head:{reference}");
        transport_git(
            &self.bare,
            &args(&["fetch", "--no-tags", &destination, &refspec]),
        )?;
        Ok(git(
            &self.bare,
            &args(&["rev-parse", "--verify", &format!("{reference}^{{commit}}")]),
        )?
        .trim()
        .to_owned())
    }

    /// Resolve a configured branch or full ref to its current commit after fetching.
    pub fn resolve_base(&self, reference: &str) -> Result<String> {
        let reference = if reference.starts_with("refs/heads/") {
            reference.to_owned()
        } else {
            format!("refs/heads/{reference}")
        };
        if self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.target_ref != reference)
        {
            return Err(Error::Invalid(
                "target branch differs from configured binding".into(),
            ));
        }
        git(&self.bare, &args(&["check-ref-format", &reference]))?;
        Ok(git(
            &self.bare,
            &args(&["rev-parse", "--verify", &format!("{reference}^{{commit}}")]),
        )?
        .trim()
        .to_owned())
    }

    /// Check a full commit against the bound target branch's locally fetched
    /// tip. Fetch immediately beforehand and compare the tip with forge evidence;
    /// this point-in-time check is not an atomic guarantee about later updates.
    pub fn is_ancestor(&self, commit: &str, target_branch: &str) -> Result<bool> {
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| Error::Invalid("repository is not bound to a forge".into()))?;
        let reference = if target_branch.starts_with("refs/heads/") {
            target_branch.to_owned()
        } else {
            format!("refs/heads/{target_branch}")
        };
        if reference != binding.target_ref {
            return Err(Error::Invalid(
                "target branch differs from configured binding".into(),
            ));
        }
        verified_origin(&self.bare, binding)?;
        verify_commit(&self.bare, commit)?;
        let target = run(
            OsStr::new("git"),
            &args(&["rev-parse", "--verify", &format!("{reference}^{{commit}}")]),
            Some(&self.bare),
            &[("GIT_NO_REPLACE_OBJECTS", "1")],
        )?;
        let target = String::from_utf8(target.stdout)?;
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", commit, target.trim()])
            .current_dir(&self.bare)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .output()?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Error::Process {
                program: "git".into(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
        }
    }

    /// Locate a uniquely named service in an immutable commit's catalog.
    /// Only regular YAML blobs directly under catalog/services are considered;
    /// malformed entries and duplicate matches for the requested name fail closed.
    pub fn service_file(&self, base: &str, service: &str) -> Result<PathBuf> {
        if service.is_empty()
            || service.len() > 253
            || service.starts_with('.')
            || service.ends_with('.')
            || !service
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        {
            return Err(Error::Invalid("invalid service or base commit".into()));
        }
        verify_commit(&self.bare, base)?;
        let env = [("GIT_NO_REPLACE_OBJECTS", "1")];
        let tree = format!("{base}:catalog/services");
        let listing = run(
            OsStr::new("git"),
            &args(&["ls-tree", "-z", &tree]),
            Some(&self.bare),
            &env,
        )?;
        let mut selected = None;
        for entry in listing
            .stdout
            .split(|b| *b == 0)
            .filter(|entry| !entry.is_empty())
        {
            let tab = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| Error::Invalid("invalid catalog tree entry".into()))?;
            let metadata = std::str::from_utf8(&entry[..tab])
                .map_err(|_| Error::Invalid("invalid catalog tree entry".into()))?;
            let filename = std::str::from_utf8(&entry[tab + 1..])
                .map_err(|_| Error::Invalid("invalid catalog filename".into()))?;
            let file = Path::new("catalog/services").join(filename);
            if !matches!(
                file.extension().and_then(|s| s.to_str()),
                Some("yaml" | "yml")
            ) {
                continue;
            }
            let mut parts = metadata.split(' ');
            let mode = parts.next();
            let kind = parts.next();
            let object = parts.next().unwrap_or("");
            let mut components = file.components();
            if parts.next().is_some()
                || !matches!(mode, Some("100644" | "100755"))
                || kind != Some("blob")
                || !matches!(object.len(), 40 | 64)
                || !object.bytes().all(|b| b.is_ascii_hexdigit())
                || !matches!(components.next(), Some(Component::Normal(s)) if s == "catalog")
                || !matches!(components.next(), Some(Component::Normal(s)) if s == "services")
                || !matches!(components.next(), Some(Component::Normal(_)))
                || components.next().is_some()
            {
                return Err(Error::Invalid("invalid catalog service blob".into()));
            }
            let blob = run(
                OsStr::new("git"),
                &args(&["cat-file", "blob", object]),
                Some(&self.bare),
                &env,
            )?;
            let value: Value = serde_yaml::from_slice(&blob.stdout)?;
            if value.get("name").and_then(Value::as_str) == Some(service)
                && selected.replace(file).is_some()
            {
                return Err(Error::Invalid("duplicate service name in catalog".into()));
            }
        }
        selected.ok_or(Error::ServiceNotFound)
    }

    /// Discard only a worktree created by this repository handle.
    pub fn discard_worktree(&self, worktree: ChangeWorktree) -> Result<()> {
        if !worktree.owned || worktree.repository != self.bare {
            return Err(Error::Invalid(
                "worktree is not owned by this repository".into(),
            ));
        }
        if fs::canonicalize(&worktree.path)? != worktree.path {
            return Err(Error::Invalid("worktree path changed".into()));
        }
        let common = git(&worktree.path, &args(&["rev-parse", "--git-common-dir"]))?;
        if fs::canonicalize(worktree.path.join(common.trim()))? != self.bare {
            return Err(Error::Invalid("worktree repository changed".into()));
        }
        let mut command = args(&["worktree", "remove", "--force", "--"]);
        command.push(worktree.path.into_os_string());
        git(&self.bare, &command)?;
        Ok(())
    }

    /// Render an explicit commit in an isolated, temporary worktree. The path
    /// must not exist, and is removed on either successful or failed rendering.
    pub fn render_ref(&self, commit: &str, path: &Path, render: &RenderCommand) -> Result<String> {
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| Error::Invalid("repository is not bound to a forge".into()))?;
        verified_origin(&self.bare, binding)?;
        let worktree = self.create_worktree(commit, path)?;
        let result = worktree.render(render);
        let cleanup = self.discard_worktree(worktree);
        match (result, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    /// Render two approved commit IDs in isolated worktrees. Paths must not exist.
    pub fn preview_refs(
        &self,
        base: &str,
        head: &str,
        base_path: &Path,
        head_path: &Path,
        render: &RenderCommand,
    ) -> Result<Preview> {
        let base_tree = self.create_worktree(base, base_path)?;
        let result = (|| {
            let head_tree = self.create_worktree(head, head_path)?;
            let result = (|| {
                let base_render = base_tree.render(render)?;
                let proposed_render = head_tree.render(render)?;
                let data_diff = git(
                    &self.bare,
                    &args(&["diff", "--no-ext-diff", "--no-textconv", base, head, "--"]),
                )?;
                let rendered_diff = TextDiff::from_lines(&base_render, &proposed_render)
                    .unified_diff()
                    .header("base-render", "proposed-render")
                    .to_string();
                Ok(Preview {
                    base_render,
                    proposed_render,
                    data_diff,
                    rendered_diff,
                })
            })();
            let cleanup = self.discard_worktree(head_tree);
            match (result, cleanup) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            }
        })();
        let cleanup = self.discard_worktree(base_tree);
        match (result, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    /// Create a detached, independent worktree at an explicit full commit ID.
    pub fn create_worktree(&self, base: &str, path: &Path) -> Result<ChangeWorktree> {
        if !matches!(base.len(), 40 | 64) || !base.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid(
                "base must be a full hexadecimal commit ID".into(),
            ));
        }
        let resolved = git(
            &self.bare,
            &args(&["rev-parse", "--verify", &format!("{base}^{{commit}}")]),
        )?
        .trim()
        .to_owned();
        if !resolved.eq_ignore_ascii_case(base) {
            return Err(Error::Invalid(
                "base must identify a commit, not a tag".into(),
            ));
        }
        let path = absolute(path)?;
        let mut command = args(&["worktree", "add", "--detach", "--"]);
        command.push(path.as_os_str().into());
        command.push(resolved.clone().into());
        git(&self.bare, &command)?;
        Ok(ChangeWorktree {
            path: fs::canonicalize(path)?,
            base: resolved,
            changed_files: Vec::new(),
            preview_hashes: BTreeMap::new(),
            generated_hashes: BTreeMap::new(),
            repository: self.bare.clone(),
            binding: self.binding.clone(),
            owned: true,
        })
    }

    /// Recover a committed worktree after a crash or failed push.
    pub fn resume_committed_worktree(&self, base: &str, path: &Path) -> Result<ChangeWorktree> {
        let path = fs::canonicalize(path)?;
        let common = git(&path, &args(&["rev-parse", "--git-common-dir"]))?;
        let common = fs::canonicalize(path.join(common.trim()))?;
        if common != self.bare
            || !matches!(base.len(), 40 | 64)
            || !base.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::Invalid(
                "worktree repository or base is invalid".into(),
            ));
        }
        let parents = git(&path, &args(&["rev-list", "--parents", "-n", "1", "HEAD"]))?;
        let fields: Vec<_> = parents.split_whitespace().collect();
        if fields.len() != 2
            || fields[1] != base
            || !git(
                &path,
                &args(&["status", "--porcelain", "--untracked-files=all"]),
            )?
            .is_empty()
        {
            return Err(Error::Invalid(
                "worktree must contain one clean commit on the requested base".into(),
            ));
        }
        let names = git(
            &path,
            &args(&[
                "diff",
                "--no-renames",
                "--name-only",
                "-z",
                base,
                "HEAD",
                "--",
            ]),
        )?;
        let mut changed_files = Vec::new();
        for name in names.split('\0').filter(|s| !s.is_empty()) {
            let file = PathBuf::from(name);
            if !is_generated_path(&file) {
                validate_catalog_path(&path, &file)?;
                changed_files.push(file);
            }
        }
        let preview_hashes = file_hashes(&path, &changed_files)?;
        let generated_hashes = generated_tree_hashes(&path)?;
        Ok(ChangeWorktree {
            path,
            base: base.to_owned(),
            changed_files,
            preview_hashes,
            generated_hashes,
            repository: self.bare.clone(),
            binding: self.binding.clone(),
            owned: false,
        })
    }

    /// Remove a completed worktree; fails rather than discarding uncommitted data.
    pub fn remove_worktree(&self, path: &Path) -> Result<()> {
        let mut command = args(&["worktree", "remove", "--"]);
        command.push(absolute(path)?.into_os_string());
        git(&self.bare, &command)?;
        Ok(())
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn file_hashes(root: &Path, paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, Option<[u8; 32]>>> {
    paths
        .iter()
        .map(|path| {
            let hash = match fs::read(root.join(path)) {
                Ok(bytes) => Some(Sha256::digest(bytes).into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            Ok((path.clone(), hash))
        })
        .collect()
}

const GENERATED_ROOTS: [&str; 2] = ["rendered/inference", "rendered/pools"];

fn is_generated_path(path: &Path) -> bool {
    GENERATED_ROOTS.iter().any(|root| path.starts_with(root))
}

/// Capture the entire renderer-owned tree so files added or changed after the
/// preview cannot be included in a publication. Never follow output symlinks.
fn generated_tree_hashes(root: &Path) -> Result<BTreeMap<PathBuf, [u8; 32]>> {
    fn visit(root: &Path, path: &Path, hashes: &mut BTreeMap<PathBuf, [u8; 32]>) -> Result<()> {
        let absolute = root.join(path);
        let metadata = fs::symlink_metadata(&absolute)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(absolute)? {
                visit(root, &path.join(entry?.file_name()), hashes)?;
            }
        } else if metadata.is_file() {
            let mut digest = Sha256::new();
            digest.update(fs::read(absolute)?);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                digest.update([(metadata.permissions().mode() & 0o111 != 0) as u8]);
            }
            hashes.insert(path.to_path_buf(), digest.finalize().into());
        } else {
            return Err(Error::Invalid(
                "generated paths must be regular files or directories, not symlinks".into(),
            ));
        }
        Ok(())
    }

    let mut hashes = BTreeMap::new();
    match fs::symlink_metadata(root.join("rendered")) {
        Ok(metadata) if metadata.is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(hashes),
        Ok(_) => return Err(Error::Invalid("rendered must be a real directory".into())),
        Err(error) => return Err(error.into()),
    }
    for name in GENERATED_ROOTS {
        let path = Path::new(name);
        match fs::symlink_metadata(root.join(path)) {
            Ok(metadata) if metadata.is_dir() => visit(root, path, &mut hashes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(Error::Invalid(
                    "generated root must be a real directory".into(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(hashes)
}

impl ChangeWorktree {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn base(&self) -> &str {
        &self.base
    }

    fn check_base(&self) -> Result<()> {
        if git(&self.path, &args(&["rev-parse", "HEAD"]))?.trim() != self.base {
            return Err(Error::Invalid(
                "worktree HEAD no longer matches base".into(),
            ));
        }
        Ok(())
    }

    /// Render in an isolated worktree without inheriting Git or service secrets.
    /// Python renderers must be trusted, absolute files outside the checkout.
    pub fn render(&self, command: &RenderCommand) -> Result<String> {
        if command.executable.as_os_str() == OsStr::new("python3") {
            let script = match command.args.as_slice() {
                [isolation, script, ..] if isolation == OsStr::new("-I") => Path::new(script),
                _ => {
                    return Err(Error::Invalid(
                        "untrusted renderer: Python requires an isolated absolute script".into(),
                    ));
                }
            };
            if !script.is_absolute() {
                return Err(Error::Invalid(
                    "untrusted renderer: script path must be absolute".into(),
                ));
            }
            let metadata = fs::symlink_metadata(script)?;
            let resolved = fs::canonicalize(script)?;
            if !metadata.is_file()
                || resolved != script
                || resolved.starts_with(&self.path)
                || resolved.starts_with(&self.repository)
            {
                return Err(Error::Invalid(
                    "untrusted renderer: script must be a canonical regular file outside the checkout".into(),
                ));
            }
        }

        let configured_path = std::env::var_os("PATH")
            .ok_or_else(|| Error::Invalid("renderer PATH is not configured".into()))?;
        let mut trusted_paths = Vec::new();
        for entry in std::env::split_paths(&configured_path) {
            if !entry.is_absolute() {
                continue;
            }
            if let Ok(resolved) = fs::canonicalize(entry)
                && !resolved.starts_with(&self.path)
                && !resolved.starts_with(&self.repository)
            {
                trusted_paths.push(resolved);
            }
        }
        if trusted_paths.is_empty() {
            return Err(Error::Invalid(
                "renderer PATH contains no trusted directories".into(),
            ));
        }
        let path = std::env::join_paths(trusted_paths)
            .map_err(|_| Error::Invalid("renderer PATH is invalid".into()))?;
        let temporary = tempfile::Builder::new()
            .prefix("thalamus-render-")
            .tempdir()?;
        let temporary_path = fs::canonicalize(temporary.path())?;
        if temporary_path.starts_with(&self.path) || temporary_path.starts_with(&self.repository) {
            return Err(Error::Invalid(
                "renderer temporary directory must be outside the checkout".into(),
            ));
        }
        let output = Command::new(&command.executable)
            .args(&command.args)
            .current_dir(&self.path)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &temporary_path)
            .env("TMPDIR", &temporary_path)
            .env("KCL_CACHE_PATH", &temporary_path)
            .env("KCL_PKG_PATH", &temporary_path)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .output()?;
        if !output.status.success() {
            return Err(Error::Process {
                program: command.executable.to_string_lossy().into_owned(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    /// Preview the inverse of a single-parent commit against this worktree's base.
    /// Conflicts fail rather than silently changing the requested inverse.
    pub fn preview_revert(&mut self, commit: &str, render: &RenderCommand) -> Result<Preview> {
        self.check_base()?;
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid("revert requires a full commit ID".into()));
        }
        if !git(
            &self.path,
            &args(&["status", "--porcelain", "--untracked-files=all"]),
        )?
        .is_empty()
        {
            return Err(Error::Invalid(
                "worktree must be clean before preview".into(),
            ));
        }
        let parents = git(
            &self.path,
            &args(&["rev-list", "--parents", "-n", "1", commit]),
        )?;
        let fields: Vec<_> = parents.split_whitespace().collect();
        if fields.len() != 2 || !fields[0].eq_ignore_ascii_case(commit) {
            return Err(Error::Invalid(
                "revert requires a single-parent commit".into(),
            ));
        }
        let names = git(
            &self.path,
            &args(&[
                "diff",
                "--no-renames",
                "--name-only",
                "-z",
                fields[1],
                commit,
                "--",
            ]),
        )?;
        let changed_files: Vec<PathBuf> = names
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|path| !is_generated_path(path))
            .collect();
        if changed_files.is_empty() {
            return Err(Error::Invalid(
                "revert commit has no catalog changes".into(),
            ));
        }
        let mut created_files = Vec::new();
        for path in &changed_files {
            validate_catalog_path(&self.path, path)?;
            if !self.path.join(path).exists() {
                validate_new_service(&self.path, path)?;
                created_files.push(path.clone());
            }
            for revision in [fields[1], commit] {
                let mut command = args(&["ls-tree", "-z", revision, "--"]);
                command.push(path.as_os_str().to_owned());
                let entry = git(&self.path, &command)?;
                if !entry.is_empty()
                    && !entry.starts_with("100644 blob ")
                    && !entry.starts_with("100755 blob ")
                {
                    return Err(Error::Invalid(
                        "revert only permits regular catalog YAML files".into(),
                    ));
                }
            }
        }
        let base_render = self.render(render)?;
        let mut inverse = args(&[
            "diff",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            commit,
            fields[1],
            "--",
        ]);
        inverse.extend(changed_files.iter().map(|path| path.as_os_str().to_owned()));
        let patch = git(&self.path, &inverse)?;
        git_apply_inverse(&self.path, patch.as_bytes())?;
        git(&self.path, &args(&["reset", "--quiet", "HEAD", "--"]))?;
        for path in &changed_files {
            validate_catalog_path(&self.path, path)?;
        }
        let proposed_render = self.render(render)?;
        if !created_files.is_empty() {
            let mut add = args(&["add", "--intent-to-add", "--"]);
            add.extend(created_files.iter().map(|p| p.as_os_str().to_owned()));
            git(&self.path, &add)?;
        }
        let mut command = args(&["diff", "--no-ext-diff", "--no-textconv", "--"]);
        command.extend(changed_files.iter().map(|p| p.as_os_str().to_owned()));
        let data_diff = git(&self.path, &command)?;
        self.preview_hashes = file_hashes(&self.path, &changed_files)?;
        self.generated_hashes = generated_tree_hashes(&self.path)?;
        self.changed_files = changed_files;
        let rendered_diff = TextDiff::from_lines(&base_render, &proposed_render)
            .unified_diff()
            .header("base-render", "proposed-render")
            .to_string();
        Ok(Preview {
            base_render,
            proposed_render,
            data_diff,
            rendered_diff,
        })
    }

    /// Render the unmodified base, apply structured patches, then render and diff.
    /// Existing files must be tracked catalog YAML. A new direct
    /// catalog/services/*.yaml file requires a structured root Set operation.
    pub fn preview(&mut self, patches: &[Patch], render: &RenderCommand) -> Result<Preview> {
        self.check_base()?;
        if patches.is_empty() {
            return Err(Error::Invalid("at least one patch is required".into()));
        }
        if !git(
            &self.path,
            &args(&["status", "--porcelain", "--untracked-files=all"]),
        )?
        .is_empty()
        {
            return Err(Error::Invalid(
                "worktree must be clean before preview".into(),
            ));
        }
        let base_render = self.render(render)?;
        let mut documents = BTreeMap::<PathBuf, Value>::new();
        let mut new_files = Vec::new();
        for patch in patches {
            if !documents.contains_key(&patch.file) {
                match fs::symlink_metadata(self.path.join(&patch.file)) {
                    Ok(_) => {
                        validate_file(&self.path, &patch.file)?;
                        let mut command = args(&["ls-files", "--error-unmatch", "--"]);
                        command.push(patch.file.as_os_str().into());
                        git(&self.path, &command)?;
                        documents.insert(
                            patch.file.clone(),
                            serde_yaml::from_slice(&fs::read(self.path.join(&patch.file))?)?,
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        validate_new_service(&self.path, &patch.file)?;
                        if !patch.path.is_empty()
                            || !matches!(&patch.operation, Operation::Set(Value::Mapping(_)))
                        {
                            return Err(Error::Invalid(
                                "new service requires a root mapping Set".into(),
                            ));
                        }
                        new_files.push(patch.file.clone());
                        documents.insert(patch.file.clone(), Value::Null);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            apply(
                documents.get_mut(&patch.file).expect("inserted above"),
                &patch.path,
                &patch.operation,
            )?;
        }
        if !new_files.is_empty() {
            fs::create_dir_all(self.path.join("catalog/services"))?;
        }
        for (path, value) in &documents {
            let contents = serde_yaml::to_string(value)?;
            if new_files.contains(path) {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(self.path.join(path))?;
                file.write_all(contents.as_bytes())?;
            } else {
                fs::write(self.path.join(path), contents)?;
            }
        }
        let proposed_render = self.render(render)?;
        if !new_files.is_empty() {
            let mut add = args(&["add", "--intent-to-add", "--"]);
            add.extend(new_files.iter().map(|p| p.as_os_str().to_owned()));
            git(&self.path, &add)?;
        }
        let changed_files: Vec<PathBuf> = documents.into_keys().collect();
        let mut command = args(&["diff", "--no-ext-diff", "--no-textconv", "--"]);
        command.extend(changed_files.iter().map(|p| p.as_os_str().to_owned()));
        let data_diff = git(&self.path, &command)?;
        self.preview_hashes = file_hashes(&self.path, &changed_files)?;
        self.generated_hashes = generated_tree_hashes(&self.path)?;
        self.changed_files = changed_files;
        let rendered_diff = TextDiff::from_lines(&base_render, &proposed_render)
            .unified_diff()
            .header("base-render", "proposed-render")
            .to_string();
        Ok(Preview {
            base_render,
            proposed_render,
            data_diff,
            rendered_diff,
        })
    }

    /// Create this change's deterministic branch only if it is absent.
    /// Retrying an already-published identical commit is a read-only success.
    pub fn commit_and_push(
        &self,
        change_id: &str,
        message: &str,
        author: &CommitAuthor,
    ) -> Result<PublishedCommit> {
        self.commit_and_push_inner(change_id, message, author, None)
    }

    /// Replace this change's existing Thalamus branch using an exact-head
    /// lease against the single configured push destination. If it already
    /// points at the generated commit, return it without pushing so callers
    /// can retry after a successful push.
    pub fn commit_and_push_update(
        &self,
        change_id: &str,
        message: &str,
        author: &CommitAuthor,
        expected_old_head: &str,
    ) -> Result<PublishedCommit> {
        self.commit_and_push_inner(change_id, message, author, Some(expected_old_head))
    }

    fn commit_and_push_inner(
        &self,
        change_id: &str,
        message: &str,
        author: &CommitAuthor,
        expected_old_head: Option<&str>,
    ) -> Result<PublishedCommit> {
        if change_id.is_empty()
            || message.is_empty()
            || author.name.is_empty()
            || author.email.is_empty()
            || self.changed_files.is_empty()
        {
            return Err(Error::Invalid(
                "change ID, message, author and previewed files are required".into(),
            ));
        }
        for path in &self.changed_files {
            validate_catalog_path(&self.path, path)?;
        }
        if file_hashes(&self.path, &self.changed_files)? != self.preview_hashes {
            return Err(Error::Invalid(
                "previewed files changed after rendering".into(),
            ));
        }
        if generated_tree_hashes(&self.path)? != self.generated_hashes {
            return Err(Error::Invalid(
                "generated files changed after rendering".into(),
            ));
        }
        let branch = branch_for_change(change_id)?;
        let reference = format!("refs/heads/{branch}");
        if let Some(expected) = expected_old_head
            && (expected.len() != self.base.len()
                || !expected.bytes().all(|byte| byte.is_ascii_hexdigit())
                || expected.bytes().all(|byte| byte == b'0'))
        {
            return Err(Error::Invalid(
                "expected old head must be a nonzero full hexadecimal commit ID".into(),
            ));
        }
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| Error::Invalid("repository is not bound to a forge".into()))?;
        verified_origin(&self.path, binding)?;
        let env = [
            ("GIT_AUTHOR_NAME", author.name.as_str()),
            ("GIT_AUTHOR_EMAIL", author.email.as_str()),
            ("GIT_COMMITTER_NAME", author.name.as_str()),
            ("GIT_COMMITTER_EMAIL", author.email.as_str()),
            ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
        ];
        let mut commit = git(&self.path, &args(&["rev-parse", "HEAD"]))?
            .trim()
            .to_owned();
        if commit == self.base {
            if !git(&self.path, &args(&["diff", "--cached", "--name-only"]))?.is_empty() {
                return Err(Error::Invalid("worktree has staged changes".into()));
            }
            let mut add = args(&["add", "--"]);
            add.extend(self.changed_files.iter().map(|p| p.as_os_str().to_owned()));
            git(&self.path, &add)?;
            // Stage only the renderer's captured files and tracked removals.
            // In particular, do not sweep unrelated worktree files into a PR.
            let tracked = git(
                &self.path,
                &args(&[
                    "ls-files",
                    "-z",
                    "--",
                    GENERATED_ROOTS[0],
                    GENERATED_ROOTS[1],
                ]),
            )?;
            let mut generated: BTreeSet<&Path> =
                self.generated_hashes.keys().map(PathBuf::as_path).collect();
            generated.extend(
                tracked
                    .split('\0')
                    .filter(|name| !name.is_empty())
                    .map(Path::new),
            );
            if !generated.is_empty() {
                let mut add = args(&["add", "--all", "--force", "--"]);
                add.extend(generated.iter().map(|path| path.as_os_str().to_owned()));
                git(&self.path, &add)?;
            }
            run(
                OsStr::new("git"),
                &args(&[
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "commit",
                    "--cleanup=verbatim",
                    "-m",
                    message,
                ]),
                Some(&self.path),
                &env,
            )?;
            commit = git(&self.path, &args(&["rev-parse", "HEAD"]))?
                .trim()
                .to_owned();
        } else {
            let parents = git(
                &self.path,
                &args(&["rev-list", "--parents", "-n", "1", "HEAD"]),
            )?;
            let fields: Vec<_> = parents.split_whitespace().collect();
            if fields.len() != 2
                || fields[1] != self.base
                || !git(
                    &self.path,
                    &args(&["status", "--porcelain", "--untracked-files=all"]),
                )?
                .is_empty()
            {
                return Err(Error::Invalid(
                    "committed worktree is not clean or based on the requested commit".into(),
                ));
            }
            let tree = git(&self.path, &args(&["rev-parse", "HEAD^{tree}"]))?;
            let expected = run(
                OsStr::new("git"),
                &args(&[
                    "-c",
                    "commit.gpgsign=false",
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    &self.base,
                    "-m",
                    message,
                ]),
                Some(&self.path),
                &env,
            )?;
            if String::from_utf8(expected.stdout)?.trim() != commit {
                return Err(Error::Invalid(
                    "committed change does not match author and message".into(),
                ));
            }
        }
        let destination = verified_origin(&self.path, binding)?.push;
        let remote = transport_git(
            &self.path,
            &args(&["ls-remote", "--refs", "--", &destination, &reference]),
        )?;
        let mut lines = remote.lines();
        let remote_head = match lines.next() {
            Some(line) => {
                let (head, name) = line
                    .split_once('\t')
                    .ok_or_else(|| Error::Invalid("invalid remote branch evidence".into()))?;
                if name != reference
                    || head.len() != self.base.len()
                    || !head.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || lines.next().is_some()
                {
                    return Err(Error::Invalid("invalid remote branch evidence".into()));
                }
                Some(head)
            }
            None => None,
        };
        // A previous attempt may have succeeded before its result was saved.
        // No push occurs when the remote already has the exact same commit.
        if remote_head.is_some_and(|head| head.eq_ignore_ascii_case(&commit)) {
            return Ok(PublishedCommit { branch, commit });
        }
        match (expected_old_head, remote_head) {
            (None, None) => {}
            (Some(expected), Some(head)) if head.eq_ignore_ascii_case(expected) => {}
            (None, Some(_)) => {
                return Err(Error::Invalid("remote branch already exists".into()));
            }
            _ => {
                return Err(Error::Invalid(
                    "remote branch does not match expected old head".into(),
                ));
            }
        }
        // Preflight is not the lease: the server must still compare the ref
        // with the exact old head (or absence) during the push itself.
        let lease = format!(
            "--force-with-lease={reference}:{}",
            expected_old_head.unwrap_or("").to_ascii_lowercase()
        );
        let refspec = format!("HEAD:{reference}");
        transport_git(
            &self.path,
            &args(&[
                "push",
                "--no-follow-tags",
                "--recurse-submodules=no",
                &lease,
                &destination,
                &refspec,
            ]),
        )?;
        Ok(PublishedCommit { branch, commit })
    }
}

/// A stable, Git-safe identity that does not expose the change ID in a ref.
pub fn branch_for_change(change_id: &str) -> Result<String> {
    if change_id.is_empty() {
        return Err(Error::Invalid("change ID is empty".into()));
    }
    Ok(format!(
        "thalamus/{:x}",
        Sha256::digest(change_id.as_bytes())
    ))
}

fn validate_new_service(root: &Path, path: &Path) -> Result<()> {
    let parts: Vec<_> = path.components().collect();
    if parts.len() != 3
        || parts[0] != Component::Normal(OsStr::new("catalog"))
        || parts[1] != Component::Normal(OsStr::new("services"))
        || !matches!(parts[2], Component::Normal(_))
        || !matches!(
            path.extension().and_then(OsStr::to_str),
            Some("yaml" | "yml")
        )
    {
        return Err(Error::Invalid(
            "new files must be YAML directly under catalog/services".into(),
        ));
    }
    for directory in [root.join("catalog"), root.join("catalog/services")] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(Error::Invalid(
                    "catalog directories must not be symlinks".into(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

// Validate existing components without following links, even for a deleted leaf.
fn validate_catalog_path(root: &Path, path: &Path) -> Result<()> {
    let mut components = path.components();
    if components.next() != Some(Component::Normal(OsStr::new("catalog")))
        || !matches!(
            path.extension().and_then(OsStr::to_str),
            Some("yaml" | "yml")
        )
    {
        return Err(Error::Invalid("change must target catalog YAML".into()));
    }
    let mut current = root.to_path_buf();
    let mut missing = false;
    for component in std::iter::once(Component::Normal(OsStr::new("catalog"))).chain(components) {
        let Component::Normal(part) = component else {
            return Err(Error::Invalid("invalid catalog path".into()));
        };
        current.push(part);
        if !missing {
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(Error::Invalid(
                        "symlinks are not allowed in catalog paths".into(),
                    ));
                }
                Ok(metadata) if current == root.join(path) && !metadata.is_file() => {
                    return Err(Error::Invalid(
                        "catalog target must be a regular file".into(),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn validate_file(root: &Path, path: &Path) -> Result<()> {
    let mut components = path.components();
    if components.next() != Some(Component::Normal(OsStr::new("catalog"))) {
        return Err(Error::Invalid("patch must be under catalog/".into()));
    }
    let mut current = root.join("catalog");
    if fs::symlink_metadata(&current)?.file_type().is_symlink() {
        return Err(Error::Invalid(
            "symlinks are not allowed in catalog paths".into(),
        ));
    }
    let mut count = 0;
    for component in components {
        let Component::Normal(part) = component else {
            return Err(Error::Invalid("invalid catalog path".into()));
        };
        current.push(part);
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Err(Error::Invalid(
                "symlinks are not allowed in catalog paths".into(),
            ));
        }
        count += 1;
    }
    if count == 0
        || !matches!(
            path.extension().and_then(OsStr::to_str),
            Some("yaml" | "yml")
        )
        || !fs::metadata(current)?.is_file()
    {
        return Err(Error::Invalid(
            "patch target must be a regular YAML file".into(),
        ));
    }
    Ok(())
}

fn apply(value: &mut Value, path: &[String], operation: &Operation) -> Result<()> {
    if path.is_empty() {
        return match operation {
            Operation::Set(new) => {
                *value = new.clone();
                Ok(())
            }
            Operation::Remove => Err(Error::Invalid("cannot remove a document root".into())),
        };
    }
    let key = &path[0];
    let last = path.len() == 1;
    match value {
        Value::Mapping(map) => {
            let key = Value::String(key.clone());
            if last {
                match operation {
                    Operation::Set(new) => {
                        map.insert(key, new.clone());
                    }
                    Operation::Remove => {
                        if map.remove(&key).is_none() {
                            return Err(Error::Invalid("mapping key does not exist".into()));
                        }
                    }
                }
                Ok(())
            } else {
                apply(
                    map.get_mut(&key)
                        .ok_or_else(|| Error::Invalid("mapping key does not exist".into()))?,
                    &path[1..],
                    operation,
                )
            }
        }
        Value::Sequence(seq) => {
            let index = key
                .parse::<usize>()
                .map_err(|_| Error::Invalid("invalid sequence index".into()))?;
            if index >= seq.len() {
                return Err(Error::Invalid("sequence index out of bounds".into()));
            }
            if last {
                match operation {
                    Operation::Set(new) => seq[index] = new.clone(),
                    Operation::Remove => {
                        seq.remove(index);
                    }
                }
                Ok(())
            } else {
                apply(&mut seq[index], &path[1..], operation)
            }
        }
        _ => Err(Error::Invalid("patch traverses a scalar".into())),
    }
}
