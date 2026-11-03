//! The Herdr app frontend. herdr-app mods are JavaScript and cannot tail transcripts, so a
//! background `serve` tracks every agent terminal of the local herdr-core and writes one model
//! file per terminal; the `tracks` mod draws it and talks back through `send` and `patch`.

mod core;
mod model;
mod serve;

use anyhow::{bail, Context, Result};
use herdr_tracks::engine::patch::{self, DiffRequest};
use herdr_tracks::engine::state::{state_dir, write_atomic};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where the app frontend keeps its files, beside the engine's session files.
fn dir() -> PathBuf {
    state_dir().join("app")
}

fn models_dir() -> PathBuf {
    dir().join("models")
}

fn inbox_dir() -> PathBuf {
    dir().join("inbox")
}

fn heartbeat_file() -> PathBuf {
    dir().join("heartbeat")
}

/// `herdr-tracks app <command>`.
pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("serve") => serve::serve(),
        Some("ensure") => ensure(),
        Some("send") => match args {
            [_, terminal, command] => send(terminal, command),
            _ => bail!("usage: app send <terminal> <command json>"),
        },
        Some("patch") => match args {
            [_, request] => print_patch(request),
            _ => bail!("usage: app patch <diff request json>"),
        },
        _ => bail!("usage: app serve|ensure|send|patch"),
    }
}

/// Keeps `serve` running: marks the app as in use, starts it as a transient user unit unless it
/// runs, and prints the folder of the model files.
fn ensure() -> Result<()> {
    write_atomic(&heartbeat_file(), b"")?;
    if !serve::running()? {
        let exe = std::env::current_exe()?;
        // A concurrent `ensure` may have started it meanwhile: the unit name exists then.
        let _ = std::process::Command::new("systemd-run")
            .args(["--user", "--unit=herdr-tracks-app", "--collect", "--quiet", "--"])
            .arg(exe)
            .args(["app", "serve"])
            .status()
            .context("systemd-run")?;
    }
    println!("{}", models_dir().display());
    Ok(())
}

/// Queues a command for `serve` about one terminal's chat (see [`serve::Command`]).
fn send(terminal: &str, command: &str) -> Result<()> {
    let parsed: serde_json::Value = serde_json::from_str(command).context("command json")?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let file = inbox_dir().join(format!("{nanos}-{}.json", std::process::id()));
    write_atomic(&file, &serde_json::to_vec(&serde_json::json!({ "terminal": terminal, "command": parsed }))?)
}

/// Prints a diff's patch, for the diff mod.
fn print_patch(request: &str) -> Result<()> {
    let request: DiffRequest = serde_json::from_str(request).context("diff request json")?;
    match patch::read_patch(&request, &mut patch::Patches::new()).0 {
        Ok(text) => {
            print!("{text}");
            Ok(())
        }
        Err(e) => bail!(e),
    }
}
