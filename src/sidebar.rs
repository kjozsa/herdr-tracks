//! The sidebar pane: follows one agent chat session in its tab and lists the git repos that
//! session changed (edit/write/commit tool calls in its omp or Claude Code transcript, or a
//! repo's git state moving after the session first touched it), with branch + changed files.

use crate::diff::DiffRequest;
use crate::git::{self, Change, RepoStatus};
use crate::github::{self, Checks, PrRef, PrState, PrStatus};
use crate::herdr::{self, PaneInfo};
use crate::repos;
use crate::state::{self, PrRecord, RepoRecord, SessionRepos};
use crate::transcript::{Format, Touch, Transcript};
use anyhow::{anyhow, Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const GIT_EVERY: Duration = Duration::from_secs(5);
const PR_EVERY: Duration = Duration::from_secs(30);
/// Delay before the selected file's diff opens, so holding an arrow key skips through.
const DIFF_DEBOUNCE: Duration = Duration::from_millis(120);

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
    /// File chosen with the arrow keys or a click, as (checkout root, path); its diff is shown.
    selected: Option<(PathBuf, String)>,
    /// When the selected file's diff should open; holding an arrow key opens only the last one.
    diff_due: Option<Instant>,
    /// First rendered row on screen when the list is taller than the pane.
    scroll: usize,
}

/// What a click on a sidebar row does.
#[derive(Debug, Clone, PartialEq)]
enum Click {
    /// Open the file's diff in a pane next to the agent.
    File { root: PathBuf, change: Change },
    /// Open the pull request in the browser.
    Pr { url: String },
}

/// All rendered rows plus the click target of each clickable row (by row index).
struct Screen {
    lines: Vec<Line>,
    clicks: HashMap<usize, Click>,
}

impl Screen {
    /// Changed files in display order, as (row, checkout root, change).
    fn files(&self) -> Vec<(usize, &PathBuf, &Change)> {
        let mut files: Vec<_> = self
            .clicks
            .iter()
            .filter_map(|(row, click)| match click {
                Click::File { root, change } => Some((*row, root, change)),
                Click::Pr { .. } => None,
            })
            .collect();
        files.sort_by_key(|(row, ..)| *row);
        files
    }

    fn row_of(&self, selected: Option<&(PathBuf, String)>) -> Option<usize> {
        let (root, path) = selected?;
        self.files().into_iter().find(|(_, r, c)| *r == root && c.path == *path).map(|(row, ..)| row)
    }
}

/// The part of a [`Screen`] that fits the pane; clicks keyed by on-screen row.
struct View {
    lines: Vec<Line>,
    clicks: HashMap<usize, Click>,
    highlight: Option<usize>,
}

struct PrBatch {
    session: String,
    results: Vec<(PrRef, Result<PrStatus, String>)>,
}

pub fn run() -> Result<()> {
    let me = std::env::var("HERDR_PANE_ID").map_err(|_| anyhow!("HERDR_PANE_ID not set"))?;
    let _term = Term::enter()?;
    event_loop(&me)
}

/// Runs for the pane's lifetime. There is no quit key: the sidebar is mandatory.
fn event_loop(me: &str) -> Result<()> {
    let mut model = Model::default();
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
        if model.diff_due.is_some_and(|due| now >= due) {
            model.diff_due = None;
            if let Err(e) = show_selected_diff(&mut model) {
                model.error = Some(format!("{e:#}"));
            }
        }
        let (cols, rows) = terminal::size()?;
        let screen = render(&model, usize::from(cols));
        let selected_row = screen.row_of(model.selected.as_ref());
        let view = window(&screen, usize::from(cols), usize::from(rows), &mut model.scroll, selected_row);
        if (&view.lines, view.highlight) != (&shown.0, shown.1) {
            draw(&view, usize::from(cols))?;
            shown = (view.lines.clone(), view.highlight);
        }
        let mut deadline = next_sample.min(next_git);
        if let Some(due) = model.diff_due {
            deadline = deadline.min(due);
        }
        if event::poll(deadline.saturating_duration_since(Instant::now()))? {
            let result = match event::read()? {
                Event::Key(KeyEvent { code: KeyCode::Down | KeyCode::Char('j'), .. }) => {
                    move_selection(&mut model, &screen, 1);
                    Ok(())
                }
                Event::Key(KeyEvent { code: KeyCode::Up | KeyCode::Char('k'), .. }) => {
                    move_selection(&mut model, &screen, -1);
                    Ok(())
                }
                Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => match &model.diff_pane {
                    Some(pane) => herdr::pane_focus(pane),
                    None => Ok(()),
                },
                Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                    model.selected = None;
                    model.diff_due = None;
                    match model.diff_pane.take() {
                        Some(pane) => herdr::pane_close(&pane),
                        None => Ok(()),
                    }
                }
                Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, .. }) => {
                    match view.clicks.get(&usize::from(row)) {
                        Some(click) => handle_click(&mut model, click),
                        None => Ok(()),
                    }
                }
                Event::Resize(..) => {
                    shown = (Vec::new(), None);
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(e) = result {
                model.error = Some(format!("{e:#}"));
            }
        }
    }
}

/// Moves the selection `step` files down (or up), clamped to the list; the diff follows
/// once the keys settle.
fn move_selection(model: &mut Model, screen: &Screen, step: isize) {
    let files = screen.files();
    let Some(last) = files.len().checked_sub(1) else { return };
    let current = model
        .selected
        .as_ref()
        .and_then(|(root, path)| files.iter().position(|(_, r, c)| *r == root && c.path == *path));
    let next = match current {
        Some(i) => i.saturating_add_signed(step).min(last),
        None if step > 0 => 0,
        None => last,
    };
    if Some(next) == current {
        return;
    }
    let (_, root, change) = files[next];
    model.selected = Some((root.clone(), change.path.clone()));
    model.diff_due = Some(Instant::now() + DIFF_DEBOUNCE);
}

fn handle_click(model: &mut Model, click: &Click) -> Result<()> {
    match click {
        Click::Pr { url } => {
            Command::new("xdg-open")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("xdg-open")?;
        }
        Click::File { root, change } => {
            model.selected = Some((root.clone(), change.path.clone()));
            model.diff_due = None;
            show_selected_diff(model)?;
        }
    }
    Ok(())
}

/// Shows the selected file's diff without taking focus from the sidebar. The new pane is split
/// off the previous diff pane, which then closes, so the layout holds still while browsing.
fn show_selected_diff(model: &mut Model) -> Result<()> {
    let Some((root, path)) = &model.selected else { return Ok(()) };
    let Some(Ok(status)) = model.statuses.get(root) else { return Ok(()) };
    let Some(change) = status.changes.iter().find(|c| &c.path == path) else { return Ok(()) };
    let agent = model.session.as_ref().map(|s| s.pane_id.clone()).ok_or_else(|| anyhow!("no agent pane"))?;
    let env = [(crate::diff::ENV, serde_json::to_string(&DiffRequest::new(root, change))?)];
    let open = |target: &str| herdr::open_plugin_pane(&crate::dock::plugin_id(), crate::DIFF_ENTRYPOINT, target, false, &env);
    let pane = match model.diff_pane.as_deref() {
        // The previous diff pane may already be gone (the user quit it): fall back to the agent.
        Some(previous) => open(previous).or_else(|_| open(&agent))?,
        None => open(&agent)?,
    };
    if let Some(previous) = model.diff_pane.replace(pane) {
        let _ = herdr::pane_close(&previous);
    }
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
    touches.extend(observed.into_iter().map(|path| Touch { path, writes: false }));

    let SessionRepos { repos: records, prs, .. } = &mut session.repos;
    let mut dirty = false;
    for touch in touches {
        let Some(root) = repos::repo_root(&touch.path) else { continue };
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
    for pr in activity.prs {
        if !prs.iter().any(|r| r.pr == pr) {
            prs.push(PrRecord { pr, root: None, status: None });
            dirty = true;
        }
    }
    // A PR belongs to the touched checkout whose remote is its repo; opening it counts as
    // a change to that checkout.
    for pr in prs.iter_mut().filter(|p| p.root.is_none()) {
        let slug = pr.pr.slug();
        let owner = records.iter_mut().find(|r| {
            model.remotes.entry(r.root.clone()).or_insert_with(|| github::remote_slugs(&r.root)).contains(&slug)
        });
        if let Some(record) = owner {
            record.changed = true;
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
        s.repos.prs.iter().any(|p| p.status.is_none() && !model.pr_errors.contains_key(&p.pr.url()))
    })
}

/// Fetches every non-final PR's status on a background thread.
fn start_pr_refresh(model: &Model) -> Option<Receiver<PrBatch>> {
    let session = model.session.as_ref()?;
    let todo: Vec<PrRef> = session
        .repos
        .prs
        .iter()
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
}

type Line = Vec<(String, Tone)>;

fn render(model: &Model, cols: usize) -> Screen {
    let mut lines: Vec<Line> = Vec::new();
    let mut clicks: HashMap<usize, Click> = HashMap::new();
    if let Some(err) = &model.error {
        lines.push(vec![(fit_end(&format!("! {err}"), cols), Tone::Error)]);
    }
    let Some(session) = &model.session else {
        lines.push(vec![(fit_end("no agent in this tab", cols), Tone::Dim)]);
        return Screen { lines, clicks };
    };
    if !session.title.is_empty() {
        lines.push(vec![(fit_end(&session.title, cols), Tone::Dim)]);
    }
    // Changed repos that still exist (`refresh_git` skips deleted ones), then PRs whose
    // repo has no touched checkout.
    let shown: Vec<&PathBuf> = session
        .repos
        .repos
        .iter()
        .filter(|r| r.changed && model.statuses.contains_key(&r.root))
        .map(|r| &r.root)
        .collect();
    let orphans: Vec<&PrRecord> = session.repos.prs.iter().filter(|p| p.root.is_none()).collect();
    if shown.is_empty() && orphans.is_empty() {
        lines.push(vec![(fit_end("no changes yet", cols), Tone::Dim)]);
    }
    for root in shown {
        if !lines.is_empty() {
            lines.push(Vec::new());
        }
        lines.push(repo_header(root, model.statuses.get(root), cols));
        for pr in session.repos.prs.iter().filter(|p| p.root.as_ref() == Some(root)) {
            clicks.insert(lines.len(), Click::Pr { url: pr.pr.url() });
            lines.push(pr_line(pr, model.pr_errors.get(&pr.pr.url()), cols));
        }
        match model.statuses.get(root) {
            None => {}
            Some(Err(e)) => lines.push(vec![(fit_end(&format!(" ! {e}"), cols), Tone::Error)]),
            Some(Ok(s)) => {
                for change in &s.changes {
                    let code = String::from_utf8_lossy(&change.code).into_owned();
                    let path = fit_start(&change.display(), cols.saturating_sub(4));
                    clicks.insert(lines.len(), Click::File { root: root.clone(), change: change.clone() });
                    lines.push(vec![
                        (" ".into(), Tone::Plain),
                        (code, change_tone(change.code)),
                        (" ".into(), Tone::Plain),
                        (path, Tone::Plain),
                    ]);
                }
            }
        }
    }
    for pr in orphans {
        if !lines.is_empty() {
            lines.push(Vec::new());
        }
        lines.push(vec![(fit_end(&format!("{}/{}", pr.pr.owner, pr.pr.repo), cols), Tone::Repo)]);
        clicks.insert(lines.len(), Click::Pr { url: pr.pr.url() });
        lines.push(pr_line(pr, model.pr_errors.get(&pr.pr.url()), cols));
    }
    Screen { lines, clicks }
}

/// ` #121 open ✓`: number (a link to the PR), state, CI rollup (omitted when the PR has no checks).
fn pr_line(pr: &PrRecord, error: Option<&String>, cols: usize) -> Line {
    let number = format!("#{}", pr.pr.number);
    let room = cols.saturating_sub(number.width() + 2);
    let mut line = vec![(" ".into(), Tone::Plain), (hyperlink(&pr.pr.url(), &number), Tone::Link)];
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

fn repo_header(root: &Path, status: Option<&Result<RepoStatus, String>>, cols: usize) -> Line {
    let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
    let mut branch = String::new();
    if let Some(Ok(s)) = status {
        branch = s.branch.clone();
        if s.ahead > 0 {
            branch.push_str(&format!(" ↑{}", s.ahead));
        }
        if s.behind > 0 {
            branch.push_str(&format!(" ↓{}", s.behind));
        }
    }
    let name = fit_end(&name, cols);
    let room = cols.saturating_sub(name.width() + 1);
    if branch.is_empty() || room < 2 {
        return vec![(name, Tone::Repo)];
    }
    vec![(name, Tone::Repo), (" ".into(), Tone::Plain), (fit_end(&branch, room), Tone::Branch)]
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
/// left below, the last line counts them instead. Clicks are re-keyed to on-screen rows.
fn window(screen: &Screen, cols: usize, rows: usize, scroll: &mut usize, selected: Option<usize>) -> View {
    let total = screen.lines.len();
    if total <= rows {
        *scroll = 0;
        return View { lines: screen.lines.clone(), clicks: screen.clicks.clone(), highlight: selected };
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
        if highlighted {
            // Highlighted rows are file rows: plain text, so their display width is exact.
            let used: usize = line.iter().map(|(text, _)| text.width()).sum();
            let pad = " ".repeat(cols.saturating_sub(used));
            queue!(out, SetAttribute(Attribute::Reverse), Print(pad), SetAttribute(Attribute::Reset))?;
        }
        queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    }
    queue!(out, cursor::MoveTo(0, view.lines.len() as u16), terminal::Clear(terminal::ClearType::FromCursorDown))?;
    out.flush()?;
    Ok(())
}

fn style(out: &mut impl Write, tone: Tone) -> Result<()> {
    match tone {
        Tone::Plain => {}
        Tone::Link => queue!(out, SetAttribute(Attribute::Underlined))?,
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

/// Raw mode + alternate screen for the pane's lifetime.
struct Term;

impl Term {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), terminal::EnterAlternateScreen, cursor::Hide, EnableMouseCapture)?;
        Ok(Term)
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture, cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
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
        let change = |code: &[u8; 2], path: &str| Change { code: *code, path: path.into(), orig: None };
        let pr = PrRef { owner: "o".into(), repo: "app".into(), number: 7 };
        let mut model = Model::default();
        model.statuses.insert(
            root.clone(),
            Ok(RepoStatus {
                branch: "main".into(),
                ahead: 0,
                behind: 0,
                changes: vec![change(b" M", "a.rs"), change(b"??", "b.rs")],
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

        // Arrows walk the files in order and stop at the ends.
        let selected = |m: &Model| m.selected.as_ref().map(|(_, p)| p.clone());
        move_selection(&mut model, &screen, 1);
        assert_eq!(selected(&model).as_deref(), Some("a.rs"));
        move_selection(&mut model, &screen, 1);
        move_selection(&mut model, &screen, 1);
        assert_eq!(selected(&model).as_deref(), Some("b.rs"));
        assert!(model.diff_due.is_some(), "moving schedules the diff");

        // Selecting the last file scrolls it into view; clicks and highlight follow the scroll.
        let view = window(&screen, 40, 5, &mut scroll, screen.row_of(model.selected.as_ref()));
        assert_eq!(scroll, 2);
        assert_eq!(view.highlight, Some(3));
        assert_eq!(view.clicks.get(&3), Some(&Click::File { root: root.clone(), change: change(b"??", "b.rs") }));
        assert_eq!(view.clicks.get(&1), Some(&Click::Pr { url: pr.url() }));
        move_selection(&mut model, &screen, -1);
        assert_eq!(selected(&model).as_deref(), Some("a.rs"));
    }
}
