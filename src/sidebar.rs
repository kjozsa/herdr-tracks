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
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
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
    /// Diff pane opened by the last file click; replaced by the next one.
    diff_pane: Option<String>,
}

/// What a click on a sidebar row does.
#[derive(Debug, Clone, PartialEq)]
enum Click {
    /// Open the file's diff in a pane next to the agent.
    File { root: PathBuf, change: Change },
    /// Open the pull request in the browser.
    Pr { url: String },
}

/// Rendered rows plus the click target of each clickable row (by row index).
struct Screen {
    lines: Vec<Line>,
    clicks: HashMap<usize, Click>,
}

struct PrBatch {
    session: String,
    results: Vec<(PrRef, Result<PrStatus, String>)>,
}

pub fn run() -> Result<()> {
    let me = std::env::var("HERDR_PANE_ID").map_err(|_| anyhow!("HERDR_PANE_ID not set"))?;
    let quit = {
        let _term = Term::enter()?;
        event_loop(&me)?
    };
    if quit {
        crate::dock::close_self(&me)?;
    }
    Ok(())
}

/// Runs until the user quits (`true`) or the terminal goes away (`false`).
fn event_loop(me: &str) -> Result<bool> {
    let mut model = Model::default();
    let mut shown: Vec<Line> = Vec::new();
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
        let screen = render(&model, usize::from(cols), usize::from(rows));
        if screen.lines != shown {
            draw(&screen.lines)?;
            shown = screen.lines;
        }
        let wait = next_sample.min(next_git).saturating_duration_since(Instant::now());
        if event::poll(wait)? {
            match event::read()? {
                Event::Key(KeyEvent { code: KeyCode::Char('q'), .. }) => return Ok(true),
                Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    return Ok(true)
                }
                Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, .. }) => {
                    if let Some(click) = screen.clicks.get(&usize::from(row)) {
                        if let Err(e) = handle_click(&mut model, click) {
                            model.error = Some(format!("{e:#}"));
                        }
                    }
                }
                Event::Resize(..) => shown.clear(),
                _ => {}
            }
        }
    }
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
            let agent = model.session.as_ref().map(|s| s.pane_id.clone()).ok_or_else(|| anyhow!("no agent pane"))?;
            if let Some(previous) = model.diff_pane.take() {
                let _ = herdr::pane_close(&previous); // already closed by the user is fine
            }
            let request = serde_json::to_string(&DiffRequest::new(root, change))?;
            let pane = herdr::open_plugin_pane(
                &crate::dock::plugin_id(),
                crate::DIFF_ENTRYPOINT,
                &agent,
                true,
                &[(crate::diff::ENV, request)],
            )?;
            model.diff_pane = Some(pane);
        }
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

fn render(model: &Model, cols: usize, rows: usize) -> Screen {
    let mut lines: Vec<Line> = Vec::new();
    let mut clicks: HashMap<usize, Click> = HashMap::new();
    if let Some(err) = &model.error {
        lines.push(vec![(fit_end(&format!("! {err}"), cols), Tone::Error)]);
    }
    let Some(session) = &model.session else {
        lines.push(vec![(fit_end("no agent in this tab", cols), Tone::Dim)]);
        return clip(Screen { lines, clicks }, cols, rows);
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
    clip(Screen { lines, clicks }, cols, rows)
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

/// Keeps the screen's worth of rows, replacing the overflow with a count; rows cut off lose
/// their click targets.
fn clip(mut screen: Screen, cols: usize, rows: usize) -> Screen {
    if screen.lines.len() > rows && rows > 0 {
        let hidden = screen.lines.len() - (rows - 1);
        screen.lines.truncate(rows - 1);
        screen.clicks.retain(|row, _| *row < rows - 1);
        screen.lines.push(vec![(fit_end(&format!("… {hidden} more lines"), cols), Tone::Dim)]);
    }
    screen
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

fn draw(lines: &[Line]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for (row, line) in lines.iter().enumerate() {
        queue!(out, cursor::MoveTo(0, row as u16))?;
        for (text, tone) in line {
            style(&mut out, *tone)?;
            queue!(out, Print(text), SetAttribute(Attribute::Reset), ResetColor)?;
        }
        queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    }
    queue!(out, cursor::MoveTo(0, lines.len() as u16), terminal::Clear(terminal::ClearType::FromCursorDown))?;
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
    fn click_targets_follow_rendered_rows_and_clipping() {
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
        let full = render(&model, 40, 20);
        assert_eq!(full.clicks.get(&3), Some(&Click::Pr { url: pr.url() }));
        assert_eq!(full.clicks.get(&4), Some(&Click::File { root: root.clone(), change: change(b" M", "a.rs") }));
        assert_eq!(full.clicks.get(&5), Some(&Click::File { root: root.clone(), change: change(b"??", "b.rs") }));
        assert_eq!(full.clicks.len(), 3);
        // Five rows: four content rows plus "… more"; the cut-off file rows must not stay clickable.
        let clipped = render(&model, 40, 5);
        assert_eq!(clipped.lines.len(), 5);
        assert_eq!(clipped.clicks.keys().copied().collect::<Vec<_>>(), [3]);
    }
}
