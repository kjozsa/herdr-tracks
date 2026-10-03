//! The diff pane: a small viewer for the diff selected in the sidebar, either a changed local
//! file or (part of) a pull request. The sidebar names it in a request file and the viewer
//! redraws when that changes, so browsing swaps the content in place instead of opening panes.

use crate::git::Change;
use crate::term::Term;
use crate::{ansi, github, herdr};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use crossterm::style::Print;
use crossterm::{cursor, queue, terminal};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
/// How long a fetched pull-request patch is reused before fetching it again.
const PR_PATCH_TTL: Duration = Duration::from_secs(30);
const WHEEL_LINES: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffRequest {
    /// A changed file in a local checkout: staged and unstaged changes together.
    Local { root: PathBuf, path: String, orig: Option<String>, untracked: bool },
    /// What the unpushed commits change in one file of a local checkout (`@{u}...HEAD`).
    Unpushed { root: PathBuf, path: String },
    /// A pull request's patch: one file of it, or all of it.
    Pr { url: String, path: Option<String> },
}

impl DiffRequest {
    pub fn local(root: &Path, change: &Change) -> Self {
        DiffRequest::Local {
            root: root.to_path_buf(),
            path: change.path.clone(),
            orig: change.orig.clone(),
            untracked: change.untracked(),
        }
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

/// Fetched pull-request patches by URL, with when they were fetched.
type PrPatches = HashMap<String, (Instant, Result<String, String>)>;

/// Shows the requested diff until `q`/`Esc`, re-rendering when the request or width changes.
/// Also ends once `owner` (the sidebar pane) is gone: nothing would point the viewer anywhere.
fn view(file: &Path, owner: Option<&str>) -> Result<()> {
    let mut request_text = String::new();
    let mut rendered: Option<(String, u16)> = None;
    let mut lines: Vec<String> = Vec::new();
    let mut patches = PrPatches::new();
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
        if rendered.as_ref() != Some(&(request_text.clone(), cols)) {
            lines = match serde_json::from_str::<DiffRequest>(&request_text) {
                Ok(request) => diff_lines(&request, cols, &mut patches),
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

/// The request's patch as ANSI lines, formatted by delta when that is the user's git pager.
fn diff_lines(request: &DiffRequest, cols: u16, patches: &mut PrPatches) -> Vec<String> {
    let (patch, repo) = match request {
        DiffRequest::Local { root, .. } | DiffRequest::Unpushed { root, .. } => {
            (local_patch(request, root), Some(root.as_path()))
        }
        DiffRequest::Pr { url, path } => (pr_patch(patches, url, path.as_deref()), None),
    };
    let patch = match patch {
        Ok(patch) if patch.trim().is_empty() => return vec!["no changes".into()],
        Ok(patch) => patch,
        Err(e) => return vec![e],
    };
    let formatted = match delta_pager(repo) {
        Some(pager) => through_delta(&pager, &patch, cols).unwrap_or_else(|| colorize(&patch)),
        None => colorize(&patch),
    };
    formatted.lines().map(|l| l.replace('\t', "    ")).collect()
}

fn local_patch(request: &DiffRequest, root: &Path) -> Result<String, String> {
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(root).args(args).stdin(Stdio::null()).output();
    let has_head = git(&["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok_and(|o| o.status.success());
    let args = diff_args(request, has_head);
    let out = git(&args.iter().map(String::as_str).collect::<Vec<_>>()).map_err(|e| format!("git: {e}"))?;
    let untracked = matches!(request, DiffRequest::Local { untracked: true, .. });
    // `--no-index` exits 1 when the files differ; other failures leave stdout empty.
    if out.stdout.is_empty() && !out.status.success() && !untracked {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pull request's patch, or one file of it; fetched once per [`PR_PATCH_TTL`].
fn pr_patch(patches: &mut PrPatches, url: &str, path: Option<&str>) -> Result<String, String> {
    let fresh = patches.get(url).is_some_and(|(at, _)| at.elapsed() < PR_PATCH_TTL);
    if !fresh {
        patches.insert(url.to_string(), (Instant::now(), github::pr_diff(url)));
    }
    let patch = patches[url].1.as_ref().map_err(String::clone)?;
    match path {
        None => Ok(patch.clone()),
        Some(path) => github::patch_section(patch, path).map(str::to_string).ok_or_else(|| format!("{path} is not in this PR")),
    }
}

/// The user's `core.pager` (as seen from `repo`) when it is delta.
fn delta_pager(repo: Option<&Path>) -> Option<String> {
    let mut git = Command::new("git");
    if let Some(repo) = repo {
        git.arg("-C").arg(repo);
    }
    let out = git.args(["config", "--get", "core.pager"]).stdin(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|p| p.contains("delta"))
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

/// Local changes: staged and unstaged together (`git diff HEAD`), untracked files as all-new;
/// unpushed commits: what a push would add (`@{u}...HEAD`).
fn diff_args(request: &DiffRequest, has_head: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["diff".into(), "--color=never".into()];
    match request {
        DiffRequest::Local { path, untracked: true, .. } => {
            args.extend(["--no-index", "--", "/dev/null", path.as_str()].map(String::from));
        }
        DiffRequest::Local { path, orig, .. } => {
            // Before the first commit there is no HEAD: show what is staged.
            args.extend([if has_head { "HEAD" } else { "--cached" }, "-M", "--"].map(String::from));
            args.extend(orig.clone());
            args.push(path.clone());
        }
        DiffRequest::Unpushed { path, .. } => {
            args.extend(["-M", "@{u}...HEAD", "--", path.as_str()].map(String::from));
        }
        DiffRequest::Pr { .. } => return Vec::new(),
    }
    args
}

fn draw(lines: &[String], cols: u16, rows: u16) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for row in 0..rows {
        queue!(out, cursor::MoveTo(0, row))?;
        if let Some(line) = lines.get(usize::from(row)) {
            queue!(out, Print(ansi::fit(line, usize::from(cols))))?;
        }
        queue!(out, Print("\x1b[0m"), terminal::Clear(terminal::ClearType::UntilNewLine))?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, orig: Option<&str>, untracked: bool) -> DiffRequest {
        DiffRequest::Local { root: "/r".into(), path: path.into(), orig: orig.map(String::from), untracked }
    }

    #[test]
    fn diff_args_cover_tracked_renamed_untracked_and_unborn() {
        let args = |r: &DiffRequest, head: bool| diff_args(r, head)[2..].to_vec();
        assert_eq!(args(&local("a.rs", None, false), true), ["HEAD", "-M", "--", "a.rs"]);
        assert_eq!(args(&local("new.rs", Some("old.rs"), false), true), ["HEAD", "-M", "--", "old.rs", "new.rs"]);
        assert_eq!(args(&local("n.md", None, true), true), ["--no-index", "--", "/dev/null", "n.md"]);
        assert_eq!(args(&local("a.rs", None, false), false), ["--cached", "-M", "--", "a.rs"]);
        let unpushed = DiffRequest::Unpushed { root: "/r".into(), path: "a.rs".into() };
        assert_eq!(args(&unpushed, true), ["-M", "@{u}...HEAD", "--", "a.rs"]);
    }

    #[test]
    fn colorize_marks_headers_hunks_and_changed_lines() {
        let out = colorize("diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n same\n");
        assert_eq!(
            out,
            "\x1b[1mdiff --git a/x b/x\x1b[0m\n\x1b[36m@@ -1 +1 @@\x1b[0m\n\x1b[31m-old\x1b[0m\n\x1b[32m+new\x1b[0m\n same\n"
        );
    }
}
