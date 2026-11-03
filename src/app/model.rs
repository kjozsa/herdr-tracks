//! A chat's tracks as JSON for the `tracks` mod: what to list, in order, and the diff request
//! and URL each row stands for. Laying out and capping rows is the mod's.

use herdr_tracks::engine::git::{Change, LineStat};
use herdr_tracks::engine::github::{Checks, PrState};
use herdr_tracks::engine::patch::{CommitAt, DiffRequest};
use herdr_tracks::engine::place::{self, Entry, Placed};
use herdr_tracks::engine::state::PrRecord;
use herdr_tracks::engine::Tracker;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn build(terminal: &str, tracker: &Tracker) -> Value {
    let Some(session) = &tracker.session else {
        return json!({ "terminal": terminal, "error": tracker.error, "repos": [], "orphans": [], "links": [] });
    };
    let placed = place::place(tracker, &session.saved);
    let repos: Vec<Value> = placed.shown.iter().map(|root| repo(tracker, &placed, root)).collect();
    let orphans: Vec<Value> =
        placed.prs.iter().filter(|(_, at)| at.is_none()).map(|(pr, _)| pr_json(tracker, &placed, pr, None)).collect();
    let files: serde_json::Map<String, Value> = tracker
        .commit_files
        .iter()
        .map(|(hash, files)| {
            let value = match files {
                Ok(files) => json!({ "files": files.iter().map(|f| json!({ "code": f.code.to_string(), "path": f.path, "stat": stat(Some(f.stat)) })).collect::<Vec<_>>() }),
                Err(e) => json!({ "error": e }),
            };
            (hash.clone(), value)
        })
        .collect();
    json!({
        "terminal": terminal,
        "title": session.title,
        "error": tracker.error,
        "repos": repos,
        "orphans": orphans,
        "links": session.shown_links(),
        "files": files,
    })
}

fn repo(tracker: &Tracker, placed: &Placed, root: &PathBuf) -> Value {
    let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
    let mut repo = json!({ "root": root, "name": name, "diff": DiffRequest::Worktree { root: root.clone() } });
    match tracker.statuses.get(root) {
        Some(Ok(s)) => {
            repo["branch"] = json!(s.branch);
            repo["ahead"] = json!(s.ahead);
            repo["behind"] = json!(s.behind);
            repo["changes"] = s.changes.iter().map(|c| change(root, c)).collect();
        }
        Some(Err(e)) => repo["error"] = json!(e),
        None => {}
    }
    let slug = tracker.remotes.get(root).and_then(|slugs| slugs.first()).cloned();
    let history: Vec<Value> = place::history(tracker, placed, root)
        .iter()
        .map(|entry| match entry {
            Entry::Pr { pr, .. } => json!({ "pr": pr_json(tracker, placed, pr, Some(root)) }),
            Entry::Commit(c) => {
                let at = CommitAt { hash: c.hash.clone(), root: Some(root.clone()), repo: slug.clone() };
                json!({ "commit": commit(at, &c.subject, true) })
            }
        })
        .collect();
    repo["history"] = history.into();
    repo
}

fn change(root: &Path, c: &Change) -> Value {
    json!({
        "code": String::from_utf8_lossy(&c.code),
        "path": c.path,
        "display": c.display(),
        "stat": stat(c.stat),
        "diff": DiffRequest::local(root, c),
    })
}

fn stat(stat: Option<LineStat>) -> Value {
    match stat {
        Some(LineStat::Lines { added, removed }) => json!({ "added": added, "removed": removed }),
        Some(LineStat::Binary) => json!({ "binary": true }),
        None => Value::Null,
    }
}

fn commit(at: CommitAt, subject: &str, chat: bool) -> Value {
    json!({
        "hash": at.hash,
        "subject": subject,
        "chat": chat,
        "url": at.url(),
        "diff": DiffRequest::Commit { commit: at.clone(), path: None },
        "at": at,
    })
}

/// A pull request listed under `root` (`None`: under a heading of its own).
fn pr_json(tracker: &Tracker, placed: &Placed, pr: &PrRecord, root: Option<&PathBuf>) -> Value {
    let url = pr.pr.url();
    let mut value = json!({
        "url": url,
        "number": pr.pr.number,
        "repo": format!("{}/{}", pr.pr.owner, pr.pr.repo),
        "target": pr.pr,
        "diff": DiffRequest::Pr { url: url.clone() },
        "error": tracker.pr_errors.get(&url),
    });
    if let Some(view) = tracker.pr_views.get(&url) {
        let state = match view.status.state {
            PrState::Open => "open",
            PrState::Draft => "draft",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        };
        let checks = view.status.checks.map(|c| match c {
            Checks::Passing => "passing",
            Checks::Pending => "pending",
            Checks::Failing => "failing",
        });
        value["state"] = json!(state);
        value["checks"] = json!(checks);
        value["title"] = json!(view.title);
        value["commits"] = place::pr_commits(placed, view)
            .iter()
            .map(|c| commit(CommitAt { hash: c.oid.to_string(), root: root.cloned(), repo: Some(pr.pr.slug()) }, c.subject, c.chat))
            .collect();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_tracks::engine::fixture::{fixture, gh, pr, root, view};
    use herdr_tracks::engine::git::Commit;

    #[test]
    fn repos_list_their_prs_and_loose_commits_with_the_diffs_they_show() {
        let mut tracker = fixture(&["e2e"], &[pr("e2e", 5), pr("infra", 9)]);
        let commit = |hash: &str, time| Commit { hash: hash.into(), time, subject: format!("commit {hash}"), on_head: true };
        tracker.commits.insert(root("e2e"), vec![commit("h2", 2), commit("h1", 1)]);
        tracker.pr_views.insert(pr("e2e", 5).url(), view(PrState::Open, vec![gh("h1", "commit h1", 1)]));
        let model = build("t2", &tracker);
        let repo = &model["repos"][0];
        assert_eq!(repo["name"], "e2e");
        assert_eq!(repo["diff"], json!({ "kind": "worktree", "root": "/r/e2e" }));
        // Freshest first: the loose commit, then the PR holding the older one.
        assert_eq!(repo["history"][0]["commit"]["hash"], "h2");
        assert_eq!(repo["history"][1]["pr"]["number"], 5);
        assert_eq!(repo["history"][1]["pr"]["commits"][0]["chat"], true);
        assert_eq!(repo["history"][1]["pr"]["diff"]["kind"], "pr");
        assert_eq!(model["orphans"][0]["repo"], "o/infra", "no checkout of its repo");
    }
}
