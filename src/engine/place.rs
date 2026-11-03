//! Where each of a chat's pull requests and commits is listed, across the checkouts it touched.

use super::git::Commit;
use super::github::{PrCommit, PrView};
use super::state::{PrRecord, RepoRecord, SessionState};
use super::Tracker;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Where the repos list each pull request and each of the chat's commits.
///
/// Checkouts of one repository can all list a commit (worktrees share branches); it shows
/// once. A PR shows under the checkout of its head branch, else the one it was recorded with;
/// a commit inside the PR holding it, else under a checkout whose HEAD reaches it, else the
/// first checkout listing it. A commit no HEAD reaches that repeats the subject of one of the
/// chat's commits a PR holds (the copy left on the old branch by a rebase or cherry-pick) is
/// not listed.
pub struct Placed<'a> {
    /// Listed PRs, by repo and newest first, each with the checkout it shows under (`None`:
    /// no touched checkout, a heading of its own).
    pub prs: Vec<(&'a PrRecord, Option<&'a PathBuf>)>,
    /// Checkouts shown, in the chat's order.
    pub shown: Vec<&'a PathBuf>,
    /// The chat's commits in shown checkouts, each once.
    pub chat: Vec<&'a Commit>,
    /// The chat's commits no listed PR holds, by the checkout they show under.
    loose: HashMap<&'a Path, Vec<&'a Commit>>,
}

pub fn place<'a>(tracker: &'a Tracker, saved: &'a SessionState) -> Placed<'a> {
    // Repos the chat changed or has pull requests in (`refresh_git` skips deleted ones), then
    // PRs whose repo has no touched checkout. Whatever the user dismissed stays hidden.
    // Without a checkout, PRs go newest first by number: GitHub numbers them in creation order.
    let mut prs: Vec<&PrRecord> = saved.visible_prs().collect();
    prs.sort_by_key(|p| (p.pr.slug(), std::cmp::Reverse(p.pr.number)));
    let open: Vec<&RepoRecord> =
        saved.repos.iter().filter(|r| tracker.statuses.contains_key(&r.root) && !saved.repo_dismissed(&r.root)).collect();
    let on_head_branch = |pr: &PrRecord| {
        let head = &tracker.pr_views.get(&pr.pr.url())?.head;
        let checkout = open.iter().find(|r| {
            matches!(tracker.statuses.get(&r.root), Some(Ok(s)) if s.branch == *head)
                && tracker.remotes.get(&r.root).is_some_and(|slugs| slugs.contains(&pr.pr.slug()))
        });
        checkout.map(|r| &r.root)
    };
    let prs: Vec<(&PrRecord, Option<&PathBuf>)> = prs.into_iter().map(|p| (p, on_head_branch(p).or(p.root.as_ref()))).collect();
    let shown: Vec<&PathBuf> =
        open.iter().filter(|r| r.changed || prs.iter().any(|(_, at)| *at == Some(&r.root))).map(|r| &r.root).collect();

    let listed = |root: &PathBuf, hash: &str| tracker.commits.get(root).into_iter().flatten().find(|c| c.hash == hash);
    let mut chat: Vec<&Commit> = Vec::new();
    for commit in shown.iter().flat_map(|root| tracker.commits.get(*root).into_iter().flatten()) {
        if !chat.iter().any(|c| c.hash == commit.hash) {
            chat.push(commit);
        }
    }
    let views: Vec<&PrView> = prs.iter().filter_map(|(p, _)| tracker.pr_views.get(&p.pr.url())).collect();
    let held = |c: &Commit| views.iter().any(|v| v.holds(&c.hash));
    let reached = |c: &Commit| shown.iter().any(|root| listed(root, &c.hash).is_some_and(|c| c.on_head));
    let same_repo = |a: &Commit, b: &Commit| shown.iter().any(|root| listed(root, &a.hash).is_some() && listed(root, &b.hash).is_some());
    let superseded =
        |c: &Commit| !reached(c) && chat.iter().any(|k| k.hash != c.hash && k.subject == c.subject && held(k) && same_repo(c, k));
    let mut loose: HashMap<&Path, Vec<&Commit>> = HashMap::new();
    for &commit in chat.iter().filter(|c| !held(c) && !superseded(c)) {
        let home = shown
            .iter()
            .find(|root| listed(root, &commit.hash).is_some_and(|c| c.on_head))
            .or_else(|| shown.iter().find(|root| listed(root, &commit.hash).is_some()));
        if let Some(home) = home {
            loose.entry(home.as_path()).or_default().push(commit);
        }
    }
    Placed { prs, shown, chat, loose }
}

/// What a repo's history lists: a pull request, or a commit of the chat's that no listed pull
/// request holds.
pub enum Entry<'a> {
    Pr { pr: &'a PrRecord, time: Option<i64> },
    Commit(&'a Commit),
}

impl Entry<'_> {
    /// When it last moved: a commit's time; a PR's newest held commit or GitHub activity.
    pub fn time(&self) -> Option<i64> {
        match self {
            Entry::Pr { time, .. } => *time,
            Entry::Commit(commit) => Some(commit.time),
        }
    }
}

/// The repo's pull requests and the chat's commits there in one list, freshest first. A PR
/// is as fresh as the newest of the chat's commits it holds, or its last activity on GitHub;
/// PRs with neither (not fetched yet) go last.
pub fn history<'a>(tracker: &'a Tracker, placed: &Placed<'a>, root: &Path) -> Vec<Entry<'a>> {
    let mut entries: Vec<Entry> = Vec::new();
    for (pr, _) in placed.prs.iter().filter(|(_, at)| at.is_some_and(|at| at == root)) {
        let view = tracker.pr_views.get(&pr.pr.url());
        let held = placed.chat.iter().filter(|c| view.is_some_and(|v| v.holds(&c.hash))).map(|c| c.time);
        let time = held.chain(view.and_then(|v| v.updated)).max();
        entries.push(Entry::Pr { pr, time });
    }
    entries.extend(placed.loose.get(root).into_iter().flatten().map(|c| Entry::Commit(c)));
    // Stable: among equals (PRs without a time), the newer number stays first.
    entries.sort_by_key(|e| std::cmp::Reverse(e.time()));
    entries
}

/// A commit of a pull request as listed: its subject (the checkout's, for the merge commit
/// GitHub gives none for) and whether the chat made it.
pub struct PrCommitLine<'a> {
    pub oid: &'a str,
    pub subject: &'a str,
    pub chat: bool,
}

/// A pull request's commits, its merge commit included, newest first.
pub fn pr_commits<'a>(placed: &Placed<'a>, view: &'a PrView) -> Vec<PrCommitLine<'a>> {
    let mut commits: Vec<&PrCommit> = view.merge.iter().chain(&view.commits).collect();
    commits.sort_by_key(|c| std::cmp::Reverse(c.time));
    commits
        .into_iter()
        .map(|commit| {
            let mine = placed.chat.iter().find(|c| c.hash == commit.oid);
            let subject = commit.subject.as_deref().or(mine.map(|c| c.subject.as_str())).unwrap_or("merge commit");
            PrCommitLine { oid: &commit.oid, subject, chat: mine.is_some() }
        })
        .collect()
}
