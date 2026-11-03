//! Where a diff comes from: a changed local file, every uncommitted change of a checkout, a
//! commit (or one file of it) from its checkout or GitHub, or a pull request's patch.

use crate::engine::git::{self, Change, FileChange};
use crate::engine::github;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a patch fetched from GitHub is reused before fetching it again.
const FETCHED_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffRequest {
    /// A changed file in a local checkout: staged and unstaged changes together.
    Local { root: PathBuf, path: String, orig: Option<String>, untracked: bool },
    /// Every uncommitted change of a local checkout, untracked files included.
    Worktree { root: PathBuf },
    /// A commit, its message and what it changes (a merge: against its first parent, i.e.
    /// what it brought in); or one file of it.
    Commit { commit: CommitAt, path: Option<String> },
    /// A pull request's whole patch.
    Pr { url: String },
}

impl DiffRequest {
    /// Whether what it shows can change: a commit cannot (amending one makes another).
    pub fn changes(&self) -> bool {
        !matches!(self, DiffRequest::Commit { .. })
    }

    pub fn local(root: &Path, change: &Change) -> Self {
        DiffRequest::Local {
            root: root.to_path_buf(),
            path: change.path.clone(),
            orig: change.orig.clone(),
            untracked: change.untracked(),
        }
    }
}

/// A commit and where to read it: the local checkout when it has the commit, else GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommitAt {
    pub hash: String,
    pub root: Option<PathBuf>,
    /// `owner/repo` on GitHub.
    pub repo: Option<String>,
}

impl CommitAt {
    /// The checkout to read it from, when it has the commit.
    fn local(&self) -> Option<&Path> {
        self.root.as_deref().filter(|root| git::commit_exists(root, &self.hash))
    }

    fn remote(&self) -> Result<&str, String> {
        self.repo.as_deref().ok_or_else(|| format!("{} is in no local checkout and no known GitHub repo", self.hash))
    }

    /// The files it changes, with line counts.
    pub fn files(&self) -> Result<Vec<FileChange>, String> {
        match self.local() {
            Some(root) => git::commit_files(root, &self.hash),
            None => github::commit_files(self.remote()?, &self.hash),
        }
    }

    /// Its page on GitHub.
    pub fn url(&self) -> Option<String> {
        Some(format!("https://github.com/{}/commit/{}", self.repo.as_deref()?, self.hash))
    }
}

/// Patches fetched from GitHub (pull requests, commits not in a checkout), by URL, with when
/// they were fetched.
pub type Patches = HashMap<String, (Instant, Result<String, String>)>;

/// The request's patch, and the checkout it comes from.
pub fn read_patch<'a>(request: &'a DiffRequest, patches: &mut Patches) -> (Result<String, String>, Option<&'a Path>) {
    match request {
        DiffRequest::Local { root, .. } => (local_patch(request, root), Some(root.as_path())),
        DiffRequest::Worktree { root } => (worktree_patch(root), Some(root.as_path())),
        DiffRequest::Commit { commit, path } => match commit.local() {
            Some(root) => (local_patch(request, root), Some(root)),
            None => (remote_commit_patch(patches, commit, path.as_deref()), None),
        },
        DiffRequest::Pr { url } => (fetched(patches, url, || github::pr_diff(url)), None),
    }
}

fn local_patch(request: &DiffRequest, root: &Path) -> Result<String, String> {
    let has_head = git::git_stdout(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_some();
    let out = git::git(root).args(diff_args(request, has_head)).output().map_err(|e| format!("git: {e}"))?;
    let untracked = matches!(request, DiffRequest::Local { untracked: true, .. });
    // `--no-index` exits 1 when the files differ; other failures leave stdout empty.
    if out.stdout.is_empty() && !out.status.success() && !untracked {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every uncommitted change of a checkout: tracked files against HEAD, then untracked files as
/// all-new.
fn worktree_patch(root: &Path) -> Result<String, String> {
    let mut patch = local_patch(&DiffRequest::Worktree { root: root.to_path_buf() }, root)?;
    let untracked = git::git_stdout(root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        let file = DiffRequest::Local { root: root.to_path_buf(), path: path.into(), orig: None, untracked: true };
        patch.push_str(&local_patch(&file, root)?);
    }
    Ok(patch)
}

/// A commit from GitHub, or one file of it.
fn remote_commit_patch(patches: &mut Patches, commit: &CommitAt, path: Option<&str>) -> Result<String, String> {
    let repo = commit.remote()?;
    let url = commit.url().unwrap_or_default();
    let patch = fetched(patches, &url, || github::commit_diff(repo, &commit.hash))?;
    match path {
        None => Ok(patch),
        Some(path) => github::patch_section(&patch, path).map(str::to_string).ok_or_else(|| format!("{path} is not in this commit")),
    }
}

/// What `fetch` returns, fetched once per [`FETCHED_TTL`] for `url`.
fn fetched(patches: &mut Patches, url: &str, fetch: impl FnOnce() -> Result<String, String>) -> Result<String, String> {
    let fresh = patches.get(url).is_some_and(|(at, _)| at.elapsed() < FETCHED_TTL);
    if !fresh {
        patches.insert(url.to_string(), (Instant::now(), fetch()));
    }
    patches[url].1.clone()
}

/// Local changes: staged and unstaged together (`git diff HEAD`), untracked files as all-new;
/// a commit: `git show`, of one file or all.
fn diff_args(request: &DiffRequest, has_head: bool) -> Vec<&str> {
    // Before the first commit there is no HEAD: show what is staged.
    let base = if has_head { "HEAD" } else { "--cached" };
    match request {
        DiffRequest::Local { path, untracked: true, .. } => vec!["diff", "--color=never", "--no-index", "--", "/dev/null", path.as_str()],
        DiffRequest::Local { path, orig, .. } => {
            let mut args = vec!["diff", "--color=never", base, "-M", "--"];
            args.extend(orig.as_deref());
            args.push(path);
            args
        }
        DiffRequest::Worktree { .. } => vec!["diff", "--color=never", base, "-M"],
        DiffRequest::Commit { commit, path } => {
            let mut args = vec!["show", "--color=never", "--format=fuller", "--diff-merges=first-parent", "-M", commit.hash.as_str()];
            args.extend(path.as_deref().into_iter().flat_map(|path| ["--", path]));
            args
        }
        DiffRequest::Pr { .. } => unreachable!("a pull request's patch comes from GitHub"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, orig: Option<&str>, untracked: bool) -> DiffRequest {
        DiffRequest::Local { root: "/r".into(), path: path.into(), orig: orig.map(String::from), untracked }
    }

    #[test]
    fn diff_args_cover_tracked_renamed_untracked_and_unborn() {
        let args = |r: &DiffRequest, head: bool| diff_args(r, head)[2..].iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(args(&local("a.rs", None, false), true), ["HEAD", "-M", "--", "a.rs"]);
        assert_eq!(args(&local("new.rs", Some("old.rs"), false), true), ["HEAD", "-M", "--", "old.rs", "new.rs"]);
        assert_eq!(args(&local("n.md", None, true), true), ["--no-index", "--", "/dev/null", "n.md"]);
        assert_eq!(args(&local("a.rs", None, false), false), ["--cached", "-M", "--", "a.rs"]);
        let commit = |path: Option<&str>| DiffRequest::Commit {
            commit: CommitAt { hash: "h1".into(), root: None, repo: None },
            path: path.map(String::from),
        };
        assert_eq!(diff_args(&commit(None), true)[0], "show");
        assert_eq!(args(&commit(Some("a.rs")), true)[3..], ["h1", "--", "a.rs"]);
    }
}
