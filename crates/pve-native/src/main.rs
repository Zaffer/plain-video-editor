//! pve: command-line front end to the core.
//!
//!   pve probe FILE
//!   pve plan PROJECT [--mode auto|copy|encode] [--draft] [--encoder auto|x264|nvenc] [--json] [--commands]
//!   pve export PROJECT -o OUT [--mode ...] [--draft] [--encoder ...] [--force]

use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use pve_core::planner::{plan, ExportPlan, Infos, Kind, Mode};
use pve_core::session::Session;
use pve_core::time::to_f64;
use pve_native::ffmpeg::{self, Encoder, ExportError, Paths};
use pve_native::probe::probe;

const USAGE: &str = "\
Plain video editor: command line.

  pve probe FILE                 show what the planner knows about a file
  pve plan PROJECT               show what an export would do, and why
  pve export PROJECT -o OUT      export a project

Options for plan and export:
  --mode auto|copy|encode   auto: lossless when possible; copy: lossless or fail; encode: always exact
  --draft                   fast low-resolution render for previewing
  --encoder auto|x264|nvenc auto: NVIDIA GPU encoder for outputs larger than 1080p when available
  --json, --commands        (plan) print the plan as JSON, print the ffmpeg commands
  -o, --output OUT, --force (export) where to write, and whether to overwrite";

fn t(x: &pve_core::time::R) -> String {
    format!("{:.3}", to_f64(x))
}

fn g(x: f64) -> String {
    pve_core::planner::fmt_g(x)
}

fn absolute(path: &str) -> Result<String, String> {
    std::path::absolute(path).map(|p| p.to_string_lossy().into_owned()).map_err(|e| format!("{path}: {e}"))
}

fn load(project_file: &str) -> Result<(pve_core::model::Project, Paths, Infos), String> {
    let file = absolute(project_file)?;
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{project_file}: {e}"))?;
    let json = serde_json::from_str(&text).map_err(|e| format!("{project_file}: {e}"))?;
    let project = Session::read(&file, &json)?;
    let used: BTreeSet<&String> =
        project.clips.iter().map(|c| &c.media).chain(project.lanes.iter().flat_map(|l| l.items.iter().map(|i| &i.media))).collect();
    let mut infos = Infos::new();
    for id in used {
        infos.insert(id.clone(), probe(&project.media[id], true)?);
    }
    let paths = project.media.clone();
    Ok((project, paths, infos))
}

fn describe(p: &ExportPlan) -> String {
    let mut out = Vec::new();
    if p.kind == Kind::Copy {
        out.push(format!("Lossless copy, {} s. Cuts are moved outwards to keyframes:", t(&p.duration)));
        for (i, s) in p.segments.iter().enumerate() {
            let mut line = format!("  {}. {}  {} -> {}  ({} s)", i + 1, s.media, t(&s.start), t(&s.end), t(&s.duration()));
            if (s.start, s.end) != (s.requested_start, s.requested_end) {
                line += &format!("   asked for {} -> {}", t(&s.requested_start), t(&s.requested_end));
            }
            out.push(line);
        }
        out.push(format!("  sound: {}", if p.copy_audio { "copied" } else { "none" }));
    } else {
        let v = p.video.as_ref().expect("an encode plan has a target");
        let label = match v.color {
            "hlg" => "HDR (HLG), HEVC 10-bit",
            "pq" => "HDR (PQ), HEVC 10-bit",
            _ => "standard range, H.264",
        };
        out.push(format!(
            "{}, {} s: {}x{} at {} fps, {label}",
            if p.draft { "Draft re-encode" } else { "Re-encode" }, t(&p.duration), v.width, v.height, g(to_f64(&v.fps))
        ));
        if !p.reasons.is_empty() {
            out.push(format!("  not lossless because: {}", p.reasons.join("; ")));
        }
        for (i, s) in p.segments.iter().enumerate() {
            let mut notes = Vec::new();
            if let Some(c) = &s.crop {
                notes.push(format!("crop {}x{}+{}+{}", c.w, c.h, c.x, c.y));
            }
            if let Some((fw, fh)) = s.fit.filter(|f| *f != (v.width, v.height)) {
                notes.push(format!("fitted to {fw}x{fh} with bars"));
            }
            if let Some(c) = s.color.as_ref().filter(|c| c.kind != v.color) {
                notes.push(format!("{} tone-mapped to standard range", c.kind.to_uppercase()));
            }
            out.push(format!("  {}. {}  {} -> {}  {}", i + 1, s.media, t(&s.start), t(&s.end), notes.join(", ")));
        }
        if p.audio.is_empty() {
            out.push("  sound: none".into());
        } else {
            out.push("  sound:".into());
            for a in &p.audio {
                let gain = if a.gain_db != 0.0 { format!(", {}{} dB", if a.gain_db > 0.0 { "+" } else { "" }, g(a.gain_db)) } else { String::new() };
                out.push(format!("     {}  {} -> {}  at {}{gain}", a.media, t(&a.start), t(&a.end), t(&a.at)));
            }
        }
    }
    out.extend(p.warnings.iter().map(|w| format!("  warning: {w}")));
    out.join("\n")
}

/// POSIX-shell quoting, for printing commands a person can paste.
fn quote(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !arg.is_empty() && arg.chars().all(safe) {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

struct Args {
    cmd: String,
    target: String,
    mode: Mode,
    draft: bool,
    encoder: Encoder,
    json: bool,
    commands: bool,
    output: Option<String>,
    force: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args { cmd: String::new(), target: String::new(), mode: Mode::Auto, draft: false, encoder: Encoder::Auto, json: false, commands: false, output: None, force: false };
    let mut positional = Vec::new();
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--mode" => a.mode = Mode::parse(&value("--mode")?).ok_or("--mode must be auto, copy or encode")?,
            "--encoder" => a.encoder = Encoder::parse(&value("--encoder")?).ok_or("--encoder must be auto, x264 or nvenc")?,
            "-o" | "--output" => a.output = Some(value("--output")?),
            "--draft" => a.draft = true,
            "--json" => a.json = true,
            "--commands" => a.commands = true,
            "--force" => a.force = true,
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s}")),
            _ => positional.push(arg.clone()),
        }
    }
    if positional.len() != 2 || !matches!(positional[0].as_str(), "probe" | "plan" | "export") {
        return Err(USAGE.to_string());
    }
    a.cmd = positional[0].clone();
    a.target = positional[1].clone();
    Ok(a)
}

fn run(args: &Args, cancel: &Arc<AtomicBool>) -> Result<u8, String> {
    if args.cmd == "probe" {
        println!("{}", serde_json::to_string_pretty(&probe(&args.target, true)?.to_json()).unwrap());
        return Ok(0);
    }
    let (project, paths, infos) = load(&args.target)?;
    let p = plan(&project, &infos, args.mode, args.draft)?;
    // Probing ffmpeg's encoders takes about 0.4 s, so only do it when the answer is needed.
    let caps = || ffmpeg::detect().map_err(|e| e.to_string());

    if args.cmd == "plan" {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&p.to_json()).unwrap());
        } else {
            println!("{}", describe(&p));
        }
        if args.commands {
            for step in ffmpeg::build(&p, &paths, "OUTPUT.mp4", caps()?, "TMP", args.encoder).map_err(|e| e.to_string())? {
                println!("\n{}", step.argv.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" "));
            }
        }
        return Ok(0);
    }

    let output = args.output.as_deref().ok_or("export needs -o OUT")?;
    let out = absolute(output)?;
    if paths.values().any(|p| *p == out) {
        eprintln!("error: the output would overwrite a source file");
        return Ok(2);
    }
    if Path::new(&out).exists() && !args.force {
        eprintln!("error: {out} exists (use --force to overwrite)");
        return Ok(2);
    }
    println!("{}", describe(&p));
    if p.kind == Kind::Encode {
        let gpu = ffmpeg::uses_gpu_encoder(&p, caps()?, args.encoder).map_err(|e| e.to_string())?;
        println!("  encoder: {}", if gpu { "NVIDIA GPU" } else { "x264" });
    }
    let started = Instant::now();
    let tty = std::io::stderr().is_terminal();
    let progress = |f: f64| {
        if tty {
            eprint!("\r  {:5.1}%", f * 100.0);
            let _ = std::io::stderr().flush();
        }
    };
    match ffmpeg::run(&p, &paths, &out, progress, cancel, args.encoder) {
        Ok(()) => {
            if tty {
                eprint!("\r");
            }
            println!("wrote {out} in {:.1} s", started.elapsed().as_secs_f64());
            Ok(0)
        }
        Err(ExportError::Cancelled) => {
            eprintln!("\ncancelled");
            Ok(130)
        }
        Err(ExportError::Failed(m)) => Err(m),
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::from(if argv.is_empty() { 2 } else { 0 });
    }
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    // Ctrl-C asks the export to stop, so the half-written file is cleaned up.
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let _ = ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed));
    match run(&args, &cancel) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
