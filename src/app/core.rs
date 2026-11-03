//! The local herdr-core: its terminals, through its JSON-RPC socket.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// A live terminal whose program reports an agent status.
pub struct Agent {
    pub address: String,
    /// The terminal's shell.
    pub pid: Option<u32>,
    /// The agent as it reports itself: `claude-code`, `pi`, `codex`, ...
    pub app: String,
    pub title: String,
    pub cwd: Option<String>,
    pub foreground_pid: Option<u32>,
    pub foreground_cwd: Option<String>,
}

fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime.join("herdr-core/default.sock")
}

fn call(method: &str, params: Value) -> Result<Value> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).with_context(|| format!("connecting {}", path.display()))?;
    let mut line = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let mut reply: Value = serde_json::from_str(&reply).with_context(|| format!("decoding {method} reply"))?;
    if let Some(err) = reply.get("error") {
        bail!("{method}: {err}");
    }
    reply.get_mut("result").map(Value::take).ok_or_else(|| anyhow!("{method}: reply has no result"))
}

/// The live terminals running an agent.
pub fn agents() -> Result<Vec<Agent>> {
    let mut list = call("terminal.list", json!({}))?;
    let terminals = list["terminals"].as_array_mut().map(std::mem::take).unwrap_or_default();
    Ok(terminals.iter().filter_map(agent).collect())
}

fn agent(t: &Value) -> Option<Agent> {
    if t["state"] != "live" {
        return None;
    }
    let app = t["status"]["app"].as_str()?.to_string();
    let text = |v: &Value| v.as_str().map(str::to_string);
    let pid = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok());
    Some(Agent {
        address: text(&t["address"])?,
        pid: pid(&t["pid"]),
        title: text(&t["title"]).unwrap_or_else(|| app.clone()),
        app,
        cwd: text(&t["cwd"]),
        foreground_pid: pid(&t["foreground"]["pid"]),
        foreground_cwd: text(&t["foreground"]["cwd"]),
    })
}
