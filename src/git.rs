//! `git status --porcelain=v2 --branch` for one checkout.

use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Two-letter status code as in porcelain v1, e.g. ` M`, `A `, `??`, `UU`.
    pub code: [u8; 2],
    pub path: String,
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
    Ok(parse(&String::from_utf8_lossy(&out.stdout)))
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
            Some(Change { code: code(f.first()?), path: f.get(7)?.to_string() })
        }
        "2" => {
            let f: Vec<&str> = rest.splitn(9, ' ').collect();
            let (path, orig) = f.get(8)?.split_once('\t')?;
            Some(Change { code: code(f.first()?), path: format!("{orig} -> {path}") })
        }
        "u" => {
            let f: Vec<&str> = rest.splitn(10, ' ').collect();
            Some(Change { code: code(f.first()?), path: f.get(9)?.to_string() })
        }
        "?" => Some(Change { code: *b"??", path: rest.to_string() }),
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
        let codes: Vec<(&[u8; 2], &str)> = s.changes.iter().map(|c| (&c.code, c.path.as_str())).collect();
        assert_eq!(
            codes,
            [(b" M", "src/a b.rs"), (b"R ", "old.rs -> new.rs"), (b"UU", "conflict.rs"), (b"??", "notes.md")]
        );
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
}
