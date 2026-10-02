//! GitHub pull requests: recognising PR URLs, matching them to local checkouts by remote,
//! and reading state + CI rollup through the `gh` CLI.

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

pub fn pr_status(pr: &PrRef) -> Result<PrStatus, String> {
    let json = gh(&["pr", "view", &pr.url(), "--json", "state,isDraft,statusCheckRollup"])?;
    let json: Value = serde_json::from_str(&json).map_err(|e| format!("gh output: {e}"))?;
    Ok(parse_status(&json))
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

/// One file of a pull request, as `gh pr view --json files` reports it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PrFile {
    pub path: String,
    #[serde(rename = "changeType", default)]
    pub change_type: String,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
}

impl PrFile {
    /// Status letter in the style of `git status`: `A`, `D`, `R`, `C` or `M`.
    pub fn code(&self) -> char {
        match self.change_type.as_str() {
            "ADDED" => 'A',
            "DELETED" => 'D',
            "RENAMED" => 'R',
            "COPIED" => 'C',
            _ => 'M',
        }
    }
}

pub fn pr_files(url: &str) -> Result<Vec<PrFile>, String> {
    let json = gh(&["pr", "view", url, "--json", "files"])?;
    let mut value: Value = serde_json::from_str(&json).map_err(|e| format!("gh output: {e}"))?;
    serde_json::from_value(value["files"].take()).map_err(|e| format!("gh files: {e}"))
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
}
