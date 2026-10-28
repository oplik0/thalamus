//! Git-backed previews and changes to existing YAML catalog files.
//!
//! Git and KCL are invoked directly, without a shell. The caller supplies the
//! exact KCL arguments used by its CI and is responsible for credentials,
//! repository trust, worktree lifecycle, and serializing access to a worktree.
//!
//! To regenerate a preview after the base moves, fetch and resolve the base,
//! create a new worktree at that commit, and reapply the requested patches.
//! Existing worktrees remain pinned to their original bases.

use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use thiserror::Error;

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

/// KCL executable and arguments, run from the root of each worktree.
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
}

#[derive(Debug)]
pub struct ChangeWorktree {
    path: PathBuf,
    base: String,
    changed_files: Vec<PathBuf>,
    preview_hashes: BTreeMap<PathBuf, Option<[u8; 32]>>,
    repository: PathBuf,
    owned: bool,
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
        run(OsStr::new("git"), args, Some(cwd), &[])?.stdout,
    )?)
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
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
        Ok(Self { bare })
    }

    /// Refresh refs before choosing a base. Existing worktrees remain pinned.
    pub fn fetch(&self) -> Result<()> {
        git(
            &self.bare,
            &args(&[
                "fetch",
                "--prune",
                "origin",
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ]),
        )?;
        Ok(())
    }

    /// Resolve a configured branch or full ref to its current commit after fetching.
    pub fn resolve_base(&self, reference: &str) -> Result<String> {
        let reference = if reference.starts_with("refs/heads/") {
            reference.to_owned()
        } else {
            format!("refs/heads/{reference}")
        };
        git(&self.bare, &args(&["check-ref-format", &reference]))?;
        Ok(git(
            &self.bare,
            &args(&["rev-parse", "--verify", &format!("{reference}^{{commit}}")]),
        )?
        .trim()
        .to_owned())
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
            repository: self.bare.clone(),
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
            &args(&["diff", "--name-only", "-z", base, "HEAD", "--"]),
        )?;
        let changed_files: Vec<PathBuf> = names
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        for file in &changed_files {
            validate_catalog_path(&path, file)?;
        }
        let preview_hashes = file_hashes(&path, &changed_files)?;
        Ok(ChangeWorktree {
            path,
            base: base.to_owned(),
            changed_files,
            preview_hashes,
            repository: self.bare.clone(),
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

    fn render(&self, command: &RenderCommand) -> Result<String> {
        Ok(String::from_utf8(
            run(&command.executable, &command.args, Some(&self.path), &[])?.stdout,
        )?)
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
            .collect();
        if changed_files.is_empty() {
            return Err(Error::Invalid("revert commit has no changes".into()));
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
        git(
            &self.path,
            &args(&[
                "-c",
                "core.hooksPath=/dev/null",
                "revert",
                "--no-commit",
                commit,
            ]),
        )?;
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

    /// Commit the previewed files and push without force. Repeating the same
    /// change from the same base with the same contents, message and author
    /// creates the same commit and safely reuses its deterministic branch.
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
        let env = [
            ("GIT_AUTHOR_NAME", author.name.as_str()),
            ("GIT_AUTHOR_EMAIL", author.email.as_str()),
            ("GIT_COMMITTER_NAME", author.name.as_str()),
            ("GIT_COMMITTER_EMAIL", author.email.as_str()),
            ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
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
        let refspec = format!("HEAD:{reference}");
        if let Some(expected) = expected_old_head {
            let destinations = git(
                &self.path,
                &args(&["remote", "get-url", "--push", "--all", "origin"]),
            )?;
            let mut destinations = destinations.lines();
            let destination = destinations.next().filter(|url| !url.is_empty());
            if destination.is_none() || destinations.next().is_some() {
                return Err(Error::Invalid(
                    "updating a branch requires exactly one push destination".into(),
                ));
            }
            let remote = git(
                &self.path,
                &args(&[
                    "ls-remote",
                    "--refs",
                    "--",
                    destination.unwrap(),
                    &reference,
                ]),
            )?;
            let remote_head = remote.lines().find_map(|line| {
                line.split_once('\t')
                    .and_then(|(head, name)| (name == reference).then_some(head))
            });
            // A previous attempt may have pushed successfully before its
            // caller persisted the result. No update is needed in that case.
            if remote_head.is_some_and(|head| head.eq_ignore_ascii_case(&commit)) {
                return Ok(PublishedCommit { branch, commit });
            }
            if !remote_head.is_some_and(|head| head.eq_ignore_ascii_case(expected)) {
                return Err(Error::Invalid(
                    "remote branch does not match expected old head".into(),
                ));
            }
            // The preflight check is not the lease: Git checks this exact
            // expected remote head atomically when it performs the update.
            let lease = format!(
                "--force-with-lease={reference}:{}",
                expected.to_ascii_lowercase()
            );
            git(
                &self.path,
                &args(&[
                    "push",
                    "--no-follow-tags",
                    "--recurse-submodules=no",
                    &lease,
                    "origin",
                    &refspec,
                ]),
            )?;
        } else {
            git(&self.path, &args(&["push", "origin", &refspec]))?;
        }
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
