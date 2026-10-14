//! `git status --porcelain=v2 --branch` for one checkout, with per-file line counts.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};

/// Untracked files larger than this are not read to count their lines.
const COUNT_LIMIT: u64 = 4 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Two-letter status code as in porcelain v1, e.g. ` M`, `A `, `??`, `UU`.
    pub code: [u8; 2],
    /// Path relative to the checkout root.
    pub path: String,
    /// Previous path of a rename or copy.
    pub orig: Option<String>,
    /// Lines added and removed against HEAD (staged and unstaged together).
    pub stat: Option<LineStat>,
}

/// Lines a change adds and removes, as `git diff --numstat` counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStat {
    Lines { added: u32, removed: u32 },
    Binary,
}

impl Change {
    /// `path`, or `orig -> path` for renames.
    pub fn display(&self) -> String {
        match &self.orig {
            Some(orig) => format!("{orig} -> {}", self.path),
            None => self.path.clone(),
        }
    }

    pub fn untracked(&self) -> bool {
        self.code == *b"??"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<Change>,
    /// Hash of HEAD plus every status entry (with index blob ids). Upstream tracking is left
    /// out, so fetches and pushes do not count as local changes.
    pub fingerprint: u64,
}

pub fn status(root: &Path) -> Result<RepoStatus, String> {
    let out = git(root)
        .args(["-c", "core.quotePath=false", "status", "--porcelain=v2", "--branch"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(first_stderr_line(&out, "git status failed"));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut status = parse(&text);
    let stats = numstat(root, text.contains("# branch.oid (initial)"));
    for change in &mut status.changes {
        change.stat = if change.untracked() { count_lines(&root.join(&change.path)) } else { stats.get(&change.path).copied() };
    }
    Ok(status)
}

/// Line counts of every changed tracked file against HEAD; before the first commit, staged
/// plus unstaged changes.
fn numstat(root: &Path, unborn: bool) -> HashMap<String, LineStat> {
    if !unborn {
        return diff_numstat(root, &["HEAD"]);
    }
    let mut stats = diff_numstat(root, &["--cached"]);
    for (path, stat) in diff_numstat(root, &[]) {
        let merged = match (stats.get(&path), stat) {
            (Some(LineStat::Lines { added: a, removed: r }), LineStat::Lines { added, removed }) => {
                LineStat::Lines { added: a + added, removed: r + removed }
            }
            (Some(_), _) => LineStat::Binary,
            (None, stat) => stat,
        };
        stats.insert(path, merged);
    }
    stats
}

fn diff_numstat(root: &Path, args: &[&str]) -> HashMap<String, LineStat> {
    let args = [&["-c", "core.quotePath=false", "diff", "--numstat", "-z", "-M"][..], args].concat();
    parse_numstat(&git_stdout(root, &args).unwrap_or_default())
}

/// `git diff --numstat -z`: `added\tremoved\tpath\0`, or `added\tremoved\t\0old\0new\0` for
/// renames; binary files report `-` for both counts.
fn parse_numstat(text: &str) -> HashMap<String, LineStat> {
    let mut stats = HashMap::new();
    let mut tokens = text.split('\0');
    while let Some(record) = tokens.next() {
        let mut fields = record.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (fields.next(), fields.next(), fields.next()) else { continue };
        let path = if path.is_empty() {
            tokens.next(); // previous path
            match tokens.next() {
                Some(new) => new,
                None => continue,
            }
        } else {
            path
        };
        let stat = match (added.parse(), removed.parse()) {
            (Ok(added), Ok(removed)) => LineStat::Lines { added, removed },
            _ => LineStat::Binary,
        };
        stats.insert(path.to_string(), stat);
    }
    stats
}

/// An untracked file counts as all lines added; `None` for directories and very large files.
fn count_lines(path: &Path) -> Option<LineStat> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > COUNT_LIMIT {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return Some(LineStat::Binary);
    }
    let newlines = bytes.iter().filter(|b| **b == b'\n').count();
    let unterminated = usize::from(bytes.last().is_some_and(|b| *b != b'\n'));
    Some(LineStat::Lines { added: u32::try_from(newlines + unterminated).unwrap_or(u32::MAX), removed: 0 })
}

fn parse(text: &str) -> RepoStatus {
    let mut s = RepoStatus {
        branch: String::new(),
        ahead: 0,
        behind: 0,
        changes: Vec::new(),
        fingerprint: FNV_OFFSET,
    };
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.head" => s.branch = if value == "(detached)" { "detached".into() } else { value.into() },
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            s.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            s.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            if key == "branch.upstream" || key == "branch.ab" {
                continue;
            }
        } else if let Some(change) = parse_entry(line) {
            s.changes.push(change);
        }
        s.fingerprint = fnv1a(s.fingerprint, line.as_bytes());
    }
    s
}

/// Ordinary (`1`), renamed/copied (`2`), unmerged (`u`) and untracked (`?`) entries.
fn parse_entry(line: &str) -> Option<Change> {
    let code = |xy: &str| -> [u8; 2] {
        let b = xy.as_bytes();
        let unchanged = |c: u8| if c == b'.' { b' ' } else { c };
        [unchanged(b[0]), unchanged(b[1])]
    };
    let (kind, rest) = line.split_once(' ')?;
    match kind {
        "1" => {
            let f: Vec<&str> = rest.splitn(8, ' ').collect();
            Some(Change { code: code(f.first()?), path: f.get(7)?.to_string(), orig: None, stat: None })
        }
        "2" => {
            let f: Vec<&str> = rest.splitn(9, ' ').collect();
            let (path, orig) = f.get(8)?.split_once('\t')?;
            Some(Change { code: code(f.first()?), path: path.to_string(), orig: Some(orig.to_string()), stat: None })
        }
        "u" => {
            let f: Vec<&str> = rest.splitn(10, ' ').collect();
            Some(Change { code: code(f.first()?), path: f.get(9)?.to_string(), orig: None, stat: None })
        }
        "?" => Some(Change { code: *b"??", path: rest.to_string(), orig: None, stat: None }),
        _ => None,
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a: stable across builds, so persisted baselines stay comparable.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes.iter().chain(b"\n") {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// A commit, as listed under its repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    /// Committer time, unix seconds.
    pub time: i64,
    pub subject: String,
}

/// Commits on any branch, remote branch or tag (or a detached HEAD) committed within one of
/// `windows` (unix milliseconds, start and end), newest first. Committer time, not author
/// time: a rebase or cherry-pick within a window counts, a merged-in older commit does not.
pub fn commits_within(root: &Path, windows: &[(i64, i64)]) -> Vec<Commit> {
    let Some(since) = windows.iter().map(|(start, _)| start / 1000 - 1).min() else { return Vec::new() };
    let since = format!("--since=@{since}");
    let log = git_stdout(root, &["log", "HEAD", "--branches", "--remotes", "--tags", "--format=%H%x1f%ct%x1f%s", &since]);
    log.map_or_else(Vec::new, |text| parse_log(&text, windows))
}

fn parse_log(text: &str, windows: &[(i64, i64)]) -> Vec<Commit> {
    // Git keeps whole seconds: allow one on either side of a window.
    let within = |time: i64| windows.iter().any(|(start, end)| time * 1000 >= start - 1000 && time * 1000 <= end + 1000);
    let mut commits: Vec<Commit> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\x1f');
            let (hash, time) = (fields.next()?, fields.next()?.parse().ok()?);
            Some(Commit { hash: hash.into(), time, subject: fields.next().unwrap_or_default().into() })
        })
        .filter(|c| within(c.time))
        .collect();
    commits.sort_by_key(|c| std::cmp::Reverse(c.time));
    commits
}

/// A file a commit changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Status letter as `git status` writes it: `A`, `D`, `R`, `C`, `M`, …
    pub code: char,
    /// Path after the commit (the new path of a rename).
    pub path: String,
    pub stat: LineStat,
}

/// Whether the checkout has the commit.
pub fn commit_exists(root: &Path, hash: &str) -> bool {
    git_stdout(root, &["cat-file", "-e", &format!("{hash}^{{commit}}")]).is_some()
}

/// The files a commit changes (a merge: against its first parent), with line counts.
pub fn commit_files(root: &Path, hash: &str) -> Result<Vec<FileChange>, String> {
    let show = |format: &str| {
        git_stdout(root, &["-c", "core.quotePath=false", "show", "--format=", "-z", "-M", "--diff-merges=first-parent", format, hash])
            .ok_or_else(|| format!("git show {hash} failed"))
    };
    let stats = parse_numstat(&show("--numstat")?);
    Ok(parse_name_status(&show("--name-status")?)
        .into_iter()
        .map(|(code, path)| FileChange { code, stat: stats.get(&path).copied().unwrap_or(LineStat::Binary), path })
        .collect())
}

/// `--name-status -z`: `M\0path\0`, or `R100\0old\0new\0` for renames and copies.
fn parse_name_status(text: &str) -> Vec<(char, String)> {
    let mut files = Vec::new();
    let mut tokens = text.split('\0').filter(|t| !t.is_empty());
    while let Some(status) = tokens.next() {
        let Some(code) = status.chars().next() else { continue };
        if matches!(code, 'R' | 'C') {
            tokens.next(); // previous path
        }
        if let Some(path) = tokens.next() {
            files.push((code, path.to_string()));
        }
    }
    files
}

/// `git -C <root>`, taking no optional locks: a git command of the user's running meanwhile
/// does not find the index locked.
pub(crate) fn git(root: &Path) -> Command {
    let mut git = Command::new("git");
    git.arg("--no-optional-locks").arg("-C").arg(root);
    git
}

/// What the git command prints, when it succeeds.
pub(crate) fn git_stdout(root: &Path, args: &[&str]) -> Option<String> {
    let out = git(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The first line a failed command wrote to stderr, or `fallback` when it wrote none.
pub(crate) fn first_stderr_line(out: &Output, fallback: &str) -> String {
    String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or(fallback).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_skips_the_old_path_of_renames() {
        let text = "M\0src/a.rs\0R087\0old name.rs\0new name.rs\0A\0b.rs\0";
        assert_eq!(parse_name_status(text), [('M', "src/a.rs".into()), ('R', "new name.rs".into()), ('A', "b.rs".into())]);
    }

    #[test]
    fn commits_count_only_within_the_chat_s_tool_calls() {
        let log = [
            "a1\x1f1000\x1fmerge the feature",
            "b2\x1f400\x1fold commit merged in",
            "c3\x1f990\x1ffix: a | b",
            "d4\x1f2000\x1fanother chat's commit",
        ]
        .join("\n");
        // Two calls: 989.5–995 s and 999–1000.2 s.
        let commits = parse_log(&log, &[(989_500, 995_000), (999_000, 1_000_200)]);
        let shown: Vec<(&str, &str)> = commits.iter().map(|c| (c.hash.as_str(), c.subject.as_str())).collect();
        assert_eq!(shown, [("a1", "merge the feature"), ("c3", "fix: a | b")], "newest first, outside windows left out");
    }

    const BASE: &str = "# branch.oid 1111\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n";

    #[test]
    fn parses_branch_tracking_and_entries() {
        let s = parse(&format!(
            "{BASE}1 .M N... 100644 100644 100644 aaa aaa src/a b.rs\n\
             2 R. N... 100644 100644 100644 bbb bbb R100 new.rs\told.rs\n\
             u UU N... 100644 100644 100644 100644 c1 c2 c3 conflict.rs\n\
             ? notes.md\n"
        ));
        assert_eq!((s.branch.as_str(), s.ahead, s.behind), ("main", 2, 1));
        let codes: Vec<(&[u8; 2], String)> = s.changes.iter().map(|c| (&c.code, c.display())).collect();
        assert_eq!(
            codes,
            [
                (b" M", "src/a b.rs".to_string()),
                (b"R ", "old.rs -> new.rs".to_string()),
                (b"UU", "conflict.rs".to_string()),
                (b"??", "notes.md".to_string())
            ]
        );
        assert_eq!((s.changes[1].path.as_str(), s.changes[1].orig.as_deref()), ("new.rs", Some("old.rs")));
        assert_eq!(parse("# branch.oid (initial)\n# branch.head (detached)\n").branch, "detached");
    }

    #[test]
    fn fingerprint_tracks_local_state_but_not_upstream() {
        let fp = |text: &str| parse(text).fingerprint;
        let clean = fp(BASE);
        assert_eq!(clean, fp(&BASE.replace("+2 -1", "+0 -5").replace("origin/main", "fork/main")));
        assert_ne!(clean, fp(&BASE.replace("1111", "2222")), "new commit");
        assert_ne!(clean, fp(&format!("{BASE}? scratch.txt\n")), "new untracked file");
        let staged = |blob: &str| fp(&format!("{BASE}1 M. N... 100644 100644 100644 aaa {blob} a.rs\n"));
        assert_ne!(staged("bbb"), staged("ccc"), "re-staged content");
    }

    #[test]
    fn numstat_reads_counts_renames_and_binaries() {
        let stats = parse_numstat(concat!("3\t1\tsrc/a b.rs\0", "10\t0\t\0old.rs\0new.rs\0", "-\t-\tlogo.png\0"));
        assert_eq!(stats.get("src/a b.rs"), Some(&LineStat::Lines { added: 3, removed: 1 }));
        assert_eq!(stats.get("new.rs"), Some(&LineStat::Lines { added: 10, removed: 0 }), "renames key by new path");
        assert_eq!(stats.get("old.rs"), None);
        assert_eq!(stats.get("logo.png"), Some(&LineStat::Binary));
    }
}
