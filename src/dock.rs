//! Docking the sidebar as a right-edge split of a tab, sized to herdr's left sidebar.

use crate::herdr::{self, Layout, PaneInfo};
use crate::state::{DockGuard, TabEntry};
use anyhow::{anyhow, bail, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// herdr's defaults for `[ui] sidebar_width`, `sidebar_min_width`, `sidebar_max_width`.
const DEFAULT_WIDTH: u32 = 26;
const DEFAULT_MIN_WIDTH: u32 = 18;
const DEFAULT_MAX_WIDTH: u32 = 36;
/// Columns left for the rest of the tab; narrower tabs are not auto-docked.
const MIN_MAIN_WIDTH: u32 = 40;
/// herdr clamps split ratios to 0.1..=0.9.
const MAX_RATIO: f64 = 0.9;

fn plugin_id() -> String {
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

/// Auto-dock (event hooks): add a sidebar to the context tab if it hosts an agent, has none,
/// and the user has not closed it there.
pub fn ensure() -> Result<()> {
    let tab = context_tab()?;
    let mut guard = DockGuard::acquire()?;
    let panes = herdr::pane_list()?;
    let tab_panes: Vec<&PaneInfo> = panes.iter().filter(|p| p.tab_id == tab).collect();
    if let Some(entry) = guard.state.tabs.get_mut(&tab) {
        if entry.closed {
            return Ok(());
        }
        if let Some(id) = &entry.pane_id {
            if !tab_panes.iter().any(|p| &p.pane_id == id) {
                // Closed through herdr itself: treat like an explicit close.
                entry.closed = true;
                guard.save()?;
            }
            return Ok(());
        }
    }
    if !tab_panes.iter().any(|p| p.agent.is_some()) {
        return Ok(());
    }
    let pane_id = match find_sidebar(&tab_panes)? {
        Some(id) => id,
        None => match dock(&tab_panes)? {
            Some(id) => id,
            None => return Ok(()),
        },
    };
    guard.state.tabs.insert(tab, TabEntry { pane_id: Some(pane_id), closed: false });
    guard.save()
}

/// Action: close the context tab's sidebar, or dock one.
pub fn toggle() -> Result<()> {
    let tab = context_tab()?;
    let mut guard = DockGuard::acquire()?;
    let panes = herdr::pane_list()?;
    let tab_panes: Vec<&PaneInfo> = panes.iter().filter(|p| p.tab_id == tab).collect();
    let recorded = guard
        .state
        .tabs
        .get(&tab)
        .and_then(|e| e.pane_id.clone())
        .filter(|id| tab_panes.iter().any(|p| &p.pane_id == id));
    let entry = match recorded.map_or_else(|| find_sidebar(&tab_panes), |id| Ok(Some(id)))? {
        Some(open) => {
            herdr::pane_close(&open)?;
            TabEntry { pane_id: None, closed: true }
        }
        None => {
            let id = dock(&tab_panes)?.ok_or_else(|| anyhow!("tab {tab} cannot fit a sidebar"))?;
            TabEntry { pane_id: Some(id), closed: false }
        }
    };
    guard.state.tabs.insert(tab, entry);
    guard.save()
}

/// Startup hook: drop state for tabs that no longer exist, and forget sidebars that did not
/// survive the restart so their tabs dock again. Explicit closes are kept.
pub fn startup() -> Result<()> {
    let mut guard = DockGuard::acquire()?;
    let panes = herdr::pane_list()?;
    let live_panes: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
    let live_tabs: HashSet<&str> = panes.iter().map(|p| p.tab_id.as_str()).collect();
    guard.state.tabs.retain(|tab, e| {
        live_tabs.contains(tab.as_str())
            && (e.closed || e.pane_id.as_deref().is_some_and(|id| live_panes.contains(id)))
    });
    guard.save()
}

/// Called by the sidebar itself when the user quits it: remember the close, then close the pane.
pub fn close_self(pane_id: &str) -> Result<()> {
    {
        let mut guard = DockGuard::acquire()?;
        let tab = herdr::pane_layout(pane_id)?.tab_id;
        guard.state.tabs.insert(tab, TabEntry { pane_id: None, closed: true });
        guard.save()?;
    }
    herdr::pane_close(pane_id)
}

/// A sidebar process already running in one of `panes` (state lost or never recorded).
fn find_sidebar(panes: &[&PaneInfo]) -> Result<Option<String>> {
    for pane in panes {
        let info = herdr::pane_process_info(&pane.pane_id)?;
        let is_sidebar = info.foreground_processes.iter().any(|p| {
            let argv = p.argv.as_deref().unwrap_or_default();
            let argv0 = p.argv0.as_deref().or(argv.first().map(String::as_str)).unwrap_or("");
            Path::new(argv0).file_name().is_some_and(|f| f == crate::BIN_NAME)
                && argv.get(1).map(String::as_str) == Some("pane")
        });
        if is_sidebar {
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
    let pane = herdr::open_sidebar_pane(&plugin_id(), &target)?;
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
        .unwrap_or_else(|| config_dir())
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
}
