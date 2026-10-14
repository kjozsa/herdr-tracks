//! Laying out and drawing the sidebar: repo headers, file and pull-request rows, the visible
//! window of a long list, and terminal output.

use super::{Click, Key, Menu, Model, Target};
use crate::ansi::{self, fit_end, fit_start};
use crate::diff::CommitAt;
use crate::git::{Commit, LineStat, RepoStatus};
use crate::github::{self, Checks, PrCommit, PrState, PrView};
use crate::state::PrRecord;
use anyhow::Result;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Commits listed per repo; older ones are counted on one more row.
const COMMITS_SHOWN: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
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
    /// Clickable text, drawn as an OSC 8 hyperlink to the row's URL; underlined so it reads as
    /// a link.
    Link,
    /// An entry of the right-click menu.
    Menu,
    /// A web link in the links block, drawn as an OSC 8 hyperlink to the row's URL.
    Url,
}

/// A row's text: plain pieces, each in its tone.
pub(super) type Line = Vec<(String, Tone)>;

/// One row and what it acts on.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Row {
    pub(super) line: Line,
    /// What selecting or clicking it shows.
    pub(super) click: Option<Click>,
    /// What its right-click menu acts on.
    pub(super) menu: Option<Target>,
    /// The web link of a links block row.
    pub(super) link: Option<String>,
}

impl Row {
    fn new(line: Line) -> Self {
        Row { line, ..Row::default() }
    }

    /// The URL it stands for: a link, or a pull request.
    pub(super) fn url(&self) -> Option<&str> {
        match &self.click {
            Some(Click::Pr { url }) => Some(url),
            _ => self.link.as_deref(),
        }
    }
}

/// A row of one piece of text, cut to fit.
fn fitted(text: &str, tone: Tone, cols: usize) -> Row {
    Row::new(vec![(fit_end(text, cols), tone)])
}

/// All rendered rows.
pub(super) struct Screen {
    pub(super) rows: Vec<Row>,
}

impl Screen {
    /// Selectable rows in display order, with their row index.
    pub(super) fn clicks(&self) -> impl Iterator<Item = (usize, &Click)> {
        self.rows.iter().enumerate().filter_map(|(at, row)| Some((at, row.click.as_ref()?)))
    }

    pub(super) fn row_of(&self, key: Option<&Key>) -> Option<usize> {
        let key = key?;
        self.clicks().find(|(_, click)| click.key() == *key).map(|(at, _)| at)
    }
}

/// The rows of a [`Screen`] that fit the pane, and the links block pinned below them.
pub(super) struct View {
    pub(super) rows: Vec<Row>,
    /// The row of the links block's rule, when there is a block.
    pub(super) links_top: Option<usize>,
    pub(super) highlight: Option<usize>,
}

impl View {
    /// Pins `block` (see [`links`]) to the bottom of a `rows`-high pane, below the list.
    pub(super) fn pin(&mut self, block: Vec<Row>, rows: usize) {
        self.rows.resize_with(rows - block.len(), Row::default);
        if !block.is_empty() {
            self.links_top = Some(self.rows.len());
        }
        self.rows.extend(block);
    }

    /// The URL a click at `col` of `row` would open: a link, or a pull request's number and
    /// status (not its title).
    pub(super) fn pointer_url(&self, row: usize, col: usize) -> Option<&str> {
        let row = self.rows.get(row)?;
        match &row.click {
            Some(Click::Pr { url }) => (zone(&row.line, col) == Zone::Link).then_some(url.as_str()),
            _ => row.link.as_deref(),
        }
    }

    /// Draws the open menu over the rows it occupies.
    pub(super) fn overlay(&mut self, menu: &Menu, cols: usize, rows: usize) {
        for (row, _, label) in menu.placed(rows) {
            if self.rows.len() <= row {
                self.rows.resize_with(row + 1, Row::default);
            }
            let text = fit_end(&format!(" ▸ {label}"), cols);
            let pad = " ".repeat(cols.saturating_sub(ansi::width(&text)));
            self.rows[row].line = vec![(format!("{text}{pad}"), Tone::Menu)];
        }
    }
}

pub(super) fn render(model: &Model, cols: usize) -> Screen {
    let mut rows: Vec<Row> = Vec::new();
    if let Some(err) = &model.error {
        rows.push(fitted(&format!("! {err}"), Tone::Error, cols));
    }
    let Some(session) = &model.session else {
        rows.push(fitted("no agent in this tab", Tone::Dim, cols));
        return Screen { rows };
    };
    if !session.title.is_empty() {
        rows.push(fitted(&session.title, Tone::Dim, cols));
    }
    // Repos the chat changed or has pull requests in (`refresh_git` skips deleted ones), then
    // PRs whose repo has no touched checkout. Whatever the user dismissed stays hidden.
    // Without a checkout, PRs go newest first by number: GitHub numbers them in creation order.
    let saved = &session.saved;
    let mut prs: Vec<&PrRecord> = saved.visible_prs().collect();
    prs.sort_by_key(|p| (p.pr.slug(), std::cmp::Reverse(p.pr.number)));
    let has_prs = |root: &PathBuf| prs.iter().any(|p| p.root.as_ref() == Some(root));
    let shown: Vec<&PathBuf> = saved
        .repos
        .iter()
        .filter(|r| (r.changed || has_prs(&r.root)) && model.statuses.contains_key(&r.root))
        .filter(|r| !saved.repo_dismissed(&r.root))
        .map(|r| &r.root)
        .collect();
    let orphans: Vec<&PrRecord> = prs.iter().copied().filter(|p| p.root.is_none()).collect();
    if shown.is_empty() && orphans.is_empty() {
        rows.push(fitted("no changes yet", Tone::Dim, cols));
    }
    for root in shown {
        if !rows.is_empty() {
            rows.push(Row::default());
        }
        // The header shows all uncommitted changes at once.
        let header = repo_header(root, model.statuses.get(root), cols);
        rows.push(Row { menu: Some(Target::Repo(root.clone())), click: Some(Click::Repo { root: root.clone() }), ..Row::new(header) });
        // Uncommitted work first: it is the freshest.
        match model.statuses.get(root) {
            None => {}
            Some(Err(e)) => rows.push(fitted(&format!(" ! {e}"), Tone::Error, cols)),
            Some(Ok(s)) => {
                // Uncommitted changes (two-letter `git status` codes); committed work is listed
                // as commits, not files.
                for change in &s.changes {
                    let code = String::from_utf8_lossy(&change.code).into_owned();
                    let line = file_row(" ", (code, change_tone(change.code)), &change.display(), change.stat, cols);
                    rows.push(Row { click: Some(Click::File { root: root.clone(), change: change.clone() }), ..Row::new(line) });
                }
            }
        }
        let entries = history(model, root, prs.iter().copied().filter(|p| p.root.as_ref() == Some(root)));
        let repo = model.remotes.get(root).and_then(|slugs| slugs.first()).cloned();
        let mut loose = 0;
        for entry in &entries {
            match entry {
                Entry::Pr { pr, .. } => push_pr(&mut rows, model, pr, cols),
                Entry::Commit(commit) => {
                    loose += 1;
                    if loose <= COMMITS_SHOWN {
                        let at = CommitAt { hash: commit.hash.clone(), root: Some(root.clone()), repo: repo.clone() };
                        let commit = CommitLine { at, under: None, subject: &commit.subject, chat: true };
                        push_commit(&mut rows, model, commit, " ", cols);
                    } else if loose == COMMITS_SHOWN + 1 {
                        // Older loose commits are counted where the first of them would be.
                        let more = entries.iter().filter(|e| matches!(e, Entry::Commit(_))).count() - COMMITS_SHOWN;
                        rows.push(fitted(&earlier(" ", more), Tone::Dim, cols));
                    }
                }
            }
        }
    }
    for pr in orphans {
        if !rows.is_empty() {
            rows.push(Row::default());
        }
        let heading = fitted(&format!("{}/{}", pr.pr.owner, pr.pr.repo), Tone::Repo, cols);
        rows.push(Row { menu: Some(Target::Pr(pr.pr.clone())), ..heading });
        push_pr(&mut rows, model, pr, cols);
    }
    Screen { rows }
}

/// What a repo's history lists: a pull request, or a commit of the chat's that no listed pull
/// request holds.
enum Entry<'a> {
    Pr { pr: &'a PrRecord, time: Option<i64> },
    Commit(&'a Commit),
}

impl Entry<'_> {
    /// When it last moved: a commit's time; a PR's newest held commit or GitHub activity.
    fn time(&self) -> Option<i64> {
        match self {
            Entry::Pr { time, .. } => *time,
            Entry::Commit(commit) => Some(commit.time),
        }
    }
}

/// The repo's pull requests and the chat's commits there in one list, freshest first. A PR
/// is as fresh as the newest of the chat's commits it holds, or its last activity on GitHub;
/// PRs with neither (not fetched yet) go last.
fn history<'a>(model: &'a Model, root: &Path, prs: impl Iterator<Item = &'a PrRecord>) -> Vec<Entry<'a>> {
    let mut loose: Vec<&Commit> = model.commits.get(root).into_iter().flatten().collect();
    let mut entries: Vec<Entry> = Vec::new();
    for pr in prs {
        let view = model.pr_views.get(&pr.pr.url());
        let (held, rest): (Vec<&Commit>, Vec<&Commit>) = loose.into_iter().partition(|c| view.is_some_and(|v| v.holds(&c.hash)));
        loose = rest;
        let time = held.iter().map(|c| c.time).chain(view.and_then(|v| v.updated)).max();
        entries.push(Entry::Pr { pr, time });
    }
    entries.extend(loose.into_iter().map(Entry::Commit));
    // Stable: among equals (PRs without a time), the newer number stays first.
    entries.sort_by_key(|e| std::cmp::Reverse(e.time()));
    entries
}

fn earlier(indent: &str, more: usize) -> String {
    format!("{indent}  … {more} earlier commit{}", if more == 1 { "" } else { "s" })
}

/// A pull request row and, when expanded, its commits (the merge commit included), newest
/// first, [`COMMITS_SHOWN`] at most.
fn push_pr(rows: &mut Vec<Row>, model: &Model, pr: &PrRecord, cols: usize) {
    let url = pr.pr.url();
    let expanded = model.expanded.contains(&url);
    let view = model.pr_views.get(&url);
    let line = pr_line(pr.pr.number, view, model.pr_errors.get(&url), expanded, cols);
    rows.push(Row { click: Some(Click::Pr { url: url.clone() }), menu: Some(Target::Pr(pr.pr.clone())), ..Row::new(line) });
    if !expanded {
        return;
    }
    let Some(view) = view else {
        rows.push(Row::new(vec![("    …".into(), Tone::Dim)]));
        return;
    };
    let chat: Vec<&Commit> = pr.root.as_ref().and_then(|root| model.commits.get(root)).into_iter().flatten().collect();
    let mut commits: Vec<&PrCommit> = view.merge.iter().chain(&view.commits).collect();
    commits.sort_by_key(|c| std::cmp::Reverse(c.time));
    for commit in commits.iter().take(COMMITS_SHOWN) {
        let mine = chat.iter().find(|c| c.hash == commit.oid);
        // GitHub names no subject for the merge commit; the checkout may know it.
        let subject = commit.subject.as_deref().or(mine.map(|c| c.subject.as_str())).unwrap_or("merge commit");
        let at = CommitAt { hash: commit.oid.clone(), root: pr.root.clone(), repo: Some(pr.pr.slug()) };
        let commit = CommitLine { at, under: Some(url.clone()), subject, chat: mine.is_some() };
        push_commit(rows, model, commit, "   ", cols);
    }
    if commits.len() > COMMITS_SHOWN {
        rows.push(fitted(&earlier("   ", commits.len() - COMMITS_SHOWN), Tone::Dim, cols));
    }
}

/// A commit as listed: where to read it, the pull request it is listed in, its subject, and
/// whether the chat made it.
struct CommitLine<'a> {
    at: CommitAt,
    under: Option<String>,
    subject: &'a str,
    chat: bool,
}

/// A commit row and, when expanded, one row per file it changes.
fn push_commit(rows: &mut Vec<Row>, model: &Model, commit: CommitLine, indent: &str, cols: usize) {
    let expanded = model.expanded.contains(&commit.at.hash);
    let line = commit_row(indent, &commit, expanded, cols);
    let CommitLine { at, under, .. } = commit;
    rows.push(Row { click: Some(Click::Commit { commit: at.clone(), under }), ..Row::new(line) });
    if !expanded {
        return;
    }
    let indent = format!("{indent}   ");
    match model.commit_files.get(&at.hash) {
        None => rows.push(Row::new(vec![(format!("{indent}…"), Tone::Dim)])),
        Some(Err(e)) => rows.push(fitted(&format!("{indent}! {e}"), Tone::Error, cols)),
        Some(Ok(files)) => {
            for file in files {
                let tone = match file.code {
                    'A' => Tone::Good,
                    'D' => Tone::Bad,
                    _ => Tone::Waiting,
                };
                let line = file_row(&indent, (file.code.to_string(), tone), &file.path, Some(file.stat), cols);
                rows.push(Row { click: Some(Click::CommitFile { commit: at.clone(), path: file.path.clone() }), ..Row::new(line) });
            }
        }
    }
}

/// `<indent>▸ 225ac66 merge the feature…`: expand marker (green for the chat's own commits),
/// short hash, subject cut to fit (dim for commits the chat did not make).
fn commit_row(indent: &str, commit: &CommitLine, expanded: bool, cols: usize) -> Line {
    let short: String = commit.at.hash.chars().take(7).collect();
    let prefix = format!("{indent}▸ {short} ");
    let subject = fit_end(commit.subject, cols.saturating_sub(ansi::width(&prefix)));
    let (marker, text) = if commit.chat { (Tone::Good, Tone::Plain) } else { (Tone::Dim, Tone::Dim) };
    vec![
        (indent.to_string(), Tone::Plain),
        ((if expanded { "▾" } else { "▸" }).into(), marker),
        (" ".into(), Tone::Plain),
        (short, Tone::Dim),
        (" ".into(), Tone::Plain),
        (subject, text),
    ]
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
    let stat_width = stat.iter().map(|(t, _)| ansi::width(t) + 1).sum::<usize>(); // each with a leading space
    let prefix = ansi::width(indent) + ansi::width(&code) + 1;
    let path = fit_start(path, cols.saturating_sub(prefix + stat_width));
    let mut line = vec![(indent.to_string(), Tone::Plain), (code, tone), (" ".into(), Tone::Plain)];
    let gap = cols.saturating_sub(prefix + ansi::width(&path) + stat_width);
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

/// ` ▸ PR#121 open ✓ fix the login`: expand marker, number (a link to the PR), state, CI
/// rollup (omitted when the PR has no checks), title cut to fit. Before the first fetch, `…` or
/// the fetch's error instead of state and title.
fn pr_line(number: u32, view: Option<&PrView>, error: Option<&String>, expanded: bool, cols: usize) -> Line {
    let number = format!("PR#{number}");
    let room = cols.saturating_sub(ansi::width(&number) + 4);
    let marker = if expanded { " ▾ " } else { " ▸ " };
    let mut line = vec![(marker.into(), Tone::Dim), (number, Tone::Link)];
    let Some(view) = view else {
        match error {
            Some(err) => line.push((format!(" {}", fit_end(&format!("! {err}"), room)), Tone::Error)),
            None => line.push((" …".into(), Tone::Dim)),
        }
        return line;
    };
    let (state, tone) = match view.status.state {
        PrState::Open => ("open", Tone::Good),
        PrState::Draft => ("draft", Tone::Dim),
        PrState::Merged => ("merged", Tone::Merged),
        PrState::Closed => ("closed", Tone::Bad),
    };
    line.push((format!(" {state}"), tone));
    if let Some(checks) = view.status.checks {
        let (mark, tone) = match checks {
            Checks::Passing => ("✓", Tone::Good),
            Checks::Failing => ("✗", Tone::Bad),
            Checks::Pending => ("◔", Tone::Waiting),
        };
        line.push((format!(" {mark}"), tone));
    }
    let used: usize = line.iter().map(|(t, _)| ansi::width(t)).sum();
    let room = cols.saturating_sub(used + 1);
    if room > 0 {
        line.push((format!(" {}", fit_end(&view.title, room)), Tone::Plain));
    }
    line
}

/// The links block pinned to the bottom of the pane: a rule, then the chat's links, newest
/// first, from `scroll` on as far as `max` rows allow. When not all fit, the rule counts them:
/// `─ links 4–12 of 30`. `scroll` is clamped to the list. Pull requests already listed above
/// are left out. Empty when there is nothing to show or no room.
pub(super) fn links(model: &Model, cols: usize, max: usize, scroll: &mut usize) -> Vec<Row> {
    let Some(session) = &model.session else { return Vec::new() };
    let saved = &session.saved;
    let listed = |url: &str| github::pr_urls(url).iter().any(|pr| saved.prs.iter().any(|p| p.pr.same(pr)));
    let urls: Vec<&String> = session.links.iter().filter(|url| !listed(url) && !saved.link_dismissed(url)).collect();
    let room = max.saturating_sub(1);
    if urls.is_empty() || room == 0 {
        return Vec::new();
    }
    *scroll = (*scroll).min(urls.len().saturating_sub(room));
    let shown = &urls[*scroll..urls.len().min(*scroll + room)];
    let title = match (*scroll + 1, *scroll + shown.len()) {
        _ if shown.len() == urls.len() => "─ links ".to_string(),
        (first, last) if first == last => format!("─ links {first} of {} ", urls.len()),
        (first, last) => format!("─ links {first}–{last} of {} ", urls.len()),
    };
    let rule = format!("{title}{}", "─".repeat(cols.saturating_sub(ansi::width(&title))));
    let mut block = vec![Row { menu: Some(Target::Links), ..fitted(&rule, Tone::Dim, cols) }];
    for url in shown {
        let line = vec![(" ".into(), Tone::Plain), (link_text(url, cols.saturating_sub(1)), Tone::Url)];
        block.push(Row { menu: Some(Target::Link((*url).clone())), link: Some((*url).clone()), ..Row::new(line) });
    }
    block
}

/// A URL as shown in `max` columns: without scheme and `www.`; when too long, the host and the
/// end of the path, which tells similar links apart: `raw.githubusercontent.com…/src/dock.rs`.
fn link_text(url: &str, max: usize) -> String {
    let shown = url.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.");
    let (host, path) = shown.split_once('/').unwrap_or((shown, ""));
    if ansi::width(shown) <= max || ansi::width(host) + 10 > max {
        return fit_end(shown, max);
    }
    let tail = fit_start(&format!("/{path}"), max - ansi::width(host));
    // Start the tail at a path segment rather than mid-name, when there is one to start at.
    match tail.find('/') {
        Some(at) => format!("{host}…{}", &tail[at..]),
        None => format!("{host}{tail}"),
    }
}

/// `name branch ↑1 ↓2`: a long branch name is shortened, never the ahead/behind counts.
pub(super) fn repo_header(root: &Path, status: Option<&Result<RepoStatus, String>>, cols: usize) -> Line {
    let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
    let name = fit_end(&name, cols);
    let Some(Ok(s)) = status else { return vec![(name, Tone::Repo)] };
    let tracking: String = [(s.ahead, '↑'), (s.behind, '↓')]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, arrow)| format!(" {arrow}{n}"))
        .collect();
    let room = cols.saturating_sub(ansi::width(&name) + 1 + ansi::width(&tracking));
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
/// left below, the last row counts them instead.
pub(super) fn window(screen: &Screen, cols: usize, rows: usize, scroll: &mut usize, selected: Option<usize>) -> View {
    let total = screen.rows.len();
    if total <= rows {
        *scroll = 0;
        return View { rows: screen.rows.clone(), links_top: None, highlight: selected };
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
    let mut shown = screen.rows[*scroll..end].to_vec();
    if end < total {
        shown.push(fitted(&format!("… {} more lines", total - end), Tone::Dim, cols));
    }
    let highlight = selected.filter(|row| (*scroll..end).contains(row)).map(|row| row - *scroll);
    View { rows: shown, links_top: None, highlight }
}

/// The screen column of a row's expand marker (`▸`/`▾`), if it has one.
pub(super) fn marker_column(line: &Line) -> Option<usize> {
    let mut col = 0;
    for (text, _) in line {
        if let Some(at) = text.find(['▸', '▾']) {
            return Some(col + ansi::width(&text[..at]));
        }
        col += ansi::width(text);
    }
    None
}

/// The part of a row a click lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Zone {
    /// The expand marker, give or take a column.
    Marker,
    /// A pull request's number and status, up to its title.
    Link,
    /// Anything else.
    Body,
}

pub(super) fn zone(line: &Line, col: usize) -> Zone {
    if marker_column(line).is_some_and(|m| col.abs_diff(m) <= 1) {
        return Zone::Marker;
    }
    let (mut at, mut link, mut title) = (0, None, None);
    for (text, tone) in line {
        match tone {
            Tone::Link if link.is_none() => link = Some(at),
            Tone::Plain if link.is_some() && title.is_none() => title = Some(at),
            _ => {}
        }
        at += ansi::width(text);
    }
    match link {
        Some(start) if col >= start && col < title.unwrap_or(usize::MAX) => Zone::Link,
        _ => Zone::Body,
    }
}

/// Draws the view. Link and URL text is an OSC 8 hyperlink to the row's URL (herdr opens it
/// on Ctrl+click); under the pointer (`hover`, see [`Row::url`]) it is underlined and bold.
pub(super) fn draw(view: &View, cols: usize, hover: Option<&str>) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for (at, row) in view.rows.iter().enumerate() {
        let highlighted = view.highlight == Some(at);
        let url = row.url();
        let hovered = hover.is_some() && url == hover;
        queue!(out, cursor::MoveTo(0, at as u16))?;
        for (text, tone) in &row.line {
            style(&mut out, *tone)?;
            if highlighted {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            match url.filter(|_| matches!(tone, Tone::Url | Tone::Link)) {
                Some(url) => {
                    if hovered {
                        queue!(out, SetAttribute(Attribute::Underlined), SetAttribute(Attribute::Bold))?;
                    }
                    queue!(out, Print("\x1b]8;;"), Print(url), Print("\x1b\\"), Print(text), Print("\x1b]8;;\x1b\\"))?;
                }
                None => queue!(out, Print(text))?,
            }
            queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
        }
        let used: usize = row.line.iter().map(|(text, _)| ansi::width(text)).sum();
        if highlighted {
            let pad = " ".repeat(cols.saturating_sub(used));
            queue!(out, SetAttribute(Attribute::Reverse), Print(pad), SetAttribute(Attribute::Reset))?;
        } else if used < cols {
            // Not on full rows: after the last column the cursor waits to wrap, and an erase
            // there wipes that last cell.
            queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
        }
    }
    // Below a full pane there is nothing to clear: the cursor would stop on the last row and the
    // erase would wipe it.
    if view.rows.len() < usize::from(terminal::size()?.1) {
        queue!(out, cursor::MoveTo(0, view.rows.len() as u16), terminal::Clear(terminal::ClearType::FromCursorDown))?;
    }
    out.flush()?;
    Ok(())
}

fn style(out: &mut impl Write, tone: Tone) -> Result<()> {
    match tone {
        Tone::Plain => {}
        Tone::Link => queue!(out, SetAttribute(Attribute::Underlined))?,
        Tone::Menu => queue!(out, SetAttribute(Attribute::Reverse), SetAttribute(Attribute::Bold))?,
        Tone::Url => queue!(out, SetForegroundColor(Color::Blue))?,
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
    use crate::diff::DiffRequest;
    use crate::git::{Change, FileChange};
    use crate::sidebar::fixture::{clean, fixture, gh, pr, root, session, text, view};

    /// Selectable rows under the repo headers.
    fn keys(screen: &Screen) -> Vec<Key> {
        screen.clicks().map(|(_, click)| click.key()).filter(|key| !matches!(key, Key::Repo(_))).collect()
    }

    fn row<'a>(screen: &'a Screen, key: &Key) -> &'a Row {
        &screen.rows[screen.row_of(Some(key)).expect("row is shown")]
    }

    /// The chat's commits `h0`..`h<n-1>` in `/r/e2e`, committed at their number.
    fn commits(n: i64) -> Vec<Commit> {
        (0..n).rev().map(|n| Commit { hash: format!("h{n}"), time: n, subject: format!("commit {n}") }).collect()
    }

    fn at(hash: &str, repo: Option<&str>) -> CommitAt {
        CommitAt { hash: hash.into(), root: Some(root("e2e")), repo: repo.map(String::from) }
    }

    #[test]
    fn long_branch_names_give_way_to_the_ahead_behind_counts() {
        let status = RepoStatus { branch: "nightly-maintenance-2026-09-29".into(), ahead: 1, behind: 2, ..clean() };
        let header = text(&repo_header(&root("e2e"), Some(&Ok(status)), 30));
        assert_eq!(header, "e2e nightly-maintenance… ↑1 ↓2");
        assert_eq!(ansi::width(&header), 30);
    }

    #[test]
    fn the_header_shows_all_uncommitted_changes_and_only_those_are_file_rows() {
        let mut model = fixture(&["e2e"], &[]);
        let changes = vec![Change { code: *b" M", path: "a.py".into(), orig: None, stat: None }];
        model.statuses.insert(root("e2e"), Ok(RepoStatus { changes, ahead: 2, ..clean() }));
        model.commits.insert(root("e2e"), commits(2));
        let screen = render(&model, 40);
        let header = row(&screen, &Key::Repo(root("e2e"))).click.as_ref().unwrap();
        assert_eq!(header.diff_request(), DiffRequest::Worktree { root: root("e2e") });
        assert_eq!(keys(&screen), [Key::File(root("e2e"), "a.py".into()), Key::Commit("h1".into()), Key::Commit("h0".into())]);
    }

    #[test]
    fn the_chat_s_commits_show_newest_first_and_count_the_rest() {
        let mut model = fixture(&["e2e"], &[]);
        model.commits.insert(root("e2e"), commits(12));
        let screen = render(&model, 40);
        assert_eq!(keys(&screen), (2..12).rev().map(|n| Key::Commit(format!("h{n}"))).collect::<Vec<_>>());
        let last = screen.rows.last().unwrap();
        assert!(last.click.is_none() && text(&last.line).contains(" 2 earlier"), "{:?}", last.line);
        let newest = row(&screen, &Key::Commit("h11".into())).click.as_ref().unwrap();
        assert_eq!(newest.diff_request(), DiffRequest::Commit { commit: at("h11", None), path: None });
    }

    #[test]
    fn prs_and_commits_list_freshest_first() {
        // A PR is as fresh as the newest of the chat's commits it holds (or its last activity
        // on GitHub); a PR not fetched yet goes last.
        let mut model = fixture(&["e2e"], &[pr("e2e", 5), pr("e2e", 6)]);
        model.commits.insert(root("e2e"), commits(12));
        let held = vec![gh("h3", "gh 3", 3), gh("h7", "gh 7", 7), gh("x1", "someone else's", 8)];
        model.pr_views.insert(pr("e2e", 5).url(), view(PrState::Open, held));
        let commits = |ns: &[i64]| ns.iter().map(|n| Key::Commit(format!("h{n}"))).collect::<Vec<_>>();
        let prs = |n| vec![Key::Pr(pr("e2e", n).url())];
        let expected = [commits(&[11, 10, 9, 8]), prs(5), commits(&[6, 5, 4, 2, 1, 0]), prs(6)].concat();
        assert_eq!(keys(&render(&model, 40)), expected);
    }

    #[test]
    fn pull_requests_list_newest_first_whatever_order_the_chat_named_them() {
        let model = fixture(&["kil"], &[840, 843, 9, 841].map(|n| pr("kil", n)));
        let expected: Vec<Key> = [843, 841, 840, 9].iter().map(|n| Key::Pr(pr("kil", *n).url())).collect();
        assert_eq!(keys(&render(&model, 40)), expected);
    }

    #[test]
    fn pr_rows_show_the_fetched_state_and_the_title_cut_to_fit() {
        let mut model = fixture(&["e2e"], &[pr("e2e", 5), pr("e2e", 6), pr("e2e", 7)]);
        model.pr_views.insert(pr("e2e", 5).url(), view(PrState::Open, Vec::new()));
        model.pr_errors.insert(pr("e2e", 7).url(), "gh: offline".into());
        let screen = render(&model, 40);
        let shown = |number| text(&row(&screen, &Key::Pr(pr("e2e", number).url())).line);
        assert_eq!(ansi::width(&shown(5)), 40);
        assert!(shown(5).ends_with("PR#5 open lodge every beneficial own…"), "{:?}", shown(5));
        assert!(shown(6).ends_with("PR#6 …"), "not fetched yet: {:?}", shown(6));
        assert!(shown(7).ends_with("PR#7 ! gh: offline"), "{:?}", shown(7));
    }

    #[test]
    fn pr_rows_split_into_marker_link_and_title() {
        let mut model = fixture(&["e2e"], &[pr("e2e", 5)]);
        model.pr_views.insert(pr("e2e", 5).url(), view(PrState::Open, Vec::new()));
        let screen = render(&model, 40);
        let line = &row(&screen, &Key::Pr(pr("e2e", 5).url())).line;
        let shown = text(line);
        let col = |piece: &str| ansi::width(&shown[..shown.find(piece).unwrap()]);
        // The marker expands, number and status open the browser, the title shows the diff.
        let zones = ["▸", "PR#5", "open", "lodge"].map(|piece| zone(line, col(piece)));
        assert_eq!(zones, [Zone::Marker, Zone::Link, Zone::Link, Zone::Body]);
    }

    #[test]
    fn expanded_prs_list_their_commits_and_expanded_commits_their_files() {
        let mut model = fixture(&["e2e"], &[pr("e2e", 5)]);
        model.commits.insert(root("e2e"), commits(12));
        let held = vec![gh("h3", "gh 3", 3), gh("h7", "gh 7", 7), gh("x1", "someone else's", 8)];
        model.pr_views.insert(pr("e2e", 5).url(), view(PrState::Open, held));
        model.expanded.insert(pr("e2e", 5).url());
        model.expanded.insert("h7".into());
        let stat = LineStat::Lines { added: 1, removed: 0 };
        model.commit_files.insert("h7".into(), Ok(vec![FileChange { code: 'M', path: "a.py".into(), stat }]));
        let screen = render(&model, 40);
        // All the PR's commits, newest first: the chat's own marked green, others dim; the
        // expanded commit's files under it, each with its diff in the commit.
        let pr_at = screen.row_of(Some(&Key::Pr(pr("e2e", 5).url()))).unwrap();
        let under = &screen.rows[pr_at + 1..pr_at + 5];
        let under_keys: Vec<Key> = under.iter().filter_map(|r| r.click.as_ref().map(Click::key)).collect();
        let commit = |hash: &str| Key::Commit(hash.into());
        assert_eq!(under_keys, [commit("x1"), commit("h7"), Key::CommitFile("h7".into(), "a.py".into()), commit("h3")]);
        assert_eq!(under[0].line[1].1, Tone::Dim, "not the chat's commit");
        assert_eq!(under[1].line[1].1, Tone::Good, "the chat's commit");
        assert_eq!(under[1].click, Some(Click::Commit { commit: at("h7", Some("o/e2e")), under: Some(pr("e2e", 5).url()) }));
        let file_diff = under[2].click.as_ref().unwrap().diff_request();
        assert_eq!(file_diff, DiffRequest::Commit { commit: at("h7", Some("o/e2e")), path: Some("a.py".into()) });
        // Only the expand markers toggle: a PR's, a commit's under it; file rows have none.
        assert_eq!(marker_column(&screen.rows[pr_at].line), Some(1));
        assert_eq!(marker_column(&under[1].line), Some(3));
        assert_eq!(marker_column(&under[2].line), None);
        assert!(text(&screen.rows[pr_at].line).contains('▾') && text(&under[1].line).contains('▾'), "expanded markers");
    }

    #[test]
    fn repo_and_pr_rows_carry_their_menu_targets() {
        let mut model = fixture(&["mine", "e2e"], &[pr("e2e", 36), pr("infra", 133)]);
        session(&mut model).saved.repos[1].changed = false;
        let screen = render(&model, 40);
        let menus: Vec<Target> = screen.rows.iter().filter_map(|r| r.menu.clone()).collect();
        // e2e is shown for its PR; #133 has no checkout and a heading of its own.
        let (mine, e2e, infra) = (Target::Repo(root("mine")), Target::Repo(root("e2e")), Target::Pr(pr("infra", 133)));
        assert_eq!(menus, [mine, e2e, Target::Pr(pr("e2e", 36)), infra.clone(), infra]);
    }

    #[test]
    fn links_are_pinned_to_the_bottom_newest_first_without_listed_prs() {
        let mut model = fixture(&[], &[pr("r", 7)]);
        let listed = format!("{}/files", pr("r", 7).url());
        session(&mut model).links = ["https://www.new.io/a", &listed, "https://mid.io/b", "https://old.io/c"].map(String::from).into();
        // Room for the rule and two links: the newest two that are not PR rows already.
        let block = links(&model, 30, 3, &mut 0);
        let urls: Vec<&str> = block.iter().filter_map(|r| r.link.as_deref()).collect();
        assert_eq!(urls, ["https://www.new.io/a", "https://mid.io/b"]);
        assert_eq!(text(&block[1].line), " new.io/a", "scheme and www. are not shown");
        let menus: Vec<_> = block.iter().map(|r| r.menu.clone()).collect();
        assert_eq!(menus, [Some(Target::Links), Some(Target::Link(urls[0].into())), Some(Target::Link(urls[1].into()))]);
        assert!(links(&model, 30, 1, &mut 0).is_empty(), "no room for a link: no block");

        // Pinned to the last rows of a 10-row pane, where a row stands for its link.
        let mut view = window(&render(&model, 30), 30, 10 - block.len(), &mut 0, None);
        view.pin(block, 10);
        assert_eq!(view.rows.len(), 10);
        assert_eq!(view.links_top, Some(7));
        assert_eq!([8, 9].map(|row| view.rows[row].url()), [Some("https://www.new.io/a"), Some("https://mid.io/b")]);

        // Scrolled past the end, the block shows the last links and the rule counts them.
        let mut scroll = 9;
        let block = links(&model, 30, 2, &mut scroll);
        assert_eq!(scroll, 2, "three links, room for one");
        assert!(text(&block[0].line).starts_with("─ links 3 of 3 ─"), "{:?}", block[0].line);
        assert_eq!(block[1].link.as_deref(), Some("https://old.io/c"));
    }

    #[test]
    fn the_window_keeps_rows_whole_and_scrolls_to_the_selection() {
        let mut model = fixture(&["app"], &[pr("app", 7)]);
        session(&mut model).title = "chat".into();
        let change = |path: &str| Change { code: *b" M", path: path.into(), orig: None, stat: None };
        model.statuses.insert(root("app"), Ok(RepoStatus { changes: vec![change("a.rs"), change("b.rs")], ..clean() }));
        // Title, blank, repo header, two files, the PR: six rows for five.
        let screen = render(&model, 40);
        let shown_keys = |view: &View| view.rows.iter().filter_map(|r| r.click.as_ref().map(Click::key)).collect::<Vec<_>>();
        let mut scroll = 0;
        let view = window(&screen, 40, 5, &mut scroll, None);
        assert_eq!(view.rows.len(), 5);
        assert_eq!(shown_keys(&view), [Key::Repo(root("app")), Key::File(root("app"), "a.rs".into())], "the last row counts the rest");

        // Selecting the last row scrolls it into view; the highlight follows the scroll.
        let pr_key = Key::Pr(pr("app", 7).url());
        let view = window(&screen, 40, 5, &mut scroll, screen.row_of(Some(&pr_key)));
        assert_eq!(scroll, 2);
        let highlight = view.highlight.unwrap();
        assert_eq!(view.rows[highlight].click.as_ref().map(Click::key), Some(pr_key));
        assert_eq!(shown_keys(&view)[0], Key::Repo(root("app")), "scrolled past the title and blank");
        // A pull request row stands for its URL; a repo header doesn't.
        assert_eq!(view.rows[highlight].url(), Some(pr("app", 7).url().as_str()));
        let header = screen.row_of(Some(&Key::Repo(root("app")))).unwrap();
        assert_eq!(screen.rows[header].url(), None);
    }

    #[test]
    fn file_rows_right_align_line_counts_and_shorten_the_path() {
        let stat = |added, removed| Some(LineStat::Lines { added, removed });
        let row = file_row(" ", (" M".into(), Tone::Unstaged), "src/a.rs", stat(12, 3), 24);
        assert_eq!(text(&row), format!("  M src/a.rs{}+12 -3", " ".repeat(6)));
        assert_eq!(ansi::width(&text(&row)), 24, "counts end at the right edge");
        assert!(row.contains(&("+12".into(), Tone::Good)) && row.contains(&("-3".into(), Tone::Bad)));
        // Long paths give way to the counts; zero counts and missing stats are left out.
        let row = file_row(" ", ("??".into(), Tone::Untracked), "deep/nested/dir/new_file.rs", stat(40, 0), 24);
        assert_eq!(text(&row), " ?? …dir/new_file.rs +40");
        assert_eq!(ansi::width(&text(&row)), 24);
        assert_eq!(text(&file_row(" ", ("A ".into(), Tone::Staged), "logo.png", Some(LineStat::Binary), 20)), " A  logo.png     bin");
        assert_eq!(text(&file_row(" ", (" M".into(), Tone::Unstaged), "x.rs", None, 20)), "  M x.rs");
    }

    #[test]
    fn long_links_keep_the_host_and_the_end_of_the_path() {
        let url = "https://raw.githubusercontent.com/jwanga/herdr-plugin-github-status/main/src/dock.rs";
        assert_eq!(link_text(url, 45), "raw.githubusercontent.com…/main/src/dock.rs");
        assert_eq!(link_text("https://www.herdr.dev/docs/plugins/", 45), "herdr.dev/docs/plugins/");
        // A host taking most of the width: plain cut at the end.
        assert_eq!(link_text("https://a-very-long-subdomain.example.com/x/y", 20), "a-very-long-subdoma…");
    }

    #[test]
    fn change_codes_map_to_git_colors() {
        assert_eq!(change_tone(*b"M "), Tone::Staged);
        assert_eq!(change_tone(*b" M"), Tone::Unstaged);
        assert_eq!(change_tone(*b"MM"), Tone::Unstaged);
        assert_eq!(change_tone(*b"??"), Tone::Untracked);
        assert_eq!(change_tone(*b"UU"), Tone::Conflict);
    }
}
