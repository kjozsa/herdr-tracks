mod diff;
mod dock;
mod git;
mod github;
mod herdr;
mod repos;
mod sidebar;
mod state;
mod transcript;
mod term;

pub const PLUGIN_ID: &str = "kjozsa.git-sidebar";
/// Manifest `[[panes]]` ids.
pub const SIDEBAR_ENTRYPOINT: &str = "sidebar";
pub const DIFF_ENTRYPOINT: &str = "diff";
pub const BIN_NAME: &str = "herdr-git-sidebar";

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let result = match mode.as_str() {
        "pane" => sidebar::run(),
        "diff" => diff::run(),
        "ensure" => dock::ensure(),
        "show" => dock::show(),
        "startup" => dock::startup(),
        _ => {
            eprintln!("usage: {BIN_NAME} pane|diff|ensure|show|startup");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("{BIN_NAME} {mode}: {e:#}");
        std::process::exit(1);
    }
}
