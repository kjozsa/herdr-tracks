//! Docking the sidebar as a right-edge split of a tab, sized to herdr's left sidebar.

use crate::herdr::{self, Layout, PaneInfo};
use crate::state::DockGuard;
use anyhow::{anyhow, bail, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// herdr's defaults for `[ui] sidebar_width`, `sidebar_min_width`, `sidebar_max_width`.
const DEFAULT_WIDTH: u32 = 26;
const DEFAULT_MIN_WIDTH: u32 = 18;
const DEFAULT_MAX_WIDTH: u32 = 36;
/// Columns left for the rest of the tab; narrower tabs are not auto-docked.
const MIN_MAIN_WIDTH: u32 = 40;
/// herdr clamps split ratios to 0.1..=0.9.
const MAX_RATIO: f64 = 0.9;

pub fn plugin_id() -> String {
    std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| crate::PLUGIN_ID.to_string())
}

/// Tab the invocation is about: the event's or focused tab from herdr's plugin context.
fn context_tab() -> Result<String> {
    std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
        .and_then(|v| v["tab_id"].as_str().map(str::to_string))
        .or_else(|| std::env::var("HERDR_TAB_ID").ok().filter(|t| !t.is_empty()))
        .ok_or_else(|| anyhow!("no tab in plugin context; run from inside herdr"))
}

/// Event hooks: keep a sidebar in the context tab whenever it hosts an agent. A closed sidebar
/// docks again straight away (`pane.closed`): it is not meant to be dismissed. Once the last
/// other pane of a tab closes, its Tracks panes close too.
pub fn ensure() -> Result<()> {
    place(false)
}

/// Action: show the sidebar in the focused tab, even one without an agent.
pub fn show() -> Result<()> {
    place(true)
}

fn place(without_agent: bool) -> Result<()> {
    let mut guard = DockGuard::acquire()?;
    let closed = closed_pane();
    let panes = herdr::pane_list()?;
    // Forget sidebars of tabs that have since been closed.
    let live_tabs: HashSet<&str> = panes.iter().map(|p| p.tab_id.as_str()).collect();
    let known = guard.state.tabs.len();
    guard.state.tabs.retain(|tab, _| live_tabs.contains(tab.as_str()));
    let mut changed = guard.state.tabs.len() != known;
    // Left on their own, Tracks panes would keep the tab, and its workspace, open under the
    // plugin's name: once anything closes, close them too. Their tabs are forgotten first, so
    // that closing a sidebar does not dock it again.
    let orphans = if closed.is_some() { orphans(&panes) } else { Vec::new() };
    for pane in &orphans {
        changed |= guard.state.tabs.remove(&pane.tab_id).is_some();
    }
    if changed {
        guard.save()?;
    }
    for pane in orphans {
        herdr::pane_close(&pane.pane_id)?;
    }
    let tab = match closed {
        // Only a closed sidebar matters; other panes close all the time.
        Some(closed) => match guard.state.tabs.iter().find(|(_, pane)| **pane == closed) {
            Some((tab, _)) => tab.clone(),
            None => return Ok(()),
        },
        None => context_tab()?,
    };
    let mut tab_panes: Vec<&PaneInfo> = panes.iter().filter(|p| p.tab_id == tab).collect();
    if let Some(id) = guard.state.tabs.get(&tab).filter(|id| tab_panes.iter().any(|p| &p.pane_id == *id)).cloned() {
        if runs(&id, Mode::Sidebar)? {
            return Ok(());
        }
        // herdr restores plugin panes as plain shells after a restart: replace this one.
        guard.state.tabs.remove(&tab);
        guard.save()?;
        herdr::pane_close(&id)?;
        tab_panes.retain(|p| p.pane_id != id);
    }
    if tab_panes.is_empty() || !(without_agent || tab_panes.iter().any(|p| p.agent.is_some())) {
        return Ok(());
    }
    let pane = match find_sidebar(&tab_panes)? {
        Some(id) => id,
        None => match dock(&tab_panes)? {
            Some(id) => id,
            None if without_agent => bail!("tab {tab} cannot fit a sidebar (zoomed or too narrow)"),
            None => return Ok(()),
        },
    };
    guard.state.tabs.insert(tab, pane);
    guard.save()
}

/// The pane a `pane.closed` hook is about. The event names only the pane: herdr's context then
/// describes whichever tab is focused, so the tab comes from the dock state instead.
fn closed_pane() -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_EVENT_JSON").ok()?).ok()?;
    let data = &event["data"];
    (data["type"] == "pane_closed").then(|| data["pane_id"].as_str().map(str::to_string)).flatten()
}

/// The Tracks panes of tabs that hold nothing else: the agent and every other pane closed.
fn orphans(panes: &[PaneInfo]) -> Vec<&PaneInfo> {
    let mut tabs: BTreeMap<&str, Vec<&PaneInfo>> = BTreeMap::new();
    for pane in panes {
        tabs.entry(pane.tab_id.as_str()).or_default().push(pane);
    }
    tabs.into_values().filter(|tab| tab.iter().all(|p| is_tracks(p))).flatten().collect()
}

fn is_tracks(pane: &PaneInfo) -> bool {
    matches!(pane.label.as_deref(), Some(SIDEBAR_TITLE | DIFF_TITLE))
}

/// Startup hook. herdr restores panes after a restart, but plugin panes come back as plain
/// shells: close every Tracks pane that no longer runs Tracks and dock fresh sidebars in the
/// tabs that host an agent. Also deletes plugin files nothing uses any more.
pub fn startup() -> Result<()> {
    let mut guard = DockGuard::acquire()?;
    let panes = herdr::pane_list()?;
    let mut closed = HashSet::new();
    for pane in &panes {
        let mode = match pane.label.as_deref() {
            Some(SIDEBAR_TITLE) => Mode::Sidebar,
            Some(DIFF_TITLE) => Mode::Diff,
            _ => continue,
        };
        if !runs(&pane.pane_id, mode)? {
            herdr::pane_close(&pane.pane_id)?;
            closed.insert(pane.pane_id.as_str());
        }
    }
    let live: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).filter(|id| !closed.contains(id)).collect();
    guard.state.tabs.retain(|_, pane| live.contains(pane.as_str()));
    let panes: Vec<&PaneInfo> = panes.iter().filter(|p| live.contains(p.pane_id.as_str())).collect();
    let agent_tabs: BTreeSet<&str> = panes.iter().filter(|p| p.agent.is_some()).map(|p| p.tab_id.as_str()).collect();
    let mut docked = 0;
    for tab in agent_tabs {
        if guard.state.tabs.contains_key(tab) {
            continue;
        }
        let tab_panes: Vec<&PaneInfo> = panes.iter().copied().filter(|p| p.tab_id == tab).collect();
        if let Some(pane) = dock(&tab_panes)? {
            guard.state.tabs.insert(tab.to_string(), pane);
            docked += 1;
        }
    }
    guard.save()?;
    let pane_ids: Vec<&str> = live.into_iter().collect();
    let sessions: Vec<&str> = panes.iter().filter_map(|p| p.agent_session.as_ref()).map(|s| s.value.as_str()).collect();
    let removed = crate::state::prune(&pane_ids, &sessions)?;
    println!("startup: replaced {} restored pane(s), docked {docked} sidebar(s), removed {removed} stale file(s)", closed.len());
    Ok(())
}

/// Manifest titles of the plugin's panes; herdr shows them as pane labels.
const SIDEBAR_TITLE: &str = "tracks";
const DIFF_TITLE: &str = "tracks diff";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Sidebar,
    Diff,
}

/// Whether the pane's foreground process is this plugin running in `mode`.
fn runs(pane_id: &str, mode: Mode) -> Result<bool> {
    let arg = match mode {
        Mode::Sidebar => "pane",
        Mode::Diff => "diff",
    };
    let info = herdr::pane_process_info(pane_id)?;
    Ok(info.foreground_processes.iter().any(|p| {
        let argv = p.argv.as_deref().unwrap_or_default();
        let argv0 = p.argv0.as_deref().or(argv.first().map(String::as_str)).unwrap_or("");
        Path::new(argv0).file_name().is_some_and(|f| f == crate::BIN_NAME) && argv.get(1).map(String::as_str) == Some(arg)
    }))
}

/// A sidebar process already running in one of `panes` (state lost or never recorded).
fn find_sidebar(panes: &[&PaneInfo]) -> Result<Option<String>> {
    for pane in panes {
        if runs(&pane.pane_id, Mode::Sidebar)? {
            return Ok(Some(pane.pane_id.clone()));
        }
    }
    Ok(None)
}

/// Opens the sidebar right of the tab's rightmost, tallest pane. `None` when the tab is
/// zoomed or too narrow.
fn dock(tab_panes: &[&PaneInfo]) -> Result<Option<String>> {
    let any = tab_panes.first().ok_or_else(|| anyhow!("tab has no panes"))?;
    let layout = herdr::pane_layout(&any.pane_id)?;
    let width = sidebar_width();
    if layout.zoomed || layout.area.width < width + MIN_MAIN_WIDTH {
        return Ok(None);
    }
    let target = dock_target(&layout)?;
    let pane = herdr::open_plugin_pane(&plugin_id(), crate::SIDEBAR_ENTRYPOINT, &target, false, &[])?;
    if let Err(e) = snap_width(&pane, width) {
        eprintln!("sizing {pane}: {e:#}");
    }
    Ok(Some(pane))
}

fn dock_target(layout: &Layout) -> Result<String> {
    let right = layout.area.right();
    layout
        .panes
        .iter()
        .filter(|p| p.rect.right() == right)
        .max_by_key(|p| (p.rect.height, p.focused))
        .or_else(|| layout.panes.iter().find(|p| p.pane_id == layout.focused_pane_id))
        .map(|p| p.pane_id.clone())
        .ok_or_else(|| anyhow!("layout has no panes"))
}

/// (columns of the right split containing `pane`, columns of `pane`, split ratio).
fn split_widths(pane: &str) -> Result<(u32, u32, f64)> {
    let layout = herdr::pane_layout(pane)?;
    let me = layout
        .panes
        .iter()
        .find(|p| p.pane_id == pane)
        .ok_or_else(|| anyhow!("{pane} missing from its layout"))?;
    let split = layout
        .splits
        .iter()
        .filter(|s| s.direction == "right" && s.rect.right() == me.rect.right() && s.rect.x < me.rect.x)
        .filter(|s| s.rect.y <= me.rect.y && me.rect.bottom() <= s.rect.bottom())
        .min_by_key(|s| s.rect.width * s.rect.height)
        .ok_or_else(|| anyhow!("{pane} is not the right side of a split"))?;
    Ok((split.rect.width, me.rect.width, split.ratio))
}

/// Re-snaps a docked sidebar to the configured width, e.g. after a neighbouring pane closed
/// and herdr handed it that pane's columns.
pub fn hold_width(pane: &str) -> Result<()> {
    snap_width(pane, sidebar_width())
}

/// Resizes the right-hand `pane` to `width` columns. herdr gives the left side
/// round(total * ratio) columns and clamps ratios to 0.1..=0.9, so on wide splits the pane
/// cannot get narrower than ~10% of the split; resizing the right pane "right" grows the ratio.
fn snap_width(pane: &str, width: u32) -> Result<()> {
    let clamp = |total: u32| {
        let narrowest = total - (f64::from(total) * MAX_RATIO).round() as u32;
        width.max(narrowest).min(total.saturating_sub(MIN_MAIN_WIDTH / 2)).max(1)
    };
    for _ in 0..3 {
        let (total, current, ratio) = split_widths(pane)?;
        let want = clamp(total);
        if current == want {
            return Ok(());
        }
        let target = f64::from(total - want) / f64::from(total);
        let mut delta = target - ratio;
        if delta.abs() < 0.0005 {
            delta = if current > want { 1.0 } else { -1.0 } / f64::from(total);
        }
        herdr::pane_resize(pane, if delta > 0.0 { "right" } else { "left" }, delta.abs())?;
    }
    let (total, current, _) = split_widths(pane)?;
    if current != clamp(total) {
        bail!("{pane} is {current} columns wide, wanted {}", clamp(total));
    }
    Ok(())
}

/// herdr's left sidebar width: the live value from `session.json` (a dragged width), else
/// `[ui] sidebar_width`, clamped to `[ui] sidebar_min_width..=sidebar_max_width` as herdr does.
pub fn sidebar_width() -> u32 {
    let live = std::fs::read(herdr_runtime_dir().join("session.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["sidebar_width"].as_u64())
        .and_then(|w| u32::try_from(w).ok());
    let ui = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|cfg| cfg.get("ui")?.as_table().cloned());
    resolve_width(live, ui.as_ref())
}

fn resolve_width(live: Option<u32>, ui: Option<&toml::Table>) -> u32 {
    let setting = |key: &str, default: u32| {
        ui.and_then(|t| t.get(key)?.as_integer())
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0)
            .unwrap_or(default)
    };
    let min = setting("sidebar_min_width", DEFAULT_MIN_WIDTH);
    let max = setting("sidebar_max_width", DEFAULT_MAX_WIDTH).max(min);
    live.filter(|w| *w > 0).unwrap_or_else(|| setting("sidebar_width", DEFAULT_WIDTH)).clamp(min, max)
}

/// Directory holding the session's socket and `session.json` (named sessions have their own).
fn herdr_runtime_dir() -> PathBuf {
    std::env::var_os("HERDR_SOCKET_PATH")
        .and_then(|s| PathBuf::from(s).parent().map(Path::to_path_buf))
        .unwrap_or_else(config_dir)
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("herdr")
}

fn config_path() -> PathBuf {
    std::env::var_os("HERDR_CONFIG_PATH").map(PathBuf::from).unwrap_or_else(|| config_dir().join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_follows_herdr_clamping() {
        let ui = |text: &str| text.parse::<toml::Table>().unwrap();
        assert_eq!(resolve_width(None, None), DEFAULT_WIDTH);
        let forced = ui("sidebar_width = 50\nsidebar_min_width = 50\nsidebar_max_width = 100");
        assert_eq!(resolve_width(None, Some(&forced)), 50);
        assert_eq!(resolve_width(Some(80), Some(&forced)), 80, "a dragged width wins within bounds");
        assert_eq!(resolve_width(None, Some(&ui("sidebar_min_width = 50"))), 50, "minimum lifts the default width");
        assert_eq!(resolve_width(None, Some(&ui("sidebar_width = 60"))), DEFAULT_MAX_WIDTH, "default maximum caps it");
        assert_eq!(resolve_width(Some(10), None), DEFAULT_MIN_WIDTH);
    }

    #[test]
    fn tabs_left_with_only_tracks_panes_are_orphaned() {
        let pane = |id: &str, tab: &str, label: Option<&str>, agent: Option<&str>| PaneInfo {
            pane_id: id.into(),
            tab_id: tab.into(),
            terminal_id: String::new(),
            agent: agent.map(str::to_string),
            agent_session: None,
            cwd: None,
            foreground_cwd: None,
            terminal_title_stripped: None,
            label: label.map(str::to_string),
        };
        let panes = [
            // The agent closed: sidebar and diff are all that is left.
            pane("a1", "a", Some(SIDEBAR_TITLE), None),
            pane("a2", "a", Some(DIFF_TITLE), None),
            // The agent runs.
            pane("b1", "b", None, Some("omp")),
            pane("b2", "b", Some(SIDEBAR_TITLE), None),
            // The agent exited, its shell stays.
            pane("c1", "c", None, None),
            pane("c2", "c", Some(SIDEBAR_TITLE), None),
        ];
        let ids: Vec<&str> = orphans(&panes).iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(ids, ["a1", "a2"]);
    }
}
