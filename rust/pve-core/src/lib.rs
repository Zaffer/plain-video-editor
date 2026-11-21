//! Plain video editor core.
//!
//! Pure logic: the project model, the export planner and the editing session.
//! No filesystem, processes, threads, ffmpeg or mpv types, so it builds for
//! wasm32 as well as natively. Platform backends (probing, export, preview)
//! live in other crates and talk to this one through `MediaInfo` and `ExportPlan`.

pub mod media;
pub mod model;
pub mod planner;
pub mod session;
pub mod time;
