//! Neutral description of a media file.
//!
//! The planner only ever sees this. Natively it is filled from ffprobe; a
//! browser build would fill it from a demuxer library.

use serde_json::{json, Value};

use crate::time::{self, R};

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub codec: String,
    pub profile: String,
    pub level: i64,
    /// Stored pixels, before rotation and pixel-aspect correction.
    pub width: i64,
    pub height: i64,
    /// Pixel aspect ratio (1 = square).
    pub sar: R,
    /// Degrees: 0, 90, 180 or 270.
    pub rotation: i64,
    /// Average frame rate.
    pub fps: R,
    pub pix_fmt: String,
    pub bit_depth: i64,
    /// Empty when untagged.
    pub primaries: String,
    pub transfer: String,
    pub matrix: String,
    pub full_range: bool,
    pub time_base: R,
    pub field_order: String,
    pub codec_tag: String,
    pub extradata_hash: String,
}

impl VideoInfo {
    /// Size after rotation, still in stored (possibly non-square) pixels.
    pub fn decoded_size(&self) -> (i64, i64) {
        if self.rotation % 180 == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    pub fn decoded_sar(&self) -> R {
        if self.rotation % 180 == 90 {
            self.sar.recip()
        } else {
            self.sar
        }
    }

    /// (x, y) factors from decoded pixels to display (square) pixels.
    ///
    /// Non-square pixels are stretched, never squeezed, so no stored detail is
    /// lost: 1080x1080 with 9:16 pixels displays as 1080x1920, not 608x1080.
    /// This is also how mpv sizes the picture, so preview and crop coordinates agree.
    pub fn display_scale(&self) -> (R, R) {
        let sar = self.decoded_sar();
        if sar >= time::int(1) {
            (sar, time::int(1))
        } else {
            (time::int(1), sar.recip())
        }
    }

    /// Upright, square-pixel size. May be fractional for odd pixel shapes.
    pub fn display_size(&self) -> (R, R) {
        let (w, h) = self.decoded_size();
        let (sx, sy) = self.display_scale();
        (time::int(w as i128) * sx, time::int(h as i128) * sy)
    }

    /// "sdr", "hlg" or "pq".
    pub fn color_kind(&self) -> &'static str {
        match self.transfer.as_str() {
            "arib-std-b67" => "hlg",
            "smpte2084" => "pq",
            _ => "sdr",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub codec: String,
    pub profile: String,
    pub sample_rate: i64,
    pub channels: i64,
    pub layout: String,
    pub time_base: R,
    pub extradata_hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub container: String,
    /// Seconds, measured from the first timestamp in the file.
    pub duration: R,
    /// None for sound-only files.
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    /// Video keyframe times in seconds from the start of the media, ascending.
    /// None = not indexed (lossless planning is then unavailable).
    pub keyframes: Option<Vec<R>>,
    /// Decode time of each keyframe, parallel to `keyframes`. With reordered
    /// (B) frames a keyframe is decoded slightly before it is shown.
    pub keyframe_dts: Option<Vec<R>>,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_owned()
}

fn i(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

fn t(v: &Value, k: &str) -> Result<R, String> {
    v.get(k).and_then(Value::as_str).and_then(time::parse).ok_or_else(|| format!("bad or missing {k:?}"))
}

fn times(v: Option<&Value>) -> Result<Option<Vec<R>>, String> {
    match v.and_then(Value::as_array) {
        None => Ok(None),
        Some(a) => a
            .iter()
            .map(|x| x.as_str().and_then(time::parse).ok_or_else(|| "bad keyframe time".to_string()))
            .collect::<Result<Vec<R>, String>>()
            .map(Some),
    }
}

impl MediaInfo {
    pub fn from_json(d: &Value) -> Result<MediaInfo, String> {
        let video = match d.get("video") {
            Some(v) if v.is_object() => Some(VideoInfo {
                codec: s(v, "codec"),
                profile: s(v, "profile"),
                level: i(v, "level"),
                width: i(v, "width"),
                height: i(v, "height"),
                sar: t(v, "sar")?,
                rotation: i(v, "rotation"),
                fps: t(v, "fps")?,
                pix_fmt: s(v, "pix_fmt"),
                bit_depth: i(v, "bit_depth"),
                primaries: s(v, "primaries"),
                transfer: s(v, "transfer"),
                matrix: s(v, "matrix"),
                full_range: v.get("full_range").and_then(Value::as_bool).unwrap_or(false),
                time_base: t(v, "time_base")?,
                field_order: s(v, "field_order"),
                codec_tag: s(v, "codec_tag"),
                extradata_hash: s(v, "extradata_hash"),
            }),
            _ => None,
        };
        let audio = match d.get("audio") {
            Some(a) if a.is_object() => Some(AudioInfo {
                codec: s(a, "codec"),
                profile: s(a, "profile"),
                sample_rate: i(a, "sample_rate"),
                channels: i(a, "channels"),
                layout: s(a, "layout"),
                time_base: t(a, "time_base")?,
                extradata_hash: s(a, "extradata_hash"),
            }),
            _ => None,
        };
        Ok(MediaInfo {
            container: s(d, "container"),
            duration: t(d, "duration")?,
            video,
            audio,
            keyframes: times(d.get("keyframes"))?,
            keyframe_dts: times(d.get("keyframe_dts"))?,
        })
    }

    pub fn to_json(&self) -> Value {
        let list = |v: &Option<Vec<R>>| match v {
            Some(k) => Value::Array(k.iter().map(|x| json!(time::to_string(x))).collect()),
            None => Value::Null,
        };
        json!({
            "container": self.container,
            "duration": time::to_string(&self.duration),
            "video": self.video.as_ref().map(|v| json!({
                "codec": v.codec, "profile": v.profile, "level": v.level, "width": v.width, "height": v.height,
                "sar": time::to_string(&v.sar), "rotation": v.rotation, "fps": time::to_string(&v.fps),
                "pix_fmt": v.pix_fmt, "bit_depth": v.bit_depth, "primaries": v.primaries, "transfer": v.transfer,
                "matrix": v.matrix, "full_range": v.full_range, "time_base": time::to_string(&v.time_base),
                "field_order": v.field_order, "codec_tag": v.codec_tag, "extradata_hash": v.extradata_hash,
            })),
            "audio": self.audio.as_ref().map(|a| json!({
                "codec": a.codec, "profile": a.profile, "sample_rate": a.sample_rate, "channels": a.channels,
                "layout": a.layout, "time_base": time::to_string(&a.time_base), "extradata_hash": a.extradata_hash,
            })),
            "keyframes": list(&self.keyframes),
            "keyframe_dts": list(&self.keyframe_dts),
        })
    }
}
