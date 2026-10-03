//! The sidebar pane: follows one agent chat session in its tab and lists the git repos that
//! session changed (edit/write/commit tool calls in its omp or Claude Code transcript, or a
//! repo's git state moving after the session first touched it), with branch + changed files.
//!
//! This module holds the state, the event loop and input handling; [`collect`] gathers the
//! data and [`render`] lays it out and draws it.

mod collect;
mod render;

use crate::diff::{self, DiffRequest};
use crate::git::{Change, RepoStatus};
use crate::github::{self, PrFile, PrRef};
use crate::herdr;
use crate::state::{self, DismissedRepo, SessionRepos};
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

/// Which chat session the sidebar follows, and whether its repo set is persisted.
struct Session {
    pane_id: String,
    title: String,
    repos: SessionRepos,
    persisted: bool,
    /// Transcript being tailed (omp, Claude Code); other agents rely on process working dirs only.
    transcript: Option<Transcript>,
}

#[derive(Default)]
struct Model {
    session: Option<Session>,
    statuses: HashMap<PathBuf, Result<RepoStatus, String>>,
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
    /// Pull requests expanded to list their files, by URL.
    expanded: HashSet<String>,
    /// Files of expanded pull requests, by URL; absent while loading.
    pr_files: HashMap<String, Result<Vec<PrFile>, String>>,
    /// In-flight file list fetches (run `gh` off the UI thread).
    files_jobs: Vec<Receiver<FilesFetched>>,
    /// This sidebar's own pane id; names the request file its diff viewer follows.
    me: String,
    /// First rendered row on screen when the list is taller than the pane.
    scroll: usize,
    /// Right-click menu, while open.
    menu: Option<Menu>,
}

impl Model {
    /// Shows a failure on the sidebar's first row; the loop keeps running.
    fn note(&mut self, result: Result<()>) {
        if let Err(e) = result {
            self.error = Some(format!("{e:#}"));
        }
    }
}

/// A pull request's file list, fetched in the background, by URL.
type FilesFetched = (String, Result<Vec<PrFile>, String>);

/// What a sidebar row is: selecting or clicking it shows its diff.
#[derive(Debug, Clone, PartialEq)]
enum Click {
    /// A changed file in a local checkout.
    File { root: PathBuf, change: Change },
    /// A pull request; expands to its files.
    Pr { url: String },
    /// One file of an expanded pull request.
    PrFile { url: String, path: String },
    /// A file changed by commits not yet pushed.
    Unpushed { root: PathBuf, path: String },
}

/// Identity of a selectable row that survives refreshes (a file's status code may change).
#[derive(Debug, Clone, PartialEq)]
enum Key {
    File(PathBuf, String),
    Pr(String),
    PrFile(String, String),
    Unpushed(PathBuf, String),
}

impl Click {
    fn key(&self) -> Key {
        match self {
            Click::File { root, change } => Key::File(root.clone(), change.path.clone()),
            Click::Pr { url } => Key::Pr(url.clone()),
            Click::PrFile { url, path } => Key::PrFile(url.clone(), path.clone()),
            Click::Unpushed { root, path } => Key::Unpushed(root.clone(), path.clone()),
        }
    }

    fn diff_request(&self) -> DiffRequest {
        match self {
            Click::File { root, change } => DiffRequest::local(root, change),
            Click::Pr { url } => DiffRequest::Pr { url: url.clone(), path: None },
            Click::PrFile { url, path } => DiffRequest::Pr { url: url.clone(), path: Some(path.clone()) },
            Click::Unpushed { root, path } => DiffRequest::Unpushed { root: root.clone(), path: path.clone() },
        }
    }
}

/// What a right-click menu acts on: a repo header, or a pull request row.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Repo(PathBuf),
    Pr(PrRef),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MenuItem {
    OpenPr,
    Dismiss,
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
            Target::Pr(_) => &[(MenuItem::OpenPr, "open on GitHub"), (MenuItem::Dismiss, "dismiss PR")],
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
    let mut shown: (Vec<Line>, Option<usize>) = (Vec::new(), None);
    let now = Instant::now();
    let mut due = Due { sample: now, git: now, pr: now };
    loop {
        refresh(&mut model, &mut due);
        let (cols, rows) = terminal::size()?;
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        let screen = render::render(&model, cols);
        let selected_row = screen.row_of(model.selected.as_ref());
        let mut view = render::window(&screen, cols, rows, &mut model.scroll, selected_row);
        if let Some(menu) = &model.menu {
            view.overlay(menu, cols, rows);
        }
        if (&view.lines, view.highlight) != (&shown.0, shown.1) {
            render::draw(&view, cols)?;
            shown = (view.lines.clone(), view.highlight);
        }
        if event::poll(due.sample.min(due.git).saturating_duration_since(Instant::now()))? {
            let event = event::read()?;
            if matches!(event, Event::Resize(..)) {
                shown = (Vec::new(), None);
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
        model.statuses = collect::refresh_git(model);
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
    let Model { files_jobs, pr_files, .. } = model;
    files_jobs.retain(|job| match job.try_recv() {
        Ok((url, files)) => {
            pr_files.insert(url, files);
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
        (None, Event::Key(KeyEvent { code, .. })) => handle_key(model, screen, code),
        (None, Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, modifiers, .. })) => {
            match view.clicks.get(&usize::from(row)) {
                Some(click) => handle_click(model, click, modifiers.contains(KeyModifiers::CONTROL)),
                None => Ok(()),
            }
        }
        (menu, Event::Resize(..)) => {
            model.menu = menu;
            crate::dock::hold_width(&model.me)
        }
        // Mouse moves and releases, focus changes: an open menu stays open.
        (menu, _) => {
            model.menu = menu;
            Ok(())
        }
    }
}

/// ↑/↓ (`k`/`j`) select rows, →/← (`l`/`h`) expand and collapse pull requests, `o` opens the
/// selected pull request in the browser, Enter focuses the diff, Esc closes it.
fn handle_key(model: &mut Model, screen: &Screen, code: KeyCode) -> Result<()> {
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
            if let Some(Key::Pr(url)) = &model.selected {
                expand(model, url.clone());
            }
            Ok(())
        }
        KeyCode::Left | KeyCode::Char('h') => match model.selected.clone() {
            Some(Key::Pr(url)) => {
                model.expanded.remove(&url);
                Ok(())
            }
            Some(Key::PrFile(url, _)) => {
                model.expanded.remove(&url);
                model.selected = Some(Key::Pr(url.clone()));
                show_diff(model, &DiffRequest::Pr { url, path: None })
            }
            _ => Ok(()),
        },
        KeyCode::Char('o') => match &model.selected {
            Some(Key::Pr(url) | Key::PrFile(url, _)) => open_url(url),
            _ => Ok(()),
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

/// Lists a pull request's files under it, fetching them in the background.
fn expand(model: &mut Model, url: String) {
    if !model.expanded.insert(url.clone()) {
        return;
    }
    model.pr_files.remove(&url); // refetch: an open PR may have changed
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let files = github::pr_files(&url);
        let _ = tx.send((url, files));
    });
    model.files_jobs.push(rx);
}

fn run_menu(model: &mut Model, target: &Target, item: MenuItem) -> Result<()> {
    match (item, target) {
        (MenuItem::OpenPr, Target::Pr(pr)) => open_url(&pr.url()),
        (MenuItem::Dismiss, target) => dismiss(model, target),
        (MenuItem::OpenPr, Target::Repo(_)) => Ok(()),
    }
}

/// Hides a pull request from this chat's sidebar for good, or a repo (with its pull requests)
/// until the chat touches it again.
fn dismiss(model: &mut Model, target: &Target) -> Result<()> {
    let Some(session) = model.session.as_mut() else { return Ok(()) };
    let at = session.transcript.as_ref().map_or(0, Transcript::position);
    let repos = &mut session.repos;
    match target {
        Target::Repo(root) if !is_dismissed(repos, root) => repos.dismissed_repos.push(DismissedRepo { root: root.clone(), at }),
        Target::Pr(pr) if !repos.dismissed_prs.iter().any(|d| d.same(pr)) => repos.dismissed_prs.push(pr.clone()),
        _ => return Ok(()),
    }
    if session.persisted {
        state::save_session(&session.repos)?;
    }
    Ok(())
}

fn open_url(url: &str) -> Result<()> {
    Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("xdg-open")?;
    Ok(())
}

/// Clicking selects the row and shows its diff; a pull request row also expands or collapses.
/// Ctrl+click on a pull request opens it in the browser.
fn handle_click(model: &mut Model, click: &Click, ctrl: bool) -> Result<()> {
    if let (Click::Pr { url }, true) = (click, ctrl) {
        return open_url(url);
    }
    if let Click::Pr { url } = click
        && !model.expanded.remove(url) {
            expand(model, url.clone());
        }
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
        });
        let screen = render(&model, 40);
        let rows: Vec<Key> = screen.rows().into_iter().map(|(_, c)| c.key()).collect();
        assert_eq!(rows, [Key::File(root.clone(), "a.py".into()), Key::Unpushed(root.clone(), "e2e/b.py".into())]);
        let unpushed = screen.rows()[1].1.diff_request();
        assert_eq!(unpushed, DiffRequest::Unpushed { root, path: "e2e/b.py".into() });
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
        assert_eq!(below.placed(10).iter().map(|(r, i, _)| (*r, *i)).collect::<Vec<_>>(), [(4, MenuItem::OpenPr), (5, MenuItem::Dismiss)]);
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
        });
        // Rows: 0 title, 1 blank, 2 repo header, 3 PR, 4 a.rs, 5 b.rs.
        let screen = render(&model, 40);
        assert_eq!(screen.clicks.get(&3), Some(&Click::Pr { url: pr.url() }));
        assert_eq!(screen.clicks.get(&4), Some(&Click::File { root: root.clone(), change: change(b" M", "a.rs") }));
        assert_eq!(screen.clicks.get(&5), Some(&Click::File { root: root.clone(), change: change(b"??", "b.rs") }));
        assert_eq!(screen.clicks.len(), 3);

        // Five rows, nothing selected: four content rows plus "… more"; cut-off rows lose clicks.
        let mut scroll = 0;
        let view = window(&screen, 40, 5, &mut scroll, None);
        assert_eq!(view.lines.len(), 5);
        assert_eq!(view.clicks.keys().copied().collect::<Vec<_>>(), [3]);

        // Arrows walk all rows in order (the PR too), report the diff to show, and stop at ends.
        assert_eq!(move_selection(&mut model, &screen, 1), Some(DiffRequest::Pr { url: pr.url(), path: None }));
        assert_eq!(move_selection(&mut model, &screen, 1), Some(DiffRequest::local(&root, &change(b" M", "a.rs"))));
        move_selection(&mut model, &screen, 1);
        assert_eq!(model.selected, Some(Key::File(root.clone(), "b.rs".into())));
        assert_eq!(move_selection(&mut model, &screen, 1), None, "already on the last row");

        // Selecting the last file scrolls it into view; clicks and highlight follow the scroll.
        let view = window(&screen, 40, 5, &mut scroll, screen.row_of(model.selected.as_ref()));
        assert_eq!(scroll, 2);
        assert_eq!(view.highlight, Some(3));
        assert_eq!(view.clicks.get(&3), Some(&Click::File { root: root.clone(), change: change(b"??", "b.rs") }));
        assert_eq!(view.clicks.get(&1), Some(&Click::Pr { url: pr.url() }));

        // Expanding the PR lists its files right under it; they are selectable and map to the
        // PR's per-file diff, and the local files move down.
        model.expanded.insert(pr.url());
        let file = PrFile { path: "x.rs".into(), change_type: "ADDED".into(), additions: 5, deletions: 0 };
        model.pr_files.insert(pr.url(), Ok(vec![file]));
        let screen = render(&model, 40);
        assert_eq!(screen.clicks.get(&4), Some(&Click::PrFile { url: pr.url(), path: "x.rs".into() }));
        assert_eq!(screen.clicks.get(&5), Some(&Click::File { root: root.clone(), change: change(b" M", "a.rs") }));
        model.selected = Some(Key::Pr(pr.url()));
        assert_eq!(
            move_selection(&mut model, &screen, 1),
            Some(DiffRequest::Pr { url: pr.url(), path: Some("x.rs".into()) })
        );
    }
}
