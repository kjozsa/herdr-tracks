//! The diff pane: a small viewer for the diff selected in the sidebar: a changed local file,
//! a commit (or one file of it), or a pull request. The sidebar names it in a request file and
//! the viewer redraws when that changes, so browsing swaps the content in place instead of
//! opening panes.

use super::term::Term;
use super::{ansi, herdr};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use crossterm::style::Print;
use crossterm::{cursor, queue, terminal};
use herdr_tracks::engine::git;
use herdr_tracks::engine::state::{self, keyed_file};
use herdr_tracks::engine::patch::{read_patch, DiffRequest, Patches};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Environment variable naming the request file the viewer follows.
pub const ENV: &str = "TRACKS_DIFF_FILE";
/// Environment variable naming the sidebar pane that opened the viewer.
pub const OWNER_ENV: &str = "TRACKS_DIFF_OWNER";
/// How often the viewer checks the request file.
const POLL: Duration = Duration::from_millis(50);
/// How often the viewer checks that its sidebar still exists.
const OWNER_CHECK: Duration = Duration::from_secs(1);
/// How often the viewer reads an uncommitted change or a pull request again, to follow edits,
/// commits and pushes (a pull request is fetched again once per `patch::FETCHED_TTL`).
const REFRESH: Duration = Duration::from_secs(1);
const WHEEL_LINES: usize = 3;

/// The request file of the diff pane belonging to `sidebar_pane`.
pub fn request_file(sidebar_pane: &str) -> PathBuf {
    keyed_file("diff", sidebar_pane)
}

/// Deletes the request files of sidebars other than `live_panes`.
pub fn prune_requests(live_panes: &[&str]) -> Result<usize> {
    let keep: Vec<PathBuf> = live_panes.iter().map(|p| request_file(p)).collect();
    state::prune_files(&state::state_dir().join("diff"), &keep, None)
}

pub fn run() -> Result<()> {
    let file = PathBuf::from(std::env::var(ENV).with_context(|| format!("{ENV} not set"))?);
    let owner = std::env::var(OWNER_ENV).ok();
    {
        let _term = Term::enter()?;
        view(&file, owner.as_deref())?;
    }
    if let Ok(me) = std::env::var("HERDR_PANE_ID") {
        herdr::pane_close(&me)?;
    }
    Ok(())
}

/// Shows the requested diff until `q`/`Esc`, re-rendering when the request or width changes,
/// or when the patch does (read again every [`REFRESH`]; the scroll stays). `w` wraps long lines, or cuts them at the edge again. Also ends once `owner` (the sidebar
/// pane) is gone: nothing would point the viewer anywhere.
fn view(file: &Path, owner: Option<&str>) -> Result<()> {
    let mut request_text = String::new();
    let mut rendered: Option<(String, u16)> = None;
    let mut request: Option<DiffRequest> = None;
    // The patch shown: `None` while there is no request.
    let mut patch: Option<Result<String, String>> = None;
    let mut next_refresh = Instant::now() + REFRESH;
    let mut lines: Vec<String> = Vec::new();
    let mut wrap = false;
    // The rows on screen (`lines`, wrapped or not) and, per row, its line in `lines`.
    let mut shown: Vec<String> = Vec::new();
    let mut origin: Vec<usize> = Vec::new();
    let mut patches = Patches::new();
    let mut scroll = 0usize;
    let mut dirty = true;
    let mut next_owner_check = Instant::now() + OWNER_CHECK;
    loop {
        let (cols, rows) = terminal::size()?;
        if let Ok(text) = std::fs::read_to_string(file)
            && text != request_text {
                request_text = text;
                scroll = 0;
            }
        let new = rendered.as_ref() != Some(&(request_text.clone(), cols));
        if new {
            request = serde_json::from_str(&request_text).ok();
            rendered = Some((request_text.clone(), cols));
        }
        let refresh = Instant::now() >= next_refresh && request.as_ref().is_some_and(DiffRequest::changes);
        if new || refresh {
            next_refresh = Instant::now() + REFRESH;
            let (fresh, repo) = match &request {
                Some(request) => {
                    let (fresh, repo) = read_patch(request, &mut patches);
                    (Some(fresh), repo)
                }
                None => (None, None),
            };
            if new || fresh != patch {
                lines = match &fresh {
                    Some(fresh) => diff_lines(fresh, repo, cols),
                    None => vec!["waiting for a file…".into()],
                };
                patch = fresh;
                (shown, origin) = layout(&lines, wrap, cols);
                dirty = true;
            }
        }
        // The last row holds the key hint; the diff scrolls above it.
        let page = usize::from(rows.saturating_sub(1)).max(1);
        scroll = scroll.min(shown.len().saturating_sub(page));
        if dirty {
            draw(&shown[scroll..], cols, rows, if wrap { "w unwrap · q close" } else { "w wrap · q close" })?;
            dirty = false;
        }
        if !event::poll(POLL)? {
            if Instant::now() >= next_owner_check {
                next_owner_check = Instant::now() + OWNER_CHECK;
                if owner.is_some_and(|pane| !herdr::pane_exists(pane)) {
                    return Ok(());
                }
            }
            continue;
        }
        dirty = true;
        match event::read()? {
            Event::Key(KeyEvent { code, .. }) => match code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => scroll += 1,
                KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => scroll += page,
                KeyCode::PageUp | KeyCode::Char('b') => scroll = scroll.saturating_sub(page),
                KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                KeyCode::End | KeyCode::Char('G') => scroll = usize::MAX,
                KeyCode::Char('w') => {
                    // Keep the line at the top of the screen there.
                    let top = origin.get(scroll).copied().unwrap_or(0);
                    wrap = !wrap;
                    (shown, origin) = layout(&lines, wrap, cols);
                    scroll = origin.iter().position(|&line| line == top).unwrap_or(0);
                }
                _ => dirty = false,
            },
            Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollDown, .. }) => scroll += WHEEL_LINES,
            Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollUp, .. }) => {
                scroll = scroll.saturating_sub(WHEEL_LINES)
            }
            _ => {}
        }
    }
}

/// The rows to show: each line whole (cut at the edge when drawn), or wrapped at `cols`; with
/// the index of the line each row comes from.
fn layout(lines: &[String], wrap: bool, cols: u16) -> (Vec<String>, Vec<usize>) {
    if !wrap {
        return (lines.to_vec(), (0..lines.len()).collect());
    }
    let mut shown = Vec::with_capacity(lines.len());
    let mut origin = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        for piece in ansi::wrap(line, usize::from(cols)) {
            shown.push(piece);
            origin.push(i);
        }
    }
    (shown, origin)
}

/// A patch from `repo` as ANSI lines, formatted by delta when that is the user's git pager.
fn diff_lines(patch: &Result<String, String>, repo: Option<&Path>, cols: u16) -> Vec<String> {
    let patch = match patch {
        Ok(patch) if patch.trim().is_empty() => return vec!["no changes".into()],
        Ok(patch) => patch,
        Err(e) => return vec![e.clone()],
    };
    let formatted = match delta_pager(repo) {
        Some(pager) => through_delta(&pager, patch, cols).unwrap_or_else(|| colorize(patch)),
        None => colorize(patch),
    };
    formatted.lines().map(|l| l.replace('\t', "    ")).collect()
}


/// The user's `core.pager` (as seen from `repo`) when it is delta.
fn delta_pager(repo: Option<&Path>) -> Option<String> {
    let pager = git::git_stdout(repo.unwrap_or(Path::new(".")), &["config", "--get", "core.pager"])?;
    Some(pager.trim().to_string()).filter(|p| p.contains("delta"))
}

/// Runs `patch` through the configured delta command at the pane's width, without paging.
fn through_delta(pager: &str, patch: &str, cols: u16) -> Option<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("{pager} --width={cols} --paging=never"))
        .env("DELTA_PAGER", "cat")
        .env("PAGER", "cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input = patch.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Plain git-style colours for a unified diff, when delta is not in use.
fn colorize(patch: &str) -> String {
    let mut out = String::with_capacity(patch.len() + patch.len() / 4);
    for line in patch.lines() {
        let colour = if line.starts_with("diff --git") || line.starts_with("+++") || line.starts_with("---") {
            "1"
        } else if line.starts_with("@@") {
            "36"
        } else if line.starts_with('+') {
            "32"
        } else if line.starts_with('-') {
            "31"
        } else {
            ""
        };
        if colour.is_empty() {
            out.push_str(line);
        } else {
            out.push_str(&format!("\x1b[{colour}m{line}\x1b[0m"));
        }
        out.push('\n');
    }
    out
}


/// Draws `lines` from the top and `hint`, dimmed and right-aligned, on the last row.
fn draw(lines: &[String], cols: u16, rows: u16, hint: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let body = if rows > 1 { rows - 1 } else { rows };
    for row in 0..body {
        queue!(out, cursor::MoveTo(0, row))?;
        let line = lines.get(usize::from(row)).map(|line| ansi::fit(line, usize::from(cols))).unwrap_or_default();
        queue!(out, Print(&line), Print("\x1b[0m"))?;
        // Not on full rows: after the last column the cursor waits to wrap, and an erase there
        // wipes that last cell.
        if ansi::width(&line) < usize::from(cols) {
            queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
        }
    }
    if body < rows {
        let hint = ansi::fit(hint, usize::from(cols).saturating_sub(1));
        let pad = usize::from(cols).saturating_sub(ansi::width(&hint) + 1);
        queue!(out, cursor::MoveTo(0, body), terminal::Clear(terminal::ClearType::CurrentLine))?;
        queue!(out, cursor::MoveTo(pad as u16, body), Print(format!("\x1b[2m{hint}\x1b[0m")))?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorize_marks_headers_hunks_and_changed_lines() {
        let out = colorize("diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n same\n");
        assert_eq!(
            out,
            "\x1b[1mdiff --git a/x b/x\x1b[0m\n\x1b[36m@@ -1 +1 @@\x1b[0m\n\x1b[31m-old\x1b[0m\n\x1b[32m+new\x1b[0m\n same\n"
        );
    }
}
