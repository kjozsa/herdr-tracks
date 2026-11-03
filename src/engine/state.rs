//! Files under `HERDR_PLUGIN_STATE_DIR`: the repos and pull requests of each agent chat session,
//! shared by every frontend; frontends keep their own files beside them.

use crate::engine::github::PrRef;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/state/herdr/plugins").join(crate::PLUGIN_ID)
}

/// Writes the file whole or not at all (through a temporary file), creating its directory.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// What one agent chat session keeps: the repos it touched, in first-touch order, the PRs it
/// opened or referred to, and what the user dismissed from its sidebar.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionState {
    /// The chat session's herdr reference.
    #[serde(rename = "session")]
    pub key: String,
    pub repos: Vec<RepoRecord>,
    #[serde(default)]
    pub prs: Vec<PrRecord>,
    /// Repos the user dismissed from this chat's sidebar (right-click menu).
    #[serde(default)]
    pub dismissed_repos: Vec<DismissedRepo>,
    /// Pull requests the user dismissed from this chat's sidebar.
    #[serde(default)]
    pub dismissed_prs: Vec<PrRef>,
    /// Links the user dismissed from this chat's sidebar.
    #[serde(default)]
    pub dismissed_links: Vec<DismissedLink>,
}

/// What the user dismisses from a chat's sidebar.
pub enum Dismissal<'a> {
    /// A repo, with its pull requests, until the chat touches it again.
    Repo(&'a Path),
    /// A pull request, for good.
    Pr(&'a PrRef),
    /// Links, until the chat mentions them again.
    Links(&'a [String]),
}

impl SessionState {
    pub fn repo_dismissed(&self, root: &Path) -> bool {
        self.dismissed_repos.iter().any(|d| d.root == root)
    }

    pub fn link_dismissed(&self, url: &str) -> bool {
        self.dismissed_links.iter().any(|d| d.url == url)
    }

    /// Pull requests still shown: not dismissed, and not under a dismissed repo.
    pub fn visible_prs(&self) -> impl Iterator<Item = &PrRecord> {
        self.prs.iter().filter(|p| {
            !self.dismissed_prs.iter().any(|d| d.same(&p.pr)) && !p.root.as_ref().is_some_and(|root| self.repo_dismissed(root))
        })
    }

    /// Hides what is not hidden yet, as of transcript position `at`. Returns whether anything
    /// changed.
    pub fn dismiss(&mut self, what: Dismissal, at: u64) -> bool {
        match what {
            Dismissal::Repo(root) if !self.repo_dismissed(root) => {
                self.dismissed_repos.push(DismissedRepo { root: root.to_path_buf(), at });
            }
            Dismissal::Pr(pr) if !self.dismissed_prs.iter().any(|d| d.same(pr)) => self.dismissed_prs.push(pr.clone()),
            Dismissal::Links(urls) => {
                let before = self.dismissed_links.len();
                for url in urls {
                    if !self.link_dismissed(url) {
                        self.dismissed_links.push(DismissedLink { url: url.clone(), at });
                    }
                }
                return self.dismissed_links.len() != before;
            }
            _ => return false,
        }
        true
    }

    /// A tool call at transcript position `at` touched the repo: brings it back if it was
    /// dismissed before. Returns whether it was.
    pub fn revive_repo(&mut self, root: &Path, at: u64) -> bool {
        let before = self.dismissed_repos.len();
        self.dismissed_repos.retain(|d| !(d.root == root && d.at < at));
        self.dismissed_repos.len() != before
    }

    /// The chat mentioned the link at transcript position `at`: brings it back if it was
    /// dismissed before. Returns whether it was.
    pub fn revive_link(&mut self, url: &str, at: u64) -> bool {
        let before = self.dismissed_links.len();
        self.dismissed_links.retain(|d| !(d.url == url && d.at < at));
        self.dismissed_links.len() != before
    }
}

/// A repo hidden from the sidebar until the chat touches it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DismissedRepo {
    pub root: PathBuf,
    /// Transcript position at dismissal; only tool calls after it bring the repo back.
    pub at: u64,
}

/// A link hidden from the sidebar until the chat mentions it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DismissedLink {
    pub url: String,
    /// Transcript position at dismissal; only mentions after it bring the link back.
    pub at: u64,
}

/// A pull request of the chat. Its status is fetched from GitHub by each sidebar, not kept.
#[derive(Debug, Serialize, Deserialize)]
pub struct PrRecord {
    pub pr: PrRef,
    /// Local checkout whose remote is the PR's repo; `None` until one is touched.
    pub root: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoRecord {
    pub root: PathBuf,
    /// The session wrote here, or the repo's git fingerprint moved since first touch.
    pub changed: bool,
    /// Git fingerprint at first touch; `None` until the first status after the touch.
    pub baseline: Option<u64>,
}

/// A session key or pane id as a file name in `dir`: other characters than ASCII letters,
/// digits, `-`, `_` and `.` become `_`, and long keys keep their end.
pub fn keyed_file(dir: &str, key: &str) -> PathBuf {
    let name: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    let name = &name[name.len().saturating_sub(200)..];
    state_dir().join(dir).join(format!("{name}.json"))
}

fn session_file(key: &str) -> PathBuf {
    keyed_file("sessions", key)
}

pub fn load_session(key: &str) -> SessionState {
    std::fs::read(session_file(key))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| SessionState { key: key.to_string(), ..SessionState::default() })
}

pub fn save_session(state: &SessionState) -> Result<()> {
    write_atomic(&session_file(&state.key), &serde_json::to_vec_pretty(state)?)
}

/// Session files untouched for this long belong to chats that are over.
const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// Deletes the session files of chats untouched for 30 days, but those of `live_sessions`.
pub fn prune_sessions(live_sessions: &[&str]) -> Result<usize> {
    let keep: Vec<PathBuf> = live_sessions.iter().map(|s| session_file(s)).collect();
    prune_files(&state_dir().join("sessions"), &keep, Some(SESSION_TTL))
}

/// Deletes the files in `dir` but `keep`; with `ttl`, only those untouched for that long.
pub fn prune_files(dir: &Path, keep: &[PathBuf], ttl: Option<Duration>) -> Result<usize> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(0) };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let old = |ttl| entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > ttl));
        if !keep.contains(&path) && ttl.is_none_or(old) {
            std::fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismissed_repos_and_links_come_back_only_when_named_after_the_dismissal() {
        let mut state = SessionState::default();
        assert!(state.dismiss(Dismissal::Repo(Path::new("/r/lib")), 10));
        assert!(!state.revive_repo(Path::new("/r/lib"), 10) && state.revive_repo(Path::new("/r/lib"), 11));
        let links = ["https://a.io".to_string(), "https://b.io".to_string()];
        assert!(state.dismiss(Dismissal::Links(&links[..1]), 20));
        assert!(state.dismiss(Dismissal::Links(&links), 21), "the one not dismissed yet");
        assert!(!state.dismiss(Dismissal::Links(&links), 22));
        assert!(!state.revive_link("https://a.io", 20) && state.revive_link("https://a.io", 30));
        assert!(!state.link_dismissed("https://a.io") && state.link_dismissed("https://b.io"));
    }

    #[test]
    fn session_files_load_with_or_without_a_pr_status() {
        let pr = |n, rest| format!(r#"{{"pr":{{"owner":"o","repo":"r","number":{n}}},"root":null{rest}}}"#);
        let json = format!(r#"{{"session":"s","repos":[],"prs":[{},{}]}}"#, pr(5, r#","status":{"state":"Merged","checks":null}"#), pr(6, ""));
        let state: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!((state.key.as_str(), state.prs.len()), ("s", 2));
        assert_eq!(serde_json::to_value(&state).unwrap()["session"], "s");
    }
}
