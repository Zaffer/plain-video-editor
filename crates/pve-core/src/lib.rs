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

/// What the tests in this crate share: the two described media files in
/// `testdata/media` and a short way to write a project.
#[cfg(test)]
pub(crate) mod testdata {
    use serde_json::{json, Value};

    use crate::media::MediaInfo;
    use crate::model::Project;
    use crate::planner::Infos;

    /// "clip1": 640x480 H.264 with reordered frames, no sound.
    /// "clip2": 1080x1080 H.264 with 9:16 pixels, HLG, with sound.
    pub fn info(name: &str) -> MediaInfo {
        let text = match name {
            "clip1" => include_str!("../../../testdata/media/clip1.json"),
            "clip2" => include_str!("../../../testdata/media/clip2.json"),
            _ => panic!("no described media called {name}"),
        };
        MediaInfo::from_json(&serde_json::from_str(text).unwrap()).unwrap()
    }

    pub fn infos() -> Infos {
        ["clip1", "clip2"].iter().map(|n| (n.to_string(), info(n))).collect()
    }

    pub fn project(clips: Value, lanes: Value) -> Result<Project, String> {
        Project::from_json(&json!({
            "version": 1,
            "media": {"clip1": {"path": "clip1.mp4"}, "clip2": {"path": "clip2.mp4"}},
            "clips": clips,
            "audio_lanes": lanes,
        }))
    }

    /// Ten seconds of each clip cropped to 4:3, with clip 2's sound under clip 1 as well.
    pub fn montage() -> Project {
        project(
            json!([
                {"media": "clip1", "in": 5, "out": 15, "crop": {"aspect": "4:3"}},
                {"media": "clip2", "in": 7, "out": 17, "crop": {"aspect": "4:3"}},
            ]),
            json!([{"items": [{"media": "clip2", "in": 7, "out": 17, "at": 0}]}]),
        )
        .unwrap()
    }
}
