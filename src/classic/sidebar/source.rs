//! The chat a sidebar follows: an agent pane of its tab, as herdr describes it.

use crate::classic::herdr::{self, PaneInfo};
use anyhow::Result;
use herdr_tracks::engine::{repos, Chat};
use std::path::PathBuf;

/// The agent pane of `me`'s tab to follow: the focused one, else `previous` (the one followed
/// so far), else the first.
pub(super) fn find(me: &str, previous: Option<&str>) -> Result<Option<Chat>> {
    let layout = herdr::pane_layout(me)?;
    let panes: Vec<PaneInfo> = herdr::pane_list()?
        .into_iter()
        .filter(|p| p.tab_id == layout.tab_id && p.pane_id != me && p.agent.is_some())
        .collect();
    let Some(target) = panes
        .iter()
        .find(|p| p.pane_id == layout.focused_pane_id)
        .or_else(|| panes.iter().find(|p| Some(p.pane_id.as_str()) == previous))
        .or_else(|| panes.first())
    else {
        return Ok(None);
    };
    let mut cwds: Vec<PathBuf> = [&target.foreground_cwd, &target.cwd].into_iter().flatten().map(PathBuf::from).collect();
    if let Some(pid) = herdr::pane_process_info(&target.pane_id)?.shell_pid {
        cwds.extend(repos::process_tree_cwds(pid));
    }
    Ok(Some(Chat {
        pane: target.pane_id.clone(),
        terminal: target.terminal_id.clone(),
        agent: target.agent.clone(),
        session: target.agent_session.as_ref().map(|s| s.value.clone()),
        title: target.terminal_title_stripped.clone().or_else(|| target.agent.clone()).unwrap_or_default(),
        cwd: target.cwd.clone().map(PathBuf::from),
        cwds,
    }))
}
