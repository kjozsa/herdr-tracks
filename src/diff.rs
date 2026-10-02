//! The diff pane: a small viewer for `git diff` of the file selected in the sidebar. The
//! sidebar names the file in a request file and the viewer redraws when it changes, so browsing
//! files swaps the content in place instead of opening a new pane per file.

use crate::git::Change;
use crate::herdr;
use crate::term::Term;
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use crossterm::style::Print;
use crossterm::{cursor, queue, terminal};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

/// Environment variable naming the request file the viewer follows.
pub const ENV: &str = "TRACKS_DIFF_FILE";
/// Environment variable naming the sidebar pane that opened the viewer.
pub const OWNER_ENV: &str = "TRACKS_DIFF_OWNER";
/// How often the viewer checks the request file.
const POLL: Duration = Duration::from_millis(50);
/// How often the viewer checks that its sidebar still exists.
const OWNER_CHECK: Duration = Duration::from_secs(1);
const WHEEL_LINES: usize = 3;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffRequest {
    pub root: PathBuf,
    pub path: String,
    pub orig: Option<String>,
    pub untracked: bool,
}

impl DiffRequest {
    pub fn new(root: &Path, change: &Change) -> Self {
        Self { root: root.to_path_buf(), path: change.path.clone(), orig: change.orig.clone(), untracked: change.untracked() }
    }
}

/// The request file of the diff pane belonging to `sidebar_pane`.
pub fn request_file(sidebar_pane: &str) -> PathBuf {
    let name: String = sidebar_pane.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    crate::state::state_dir().join("diff").join(format!("{name}.json"))
}

pub fn write_request(file: &Path, request: &DiffRequest) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(request)?)?;
    std::fs::rename(&tmp, file)?;
    Ok(())
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

/// Shows the requested diff until `q`/`Esc`, re-rendering when the request or width changes.
/// Also ends once `owner` (the sidebar pane) is gone: nothing would point the viewer anywhere.
fn view(file: &Path, owner: Option<&str>) -> Result<()> {
    let mut request_text = String::new();
    let mut rendered: Option<(String, u16)> = None;
    let mut lines: Vec<String> = Vec::new();
    let mut scroll = 0usize;
    let mut dirty = true;
    let mut next_owner_check = Instant::now() + OWNER_CHECK;
    loop {
        let (cols, rows) = terminal::size()?;
        if let Ok(text) = std::fs::read_to_string(file) {
            if text != request_text {
                request_text = text;
                scroll = 0;
            }
        }
        if rendered.as_ref() != Some(&(request_text.clone(), cols)) {
            lines = match serde_json::from_str::<DiffRequest>(&request_text) {
                Ok(request) => diff_lines(&request, cols),
                Err(_) => vec!["waiting for a file…".into()],
            };
            rendered = Some((request_text.clone(), cols));
            dirty = true;
        }
        let page = usize::from(rows);
        scroll = scroll.min(lines.len().saturating_sub(page));
        if dirty {
            draw(&lines[scroll..], cols, rows)?;
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

/// `git diff` for the request, formatted by the user's pager when it is delta, as ANSI lines.
fn diff_lines(request: &DiffRequest, cols: u16) -> Vec<String> {
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(&request.root).args(args).stdin(Stdio::null()).output();
    let has_head = git(&["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok_and(|o| o.status.success());
    let args = diff_args(request, has_head);
    let diff = match git(&args.iter().map(String::as_str).collect::<Vec<_>>()) {
        Ok(out) if out.stdout.is_empty() && !out.status.success() && !request.untracked => {
            return vec![String::from_utf8_lossy(&out.stderr).trim().to_string()];
        }
        Ok(out) => out.stdout,
        Err(e) => return vec![format!("git: {e}")],
    };
    if diff.is_empty() {
        return vec!["no changes".into()];
    }
    let pager = git(&["config", "--get", "core.pager"])
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| p.contains("delta"));
    let formatted = pager.and_then(|pager| through_delta(&pager, &diff, cols)).unwrap_or(diff);
    String::from_utf8_lossy(&formatted).lines().map(|l| l.replace('\t', "    ")).collect()
}

/// Runs `diff` through the configured delta command at the pane's width, without paging.
fn through_delta(pager: &str, diff: &[u8], cols: u16) -> Option<Vec<u8>> {
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
    let input = diff.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();
    out.status.success().then_some(out.stdout)
}

/// Staged and unstaged changes together (`git diff HEAD`); untracked files as all-new.
fn diff_args(request: &DiffRequest, has_head: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["diff".into(), "--color=always".into()];
    if request.untracked {
        args.extend(["--no-index", "--", "/dev/null"].map(String::from));
    } else {
        // Before the first commit there is no HEAD: show what is staged.
        args.extend([if has_head { "HEAD" } else { "--cached" }, "-M", "--"].map(String::from));
        args.extend(request.orig.clone());
    }
    args.push(request.path.clone());
    args
}

fn draw(lines: &[String], cols: u16, rows: u16) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for row in 0..rows {
        queue!(out, cursor::MoveTo(0, row))?;
        if let Some(line) = lines.get(usize::from(row)) {
            queue!(out, Print(fit_ansi(line, usize::from(cols))))?;
        }
        queue!(out, Print("\x1b[0m"), terminal::Clear(terminal::ClearType::UntilNewLine))?;
    }
    out.flush()?;
    Ok(())
}

/// Cuts an ANSI-coloured line to `cols` display columns, keeping escape sequences intact.
fn fit_ansi(line: &str, cols: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut used = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            match chars.next() {
                // CSI: parameters up to a final byte in @..~.
                Some('[') => {
                    out.push('[');
                    for c in chars.by_ref() {
                        out.push(c);
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \.
                Some(']') => {
                    out.push(']');
                    while let Some(c) = chars.next() {
                        out.push(c);
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            if c == '\x1b' {
                                out.push(chars.next().unwrap_or('\\'));
                            }
                            break;
                        }
                    }
                }
                Some(other) => out.push(other),
                None => {}
            }
            continue;
        }
        let w = c.width().unwrap_or(0);
        if used + w > cols {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(path: &str, orig: Option<&str>, untracked: bool) -> DiffRequest {
        DiffRequest { root: "/r".into(), path: path.into(), orig: orig.map(String::from), untracked }
    }

    #[test]
    fn diff_args_cover_tracked_renamed_untracked_and_unborn() {
        let args = |r: &DiffRequest, head: bool| diff_args(r, head)[2..].to_vec();
        assert_eq!(args(&request("a.rs", None, false), true), ["HEAD", "-M", "--", "a.rs"]);
        assert_eq!(args(&request("new.rs", Some("old.rs"), false), true), ["HEAD", "-M", "--", "old.rs", "new.rs"]);
        assert_eq!(args(&request("n.md", None, true), true), ["--no-index", "--", "/dev/null", "n.md"]);
        assert_eq!(args(&request("a.rs", None, false), false), ["--cached", "-M", "--", "a.rs"]);
        assert_eq!(diff_args(&request("a.rs", None, false), true)[..2], ["diff", "--color=always"]);
    }

    #[test]
    fn fit_ansi_counts_only_visible_columns() {
        let red = "\x1b[38;2;255;0;0m";
        assert_eq!(fit_ansi(&format!("{red}abcdef\x1b[0m"), 3), format!("{red}abc"));
        assert_eq!(fit_ansi("日本語", 5), "日本");
        assert_eq!(fit_ansi("\x1b]8;;https://x\x1b\\ab\x1b]8;;\x1b\\", 1), "\x1b]8;;https://x\x1b\\a");
        assert_eq!(fit_ansi("short", 80), "short");
    }
}
