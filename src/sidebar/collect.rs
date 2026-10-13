//! Gathering what the sidebar shows: the followed chat, the repos and pull requests it
//! touched, their git status, and pull-request status from GitHub.

use super::{Model, Session, Statuses};
use crate::git::{self, Commit};
use crate::github::{self, PrRef, PrView};
use crate::herdr::{self, PaneInfo};
use crate::repos;
use crate::state::{self, PrRecord, RepoRecord, SessionState};
use crate::transcript::{Format, Touch, Transcript};
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

/// Pull-request statuses and commits fetched in the background for one chat.
pub(super) struct PrBatch {
    session: String,
    results: Vec<(PrRef, Result<PrView, String>)>,
}

/// How many of a chat's most recent links are kept.
const LINKS_KEPT: usize = 50;
/// A tool call still running after this long is taken to have died with its agent: commits
/// later than that are not counted as the chat's.
const CALL_MAX_MS: i64 = 2 * 60 * 60 * 1000;

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
    let switched = model.session.as_ref().map(|s| &s.saved.key) != Some(&key);
    let session = match &mut model.session {
        Some(session) if !switched => {
            session.pane_id = target.pane_id.clone();
            session.title = title;
            session
        }
        slot => {
            let saved = if persisted { state::load_session(&key) } else { SessionState { key, ..SessionState::default() } };
            let fallback_cwd = target.cwd.clone().map(PathBuf::from).unwrap_or_default();
            let transcript = target
                .agent
                .as_deref()
                .and_then(Format::of_agent)
                .zip(target.agent_session.as_ref())
                .map(|(format, s)| Transcript::new(format, s.value.clone(), fallback_cwd));
            slot.insert(Session {
                pane_id: target.pane_id.clone(),
                title,
                saved,
                persisted,
                transcript,
                links: Vec::new(),
                calls: Vec::new(),
            })
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

    let saved = &mut session.saved;
    let mut dirty = false;
    // Newest first; mentioning a dismissed link again, after the dismissal, brings it back.
    for (url, at) in activity.links {
        dirty |= saved.revive_link(&url, at);
        session.links.retain(|l| *l != url);
        session.links.insert(0, url);
    }
    session.links.truncate(LINKS_KEPT);
    session.calls.extend(activity.calls);
    for touch in touches {
        let Some(root) = repos::repo_root(&touch.path) else { continue };
        // A tool call after the dismissal brings a dismissed repo back; the agent merely
        // sitting in it (process working dirs) does not.
        if let Some(at) = touch.at {
            dirty |= saved.revive_repo(&root, at);
        }
        match saved.repos.iter_mut().find(|r| r.root == root) {
            Some(record) if touch.writes && !record.changed => {
                record.changed = true;
                dirty = true;
            }
            Some(_) => {}
            None => {
                saved.repos.push(RepoRecord { root, changed: touch.writes, baseline: None });
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
        if !saved.prs.iter().any(|r| r.pr.same(&pr)) {
            saved.prs.push(PrRecord { pr, root: None });
            dirty = true;
        }
    }
    // A PR belongs to the touched checkout whose remote is its repo, and is listed under it.
    for pr in saved.prs.iter_mut().filter(|p| p.root.is_none()) {
        let slug = pr.pr.slug();
        let owner = saved.repos.iter().find(|r| {
            model.remotes.entry(r.root.clone()).or_insert_with(|| github::remote_slugs(&r.root)).contains(&slug)
        });
        if let Some(record) = owner {
            pr.root = Some(record.root.clone());
            dirty = true;
        }
    }
    if dirty {
        session.save()?;
    }
    Ok(switched || dirty)
}

/// A shown PR neither fetched nor failed yet: fetched at once rather than with the next
/// periodic refresh.
pub(super) fn has_unfetched_pr(model: &Model) -> bool {
    model.session.as_ref().is_some_and(|s| {
        s.saved.visible_prs().any(|p| !model.pr_views.contains_key(&p.pr.url()) && !model.pr_errors.contains_key(&p.pr.url()))
    })
}

/// A PR's status, title and commits are fetched until it is merged or closed: once per sidebar
/// for those.
fn needs_fetch(model: &Model, pr: &PrRecord) -> bool {
    model.pr_views.get(&pr.pr.url()).is_none_or(|view| !view.status.state.is_final())
}

/// Fetches every shown PR in need (see [`needs_fetch`]) on a background thread.
pub(super) fn start_pr_refresh(model: &Model) -> Option<Receiver<PrBatch>> {
    let session = model.session.as_ref()?;
    let todo: Vec<PrRef> = session.saved.visible_prs().filter(|p| needs_fetch(model, p)).map(|p| p.pr.clone()).collect();
    if todo.is_empty() {
        return None;
    }
    let key = session.saved.key.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let results = std::thread::scope(|scope| {
            let jobs: Vec<_> = todo.iter().map(|pr| scope.spawn(move || github::pr_view(pr))).collect();
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

/// Takes in a batch fetched for the followed chat. A failure keeps the last view: the row
/// shows what was fetched before.
pub(super) fn apply_prs(model: &mut Model, batch: PrBatch) {
    if model.session.as_ref().is_none_or(|s| s.saved.key != batch.session) {
        return;
    }
    for (pr, result) in batch.results {
        match result {
            Ok(view) => {
                model.pr_errors.remove(&pr.url());
                model.pr_views.insert(pr.url(), view);
            }
            Err(e) => {
                model.pr_errors.insert(pr.url(), e);
            }
        }
    }
}

/// Records each touched repo's fingerprint at first touch, and marks it changed once the
/// fingerprint moves: catches changes made by shell commands, not just edit/write tools.
pub(super) fn track_changes(model: &mut Model) -> Result<()> {
    let Some(session) = &mut model.session else { return Ok(()) };
    let mut dirty = false;
    for record in &mut session.saved.repos {
        let Some(Ok(status)) = model.statuses.get(&record.root) else { continue };
        match record.baseline {
            None => record.baseline = Some(status.fingerprint),
            Some(base) if base != status.fingerprint && !record.changed => record.changed = true,
            Some(_) => continue,
        }
        dirty = true;
    }
    if dirty {
        session.save()?;
    }
    Ok(())
}

/// Every shown repo's `git status`, and the commits the chat made in it (see [`call_windows`]).
pub(super) fn refresh_git(model: &Model) -> (Statuses, HashMap<PathBuf, Vec<Commit>>) {
    let Some(session) = &model.session else { return Default::default() };
    let windows = call_windows(session, now_ms());
    let windows = windows.as_slice();
    std::thread::scope(|scope| {
        // Repos deleted since they were touched stay recorded but are not shown.
        let jobs: Vec<_> = session
            .saved
            .repos
            .iter()
            .map(|r| &r.root)
            .filter(|root| root.join(".git").exists())
            .map(|root| (root.clone(), scope.spawn(move || (git::status(root), git::commits_within(root, windows)))))
            .collect();
        let mut statuses = HashMap::new();
        let mut commits = HashMap::new();
        for (root, job) in jobs {
            let (status, made) = job.join().unwrap_or_else(|_| (Err("git status panicked".into()), Vec::new()));
            statuses.insert(root.clone(), status);
            commits.insert(root, made);
        }
        (statuses, commits)
    })
}

/// When the chat was acting: its tool calls, finished or still running (those up to now), in
/// no particular order. Unix milliseconds.
pub(super) fn call_windows(session: &Session, now: i64) -> Vec<(i64, i64)> {
    let running = session.transcript.iter().flat_map(Transcript::running).map(|start| (start, now.min(start + CALL_MAX_MS)));
    session.calls.iter().copied().chain(running).collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::PrState;
    use crate::sidebar::fixture::{fixture, pr, session, view};

    #[test]
    fn call_windows_are_the_chat_s_calls() {
        let mut model = fixture(&[], &[]);
        session(&mut model).calls = vec![(50, 60), (10, 20), (15, 30)];
        assert_eq!(call_windows(session(&mut model), 100), [(50, 60), (10, 20), (15, 30)]);
    }

    #[test]
    fn prs_are_fetched_until_merged_or_closed() {
        let (open, merged, failed) = (pr("r", 1), pr("r", 2), pr("r", 3));
        let mut model = fixture(&[], &[open.clone(), merged.clone(), failed.clone()]);
        assert!(has_unfetched_pr(&model), "nothing fetched yet: fetch at once");
        model.pr_views.insert(open.url(), view(PrState::Open, Vec::new()));
        model.pr_views.insert(merged.url(), view(PrState::Merged, Vec::new()));
        model.pr_errors.insert(failed.url(), "gh: offline".into());
        assert!(!has_unfetched_pr(&model), "fetched or failed: left to the periodic refresh");
        let saved = &model.session.as_ref().unwrap().saved;
        let due: Vec<u32> = saved.visible_prs().filter(|p| needs_fetch(&model, p)).map(|p| p.pr.number).collect();
        assert_eq!(due, [1, 3], "a merged PR does not change any more");
    }

    #[test]
    fn a_failed_fetch_keeps_the_last_view() {
        let mut model = fixture(&[], &[pr("r", 1)]);
        let batch = |result| PrBatch { session: "s".into(), results: vec![(pr("r", 1), result)] };
        apply_prs(&mut model, batch(Ok(view(PrState::Open, Vec::new()))));
        apply_prs(&mut model, batch(Err("gh: offline".into())));
        assert!(model.pr_views.contains_key(&pr("r", 1).url()));
        assert_eq!(model.pr_errors.get(&pr("r", 1).url()).map(String::as_str), Some("gh: offline"));
        apply_prs(&mut model, PrBatch { session: "another chat".into(), results: vec![(pr("r", 2), Err("x".into()))] });
        assert!(!model.pr_errors.contains_key(&pr("r", 2).url()), "a batch of a chat no longer followed");
    }
}
