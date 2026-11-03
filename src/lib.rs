//! Tracks' engine, shared by its frontends: the classic herdr plugin (this crate's binary) and
//! the Herdr app.

pub mod engine;

/// The herdr plugin id; also names the state directory every frontend shares.
pub const PLUGIN_ID: &str = "kjozsa.tracks";
