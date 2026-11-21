//! Native backend: describe a media file with ffprobe.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use pve_core::media::{AudioInfo, MediaInfo, VideoInfo};
use pve_core::time::{self, int, R};
use serde_json::Value;

fn ffprobe(args: &[&str]) -> Result<String, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "ffprobe not found on PATH".to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(if err.is_empty() { format!("ffprobe failed on {}", args.last().unwrap_or(&"")) } else { err });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_owned()
}

/// A fraction field such as "30000/1001" or "9:16"; None if missing or not a number.
fn frac(v: &Value, k: &str) -> Option<R> {
    match v.get(k)? {
        Value::String(s) => time::parse_ratio(s),
        Value::Number(n) => time::parse(&n.to_string()),
        _ => None,
    }
}

fn whole(v: &Value, k: &str) -> Option<i64> {
    match v.get(k)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn tag(v: &Value, k: &str) -> String {
    match v.get(k).and_then(Value::as_str) {
        None | Some("unknown") | Some("unspecified") => String::new(),
        Some(s) => s.to_owned(),
    }
}

fn bit_depth(st: &Value) -> i64 {
    if let Some(b) = whole(st, "bits_per_raw_sample").filter(|b| *b > 0) {
        return b;
    }
    // yuv420p10le, p010le, gbrp12be ... : the two digits before an optional le/be
    let fmt = text(st, "pix_fmt");
    let body = fmt.strip_suffix("le").or_else(|| fmt.strip_suffix("be")).unwrap_or(&fmt);
    let digits: String = body.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
    let after_p = body[..body.len() - digits.len()].ends_with('p');
    match digits.trim_start_matches('0').parse::<i64>() {
        Ok(n) if after_p && digits.len() >= 2 && n >= 9 => n,
        _ => 8,
    }
}

/// Describe a file. Sound-only files are accepted (video is then None).
/// `keyframes` also indexes the keyframes, which reads the whole file's packet table.
pub fn probe(path: &str, keyframes: bool) -> Result<MediaInfo, String> {
    let raw = ffprobe(&["-show_format", "-show_streams", "-show_data_hash", "sha256", "-of", "json", path])?;
    let d: Value = serde_json::from_str(&raw).map_err(|e| format!("{path}: unreadable ffprobe output: {e}"))?;
    let empty = Vec::new();
    let streams = d.get("streams").and_then(Value::as_array).unwrap_or(&empty);
    let kind = |s: &Value, k: &str| s.get("codec_type").and_then(Value::as_str) == Some(k);
    let vs = streams.iter().find(|s| kind(s, "video") && s.pointer("/disposition/attached_pic").and_then(Value::as_i64).unwrap_or(0) == 0);
    let au = streams.iter().find(|s| kind(s, "audio"));
    if vs.is_none() && au.is_none() {
        return Err(format!("{path}: no video or sound found"));
    }
    let fmt = d.get("format").cloned().unwrap_or(Value::Null);
    // Exact rational times from integer timestamps; ffprobe's decimal strings are rounded.
    let file_start = streams
        .iter()
        .filter(|s| kind(s, "video") || kind(s, "audio"))
        .filter_map(|s| Some(int(whole(s, "start_pts")? as i128) * frac(s, "time_base").filter(|t| *t > time::zero())?))
        .min()
        .or_else(|| frac(&fmt, "start_time"))
        .unwrap_or_else(time::zero);

    let audio = au.map(|a| AudioInfo {
        codec: text(a, "codec_name"),
        profile: text(a, "profile"),
        sample_rate: whole(a, "sample_rate").unwrap_or(0),
        channels: whole(a, "channels").unwrap_or(0),
        layout: text(a, "channel_layout"),
        time_base: frac(a, "time_base").unwrap_or_else(|| time::r(1, 48000)),
        extradata_hash: text(a, "extradata_hash"),
    });
    let format_duration = frac(&fmt, "duration").unwrap_or_else(time::zero);
    let container = text(&fmt, "format_name");
    let Some(vs) = vs else {
        return Ok(MediaInfo { container, duration: format_duration, video: None, audio, keyframes: None, keyframe_dts: None });
    };

    let v_tb = frac(vs, "time_base").unwrap_or_else(|| time::r(1, 90000));
    let duration = whole(vs, "duration_ts").map(|ts| int(ts as i128) * v_tb).unwrap_or(format_duration);
    let rotation = vs
        .get("side_data_list")
        .and_then(Value::as_array)
        .and_then(|l| l.iter().filter_map(|sd| sd.get("rotation")).next_back())
        .and_then(|r| r.as_f64().or_else(|| r.as_str()?.parse().ok()))
        .map_or(0, |deg| (deg.round() as i64).rem_euclid(360));
    let fps = frac(vs, "avg_frame_rate")
        .filter(|f| *f > time::zero())
        .or_else(|| frac(vs, "r_frame_rate").filter(|f| *f > time::zero()))
        .unwrap_or_else(|| int(30));
    let pix_fmt = text(vs, "pix_fmt");
    let video = VideoInfo {
        codec: text(vs, "codec_name"),
        profile: text(vs, "profile"),
        level: whole(vs, "level").unwrap_or(0),
        width: whole(vs, "width").unwrap_or(0),
        height: whole(vs, "height").unwrap_or(0),
        sar: frac(vs, "sample_aspect_ratio").filter(|s| *s > time::zero()).unwrap_or_else(|| int(1)),
        rotation,
        fps,
        bit_depth: bit_depth(vs),
        primaries: tag(vs, "color_primaries"),
        transfer: tag(vs, "color_transfer"),
        matrix: tag(vs, "color_space"),
        full_range: vs.get("color_range").and_then(Value::as_str) == Some("pc") || pix_fmt.starts_with("yuvj"),
        pix_fmt,
        time_base: v_tb,
        field_order: text(vs, "field_order"),
        codec_tag: text(vs, "codec_tag_string"),
        extradata_hash: text(vs, "extradata_hash"),
    };

    let (mut kf, mut kd) = (None, None);
    if keyframes {
        // Packet flags only: demuxes without decoding. Times are rebased to the
        // file's first timestamp, which is also what ffmpeg's -ss counts from.
        let index = whole(vs, "index").unwrap_or(0).to_string();
        let out = ffprobe(&["-select_streams", &index, "-show_entries", "packet=pts,dts,flags", "-of", "csv=p=0", path])?;
        let mut map: BTreeMap<R, R> = BTreeMap::new();
        for line in out.lines() {
            let mut f = line.split(',');
            let (pts, dts, flags) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
            if !flags.contains('K') {
                continue;
            }
            let Ok(pts) = pts.parse::<i64>() else { continue };
            let p = int(pts as i128) * v_tb - file_start;
            let dts = dts.parse::<i64>().map(|d| int(d as i128) * v_tb - file_start).unwrap_or(p);
            map.insert(p.max(time::zero()), dts);
        }
        kd = Some(map.values().copied().collect());
        kf = Some(map.into_keys().collect());
    }

    Ok(MediaInfo { container, duration, video: Some(video), audio, keyframes: kf, keyframe_dts: kd })
}
