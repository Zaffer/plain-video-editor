//! Native export backend: lower an ExportPlan to ffmpeg command lines and run them.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use pve_core::planner::{fmt_g, ExportPlan, Kind, Segment, VideoTarget};
use pve_core::time::{self, int, r, round_half_even, seconds, R};

pub const AUDIO_RATE: i128 = 48000;
/// More clips than this decode on the CPU, to bound GPU memory.
pub const MAX_HW_DECODERS: usize = 8;

/// Short fade at each audio cut so joins do not click.
fn edge_fade() -> R {
    r(1, 100)
}
/// Seek just past a keyframe so rounding cannot land on the previous one.
fn seek_nudge() -> R {
    r(1, 1000)
}
/// Stop just before the end keyframe's decode time.
fn stop_margin() -> R {
    r(1, 10000)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Cancelled => write!(f, "cancelled"),
            ExportError::Failed(m) => write!(f, "{m}"),
        }
    }
}

fn failed<T>(m: impl Into<String>) -> Result<T, ExportError> {
    Err(ExportError::Failed(m.into()))
}

/// What this machine's ffmpeg can do. Everything defaults to the plain software path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub filters: BTreeSet<String>,
    /// NVIDIA encoders work here (H.264 and 10-bit HEVC).
    pub nvenc: bool,
    /// An ffmpeg -hwaccel name that works here.
    pub hw_decode: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    /// GPU for outputs larger than 1080p, where it saves real time; x264 otherwise.
    Auto,
    X264,
    Nvenc,
}

impl Encoder {
    pub fn parse(s: &str) -> Option<Encoder> {
        match s {
            "auto" => Some(Encoder::Auto),
            "x264" => Some(Encoder::X264),
            "nvenc" => Some(Encoder::Nvenc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub argv: Vec<String>,
    /// Output seconds this step produces, for progress.
    pub duration: R,
    /// (path, content) to write before running.
    pub list_file: Option<(String, String)>,
}

fn ffmpeg(args: &[&str]) -> Result<std::process::Output, ExportError> {
    Command::new("ffmpeg").arg("-hide_banner").args(args).stdin(Stdio::null()).output().or_else(|_| failed("ffmpeg not found on PATH"))
}

/// Probe ffmpeg once: its filters, and whether the NVIDIA encoders and decoder really run.
pub fn detect() -> Result<&'static Capabilities, ExportError> {
    static CAPS: OnceLock<Capabilities> = OnceLock::new();
    if let Some(c) = CAPS.get() {
        return Ok(c);
    }
    let out = String::from_utf8_lossy(&ffmpeg(&["-filters"])?.stdout).into_owned();
    let filters: BTreeSet<String> = out
        .lines()
        .filter_map(|line| {
            let p: Vec<&str> = line.split_whitespace().collect();
            (p.len() >= 3 && p[2].contains("->")).then(|| p[1].to_owned())
        })
        .collect();
    let test = ["-v", "error", "-f", "lavfi", "-i", "color=black:s=256x256:r=30:d=0.2"];
    let ok = |extra: &[&str]| ffmpeg(&[&test[..], extra].concat()).map(|o| o.status.success()).unwrap_or(false);
    let nvenc = ok(&["-c:v", "h264_nvenc", "-f", "null", "-"])
        && ok(&["-pix_fmt", "p010le", "-c:v", "hevc_nvenc", "-profile:v", "main10", "-f", "null", "-"]);
    let hw = nvenc && String::from_utf8_lossy(&ffmpeg(&["-hwaccels"])?.stdout).split_whitespace().any(|w| w == "cuda");
    Ok(CAPS.get_or_init(|| Capabilities { filters, nvenc, hw_decode: hw.then(|| "cuda".to_owned()) }))
}

/// Measured at equal quality on 1080p: the GPU encoder is 1.3-1.7x faster and about a third larger.
pub fn uses_gpu_encoder(plan: &ExportPlan, caps: &Capabilities, encoder: Encoder) -> Result<bool, ExportError> {
    if plan.kind != Kind::Encode || plan.draft || encoder == Encoder::X264 {
        return Ok(false);
    }
    if encoder == Encoder::Nvenc {
        return if caps.nvenc { Ok(true) } else { failed("the NVIDIA encoder is not available on this machine") };
    }
    let v = plan.video.as_ref().expect("an encode plan has a target");
    Ok(caps.nvenc && v.width * v.height > 1920 * 1080)
}

pub type Paths = BTreeMap<String, String>;

pub fn build(plan: &ExportPlan, paths: &Paths, out: &str, caps: &Capabilities, tmpdir: &str, encoder: Encoder) -> Result<Vec<Step>, ExportError> {
    if plan.kind == Kind::Copy {
        return Ok(build_copy(plan, paths, out, tmpdir));
    }
    let gpu = uses_gpu_encoder(plan, caps, encoder)?;
    Ok(vec![build_encode(plan, paths, out, caps, gpu)])
}

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn base() -> Vec<String> {
    strs(&["ffmpeg", "-hide_banner", "-nostdin", "-v", "error", "-y", "-progress", "pipe:1", "-nostats"])
}

/// The extension with its dot, as written ("" if none).
fn suffix(path: &str) -> String {
    Path::new(path).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default()
}

fn container_flags(out: &str, faststart: bool) -> Vec<String> {
    if !matches!(suffix(out).to_lowercase().as_str(), ".mp4" | ".mov" | ".m4v") {
        return Vec::new();
    }
    // No timecode track: it would carry the source's start timecode, which is wrong after a cut,
    // and it is a third stream that players and later steps do not expect.
    let mut f = if faststart { strs(&["-movflags", "+faststart"]) } else { Vec::new() };
    f.extend(strs(&["-write_tmcd", "0"]));
    f
}

// ---------------------------------------------------------------- copy

fn copy_cut(seg: &Segment, path: &str, out: &str, audio: bool) -> Step {
    let mut a = base();
    let cut = seg.start > time::zero();
    let seek = if cut { seg.start + seek_nudge() } else { time::zero() };
    if cut {
        a.extend(["-ss".to_string(), seconds(&seek)]);
    }
    a.extend(["-i".to_string(), path.to_string()]);
    if let Some(end_decode) = &seg.end_decode {
        // ffmpeg ends a stream copy on decode time, so stop at the end keyframe's
        // decode time; using its display time would let it and a few later frames through.
        a.extend(["-t".to_string(), seconds(&(*end_decode - seek - stop_margin()))]);
    }
    a.extend(strs(&["-map", "0:v:0"]));
    if audio {
        a.extend(strs(&["-map", "0:a:0"]));
    }
    a.extend(strs(&["-c", "copy", "-map_metadata", "0"]));
    if cut {
        a.extend(strs(&["-avoid_negative_ts", "make_zero"]));
    }
    a.extend(container_flags(out, true));
    a.push(out.to_string());
    Step { argv: a, duration: seg.duration(), list_file: None }
}

fn join_path(dir: &str, name: &str) -> String {
    Path::new(dir).join(name).to_string_lossy().into_owned()
}

fn build_copy(plan: &ExportPlan, paths: &Paths, out: &str, tmpdir: &str) -> Vec<Step> {
    if plan.segments.len() == 1 {
        let seg = &plan.segments[0];
        return vec![copy_cut(seg, &paths[&seg.media], out, plan.copy_audio)];
    }
    let ext = suffix(out);
    let (mut steps, mut lines) = (Vec::new(), Vec::new());
    for (i, seg) in plan.segments.iter().enumerate() {
        let part = join_path(tmpdir, &format!("part{i:03}{ext}"));
        steps.push(copy_cut(seg, &paths[&seg.media], &part, plan.copy_audio));
        lines.push(format!("file '{}'", part.replace('\'', "'\\''")));
    }
    let listing = join_path(tmpdir, "parts.txt");
    let mut join = base();
    join.extend(strs(&["-f", "concat", "-safe", "0", "-i", &listing, "-map", "0:v:0"]));
    if plan.copy_audio {
        join.extend(strs(&["-map", "0:a:0"]));
    }
    join.extend(strs(&["-c", "copy"]));
    join.extend(container_flags(out, true));
    join.push(out.to_string());
    steps.push(Step { argv: join, duration: plan.duration, list_file: Some((listing, lines.join("\n") + "\n")) });
    steps
}

// ---------------------------------------------------------------- encode

fn pix_fmt(tgt: &VideoTarget) -> &'static str {
    if tgt.color == "sdr" {
        "yuv420p"
    } else {
        "yuv420p10le"
    }
}

fn hdr_trc(color: &str) -> &'static str {
    match color {
        "hlg" => "arib-std-b67",
        "pq" => "smpte2084",
        _ => "",
    }
}

fn video_chain(seg: &Segment, tgt: &VideoTarget, filters: &BTreeSet<String>) -> String {
    let (fw, fh) = seg.fit.expect("an encode segment has a fit");
    let c = seg.color.as_ref().expect("an encode segment has a colour");
    let mut f = vec!["setpts=PTS-STARTPTS".to_string()];
    if let Some(crop) = &seg.crop {
        f.push(format!("crop={}:{}:{}:{}", crop.w, crop.h, crop.x, crop.y));
    }
    if tgt.color == "sdr" && c.kind != "sdr" {
        // HDR clip into a standard-range output: tone-map.
        if filters.contains("libplacebo") {
            f.push(format!(
                "libplacebo=w={fw}:h={fh}:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p"
            ));
        } else {
            f.push(format!("zscale=w={fw}:h={fh}:filter=lanczos:t=linear:npl=100"));
            f.extend(strs(&["format=gbrpf32le", "zscale=p=bt709", "tonemap=hable:desat=0",
                            "zscale=t=bt709:m=bt709:r=limited:d=error_diffusion"]));
        }
    } else if tgt.color == "sdr" {
        let zmatrix = match c.matrix {
            "bt601" => "170m",
            "bt2020nc" => "2020_ncl",
            _ => "709",
        };
        let range = if c.full_range { "full" } else { "limited" };
        let dither = if c.bit_depth > 8 { ":d=error_diffusion" } else { "" };
        f.push(format!("zscale=w={fw}:h={fh}:filter=lanczos:min={zmatrix}:m=709:rin={range}:r=limited{dither}"));
    } else {
        f.push(format!("zscale=w={fw}:h={fh}:filter=lanczos"));
    }
    if (fw, fh) != (tgt.width, tgt.height) {
        f.push(format!("pad={}:{}:(ow-iw)/2:(oh-ih)/2", tgt.width, tgt.height));
    }
    f.push(format!("fps={}/{}", tgt.fps.numer(), tgt.fps.denom()));
    f.push(format!("format={}", pix_fmt(tgt)));
    f.push("setsar=1".into());
    if tgt.color == "sdr" {
        f.push("setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv".into());
    }
    f.join(",")
}

fn video_codec(tgt: &VideoTarget, draft: bool, gpu: bool) -> Vec<String> {
    let gop = round_half_even(&(tgt.fps * int(2))).max(1).to_string();
    let sdr_tags = strs(&["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"]);
    let hdr_tags = strs(&["-color_primaries", "bt2020", "-color_trc", hdr_trc(tgt.color), "-colorspace", "bt2020nc", "-color_range", "tv"]);
    let big = tgt.height >= 2160;
    let mut a: Vec<String>;
    if gpu {
        // Constant quantiser, matched by measurement to the software settings below.
        let common = strs(&["-preset", "p5", "-tune", "hq", "-rc", "constqp", "-bf", "2", "-g", &gop]);
        if tgt.color == "sdr" {
            a = strs(&["-c:v", "h264_nvenc"]);
            a.extend(common);
            a.extend(strs(&["-qp", if big { "20" } else { "18" }, "-profile:v", "high", "-pix_fmt", "yuv420p"]));
            a.extend(sdr_tags);
        } else {
            a = strs(&["-c:v", "hevc_nvenc"]);
            a.extend(common);
            a.extend(strs(&["-qp", "20", "-profile:v", "main10", "-pix_fmt", "p010le", "-tag:v", "hvc1"]));
            a.extend(hdr_tags);
        }
        return a;
    }
    if tgt.color == "sdr" {
        a = strs(&["-c:v", "libx264"]);
        if draft {
            a.extend(strs(&["-preset", "ultrafast", "-crf", "28"]));
        } else {
            a.extend(strs(&["-preset", "medium", "-crf", if big { "20" } else { "18" }]));
        }
        a.extend(strs(&["-pix_fmt", "yuv420p", "-g", &gop]));
        a.extend(sdr_tags);
        return a;
    }
    a = strs(&["-c:v", "libx265", "-preset", "medium", "-crf", "20", "-pix_fmt", "yuv420p10le", "-tag:v", "hvc1",
               "-g", &gop, "-x265-params", "log-level=error"]);
    a.extend(hdr_tags);
    a
}

fn build_encode(plan: &ExportPlan, paths: &Paths, out: &str, caps: &Capabilities, gpu: bool) -> Step {
    let tgt = plan.video.as_ref().expect("an encode plan has a target");
    let mut a = base();
    let tone_mapped = plan.segments.iter().any(|s| s.color.as_ref().is_some_and(|c| c.kind != "sdr"));
    if tgt.color == "sdr" && caps.filters.contains("libplacebo") && tone_mapped {
        a.extend(strs(&["-init_hw_device", "vulkan"]));
    }
    let mut graph: Vec<String> = Vec::new();
    let mut n = 0usize;
    // One input per use, each seeked on the input side: fast, and frame-accurate when re-encoding.
    // Decoding on the GPU gives the same frames sooner (checked: byte-identical exports);
    // ffmpeg falls back to the CPU per file if the GPU cannot decode it.
    let hw = caps.hw_decode.as_deref().filter(|_| plan.segments.len() <= MAX_HW_DECODERS);
    for seg in &plan.segments {
        if let Some(hw) = hw {
            a.extend(strs(&["-hwaccel", hw]));
        }
        a.extend(["-ss".into(), seconds(&seg.start), "-t".into(), seconds(&seg.duration()), "-an".into(), "-i".into(), paths[&seg.media].clone()]);
        graph.push(format!("[{n}:v:0]{}[v{n}]", video_chain(seg, tgt, &caps.filters)));
        n += 1;
    }
    let nv = n;
    if nv == 1 {
        graph.push("[v0]null[v]".into());
    } else {
        let ins: String = (0..nv).map(|i| format!("[v{i}]")).collect();
        graph.push(format!("{ins}concat=n={nv}:v=1:a=0[v]"));
    }

    for (j, src) in plan.audio.iter().enumerate() {
        let dur = src.end - src.start;
        a.extend(["-ss".into(), seconds(&src.start), "-t".into(), seconds(&dur), "-vn".into(), "-i".into(), paths[&src.media].clone()]);
        let fade = edge_fade().min(dur / int(2));
        let mut f = vec![
            "asetpts=PTS-STARTPTS".to_string(),
            format!("aresample={AUDIO_RATE}:async=1"),
            "aformat=sample_fmts=fltp:channel_layouts=stereo".to_string(),
            format!("afade=t=in:d={}", seconds(&fade)),
            format!("afade=t=out:st={}:d={}", seconds(&(dur - fade)), seconds(&fade)),
        ];
        if src.gain_db != 0.0 {
            f.push(format!("volume={}dB", fmt_g(src.gain_db)));
        }
        if src.at > time::zero() {
            f.push(format!("adelay=delays={}S:all=1", round_half_even(&(src.at * int(AUDIO_RATE)))));
        }
        graph.push(format!("[{n}:a:0]{}[a{j}]", f.join(",")));
        n += 1;
    }

    let mut maps = strs(&["-map", "[v]"]);
    if !plan.audio.is_empty() {
        let na = plan.audio.len();
        let mut tail = Vec::new();
        if na > 1 {
            // normalize=0: otherwise amix changes the volume whenever a source starts or ends.
            tail.push(format!("amix=inputs={na}:normalize=0:dropout_transition=0"));
        }
        if na > 1 || plan.audio.iter().any(|s| s.gain_db > 0.0) {
            tail.push("alimiter=limit=0.89:level=disabled".into());
        }
        tail.push(format!("apad=whole_dur={}", seconds(&plan.duration)));
        tail.push(format!("atrim=duration={}", seconds(&plan.duration)));
        let ins: String = (0..na).map(|j| format!("[a{j}]")).collect();
        graph.push(format!("{ins}{}[a]", tail.join(",")));
        maps.extend(strs(&["-map", "[a]"]));
    }

    a.extend(["-filter_complex".to_string(), graph.join(";")]);
    a.extend(maps);
    a.extend(video_codec(tgt, plan.draft, gpu));
    a.extend(strs(&["-fps_mode", "cfr"]));
    if !plan.audio.is_empty() {
        a.extend(strs(&["-c:a", "aac", "-b:a", if plan.draft { "128k" } else { "192k" }, "-ar", &AUDIO_RATE.to_string()]));
    }
    a.extend(container_flags(out, !plan.draft));
    a.push(out.to_string());
    Step { argv: a, duration: plan.duration, list_file: None }
}

// ---------------------------------------------------------------- run

/// Removes a directory and its contents when dropped.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Removes a file when dropped (a no-op once it has been renamed away).
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Export to `out`. Writes to a temporary name and renames on success, so `out` is never half-written.
///
/// Setting `cancel` (from another thread) stops the export and returns `ExportError::Cancelled`.
pub fn run(
    plan: &ExportPlan,
    paths: &Paths,
    out: &str,
    mut on_progress: impl FnMut(f64),
    cancel: &AtomicBool,
    encoder: Encoder,
) -> Result<(), ExportError> {
    let out_path = Path::new(out);
    let dir = out_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stem = out_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let partial = Partial(dir.join(format!(".{stem}.partial{}", suffix(out))));
    let tmp = TempDir(dir.join(format!(".pve-{}", std::process::id())));
    std::fs::create_dir_all(&tmp.0).or_else(|e| failed(format!("cannot write to {}: {e}", dir.display())))?;

    // A lossless copy needs no knowledge of encoders or filters, so skip the (0.4 s) probe of ffmpeg.
    let plain = Capabilities::default();
    let caps = if plan.kind == Kind::Encode { detect()? } else { &plain };
    let steps = build(plan, paths, &partial.0.to_string_lossy(), caps, &tmp.0.to_string_lossy(), encoder)?;
    let total = time::to_f64(&steps.iter().fold(time::zero(), |a, s| a + s.duration)).max(1e-9);
    let mut done = 0.0;
    for step in &steps {
        if let Some((path, content)) = &step.list_file {
            std::fs::write(path, content).or_else(|e| failed(format!("cannot write {path}: {e}")))?;
        }
        run_step(step, |sec| on_progress(((done + sec) / total).min(1.0)), cancel)?;
        done += time::to_f64(&step.duration);
    }
    std::fs::rename(&partial.0, out_path).or_else(|e| failed(format!("cannot write {out}: {e}")))?;
    on_progress(1.0);
    Ok(())
}

fn run_step(step: &Step, mut on_seconds: impl FnMut(f64), cancel: &AtomicBool) -> Result<(), ExportError> {
    let mut child = Command::new(&step.argv[0])
        .args(&step.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .or_else(|_| failed("ffmpeg not found on PATH"))?;
    // Drain stderr on its own thread so a chatty ffmpeg can never block on a full pipe.
    let mut stderr = child.stderr.take().expect("piped");
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    // ffmpeg writes a progress block about twice a second.
    for line in BufReader::new(child.stdout.take().expect("piped")).lines() {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ExportError::Cancelled);
        }
        let Ok(line) = line else { break };
        if let Some(us) = line.trim().strip_prefix("out_time_us=").and_then(|v| v.parse::<i64>().ok()) {
            on_seconds((us as f64 / 1e6).max(0.0));
        }
    }
    let status = child.wait().or_else(|e| failed(e.to_string()))?;
    let errors = errors.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        return Err(ExportError::Cancelled);
    }
    if !status.success() {
        let text = errors.trim();
        let tail: String = text.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
        return failed(if tail.is_empty() { format!("ffmpeg stopped with {status}") } else { tail });
    }
    Ok(())
}
