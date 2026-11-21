//! Editing session: the project being edited, the selection, undo, and timeline maths.
//!
//! Pure logic with no GUI or player types. The GUI calls these operations and redraws.
//!
//! Either a video clip or a sound item is "the target": the thing the trim bar,
//! the timestamps and the volume act on.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use crate::media::MediaInfo;
use crate::model::{AudioItem, AudioLane, Clip, Crop, Project, Rect};
use crate::planner::{plan, ExportPlan, Infos, Kind, Mode};
use crate::time::{self, r, to_f64, R};

/// Smallest crop, in display pixels.
pub const MIN_CROP: i64 = 16;
pub const UNDO_LIMIT: usize = 200;
/// Sound lanes besides the clips' own sound.
pub const LANES: usize = 2;
pub const MIN_GAIN_DB: f64 = -60.0;
pub const MAX_GAIN_DB: f64 = 12.0;

/// Shortest sound item, seconds.
fn min_sound() -> R {
    r(1, 20)
}

/// Largest centred rectangle of `ratio` (w/h) inside a dw x dh frame, in whole display pixels.
pub fn centered_rect(dw: f64, dh: f64, ratio: &R) -> Rect {
    let ratio = to_f64(ratio);
    let (w, h) = if dw / dh > ratio { (dh * ratio, dh) } else { (dw, dw / ratio) };
    let (w, h) = (w as i64, h as i64);
    Rect { x: ((dw - w as f64) / 2.0) as i64, y: ((dh - h as f64) / 2.0) as i64, w, h }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Off,
    Free,
    /// Width / height.
    Ratio(R),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Clip(usize),
    /// (lane, item)
    Sound(usize, usize),
}

#[derive(Clone)]
struct Snapshot {
    clips: Vec<Clip>,
    lanes: Vec<AudioLane>,
    selected: Option<usize>,
    sel_audio: Option<(usize, usize)>,
}

pub struct Session {
    pub project: Project,
    pub infos: Infos,
    /// Selected clip.
    pub selected: Option<usize>,
    /// (lane, item) when a sound item is the target.
    pub sel_audio: Option<(usize, usize)>,
    /// Force exact cuts (always re-encode).
    pub exact: bool,
    /// A crop shape applies to every clip.
    pub lock_shape: bool,
    /// Project file, once saved or opened.
    pub file: Option<String>,
    saved: String,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

fn file_stem(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "clip".into())
}

/// `path` relative to the directory `base`, by comparing components (no filesystem access).
pub fn relative_path(path: &str, base: &str) -> String {
    let (p, b): (Vec<Component>, Vec<Component>) = (Path::new(path).components().collect(), Path::new(base).components().collect());
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &p[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        ".".into()
    } else {
        out.to_string_lossy().into_owned()
    }
}

/// Join `rel` onto the directory `base` and tidy "." and ".." away (no filesystem access).
pub fn absolute_path(rel: &str, base: &str) -> String {
    let mut out = PathBuf::new();
    let joined = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { Path::new(base).join(rel) };
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

impl Session {
    pub fn new() -> Session {
        let mut s = Session {
            project: Project { lanes: (0..LANES).map(|_| AudioLane::default()).collect(), ..Project::default() },
            infos: Infos::new(),
            selected: None,
            sel_audio: None,
            exact: false,
            lock_shape: true,
            file: None,
            saved: String::new(),
            undo: Vec::new(),
            redo: Vec::new(),
        };
        s.saved = s.state();
        s
    }

    // ------------------------------------------------------------ basics

    pub fn clips(&self) -> &[Clip] {
        &self.project.clips
    }

    pub fn lanes(&self) -> &[AudioLane] {
        &self.project.lanes
    }

    pub fn clip(&self) -> Option<&Clip> {
        self.selected.and_then(|i| self.project.clips.get(i))
    }

    fn clip_mut(&mut self) -> Option<&mut Clip> {
        self.selected.and_then(|i| self.project.clips.get_mut(i))
    }

    /// The selected sound item, if a sound item is the target.
    pub fn sound(&self) -> Option<&AudioItem> {
        self.sel_audio.and_then(|(l, i)| self.project.lanes.get(l)?.items.get(i))
    }

    pub fn target(&self) -> Option<Target> {
        match (self.sel_audio, self.selected) {
            (Some((l, i)), _) if self.sound().is_some() => Some(Target::Sound(l, i)),
            (_, Some(i)) if i < self.project.clips.len() => Some(Target::Clip(i)),
            _ => None,
        }
    }

    /// (media id, in, out, gain) of the target.
    pub fn target_span(&self) -> Option<(&str, R, R, f64)> {
        match self.target()? {
            Target::Clip(i) => {
                let c = &self.project.clips[i];
                Some((&c.media, c.start, c.end, c.gain_db))
            }
            Target::Sound(l, i) => {
                let s = &self.project.lanes[l].items[i];
                Some((&s.media, s.start, s.end, s.gain_db))
            }
        }
    }

    pub fn target_info(&self) -> Option<&MediaInfo> {
        self.infos.get(self.target_span()?.0)
    }

    pub fn target_path(&self) -> Option<&str> {
        self.project.media.get(self.target_span()?.0).map(String::as_str)
    }

    pub fn info(&self, i: usize) -> &MediaInfo {
        &self.infos[&self.project.clips[i].media]
    }

    pub fn path(&self, i: usize) -> &str {
        &self.project.media[&self.project.clips[i].media]
    }

    /// Smallest step for the target: one video frame, or a short moment for sound.
    pub fn frame(&self) -> R {
        match (self.target(), self.target_info().and_then(|i| i.video.as_ref())) {
            (Some(Target::Clip(_)), Some(v)) => v.fps.recip(),
            _ => min_sound(),
        }
    }

    pub fn display_size(&self, i: usize) -> (f64, f64) {
        let (w, h) = self.info(i).video.as_ref().expect("a clip has a picture").display_size();
        (to_f64(&w), to_f64(&h))
    }

    pub fn select(&mut self, i: usize) {
        self.selected = Some(i);
        self.sel_audio = None;
    }

    pub fn select_sound(&mut self, lane: usize, i: usize) {
        self.sel_audio = Some((lane, i));
    }

    // ------------------------------------------------------------ undo

    fn snapshot(&self) -> Snapshot {
        Snapshot { clips: self.project.clips.clone(), lanes: self.project.lanes.clone(), selected: self.selected, sel_audio: self.sel_audio }
    }

    /// Call before a change (once per gesture) so it can be undone.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > UNDO_LIMIT {
            let extra = self.undo.len() - UNDO_LIMIT;
            self.undo.drain(..extra);
        }
        self.redo.clear();
    }

    fn restore(&mut self, s: Snapshot) {
        self.project.clips = s.clips;
        self.project.lanes = s.lanes;
        self.selected = s.selected;
        self.sel_audio = s.sel_audio;
    }

    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else { return false };
        self.redo.push(self.snapshot());
        self.restore(s);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else { return false };
        self.undo.push(self.snapshot());
        self.restore(s);
        true
    }

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    // ------------------------------------------------------------ media and clips

    /// `path` must be absolute.
    fn media_id(&mut self, path: &str, info: MediaInfo) -> String {
        if let Some((k, _)) = self.project.media.iter().find(|(_, v)| v.as_str() == path) {
            return k.clone();
        }
        let stem = file_stem(path);
        let (mut media, mut n) = (stem.clone(), 1);
        while self.project.media.contains_key(&media) {
            n += 1;
            media = format!("{stem}-{n}");
        }
        self.project.media.insert(media.clone(), path.to_owned());
        self.infos.insert(media.clone(), info);
        media
    }

    /// Add a video file as a new clip covering all of it. Returns the clip's index.
    pub fn add_media(&mut self, path: &str, info: MediaInfo) -> Result<usize, String> {
        if info.video.is_none() {
            return Err("this file has no picture; add it as sound instead".into());
        }
        let media = self.media_id(path, info);
        self.checkpoint();
        let info = &self.infos[&media];
        let mut clip = Clip {
            media: media.clone(),
            start: time::zero(),
            end: info.duration,
            crop: None,
            crop_aspect: None,
            audio: info.audio.is_some(),
            gain_db: 0.0,
        };
        if self.lock_shape {
            if let Some(ratio) = self.project.clips.first().and_then(|c| c.crop_aspect) {
                // Joining a montage that has a shape: take the same shape.
                let (dw, dh) = info.video.as_ref().unwrap().display_size();
                clip.crop = Some(Crop::Rect(centered_rect(to_f64(&dw), to_f64(&dh), &ratio)));
                clip.crop_aspect = Some(ratio);
            }
        }
        self.project.clips.push(clip);
        self.select(self.project.clips.len() - 1);
        Ok(self.project.clips.len() - 1)
    }

    /// Replace a file's description (e.g. once its keyframes are indexed).
    pub fn set_info(&mut self, path: &str, info: MediaInfo) {
        let ids: Vec<String> = self.project.media.iter().filter(|(_, v)| v.as_str() == path).map(|(k, _)| k.clone()).collect();
        for id in ids {
            self.infos.insert(id, info.clone());
        }
    }

    pub fn remove(&mut self, i: usize) {
        if i >= self.project.clips.len() {
            return;
        }
        self.checkpoint();
        self.project.clips.remove(i);
        let n = self.project.clips.len();
        self.selected = match self.selected {
            _ if n == 0 => None,
            Some(s) if s < i => Some(s.min(n - 1)),
            Some(s) => Some(s.saturating_sub(1).min(n - 1)),
            None => None,
        };
        self.sel_audio = None;
    }

    /// Move clip i to position j. The caller checkpoints once per drag.
    pub fn move_clip(&mut self, i: usize, j: usize) {
        let n = self.project.clips.len();
        if n == 0 || i >= n {
            return;
        }
        let j = j.min(n - 1);
        if i != j {
            let c = self.project.clips.remove(i);
            self.project.clips.insert(j, c);
            self.selected = Some(j);
        }
    }

    // ------------------------------------------------------------ trimming (clip or sound item)

    fn span_mut(&mut self) -> Option<(&mut R, &mut R)> {
        match self.target()? {
            Target::Clip(i) => {
                let c = &mut self.project.clips[i];
                Some((&mut c.start, &mut c.end))
            }
            Target::Sound(l, i) => {
                let s = &mut self.project.lanes[l].items[i];
                Some((&mut s.start, &mut s.end))
            }
        }
    }

    pub fn set_in(&mut self, t: R) {
        let frame = self.frame();
        if let Some((start, end)) = self.span_mut() {
            *start = time::zero().max(t.min(*end - frame));
        }
    }

    pub fn set_out(&mut self, t: R) {
        let frame = self.frame();
        let Some(duration) = self.target_info().map(|i| i.duration) else { return };
        if let Some((start, end)) = self.span_mut() {
            *end = duration.min(t.max(*start + frame));
        }
    }

    /// Cut the selected clip in two at source time t. The second half becomes selected.
    pub fn split(&mut self, t: R) -> bool {
        let Some(Target::Clip(i)) = self.target() else { return false };
        let frame = self.frame();
        let c = &self.project.clips[i];
        if !(c.start + frame <= t && t <= c.end - frame) {
            return false;
        }
        self.checkpoint();
        let mut second = self.project.clips[i].clone();
        self.project.clips[i].end = t;
        second.start = t;
        self.project.clips.insert(i + 1, second);
        self.selected = Some(i + 1);
        true
    }

    pub fn set_gain(&mut self, db: f64) {
        let db = (db.clamp(MIN_GAIN_DB, MAX_GAIN_DB) * 100.0).round() / 100.0;
        match self.target() {
            Some(Target::Clip(i)) => self.project.clips[i].gain_db = db,
            Some(Target::Sound(l, i)) => self.project.lanes[l].items[i].gain_db = db,
            None => {}
        }
    }

    // ------------------------------------------------------------ sound lanes

    fn overlaps(&self, lane: usize, at: &R, duration: &R) -> bool {
        let (a0, a1) = (*at, *at + *duration);
        self.project.lanes[lane].items.iter().any(|o| o.at < a1 && a0 < o.at + o.duration())
    }

    /// Add a file's sound to a lane at montage time `at`, in the first lane where it does not overlap.
    pub fn add_sound(&mut self, path: &str, info: MediaInfo, at: R) -> Result<(usize, usize), String> {
        if info.audio.is_none() {
            return Err("this file has no sound".into());
        }
        let media = self.media_id(path, info);
        self.checkpoint();
        while self.project.lanes.len() < LANES {
            self.project.lanes.push(AudioLane::default());
        }
        let duration = self.infos[&media].duration;
        let item = AudioItem { media, start: time::zero(), end: duration, at: at.max(time::zero()), gain_db: 0.0 };
        let lane = (0..LANES).find(|&n| !self.overlaps(n, &item.at, &duration)).unwrap_or(0);
        self.project.lanes[lane].items.push(item);
        self.sel_audio = Some((lane, self.project.lanes[lane].items.len() - 1));
        Ok(self.sel_audio.unwrap())
    }

    /// Move the selected sound item along the montage, and optionally to another lane.
    pub fn move_sound(&mut self, at: R, lane: Option<usize>) {
        let Some((cur, i)) = self.sel_audio else { return };
        if self.sound().is_none() {
            return;
        }
        self.project.lanes[cur].items[i].at = at.max(time::zero());
        if let Some(to) = lane.filter(|&to| to != cur && to < self.project.lanes.len()) {
            let item = self.project.lanes[cur].items.remove(i);
            self.project.lanes[to].items.push(item);
            self.sel_audio = Some((to, self.project.lanes[to].items.len() - 1));
        }
    }

    pub fn remove_sound(&mut self) {
        let Some((lane, i)) = self.sel_audio else { return };
        if self.sound().is_none() {
            return;
        }
        self.checkpoint();
        self.project.lanes[lane].items.remove(i);
        self.sel_audio = None;
    }

    /// The item playing in `lane` at montage time t, and the time within its source.
    pub fn sound_at(&self, lane: usize, t: &R) -> Option<(&AudioItem, R)> {
        self.project.lanes.get(lane)?.items.iter().find(|it| it.at <= *t && *t < it.at + it.duration()).map(|it| (it, it.start + (*t - it.at)))
    }

    // ------------------------------------------------------------ crop

    /// With lock_shape, ratios and Off apply to every clip; Free is always the selected clip only.
    pub fn set_shape(&mut self, shape: &Shape) {
        let Some(selected) = self.selected.filter(|&i| i < self.project.clips.len()) else { return };
        self.checkpoint();
        let everyone = self.lock_shape && *shape != Shape::Free;
        let targets: Vec<usize> = if everyone { (0..self.project.clips.len()).collect() } else { vec![selected] };
        for i in targets {
            let (dw, dh) = self.display_size(i);
            let c = &mut self.project.clips[i];
            let has_rect = matches!(c.crop, Some(Crop::Rect(_)));
            match shape {
                Shape::Off => {
                    c.crop = None;
                    c.crop_aspect = None;
                }
                Shape::Free => {
                    if !has_rect {
                        c.crop = Some(Crop::Rect(Rect { x: (dw * 0.1) as i64, y: (dh * 0.1) as i64, w: (dw * 0.8) as i64, h: (dh * 0.8) as i64 }));
                    }
                    c.crop_aspect = None;
                }
                Shape::Ratio(ratio) => {
                    if c.crop_aspect.as_ref() != Some(ratio) || !has_rect {
                        c.crop = Some(Crop::Rect(centered_rect(dw, dh, ratio)));
                        c.crop_aspect = Some(*ratio);
                    }
                }
            }
        }
    }

    pub fn shape(&self) -> Shape {
        match self.clip() {
            Some(c) if c.crop.is_some() => c.crop_aspect.map_or(Shape::Free, Shape::Ratio),
            _ => Shape::Off,
        }
    }

    /// Set the selected clip's crop, kept inside the frame. The caller checkpoints once per drag.
    pub fn set_crop(&mut self, rect: Rect) {
        let Some(i) = self.selected.filter(|&i| i < self.project.clips.len()) else { return };
        let (dw, dh) = self.display_size(i);
        let (dw, dh) = (dw as i64, dh as i64);
        let w = rect.w.min(dw).max(MIN_CROP);
        let h = rect.h.min(dh).max(MIN_CROP);
        let x = rect.x.min(dw - w).max(0);
        let y = rect.y.min(dh - h).max(0);
        self.project.clips[i].crop = Some(Crop::Rect(Rect { x, y, w, h }));
    }

    pub fn set_sound_on(&mut self, on: bool) {
        if let Some(c) = self.clip_mut() {
            c.audio = on;
        }
    }

    // ------------------------------------------------------------ montage timeline

    pub fn offsets(&self) -> Vec<R> {
        let mut t = time::zero();
        self.project.clips.iter().map(|c| {
            let at = t;
            t += c.duration();
            at
        }).collect()
    }

    pub fn total(&self) -> R {
        self.project.clips.iter().fold(time::zero(), |a, c| a + c.duration())
    }

    /// Where the selected clip's source time falls on the montage (clamped into the clip).
    pub fn montage_time(&self, source_time: &R) -> R {
        let (Some(i), Some(c)) = (self.selected, self.clip()) else { return time::zero() };
        self.offsets()[i] + time::zero().max((*source_time).min(c.end) - c.start)
    }

    /// Montage time -> (clip index, source time). Past the end gives the last clip's out point.
    pub fn locate(&self, t: &R) -> Option<(usize, R)> {
        let last = self.project.clips.last()?;
        for (i, (off, c)) in self.offsets().iter().zip(&self.project.clips).enumerate() {
            if *t < *off + c.duration() {
                return Some((i, c.start + (*t - *off).max(time::zero())));
            }
        }
        Some((self.project.clips.len() - 1, last.end))
    }

    // ------------------------------------------------------------ export

    /// What an export would do now. None when there are no clips.
    pub fn plan(&self) -> Result<Option<ExportPlan>, String> {
        if self.project.clips.is_empty() {
            return Ok(None);
        }
        plan(&self.project, &self.infos, if self.exact { Mode::Encode } else { Mode::Auto }, false).map(Some)
    }

    pub fn default_output(&self, p: &ExportPlan) -> String {
        let first = Path::new(self.path(0));
        let stem = file_stem(self.path(0));
        let ext = match (p.kind, first.extension()) {
            (Kind::Copy, Some(e)) => format!(".{}", e.to_string_lossy()),
            _ => ".mp4".to_string(),
        };
        let name = if self.project.clips.len() == 1 { format!("{stem}-cut{ext}") } else { format!("{stem}-montage{ext}") };
        first.with_file_name(name).to_string_lossy().into_owned()
    }

    // ------------------------------------------------------------ project files

    fn state(&self) -> String {
        if self.project.clips.is_empty() {
            String::new()
        } else {
            self.project.to_json().to_string()
        }
    }

    /// There are edits that have not been saved to a project file.
    pub fn dirty(&self) -> bool {
        self.state() != self.saved
    }

    /// The project as it is written to `file`: media paths relative to the file's directory.
    pub fn file_json(&self, file: &str) -> Value {
        let base = Path::new(file).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let mut d = self.project.to_json();
        d["media"] = Value::Object(
            self.project.media.iter().map(|(k, v)| (k.clone(), json!({ "path": relative_path(v, &base) }))).collect(),
        );
        d
    }

    /// Call after the project has been written to `file` (an absolute path).
    pub fn mark_saved(&mut self, file: &str) {
        self.file = Some(file.to_owned());
        self.saved = self.state();
    }

    /// Parse a project file's contents; media paths come back absolute. `file` must be absolute.
    pub fn read(file: &str, contents: &Value) -> Result<Project, String> {
        let base = Path::new(file).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let mut project = Project::from_json(contents)?;
        for v in project.media.values_mut() {
            *v = absolute_path(v, &base);
        }
        Ok(project)
    }

    /// Replace the session with a project that has been read and whose media has been described.
    pub fn open(&mut self, file: &str, mut project: Project, infos: Infos) {
        while project.lanes.len() < LANES {
            project.lanes.push(AudioLane::default());
        }
        self.selected = if project.clips.is_empty() { None } else { Some(0) };
        self.sel_audio = None;
        self.project = project;
        self.infos = infos;
        self.undo.clear();
        self.redo.clear();
        self.mark_saved(file);
    }

    /// Autosaving must not count as saving the named project: put these back afterwards.
    pub fn saved_marker(&self) -> (Option<String>, String) {
        (self.file.clone(), self.saved.clone())
    }

    pub fn restore_saved_marker(&mut self, marker: (Option<String>, String)) {
        (self.file, self.saved) = marker;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::int;

    fn info(name: &str) -> MediaInfo {
        let text = match name {
            "clip1" => include_str!("../../../tests/fixtures/clip1.json"),
            _ => include_str!("../../../tests/fixtures/clip2.json"),
        };
        MediaInfo::from_json(&serde_json::from_str(text).unwrap()).unwrap()
    }

    fn session() -> Session {
        let mut s = Session::new();
        s.add_media("/media/clip1.mp4", info("clip1")).unwrap();
        s.add_media("/media/clip2.mp4", info("clip2")).unwrap();
        s
    }

    fn rect(c: &Clip) -> Rect {
        match c.crop {
            Some(Crop::Rect(r)) => r,
            _ => panic!("no rect crop"),
        }
    }

    #[test]
    fn new_clips_cover_the_whole_file() {
        let s = session();
        assert_eq!(s.clips()[0].end, info("clip1").duration);
        assert_eq!((s.clips()[1].start, s.clips()[1].end), (int(0), info("clip2").duration));
        assert_eq!((s.selected, s.clips()[0].audio, s.clips()[1].audio), (Some(1), false, true));
        assert_eq!(s.project.media["clip2"], "/media/clip2.mp4");
    }

    #[test]
    fn trim_split_and_undo() {
        let mut s = session();
        s.select(0);
        s.checkpoint();
        s.set_in(int(5));
        s.set_out(int(15));
        assert!(s.split(int(9)));
        let spans: Vec<_> = s.clips()[..2].iter().map(|c| (c.media.as_str(), c.start, c.end)).collect();
        assert_eq!(spans, vec![("clip1", int(5), int(9)), ("clip1", int(9), int(15))]);
        assert_eq!(s.selected, Some(1));
        assert!(!s.split(int(9))); // nothing to split at a clip's own edge
        assert!(s.undo());
        assert_eq!((s.clips().len(), s.clips()[0].end), (2, int(15)));
        assert!(s.undo());
        assert_eq!((s.clips()[0].start, s.clips()[0].end), (int(0), info("clip1").duration));
        assert!(s.redo());
        assert_eq!(s.clips()[0].start, int(5));
    }

    #[test]
    fn trim_is_clamped() {
        let mut s = session();
        s.select(0);
        s.set_out(int(999));
        s.set_in(int(999));
        let c = &s.clips()[0];
        assert_eq!(c.end, info("clip1").duration);
        assert_eq!(c.end - c.start, s.frame());
    }

    #[test]
    fn one_shape_for_all_clips_and_unlock() {
        let mut s = session();
        s.set_shape(&Shape::Ratio(r(4, 3)));
        assert_eq!(rect(&s.clips()[0]), Rect { x: 0, y: 0, w: 640, h: 480 });
        assert_eq!(rect(&s.clips()[1]), Rect { x: 0, y: 555, w: 1080, h: 810 });
        assert_eq!(plan(&s.project, &s.infos, Mode::Encode, false).unwrap().video.unwrap().width, 1080);
        s.lock_shape = false;
        s.set_shape(&Shape::Ratio(int(1))); // only the selected clip (clip2)
        assert_eq!((s.clips()[0].crop_aspect, s.clips()[1].crop_aspect), (Some(r(4, 3)), Some(int(1))));
        assert_eq!(s.shape(), Shape::Ratio(int(1)));
        s.lock_shape = true;
        s.set_shape(&Shape::Off);
        assert!(s.clips().iter().all(|c| c.crop.is_none()));
        assert_eq!(s.shape(), Shape::Off);
    }

    #[test]
    fn crop_stays_inside_the_frame() {
        let mut s = session();
        s.select(0);
        s.set_crop(Rect { x: 600, y: -50, w: 9999, h: 100 });
        assert_eq!(rect(s.clip().unwrap()), Rect { x: 0, y: 0, w: 640, h: 100 });
    }

    fn trimmed() -> Session {
        let mut s = session();
        s.select(0);
        s.set_in(int(5));
        s.set_out(int(15));
        s.select(1);
        s.set_in(int(7));
        s.set_out(int(17));
        s
    }

    #[test]
    fn reorder_remove_and_montage_time() {
        let mut s = trimmed();
        assert_eq!((s.offsets(), s.total()), (vec![int(0), int(10)], int(20)));
        assert_eq!(s.montage_time(&int(9)), int(12));
        assert_eq!((s.locate(&int(3)), s.locate(&int(12)), s.locate(&int(99))), (Some((0, int(8))), Some((1, int(9))), Some((1, int(17)))));
        s.move_clip(1, 0);
        assert_eq!((s.clips()[0].media.as_str(), s.selected), ("clip2", Some(0)));
        s.remove(0);
        assert_eq!((s.clips().len(), s.clips()[0].media.as_str(), s.selected), (1, "clip1", Some(0)));
        s.remove(0);
        assert_eq!((s.clips().len(), s.selected, s.target()), (0, None, None));
        assert!(s.plan().unwrap().is_none());
    }

    #[test]
    fn plan_follows_the_exact_switch() {
        let mut s = Session::new();
        s.add_media("/media/clip1.mp4", info("clip1")).unwrap();
        s.set_in(int(5));
        s.set_out(int(15));
        assert_eq!(s.plan().unwrap().unwrap().kind, Kind::Copy);
        s.exact = true;
        let p = s.plan().unwrap().unwrap();
        assert_eq!(p.kind, Kind::Encode);
        assert_eq!(s.default_output(&p), "/media/clip1-cut.mp4");
    }

    #[test]
    fn sound_lanes() {
        let mut s = session();
        assert_eq!(s.add_sound("/media/clip2.mp4", info("clip2"), int(0)), Ok((0, 0)));
        s.set_in(int(7));
        s.set_out(int(17)); // trims the sound item, not the clip
        assert_eq!((s.sound().unwrap().start, s.sound().unwrap().end, s.clips()[1].start), (int(7), int(17), int(0)));
        assert_eq!(s.add_sound("/media/clip2.mp4", info("clip2"), int(5)), Ok((1, 0))); // overlaps: other lane
        assert_eq!(s.sound_at(0, &int(3)).map(|(_, t)| t), Some(int(10)));
        assert!(s.sound_at(0, &int(10)).is_none());
        s.select_sound(0, 0);
        s.move_sound(int(40), Some(1));
        assert_eq!((s.sel_audio, s.sound().unwrap().at, s.lanes()[0].items.len()), (Some((1, 1)), int(40), 0));
        s.set_gain(-200.0);
        assert_eq!(s.sound().unwrap().gain_db, -60.0);
        s.remove_sound();
        assert_eq!((s.sel_audio, s.lanes()[1].items.len()), (None, 1));
        assert!(s.undo());
        assert_eq!(s.lanes()[1].items.len(), 2);
        assert!(s.add_sound("/media/clip1.mp4", info("clip1"), int(0)).is_err()); // clip1 has no sound
    }

    #[test]
    fn sound_under_both_clips_plans_two_sources() {
        let mut s = trimmed();
        s.add_sound("/media/clip2.mp4", info("clip2"), int(0)).unwrap();
        s.set_in(int(7));
        s.set_out(int(17));
        let p = s.plan().unwrap().unwrap();
        let mut at: Vec<R> = p.audio.iter().map(|a| a.at).collect();
        at.sort();
        assert_eq!((p.kind, at), (Kind::Encode, vec![int(0), int(10)]));
    }

    #[test]
    fn project_file_round_trip() {
        let mut s = session();
        s.select(0);
        s.set_in(int(5));
        s.set_out(int(15));
        s.set_shape(&Shape::Ratio(r(4, 3)));
        s.add_sound("/media/clip2.mp4", info("clip2"), int(2)).unwrap();
        assert!(s.dirty());
        let file = "/projects/a/edit.pve";
        let stored = s.file_json(file);
        assert_eq!(stored["media"]["clip1"]["path"], "../../media/clip1.mp4");
        s.mark_saved(file);
        assert!(!s.dirty());
        let project = Session::read(file, &stored).unwrap();
        assert_eq!(project.media, s.project.media); // absolute again
        let mut t = Session::new();
        t.open(file, project, s.infos.clone());
        assert_eq!(t.project.to_json(), s.project.to_json());
        assert!(!t.dirty());
        t.set_out(int(14));
        assert!(t.dirty());
    }

    #[test]
    fn centered_rects() {
        assert_eq!(centered_rect(1920.0, 1080.0, &r(4, 3)), Rect { x: 240, y: 0, w: 1440, h: 1080 });
        assert_eq!(centered_rect(1080.0, 1920.0, &int(1)), Rect { x: 0, y: 420, w: 1080, h: 1080 });
    }

    #[test]
    fn paths_without_a_filesystem() {
        assert_eq!(relative_path("/a/b/c.mp4", "/a/b"), "c.mp4");
        assert_eq!(relative_path("/a/x/c.mp4", "/a/b/d"), "../../x/c.mp4");
        assert_eq!(absolute_path("../x/c.mp4", "/a/b"), "/a/x/c.mp4");
        assert_eq!(absolute_path("/q/c.mp4", "/a/b"), "/q/c.mp4");
    }
}
