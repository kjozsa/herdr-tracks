//! GitHub pull requests: recognising PR URLs, matching them to local checkouts by remote,
//! and reading state + CI rollup through the `gh` CLI.

use crate::git::{FileChange, LineStat};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// `https://github.com/<owner>/<repo>/pull/<number>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u32,
}

impl PrRef {
    pub fn url(&self) -> String {
        format!("https://github.com/{}/{}/pull/{}", self.owner, self.repo, self.number)
    }

    /// `owner/repo`, lowercased for comparison with remote slugs.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo).to_lowercase()
    }

    /// The same pull request, whatever the letter case of owner and repo (GitHub ignores it).
    pub fn same(&self, other: &PrRef) -> bool {
        self.number == other.number && self.slug() == other.slug()
    }
}

/// Every PR URL in `text`. `…/pull/new/<branch>` (the hint `git push` prints) is not a PR.
pub fn pr_urls(text: &str) -> Vec<PrRef> {
    const PREFIX: &str = "https://github.com/";
    let mut found: Vec<PrRef> = Vec::new();
    for (at, _) in text.match_indices(PREFIX) {
        let rest = &text[at + PREFIX.len()..];
        let mut parts = rest.splitn(4, '/');
        let (Some(owner), Some(repo), Some("pull"), Some(tail)) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        let Ok(number) = digits.parse() else { continue };
        if !valid(owner) || !valid(repo) {
            continue;
        }
        let pr = PrRef { owner: owner.into(), repo: repo.into(), number };
        if !found.contains(&pr) {
            found.push(pr);
        }
    }
    found
}

/// `owner/repo` (lowercased) of every github.com remote of the checkout at `root`. The first
/// is the repo `gh` would default to: remotes named `upstream`, `github`, `origin`, then others.
pub fn remote_slugs(root: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--get-regexp", r"^remote\..*\.url$"])
        .output()
    else {
        return Vec::new();
    };
    let rank = |key: &str| {
        let name = key.strip_prefix("remote.").and_then(|k| k.strip_suffix(".url")).unwrap_or(key);
        ["upstream", "github", "origin"].iter().position(|n| *n == name).unwrap_or(3)
    };
    let mut remotes: Vec<(usize, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter_map(|(key, url)| Some((rank(key), remote_slug(url.trim())?)))
        .collect();
    remotes.sort_by_key(|(rank, _)| *rank);
    remotes.into_iter().map(|(_, slug)| slug).collect()
}

/// `git@github.com:o/r.git`, `https://github.com/o/r`, `ssh://git@github.com/o/r.git` → `o/r`.
fn remote_slug(url: &str) -> Option<String> {
    let after = &url[url.find("github.com")? + "github.com".len()..];
    let path = after.strip_prefix(':').or_else(|| after.strip_prefix('/'))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| format!("{owner}/{repo}").to_lowercase())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrState {
    /// Merged and closed PRs keep their last status and are not refreshed again.
    pub fn is_final(self) -> bool {
        matches!(self, PrState::Merged | PrState::Closed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Checks {
    Passing,
    Failing,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatus {
    pub state: PrState,
    /// `None` when the PR has no checks.
    pub checks: Option<Checks>,
}

/// What the sidebar fetches of a pull request.
#[derive(Debug, Clone)]
pub struct PrView {
    pub status: PrStatus,
    pub title: String,
    /// Its commits, oldest first as GitHub lists them.
    pub commits: Vec<PrCommit>,
    /// The merge commit once merged (with a squash merge, the only one on the base branch).
    pub merge: Option<PrCommit>,
    /// Last activity on GitHub (pushes, reviews, merging), unix seconds.
    pub updated: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrCommit {
    pub oid: String,
    /// First line of the message; GitHub does not give it for the merge commit.
    pub subject: Option<String>,
    /// Committed, unix seconds.
    pub time: Option<i64>,
}

impl PrView {
    /// Hashes of every commit it holds, the merge commit included.
    pub fn holds(&self, hash: &str) -> bool {
        self.commits.iter().chain(&self.merge).any(|c| c.oid == hash)
    }
}

pub fn pr_view(pr: &PrRef) -> Result<PrView, String> {
    let fields = "state,isDraft,statusCheckRollup,title,commits,mergeCommit,mergedAt,updatedAt";
    let json = gh(&["pr", "view", &pr.url(), "--json", fields])?;
    let json: Value = serde_json::from_str(&json).map_err(|e| format!("gh output: {e}"))?;
    Ok(parse_view(&json))
}

fn parse_view(json: &Value) -> PrView {
    let secs = |v: &Value| v.as_str().and_then(crate::transcript::unix_ms).map(|ms| ms / 1000);
    let commits = json["commits"].as_array().into_iter().flatten().filter_map(|c| {
        Some(PrCommit {
            oid: c["oid"].as_str()?.to_string(),
            subject: c["messageHeadline"].as_str().map(str::to_string),
            time: secs(&c["committedDate"]),
        })
    });
    let merge = json["mergeCommit"]["oid"].as_str().map(|oid| PrCommit { oid: oid.to_string(), subject: None, time: secs(&json["mergedAt"]) });
    PrView {
        status: parse_status(json),
        title: json["title"].as_str().unwrap_or_default().to_string(),
        commits: commits.collect(),
        merge,
        updated: secs(&json["updatedAt"]),
    }
}

fn parse_status(json: &Value) -> PrStatus {
    let state = match json["state"].as_str() {
        Some("MERGED") => PrState::Merged,
        Some("CLOSED") => PrState::Closed,
        _ if json["isDraft"].as_bool() == Some(true) => PrState::Draft,
        _ => PrState::Open,
    };
    PrStatus { state, checks: rollup(json["statusCheckRollup"].as_array().map(Vec::as_slice).unwrap_or_default()) }
}

/// Failing beats pending beats passing. Check runs report `status` + `conclusion`;
/// commit statuses report `state`.
fn rollup(checks: &[Value]) -> Option<Checks> {
    let mut result = None;
    for check in checks {
        let one = match (check["status"].as_str(), check["conclusion"].as_str(), check["state"].as_str()) {
            (_, Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"), _)
            | (_, _, Some("FAILURE" | "ERROR")) => Checks::Failing,
            (Some(status), _, _) if status != "COMPLETED" => Checks::Pending,
            (_, _, Some("PENDING" | "EXPECTED")) => Checks::Pending,
            _ => Checks::Passing,
        };
        result = Some(match (result, one) {
            (Some(Checks::Failing), _) | (_, Checks::Failing) => Checks::Failing,
            (Some(Checks::Pending), _) | (_, Checks::Pending) => Checks::Pending,
            _ => Checks::Passing,
        });
    }
    result
}

/// The files a commit of `slug` (`owner/repo`) changes, from GitHub.
pub fn commit_files(slug: &str, hash: &str) -> Result<Vec<FileChange>, String> {
    let json = gh(&["api", &format!("repos/{slug}/commits/{hash}")])?;
    let json: Value = serde_json::from_str(&json).map_err(|e| format!("gh output: {e}"))?;
    Ok(parse_commit_files(&json))
}

fn parse_commit_files(json: &Value) -> Vec<FileChange> {
    let count = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
    let files = json["files"].as_array().into_iter().flatten();
    files
        .filter_map(|f| {
            let code = match f["status"].as_str()? {
                "added" => 'A',
                "removed" => 'D',
                "renamed" => 'R',
                "copied" => 'C',
                _ => 'M',
            };
            let stat = LineStat::Lines { added: count(&f["additions"]), removed: count(&f["deletions"]) };
            Some(FileChange { code, path: f["filename"].as_str()?.to_string(), stat })
        })
        .collect()
}

/// A commit's whole patch, from GitHub.
pub fn commit_diff(slug: &str, hash: &str) -> Result<String, String> {
    gh(&["api", "-H", "Accept: application/vnd.github.diff", &format!("repos/{slug}/commits/{hash}")])
}

/// The pull request's whole patch.
pub fn pr_diff(url: &str) -> Result<String, String> {
    gh(&["pr", "diff", url, "--color=never"])
}

fn gh(args: &[&str]) -> Result<String, String> {
    let out = Command::new("gh").args(args).output().map_err(|e| format!("gh: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().next().unwrap_or("gh failed").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The part of a multi-file patch for `path` (the new path of a rename): from its
/// `diff --git a/… b/<path>` header up to the next file's header.
pub fn patch_section<'a>(patch: &'a str, path: &str) -> Option<&'a str> {
    let headers: Vec<usize> = patch.match_indices("diff --git ").filter(|(i, _)| *i == 0 || patch[..*i].ends_with('\n')).map(|(i, _)| i).collect();
    headers.iter().enumerate().find_map(|(n, &start)| {
        let header = patch[start..].lines().next()?;
        let (_, b) = header.rsplit_once(" b/")?;
        (b == path).then(|| &patch[start..headers.get(n + 1).copied().unwrap_or(patch.len())])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_pr_urls_but_not_push_hints() {
        let out = "remote:      https://github.com/kamuno-ch/credit-check/pull/new/fix/x        \n\
                   https://github.com/kamuno-ch/credit-check/pull/27\nsee https://github.com/kamuno-ch/credit-check/pull/27.";
        assert_eq!(pr_urls(out), [PrRef { owner: "kamuno-ch".into(), repo: "credit-check".into(), number: 27 }]);
        assert!(pr_urls("https://github.com/o/r/issues/3 https://github.com/o/r").is_empty());
    }

    #[test]
    fn normalises_remote_urls_to_slugs() {
        assert_eq!(remote_slug("git@github.com:Kamuno-CH/infra.git").as_deref(), Some("kamuno-ch/infra"));
        assert_eq!(remote_slug("https://github.com/o/r").as_deref(), Some("o/r"));
        assert_eq!(remote_slug("ssh://git@github.com/o/r.git/").as_deref(), Some("o/r"));
        assert_eq!(remote_slug("git@gitlab.com:o/r.git"), None);
    }

    #[test]
    fn state_and_check_rollup() {
        let pr = |state: &str, draft: bool, checks: Value| parse_status(&json!({"state": state, "isDraft": draft, "statusCheckRollup": checks}));
        let run = |status: &str, conclusion: &str| json!({"status": status, "conclusion": conclusion});
        assert_eq!(pr("OPEN", true, json!([])), PrStatus { state: PrState::Draft, checks: None });
        assert_eq!(pr("MERGED", false, json!([run("COMPLETED", "SUCCESS")])).state, PrState::Merged);
        let checks = |list: Value| pr("OPEN", false, list).checks;
        assert_eq!(checks(json!([run("COMPLETED", "SUCCESS"), run("COMPLETED", "SKIPPED")])), Some(Checks::Passing));
        assert_eq!(checks(json!([run("COMPLETED", "SUCCESS"), run("IN_PROGRESS", "")])), Some(Checks::Pending));
        assert_eq!(checks(json!([run("IN_PROGRESS", ""), run("COMPLETED", "FAILURE")])), Some(Checks::Failing));
        assert_eq!(checks(json!([{"state": "PENDING"}, {"state": "SUCCESS"}])), Some(Checks::Pending));
        assert_eq!(checks(json!([{"state": "ERROR"}])), Some(Checks::Failing));
    }

    #[test]
    fn patch_section_cuts_one_file_including_renames() {
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n\
                     diff --git a/old name.rs b/new name.rs\nsimilarity index 90%\n\
                     diff --git a/c.rs b/c.rs\n+mentions diff --git a/zz b/zz inline\n";
        assert_eq!(patch_section(patch, "a.rs"), Some("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n"));
        assert_eq!(patch_section(patch, "new name.rs"), Some("diff --git a/old name.rs b/new name.rs\nsimilarity index 90%\n"));
        assert_eq!(patch_section(patch, "c.rs"), Some("diff --git a/c.rs b/c.rs\n+mentions diff --git a/zz b/zz inline\n"));
        assert_eq!(patch_section(patch, "zz"), None, "only headers at line starts count");
    }

    #[test]
    fn pr_view_holds_its_commits_and_merge_commit() {
        let view = parse_view(&json!({
            "state": "MERGED",
            "commits": [{"oid": "a1", "messageHeadline": "first", "committedDate": "2026-10-13T07:04:58Z"}],
            "mergeCommit": {"oid": "m9"},
            "mergedAt": "2026-10-13T09:35:45Z",
            "updatedAt": "2026-10-13T09:35:46Z"
        }));
        assert_eq!(view.commits, [PrCommit { oid: "a1".into(), subject: Some("first".into()), time: Some(1_791_875_098) }]);
        assert_eq!(view.merge, Some(PrCommit { oid: "m9".into(), subject: None, time: Some(1_791_884_145) }));
        assert!(view.holds("a1") && view.holds("m9") && !view.holds("zz"));
        assert_eq!(view.updated, Some(1_791_884_146));
    }

    #[test]
    fn commit_files_from_the_github_api() {
        let files = parse_commit_files(&json!({"files": [
            {"filename": "a.rs", "status": "modified", "additions": 3, "deletions": 1},
            {"filename": "b.rs", "status": "renamed", "previous_filename": "old.rs", "additions": 0, "deletions": 0},
            {"filename": "c.rs", "status": "removed", "additions": 0, "deletions": 9}
        ]}));
        let codes: Vec<(char, &str)> = files.iter().map(|f| (f.code, f.path.as_str())).collect();
        assert_eq!(codes, [('M', "a.rs"), ('R', "b.rs"), ('D', "c.rs")]);
        assert_eq!(files[0].stat, LineStat::Lines { added: 3, removed: 1 });
    }
}
