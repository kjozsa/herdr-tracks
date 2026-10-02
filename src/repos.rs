//! Locating git checkouts: working directories of a pane's process tree, and the checkout
//! enclosing any path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Working directories of `root_pid` and all its descendants (Linux `/proc`).
pub fn process_tree_cwds(root_pid: u32) -> Vec<PathBuf> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if let Some(ppid) = parent_pid(pid) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut cwds = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
            cwds.push(cwd);
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    cwds
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid ...`; comm may contain spaces and parens.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The git checkout containing `path` (a `.git` directory, or a `.git` file for worktrees).
/// `path` need not exist: tool paths may be files, globs, or carry `:line` selectors.
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}
