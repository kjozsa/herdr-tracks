//! Docking the sidebar as a right-edge split of a tab, sized to herdr's left sidebar.

use crate::herdr::{self, Layout, PaneInfo};
use crate::state::DockGuard;
use anyhow::{anyhow, bail, Result};
use std::collections::{BTreeMap, HashSet};
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
/// docks again straight away (`pane.closed`): it is not meant to be dismissed.
pub fn ensure() -> Result<()> {
    place(false)
}

/// Action: show the sidebar in the focused tab, even one without an agent.
pub fn show() -> Result<()> {
    place(true)
}

fn place(without_agent: bool) -> Result<()> {
    let mut guard = DockGuard::acquire()?;
    let tab = match closed_sidebar_tab(&guard.state.tabs) {
        Some(tab) => tab,
        None => context_tab()?,
    };
    let panes = herdr::pane_list()?;
    let tab_panes: Vec<&PaneInfo> = panes.iter().filter(|p| p.tab_id == tab).collect();
    if guard.state.tabs.get(&tab).is_some_and(|id| tab_panes.iter().any(|p| &p.pane_id == id)) {
        return Ok(());
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

/// For a `pane.closed` hook on a sidebar pane: the tab it was docked in. The event names only
/// the pane, and herdr's context then describes whichever tab is focused.
fn closed_sidebar_tab(tabs: &BTreeMap<String, String>) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_EVENT_JSON").ok()?).ok()?;
    let data = &event["data"];
    if data["type"] != "pane_closed" {
        return None;
    }
    let closed = data["pane_id"].as_str()?;
    tabs.iter().find(|(_, pane)| pane.as_str() == closed).map(|(tab, _)| tab.clone())
}

/// Startup hook: forget sidebars that did not survive the restart, so their tabs dock again.
pub fn startup() -> Result<()> {
    let mut guard = DockGuard::acquire()?;
    let panes = herdr::pane_list()?;
    let live: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
    guard.state.tabs.retain(|_, pane| live.contains(pane.as_str()));
    guard.save()
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
