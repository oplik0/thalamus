//! Git-backed previews and changes to existing YAML catalog files.
//!
//! Git and KCL are invoked directly, without a shell. The caller supplies the
//! exact KCL arguments used by its CI and is responsible for credentials,
//! repository trust, worktree lifecycle, and serializing access to a worktree.

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
    preview_hashes: BTreeMap<PathBuf, [u8; 32]>,
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
            validate_file(&path, file)?;
        }
        let preview_hashes = file_hashes(&path, &changed_files)?;
        Ok(ChangeWorktree {
            path,
            base: base.to_owned(),
            changed_files,
            preview_hashes,
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

fn file_hashes(root: &Path, paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, [u8; 32]>> {
    paths
        .iter()
        .map(|path| {
            let hash: [u8; 32] = Sha256::digest(fs::read(root.join(path))?).into();
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
            validate_file(&self.path, path)?;
        }
        if file_hashes(&self.path, &self.changed_files)? != self.preview_hashes {
            return Err(Error::Invalid(
                "previewed files changed after rendering".into(),
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
        let branch = branch_for_change(change_id)?;
        git(
            &self.path,
            &args(&["push", "origin", &format!("HEAD:refs/heads/{branch}")]),
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
