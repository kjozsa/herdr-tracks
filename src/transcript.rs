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
    /// Pull requests the chat opened or referred to: URLs in user messages and tool calls,
    /// omp `pr://owner/repo/N` paths, `gh pr <verb> N -R owner/repo`.
    pub prs: Vec<PrRef>,
    /// Pull requests referred to by number only (`pr://N`, `gh pr checkout N`), with the
    /// directory whose repository they belong to.
    pub pr_numbers: Vec<(u32, PathBuf)>,
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
        let is_user_link = contains(line, b"\"role\":\"user\"") && contains(line, b"github.com/");
        if !is_session && !is_user_link && !self.may_hold_pr_result(line, b"\"toolResult\"") && !contains(line, b"\"toolCall\"")
        {
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
        match message["role"].as_str() {
            Some("toolResult") => {
                let text: String = message["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
                self.record_result(message["toolCallId"].as_str().unwrap_or_default(), &text, found);
            }
            Some("user") => found.prs.extend(github::pr_urls(&message_text(&message["content"]))),
            _ => {
                for block in message["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "toolCall") {
                    let args = &block["arguments"];
                    self.record_call(block["id"].as_str(), &omp_call(&block["name"], args), args, found);
                }
            }
        }
    }

    fn scan_claude(&mut self, line: &[u8], found: &mut Activity) {
        let is_user_link = contains(line, b"\"type\":\"user\"") && contains(line, b"github.com/");
        if !is_user_link && !contains(line, b"\"tool_use\"") && !self.may_hold_pr_result(line, b"\"tool_result\"") {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else { return };
        if let Some(cwd) = entry["cwd"].as_str() {
            self.cwd = PathBuf::from(cwd);
        }
        let content = &entry["message"]["content"];
        if entry["type"] == "user" {
            // The user's own words: a plain string or text blocks (not tool results).
            found.prs.extend(github::pr_urls(&message_text(content)));
        }
        for block in content.as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("tool_use") => {
                    let input = &block["input"];
                    self.record_call(block["id"].as_str(), &claude_call(&block["name"], input), input, found);
                }
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

    fn record_call(&mut self, id: Option<&str>, call: &Call, args: &Value, found: &mut Activity) {
        // Only arguments that name things (paths, commands, URLs): file contents being written
        // may well link pull requests the chat has nothing to do with.
        const CONTENT: &[&str] = &["content", "input", "new_string", "old_string", "code", "edits"];
        let raw = args
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| !CONTENT.contains(&key.as_str()))
            .filter_map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        found.prs.extend(github::pr_urls(&raw));
        for mention in pr_scheme_refs(&raw) {
            match mention {
                PrMention::Full(pr) => found.prs.push(pr),
                PrMention::Number(n) => found.pr_numbers.push((n, self.cwd.clone())),
            }
        }
        if let Call::Shell { command, cwd } = call {
            let tokens = shell_tokens(command);
            if let (true, Some(id)) = (creates_pr(&tokens), id) {
                self.pr_calls.insert(id.to_string());
            }
            let dir = cwd.and_then(|c| resolve(c, &self.cwd)).unwrap_or_else(|| self.cwd.clone());
            for mention in gh_pr_refs(command) {
                match mention {
                    PrMention::Full(pr) => found.prs.push(pr),
                    PrMention::Number(n) => found.pr_numbers.push((n, dir.clone())),
                }
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

/// Text of a message's content: a plain string or the `text` of its blocks.
fn message_text(content: &Value) -> String {
    match content.as_str() {
        Some(text) => text.to_string(),
        None => content.as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
    }
}

/// A pull request named in full, or by number within some repository.
#[derive(Debug, PartialEq)]
enum PrMention {
    Full(PrRef),
    Number(u32),
}

/// omp's pull-request URIs: `pr://owner/repo/N[/…]` and `pr://N[/…]` (the current repo).
fn pr_scheme_refs(text: &str) -> Vec<PrMention> {
    let mut found = Vec::new();
    for (at, _) in text.match_indices("pr://") {
        let rest = &text[at + "pr://".len()..];
        let end = rest.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ';' | ':' | '?' | '#')).unwrap_or(rest.len());
        let parts: Vec<&str> = rest[..end].split('/').collect();
        let number = |s: &str| s.parse::<u32>().ok();
        match parts.as_slice() {
            [owner, repo, n, ..] if number(n).is_some() && !owner.is_empty() && !repo.is_empty() => {
                found.push(PrMention::Full(PrRef { owner: owner.to_string(), repo: repo.to_string(), number: number(n).unwrap_or(0) }))
            }
            [n, ..] if number(n).is_some() => found.push(PrMention::Number(number(n).unwrap_or(0))),
            _ => {}
        }
    }
    found
}

/// `gh pr <verb> N` for verbs that act on an existing PR, with `-R/--repo owner/repo` if given.
/// Each `;`/`|`/`&&` segment is its own command, and the PR is the verb's first positional
/// argument: `gh pr diff 5 | head -n 20` names #5, never #20.
fn gh_pr_refs(command: &str) -> Vec<PrMention> {
    const VERBS: &[&str] =
        &["view", "diff", "checkout", "review", "comment", "merge", "edit", "ready", "close", "reopen", "checks"];
    /// Flags whose next token is their value, not a positional argument.
    const VALUED: &[&str] =
        &["-R", "--repo", "--json", "--jq", "-q", "-t", "--template", "-b", "--body", "-F", "--body-file"];
    let mut found = Vec::new();
    for segment in command.split(|c| matches!(c, ';' | '|' | '&' | '\n' | '(' | ')')) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let Some(at) = tokens.windows(3).position(|w| w[0] == "gh" && w[1] == "pr" && VERBS.contains(&w[2])) else {
            continue;
        };
        let args = &tokens[at + 3..];
        let mut repo = None;
        let mut positional = None;
        let mut i = 0;
        while i < args.len() {
            match args[i] {
                "-R" | "--repo" => repo = args.get(i + 1).copied(),
                flag if flag.starts_with("--repo=") => repo = flag.strip_prefix("--repo="),
                _ => {}
            }
            if VALUED.contains(&args[i]) {
                i += 2;
                continue;
            }
            if !args[i].starts_with('-') && positional.is_none() {
                positional = Some(args[i]);
            }
            i += 1;
        }
        let Some(number) = positional.and_then(|p| p.trim_start_matches('#').parse::<u32>().ok()) else { continue };
        found.push(match repo.and_then(|r| r.split_once('/')) {
            Some((owner, repo)) => PrMention::Full(PrRef { owner: owner.into(), repo: repo.into(), number }),
            None => PrMention::Number(number),
        });
    }
    found
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
        let dir = std::env::temp_dir().join(format!("tracks-test-{}-{name}", std::process::id()));
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

    #[test]
    fn gh_pr_verbs_and_pr_uris_name_pull_requests() {
        let pr = |owner: &str, repo: &str, number| PrMention::Full(PrRef { owner: owner.into(), repo: repo.into(), number });
        let gh = gh_pr_refs;
        assert!(gh("gh pr diff https://github.com/o/r/pull/3 | grep -n '^diff' | head -n 5").is_empty(), "URLs are found elsewhere");
        assert!(gh("gh pr view --help | head -n 6").is_empty());
        assert_eq!(gh("gh pr diff 5 --color=never | head -n 20"), [PrMention::Number(5)]);
        assert_eq!(gh("gh pr checkout 36 && git log"), [PrMention::Number(36)]);
        assert_eq!(gh("gh pr view --json title 12 -R Kamuno-CH/e2e_testing"), [pr("Kamuno-CH", "e2e_testing", 12)]);
        assert_eq!(gh("gh pr review #7 --approve --repo=o/r"), [pr("o", "r", 7)]);
        assert!(gh("gh pr list --limit 5; gh pr create --fill; gh pr view --web").is_empty());
        assert_eq!(
            pr_scheme_refs("pr://kamuno-ch/e2e_testing/36/diff/all and \"pr://41\""),
            [pr("kamuno-ch", "e2e_testing", 36), PrMention::Number(41)]
        );
        assert!(pr_scheme_refs("pr://kamuno-ch/e2e_testing").is_empty());
    }

    #[test]
    fn referenced_prs_come_from_user_words_and_call_targets_not_contents() {
        let line = |role: &str, content: &str| format!(r#"{{"type":"message","message":{{"role":"{role}","content":{content}}}}}"#);
        let call = |name: &str, args: &str| {
            line("assistant", &format!(r#"[{{"type":"toolCall","id":"x","name":"{name}","arguments":{args}}}]"#))
        };
        let (mut t, file) = transcript(
            "refs",
            Format::Omp,
            &format!(
                "{}\n",
                [
                    r#"{"type":"session","cwd":"/work/app"}"#.to_string(),
                    line("user", r#"[{"type":"text","text":"review https://github.com/o/r/pull/36/changes"}]"#),
                    call("read", r#"{"path":"pr://o/other/5/diff/all"}"#),
                    call("bash", r#"{"command":"gh pr checkout 9","cwd":"../lib"}"#),
                    call("write", r#"{"path":"CHANGELOG.md","content":"fixed in https://github.com/o/r/pull/99"}"#),
                    line("toolResult", r#"[{"type":"text","text":"see https://github.com/o/r/pull/98"}]"#),
                ]
                .join("\n")
            ),
        );
        let activity = t.poll().unwrap();
        let numbers: Vec<_> = activity.prs.iter().map(|p| (p.repo.as_str(), p.number)).collect();
        assert_eq!(numbers, [("r", 36), ("other", 5)], "file contents and tool output do not count");
        assert_eq!(activity.pr_numbers, [(9, PathBuf::from("/work/lib"))], "numbers carry the command's directory");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }
}
