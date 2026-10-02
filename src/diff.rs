//! The diff pane: `git diff` for one changed file through the user's git pager, opened by
//! clicking the file in the sidebar. The pane closes itself when the pager quits.

use crate::git::Change;
use crate::herdr;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Environment variable carrying the JSON [`DiffRequest`] into the pane.
pub const ENV: &str = "GIT_SIDEBAR_DIFF";

#[derive(Debug, Serialize, Deserialize)]
pub struct DiffRequest {
    pub root: PathBuf,
    pub path: String,
    pub orig: Option<String>,
    pub untracked: bool,
}

impl DiffRequest {
    pub fn new(root: &Path, change: &Change) -> Self {
        Self { root: root.to_path_buf(), path: change.path.clone(), orig: change.orig.clone(), untracked: change.untracked() }
    }
}

pub fn run() -> Result<()> {
    let request: DiffRequest = serde_json::from_str(&std::env::var(ENV).with_context(|| format!("{ENV} not set"))?)?;
    let has_head = Command::new("git")
        .arg("-C")
        .arg(&request.root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    // Agent shells often export PAGER/GIT_PAGER=cat, and the usual LESS=FRX quits at once on
    // a diff shorter than the pane; page with `less -R` so the pane stays until `q`.
    Command::new("git")
        .arg("-C")
        .arg(&request.root)
        .arg("--paginate")
        .args(diff_args(&request, has_head))
        .env_remove("GIT_PAGER")
        .env_remove("PAGER")
        .env("LESS", "R")
        .env("DELTA_PAGER", "less -R")
        .status()
        .context("running git diff")?;
    if let Ok(me) = std::env::var("HERDR_PANE_ID") {
        herdr::pane_close(&me)?;
    }
    Ok(())
}

/// Staged and unstaged changes together (`git diff HEAD`); untracked files as all-new.
fn diff_args(request: &DiffRequest, has_head: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if request.untracked {
        args.extend(["diff", "--no-index", "--", "/dev/null"].map(String::from));
    } else {
        // Before the first commit there is no HEAD: show what is staged.
        args.extend(["diff", if has_head { "HEAD" } else { "--cached" }, "-M", "--"].map(String::from));
        args.extend(request.orig.clone());
    }
    args.push(request.path.clone());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(path: &str, orig: Option<&str>, untracked: bool) -> DiffRequest {
        DiffRequest { root: "/r".into(), path: path.into(), orig: orig.map(String::from), untracked }
    }

    #[test]
    fn diff_args_cover_tracked_renamed_untracked_and_unborn() {
        assert_eq!(diff_args(&request("a.rs", None, false), true), ["diff", "HEAD", "-M", "--", "a.rs"]);
        assert_eq!(diff_args(&request("new.rs", Some("old.rs"), false), true), ["diff", "HEAD", "-M", "--", "old.rs", "new.rs"]);
        assert_eq!(diff_args(&request("n.md", None, true), true), ["diff", "--no-index", "--", "/dev/null", "n.md"]);
        assert_eq!(diff_args(&request("a.rs", None, false), false), ["diff", "--cached", "-M", "--", "a.rs"]);
    }
}
