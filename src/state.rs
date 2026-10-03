//! Plugin-owned files under `HERDR_PLUGIN_STATE_DIR`: which tab holds which sidebar pane,
//! and the repos and pull requests of each agent chat session.

use crate::github::{PrRef, PrStatus};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/state/herdr/plugins").join(crate::PLUGIN_ID)
}

fn write_atomic(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DockState {
    /// Sidebar pane id per tab id.
    pub tabs: BTreeMap<String, String>,
}

/// Exclusive hold on the dock state; concurrent hooks serialize on `dock.lock`.
pub struct DockGuard {
    _lock: File,
    path: PathBuf,
    pub state: DockState,
}

impl DockGuard {
    pub fn acquire() -> Result<Self> {
        let dir = state_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = File::create(dir.join("dock.lock"))?;
        lock.lock()?;
        let path = dir.join("dock.json");
        let state = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Self { _lock: lock, path, state })
    }

    pub fn save(&self) -> Result<()> {
        write_atomic(&self.path, &serde_json::to_vec_pretty(&self.state)?)
    }
}

/// Repos one agent chat session has touched, in first-touch order, and the PRs it opened.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionRepos {
    pub session: String,
    pub repos: Vec<RepoRecord>,
    #[serde(default)]
    pub prs: Vec<PrRecord>,
    /// Repos the user dismissed from this chat's sidebar (right-click menu).
    #[serde(default)]
    pub dismissed_repos: Vec<DismissedRepo>,
    /// Pull requests the user dismissed from this chat's sidebar.
    #[serde(default)]
    pub dismissed_prs: Vec<PrRef>,
}

/// A repo hidden from the sidebar until the chat touches it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DismissedRepo {
    pub root: PathBuf,
    /// Transcript position at dismissal; only tool calls after it bring the repo back.
    pub at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrRecord {
    pub pr: PrRef,
    /// Local checkout whose remote is the PR's repo; `None` until one is touched.
    pub root: Option<PathBuf>,
    /// Last fetched status. Merged/closed PRs keep it and are not fetched again.
    pub status: Option<PrStatus>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoRecord {
    pub root: PathBuf,
    /// The session wrote here, or the repo's git fingerprint moved since first touch.
    pub changed: bool,
    /// Git fingerprint at first touch; `None` until the first status after the touch.
    pub baseline: Option<u64>,
}

fn session_file(key: &str) -> PathBuf {
    let name: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    let name = &name[name.len().saturating_sub(200)..];
    state_dir().join("sessions").join(format!("{name}.json"))
}

pub fn load_session(key: &str) -> SessionRepos {
    std::fs::read(session_file(key))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| SessionRepos { session: key.to_string(), ..SessionRepos::default() })
}

pub fn save_session(repos: &SessionRepos) -> Result<()> {
    let path = session_file(&repos.session);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(&path, &serde_json::to_vec_pretty(repos)?)
}

/// Session files untouched for this long belong to chats that are over.
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// Deletes plugin files nothing uses any more: diff requests of panes that are gone, and
/// session files of chats untouched for 30 days that no open pane belongs to.
pub fn prune(live_panes: &[&str], live_sessions: &[&str]) -> Result<usize> {
    let keep_diffs: Vec<PathBuf> = live_panes.iter().map(|p| crate::diff::request_file(p)).collect();
    let keep_sessions: Vec<PathBuf> = live_sessions.iter().map(|s| session_file(s)).collect();
    let mut removed = 0;
    for (dir, keep, stale) in [
        (state_dir().join("diff"), keep_diffs, None),
        (state_dir().join("sessions"), keep_sessions, Some(SESSION_TTL)),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let old = |ttl| entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > ttl));
            if !keep.contains(&path) && stale.is_none_or(old) {
                std::fs::remove_file(&path)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}
