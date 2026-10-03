//! Locating git checkouts: working directories of a pane's process tree, and the checkout
//! enclosing any path.

use std::path::{Path, PathBuf};

/// Working directories of `root_pid` and all its descendants, found through each thread's
/// `/proc/<pid>/task/<tid>/children` list (Linux) instead of scanning every process.
pub fn process_tree_cwds(root_pid: u32) -> Vec<PathBuf> {
    let mut cwds = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
            cwds.push(cwd);
        }
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for task in tasks.flatten() {
            if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                stack.extend(children.split_whitespace().filter_map(|c| c.parse::<u32>().ok()));
            }
        }
    }
    cwds
}

/// The git checkout containing `path` (a `.git` directory, or a `.git` file for worktrees).
/// `path` need not exist: tool paths may be files, globs, or carry `:line` selectors.
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree_reaches_grandchildren() {
        let dir = std::env::temp_dir().join(format!("tracks-proc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // sh (this test's child) starts sleep in `dir`: a grandchild of this process.
        let mut sh = std::process::Command::new("sh")
            .args(["-c", "cd \"$0\" && sleep 5 & wait"])
            .arg(&dir)
            .spawn()
            .unwrap();
        let found = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            process_tree_cwds(std::process::id()).contains(&dir)
        });
        let _ = sh.kill();
        let _ = sh.wait();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(found, "the grandchild's working directory is listed");
    }
}
