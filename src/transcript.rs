//! Incremental reader for agent chat transcripts (omp: `~/.omp/agent/sessions/**/*.jsonl`,
//! Claude Code: `~/.claude/projects/*/<session>.jsonl`): extracts the paths and working
//! directories of the agent's tool calls, and the pull requests its `gh pr create` calls opened.

use crate::github::{self, PrRef};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

/// Transcript dialect, by the agent that writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `toolCall` blocks with `arguments`; results are `toolResult` messages; one session cwd.
    Omp,
    /// `tool_use` blocks with `input`; results are `tool_result` blocks; every entry has its cwd.
    Claude,
}

impl Format {
    /// The dialect of a herdr agent label, if its transcripts are understood.
    pub fn of_agent(agent: &str) -> Option<Self> {
        match agent {
            "omp" => Some(Format::Omp),
            "claude" => Some(Format::Claude),
            _ => None,
        }
    }
}

pub struct Transcript {
    format: Format,
    /// herdr's session reference: a transcript path, or (Claude) a session id.
    session_ref: String,
    /// Resolved transcript file; agents may create it only after the first message.
    path: Option<PathBuf>,
    offset: u64,
    pending: Vec<u8>,
    /// The agent's working directory as of the last line read; base for relative tool paths.
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
    pub fn new(format: Format, session_ref: String, fallback_cwd: PathBuf) -> Self {
        Self { format, session_ref, path: None, offset: 0, pending: Vec::new(), cwd: fallback_cwd, pr_calls: HashSet::new() }
    }

    pub fn poll(&mut self) -> std::io::Result<Activity> {
        if self.path.is_none() {
            self.path = locate(self.format, &self.session_ref);
        }
        let Some(path) = &self.path else { return Ok(Activity::default()) };
        let mut file = File::open(path)?;
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
            match self.format {
                Format::Omp => self.scan_omp(line, &mut found),
                Format::Claude => self.scan_claude(line, &mut found),
            }
        }
        self.pending = lines[complete..].to_vec();
        Ok(found)
    }

    /// Whether `line` may hold the result of a pending `gh pr create` call.
    fn may_hold_pr_result(&self, line: &[u8], marker: &[u8]) -> bool {
        contains(line, marker) && self.pr_calls.iter().any(|id| contains(line, id.as_bytes()))
    }

    fn scan_omp(&mut self, line: &[u8], found: &mut Activity) {
        let is_session = contains(line, b"\"type\":\"session\"");
        if !is_session && !self.may_hold_pr_result(line, b"\"toolResult\"") && !contains(line, b"\"toolCall\"") {
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
            let text: String = message["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
            self.record_result(message["toolCallId"].as_str().unwrap_or_default(), &text, found);
            return;
        }
        for block in message["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "toolCall") {
            self.record_call(block["id"].as_str(), &omp_call(&block["name"], &block["arguments"]), found);
        }
    }

    fn scan_claude(&mut self, line: &[u8], found: &mut Activity) {
        if !contains(line, b"\"tool_use\"") && !self.may_hold_pr_result(line, b"\"tool_result\"") {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else { return };
        if let Some(cwd) = entry["cwd"].as_str() {
            self.cwd = PathBuf::from(cwd);
        }
        for block in entry["message"]["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("tool_use") => self.record_call(block["id"].as_str(), &claude_call(&block["name"], &block["input"]), found),
                Some("tool_result") => {
                    let content = &block["content"];
                    let text: String = match content.as_str() {
                        Some(text) => text.to_string(),
                        None => content.as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect(),
                    };
                    self.record_result(block["tool_use_id"].as_str().unwrap_or_default(), &text, found);
                }
                _ => {}
            }
        }
    }

    fn record_call(&mut self, id: Option<&str>, call: &Call, found: &mut Activity) {
        if let (Call::Shell { command, .. }, Some(id)) = (call, id) {
            if creates_pr(&shell_tokens(command)) {
                self.pr_calls.insert(id.to_string());
            }
        }
        found.touches.extend(call_touches(call, &self.cwd));
    }

    fn record_result(&mut self, id: &str, text: &str, found: &mut Activity) {
        if self.pr_calls.remove(id) {
            found.prs.extend(github::pr_urls(text));
        }
    }
}

/// The transcript file for a herdr session reference: the reference itself when it is a file,
/// else (Claude) `~/.claude/projects/<project>/<session-id>.jsonl`.
fn locate(format: Format, session_ref: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(session_ref);
    if direct.is_file() {
        return Some(direct);
    }
    if format != Format::Claude || session_ref.contains('/') {
        return None;
    }
    let projects = PathBuf::from(std::env::var_os("HOME")?).join(".claude/projects");
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|project| project.path().join(format!("{session_ref}.jsonl")))
        .find(|candidate| candidate.is_file())
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

/// A tool call reduced to what matters for repo detection, independent of the agent.
#[derive(Debug)]
enum Call<'a> {
    Shell { command: &'a str, cwd: Option<&'a str> },
    Paths { paths: Vec<&'a str>, writes: bool },
    Other,
}

fn omp_call<'a>(name: &Value, args: &'a Value) -> Call<'a> {
    let path = || args["path"].as_str().unwrap_or_default();
    match name.as_str().unwrap_or_default() {
        "bash" => Call::Shell { command: args["command"].as_str().unwrap_or_default(), cwd: args["cwd"].as_str() },
        "edit" => Call::Paths { paths: edit_targets(args["input"].as_str().unwrap_or_default()), writes: true },
        "write" => Call::Paths { paths: vec![path()], writes: true },
        // read, grep, glob, find, …: `path` may list several, `;`-separated.
        _ => Call::Paths { paths: path().split(';').collect(), writes: false },
    }
}

fn claude_call<'a>(name: &Value, input: &'a Value) -> Call<'a> {
    let field = |key: &str| vec![input[key].as_str().unwrap_or_default()];
    match name.as_str().unwrap_or_default() {
        "Bash" => Call::Shell { command: input["command"].as_str().unwrap_or_default(), cwd: None },
        "Edit" | "MultiEdit" | "Write" => Call::Paths { paths: field("file_path"), writes: true },
        "NotebookEdit" => Call::Paths { paths: field("notebook_path"), writes: true },
        "Read" => Call::Paths { paths: field("file_path"), writes: false },
        "Grep" | "Glob" | "LS" => Call::Paths { paths: field("path"), writes: false },
        _ => Call::Other,
    }
}

/// Absolute paths one tool call works on. Paths may still carry selectors (`file.rs:10-20`)
/// or globs; callers resolve them to repos by walking ancestors.
fn call_touches(call: &Call, base: &Path) -> Vec<Touch> {
    let touches = |paths: &[&str], base: &Path, writes: bool| -> Vec<Touch> {
        paths.iter().filter_map(|p| resolve(p, base)).map(|path| Touch { path, writes }).collect()
    };
    match call {
        Call::Shell { command, cwd } => {
            let tokens = shell_tokens(command);
            let dir = cwd.and_then(|c| resolve(c, base)).unwrap_or_else(|| base.to_path_buf());
            let writes = git_writes(&tokens);
            let mut found = vec![Touch { path: dir.clone(), writes }];
            found.extend(touches(&command_dirs(&tokens), &dir, writes));
            found
        }
        Call::Paths { paths, writes } => touches(paths, base, *writes),
        Call::Other => Vec::new(),
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

    /// (path, writes) pairs for one omp tool call made from session cwd `/work/app`.
    fn touches(name: &str, args: Value) -> Vec<(String, bool)> {
        pairs(call_touches(&omp_call(&json!(name), &args), Path::new("/work/app")))
    }

    /// Same for a Claude Code tool call.
    fn claude_touches(name: &str, input: Value) -> Vec<(String, bool)> {
        pairs(call_touches(&claude_call(&json!(name), &input), Path::new("/work/app")))
    }

    fn pairs(touches: Vec<Touch>) -> Vec<(String, bool)> {
        touches.into_iter().map(|t| (t.path.display().to_string(), t.writes)).collect()
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

    #[test]
    fn claude_tools_map_to_reads_and_writes() {
        assert_eq!(claude_touches("Edit", json!({"file_path": "/r/a.rs", "old_string": "x"})), owned(&[("/r/a.rs", true)]));
        assert_eq!(claude_touches("MultiEdit", json!({"file_path": "src/b.rs"})), owned(&[("/work/app/src/b.rs", true)]));
        assert_eq!(claude_touches("NotebookEdit", json!({"notebook_path": "/r/n.ipynb"})), owned(&[("/r/n.ipynb", true)]));
        assert_eq!(claude_touches("Read", json!({"file_path": "/r/c.rs"})), owned(&[("/r/c.rs", false)]));
        assert_eq!(claude_touches("Grep", json!({"pattern": "x", "path": "../lib"})), owned(&[("/work/lib", false)]));
        assert_eq!(
            claude_touches("Bash", json!({"command": "cd ../lib && git commit -qm x", "description": "commit"})),
            owned(&[("/work/app", true), ("/work/lib", true)])
        );
        assert!(claude_touches("WebFetch", json!({"url": "https://example.com"})).is_empty());
    }

    /// Writes `content` to a fresh transcript file and returns a reader positioned at its start.
    fn transcript(name: &str, format: Format, content: &str) -> (Transcript, PathBuf) {
        let dir = std::env::temp_dir().join(format!("git-sidebar-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.jsonl");
        std::fs::write(&file, content).unwrap();
        (Transcript::new(format, file.display().to_string(), PathBuf::from("/fallback")), file)
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
        let (mut t, file) =
            transcript("lines", Format::Omp, &format!("{{\"type\":\"session\",\"cwd\":\"/repo\"}}\n{}\n{}", call("a"), &b[..20]));
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
            Format::Omp,
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

    #[test]
    fn claude_lines_use_their_own_cwd_and_yield_prs_from_string_results() {
        let tool_use = |cwd: &str, id: &str, name: &str, input: &str| {
            format!(r#"{{"type":"assistant","cwd":"{cwd}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#)
        };
        let tool_result = |id: &str, content: &str| {
            format!(r#"{{"type":"user","cwd":"/a","message":{{"role":"user","content":[{{"tool_use_id":"{id}","type":"tool_result","content":{content}}}]}}}}"#)
        };
        let (mut t, file) = transcript(
            "claude",
            Format::Claude,
            &format!(
                "{}\n{}\n{}\n{}\n",
                tool_use("/a", "u1", "Write", r#"{"file_path":"notes.md","content":"x"}"#),
                tool_use("/b", "u2", "Bash", r#"{"command":"gh pr create --fill","description":"open PR"}"#),
                tool_result("u2", r#""* Request took 1.6s\nhttps://github.com/o/r/pull/19""#),
                tool_result("u9", r#"[{"type":"text","text":"https://github.com/o/r/pull/99"}]"#),
            ),
        );
        let activity = t.poll().unwrap();
        assert_eq!(
            pairs(activity.touches),
            owned(&[("/a/notes.md", true), ("/b", false)]),
            "relative paths resolve against each entry's cwd"
        );
        assert_eq!(activity.prs, [PrRef { owner: "o".into(), repo: "r".into(), number: 19 }], "u9 was no gh pr create");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }
}
