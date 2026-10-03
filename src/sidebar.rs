//! The sidebar pane: follows one agent chat session in its tab and lists the git repos that
//! session changed (edit/write/commit tool calls in its omp or Claude Code transcript, or a
//! repo's git state moving after the session first touched it), with branch + changed files.

use crate::diff::{self, DiffRequest};
use crate::git::{self, Change, LineStat, RepoStatus};
use crate::github::{self, Checks, PrFile, PrRef, PrState, PrStatus};
use crate::herdr::{self, PaneInfo};
use crate::repos;
use crate::state::{self, DismissedRepo, PrRecord, RepoRecord, SessionRepos};
use crate::transcript::{Format, Touch, Transcript};
use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    files_jobs: Vec<Receiver<(String, Result<Vec<PrFile>, String>)>>,
    /// This sidebar's own pane id; names the request file its diff viewer follows.
    me: String,
    /// First rendered row on screen when the list is taller than the pane.
    scroll: usize,
    /// Right-click menu, while open.
    menu: Option<Menu>,
}

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

/// All rendered rows plus, by row index, what clicking and right-clicking each row acts on.
struct Screen {
    lines: Vec<Line>,
    clicks: HashMap<usize, Click>,
    menus: HashMap<usize, Target>,
}

impl Screen {
    /// Selectable rows in display order.
    fn rows(&self) -> Vec<(usize, &Click)> {
        let mut rows: Vec<_> = self.clicks.iter().map(|(row, click)| (*row, click)).collect();
        rows.sort_by_key(|(row, _)| *row);
        rows
    }

    fn row_of(&self, key: Option<&Key>) -> Option<usize> {
        let key = key?;
        self.clicks.iter().find(|(_, click)| click.key() == *key).map(|(row, _)| *row)
    }
}

/// The part of a [`Screen`] that fits the pane; clicks and menus keyed by on-screen row.
struct View {
    lines: Vec<Line>,
    clicks: HashMap<usize, Click>,
    menus: HashMap<usize, Target>,
    highlight: Option<usize>,
}

impl View {
    /// Draws the open menu over the rows it occupies.
    fn overlay(&mut self, menu: &Menu, cols: usize, rows: usize) {
        for (row, _, label) in menu.placed(rows) {
            while self.lines.len() <= row {
                self.lines.push(Vec::new());
            }
            let text = fit_end(&format!(" ▸ {label}"), cols);
            let pad = " ".repeat(cols.saturating_sub(text.width()));
            self.lines[row] = vec![(format!("{text}{pad}"), Tone::Menu)];
        }
    }
}

struct PrBatch {
    session: String,
    results: Vec<(PrRef, Result<PrStatus, String>)>,
}

pub fn run() -> Result<()> {
    let me = std::env::var("HERDR_PANE_ID").map_err(|_| anyhow!("HERDR_PANE_ID not set"))?;
    // herdr shows its own pane menu on right-click unless the pane asks for the clicks.
    herdr::forward_right_click(&me)?;
    let _term = crate::term::Term::enter()?;
    event_loop(&me)
}

/// Runs for the pane's lifetime. There is no quit key: the sidebar is mandatory.
fn event_loop(me: &str) -> Result<()> {
    let mut model = Model { me: me.to_string(), ..Model::default() };
    let mut shown: (Vec<Line>, Option<usize>) = (Vec::new(), None);
    let mut next_sample = Instant::now();
    let mut next_git = Instant::now();
    let mut next_pr = Instant::now();
    loop {
        let now = Instant::now();
        if now >= next_sample {
            next_sample = now + SAMPLE_EVERY;
            match sample(me, &mut model) {
                Ok(new_repos) => {
                    model.error = None;
                    if new_repos {
                        next_git = now;
                    }
                }
                Err(e) => model.error = Some(format!("{e:#}")),
            }
        }
        if now >= next_git {
            next_git = now + GIT_EVERY;
            model.statuses = refresh_git(&model);
            if let Err(e) = track_changes(&mut model) {
                model.error = Some(format!("{e:#}"));
            }
        }
        if let Some(job) = &model.pr_job {
            match job.try_recv() {
                Ok(batch) => {
                    model.pr_job = None;
                    if let Err(e) = apply_prs(&mut model, batch) {
                        model.error = Some(format!("{e:#}"));
                    }
                }
                Err(TryRecvError::Disconnected) => model.pr_job = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if model.pr_job.is_none() && (now >= next_pr || has_unfetched_pr(&model)) {
            next_pr = now + PR_EVERY;
            model.pr_job = start_pr_refresh(&model);
        }
        let (cols, rows) = terminal::size()?;
        let screen = render(&model, usize::from(cols));
        let selected_row = screen.row_of(model.selected.as_ref());
        let mut view = window(&screen, usize::from(cols), usize::from(rows), &mut model.scroll, selected_row);
        if let Some(menu) = &model.menu {
            view.overlay(menu, usize::from(cols), usize::from(rows));
        }
        if (&view.lines, view.highlight) != (&shown.0, shown.1) {
            draw(&view, usize::from(cols))?;
            shown = (view.lines.clone(), view.highlight);
        }
        let Model { files_jobs, pr_files, .. } = &mut model;
        files_jobs.retain(|job| match job.try_recv() {
            Ok((url, files)) => {
                pr_files.insert(url, files);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
        if event::poll(next_sample.min(next_git).saturating_duration_since(Instant::now()))? {
            let event = event::read()?;
            let result = match (model.menu.take(), event) {
                // An open menu takes the next click or key: an item runs, anything else closes it.
                (Some(menu), Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, .. })) => {
                    match menu.placed(usize::from(rows)).into_iter().find(|(r, ..)| *r == usize::from(row)) {
                        Some((_, item, _)) => run_menu(&mut model, &menu.target, item),
                        None => Ok(()),
                    }
                }
                // A right-click elsewhere opens a menu there instead (arm below).
                (Some(_), Event::Key(_) | Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Middle), .. })) => {
                    Ok(())
                }
                (menu @ Some(_), Event::Mouse(_)) => {
                    model.menu = menu;
                    Ok(())
                }
                (_, Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), row, .. })) => {
                    let row = usize::from(row);
                    model.menu = view.menus.get(&row).map(|target| Menu { target: target.clone(), row });
                    Ok(())
                }
                (_, Event::Key(KeyEvent { code, .. })) => handle_key(&mut model, &screen, code),
                (_, Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, modifiers, .. })) => {
                    match view.clicks.get(&usize::from(row)) {
                        Some(click) => handle_click(&mut model, click, modifiers.contains(KeyModifiers::CONTROL)),
                        None => Ok(()),
                    }
                }
                (menu, Event::Resize(..)) => {
                    model.menu = menu;
                    shown = (Vec::new(), None);
                    crate::dock::hold_width(&model.me)
                }
                (menu, _) => {
                    model.menu = menu;
                    Ok(())
                }
            };
            if let Err(e) = result {
                model.error = Some(format!("{e:#}"));
            }
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

fn is_dismissed(repos: &SessionRepos, root: &Path) -> bool {
    repos.dismissed_repos.iter().any(|d| d.root == root)
}

/// Pull requests still shown: not dismissed, and not under a dismissed repo.
fn visible_prs(repos: &SessionRepos) -> impl Iterator<Item = &PrRecord> {
    repos.prs.iter().filter(|p| {
        !repos.dismissed_prs.iter().any(|d| d.same(&p.pr)) && !p.root.as_ref().is_some_and(|root| is_dismissed(repos, root))
    })
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
    if let Click::Pr { url } = click {
        if !model.expanded.remove(url) {
            expand(model, url.clone());
        }
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

/// Picks the followed agent pane and records repos under its processes' working dirs.
/// Returns whether the repo list changed.
fn sample(me: &str, model: &mut Model) -> Result<bool> {
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
            slot.insert(Session { pane_id: target.pane_id.clone(), title, repos, persisted, transcript })
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

    let SessionRepos { repos: records, prs, dismissed_repos, .. } = &mut session.repos;
    let mut dirty = false;
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

fn has_unfetched_pr(model: &Model) -> bool {
    model.session.as_ref().is_some_and(|s| {
        visible_prs(&s.repos).any(|p| p.status.is_none() && !model.pr_errors.contains_key(&p.pr.url()))
    })
}

/// Fetches every non-final PR's status on a background thread.
fn start_pr_refresh(model: &Model) -> Option<Receiver<PrBatch>> {
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

fn apply_prs(model: &mut Model, batch: PrBatch) -> Result<()> {
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
fn track_changes(model: &mut Model) -> Result<()> {
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

fn refresh_git(model: &Model) -> HashMap<PathBuf, Result<RepoStatus, String>> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    Dim,
    Repo,
    Branch,
    Staged,
    Unstaged,
    Untracked,
    Conflict,
    Error,
    Good,
    Bad,
    Waiting,
    Merged,
    /// Clickable text (an OSC 8 hyperlink); underlined so it reads as a link.
    Link,
    /// An entry of the right-click menu.
    Menu,
}

type Line = Vec<(String, Tone)>;

fn render(model: &Model, cols: usize) -> Screen {
    let mut lines: Vec<Line> = Vec::new();
    let mut clicks: HashMap<usize, Click> = HashMap::new();
    let mut menus: HashMap<usize, Target> = HashMap::new();
    if let Some(err) = &model.error {
        lines.push(vec![(fit_end(&format!("! {err}"), cols), Tone::Error)]);
    }
    let Some(session) = &model.session else {
        lines.push(vec![(fit_end("no agent in this tab", cols), Tone::Dim)]);
        return Screen { lines, clicks, menus };
    };
    if !session.title.is_empty() {
        lines.push(vec![(fit_end(&session.title, cols), Tone::Dim)]);
    }
    // Repos the chat changed or has pull requests in (`refresh_git` skips deleted ones), then
    // PRs whose repo has no touched checkout. Whatever the user dismissed stays hidden.
    let prs: Vec<&PrRecord> = visible_prs(&session.repos).collect();
    let has_prs = |root: &PathBuf| prs.iter().any(|p| p.root.as_ref() == Some(root));
    let shown: Vec<&PathBuf> = session
        .repos
        .repos
        .iter()
        .filter(|r| (r.changed || has_prs(&r.root)) && model.statuses.contains_key(&r.root))
        .filter(|r| !is_dismissed(&session.repos, &r.root))
        .map(|r| &r.root)
        .collect();
    let orphans: Vec<&PrRecord> = prs.iter().copied().filter(|p| p.root.is_none()).collect();
    if shown.is_empty() && orphans.is_empty() {
        lines.push(vec![(fit_end("no changes yet", cols), Tone::Dim)]);
    }
    for root in shown {
        if !lines.is_empty() {
            lines.push(Vec::new());
        }
        menus.insert(lines.len(), Target::Repo(root.clone()));
        lines.push(repo_header(root, model.statuses.get(root), cols));
        for pr in prs.iter().filter(|p| p.root.as_ref() == Some(root)) {
            push_pr(&mut lines, &mut clicks, &mut menus, model, pr, cols);
        }
        match model.statuses.get(root) {
            None => {}
            Some(Err(e)) => lines.push(vec![(fit_end(&format!(" ! {e}"), cols), Tone::Error)]),
            Some(Ok(s)) => {
                // Uncommitted changes (two-letter `git status` codes), then what unpushed
                // commits change (`↑`).
                for change in &s.changes {
                    let code = String::from_utf8_lossy(&change.code).into_owned();
                    clicks.insert(lines.len(), Click::File { root: root.clone(), change: change.clone() });
                    lines.push(file_row(" ", (code, change_tone(change.code)), &change.display(), change.stat, cols));
                }
                for (path, stat) in &s.unpushed {
                    clicks.insert(lines.len(), Click::Unpushed { root: root.clone(), path: path.clone() });
                    lines.push(file_row(" ", ("↑ ".into(), Tone::Branch), path, Some(*stat), cols));
                }
            }
        }
    }
    for pr in orphans {
        if !lines.is_empty() {
            lines.push(Vec::new());
        }
        menus.insert(lines.len(), Target::Pr(pr.pr.clone()));
        lines.push(vec![(fit_end(&format!("{}/{}", pr.pr.owner, pr.pr.repo), cols), Tone::Repo)]);
        push_pr(&mut lines, &mut clicks, &mut menus, model, pr, cols);
    }
    Screen { lines, clicks, menus }
}

/// A pull request row and, when expanded, one row per file it changes.
fn push_pr(
    lines: &mut Vec<Line>,
    clicks: &mut HashMap<usize, Click>,
    menus: &mut HashMap<usize, Target>,
    model: &Model,
    pr: &PrRecord,
    cols: usize,
) {
    let url = pr.pr.url();
    let expanded = model.expanded.contains(&url);
    clicks.insert(lines.len(), Click::Pr { url: url.clone() });
    menus.insert(lines.len(), Target::Pr(pr.pr.clone()));
    lines.push(pr_line(pr, model.pr_errors.get(&url), expanded, cols));
    if !expanded {
        return;
    }
    match model.pr_files.get(&url) {
        None => lines.push(vec![("    …".into(), Tone::Dim)]),
        Some(Err(e)) => lines.push(vec![(fit_end(&format!("    ! {e}"), cols), Tone::Error)]),
        Some(Ok(files)) => {
            for file in files {
                let tone = match file.code() {
                    'A' => Tone::Good,
                    'D' => Tone::Bad,
                    _ => Tone::Waiting,
                };
                clicks.insert(lines.len(), Click::PrFile { url: url.clone(), path: file.path.clone() });
                let stat = LineStat::Lines { added: file.additions, removed: file.deletions };
                lines.push(file_row("    ", (file.code().to_string(), tone), &file.path, Some(stat), cols));
            }
        }
    }
}

/// `<indent><code> <path>        +12 -3`: the path is shortened from the left so the line
/// counts stay right-aligned in green and red (`bin` for binary files).
fn file_row(indent: &str, (code, tone): (String, Tone), path: &str, stat: Option<LineStat>, cols: usize) -> Line {
    let stat: Vec<(String, Tone)> = match stat {
        Some(LineStat::Lines { added, removed }) => [(added, '+', Tone::Good), (removed, '-', Tone::Bad)]
            .into_iter()
            .filter(|(n, ..)| *n > 0)
            .map(|(n, sign, tone)| (format!("{sign}{n}"), tone))
            .collect(),
        Some(LineStat::Binary) => vec![("bin".into(), Tone::Dim)],
        None => Vec::new(),
    };
    let stat_width = stat.iter().map(|(t, _)| t.width() + 1).sum::<usize>(); // each with a leading space
    let prefix = indent.width() + code.width() + 1;
    let path = fit_start(path, cols.saturating_sub(prefix + stat_width));
    let mut line = vec![(indent.to_string(), Tone::Plain), (code, tone), (" ".into(), Tone::Plain)];
    let gap = cols.saturating_sub(prefix + path.width() + stat_width);
    line.push((path, Tone::Plain));
    if !stat.is_empty() {
        line.push((" ".repeat(gap), Tone::Plain));
        for (text, tone) in stat {
            line.push((" ".into(), Tone::Plain));
            line.push((text, tone));
        }
    }
    line
}

/// ` ▸ #121 open ✓`: expand marker, number (a link to the PR), state, CI rollup (omitted when
/// the PR has no checks).
fn pr_line(pr: &PrRecord, error: Option<&String>, expanded: bool, cols: usize) -> Line {
    let number = format!("#{}", pr.pr.number);
    let room = cols.saturating_sub(number.width() + 4);
    let marker = if expanded { " ▾ " } else { " ▸ " };
    let mut line = vec![(marker.into(), Tone::Dim), (hyperlink(&pr.pr.url(), &number), Tone::Link)];
    match (pr.status, error) {
        (Some(status), _) => {
            let (state, tone) = match status.state {
                PrState::Open => ("open", Tone::Good),
                PrState::Draft => ("draft", Tone::Dim),
                PrState::Merged => ("merged", Tone::Merged),
                PrState::Closed => ("closed", Tone::Bad),
            };
            line.push((format!(" {state}"), tone));
            if let Some(checks) = status.checks {
                let (mark, tone) = match checks {
                    Checks::Passing => ("✓", Tone::Good),
                    Checks::Failing => ("✗", Tone::Bad),
                    Checks::Pending => ("◔", Tone::Waiting),
                };
                line.push((format!(" {mark}"), tone));
            }
        }
        (None, Some(err)) => line.push((format!(" {}", fit_end(&format!("! {err}"), room)), Tone::Error)),
        (None, None) => line.push((" …".into(), Tone::Dim)),
    }
    line
}

/// OSC 8 hyperlink; herdr opens it on Ctrl+click. The escapes take no columns, so callers
/// measure `text`, not the result.
fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// `name branch ↑1 ↓2`: a long branch name is shortened, never the ahead/behind counts.
fn repo_header(root: &Path, status: Option<&Result<RepoStatus, String>>, cols: usize) -> Line {
    let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
    let name = fit_end(&name, cols);
    let Some(Ok(s)) = status else { return vec![(name, Tone::Repo)] };
    let tracking: String = [(s.ahead, '↑'), (s.behind, '↓')]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, arrow)| format!(" {arrow}{n}"))
        .collect();
    let room = cols.saturating_sub(name.width() + 1 + tracking.width());
    let mut line = vec![(name, Tone::Repo)];
    if room >= 2 {
        line.push((" ".into(), Tone::Plain));
        line.push((fit_end(&s.branch, room), Tone::Branch));
    }
    if !tracking.is_empty() {
        line.push((tracking, Tone::Waiting));
    }
    line
}

fn change_tone(code: [u8; 2]) -> Tone {
    match code {
        [b'?', b'?'] => Tone::Untracked,
        [b'U', _] | [_, b'U'] | [b'A', b'A'] | [b'D', b'D'] => Tone::Conflict,
        [_, b' '] => Tone::Staged,
        _ => Tone::Unstaged,
    }
}

/// The rows that fit a `rows`-high pane. Scrolls so `selected` stays visible; when rows are
/// left below, the last line counts them instead. Clicks and menus are re-keyed to on-screen rows.
fn window(screen: &Screen, cols: usize, rows: usize, scroll: &mut usize, selected: Option<usize>) -> View {
    let total = screen.lines.len();
    if total <= rows {
        *scroll = 0;
        return View {
            lines: screen.lines.clone(),
            clicks: screen.clicks.clone(),
            menus: screen.menus.clone(),
            highlight: selected,
        };
    }
    let body = rows.saturating_sub(1);
    if let Some(row) = selected {
        if row < *scroll {
            *scroll = row;
        } else if row >= *scroll + body {
            *scroll = row + 1 - body;
        }
    }
    *scroll = (*scroll).min(total - body);
    let end = *scroll + body;
    let mut lines = screen.lines[*scroll..end].to_vec();
    if end < total {
        lines.push(vec![(fit_end(&format!("… {} more lines", total - end), cols), Tone::Dim)]);
    }
    let in_view = |row: usize| (*scroll..end).contains(&row).then(|| row - *scroll);
    View {
        lines,
        clicks: screen.clicks.iter().filter_map(|(row, click)| Some((in_view(*row)?, click.clone()))).collect(),
        menus: screen.menus.iter().filter_map(|(row, target)| Some((in_view(*row)?, target.clone()))).collect(),
        highlight: selected.and_then(in_view),
    }
}

/// Truncates keeping the start: `long-branch-na…`.
fn fit_end(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// Truncates keeping the end, where file names live: `…/deep/file.rs`.
fn fit_start(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        tail.push(c);
        used += w;
    }
    let mut out = String::from(if max > 0 { "…" } else { "" });
    out.extend(tail.into_iter().rev());
    out
}

fn draw(view: &View, cols: usize) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for (row, line) in view.lines.iter().enumerate() {
        let highlighted = view.highlight == Some(row);
        queue!(out, cursor::MoveTo(0, row as u16))?;
        for (text, tone) in line {
            style(&mut out, *tone)?;
            if highlighted {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            queue!(out, Print(text), SetAttribute(Attribute::Reset), ResetColor)?;
        }
        // Rows may carry link escapes: count only the columns they show.
        let used: usize = line.iter().map(|(text, _)| crate::ansi::width(text)).sum();
        if highlighted {
            let pad = " ".repeat(cols.saturating_sub(used));
            queue!(out, SetAttribute(Attribute::Reverse), Print(pad), SetAttribute(Attribute::Reset))?;
        } else if used < cols {
            // Not on full rows: after the last column the cursor waits to wrap, and an erase
            // there wipes that last cell.
            queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
        }
    }
    queue!(out, cursor::MoveTo(0, view.lines.len() as u16), terminal::Clear(terminal::ClearType::FromCursorDown))?;
    out.flush()?;
    Ok(())
}

fn style(out: &mut impl Write, tone: Tone) -> Result<()> {
    match tone {
        Tone::Plain => {}
        Tone::Link => queue!(out, SetAttribute(Attribute::Underlined))?,
        Tone::Menu => queue!(out, SetAttribute(Attribute::Reverse), SetAttribute(Attribute::Bold))?,
        Tone::Dim => queue!(out, SetAttribute(Attribute::Dim))?,
        Tone::Repo => queue!(out, SetAttribute(Attribute::Bold))?,
        Tone::Branch => queue!(out, SetForegroundColor(Color::Cyan))?,
        Tone::Staged => queue!(out, SetForegroundColor(Color::Green))?,
        Tone::Unstaged | Tone::Error | Tone::Bad => queue!(out, SetForegroundColor(Color::Red))?,
        Tone::Untracked => queue!(out, SetForegroundColor(Color::DarkYellow))?,
        Tone::Conflict | Tone::Merged => queue!(out, SetForegroundColor(Color::Magenta))?,
        Tone::Good => queue!(out, SetForegroundColor(Color::Green))?,
        Tone::Waiting => queue!(out, SetForegroundColor(Color::Yellow))?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_display_width() {
        assert_eq!(fit_end("feature/long-name", 8), "feature…");
        assert_eq!(fit_start("src/deep/module/file.rs", 10), "…e/file.rs");
        assert_eq!(fit_start("日本語のファイル.rs", 9), "…イル.rs");
        assert_eq!(fit_end("short", 8), "short");
    }

    #[test]
    fn file_rows_right_align_line_counts_and_shorten_the_path() {
        let text = |line: &Line| line.iter().map(|(t, _)| t.as_str()).collect::<String>();
        let stat = |added, removed| Some(LineStat::Lines { added, removed });
        let row = file_row(" ", (" M".into(), Tone::Unstaged), "src/a.rs", stat(12, 3), 24);
        assert_eq!(text(&row), format!("  M src/a.rs{}+12 -3", " ".repeat(6)));
        assert_eq!(text(&row).width(), 24, "counts end at the right edge");
        assert!(row.contains(&("+12".into(), Tone::Good)) && row.contains(&("-3".into(), Tone::Bad)));
        // Long paths give way to the counts; zero counts and missing stats are left out.
        let row = file_row(" ", ("??".into(), Tone::Untracked), "deep/nested/dir/new_file.rs", stat(40, 0), 24);
        assert_eq!(text(&row), " ?? …dir/new_file.rs +40");
        assert_eq!(text(&row).width(), 24);
        assert_eq!(text(&file_row(" ", ("A ".into(), Tone::Staged), "logo.png", Some(LineStat::Binary), 20)), " A  logo.png     bin");
        assert_eq!(text(&file_row(" ", (" M".into(), Tone::Unstaged), "x.rs", None, 20)), "  M x.rs");
    }

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
    fn change_codes_map_to_git_colors() {
        assert_eq!(change_tone(*b"M "), Tone::Staged);
        assert_eq!(change_tone(*b" M"), Tone::Unstaged);
        assert_eq!(change_tone(*b"MM"), Tone::Unstaged);
        assert_eq!(change_tone(*b"??"), Tone::Untracked);
        assert_eq!(change_tone(*b"UU"), Tone::Conflict);
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
