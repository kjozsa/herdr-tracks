//! What an agent chat leaves behind, independent of how it is shown: the [`Chat`] a frontend
//! follows goes in, a [`Tracker`] keeps the repos, pull requests, commits and links it found,
//! and [`place`] says where each one is listed.

mod collect;
pub mod git;
pub mod github;
pub mod patch;
pub mod place;
pub mod repos;
pub mod state;
mod tracker;
pub mod transcript;

#[doc(hidden)]
pub use tracker::fixture;
pub use tracker::{Chat, Session, Statuses, Target, Tracker};
