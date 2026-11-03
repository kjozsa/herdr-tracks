//! The background tracker: one [`Tracker`] per agent terminal of the local herdr-core, each
//! written to `models/<terminal>.json` whenever it changes.

use super::core::{self, Agent};
use super::{dir, heartbeat_file, inbox_dir, model, models_dir};
use anyhow::Result;
use herdr_tracks::engine::github::PrRef;
use herdr_tracks::engine::patch::CommitAt;
use herdr_tracks::engine::state::write_atomic;
use herdr_tracks::engine::{repos, Chat, Target, Tracker};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, TryLockError};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// How often the models are rebuilt and the inbox is read.
const TICK: Duration = Duration::from_millis(300);
/// How often herdr-core is asked for its terminals.
const LIST_EVERY: Duration = Duration::from_secs(1);
/// Without an `ensure` for this long, nothing shows the models any more: `serve` ends.
const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

/// What the mod asks of a chat, sent with `app send`.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Command {
    /// List a commit's files.
    Files(CommitAt),
    Dismiss(Dismiss),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Dismiss {
    Repo(PathBuf),
    Pr(PrRef),
    Link(String),
    Links,
}

impl From<Dismiss> for Target {
    fn from(d: Dismiss) -> Self {
        match d {
            Dismiss::Repo(root) => Target::Repo(root),
            Dismiss::Pr(pr) => Target::Pr(pr),
            Dismiss::Link(url) => Target::Link(url),
            Dismiss::Links => Target::Links,
        }
    }
}

#[derive(Deserialize)]
struct Queued {
    terminal: String,
    command: Command,
}

fn lock_file() -> Result<File> {
    std::fs::create_dir_all(dir())?;
    Ok(File::create(dir().join("serve.lock"))?)
}

/// Whether a `serve` holds the lock.
pub fn running() -> Result<bool> {
    match lock_file()?.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Runs until no mod has asked for it for [`IDLE_EXIT`], or until its binary is rebuilt (the
/// next `ensure` starts the new one). Another `serve` running: returns at once.
pub fn serve() -> Result<()> {
    let lock = lock_file()?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(models_dir());
    let mut trackers: BTreeMap<String, Tracker> = BTreeMap::new();
    let mut written: HashMap<String, Vec<u8>> = HashMap::new();
    let mut agents: Vec<Agent> = Vec::new();
    let mut next_list = Instant::now();
    while !idle() && !replaced() {
        if Instant::now() >= next_list {
            next_list = Instant::now() + LIST_EVERY;
            match core::agents() {
                Ok(live) => agents = live,
                Err(e) => eprintln!("herdr-core: {e:#}"),
            }
            trackers.retain(|address, _| agents.iter().any(|a| a.address == *address));
            written.retain(|address, _| {
                let live = trackers.contains_key(address);
                if !live {
                    let _ = std::fs::remove_file(model_file(address));
                }
                live
            });
        }
        for queued in take_inbox() {
            let Some(tracker) = trackers.get_mut(&queued.terminal) else { continue };
            match queued.command {
                Command::Files(commit) => tracker.fetch_files(&commit),
                Command::Dismiss(what) => {
                    let result = tracker.dismiss(&what.into());
                    tracker.note(result);
                }
            }
        }
        for agent in &agents {
            let tracker = trackers.entry(agent.address.clone()).or_default();
            tracker.refresh(|_| Ok(Some(chat(agent))));
            let bytes = serde_json::to_vec(&model::build(&agent.address, tracker))?;
            if written.get(&agent.address) != Some(&bytes) {
                write_atomic(&model_file(&agent.address), &bytes)?;
                written.insert(agent.address.clone(), bytes);
            }
        }
        std::thread::sleep(TICK);
    }
    Ok(())
}

fn model_file(address: &str) -> PathBuf {
    models_dir().join(format!("{address}.json"))
}

fn idle() -> bool {
    let touched = std::fs::metadata(heartbeat_file()).and_then(|m| m.modified());
    touched.is_ok_and(|t| SystemTime::now().duration_since(t).is_ok_and(|age| age > IDLE_EXIT))
}

/// Whether the binary was rebuilt since this process started (Linux marks the old one deleted).
fn replaced() -> bool {
    std::fs::read_link("/proc/self/exe").is_ok_and(|exe| exe.to_string_lossy().ends_with(" (deleted)"))
}

/// The commands queued since the last call, oldest first; unreadable ones are dropped.
fn take_inbox() -> Vec<Queued> {
    let Ok(entries) = std::fs::read_dir(inbox_dir()) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            let queued = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let _ = std::fs::remove_file(&file);
            queued
        })
        .collect()
}

/// The agent as the engine follows it. Claude Code names its session in
/// `~/.claude/sessions/<pid>.json`, for its own process somewhere in the terminal's tree.
fn chat(agent: &Agent) -> Chat {
    let tree = agent.pid.map(repos::process_tree).unwrap_or_default();
    let session = (agent.app == "claude-code").then(|| claude_session(agent.foreground_pid.into_iter().chain(tree.iter().copied()))).flatten();
    let mut cwds: Vec<PathBuf> = [&agent.foreground_cwd, &agent.cwd].into_iter().flatten().map(PathBuf::from).collect();
    cwds.extend(tree.iter().filter_map(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok()));
    Chat {
        pane: agent.address.clone(),
        terminal: agent.address.clone(),
        agent: Some(agent.app.clone()),
        session,
        title: agent.title.clone(),
        cwd: agent.foreground_cwd.clone().or_else(|| agent.cwd.clone()).map(PathBuf::from),
        cwds,
    }
}

fn claude_session(pids: impl Iterator<Item = u32>) -> Option<String> {
    let sessions = PathBuf::from(std::env::var_os("HOME")?).join(".claude/sessions");
    pids.into_iter().find_map(|pid| {
        let file: serde_json::Value = serde_json::from_slice(&std::fs::read(sessions.join(format!("{pid}.json"))).ok()?).ok()?;
        file["sessionId"].as_str().map(str::to_string)
    })
}
