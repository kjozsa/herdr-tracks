//! Minimal newline-delimited JSON client for the herdr socket API (Linux: Unix socket), and
//! where herdr keeps its socket, session and config.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// herdr's defaults for `[ui] sidebar_width`, `sidebar_min_width`, `sidebar_max_width`.
const DEFAULT_WIDTH: u32 = 26;
const DEFAULT_MIN_WIDTH: u32 = 18;
const DEFAULT_MAX_WIDTH: u32 = 36;

/// `$XDG_CONFIG_HOME/herdr`, else `~/.config/herdr`.
fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("herdr")
}

fn config_path() -> PathBuf {
    std::env::var_os("HERDR_CONFIG_PATH").map(PathBuf::from).unwrap_or_else(|| config_dir().join("config.toml"))
}

/// The session's socket; the default session's lives in the config dir.
fn socket_path() -> PathBuf {
    std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from).unwrap_or_else(|| config_dir().join("herdr.sock"))
}

/// Directory holding the session's socket and `session.json` (named sessions have their own).
fn runtime_dir() -> PathBuf {
    socket_path().parent().map_or_else(config_dir, Path::to_path_buf)
}

/// herdr's left sidebar width: the live value from `session.json` (a dragged width), else
/// `[ui] sidebar_width`, clamped to `[ui] sidebar_min_width..=sidebar_max_width` as herdr does.
pub fn sidebar_width() -> u32 {
    let live = std::fs::read(runtime_dir().join("session.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
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

/// Sends one request and returns its `result` object.
pub fn call(method: &str, params: Value) -> Result<Value> {
    let path = socket_path();
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("connecting {}", path.display()))?;
    let mut line = serde_json::to_vec(&json!({"id": "tracks", "method": method, "params": params}))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let mut reply: Value =
        serde_json::from_str(&reply).with_context(|| format!("decoding {method} reply"))?;
    if let Some(err) = reply.get("error") {
        bail!("{method}: {err}");
    }
    reply
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| anyhow!("{method}: reply has no result"))
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn right(&self) -> u32 {
        self.x + self.width
    }
    pub fn bottom(&self) -> u32 {
        self.y + self.height
    }
}

#[derive(Debug, Deserialize)]
pub struct LayoutPane {
    pub pane_id: String,
    pub focused: bool,
    pub rect: Rect,
}

#[derive(Debug, Deserialize)]
pub struct LayoutSplit {
    pub direction: String,
    pub ratio: f64,
    pub rect: Rect,
}

#[derive(Debug, Deserialize)]
pub struct Layout {
    pub tab_id: String,
    pub zoomed: bool,
    pub area: Rect,
    pub focused_pane_id: String,
    pub panes: Vec<LayoutPane>,
    pub splits: Vec<LayoutSplit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentSession {
    /// Native session reference: a transcript path or a session id, per agent.
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub tab_id: String,
    pub terminal_id: String,
    pub agent: Option<String>,
    pub agent_session: Option<AgentSession>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub terminal_title_stripped: Option<String>,
    /// The pane's label; plugin panes carry their manifest title.
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Process {
    pub argv0: Option<String>,
    pub argv: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct ProcessInfo {
    pub shell_pid: Option<u32>,
    #[serde(default)]
    pub foreground_processes: Vec<Process>,
}

pub fn pane_layout(pane_id: &str) -> Result<Layout> {
    let mut r = call("pane.layout", json!({"pane_id": pane_id}))?;
    Ok(serde_json::from_value(r["layout"].take())?)
}

pub fn pane_list() -> Result<Vec<PaneInfo>> {
    let mut r = call("pane.list", json!({}))?;
    Ok(serde_json::from_value(r["panes"].take())?)
}

pub fn pane_process_info(pane_id: &str) -> Result<ProcessInfo> {
    let mut r = call("pane.process_info", json!({"pane_id": pane_id}))?;
    Ok(serde_json::from_value(r["process_info"].take())?)
}

/// Moves the split edge of `pane_id` in `direction` by `amount` (a ratio delta).
pub fn pane_resize(pane_id: &str, direction: &str, amount: f64) -> Result<()> {
    call("pane.resize", json!({"pane_id": pane_id, "direction": direction, "amount": amount}))?;
    Ok(())
}

pub fn pane_close(pane_id: &str) -> Result<()> {
    call("pane.close", json!({"pane_id": pane_id}))?;
    Ok(())
}

pub fn pane_focus(pane_id: &str) -> Result<()> {
    call("pane.focus", json!({"pane_id": pane_id}))?;
    Ok(())
}

/// Lets the pane's application receive right-clicks instead of herdr's pane menu.
pub fn forward_right_click(pane_id: &str) -> Result<()> {
    call("pane.input.set", json!({"pane_id": pane_id, "right_click": "pane"}))?;
    Ok(())
}

/// Whether the pane is still open.
pub fn pane_exists(pane_id: &str) -> bool {
    call("pane.get", json!({"pane_id": pane_id})).is_ok()
}

/// Opens a plugin pane entrypoint as a right split of `target_pane_id` and returns its id.
/// `env` is added to the pane process's environment.
pub fn open_plugin_pane(
    plugin_id: &str,
    entrypoint: &str,
    target_pane_id: &str,
    focus: bool,
    env: &[(&str, String)],
) -> Result<String> {
    let env: serde_json::Map<String, Value> = env.iter().map(|(k, v)| (k.to_string(), Value::from(v.as_str()))).collect();
    let r = call(
        "plugin.pane.open",
        json!({
            "plugin_id": plugin_id,
            "entrypoint": entrypoint,
            "placement": "split",
            "direction": "right",
            "target_pane_id": target_pane_id,
            "focus": focus,
            "env": env,
        }),
    )?;
    r["plugin_pane"]["pane"]["pane_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("plugin.pane.open reply has no pane id: {r}"))
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
