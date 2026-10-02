//! Minimal newline-delimited JSON client for the herdr socket API (Linux: Unix socket).

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

fn socket_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("HERDR_SOCKET_PATH") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".config/herdr/herdr.sock"))
}

/// Sends one request and returns its `result` object.
pub fn call(method: &str, params: Value) -> Result<Value> {
    let path = socket_path()?;
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("connecting {}", path.display()))?;
    let mut line = serde_json::to_vec(&json!({"id": "git-sidebar", "method": method, "params": params}))?;
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
