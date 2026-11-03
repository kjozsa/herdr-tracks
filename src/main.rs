mod app;
mod classic;

use classic::{diff, dock, sidebar};

/// Manifest `[[panes]]` ids.
pub const SIDEBAR_ENTRYPOINT: &str = "sidebar";
pub const DIFF_ENTRYPOINT: &str = "diff";
pub const BIN_NAME: &str = "herdr-tracks";

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let result = match mode.as_str() {
        "pane" => sidebar::run(),
        "diff" => diff::run(),
        "ensure" => dock::ensure(),
        "show" => dock::show(),
        "startup" => dock::startup(),
        "app" => app::run(&std::env::args().skip(2).collect::<Vec<_>>()),
        _ => {
            eprintln!("usage: {BIN_NAME} pane|diff|ensure|show|startup|app");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("{BIN_NAME} {mode}: {e:#}");
        std::process::exit(1);
    }
}
