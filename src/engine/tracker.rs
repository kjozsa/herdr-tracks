//! The followed chat and everything gathered about it, refreshed on its own schedule.

use super::collect::{self, PrBatch};
use super::git::{Commit, FileChange, RepoStatus};
use super::github::{self, PrRef, PrView};
use super::patch::CommitAt;
use super::state::{self, Dismissal, SessionState};
use super::transcript::Transcript;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const GIT_EVERY: Duration = Duration::from_secs(5);
const PR_EVERY: Duration = Duration::from_secs(30);

/// An agent chat as a frontend finds it: where it runs and what the frontend knows about it.
#[derive(Debug, Clone, Default)]
pub struct Chat {
    /// The frontend's id of the agent's pane: preferred while it hosts an agent.
    pub pane: String,
    /// The agent's terminal: keys the chat's (unpersisted) state when it names no session.
    pub terminal: String,
    /// The agent, as the frontend names it (`claude`, `omp`, ...).
    pub agent: Option<String>,
    /// The agent's own session reference: a transcript path, or (Claude) a session id.
    pub session: Option<String>,
    pub title: String,
    /// Where the agent runs: the base of relative tool paths until its transcript names one.
    pub cwd: Option<PathBuf>,
    /// Working directories of the agent and its processes now; repos under them are touched.
    pub cwds: Vec<PathBuf>,
}

/// The chat followed, and what of it outlives the frontend.
pub struct Session {
    /// The frontend's id of the agent's pane.
    pub pane: String,
    pub title: String,
    pub saved: SessionState,
    /// Whether `saved` has a session file (the agent named its session).
    pub persisted: bool,
    /// Transcript being tailed (omp, Claude Code); other agents rely on working dirs only.
    pub transcript: Option<Transcript>,
    /// Web links the chat mentioned, newest first; rebuilt from the transcript on start.
    pub links: Vec<String>,
    /// The chat's finished tool calls, as (start, end) unix milliseconds; rebuilt from the
    /// transcript on start. Commits made within them are the chat's.
    pub calls: Vec<(i64, i64)>,
}

impl Session {
    /// Writes what the chat keeps to its session file, when it has one.
    pub fn save(&self) -> Result<()> {
        if self.persisted {
            state::save_session(&self.saved)?;
        }
        Ok(())
    }

    /// The links to list, newest first: neither dismissed nor a pull request listed already.
    pub fn shown_links(&self) -> Vec<&String> {
        let saved = &self.saved;
        let listed = |url: &str| github::pr_urls(url).iter().any(|pr| saved.prs.iter().any(|p| p.pr.same(pr)));
        self.links.iter().filter(|url| !listed(url) && !saved.link_dismissed(url)).collect()
    }
}

/// What the user can dismiss: a repo, a pull request, a link, or all links.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Repo(PathBuf),
    Pr(PrRef),
    Link(String),
    Links,
}

/// `git status` of each shown repo.
pub type Statuses = HashMap<PathBuf, Result<RepoStatus, String>>;

/// A commit's file list, fetched in the background, by hash.
type FilesFetched = (String, Result<Vec<FileChange>, String>);

/// When each kind of data is next refreshed.
struct Due {
    sample: Instant,
    git: Instant,
    pr: Instant,
}

impl Default for Due {
    fn default() -> Self {
        let now = Instant::now();
        Due { sample: now, git: now, pr: now }
    }
}

#[derive(Default)]
pub struct Tracker {
    pub session: Option<Session>,
    pub statuses: Statuses,
    /// Commits the chat made, per repo, newest first.
    pub commits: HashMap<PathBuf, Vec<Commit>>,
    /// Each pull request as last fetched, by URL: its status and title, and its commits, merge
    /// commit and last activity: the chat's commits among them are listed under the pull
    /// request, which sorts by the newest of them.
    pub pr_views: HashMap<String, PrView>,
    /// Last `gh` failure per PR URL.
    pub pr_errors: HashMap<String, String>,
    /// github.com `owner/repo` slugs of each checkout's remotes.
    pub remotes: HashMap<PathBuf, Vec<String>>,
    /// Files of commits asked for with [`Tracker::fetch_files`], by hash; absent while loading.
    pub commit_files: HashMap<String, Result<Vec<FileChange>, String>>,
    /// The last failure, shown until the next successful sample.
    pub error: Option<String>,
    /// In-flight PR status refresh (runs `gh` off the frontend's thread).
    pr_job: Option<Receiver<PrBatch>>,
    /// In-flight file list fetches (run `git` or `gh` off the frontend's thread).
    files_jobs: Vec<Receiver<FilesFetched>>,
    due: Due,
}

impl Tracker {
    /// Keeps a failure to show; the frontend keeps running.
    pub fn note(&mut self, result: Result<()>) {
        if let Err(e) = result {
            self.error = Some(format!("{e:#}"));
        }
    }

    /// When [`Tracker::refresh`] next has something to do, fetches in flight aside.
    pub fn next_due(&self) -> Instant {
        self.due.sample.min(self.due.git)
    }

    /// Refreshes whatever is due: the followed chat (every second; `find` picks it, given the
    /// pane of the one followed so far), git status (every 5 s, or at once when new repos
    /// appear), pull-request status (every 30 s, or at once when a PR has not been fetched
    /// yet) and fetched file lists.
    pub fn refresh(&mut self, find: impl FnOnce(Option<&str>) -> Result<Option<Chat>>) {
        let now = Instant::now();
        if now >= self.due.sample {
            self.due.sample = now + SAMPLE_EVERY;
            let previous = self.session.as_ref().map(|s| s.pane.clone());
            match find(previous.as_deref()).and_then(|chat| collect::sample(self, chat.as_ref())) {
                Ok(new_repos) => {
                    self.error = None;
                    if new_repos {
                        self.due.git = now;
                    }
                }
                Err(e) => self.note(Err(e)),
            }
        }
        if now >= self.due.git {
            self.due.git = now + GIT_EVERY;
            (self.statuses, self.commits) = collect::refresh_git(self);
            let result = collect::track_changes(self);
            self.note(result);
        }
        if let Some(job) = &self.pr_job {
            match job.try_recv() {
                Ok(batch) => {
                    self.pr_job = None;
                    collect::apply_prs(self, batch);
                }
                Err(TryRecvError::Disconnected) => self.pr_job = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.pr_job.is_none() && (now >= self.due.pr || collect::has_unfetched_pr(self)) {
            self.due.pr = now + PR_EVERY;
            self.pr_job = collect::start_pr_refresh(self);
        }
        let Tracker { files_jobs, commit_files, .. } = self;
        files_jobs.retain(|job| match job.try_recv() {
            Ok((hash, files)) => {
                commit_files.insert(hash, files);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
    }

    /// Fetches a commit's file list in the background, once; it lands in `commit_files`.
    pub fn fetch_files(&mut self, commit: &CommitAt) {
        if self.commit_files.contains_key(&commit.hash) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let commit = commit.clone();
        std::thread::spawn(move || {
            let files = commit.files();
            let _ = tx.send((commit.hash, files));
        });
        self.files_jobs.push(rx);
    }

    /// Hides a pull request from this chat for good, a repo (with its pull requests) until the
    /// chat touches it again, or a link (or all of them) until the chat mentions it again.
    pub fn dismiss(&mut self, target: &Target) -> Result<()> {
        let Some(session) = self.session.as_mut() else { return Ok(()) };
        let at = session.transcript.as_ref().map_or(0, Transcript::position);
        let what = match target {
            Target::Repo(root) => Dismissal::Repo(root),
            Target::Pr(pr) => Dismissal::Pr(pr),
            Target::Link(url) => Dismissal::Links(std::slice::from_ref(url)),
            Target::Links => Dismissal::Links(&session.links),
        };
        if session.saved.dismiss(what, at) {
            session.save()?;
        }
        Ok(())
    }
}

/// Trackers in known states, for tests here and in frontends.
#[doc(hidden)]
pub mod fixture {
    use super::*;
    use crate::engine::github::{PrCommit, PrState, PrStatus};
    use crate::engine::state::{PrRecord, RepoRecord};

    pub fn root(name: &str) -> PathBuf {
        PathBuf::from(format!("/r/{name}"))
    }

    pub fn pr(repo: &str, number: u32) -> PrRef {
        PrRef { owner: "o".into(), repo: repo.into(), number }
    }

    /// A clean checkout of `main`.
    pub fn clean() -> RepoStatus {
        RepoStatus { branch: "main".into(), ahead: 0, behind: 0, changes: Vec::new(), fingerprint: 0 }
    }

    /// Following a chat (not persisted) that changed the repos named (each [`root`], a
    /// [`clean`] checkout) and named the PRs, each under its repo's checkout when that is one of
    /// the repos.
    pub fn fixture(repos: &[&str], prs: &[PrRef]) -> Tracker {
        let mut tracker = Tracker::default();
        for name in repos {
            tracker.statuses.insert(root(name), Ok(clean()));
        }
        let records = repos.iter().map(|name| RepoRecord { root: root(name), changed: true, baseline: None }).collect();
        let prs = prs.iter().map(|p| PrRecord { pr: p.clone(), root: repos.contains(&p.repo.as_str()).then(|| root(&p.repo)) });
        let saved = SessionState { key: "s".into(), repos: records, prs: prs.collect(), ..SessionState::default() };
        let (title, links, calls) = (String::new(), Vec::new(), Vec::new());
        tracker.session = Some(Session { pane: "w:p1".into(), title, saved, persisted: false, transcript: None, links, calls });
        tracker
    }

    pub fn session(tracker: &mut Tracker) -> &mut Session {
        tracker.session.as_mut().expect("fixture follows a chat")
    }

    /// A fetched pull request without checks, active on GitHub at time 1.
    pub fn view(state: PrState, commits: Vec<PrCommit>) -> PrView {
        let title = "lodge every beneficial owner as owner too".into();
        PrView { status: PrStatus { state, checks: None }, title, head: "feature".into(), commits, merge: None, updated: Some(1) }
    }

    pub fn gh(oid: &str, subject: &str, time: i64) -> PrCommit {
        PrCommit { oid: oid.into(), subject: Some(subject.into()), time: Some(time) }
    }
}
