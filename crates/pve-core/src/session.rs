//! Editing session: the project being edited, the selection, undo, and timeline maths.
//!
//! Pure logic with no GUI or player types. The GUI calls these operations and redraws.
//!
//! Either a video clip or a sound item is "the target": the thing the trim bar,
//! the timestamps and the volume act on.
//!
//! Every sound is shown in a lane. A clip's own sound sits in lane 1 as a linked
//! item: it is not stored as an item but follows the clip (trim, split, reorder)
//! and stays part of a lossless copy. Moving or trimming it by itself turns it
//! into an ordinary item, which no longer follows the clip.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use crate::media::MediaInfo;
use crate::model::{AudioItem, AudioLane, Clip, Crop, Project, Rect};
use crate::planner::{plan, ExportPlan, Infos, Kind, Mode};
use crate::time::{self, r, to_f64, R};

/// Smallest crop, in display pixels.
pub const MIN_CROP: i64 = 16;
pub const UNDO_LIMIT: usize = 200;
/// Sound lanes; the clips' own sound shows in the first.
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
    /// The own sound of this clip.
    Own(usize),
}

/// A sound shown in a lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundRef {
    /// A clip's own sound, by clip index: shown in lane 1 and linked to the clip.
    Own(usize),
    /// A stored item: (lane, index).
    Item(usize, usize),
}

#[derive(Clone)]
struct Snapshot {
    clips: Vec<Clip>,
    lanes: Vec<AudioLane>,
    selected: Option<usize>,
    sel_audio: Option<(usize, usize)>,
    sel_own: bool,
}

pub struct Session {
    pub project: Project,
    pub infos: Infos,
    /// Selected clip.
    pub selected: Option<usize>,
    /// (lane, item) when a sound item is the target.
    pub sel_audio: Option<(usize, usize)>,
    /// The selected clip's own sound is the target.
    pub sel_own: bool,
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
            sel_own: false,
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

    /// The selected sound item, if a sound item is the target. For a clip's own sound this is made from the clip.
    pub fn sound(&self) -> Option<AudioItem> {
        if self.sel_own {
            return self.own_sound(self.selected?);
        }
        self.sel_audio.and_then(|(l, i)| self.project.lanes.get(l)?.items.get(i).cloned())
    }

    /// Clip i's own sound as it sits in lane 1, or None if the clip has none (or it was removed).
    pub fn own_sound(&self, i: usize) -> Option<AudioItem> {
        let c = self.project.clips.get(i)?;
        if !c.audio || self.infos.get(&c.media)?.audio.is_none() {
            return None;
        }
        Some(AudioItem { media: c.media.clone(), start: c.start, end: c.end, at: self.offsets()[i], gain_db: c.gain_db })
    }

    /// Everything shown in a lane, as (reference, item, linked): for lane 1 the clips' own sounds, then its items.
    pub fn lane_items(&self, lane: usize) -> Vec<(SoundRef, AudioItem, bool)> {
        let mut out = Vec::new();
        if lane == 0 {
            out.extend((0..self.project.clips.len()).filter_map(|i| Some((SoundRef::Own(i), self.own_sound(i)?, true))));
        }
        if let Some(l) = self.project.lanes.get(lane) {
            out.extend(l.items.iter().enumerate().map(|(i, it)| (SoundRef::Item(lane, i), it.clone(), false)));
        }
        out
    }

    /// Which sound is the target, if one is.
    pub fn sound_ref(&self) -> Option<SoundRef> {
        match self.target()? {
            Target::Own(i) => Some(SoundRef::Own(i)),
            Target::Sound(l, i) => Some(SoundRef::Item(l, i)),
            Target::Clip(_) => None,
        }
    }

    /// The lane the selected sound is shown in.
    pub fn sound_lane(&self) -> Option<usize> {
        match self.sound_ref()? {
            SoundRef::Own(_) => Some(0),
            SoundRef::Item(lane, _) => Some(lane),
        }
    }

    pub fn target(&self) -> Option<Target> {
        let clip = self.selected.filter(|&i| i < self.project.clips.len());
        if self.sel_own {
            if let Some(i) = clip.filter(|&i| self.own_sound(i).is_some()) {
                return Some(Target::Own(i));
            }
        } else if let Some((l, i)) = self.sel_audio {
            if self.project.lanes.get(l).is_some_and(|lane| i < lane.items.len()) {
                return Some(Target::Sound(l, i));
            }
        }
        clip.map(Target::Clip)
    }

    /// (media id, in, out, gain) of the target.
    pub fn target_span(&self) -> Option<(&str, R, R, f64)> {
        match self.target()? {
            Target::Clip(i) | Target::Own(i) => {
                let c = &self.project.clips[i];
                Some((&c.media, c.start, c.end, c.gain_db))
            }
            Target::Sound(l, i) => {
                let s = &self.project.lanes[l].items[i];
                Some((&s.media, s.start, s.end, s.gain_db))
            }
        }
    }

    /// Whether the target makes any sound: a clip without sound of its own does not.
    pub fn target_has_sound(&self) -> bool {
        match self.target() {
            Some(Target::Clip(i)) => self.own_sound(i).is_some(),
            Some(_) => true,
            None => false,
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
        self.clear_sound();
    }

    /// Make a sound the target.
    pub fn select_sound(&mut self, sound: SoundRef) {
        match sound {
            SoundRef::Own(i) => {
                self.selected = Some(i);
                self.sel_audio = None;
                self.sel_own = true;
            }
            SoundRef::Item(lane, i) => {
                self.sel_audio = Some((lane, i));
                self.sel_own = false;
            }
        }
    }

    /// Back to the selected clip as the target.
    pub fn clear_sound(&mut self) {
        self.sel_audio = None;
        self.sel_own = false;
    }

    // ------------------------------------------------------------ undo

    fn snapshot(&self) -> Snapshot {
        Snapshot { clips: self.project.clips.clone(), lanes: self.project.lanes.clone(), selected: self.selected, sel_audio: self.sel_audio, sel_own: self.sel_own }
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
        self.sel_own = s.sel_own;
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
        self.clear_sound();
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

    /// Turn the selected clip's own sound into an ordinary item in lane 1. It no longer follows the clip.
    fn unlink(&mut self) {
        let Some(Target::Own(i)) = self.target() else { return };
        let Some(item) = self.own_sound(i) else { return };
        let c = &mut self.project.clips[i];
        c.audio = false;
        c.gain_db = 0.0;
        while self.project.lanes.len() < LANES {
            self.project.lanes.push(AudioLane::default());
        }
        self.project.lanes[0].items.push(item);
        self.sel_audio = Some((0, self.project.lanes[0].items.len() - 1));
        self.sel_own = false;
    }

    /// The target's in and out, to change in place. A clip's own sound is unlinked first.
    fn span_mut(&mut self) -> Option<(&mut R, &mut R)> {
        self.unlink();
        match self.target()? {
            Target::Own(_) => None,
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
            // a clip's own sound keeps its volume on the clip
            Some(Target::Clip(i) | Target::Own(i)) => self.project.clips[i].gain_db = db,
            Some(Target::Sound(l, i)) => self.project.lanes[l].items[i].gain_db = db,
            None => {}
        }
    }

    // ------------------------------------------------------------ sound lanes

    fn overlaps(&self, lane: usize, at: &R, duration: &R) -> bool {
        let (a0, a1) = (*at, *at + *duration);
        self.lane_items(lane).iter().any(|(_, o, _)| o.at < a1 && a0 < o.at + o.duration())
    }

    /// Add a file's sound at montage time `at`: in `lane` if one is given, otherwise in the
    /// first lane where it overlaps nothing (else the last lane).
    pub fn add_sound(&mut self, path: &str, info: MediaInfo, at: R, lane: Option<usize>) -> Result<(usize, usize), String> {
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
        let lane = match lane {
            Some(n) => n.min(LANES - 1),
            None => (0..LANES).find(|&n| !self.overlaps(n, &item.at, &duration)).unwrap_or(LANES - 1),
        };
        self.project.lanes[lane].items.push(item);
        self.sel_audio = Some((lane, self.project.lanes[lane].items.len() - 1));
        self.sel_own = false;
        Ok(self.sel_audio.unwrap())
    }

    /// Move the selected sound item along the montage, and optionally to another lane.
    pub fn move_sound(&mut self, at: R, lane: Option<usize>) {
        self.unlink();
        let Some(Target::Sound(cur, i)) = self.target() else { return };
        self.project.lanes[cur].items[i].at = at.max(time::zero());
        if let Some(to) = lane.filter(|&to| to != cur && to < self.project.lanes.len()) {
            let item = self.project.lanes[cur].items.remove(i);
            self.project.lanes[to].items.push(item);
            self.sel_audio = Some((to, self.project.lanes[to].items.len() - 1));
        }
    }

    /// Slide the selected sound along the montage by `by` (negative: earlier), not past the
    /// start. A clip's own sound is unlinked by it, as by any move. The caller checkpoints.
    pub fn nudge_sound(&mut self, by: R) {
        if let Some(sound) = self.sound() {
            self.move_sound(sound.at + by, None);
        }
    }

    pub fn remove_sound(&mut self) {
        match self.target() {
            Some(Target::Own(i)) => {
                self.checkpoint();
                self.project.clips[i].audio = false;
                self.project.clips[i].gain_db = 0.0;
            }
            Some(Target::Sound(lane, i)) => {
                self.checkpoint();
                self.project.lanes[lane].items.remove(i);
            }
            _ => return,
        }
        self.clear_sound();
    }

    /// The stored item playing in `lane` at montage time t, and the time within its source.
    /// Clips' own sounds are not included: they play with their clip.
    pub fn sound_at(&self, lane: usize, t: &R) -> Option<(&AudioItem, R)> {
        self.project.lanes.get(lane)?.items.iter().find(|it| it.at <= *t && *t < it.at + it.duration()).map(|it| (it, it.start + (*t - it.at)))
    }

    /// What `lane` has to have ready at montage time t: the stored item playing then, or
    /// failing that the next one to start, with the time within its source to be at, and
    /// whether it is sounding at t. A preview uses it to have each sound waiting in place.
    pub fn sound_cue(&self, lane: usize, t: &R) -> Option<(&AudioItem, R, bool)> {
        if let Some((item, source_time)) = self.sound_at(lane, t) {
            return Some((item, source_time, true));
        }
        let next = self.project.lanes.get(lane)?.items.iter().filter(|it| it.at > *t).min_by_key(|it| it.at)?;
        Some((next, next.start, false))
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
        self.clear_sound();
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
    use crate::testdata::info;
    use crate::time::int;

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
        let mut s = trimmed(); // clip 1 is montage 0-10 with no sound; clip 2 is 10-20 and its own sound shows in lane 1
        let shown: Vec<_> = s.lane_items(0).iter().map(|(r, it, linked)| (*r, it.at, it.duration(), *linked)).collect();
        assert_eq!(shown, vec![(SoundRef::Own(1), int(10), int(10), true)]);
        assert_eq!(s.add_sound("/media/clip2.mp4", info("clip2"), int(0), None), Ok((1, 0))); // would overlap it: lane 2
        s.set_in(int(8));
        s.set_out(int(18)); // trims the sound item, not the clip
        assert_eq!((s.sound().unwrap().start, s.sound().unwrap().end, s.clips()[1].start), (int(8), int(18), int(7)));
        assert_eq!(s.add_sound("/media/clip2.mp4", info("clip2"), int(5), None), Ok((1, 1))); // no free lane: the last one
        s.select_sound(SoundRef::Item(1, 0));
        assert_eq!(s.sound_at(1, &int(3)).map(|(_, t)| t), Some(int(11)));
        assert!(s.sound_at(1, &int(4000)).is_none());
        // what the lane should have waiting: the item sounding now, else the next one, at its start
        assert_eq!(s.sound_cue(1, &int(3)).map(|(it, t, on)| (it.at, t, on)), Some((int(0), int(11), true)));
        assert_eq!(s.sound_cue(1, &int(4000)).map(|c| c.2), None);
        assert_eq!(s.sound_cue(1, &r(-1, 1)).map(|(it, t, on)| (it.at, t, on)), Some((int(0), int(8), false)));
        assert!(s.sound_at(0, &int(12)).is_none()); // a clip's own sound plays with its clip
        s.nudge_sound(r(1, 30));
        s.nudge_sound(int(-1));
        assert_eq!(s.sound().unwrap().at, int(0)); // slid a frame later, then a second earlier: not past the start
        s.move_sound(int(40), Some(0));
        assert_eq!((s.sel_audio, s.sound().unwrap().at, s.lanes()[1].items.len()), (Some((0, 0)), int(40), 1));
        s.set_gain(-200.0);
        assert_eq!(s.sound().unwrap().gain_db, -60.0);
        s.remove_sound();
        assert_eq!((s.sel_audio, s.lanes()[0].items.len()), (None, 0));
        assert!(s.undo());
        assert_eq!(s.lanes()[0].items.len(), 1);
        assert!(s.add_sound("/media/clip1.mp4", info("clip1"), int(0), None).is_err()); // clip1 has no sound
        assert_eq!(s.add_sound("/media/clip2.mp4", info("clip2"), int(12), Some(0)), Ok((0, 1))); // a lane asked for is used, overlap or not
    }

    #[test]
    fn a_clips_own_sound_is_a_linked_item_in_lane_1() {
        fn shown(s: &Session) -> Vec<(SoundRef, R, R, R, bool)> {
            s.lane_items(0).iter().map(|(r, it, linked)| (*r, it.at, it.start, it.end, *linked)).collect()
        }
        let mut s = trimmed();
        s.select_sound(SoundRef::Own(1));
        let sound = s.sound().unwrap();
        assert_eq!((s.sound_ref(), s.sound_lane(), s.selected, sound.at, sound.start), (Some(SoundRef::Own(1)), Some(0), Some(1), int(10), int(7)));
        // it follows the clip through trims and reordering
        s.select(1);
        s.set_in(int(9));
        s.move_clip(1, 0);
        assert_eq!(shown(&s), vec![(SoundRef::Own(0), int(0), int(9), int(17), true)]);
        assert!(s.lane_items(1).is_empty() && s.own_sound(1).is_none() && s.own_sound(7).is_none());

        let kind = |t: &Session| t.plan().unwrap().unwrap().kind;
        let mut t = Session::new();
        t.add_media("/media/clip2.mp4", info("clip2")).unwrap();
        t.set_in(int(10));
        t.set_out(int(20));
        t.select_sound(SoundRef::Own(0));
        t.set_gain(-6.0); // its volume is the clip's volume; still linked
        assert_eq!((t.sound_ref(), t.clip().unwrap().gain_db, t.sound().unwrap().gain_db), (Some(SoundRef::Own(0)), -6.0, -6.0));
        t.set_gain(0.0);
        assert_eq!(kind(&t), Kind::Copy); // linked sound is still part of a lossless copy
        // moving it by itself turns it into an ordinary item
        t.checkpoint();
        t.move_sound(int(2), None);
        assert_eq!((t.sound_ref(), t.clip().unwrap().audio), (Some(SoundRef::Item(0, 0)), false));
        assert_eq!(shown(&t), vec![(SoundRef::Item(0, 0), int(2), int(10), int(20), false)]);
        let p = t.plan().unwrap().unwrap();
        assert_eq!((p.kind, p.audio.iter().map(|a| a.at).collect::<Vec<_>>()), (Kind::Encode, vec![int(2)]));
        assert!(t.undo());
        assert_eq!((t.sound_ref(), t.clip().unwrap().audio, kind(&t)), (Some(SoundRef::Own(0)), true, Kind::Copy));
        // so does trimming it by itself; the clip keeps its own in and out
        t.checkpoint();
        t.set_out(int(15));
        assert_eq!((t.sound_ref(), t.sound().unwrap().end, t.clip().unwrap().end, t.clip().unwrap().audio), (Some(SoundRef::Item(0, 0)), int(15), int(20), false));
        assert!(t.undo());
        // removing it drops the clip's sound
        t.remove_sound();
        assert_eq!((t.sound_ref(), t.sound(), t.clip().unwrap().audio, shown(&t)), (None, None, false, vec![]));
        let p = t.plan().unwrap().unwrap();
        assert_eq!((p.kind, p.copy_audio), (Kind::Copy, false)); // a silent lossless copy
        assert!(t.undo());
        assert_eq!(shown(&t), vec![(SoundRef::Own(0), int(0), int(10), int(20), true)]);
    }

    #[test]
    fn sound_under_both_clips_plans_two_sources() {
        let mut s = trimmed();
        s.add_sound("/media/clip2.mp4", info("clip2"), int(0), None).unwrap();
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
        s.add_sound("/media/clip2.mp4", info("clip2"), int(2), None).unwrap();
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
