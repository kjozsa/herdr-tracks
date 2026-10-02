//! Incremental reader for omp chat transcripts (`~/.omp/agent/sessions/**/*.jsonl`):
//! extracts the paths and working directories of the agent's tool calls, and the pull
//! requests its `gh pr create` calls opened.

use crate::github::{self, PrRef};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

pub struct Transcript {
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
    /// Session cwd from the transcript header; base for relative tool paths.
    cwd: PathBuf,
    /// Ids of `gh pr create` calls whose result has not been read yet.
    pr_calls: HashSet<String>,
}

/// What the agent did in transcript lines appended since the last poll.
#[derive(Debug, Default)]
pub struct Activity {
    pub touches: Vec<Touch>,
    pub prs: Vec<PrRef>,
}

impl Transcript {
    pub fn new(path: PathBuf, fallback_cwd: PathBuf) -> Self {
        Self { path, offset: 0, pending: Vec::new(), cwd: fallback_cwd, pr_calls: HashSet::new() }
    }

    pub fn poll(&mut self) -> std::io::Result<Activity> {
        let mut file = File::open(&self.path)?;
        if file.metadata()?.len() < self.offset {
            // Rewritten from scratch: start over.
            self.offset = 0;
            self.pending.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let read = file.read_to_end(&mut self.pending)?;
        self.offset += read as u64;
        let complete = self.pending.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let mut found = Activity::default();
        let lines = std::mem::take(&mut self.pending);
        for line in lines[..complete].split(|b| *b == b'\n') {
            self.scan_line(line, &mut found);
        }
        self.pending = lines[complete..].to_vec();
        Ok(found)
    }

    fn scan_line(&mut self, line: &[u8], found: &mut Activity) {
        let is_session = contains(line, b"\"type\":\"session\"");
        let is_pr_result = contains(line, b"\"toolResult\"") && self.pr_calls.iter().any(|id| contains(line, id.as_bytes()));
        if !is_session && !is_pr_result && !contains(line, b"\"toolCall\"") {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else { return };
        if is_session {
            if let Some(cwd) = entry["cwd"].as_str() {
                self.cwd = PathBuf::from(cwd);
            }
            return;
        }
        let message = &entry["message"];
        if message["role"] == "toolResult" {
            let id = message["toolCallId"].as_str().unwrap_or_default();
            if self.pr_calls.remove(id) {
                let text: String =
                    message["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
                found.prs.extend(github::pr_urls(&text));
            }
            return;
        }
        let Some(blocks) = message["content"].as_array() else { return };
        for block in blocks.iter().filter(|b| b["type"] == "toolCall") {
            let args = &block["arguments"];
            if block["name"] == "bash" && creates_pr(&shell_tokens(args["command"].as_str().unwrap_or_default())) {
                if let Some(id) = block["id"].as_str() {
                    self.pr_calls.insert(id.to_string());
                }
            }
            found.touches.extend(tool_call_touches(&block["name"], args, &self.cwd));
        }
    }
}

fn creates_pr(tokens: &[&str]) -> bool {
    tokens.windows(3).any(|w| w == ["gh", "pr", "create"])
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A path a tool call worked on, and whether the call modified it.
#[derive(Debug, PartialEq, Eq)]
pub struct Touch {
    pub path: PathBuf,
    pub writes: bool,
}

/// Absolute paths one tool call works on. Paths may still carry selectors (`file.rs:10-20`)
/// or globs; callers resolve them to repos by walking ancestors.
fn tool_call_touches(name: &Value, args: &Value, base: &Path) -> Vec<Touch> {
    let touches = |paths: Vec<&str>, base: &Path, writes: bool| -> Vec<Touch> {
        paths.into_iter().filter_map(|p| resolve(p, base)).map(|path| Touch { path, writes }).collect()
    };
    match name.as_str().unwrap_or_default() {
        "bash" => {
            let cmd = args["command"].as_str().unwrap_or_default();
            let tokens = shell_tokens(cmd);
            let dir = args["cwd"].as_str().and_then(|c| resolve(c, base)).unwrap_or_else(|| base.to_path_buf());
            let writes = git_writes(&tokens);
            let mut found = vec![Touch { path: dir.clone(), writes }];
            found.extend(touches(command_dirs(&tokens), &dir, writes));
            found
        }
        "edit" => touches(edit_targets(args["input"].as_str().unwrap_or_default()), base, true),
        "write" => touches(vec![args["path"].as_str().unwrap_or_default()], base, true),
        _ => touches(args["path"].as_str().unwrap_or_default().split(';').collect(), base, false),
    }
}

fn shell_tokens(cmd: &str) -> Vec<&str> {
    cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Directory operands of `cd DIR` and `-C DIR` (git/make/tar).
fn command_dirs<'a>(tokens: &[&'a str]) -> Vec<&'a str> {
    tokens
        .windows(2)
        .filter(|w| w[0] == "cd" || w[0] == "-C")
        .map(|w| w[1].trim_matches(|c| c == '"' || c == '\''))
        .filter(|d| !d.starts_with('-') && !d.contains('`') && (!d.contains('$') || d.starts_with("$HOME")))
        .collect()
}

/// Whether the command runs a git subcommand that creates commits or rewrites the
/// working tree. Lets a replayed transcript credit bash-only changes such as `git commit`.
fn git_writes(tokens: &[&str]) -> bool {
    const WRITE_VERBS: &[&str] = &["commit", "merge", "rebase", "cherry-pick", "revert", "am", "pull", "apply"];
    tokens.iter().enumerate().filter(|(_, t)| **t == "git").any(|(i, _)| {
        let mut rest = tokens[i + 1..].iter();
        while let Some(t) = rest.next() {
            match *t {
                "-C" | "-c" => {
                    rest.next();
                }
                t if t.starts_with('-') => {}
                verb => return WRITE_VERBS.contains(&verb),
            }
        }
        false
    })
}

/// File headers of an omp hashline patch: `[path/to/file#1A2B]`.
fn edit_targets(input: &str) -> Vec<&str> {
    input
        .lines()
        .filter_map(|l| l.trim().strip_prefix('[')?.strip_suffix(']'))
        .filter_map(|h| h.rsplit_once('#').map(|(path, _tag)| path))
        .collect()
}

/// Absolute, `..`-free form of a tool path; `None` for URLs and internal URIs.
fn resolve(raw: &str, base: &Path) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains("://") {
        return None;
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let path = if raw == "~" || raw == "$HOME" {
        home()?
    } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("$HOME/")) {
        home()?.join(rest)
    } else {
        base.join(raw)
    };
    // Lexical, so ancestor walks never pass through a directory the path only named.
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// (path, writes) pairs for one tool call made from session cwd `/work/app`.
    fn touches(name: &str, args: Value) -> Vec<(String, bool)> {
        tool_call_touches(&json!(name), &args, Path::new("/work/app"))
            .into_iter()
            .map(|t| (t.path.display().to_string(), t.writes))
            .collect()
    }

    fn owned(pairs: &[(&str, bool)]) -> Vec<(String, bool)> {
        pairs.iter().map(|(p, w)| (p.to_string(), *w)).collect()
    }

    #[test]
    fn bash_touches_its_cwd_and_cd_or_dash_c_operands() {
        assert_eq!(
            touches("bash", json!({"cwd": "../lib", "command": "cd sub && make; git -C /srv/other status"})),
            owned(&[("/work/lib", false), ("/work/lib/sub", false), ("/srv/other", false)])
        );
        assert_eq!(touches("bash", json!({"command": "cd \"$DIR\" && ls -C"})), owned(&[("/work/app", false)]));
    }

    #[test]
    fn only_mutating_calls_count_as_writes() {
        assert_eq!(
            touches("bash", json!({"command": "git add a && git commit -m 'x'"})),
            owned(&[("/work/app", true)])
        );
        assert_eq!(
            touches("bash", json!({"command": "git -C ../other -c core.pager=cat commit -m x"})),
            owned(&[("/work/app", true), ("/work/other", true)])
        );
        for read_only in ["git log --oneline", "git -C ../x status", "echo \"git commit\"", "git diff"] {
            assert!(touches("bash", json!({"command": read_only})).iter().all(|(_, w)| !w), "{read_only}");
        }
        let input = "[src/a.rs#1A2B]\nPUT 1.=1:\n+x\n[/abs/b.rs#FFFF]\nCUT 2";
        assert_eq!(touches("edit", json!({"input": input})), owned(&[("/work/app/src/a.rs", true), ("/abs/b.rs", true)]));
        assert_eq!(touches("write", json!({"path": "notes.md"})), owned(&[("/work/app/notes.md", true)]));
        assert!(touches("write", json!({"path": "proc://bg_5/kill"})).is_empty());
    }

    #[test]
    fn read_paths_split_and_resolve_against_session_cwd() {
        assert_eq!(
            touches("grep", json!({"path": "src;../docs/x.md:10-20"})),
            owned(&[("/work/app/src", false), ("/work/docs/x.md:10-20", false)])
        );
        assert!(touches("read", json!({"path": "artifact://3:10-20"})).is_empty());
    }

    /// Writes `content` to a fresh transcript file and returns a reader positioned at its start.
    fn transcript(name: &str, content: &str) -> (Transcript, PathBuf) {
        let dir = std::env::temp_dir().join(format!("git-sidebar-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.jsonl");
        std::fs::write(&file, content).unwrap();
        (Transcript::new(file.clone(), PathBuf::from("/fallback")), file)
    }

    fn append(file: &Path, text: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(file).unwrap();
        std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
    }

    #[test]
    fn poll_reads_only_complete_new_lines() {
        let call = |p: &str| {
            format!(r#"{{"type":"message","message":{{"content":[{{"type":"toolCall","name":"read","arguments":{{"path":"{p}"}}}}]}}}}"#)
        };
        let b = call("b");
        let (mut t, file) = transcript("lines", &format!("{{\"type\":\"session\",\"cwd\":\"/repo\"}}\n{}\n{}", call("a"), &b[..20]));
        let paths = |a: Activity| a.touches.into_iter().map(|t| t.path).collect::<Vec<_>>();
        assert_eq!(paths(t.poll().unwrap()), [PathBuf::from("/repo/a")]);
        append(&file, &format!("{}\n", &b[20..]));
        assert_eq!(paths(t.poll().unwrap()), [PathBuf::from("/repo/b")]);
        assert!(t.poll().unwrap().touches.is_empty());
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn captures_prs_from_gh_pr_create_results_only() {
        let call = |id: &str, cmd: &str| {
            format!(r#"{{"type":"message","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"{id}","name":"bash","arguments":{{"command":"{cmd}"}}}}]}}}}"#)
        };
        let result = |id: &str, text: &str| {
            format!(r#"{{"type":"message","message":{{"role":"toolResult","toolCallId":"{id}","content":[{{"type":"text","text":"{text}"}}]}}}}"#)
        };
        let (mut t, file) = transcript(
            "prs",
            &format!(
                "{}\n{}\n{}\n",
                call("c1", "git push -u gh feat && gh pr create --fill"),
                call("c2", "gh pr view 7"),
                result("c2", "https://github.com/o/r/pull/7"),
            ),
        );
        assert!(t.poll().unwrap().prs.is_empty(), "result of c1 not written yet; c2 is not a create");
        append(&file, &format!("{}\n", result("c1", "remote: https://github.com/o/r/pull/new/feat\\nhttps://github.com/o/r/pull/12")));
        assert_eq!(t.poll().unwrap().prs, [PrRef { owner: "o".into(), repo: "r".into(), number: 12 }]);
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }
}
