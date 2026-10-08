//! Gathering what the sidebar shows: the followed chat, the repos and pull requests it
//! touched, their git status, and pull-request status from GitHub.

use super::{Model, Session};
use crate::git::{self, RepoStatus};
use crate::github::{self, PrRef, PrStatus};
use crate::herdr::{self, PaneInfo};
use crate::repos;
use crate::state::{self, PrRecord, RepoRecord, SessionRepos};
use crate::transcript::{Format, Touch, Transcript};
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

/// Pull-request statuses fetched in the background for one chat.
pub(super) struct PrBatch {
    session: String,
    results: Vec<(PrRef, Result<PrStatus, String>)>,
}

/// How many of a chat's most recent links are kept.
const LINKS_KEPT: usize = 50;

/// Picks the followed agent pane and records repos under its processes' working dirs.
/// Returns whether the repo list changed.
pub(super) fn sample(me: &str, model: &mut Model) -> Result<bool> {
    let layout = herdr::pane_layout(me)?;
    let panes: Vec<PaneInfo> = herdr::pane_list()?
        .into_iter()
        .filter(|p| p.tab_id == layout.tab_id && p.pane_id != me && p.agent.is_some())
        .collect();
    let previous = model.session.as_ref().map(|s| s.pane_id.as_str());
    let Some(target) = panes
        .iter()
        .find(|p| p.pane_id == layout.focused_pane_id)
        .or_else(|| panes.iter().find(|p| Some(p.pane_id.as_str()) == previous))
        .or_else(|| panes.first())
    else {
        let changed = model.session.is_some();
        model.session = None;
        return Ok(changed);
    };

    let (key, persisted) = match &target.agent_session {
        Some(s) => (s.value.clone(), true),
        None => (format!("terminal-{}", target.terminal_id), false),
    };
    let title = target.terminal_title_stripped.clone().or_else(|| target.agent.clone()).unwrap_or_default();
    let switched = model.session.as_ref().map(|s| &s.repos.session) != Some(&key);
    let session = match &mut model.session {
        Some(session) if !switched => {
            session.pane_id = target.pane_id.clone();
            session.title = title;
            session
        }
        slot => {
            let repos = if persisted {
                state::load_session(&key)
            } else {
                SessionRepos { session: key, ..SessionRepos::default() }
            };
            let fallback_cwd = target.cwd.clone().map(PathBuf::from).unwrap_or_default();
            let transcript = target
                .agent
                .as_deref()
                .and_then(Format::of_agent)
                .zip(target.agent_session.as_ref())
                .map(|(format, s)| Transcript::new(format, s.value.clone(), fallback_cwd));
            slot.insert(Session { pane_id: target.pane_id.clone(), title, repos, persisted, transcript, links: Vec::new() })
        }
    };

    // Transcript first: on attach it replays the whole chat in order, so repos keep
    // first-touch order. A transcript the agent has not written yet just yields nothing.
    let activity = session.transcript.as_mut().and_then(|t| t.poll().ok()).unwrap_or_default();
    let mut touches = activity.touches;
    let mut observed: Vec<PathBuf> =
        [&target.foreground_cwd, &target.cwd].into_iter().flatten().map(PathBuf::from).collect();
    if let Some(pid) = herdr::pane_process_info(&target.pane_id)?.shell_pid {
        observed.extend(repos::process_tree_cwds(pid));
    }
    touches.extend(observed.into_iter().map(|path| Touch { path, writes: false, at: None }));

    let SessionRepos { repos: records, prs, dismissed_repos, dismissed_links, .. } = &mut session.repos;
    let mut dirty = false;
    // Newest first; mentioning a dismissed link again, after the dismissal, brings it back.
    for (url, at) in activity.links {
        let before = dismissed_links.len();
        dismissed_links.retain(|d| !(d.url == url && d.at < at));
        dirty |= dismissed_links.len() != before;
        session.links.retain(|l| *l != url);
        session.links.insert(0, url);
    }
    session.links.truncate(LINKS_KEPT);
    for touch in touches {
        let Some(root) = repos::repo_root(&touch.path) else { continue };
        // A tool call after the dismissal brings a dismissed repo back; the agent merely
        // sitting in it (process working dirs) does not.
        if let Some(at) = touch.at {
            let before = dismissed_repos.len();
            dismissed_repos.retain(|d| !(d.root == root && d.at < at));
            dirty |= dismissed_repos.len() != before;
        }
        match records.iter_mut().find(|r| r.root == root) {
            Some(record) if touch.writes && !record.changed => {
                record.changed = true;
                dirty = true;
            }
            Some(_) => {}
            None => {
                records.push(RepoRecord { root, changed: touch.writes, baseline: None });
                dirty = true;
            }
        }
    }
    // PRs named by number belong to the repository of the directory they were used in.
    let mut mentioned = activity.prs;
    for (number, dir) in activity.pr_numbers {
        let Some(root) = repos::repo_root(&dir) else { continue };
        let slugs = model.remotes.entry(root.clone()).or_insert_with(|| github::remote_slugs(&root));
        if let Some((owner, repo)) = slugs.first().and_then(|slug| slug.split_once('/')) {
            mentioned.push(PrRef { owner: owner.into(), repo: repo.into(), number });
        }
    }
    for pr in mentioned {
        if !prs.iter().any(|r| r.pr.same(&pr)) {
            prs.push(PrRecord { pr, root: None, status: None });
            dirty = true;
        }
    }
    // A PR belongs to the touched checkout whose remote is its repo, and is listed under it.
    for pr in prs.iter_mut().filter(|p| p.root.is_none()) {
        let slug = pr.pr.slug();
        let owner = records.iter().find(|r| {
            model.remotes.entry(r.root.clone()).or_insert_with(|| github::remote_slugs(&r.root)).contains(&slug)
        });
        if let Some(record) = owner {
            pr.root = Some(record.root.clone());
            dirty = true;
        }
    }
    if dirty && session.persisted {
        state::save_session(&session.repos)?;
    }
    Ok(switched || dirty)
}

pub(super) fn is_dismissed(repos: &SessionRepos, root: &Path) -> bool {
    repos.dismissed_repos.iter().any(|d| d.root == root)
}

/// Pull requests still shown: not dismissed, and not under a dismissed repo.
pub(super) fn visible_prs(repos: &SessionRepos) -> impl Iterator<Item = &PrRecord> {
    repos.prs.iter().filter(|p| {
        !repos.dismissed_prs.iter().any(|d| d.same(&p.pr)) && !p.root.as_ref().is_some_and(|root| is_dismissed(repos, root))
    })
}

pub(super) fn has_unfetched_pr(model: &Model) -> bool {
    model.session.as_ref().is_some_and(|s| {
        visible_prs(&s.repos).any(|p| p.status.is_none() && !model.pr_errors.contains_key(&p.pr.url()))
    })
}

/// Fetches every non-final PR's status on a background thread.
pub(super) fn start_pr_refresh(model: &Model) -> Option<Receiver<PrBatch>> {
    let session = model.session.as_ref()?;
    let todo: Vec<PrRef> = visible_prs(&session.repos)
        .filter(|p| !p.status.is_some_and(|s| s.state.is_final()))
        .map(|p| p.pr.clone())
        .collect();
    if todo.is_empty() {
        return None;
    }
    let key = session.repos.session.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let results = std::thread::scope(|scope| {
            let jobs: Vec<_> = todo.iter().map(|pr| scope.spawn(move || github::pr_status(pr))).collect();
            todo.iter()
                .cloned()
                .zip(jobs)
                .map(|(pr, job)| (pr, job.join().unwrap_or_else(|_| Err("gh panicked".into()))))
                .collect()
        });
        let _ = tx.send(PrBatch { session: key, results });
    });
    Some(rx)
}

pub(super) fn apply_prs(model: &mut Model, batch: PrBatch) -> Result<()> {
    let Some(session) = model.session.as_mut().filter(|s| s.repos.session == batch.session) else { return Ok(()) };
    let mut dirty = false;
    for (pr, result) in batch.results {
        match result {
            Ok(status) => {
                model.pr_errors.remove(&pr.url());
                if let Some(record) = session.repos.prs.iter_mut().find(|r| r.pr == pr && r.status != Some(status)) {
                    record.status = Some(status);
                    dirty = true;
                }
            }
            Err(e) => {
                model.pr_errors.insert(pr.url(), e);
            }
        }
    }
    if dirty && session.persisted {
        state::save_session(&session.repos)?;
    }
    Ok(())
}

/// Records each touched repo's fingerprint at first touch, and marks it changed once the
/// fingerprint moves: catches changes made by shell commands, not just edit/write tools.
pub(super) fn track_changes(model: &mut Model) -> Result<()> {
    let Some(session) = &mut model.session else { return Ok(()) };
    let mut dirty = false;
    for record in &mut session.repos.repos {
        let Some(Ok(status)) = model.statuses.get(&record.root) else { continue };
        match record.baseline {
            None => record.baseline = Some(status.fingerprint),
            Some(base) if base != status.fingerprint && !record.changed => record.changed = true,
            Some(_) => continue,
        }
        dirty = true;
    }
    if dirty && session.persisted {
        state::save_session(&session.repos)?;
    }
    Ok(())
}

pub(super) fn refresh_git(model: &Model) -> HashMap<PathBuf, Result<RepoStatus, String>> {
    let Some(session) = &model.session else { return HashMap::new() };
    std::thread::scope(|scope| {
        // Repos deleted since they were touched stay recorded but are not shown.
        let jobs: Vec<_> = session
            .repos
            .repos
            .iter()
            .map(|r| &r.root)
            .filter(|root| root.join(".git").exists())
            .map(|root| (root.clone(), scope.spawn(move || git::status(root))))
            .collect();
        jobs.into_iter()
            .map(|(root, job)| (root, job.join().unwrap_or_else(|_| Err("git status panicked".into()))))
            .collect()
    })
}
