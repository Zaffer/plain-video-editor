//! Export planner: Project + MediaInfo -> a neutral ExportPlan.
//!
//! The plan says *what* to do (copy these packet ranges, or re-encode these
//! ranges to this target and mix this audio). It contains no ffmpeg syntax; a
//! backend lowers it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::media::{MediaInfo, VideoInfo};
use crate::model::{Crop, Project, Rect};
use crate::time::{self, even, int, to_f64, R};

pub const DRAFT_MAX_HEIGHT: i128 = 480;
pub const DRAFT_MAX_FPS: i128 = 30;

pub type Infos = BTreeMap<String, MediaInfo>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Copy when possible, otherwise re-encode.
    Auto,
    /// Copy or fail.
    Copy,
    /// Always re-encode, with exact cuts.
    Encode,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "auto" => Some(Mode::Auto),
            "copy" => Some(Mode::Copy),
            "encode" => Some(Mode::Encode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceColor {
    /// "sdr", "hlg" or "pq".
    pub kind: &'static str,
    /// "bt709", "bt601" or "bt2020nc" (resolved; never empty).
    pub matrix: &'static str,
    pub full_range: bool,
    pub bit_depth: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub media: String,
    /// What will actually be used.
    pub start: R,
    pub end: R,
    /// Differs from start/end only when a copy snaps to keyframes.
    pub requested_start: R,
    pub requested_end: R,
    /// Copy only: decode time of the first packet that is NOT copied (the keyframe
    /// at `end`). None = copy to the end of the file.
    pub end_decode: Option<R>,
    /// In decoded pixels (upright, before pixel-aspect correction).
    pub crop: Option<Rect>,
    /// Size inside the canvas; smaller than the canvas = padded.
    pub fit: Option<(i64, i64)>,
    pub color: Option<SourceColor>,
}

impl Segment {
    pub fn duration(&self) -> R {
        self.end - self.start
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioSource {
    pub media: String,
    pub start: R,
    pub end: R,
    /// Position in the output.
    pub at: R,
    pub gain_db: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoTarget {
    pub width: i64,
    pub height: i64,
    pub fps: R,
    /// "sdr", "hlg" or "pq".
    pub color: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Lossless stream copy.
    Copy,
    Encode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportPlan {
    pub kind: Kind,
    pub duration: R,
    pub segments: Vec<Segment>,
    /// Encode only.
    pub video: Option<VideoTarget>,
    /// Encode only.
    pub audio: Vec<AudioSource>,
    /// Copy only.
    pub copy_audio: bool,
    /// Why an encode plan is not lossless.
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub draft: bool,
}

impl ExportPlan {
    /// The plan as JSON: what `pve plan --json` prints and `testdata/golden` holds.
    pub fn to_json(&self) -> Value {
        let t = |x: &R| json!(time::to_string(x));
        json!({
            "kind": match self.kind { Kind::Copy => "copy", Kind::Encode => "encode" },
            "duration": t(&self.duration),
            "segments": self.segments.iter().map(|s| json!({
                "media": s.media,
                "start": t(&s.start), "end": t(&s.end),
                "requested_start": t(&s.requested_start), "requested_end": t(&s.requested_end),
                "end_decode": s.end_decode.as_ref().map(t),
                "crop": s.crop.map(|c| json!({"x": c.x, "y": c.y, "w": c.w, "h": c.h})),
                "fit": s.fit.map(|(w, h)| json!([w, h])),
                "color": s.color.as_ref().map(|c| json!({
                    "kind": c.kind, "matrix": c.matrix, "full_range": c.full_range, "bit_depth": c.bit_depth,
                })),
            })).collect::<Vec<_>>(),
            "video": self.video.as_ref().map(|v| json!({
                "width": v.width, "height": v.height, "fps": t(&v.fps), "color": v.color,
            })),
            "audio": self.audio.iter().map(|a| json!({
                "media": a.media, "start": t(&a.start), "end": t(&a.end), "at": t(&a.at), "gain_db": a.gain_db,
            })).collect::<Vec<_>>(),
            "copy_audio": self.copy_audio,
            "reasons": self.reasons,
            "warnings": self.warnings,
            "draft": self.draft,
        })
    }
}

pub type Result<T> = std::result::Result<T, String>;

pub fn source_color(v: &VideoInfo) -> SourceColor {
    let matrix = match v.matrix.as_str() {
        "smpte170m" | "bt470bg" => "bt601",
        "bt709" => "bt709",
        "bt2020nc" => "bt2020nc",
        // Untagged: same guess players make. HD is BT.709, SD is BT.601.
        _ if v.width >= 1280 || v.height > 576 => "bt709",
        _ => "bt601",
    };
    SourceColor { kind: v.color_kind(), matrix, full_range: v.full_range, bit_depth: v.bit_depth }
}

/// A number the short way for messages, as C's "%g" prints it: six significant digits, no trailing zeros.
pub fn fmt_g(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let exp = x.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        return format!("{x:e}");
    }
    let s = format!("{:.*}", (5 - exp).max(0) as usize, x);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    }
}

/// Project crop (display pixels) -> crop in decoded pixels, or None for the full frame.
pub fn resolve_crop(crop: Option<&Crop>, v: &VideoInfo) -> Result<Option<Rect>> {
    let Some(crop) = crop else { return Ok(None) };
    let (dw, dh) = v.decoded_size();
    let (dwr, dhr) = (int(dw as i128), int(dh as i128));
    let (sx, sy) = v.display_scale();
    let (disp_w, disp_h) = v.display_size();
    let (x, y, w, h) = match crop {
        Crop::Aspect(ratio) => {
            let (w, h) = if disp_w / disp_h > *ratio { (disp_h * *ratio / sx, dhr) } else { (dwr, disp_w / *ratio / sy) };
            let (w, h) = ((even(&w, 2) as i64).min(dw), (even(&h, 2) as i64).min(dh));
            let x = even(&(int((dw - w) as i128) / int(2)), 0) as i64;
            let y = even(&(int((dh - h) as i128) / int(2)), 0) as i64;
            (x, y, w, h)
        }
        Crop::Rect(c) => {
            let x = even(&(int(c.x as i128) / sx), 0) as i64;
            let w = even(&(int(c.w as i128) / sx), 2) as i64;
            let y = even(&(int(c.y as i128) / sy), 0) as i64;
            let h = even(&(int(c.h as i128) / sy), 2) as i64;
            if x + w > dw || y + h > dh {
                return Err(format!(
                    "crop {}x{}+{}+{} is outside the {}x{} frame",
                    c.w, c.h, c.x, c.y, fmt_g(to_f64(&disp_w)), fmt_g(to_f64(&disp_h))
                ));
            }
            (x, y, w, h)
        }
    };
    if (x, y, w, h) == (0, 0, dw, dh) {
        return Ok(None);
    }
    Ok(Some(Rect { x, y, w, h }))
}

fn differences(a: &MediaInfo, b: &MediaInfo, with_audio: bool) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    let mut note = |differs: bool, label: &'static str| {
        if differs && !out.contains(&label) {
            out.push(label);
        }
    };
    if let (Some(x), Some(y)) = (&a.video, &b.video) {
        note(x.codec != y.codec, "codec");
        note(x.profile != y.profile, "profile");
        note(x.level != y.level, "level");
        note(x.width != y.width, "width");
        note(x.height != y.height, "height");
        note(x.sar != y.sar, "pixel shape");
        note(x.rotation != y.rotation, "rotation");
        note(x.fps != y.fps, "frame rate");
        note(x.pix_fmt != y.pix_fmt, "pixel format");
        note(x.primaries != y.primaries, "colour");
        note(x.transfer != y.transfer, "colour");
        note(x.matrix != y.matrix, "colour");
        note(x.full_range != y.full_range, "colour range");
        note(x.time_base != y.time_base, "time base");
        note(x.field_order != y.field_order, "field order");
        note(x.codec_tag != y.codec_tag, "codec tag");
        note(x.extradata_hash != y.extradata_hash, "encoder settings");
    }
    if with_audio {
        if let (Some(x), Some(y)) = (&a.audio, &b.audio) {
            note(x.codec != y.codec, "audio codec");
            note(x.profile != y.profile, "audio codec");
            note(x.sample_rate != y.sample_rate, "audio sample rate");
            note(x.layout != y.layout, "audio channels");
            note(x.time_base != y.time_base, "audio time base");
            note(x.extradata_hash != y.extradata_hash, "audio settings");
        }
    }
    out
}

/// Reasons this project cannot be exported by stream copy. Empty = lossless is possible.
fn copy_blockers(project: &Project, infos: &Infos, crops: &[Option<Rect>]) -> Vec<String> {
    let mut why = Vec::new();
    if project.output_size.is_some() {
        why.push("an output size is set".to_string());
    }
    for (i, c) in crops.iter().enumerate() {
        if c.is_some() {
            why.push(format!("clip {} is cropped", i + 1));
        }
    }
    if project.lanes.iter().any(|l| !l.items.is_empty()) {
        why.push("audio lanes need mixing".into());
    }
    if project.clips.iter().any(|c| c.gain_db != 0.0) {
        why.push("a clip's volume is changed".into());
    }
    let has_audio: Vec<bool> = project.clips.iter().map(|c| c.audio && infos[&c.media].audio.is_some()).collect();
    let all_audio = has_audio.iter().all(|&b| b);
    if has_audio.iter().any(|&b| b) && !all_audio {
        why.push("some clips have sound and others do not".into());
    }
    let first_media = &project.clips[0].media;
    let first = &infos[first_media];
    for (i, c) in project.clips.iter().enumerate().skip(1) {
        if &c.media == first_media {
            continue;
        }
        let diff = differences(first, &infos[&c.media], all_audio);
        if !diff.is_empty() {
            why.push(format!("clip {} differs from clip 1 ({})", i + 1, diff.join(", ")));
        }
    }
    if project.clips.iter().any(|c| infos[&c.media].keyframes.is_none()) {
        why.push("keyframes are not indexed".into());
    }
    why
}

/// Widen [start, end] to keyframes: start to the keyframe at or before, end to the one at or after.
///
/// Returns (start, end, decode time of the end keyframe or None when the cut runs to the end of the file).
pub fn snap(info: &MediaInfo, start: &R, end: &R) -> (R, R, Option<R>) {
    let fallback = [time::zero()];
    let kf: &[R] = match info.keyframes.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => &fallback,
    };
    let i = kf.partition_point(|k| k <= start); // index after the last keyframe <= start
    let s = if i > 0 { kf[i - 1] } else { time::zero() };
    let j = kf.partition_point(|k| k < end); // first keyframe >= end
    if j >= kf.len() {
        return (s, info.duration, None);
    }
    let dts = info.keyframe_dts.as_deref().and_then(|d| d.get(j).copied()).unwrap_or(kf[j]);
    (s, kf[j], Some(dts))
}

pub fn plan(project: &Project, infos: &Infos, mode: Mode, draft: bool) -> Result<ExportPlan> {
    project.validate()?;
    for (i, c) in project.clips.iter().enumerate() {
        let info = infos.get(&c.media).ok_or_else(|| format!("clip {}: {:?} has not been read", i + 1, c.media))?;
        let Some(v) = &info.video else { return Err(format!("clip {}: {:?} has no picture", i + 1, c.media)) };
        if c.end > info.duration + v.fps.recip() {
            return Err(format!(
                "clip {}: out point {}s is past the end ({:.3}s)",
                i + 1, fmt_g(to_f64(&c.end)), to_f64(&info.duration)
            ));
        }
    }
    for lane in &project.lanes {
        for it in &lane.items {
            if !infos.contains_key(&it.media) {
                return Err(format!("{:?} has not been read", it.media));
            }
        }
    }
    let mut crops = Vec::new();
    for c in &project.clips {
        crops.push(resolve_crop(c.crop.as_ref(), infos[&c.media].video.as_ref().unwrap())?);
    }
    let blockers = copy_blockers(project, infos, &crops);
    if mode == Mode::Copy && !blockers.is_empty() {
        return Err(format!("cannot export losslessly: {}", blockers.join("; ")));
    }
    if mode != Mode::Encode && blockers.is_empty() && !draft {
        return Ok(plan_copy(project, infos));
    }
    let reasons = if mode != Mode::Encode { blockers } else { vec!["exact cuts were requested".to_string()] };
    plan_encode(project, infos, &crops, reasons, draft)
}

fn plan_copy(project: &Project, infos: &Infos) -> ExportPlan {
    let mut segs = Vec::new();
    for c in &project.clips {
        let (s, e, e_dts) = snap(&infos[&c.media], &c.start, &c.end);
        segs.push(Segment {
            media: c.media.clone(),
            start: s,
            end: e,
            requested_start: c.start,
            requested_end: c.end,
            end_decode: e_dts,
            crop: None,
            fit: None,
            color: None,
        });
    }
    let mut warnings = Vec::new();
    if segs.len() > 1 && infos[&segs[0].media].video.as_ref().is_some_and(|v| v.codec == "hevc") {
        warnings.push("joining HEVC losslessly can glitch at the joins on some files; check the result".to_string());
    }
    let first = &project.clips[0];
    ExportPlan {
        kind: Kind::Copy,
        duration: segs.iter().fold(time::zero(), |a, s| a + s.duration()),
        copy_audio: first.audio && infos[&first.media].audio.is_some(),
        segments: segs,
        video: None,
        audio: Vec::new(),
        reasons: Vec::new(),
        warnings,
        draft: false,
    }
}

fn plan_encode(project: &Project, infos: &Infos, crops: &[Option<Rect>], reasons: Vec<String>, draft: bool) -> Result<ExportPlan> {
    // Display size of each clip after its crop.
    let mut sizes: Vec<(R, R)> = Vec::new();
    for (c, crop) in project.clips.iter().zip(crops) {
        let v = infos[&c.media].video.as_ref().unwrap();
        let (w, h) = crop.map_or(v.decoded_size(), |r| (r.w, r.h));
        let (sx, sy) = v.display_scale();
        sizes.push((int(w as i128) * sx, int(h as i128) * sy));
    }

    let (mut cw, mut ch) = match project.output_size {
        Some((w, h)) => (int(w as i128), int(h as i128)),
        None => {
            // Canvas = the largest cropped clip (the first, if several tie), so nothing is downscaled.
            let mut best = sizes[0];
            for s in &sizes[1..] {
                if s.0 * s.1 > best.0 * best.1 {
                    best = *s;
                }
            }
            best
        }
    };
    if draft && ch > int(DRAFT_MAX_HEIGHT) {
        cw = cw * int(DRAFT_MAX_HEIGHT) / ch;
        ch = int(DRAFT_MAX_HEIGHT);
    }
    let (cw, ch) = (even(&cw, 2), even(&ch, 2));

    // Keep HDR only when every clip is the same kind of HDR; otherwise standard range.
    let kinds: BTreeSet<&str> = project.clips.iter().map(|c| infos[&c.media].video.as_ref().unwrap().color_kind()).collect();
    let color: &'static str = if kinds.len() == 1 && !draft {
        infos[&project.clips[0].media].video.as_ref().unwrap().color_kind()
    } else {
        "sdr"
    };

    let mut fps = project.clips.iter().map(|c| infos[&c.media].video.as_ref().unwrap().fps).max().unwrap();
    if draft {
        fps = fps.min(int(DRAFT_MAX_FPS));
    }

    let (mut segments, mut audio, mut t) = (Vec::new(), Vec::new(), time::zero());
    for ((c, crop), (w, h)) in project.clips.iter().zip(crops).zip(&sizes) {
        let info = &infos[&c.media];
        let scale = (int(cw) / *w).min(int(ch) / *h);
        let (mut fw, mut fh) = (even(&(*w * scale), 2).min(cw), even(&(*h * scale), 2).min(ch));
        if cw - fw <= 2 && ch - fh <= 2 {
            // near enough: fill the canvas rather than pad a 1px border
            (fw, fh) = (cw, ch);
        }
        segments.push(Segment {
            media: c.media.clone(),
            start: c.start,
            end: c.end,
            requested_start: c.start,
            requested_end: c.end,
            end_decode: None,
            crop: *crop,
            fit: Some((fw as i64, fh as i64)),
            color: Some(source_color(info.video.as_ref().unwrap())),
        });
        if c.audio && info.audio.is_some() {
            audio.push(AudioSource { media: c.media.clone(), start: c.start, end: c.end, at: t, gain_db: c.gain_db });
        }
        t += c.duration();
    }

    for (li, lane) in project.lanes.iter().enumerate() {
        for (ii, it) in lane.items.iter().enumerate() {
            if infos[&it.media].audio.is_none() {
                return Err(format!("audio lane {} item {}: {:?} has no sound", li + 1, ii + 1, it.media));
            }
            if it.at >= t {
                continue; // entirely after the end of the video
            }
            audio.push(AudioSource {
                media: it.media.clone(),
                start: it.start,
                end: it.end.min(it.start + (t - it.at)),
                at: it.at,
                gain_db: lane.gain_db + it.gain_db,
            });
        }
    }

    Ok(ExportPlan {
        kind: Kind::Encode,
        duration: t,
        segments,
        video: Some(VideoTarget { width: cw as i64, height: ch as i64, fps, color }),
        audio,
        copy_audio: false,
        reasons,
        warnings: Vec::new(),
        draft,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testdata::{infos, montage, project};

    fn clips(clips: Value) -> Project {
        project(clips, json!([])).unwrap()
    }

    fn auto(p: &Project) -> Result<ExportPlan> {
        plan(p, &infos(), Mode::Auto, false)
    }

    #[test]
    fn montage_takes_the_larger_shape_and_mixes_the_lane() {
        let p = auto(&montage()).unwrap();
        assert_eq!((p.kind, p.duration), (Kind::Encode, int(20)));
        // clip2's 4:3 crop (1080x810 on screen) is larger than clip1 (640x480), so it sets the size
        let v = p.video.unwrap();
        assert_eq!((v.width, v.height, v.color), (1080, 810, "sdr"));
        // clip 2's sound under clip 1 (lane) and again under clip 2 (its own)
        let mut at: Vec<R> = p.audio.iter().map(|a| a.at).collect();
        at.sort();
        assert_eq!(at, vec![int(0), int(10)]);
        assert!(plan(&montage(), &infos(), Mode::Auto, true).unwrap().draft);
    }

    #[test]
    fn trim_is_lossless_and_snaps_outwards() {
        let infos = infos();
        let p = auto(&clips(json!([{"media": "clip1", "in": 5, "out": 15}]))).unwrap();
        let s = &p.segments[0];
        assert_eq!(p.kind, Kind::Copy);
        assert!(s.start <= int(5) && s.end >= int(15));
        let keyframes = infos["clip1"].keyframes.as_ref().unwrap();
        assert!(keyframes.contains(&s.start) && keyframes.contains(&s.end));
        assert!(s.end_decode.unwrap() < s.end); // clip1 has reordered frames
    }

    #[test]
    fn trim_to_end_of_file_has_no_stop_point() {
        let p = auto(&clips(json!([{"media": "clip2", "in": 25, "out": 31}]))).unwrap();
        assert_eq!((p.segments[0].start, p.segments[0].end_decode), (int(20), None));
        assert_eq!(p.segments[0].end, infos()["clip2"].duration);
    }

    #[test]
    fn join_of_the_same_source_is_lossless() {
        let p = auto(&clips(json!([{"media": "clip1", "in": 5, "out": 7}, {"media": "clip1", "in": 18, "out": 20}]))).unwrap();
        assert_eq!(p.kind, Kind::Copy);
    }

    #[test]
    fn exact_mode_reencodes_with_the_cuts_as_asked() {
        let p = plan(&clips(json!([{"media": "clip1", "in": 5, "out": 15}])), &infos(), Mode::Encode, false).unwrap();
        assert_eq!((p.kind, p.segments[0].start, p.segments[0].end), (Kind::Encode, int(5), int(15)));
    }

    #[test]
    fn mismatched_clips_fall_back_and_say_why() {
        let p = clips(json!([{"media": "clip1", "in": 5, "out": 7}, {"media": "clip2", "in": 1, "out": 3}]));
        let auto = auto(&p).unwrap();
        assert_eq!(auto.kind, Kind::Encode);
        assert!(auto.reasons.iter().any(|r| r.contains("differs from clip 1")), "{:?}", auto.reasons);
        assert!(plan(&p, &infos(), Mode::Copy, false).is_err());
    }

    #[test]
    fn hdr_is_kept_only_when_every_clip_is_the_same_hdr() {
        let only_hdr = clips(json!([{"media": "clip2", "in": 7, "out": 10, "crop": {"aspect": "1:1"}}]));
        assert_eq!(auto(&only_hdr).unwrap().video.unwrap().color, "hlg");
        assert_eq!(auto(&montage()).unwrap().video.unwrap().color, "sdr");
    }

    #[test]
    fn crop_accounts_for_non_square_pixels() {
        // clip2 is stored 1080x1080 with 9:16 pixels: it displays as 1080x1920 (stretched, never squeezed).
        assert_eq!(infos()["clip2"].video.as_ref().unwrap().display_size(), (int(1080), int(1920)));
        let p = auto(&clips(json!([{"media": "clip2", "in": 0, "out": 1, "crop": {"x": 0, "y": 320, "w": 540, "h": 800}}]))).unwrap();
        assert_eq!(p.segments[0].crop, Some(Rect { x: 0, y: 180, w: 540, h: 450 }));
        let v = p.video.unwrap();
        assert_eq!((v.width, v.height), (540, 800));
        assert!(auto(&clips(json!([{"media": "clip2", "in": 0, "out": 1, "crop": {"x": 800, "y": 0, "w": 400, "h": 400}}]))).is_err());
    }

    #[test]
    fn different_shapes_are_fitted_with_bars() {
        let p = auto(&clips(json!([{"media": "clip1", "in": 5, "out": 7}, {"media": "clip2", "in": 1, "out": 3}]))).unwrap();
        let v = p.video.as_ref().unwrap();
        assert_eq!((v.width, v.height), (1080, 1920));
        assert_eq!(p.segments[0].fit, Some((1080, 810)));
    }

    #[test]
    fn what_the_media_cannot_give_is_refused() {
        // an out point past the end of the file
        assert!(auto(&clips(json!([{"media": "clip1", "in": 5, "out": 99}]))).is_err());
        // a sound item from a file without sound
        let silent = project(json!([{"media": "clip2", "in": 0, "out": 5}]), json!([{"items": [{"media": "clip1", "in": 0, "out": 5}]}])).unwrap();
        assert!(auto(&silent).is_err());
    }
}
