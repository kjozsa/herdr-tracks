//! `git status --porcelain=v2 --branch` for one checkout, with per-file line counts.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

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
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotePath=false", "status", "--porcelain=v2", "--branch"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().next().unwrap_or("git status failed").to_string());
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
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(root)
            .args(["-c", "core.quotePath=false", "diff", "--numstat", "-z", "-M"])
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    if !unborn {
        return parse_numstat(&run(&["HEAD"]));
    }
    let mut stats = parse_numstat(&run(&["--cached"]));
    for (path, stat) in parse_numstat(&run(&[])) {
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
    let mut s = RepoStatus { branch: String::new(), ahead: 0, behind: 0, changes: Vec::new(), fingerprint: FNV_OFFSET };
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

#[cfg(test)]
mod tests {
    use super::*;

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
