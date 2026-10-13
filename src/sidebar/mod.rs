//! The sidebar pane: follows one agent chat session in its tab and lists the git repos that
//! session changed (edit/write/commit tool calls in its omp or Claude Code transcript, or a
//! repo's git state moving after the session first touched it), with branch + changed files.
//!
//! This module holds the state, the event loop and input handling; [`collect`] gathers the
//! data and [`render`] lays it out and draws it.

mod collect;
mod render;

use crate::diff::{self, CommitAt, DiffRequest};
use crate::git::{Change, Commit, FileChange, RepoStatus};
use crate::github::{PrRef, PrView};
use crate::herdr;
use crate::state::{self, DismissedLink, DismissedRepo, SessionRepos};
use crate::transcript::Transcript;
use anyhow::{anyhow, Context, Result};
use collect::{is_dismissed, PrBatch};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::terminal;
use render::{Line, Screen};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const GIT_EVERY: Duration = Duration::from_secs(5);
const PR_EVERY: Duration = Duration::from_secs(30);
/// Links scrolled per mouse-wheel notch.
const WHEEL_LINKS: usize = 3;

/// Which chat session the sidebar follows, and whether its repo set is persisted.
struct Session {
    pane_id: String,
    title: String,
    repos: SessionRepos,
    persisted: bool,
    /// Transcript being tailed (omp, Claude Code); other agents rely on process working dirs only.
    transcript: Option<Transcript>,
    /// Web links the chat mentioned, newest first; rebuilt from the transcript on start.
    links: Vec<String>,
    /// The chat's finished tool calls, as (start, end) unix milliseconds; rebuilt from the
    /// transcript on start. Commits made within them are the chat's.
    calls: Vec<(i64, i64)>,
}

/// `git status` of each shown repo.
type Statuses = HashMap<PathBuf, Result<RepoStatus, String>>;

#[derive(Default)]
struct Model {
    session: Option<Session>,
    statuses: Statuses,
    /// Commits the chat made, per repo, newest first.
    commits: HashMap<PathBuf, Vec<Commit>>,
    /// Each pull request's commits, merge commit and last activity, by URL: the chat's commits
    /// among them are listed under the pull request, which sorts by the newest of them.
    pr_views: HashMap<String, PrView>,
    error: Option<String>,
    /// github.com `owner/repo` slugs of each checkout's remotes.
    remotes: HashMap<PathBuf, Vec<String>>,
    /// Last `gh` failure per PR URL.
    pr_errors: HashMap<String, String>,
    /// In-flight PR status refresh (runs `gh` off the UI thread).
    pr_job: Option<Receiver<PrBatch>>,
    /// The diff pane currently shown; the next diff replaces it.
    diff_pane: Option<String>,
    /// Row chosen with the arrow keys or a click; its diff is shown.
    selected: Option<Key>,
    /// Pull requests (by URL) expanded to list their commits, and commits (by hash) expanded
    /// to list their files.
    expanded: HashSet<String>,
    /// Files of expanded commits, by hash; absent while loading.
    commit_files: HashMap<String, Result<Vec<FileChange>, String>>,
    /// In-flight file list fetches (run `git` or `gh` off the UI thread).
    files_jobs: Vec<Receiver<FilesFetched>>,
    /// This sidebar's own pane id; names the request file its diff viewer follows.
    me: String,
    /// First rendered row on screen when the list is taller than the pane.
    scroll: usize,
    /// Right-click menu, while open.
    menu: Option<Menu>,
    /// The link or pull request under the mouse pointer, drawn underlined and bold so it reads
    /// as clickable.
    hover: Option<String>,
    /// How many of the newest links the links block is scrolled past.
    links_scroll: usize,
    /// Whether the selection was made with the keys: only then is it drawn highlighted (and
    /// kept in view). A clicked row needs no marking.
    keyboard: bool,
}

impl Model {
    /// Shows a failure on the sidebar's first row; the loop keeps running.
    fn note(&mut self, result: Result<()>) {
        if let Err(e) = result {
            self.error = Some(format!("{e:#}"));
        }
    }
}

/// A commit's file list, fetched in the background, by hash.
type FilesFetched = (String, Result<Vec<FileChange>, String>);

/// What a sidebar row is: selecting or clicking it shows its diff.
#[derive(Debug, Clone, PartialEq)]
enum Click {
    /// A changed file in a local checkout.
    File { root: PathBuf, change: Change },
    /// A pull request; expands to its commits.
    Pr { url: String },
    /// A file changed by commits not yet pushed.
    Unpushed { root: PathBuf, path: String },
    /// A commit, listed on its own or (`under`) in an expanded pull request; expands to its
    /// files.
    Commit { commit: CommitAt, under: Option<String> },
    /// One file of an expanded commit.
    CommitFile { commit: CommitAt, path: String },
}

/// Identity of a selectable row that survives refreshes (a file's status code may change).
#[derive(Debug, Clone, PartialEq)]
enum Key {
    File(PathBuf, String),
    Pr(String),
    Unpushed(PathBuf, String),
    Commit(String),
    CommitFile(String, String),
}

impl Click {
    fn key(&self) -> Key {
        match self {
            Click::File { root, change } => Key::File(root.clone(), change.path.clone()),
            Click::Pr { url } => Key::Pr(url.clone()),
            Click::Unpushed { root, path } => Key::Unpushed(root.clone(), path.clone()),
            Click::Commit { commit, .. } => Key::Commit(commit.hash.clone()),
            Click::CommitFile { commit, path } => Key::CommitFile(commit.hash.clone(), path.clone()),
        }
    }

    fn diff_request(&self) -> DiffRequest {
        match self {
            Click::File { root, change } => DiffRequest::local(root, change),
            Click::Pr { url } => DiffRequest::Pr { url: url.clone() },
            Click::Unpushed { root, path } => DiffRequest::Unpushed { root: root.clone(), path: path.clone() },
            Click::Commit { commit, .. } => DiffRequest::Commit { commit: commit.clone(), path: None },
            Click::CommitFile { commit, path } => DiffRequest::Commit { commit: commit.clone(), path: Some(path.clone()) },
        }
    }

    /// The page it opens in the browser: a pull request's, or a commit's on GitHub.
    fn url(&self) -> Option<String> {
        match self {
            Click::Pr { url } => Some(url.clone()),
            Click::Commit { commit, .. } | Click::CommitFile { commit, .. } => commit.url(),
            _ => None,
        }
    }
}

/// What a right-click menu acts on: a repo header, a pull request row, a link, or the links
/// block's rule.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Repo(PathBuf),
    Pr(PrRef),
    Link(String),
    Links,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MenuItem {
    /// Open the pull request or link in the browser.
    Open,
    Dismiss,
    DismissAllLinks,
}

/// An open right-click menu, anchored below (or above) the on-screen row it was opened on.
struct Menu {
    target: Target,
    row: usize,
}

impl Menu {
    /// Items with the on-screen rows they occupy in a `rows`-high pane.
    fn placed(&self, rows: usize) -> Vec<(usize, MenuItem, &'static str)> {
        let items: &[(MenuItem, &str)] = match self.target {
            Target::Repo(_) => &[(MenuItem::Dismiss, "dismiss repo")],
            Target::Pr(_) => &[(MenuItem::Open, "open on GitHub"), (MenuItem::Dismiss, "dismiss PR")],
            Target::Link(_) => &[
                (MenuItem::Open, "open in browser"),
                (MenuItem::Dismiss, "dismiss link"),
                (MenuItem::DismissAllLinks, "dismiss all links"),
            ],
            Target::Links => &[(MenuItem::DismissAllLinks, "dismiss all links")],
        };
        let start = if self.row + 1 + items.len() <= rows { self.row + 1 } else { self.row.saturating_sub(items.len()) };
        items.iter().enumerate().map(|(i, (item, label))| (start + i, *item, *label)).collect()
    }
}

pub fn run() -> Result<()> {
    let me = std::env::var("HERDR_PANE_ID").map_err(|_| anyhow!("HERDR_PANE_ID not set"))?;
    // herdr shows its own pane menu on right-click unless the pane asks for the clicks.
    herdr::forward_right_click(&me)?;
    let _term = crate::term::Term::enter()?;
    event_loop(&me)
}

/// When each kind of data is next refreshed.
struct Due {
    sample: Instant,
    git: Instant,
    pr: Instant,
}

/// Runs for the pane's lifetime. There is no quit key: the sidebar is mandatory.
fn event_loop(me: &str) -> Result<()> {
    let mut model = Model { me: me.to_string(), ..Model::default() };
    let mut shown: (Vec<Line>, Option<usize>, Option<String>) = (Vec::new(), None, None);
    let now = Instant::now();
    let mut due = Due { sample: now, git: now, pr: now };
    loop {
        refresh(&mut model, &mut due);
        let (cols, rows) = terminal::size()?;
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        let screen = render::render(&model, cols);
        let selected_row = screen.row_of(model.selected.as_ref()).filter(|_| model.keyboard);
        // The links block takes at most half the pane; the list scrolls above it.
        let mut links_scroll = model.links_scroll;
        let block = render::links(&model, cols, rows / 2, &mut links_scroll);
        model.links_scroll = links_scroll;
        let mut view = render::window(&screen, cols, rows - block.len(), &mut model.scroll, selected_row);
        view.pin(block, rows);
        if let Some(menu) = &model.menu {
            view.overlay(menu, cols, rows);
        }
        if (&view.lines, view.highlight, &model.hover) != (&shown.0, shown.1, &shown.2) {
            render::draw(&view, cols, model.hover.as_deref())?;
            shown = (view.lines.clone(), view.highlight, model.hover.clone());
        }
        if event::poll(due.sample.min(due.git).saturating_duration_since(Instant::now()))? {
            let event = event::read()?;
            if matches!(event, Event::Resize(..)) {
                shown = (Vec::new(), None, None);
            }
            let result = handle_event(&mut model, &screen, &view, rows, event);
            model.note(result);
        }
    }
}

/// Refreshes whatever is due: the followed chat (every second), git status (every 5 s, or
/// at once when new repos appear), pull-request status (every 30 s) and fetched file lists.
fn refresh(model: &mut Model, due: &mut Due) {
    let now = Instant::now();
    if now >= due.sample {
        due.sample = now + SAMPLE_EVERY;
        // A diff pane closed from its own side (`q`, or herdr) leaves nothing selected.
        if model.diff_pane.as_deref().is_some_and(|pane| !herdr::pane_exists(pane)) {
            model.diff_pane = None;
            model.selected = None;
        }
        let me = model.me.clone();
        match collect::sample(&me, model) {
            Ok(new_repos) => {
                model.error = None;
                if new_repos {
                    due.git = now;
                }
            }
            Err(e) => model.note(Err(e)),
        }
    }
    if now >= due.git {
        due.git = now + GIT_EVERY;
        (model.statuses, model.commits) = collect::refresh_git(model);
        let result = collect::track_changes(model);
        model.note(result);
    }
    if let Some(job) = &model.pr_job {
        match job.try_recv() {
            Ok(batch) => {
                model.pr_job = None;
                let result = collect::apply_prs(model, batch);
                model.note(result);
            }
            Err(TryRecvError::Disconnected) => model.pr_job = None,
            Err(TryRecvError::Empty) => {}
        }
    }
    if model.pr_job.is_none() && (now >= due.pr || collect::has_unfetched_pr(model)) {
        due.pr = now + PR_EVERY;
        model.pr_job = collect::start_pr_refresh(model);
    }
    let Model { files_jobs, commit_files, .. } = model;
    files_jobs.retain(|job| match job.try_recv() {
        Ok((hash, files)) => {
            commit_files.insert(hash, files);
            false
        }
        Err(TryRecvError::Empty) => true,
        Err(TryRecvError::Disconnected) => false,
    });
}

/// One key, click or resize. An open right-click menu takes the next click or key: an item
/// runs, anything else closes it; a right-click elsewhere opens a menu there instead.
fn handle_event(model: &mut Model, screen: &Screen, view: &render::View, rows: usize, event: Event) -> Result<()> {
    match (model.menu.take(), event) {
        (Some(menu), Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, .. })) => {
            match menu.placed(rows).into_iter().find(|(r, ..)| *r == usize::from(row)) {
                Some((_, item, _)) => run_menu(model, &menu.target, item),
                None => Ok(()),
            }
        }
        (Some(_), Event::Key(_) | Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Middle), .. })) => {
            Ok(())
        }
        (_, Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), row, .. })) => {
            let row = usize::from(row);
            model.menu = view.menus.get(&row).map(|target| Menu { target: target.clone(), row });
            Ok(())
        }
        (None, Event::Key(KeyEvent { code, .. })) => {
            model.keyboard = true;
            handle_key(model, screen, code)
        }
        (None, Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, column, modifiers })) => {
            let row = usize::from(row);
            if let Some(url) = view.links.get(&row) {
                return open_url(url);
            }
            let zone = view.lines.get(row).map_or(render::Zone::Body, |line| render::zone(line, usize::from(column)));
            match view.clicks.get(&row) {
                Some(click) => {
                    model.keyboard = false;
                    handle_click(model, click, modifiers.contains(KeyModifiers::CONTROL), zone)
                }
                None => Ok(()),
            }
        }
        (menu, Event::Resize(..)) => {
            model.menu = menu;
            crate::dock::hold_width(&model.me)
        }
        // The wheel over the links block scrolls it (and closes a menu anchored to its rows).
        (_, Event::Mouse(MouseEvent { kind: kind @ (MouseEventKind::ScrollDown | MouseEventKind::ScrollUp), row, .. }))
            if view.links_top.is_some_and(|top| usize::from(row) >= top) =>
        {
            model.links_scroll = match kind {
                MouseEventKind::ScrollDown => model.links_scroll + WHEEL_LINKS,
                _ => model.links_scroll.saturating_sub(WHEEL_LINKS),
            };
            model.hover = None;
            Ok(())
        }
        (menu, Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, row, column, .. })) => {
            model.menu = menu;
            model.hover = view.pointer_url(usize::from(row), usize::from(column)).map(str::to_string);
            Ok(())
        }
        // Mouse releases and drags, focus changes: an open menu stays open.
        (menu, _) => {
            model.menu = menu;
            Ok(())
        }
    }
}

/// ↑/↓ (`k`/`j`) select rows, →/← (`l`/`h`) expand and collapse pull requests and commits,
/// `o` opens the selected pull request or commit in the browser, Enter focuses the diff, Esc
/// closes it.
fn handle_key(model: &mut Model, screen: &Screen, code: KeyCode) -> Result<()> {
    let selected = model.selected.as_ref().and_then(|key| screen.rows().into_iter().find(|(_, c)| c.key() == *key)).map(|(_, c)| c.clone());
    match code {
        KeyCode::Down | KeyCode::Char('j') => match move_selection(model, screen, 1) {
            Some(request) => show_diff(model, &request),
            None => Ok(()),
        },
        KeyCode::Up | KeyCode::Char('k') => match move_selection(model, screen, -1) {
            Some(request) => show_diff(model, &request),
            None => Ok(()),
        },
        KeyCode::Right | KeyCode::Char('l') => {
            match selected {
                Some(Click::Pr { url }) => {
                    model.expanded.insert(url);
                }
                Some(Click::Commit { commit, .. }) => expand_commit(model, &commit),
                _ => {}
            }
            Ok(())
        }
        // Collapses the selected row, or else the one it is listed in, and selects that.
        KeyCode::Left | KeyCode::Char('h') => match selected {
            Some(Click::Pr { url }) => {
                model.expanded.remove(&url);
                Ok(())
            }
            Some(Click::Commit { commit, under }) => {
                if model.expanded.remove(&commit.hash) {
                    return Ok(());
                }
                let Some(url) = under else { return Ok(()) };
                model.expanded.remove(&url);
                handle_select(model, &Click::Pr { url })
            }
            Some(Click::CommitFile { commit, .. }) => {
                model.expanded.remove(&commit.hash);
                let under = screen.rows().into_iter().find_map(|(_, c)| match c {
                    Click::Commit { commit: c, under } if c.hash == commit.hash => Some(under.clone()),
                    _ => None,
                });
                handle_select(model, &Click::Commit { commit, under: under.flatten() })
            }
            _ => Ok(()),
        },
        KeyCode::Char('C') => dismiss(model, &Target::Links),
        KeyCode::Char('o') => match selected.and_then(|c| c.url()) {
            Some(url) => open_url(&url),
            None => Ok(()),
        },
        KeyCode::Enter => match &model.diff_pane {
            Some(pane) => herdr::pane_focus(pane),
            None => Ok(()),
        },
        KeyCode::Esc => {
            model.selected = None;
            match model.diff_pane.take() {
                Some(pane) => herdr::pane_close(&pane),
                None => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// Moves the selection `step` rows down (or up), clamped to the list. Returns the newly
/// selected row's diff, or `None` when the selection did not move.
fn move_selection(model: &mut Model, screen: &Screen, step: isize) -> Option<DiffRequest> {
    let rows = screen.rows();
    let last = rows.len().checked_sub(1)?;
    let current = model.selected.as_ref().and_then(|key| rows.iter().position(|(_, click)| click.key() == *key));
    let next = match current {
        Some(i) => i.saturating_add_signed(step).min(last),
        None if step > 0 => 0,
        None => last,
    };
    if Some(next) == current {
        return None;
    }
    let click = rows[next].1;
    model.selected = Some(click.key());
    Some(click.diff_request())
}

/// Lists a commit's files under it, fetching them in the background the first time.
fn expand_commit(model: &mut Model, commit: &CommitAt) {
    model.expanded.insert(commit.hash.clone());
    if model.commit_files.contains_key(&commit.hash) {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let commit = commit.clone();
    std::thread::spawn(move || {
        let files = commit.files();
        let _ = tx.send((commit.hash, files));
    });
    model.files_jobs.push(rx);
}

fn run_menu(model: &mut Model, target: &Target, item: MenuItem) -> Result<()> {
    match (item, target) {
        (MenuItem::Open, Target::Pr(pr)) => open_url(&pr.url()),
        (MenuItem::Open, Target::Link(url)) => open_url(url),
        (MenuItem::Open, Target::Repo(_) | Target::Links) => Ok(()),
        (MenuItem::Dismiss, target) => dismiss(model, target),
        (MenuItem::DismissAllLinks, _) => dismiss(model, &Target::Links),
    }
}

/// Hides a pull request from this chat's sidebar for good, a repo (with its pull requests)
/// until the chat touches it again, or a link (or all of them) until the chat mentions it again.
fn dismiss(model: &mut Model, target: &Target) -> Result<()> {
    let Some(session) = model.session.as_mut() else { return Ok(()) };
    let at = session.transcript.as_ref().map_or(0, Transcript::position);
    let repos = &mut session.repos;
    match target {
        Target::Repo(root) if !is_dismissed(repos, root) => repos.dismissed_repos.push(DismissedRepo { root: root.clone(), at }),
        Target::Pr(pr) if !repos.dismissed_prs.iter().any(|d| d.same(pr)) => repos.dismissed_prs.push(pr.clone()),
        Target::Link(url) if !repos.dismissed_links.iter().any(|d| d.url == *url) => {
            repos.dismissed_links.push(DismissedLink { url: url.clone(), at })
        }
        Target::Links => {
            let fresh: Vec<DismissedLink> = session
                .links
                .iter()
                .filter(|url| !repos.dismissed_links.iter().any(|d| d.url == **url))
                .map(|url| DismissedLink { url: url.clone(), at })
                .collect();
            if fresh.is_empty() {
                return Ok(());
            }
            repos.dismissed_links.extend(fresh);
        }
        _ => return Ok(()),
    }
    if session.persisted {
        state::save_session(&session.repos)?;
    }
    Ok(())
}

/// Opens the URL in the desktop's browser without waiting for it.
fn open_url(url: &str) -> Result<()> {
    let mut opener = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("xdg-open")?;
    // Collect it when it exits, or every click would leave a zombie process behind.
    std::thread::spawn(move || opener.wait());
    Ok(())
}

/// A click on a row: its expand marker expands or collapses a pull request or commit; a pull
/// request's number and status open it in the browser; anywhere else selects the row and
/// shows its diff. Ctrl+click opens a pull request or commit in the browser.
fn handle_click(model: &mut Model, click: &Click, ctrl: bool, zone: render::Zone) -> Result<()> {
    if let (Some(url), true) = (click.url(), ctrl) {
        return open_url(&url);
    }
    match (click, zone) {
        (Click::Pr { url }, render::Zone::Marker) => {
            if !model.expanded.remove(url) {
                model.expanded.insert(url.clone());
            }
            Ok(())
        }
        (Click::Pr { url }, render::Zone::Link) => open_url(url),
        (Click::Commit { commit, .. }, render::Zone::Marker) => {
            if !model.expanded.remove(&commit.hash) {
                expand_commit(model, commit);
            }
            Ok(())
        }
        _ => handle_select(model, click),
    }
}

/// Selects the row and shows its diff.
fn handle_select(model: &mut Model, click: &Click) -> Result<()> {
    model.selected = Some(click.key());
    show_diff(model, &click.diff_request())
}

/// Shows a diff without taking focus from the sidebar: points the running diff viewer at it
/// (it redraws in place), or opens the viewer next to the agent.
fn show_diff(model: &mut Model, request: &DiffRequest) -> Result<()> {
    let file = diff::request_file(&model.me);
    diff::write_request(&file, request)?;
    if model.diff_pane.as_deref().is_some_and(herdr::pane_exists) {
        return Ok(());
    }
    let agent = model.session.as_ref().map(|s| s.pane_id.clone()).ok_or_else(|| anyhow!("no agent pane"))?;
    let env = [(diff::ENV, file.display().to_string()), (diff::OWNER_ENV, model.me.clone())];
    model.diff_pane =
        Some(herdr::open_plugin_pane(&crate::dock::plugin_id(), crate::DIFF_ENTRYPOINT, &agent, false, &env)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::LineStat;
    use crate::github::{PrCommit, PrState, PrStatus};
    use crate::state::{PrRecord, RepoRecord};
    use render::{render, repo_header, window};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn unpushed_work_stays_visible() {
        let root = PathBuf::from("/r/e2e");
        let status = RepoStatus {
            branch: "nightly-maintenance-2026-09-29".into(),
            ahead: 1,
            behind: 2,
            changes: vec![Change { code: *b" M", path: "a.py".into(), orig: None, stat: None }],
            unpushed: vec![("e2e/b.py".into(), LineStat::Lines { added: 4, removed: 1 })],
            fingerprint: 0,
        };
        // A long branch name gives way; the ahead/behind counts do not.
        let header: String = repo_header(&root, Some(&Ok(status.clone())), 30).into_iter().map(|(t, _)| t).collect();
        assert_eq!(header, "e2e nightly-maintenance… ↑1 ↓2");
        assert_eq!(header.width(), 30);
        // Unpushed files are rows of their own, after the uncommitted ones.
        let mut model = Model::default();
        model.statuses.insert(root.clone(), Ok(status));
        model.session = Some(Session {
            pane_id: "w:p1".into(),
            title: String::new(),
            repos: SessionRepos {
                session: "s".into(),
                repos: vec![RepoRecord { root: root.clone(), changed: true, baseline: None }],
                prs: Vec::new(),
                ..SessionRepos::default()
            },
            persisted: false,
            transcript: None,
            calls: Vec::new(),
            links: Vec::new(),
        });
        let screen = render(&model, 40);
        let rows: Vec<Key> = screen.rows().into_iter().map(|(_, c)| c.key()).collect();
        assert_eq!(rows, [Key::File(root.clone(), "a.py".into()), Key::Unpushed(root.clone(), "e2e/b.py".into())]);
        let unpushed = screen.rows()[1].1.diff_request();
        assert_eq!(unpushed, DiffRequest::Unpushed { root: root.clone(), path: "e2e/b.py".into() });

        // The chat's commits follow, newest first, ten at most; one more row counts the rest.
        let commit = |n: i64| Commit { hash: format!("h{n}"), time: n, subject: format!("commit {n}") };
        model.commits.insert(root.clone(), (0..12).rev().map(commit).collect());
        let screen = render(&model, 40);
        let rows: Vec<Key> = screen.rows().into_iter().skip(2).map(|(_, c)| c.key()).collect();
        assert_eq!(rows, (2..12).rev().map(|n| Key::Commit(format!("h{n}"))).collect::<Vec<_>>());
        let text = |line: &render::Line| line.iter().map(|(t, _)| t.as_str()).collect::<String>();
        let last = screen.lines.len() - 1;
        assert_eq!(text(&screen.lines[last - 1]), " ▸ h2 commit 2");
        assert_eq!(text(&screen.lines[last]), "   … 2 earlier commits");
        let at = |hash: &str, repo: Option<&str>| CommitAt { hash: hash.into(), root: Some(root.clone()), repo: repo.map(String::from) };
        assert_eq!(screen.rows()[2].1.diff_request(), DiffRequest::Commit { commit: at("h11", None), path: None });

        // Pull requests join the commits in one list, freshest first: a PR by the newest of the
        // chat's commits it holds (or its last activity on GitHub). A PR not fetched yet goes last.
        let pr = |number| PrRef { owner: "o".into(), repo: "e2e".into(), number };
        let status = PrStatus { state: PrState::Open, checks: None };
        let gh = |oid: &str, subject: &str, time| PrCommit { oid: oid.into(), subject: Some(subject.into()), time: Some(time) };
        let commits = vec![gh("h3", "gh 3", 3), gh("h7", "gh 7", 7), gh("x1", "someone else's", 8)];
        let title = "lodge every beneficial owner as owner too".to_string();
        model.pr_views.insert(pr(5).url(), PrView { status, title, commits, merge: None, updated: Some(1) });
        let repos = &mut model.session.as_mut().unwrap().repos;
        for number in [5, 6] {
            let status = (number == 5).then_some(status);
            repos.prs.push(PrRecord { pr: pr(number), root: Some(root.clone()), status });
        }
        let keys = |model: &Model| render(model, 40).rows().into_iter().map(|(_, c)| c.key()).collect::<Vec<_>>();
        let commit_key = |n: i64| Key::Commit(format!("h{n}"));
        let mut expected = vec![Key::File(root.clone(), "a.py".into()), Key::Unpushed(root.clone(), "e2e/b.py".into())];
        expected.extend([11, 10, 9, 8].map(commit_key));
        expected.push(Key::Pr(pr(5).url()));
        expected.extend([6, 5, 4, 2, 1, 0].map(commit_key));
        expected.push(Key::Pr(pr(6).url()));
        assert_eq!(keys(&model), expected);
        // The title follows the state, cut to fit.
        let screen = render(&model, 40);
        let pr_row = screen.rows().iter().find(|(_, c)| c.key() == Key::Pr(pr(5).url())).unwrap().0;
        let row: String = screen.lines[pr_row].iter().map(|(t, _)| crate::ansi::fit(t, 99)).collect();
        assert_eq!(crate::ansi::width(&row), 40);
        assert!(row.ends_with("#5\x1b]8;;\x1b\\ open lodge every beneficial owner…"), "{row:?}");
        // ` ▸ #5 open lodge…`: the marker expands, number and status open the browser, the title
        // shows the diff.
        let zone = |col| render::zone(&screen.lines[pr_row], col);
        assert_eq!([1, 3, 9, 12].map(zone), [render::Zone::Marker, render::Zone::Link, render::Zone::Link, render::Zone::Body]);

        // Expanded, the PR lists all its commits, newest first: the chat's own marked green,
        // others dim; and an expanded commit lists its files, each with its diff in the commit.
        model.expanded.insert(pr(5).url());
        model.expanded.insert("h7".into());
        let stat = LineStat::Lines { added: 1, removed: 0 };
        model.commit_files.insert("h7".into(), Ok(vec![FileChange { code: 'M', path: "a.py".into(), stat }]));
        let screen = render(&model, 40);
        let pr_at = screen.rows().iter().position(|(_, c)| c.key() == Key::Pr(pr(5).url())).unwrap();
        let under: Vec<(usize, &Click)> = screen.rows()[pr_at + 1..pr_at + 5].to_vec();
        let shown: Vec<String> = under.iter().map(|(row, _)| text(&screen.lines[*row])).collect();
        assert_eq!(shown[..3], ["   ▸ x1 someone else's", "   ▾ h7 gh 7", "      M a.py                          +1"]);
        assert_eq!(screen.lines[under[0].0][1].1, render::Tone::Dim, "not the chat's commit");
        assert_eq!(screen.lines[under[1].0][1].1, render::Tone::Good, "the chat's commit");
        assert_eq!(under[1].1, &Click::Commit { commit: at("h7", Some("o/e2e")), under: Some(pr(5).url()) });
        assert_eq!(under[2].1.diff_request(), DiffRequest::Commit { commit: at("h7", Some("o/e2e")), path: Some("a.py".into()) });
        assert_eq!(under[3].1.key(), commit_key(3));
        // Only the expand markers toggle: a PR's, a commit's under it; file rows have none.
        let screen_row = |i: usize| &screen.lines[i];
        assert_eq!(render::marker_column(screen_row(screen.rows()[pr_at].0)), Some(1));
        assert_eq!(render::marker_column(screen_row(under[1].0)), Some(3));
        assert_eq!(render::marker_column(screen_row(under[2].0)), None);
    }

    #[test]
    fn calls_merge_into_windows_and_running_ones_end_now() {
        let session = Session {
            pane_id: "w:p1".into(),
            title: String::new(),
            repos: SessionRepos::default(),
            persisted: false,
            transcript: None,
            calls: vec![(50, 60), (10, 20), (15, 30)],
            links: Vec::new(),
        };
        assert_eq!(collect::call_windows(&session, 100), [(10, 30), (50, 60)]);
    }

    #[test]
    fn links_are_pinned_to_the_bottom_newest_first_without_listed_prs() {
        let pr = PrRef { owner: "o".into(), repo: "r".into(), number: 7 };
        let session = Session {
            pane_id: "w:p1".into(),
            title: "chat".into(),
            repos: SessionRepos {
                session: "s".into(),
                prs: vec![PrRecord { pr: pr.clone(), root: None, status: None }],
                ..SessionRepos::default()
            },
            persisted: false,
            transcript: None,
            calls: Vec::new(),
            links: vec![
                "https://www.new.io/a".into(),
                format!("{}/files", pr.url()),
                "https://mid.io/b".into(),
                "https://old.io/c".into(),
            ],
        };
        let mut model = Model { session: Some(session), ..Model::default() };
        // Room for the rule and two links: the newest two that are not PR rows already.
        let block = render::links(&model, 30, 3, &mut 0);
        let urls: Vec<_> = block.iter().filter_map(|(_, url)| url.as_deref()).collect();
        assert_eq!(urls, ["https://www.new.io/a", "https://mid.io/b"]);

        // Pinned to the last rows of a 10-row pane; a click there resolves to the link.
        let screen = render(&model, 30);
        let mut view = window(&screen, 30, 10 - block.len(), &mut 0, None);
        view.pin(block, 10);
        assert_eq!(view.lines.len(), 10);
        assert_eq!(view.links.get(&8).map(String::as_str), Some("https://www.new.io/a"));
        assert_eq!(view.links.get(&9).map(String::as_str), Some("https://mid.io/b"));
        let shown: String = view.lines[8].iter().map(|(t, _)| crate::ansi::fit(t, 99)).collect();
        assert_eq!(crate::ansi::width(&shown), " new.io/a".len(), "scheme and www. are not shown");
        assert!(render::links(&model, 30, 1, &mut 0).is_empty(), "no room for a link: no block");

        // A link row right-clicks to its own menu; dismissing it lets the next one move up.
        assert_eq!(view.menus.get(&8), Some(&Target::Link("https://www.new.io/a".into())));
        let menu = Menu { target: Target::Link("https://www.new.io/a".into()), row: 8 };
        let items: Vec<_> = menu.placed(10).into_iter().map(|(_, item, label)| (item, label)).collect();
        assert_eq!(
            items,
            [(MenuItem::Open, "open in browser"), (MenuItem::Dismiss, "dismiss link"), (MenuItem::DismissAllLinks, "dismiss all links")]
        );
        run_menu(&mut model, &menu.target, MenuItem::Dismiss).unwrap();
        let urls: Vec<_> = render::links(&model, 30, 3, &mut 0).into_iter().filter_map(|(_, url)| url).collect();
        assert_eq!(urls, ["https://mid.io/b", "https://old.io/c"]);

        // Moving the pointer onto a link row hovers that link; anywhere else clears it, and an
        // open menu stays open meanwhile.
        let moved = |row| Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, column: 3, row, modifiers: KeyModifiers::NONE });
        handle_event(&mut model, &screen, &view, 10, moved(9)).unwrap();
        assert_eq!(model.hover.as_deref(), Some("https://mid.io/b"));
        model.menu = Some(Menu { target: Target::Link("https://mid.io/b".into()), row: 9 });
        handle_event(&mut model, &screen, &view, 10, moved(0)).unwrap();
        assert_eq!(model.hover, None);
        assert!(model.menu.is_some());

        // The wheel scrolls the links block, not the list above it; drawing clamps the offset
        // and the rule counts what is shown.
        let wheel = |kind, row| Event::Mouse(MouseEvent { kind, column: 3, row, modifiers: KeyModifiers::NONE });
        handle_event(&mut model, &screen, &view, 10, wheel(MouseEventKind::ScrollDown, 2)).unwrap();
        assert_eq!(model.links_scroll, 0, "row 2 is above the links block");
        handle_event(&mut model, &screen, &view, 10, wheel(MouseEventKind::ScrollDown, 9)).unwrap();
        assert_eq!(model.links_scroll, WHEEL_LINKS);
        let mut scroll = model.links_scroll;
        let block = render::links(&model, 30, 2, &mut scroll);
        assert_eq!(scroll, 1, "two links, room for one: at most one to scroll past");
        let rule: String = block[0].0.iter().map(|(t, _)| t.as_str()).collect();
        assert!(rule.starts_with("─ links 2 of 2 ─"), "{rule}");
        assert_eq!(block[1].1.as_deref(), Some("https://old.io/c"));
        handle_event(&mut model, &screen, &view, 10, wheel(MouseEventKind::ScrollUp, 9)).unwrap();
        assert_eq!(model.links_scroll, 0);

        // `C`, or the rule's right-click menu, dismisses all links.
        assert_eq!(view.menus.get(&7), Some(&Target::Links));
        let rule_menu = Menu { target: Target::Links, row: 7 };
        let items: Vec<_> = rule_menu.placed(10).into_iter().map(|(_, item, label)| (item, label)).collect();
        assert_eq!(items, [(MenuItem::DismissAllLinks, "dismiss all links")]);
        handle_event(&mut model, &screen, &view, 10, Event::Key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT))).unwrap();
        assert!(render::links(&model, 30, 3, &mut 0).is_empty());
    }

    #[test]
    fn pull_requests_list_newest_first_whatever_order_the_chat_named_them() {
        let root = PathBuf::from("/r/kil");
        let pr = |number| PrRecord { pr: PrRef { owner: "o".into(), repo: "kil".into(), number }, root: Some(root.clone()), status: None };
        let mut model = Model::default();
        model.statuses.insert(
            root.clone(),
            Ok(RepoStatus { branch: "main".into(), ahead: 0, behind: 0, changes: Vec::new(), unpushed: Vec::new(), fingerprint: 0 }),
        );
        model.session = Some(Session {
            pane_id: "w:p1".into(),
            title: String::new(),
            repos: SessionRepos {
                session: "s".into(),
                repos: vec![RepoRecord { root: root.clone(), changed: true, baseline: None }],
                prs: vec![pr(840), pr(843), pr(9), pr(841)],
                ..SessionRepos::default()
            },
            persisted: false,
            transcript: None,
            calls: Vec::new(),
            links: Vec::new(),
        });
        let screen = render(&model, 40);
        let urls: Vec<&str> = screen
            .rows()
            .into_iter()
            .filter_map(|(_, click)| match click {
                Click::Pr { url } => url.rsplit('/').next(),
                _ => None,
            })
            .collect();
        assert_eq!(urls, ["843", "841", "840", "9"]);
    }

    #[test]
    fn right_click_targets_and_dismissals() {
        let (mine, reviewed) = (PathBuf::from("/r/mine"), PathBuf::from("/r/e2e"));
        let pr = |repo: &str, number| PrRef { owner: "o".into(), repo: repo.into(), number };
        let clean = || Ok(RepoStatus {
            branch: "main".into(),
            ahead: 0,
            behind: 0,
            changes: Vec::new(),
            unpushed: Vec::new(),
            fingerprint: 0,
        });
        let mut model = Model::default();
        model.statuses.insert(mine.clone(), clean());
        model.statuses.insert(reviewed.clone(), clean());
        model.session = Some(Session {
            pane_id: "w:p1".into(),
            title: String::new(),
            repos: SessionRepos {
                session: "s".into(),
                repos: vec![
                    RepoRecord { root: mine.clone(), changed: true, baseline: None },
                    RepoRecord { root: reviewed.clone(), changed: false, baseline: None },
                ],
                prs: vec![
                    PrRecord { pr: pr("e2e", 36), root: Some(reviewed.clone()), status: None },
                    PrRecord { pr: pr("infra", 133), root: None, status: None },
                ],
                ..SessionRepos::default()
            },
            persisted: false,
            transcript: None,
            calls: Vec::new(),
            links: Vec::new(),
        });
        // Rows: 0 mine, 1 blank, 2 e2e, 3 #36, 4 blank, 5 o/infra, 6 #133.
        let screen = render(&model, 40);
        assert_eq!(screen.menus.get(&0), Some(&Target::Repo(mine.clone())));
        assert_eq!(screen.menus.get(&2), Some(&Target::Repo(reviewed.clone())));
        assert_eq!(screen.menus.get(&3), Some(&Target::Pr(pr("e2e", 36))));
        assert_eq!(screen.menus.get(&5), Some(&Target::Pr(pr("infra", 133))), "a PR's own heading");
        assert_eq!(screen.menus.get(&6), Some(&Target::Pr(pr("infra", 133))));

        // The menu opens below the clicked row, or above it at the bottom of the pane.
        let below = Menu { target: Target::Pr(pr("infra", 133)), row: 3 };
        assert_eq!(below.placed(10).iter().map(|(r, i, _)| (*r, *i)).collect::<Vec<_>>(), [(4, MenuItem::Open), (5, MenuItem::Dismiss)]);
        let above = Menu { target: Target::Pr(pr("infra", 133)), row: 9 };
        assert_eq!(above.placed(10).iter().map(|(r, ..)| *r).collect::<Vec<_>>(), [7, 8]);

        // Dismissing #36 also hides its repo, which was only shown for that PR; dismissing a
        // repo hides it even though the chat changed it.
        run_menu(&mut model, &Target::Pr(pr("E2E", 36)), MenuItem::Dismiss).unwrap();
        run_menu(&mut model, &Target::Repo(mine.clone()), MenuItem::Dismiss).unwrap();
        let text: String = render(&model, 40).lines.iter().flatten().map(|(t, _)| format!("{t}\n")).collect();
        assert!(!text.contains("mine") && !text.contains("e2e") && !text.contains("#36"), "{text}");
        assert!(text.contains("o/infra") && text.contains("#133"), "{text}");
    }

    #[test]
    fn rows_clicks_selection_and_scrolling_stay_aligned() {
        let root = PathBuf::from("/r/app");
        let change = |code: &[u8; 2], path: &str| Change { code: *code, path: path.into(), orig: None, stat: None };
        let pr = PrRef { owner: "o".into(), repo: "app".into(), number: 7 };
        let mut model = Model::default();
        model.statuses.insert(
            root.clone(),
            Ok(RepoStatus {
                branch: "main".into(),
                ahead: 0,
                behind: 0,
                changes: vec![change(b" M", "a.rs"), change(b"??", "b.rs")],
                unpushed: Vec::new(),
                fingerprint: 0,
            }),
        );
        model.session = Some(Session {
            pane_id: "w:p1".into(),
            title: "chat".into(),
            repos: SessionRepos {
                session: "s".into(),
                repos: vec![RepoRecord { root: root.clone(), changed: true, baseline: None }],
                prs: vec![PrRecord { pr: pr.clone(), root: Some(root.clone()), status: None }],
                ..SessionRepos::default()
            },
            persisted: false,
            transcript: None,
            calls: Vec::new(),
            links: Vec::new(),
        });
        // Rows: 0 title, 1 blank, 2 repo header, 3 a.rs, 4 b.rs, 5 PR.
        let screen = render(&model, 40);
        assert_eq!(screen.clicks.get(&3), Some(&Click::File { root: root.clone(), change: change(b" M", "a.rs") }));
        assert_eq!(screen.clicks.get(&4), Some(&Click::File { root: root.clone(), change: change(b"??", "b.rs") }));
        assert_eq!(screen.clicks.get(&5), Some(&Click::Pr { url: pr.url() }));
        assert_eq!(screen.clicks.len(), 3);

        // Five rows, nothing selected: four content rows plus "… more"; cut-off rows lose clicks.
        let mut scroll = 0;
        let view = window(&screen, 40, 5, &mut scroll, None);
        assert_eq!(view.lines.len(), 5);
        assert_eq!(view.clicks.keys().copied().collect::<Vec<_>>(), [3]);

        // Arrows walk all rows in order (the PR too), report the diff to show, and stop at ends.
        assert_eq!(move_selection(&mut model, &screen, 1), Some(DiffRequest::local(&root, &change(b" M", "a.rs"))));
        move_selection(&mut model, &screen, 1);
        assert_eq!(model.selected, Some(Key::File(root.clone(), "b.rs".into())));
        assert_eq!(move_selection(&mut model, &screen, 1), Some(DiffRequest::Pr { url: pr.url() }));
        assert_eq!(move_selection(&mut model, &screen, 1), None, "already on the last row");

        // Selecting the last row scrolls it into view; clicks and highlight follow the scroll.
        let view = window(&screen, 40, 5, &mut scroll, screen.row_of(model.selected.as_ref()));
        assert_eq!(scroll, 2);
        assert_eq!(view.highlight, Some(3));
        assert_eq!(view.clicks.get(&3), Some(&Click::Pr { url: pr.url() }));
        assert_eq!(view.clicks.get(&1), Some(&Click::File { root: root.clone(), change: change(b" M", "a.rs") }));
        // A pull request row hovers like a link: its URL is the hover target; a repo header isn't.
        assert_eq!(view.url_at(3), Some(pr.url().as_str()));
        assert_eq!(view.url_at(0), None);
    }
}
