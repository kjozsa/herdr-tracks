//! Laying out and drawing the sidebar: repo headers, file and pull-request rows, the visible
//! window of a long list, and terminal output.

use super::collect::{is_dismissed, visible_prs};
use super::{Click, Key, Menu, Model, Target};
use crate::git::{LineStat, RepoStatus};
use crate::github::{self, Checks, PrState};
use crate::state::PrRecord;
use anyhow::Result;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    /// Clickable text (an OSC 8 hyperlink); underlined so it reads as a link.
    Link,
    /// An entry of the right-click menu.
    Menu,
    /// A web link in the links block.
    Url,
}

pub(super) type Line = Vec<(String, Tone)>;

/// All rendered rows plus, by row index, what clicking and right-clicking each row acts on.
pub(super) struct Screen {
    pub(super) lines: Vec<Line>,
    pub(super) clicks: HashMap<usize, Click>,
    pub(super) menus: HashMap<usize, Target>,
}

impl Screen {
    /// Selectable rows in display order.
    pub(super) fn rows(&self) -> Vec<(usize, &Click)> {
        let mut rows: Vec<_> = self.clicks.iter().map(|(row, click)| (*row, click)).collect();
        rows.sort_by_key(|(row, _)| *row);
        rows
    }

    pub(super) fn row_of(&self, key: Option<&Key>) -> Option<usize> {
        let key = key?;
        self.clicks.iter().find(|(_, click)| click.key() == *key).map(|(row, _)| *row)
    }
}

/// The part of a [`Screen`] that fits the pane; clicks and menus keyed by on-screen row.
pub(super) struct View {
    pub(super) lines: Vec<Line>,
    pub(super) clicks: HashMap<usize, Click>,
    pub(super) menus: HashMap<usize, Target>,
    /// The URL of each row of the pinned links block.
    pub(super) links: HashMap<usize, String>,
    /// The row of the links block's rule, when there is a block.
    pub(super) links_top: Option<usize>,
    pub(super) highlight: Option<usize>,
}

impl View {
    /// Pins `block` (see [`links`]) to the bottom of a `rows`-high pane, below the list.
    pub(super) fn pin(&mut self, block: Vec<(Line, Option<String>)>, rows: usize) {
        self.lines.resize(rows - block.len(), Vec::new());
        if !block.is_empty() {
            self.links_top = Some(self.lines.len());
        }
        for (line, url) in block {
            if let Some(url) = url {
                self.menus.insert(self.lines.len(), Target::Link(url.clone()));
                self.links.insert(self.lines.len(), url);
            }
            self.lines.push(line);
        }
    }

    /// The URL a row stands for: a link, or a pull request.
    pub(super) fn url_at(&self, row: usize) -> Option<&str> {
        match self.clicks.get(&row) {
            Some(Click::Pr { url }) => Some(url),
            _ => self.links.get(&row).map(String::as_str),
        }
    }

    /// Draws the open menu over the rows it occupies.
    pub(super) fn overlay(&mut self, menu: &Menu, cols: usize, rows: usize) {
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

pub(super) fn render(model: &Model, cols: usize) -> Screen {
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
    // Newest first within each repo: GitHub numbers pull requests in creation order.
    let mut prs: Vec<&PrRecord> = visible_prs(&session.repos).collect();
    prs.sort_by_key(|p| (p.pr.slug(), std::cmp::Reverse(p.pr.number)));
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

/// The links block pinned to the bottom of the pane: a rule, then the chat's links, newest
/// first, from `scroll` on as far as `max` rows allow, each with its URL. When not all fit, the
/// rule counts them: `─ links 4–12 of 30`. `scroll` is clamped to the list. Pull requests
/// already listed above are left out. Empty when there is nothing to show or no room.
pub(super) fn links(model: &Model, cols: usize, max: usize, scroll: &mut usize) -> Vec<(Line, Option<String>)> {
    let Some(session) = &model.session else { return Vec::new() };
    let listed = |url: &str| github::pr_urls(url).iter().any(|pr| session.repos.prs.iter().any(|p| p.pr.same(pr)));
    let dismissed = |url: &str| session.repos.dismissed_links.iter().any(|d| d.url == url);
    let urls: Vec<&String> = session.links.iter().filter(|url| !listed(url) && !dismissed(url)).collect();
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
    let rule = format!("{title}{}", "─".repeat(cols.saturating_sub(title.width())));
    let mut block = vec![(vec![(fit_end(&rule, cols), Tone::Dim)], None)];
    for url in shown {
        let text = link_text(url, cols.saturating_sub(1));
        block.push((vec![(" ".into(), Tone::Plain), (hyperlink(url, &text), Tone::Url)], Some((*url).clone())));
    }
    block
}

/// A URL as shown in `max` columns: without scheme and `www.`; when too long, the host and the
/// end of the path, which tells similar links apart: `raw.githubusercontent.com…/src/dock.rs`.
fn link_text(url: &str, max: usize) -> String {
    let shown = url.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.");
    let (host, path) = shown.split_once('/').unwrap_or((shown, ""));
    if shown.width() <= max || host.width() + 10 > max {
        return fit_end(shown, max);
    }
    let tail = fit_start(&format!("/{path}"), max - host.width());
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
pub(super) fn window(screen: &Screen, cols: usize, rows: usize, scroll: &mut usize, selected: Option<usize>) -> View {
    let total = screen.lines.len();
    if total <= rows {
        *scroll = 0;
        return View {
            lines: screen.lines.clone(),
            clicks: screen.clicks.clone(),
            menus: screen.menus.clone(),
            links: HashMap::new(),
            links_top: None,
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
        links: HashMap::new(),
        links_top: None,
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

/// Draws the view; the link or pull request under the pointer (`hover`, see [`View::url_at`])
/// is underlined and bold.
pub(super) fn draw(view: &View, cols: usize, hover: Option<&str>) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for (row, line) in view.lines.iter().enumerate() {
        let highlighted = view.highlight == Some(row);
        let hovered = hover.is_some() && view.url_at(row) == hover;
        queue!(out, cursor::MoveTo(0, row as u16))?;
        for (text, tone) in line {
            style(&mut out, *tone)?;
            if highlighted {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            if hovered && matches!(tone, Tone::Url | Tone::Link) {
                queue!(out, SetAttribute(Attribute::Underlined), SetAttribute(Attribute::Bold))?;
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
    // Below a full pane there is nothing to clear: the cursor would stop on the last row and the
    // erase would wipe it.
    if view.lines.len() < usize::from(terminal::size()?.1) {
        queue!(out, cursor::MoveTo(0, view.lines.len() as u16), terminal::Clear(terminal::ClearType::FromCursorDown))?;
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
