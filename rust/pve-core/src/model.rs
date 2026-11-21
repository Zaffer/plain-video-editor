//! Project model: pure data with exact rational time.
//!
//! Nothing here knows about ffmpeg, mpv, or the filesystem beyond an opaque
//! path string per media id.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::time::{self, R};

pub const FORMAT_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Crop rectangle in display pixels: upright, square-pixel, as the viewer sees the clip.
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Crop {
    Rect(Rect),
    /// Largest centred rectangle of this width/height ratio.
    Aspect(R),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub media: String,
    /// Seconds from the start of the media.
    pub start: R,
    pub end: R,
    pub crop: Option<Crop>,
    /// Shape a Rect crop is held to while editing; None = free.
    pub crop_aspect: Option<R>,
    /// Keep the clip's own sound.
    pub audio: bool,
    pub gain_db: f64,
}

impl Clip {
    pub fn duration(&self) -> R {
        self.end - self.start
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioItem {
    pub media: String,
    pub start: R,
    pub end: R,
    /// Position on the montage timeline.
    pub at: R,
    pub gain_db: f64,
}

impl AudioItem {
    pub fn duration(&self) -> R {
        self.end - self.start
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AudioLane {
    pub items: Vec<AudioItem>,
    pub gain_db: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Project {
    /// media id -> path
    pub media: BTreeMap<String, String>,
    pub clips: Vec<Clip>,
    pub lanes: Vec<AudioLane>,
    /// None = decided from the clips.
    pub output_size: Option<(i64, i64)>,
}

pub type Result<T> = std::result::Result<T, String>;

fn time_field(obj: &Value, key: &str, default: Option<R>) -> Result<R> {
    match obj.get(key) {
        None | Some(Value::Null) => default.ok_or_else(|| format!("missing {key:?}")),
        Some(Value::String(s)) => time::parse(s).ok_or_else(|| format!("not a time: {s:?}")),
        Some(Value::Number(n)) => time::parse(&n.to_string()).ok_or_else(|| format!("not a time: {n}")),
        Some(other) => Err(format!("not a time: {other}")),
    }
}

fn float_field(obj: &Value, key: &str) -> Result<f64> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(0.0),
        Some(v) => v.as_f64().ok_or_else(|| format!("{key:?} is not a number")),
    }
}

fn str_field(obj: &Value, key: &str) -> Result<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("missing {key:?}"))
}

fn int_field(obj: &Value, key: &str) -> Result<i64> {
    obj.get(key).and_then(Value::as_i64).ok_or_else(|| format!("{key:?} is not a whole number"))
}

fn ratio_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn crop_from(v: Option<&Value>) -> Result<Option<Crop>> {
    let Some(c) = v.filter(|c| c.as_object().is_some_and(|o| !o.is_empty())) else { return Ok(None) };
    if let Some(a) = c.get("aspect") {
        let ratio = ratio_text(a).and_then(|t| time::parse_ratio(&t)).ok_or_else(|| format!("not a ratio: {a}"))?;
        if ratio <= time::zero() {
            return Err("crop aspect must be positive".into());
        }
        return Ok(Some(Crop::Aspect(ratio)));
    }
    let rect = Rect { x: int_field(c, "x")?, y: int_field(c, "y")?, w: int_field(c, "w")?, h: int_field(c, "h")? };
    if rect.x < 0 || rect.y < 0 || rect.w <= 0 || rect.h <= 0 {
        return Err(format!("bad crop rectangle {c}"));
    }
    Ok(Some(Crop::Rect(rect)))
}

fn ratio_string(ratio: &R) -> String {
    format!("{}:{}", ratio.numer(), ratio.denom())
}

impl Project {
    pub fn validate(&self) -> Result<()> {
        if self.clips.is_empty() {
            return Err("project has no clips".into());
        }
        let mut spans: Vec<(String, &str, &R, &R)> = Vec::new();
        for (i, c) in self.clips.iter().enumerate() {
            spans.push((format!("clip {}", i + 1), &c.media, &c.start, &c.end));
        }
        for (li, lane) in self.lanes.iter().enumerate() {
            for (ii, it) in lane.items.iter().enumerate() {
                let what = format!("audio lane {} item {}", li + 1, ii + 1);
                if it.at < time::zero() {
                    return Err(format!("{what}: 'at' is negative"));
                }
                spans.push((what, &it.media, &it.start, &it.end));
            }
        }
        for (what, media, start, end) in spans {
            if !self.media.contains_key(media) {
                return Err(format!("{what}: unknown media {media:?}"));
            }
            if *start < time::zero() || end <= start {
                return Err(format!("{what}: needs 0 <= in < out"));
            }
        }
        Ok(())
    }

    pub fn from_json(d: &Value) -> Result<Project> {
        if d.get("version").and_then(Value::as_i64) != Some(FORMAT_VERSION) {
            return Err(format!("unsupported project version {}", d.get("version").unwrap_or(&Value::Null)));
        }
        let mut media = BTreeMap::new();
        for (k, v) in d.get("media").and_then(Value::as_object).ok_or("malformed project: no media")? {
            let path = match v {
                Value::String(s) => s.clone(),
                other => str_field(other, "path")?,
            };
            media.insert(k.clone(), path);
        }
        let mut clips = Vec::new();
        for c in d.get("clips").and_then(Value::as_array).ok_or("malformed project: no clips")? {
            let crop_aspect = match c.get("crop_aspect") {
                None | Some(Value::Null) => None,
                Some(v) => Some(ratio_text(v).and_then(|t| time::parse_ratio(&t)).ok_or_else(|| format!("not a ratio: {v}"))?),
            };
            clips.push(Clip {
                media: str_field(c, "media")?,
                start: time_field(c, "in", None)?,
                end: time_field(c, "out", None)?,
                crop: crop_from(c.get("crop"))?,
                crop_aspect,
                audio: c.get("audio").is_none_or(|v| v.as_bool().unwrap_or(!v.is_null())),
                gain_db: float_field(c, "gain_db")?,
            });
        }
        let mut lanes = Vec::new();
        if let Some(list) = d.get("audio_lanes").and_then(Value::as_array) {
            for lane in list {
                let mut items = Vec::new();
                if let Some(arr) = lane.get("items").and_then(Value::as_array) {
                    for i in arr {
                        items.push(AudioItem {
                            media: str_field(i, "media")?,
                            start: time_field(i, "in", None)?,
                            end: time_field(i, "out", None)?,
                            at: time_field(i, "at", Some(time::zero()))?,
                            gain_db: float_field(i, "gain_db")?,
                        });
                    }
                }
                lanes.push(AudioLane { items, gain_db: float_field(lane, "gain_db")? });
            }
        }
        let output_size = match d.get("output") {
            Some(out) if out.get("width").is_some() => Some((int_field(out, "width")?, int_field(out, "height")?)),
            _ => None,
        };
        let p = Project { media, clips, lanes, output_size };
        p.validate()?;
        Ok(p)
    }

    pub fn to_json(&self) -> Value {
        let media: Map<String, Value> = self.media.iter().map(|(k, v)| (k.clone(), json!({ "path": v }))).collect();
        let clips: Vec<Value> = self
            .clips
            .iter()
            .map(|c| {
                let mut o = Map::new();
                o.insert("media".into(), json!(c.media));
                o.insert("in".into(), json!(time::to_string(&c.start)));
                o.insert("out".into(), json!(time::to_string(&c.end)));
                match &c.crop {
                    Some(Crop::Aspect(a)) => {
                        o.insert("crop".into(), json!({ "aspect": ratio_string(a) }));
                    }
                    Some(Crop::Rect(r)) => {
                        o.insert("crop".into(), json!({ "x": r.x, "y": r.y, "w": r.w, "h": r.h }));
                    }
                    None => {}
                }
                if let Some(a) = &c.crop_aspect {
                    o.insert("crop_aspect".into(), json!(ratio_string(a)));
                }
                o.insert("audio".into(), json!(c.audio));
                o.insert("gain_db".into(), json!(c.gain_db));
                Value::Object(o)
            })
            .collect();
        let lanes: Vec<Value> = self
            .lanes
            .iter()
            .map(|lane| {
                let items: Vec<Value> = lane
                    .items
                    .iter()
                    .map(|i| {
                        json!({
                            "media": i.media, "in": time::to_string(&i.start), "out": time::to_string(&i.end),
                            "at": time::to_string(&i.at), "gain_db": i.gain_db,
                        })
                    })
                    .collect();
                json!({ "gain_db": lane.gain_db, "items": items })
            })
            .collect();
        let mut d = Map::new();
        d.insert("version".into(), json!(FORMAT_VERSION));
        d.insert("media".into(), Value::Object(media));
        d.insert("clips".into(), Value::Array(clips));
        d.insert("audio_lanes".into(), Value::Array(lanes));
        if let Some((w, h)) = self.output_size {
            d.insert("output".into(), json!({ "width": w, "height": h }));
        }
        Value::Object(d)
    }
}
