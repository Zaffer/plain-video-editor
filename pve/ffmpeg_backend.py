"""Native export backend: lower an ExportPlan to ffmpeg command lines and run them."""
from __future__ import annotations

import functools
import os
import signal
import subprocess
import tempfile
import threading
from dataclasses import dataclass
from fractions import Fraction
from pathlib import Path
from typing import Callable

from .planner import ExportPlan, Segment, VideoTarget

AUDIO_RATE = 48000
EDGE_FADE = Fraction(1, 100)  # short fade at each audio cut so joins do not click
SEEK_NUDGE = Fraction(1, 1000)  # seek just past a keyframe so rounding cannot land on the previous one
STOP_MARGIN = Fraction(1, 10000)  # stop just before the end keyframe's decode time

_ZMATRIX = {"bt709": "709", "bt601": "170m", "bt2020nc": "2020_ncl"}
_HDR_TRC = {"hlg": "arib-std-b67", "pq": "smpte2084"}


class ExportError(RuntimeError):
    pass


class ExportCancelled(ExportError):
    pass


@dataclass
class Step:
    argv: list[str]
    duration: Fraction  # output seconds this step produces, for progress
    list_file: tuple[str, str] | None = None  # (path, content) to write before running


def ts(t: Fraction) -> str:
    return f"{float(t):.6f}"


MAX_HW_DECODERS = 8  # more clips than this decode on the CPU, to bound GPU memory


@dataclass(frozen=True)
class Capabilities:
    """What this machine's ffmpeg can do. Everything defaults to the plain software path."""
    filters: frozenset[str] = frozenset()
    nvenc: bool = False  # NVIDIA encoders work here (H.264 and 10-bit HEVC)
    hw_decode: str | None = None  # an ffmpeg -hwaccel name that works here


def _ffmpeg(args: list[str]) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(["ffmpeg", "-hide_banner", *args], capture_output=True, text=True)
    except FileNotFoundError as e:
        raise ExportError("ffmpeg not found on PATH") from e


@functools.lru_cache(maxsize=1)
def detect() -> Capabilities:
    """Probe ffmpeg once: its filters, and whether the NVIDIA encoders and decoder really run."""
    out = _ffmpeg(["-filters"]).stdout
    filters = frozenset(p[1] for line in out.splitlines() if len(p := line.split()) >= 3 and "->" in p[2])
    test = ["-v", "error", "-f", "lavfi", "-i", "color=black:s=256x256:r=30:d=0.2"]
    nvenc = (
        _ffmpeg([*test, "-c:v", "h264_nvenc", "-f", "null", "-"]).returncode == 0
        and _ffmpeg([*test, "-pix_fmt", "p010le", "-c:v", "hevc_nvenc", "-profile:v", "main10", "-f", "null", "-"]).returncode == 0
    )
    hw = "cuda" if nvenc and "cuda" in _ffmpeg(["-hwaccels"]).stdout.split() else None
    return Capabilities(filters=filters, nvenc=nvenc, hw_decode=hw)


def uses_gpu_encoder(plan: ExportPlan, caps: Capabilities, encoder: str = "auto") -> bool:
    """encoder: "auto" (GPU for outputs larger than 1080p, where it saves real time), "x264" or "nvenc".

    Measured at equal quality on 1080p: the GPU encoder is 1.3-1.7x faster and about a third larger.
    """
    if plan.kind != "encode" or plan.draft or encoder == "x264":
        return False
    if encoder == "nvenc":
        if not caps.nvenc:
            raise ExportError("the NVIDIA encoder is not available on this machine")
        return True
    return caps.nvenc and plan.video.width * plan.video.height > 1920 * 1080


def build(plan: ExportPlan, paths: dict[str, str], out: str, caps: Capabilities, tmpdir: str = "",
          encoder: str = "auto") -> list[Step]:
    if plan.kind == "copy":
        return _build_copy(plan, paths, out, tmpdir)
    return [_build_encode(plan, paths, out, caps, uses_gpu_encoder(plan, caps, encoder))]


# ---------------------------------------------------------------- copy

_BASE = ["ffmpeg", "-hide_banner", "-nostdin", "-v", "error", "-y", "-progress", "pipe:1", "-nostats"]


def _container_flags(out: str, faststart: bool = True) -> list[str]:
    if Path(out).suffix.lower() not in (".mp4", ".mov", ".m4v"):
        return []
    # No timecode track: it would carry the source's start timecode, which is wrong after a cut,
    # and it is a third stream that players and later steps do not expect.
    return [*(["-movflags", "+faststart"] if faststart else []), "-write_tmcd", "0"]


def _copy_cut(seg: Segment, path: str, out: str, audio: bool) -> Step:
    a = [*_BASE]
    seek = seg.start + SEEK_NUDGE if seg.start > 0 else Fraction(0)
    if seg.start > 0:
        a += ["-ss", ts(seek)]
    a += ["-i", path]
    if seg.end_decode is not None:
        # ffmpeg ends a stream copy on decode time, so stop at the end keyframe's
        # decode time; using its display time would let it and a few later frames through.
        a += ["-t", ts(seg.end_decode - seek - STOP_MARGIN)]
    a += ["-map", "0:v:0"]
    if audio:
        a += ["-map", "0:a:0"]
    a += ["-c", "copy", "-map_metadata", "0"]
    if seg.start > 0:
        a += ["-avoid_negative_ts", "make_zero"]
    return Step(a + _container_flags(out) + [out], seg.duration)


def _build_copy(plan: ExportPlan, paths: dict[str, str], out: str, tmpdir: str) -> list[Step]:
    if len(plan.segments) == 1:
        seg = plan.segments[0]
        return [_copy_cut(seg, paths[seg.media], out, plan.copy_audio)]
    ext = Path(out).suffix
    steps, lines = [], []
    for i, seg in enumerate(plan.segments):
        part = os.path.join(tmpdir, f"part{i:03d}{ext}")
        steps.append(_copy_cut(seg, paths[seg.media], part, plan.copy_audio))
        lines.append("file '" + part.replace("'", "'\\''") + "'")
    listing = os.path.join(tmpdir, "parts.txt")
    join = [*_BASE, "-f", "concat", "-safe", "0", "-i", listing, "-map", "0:v:0",
            *(["-map", "0:a:0"] if plan.copy_audio else []), "-c", "copy", *_container_flags(out), out]
    steps.append(Step(join, plan.duration, list_file=(listing, "\n".join(lines) + "\n")))
    return steps


# ---------------------------------------------------------------- encode


def _video_chain(seg: Segment, tgt: VideoTarget, filters: set[str]) -> str:
    fw, fh = seg.fit
    c = seg.color
    f = ["setpts=PTS-STARTPTS"]
    if seg.crop:
        f.append(f"crop={seg.crop.w}:{seg.crop.h}:{seg.crop.x}:{seg.crop.y}")
    if tgt.color == "sdr" and c.kind != "sdr":
        # HDR clip into a standard-range output: tone-map.
        if "libplacebo" in filters:
            f.append(
                f"libplacebo=w={fw}:h={fh}:colorspace=bt709:color_primaries=bt709:color_trc=bt709"
                ":range=tv:format=yuv420p"
            )
        else:
            f += [
                f"zscale=w={fw}:h={fh}:filter=lanczos:t=linear:npl=100", "format=gbrpf32le", "zscale=p=bt709",
                "tonemap=hable:desat=0", "zscale=t=bt709:m=bt709:r=limited:d=error_diffusion",
            ]
    elif tgt.color == "sdr":
        z = (
            f"zscale=w={fw}:h={fh}:filter=lanczos:min={_ZMATRIX[c.matrix]}:m=709"
            f":rin={'full' if c.full_range else 'limited'}:r=limited"
        )
        f.append(z + (":d=error_diffusion" if c.bit_depth > 8 else ""))
    else:
        f.append(f"zscale=w={fw}:h={fh}:filter=lanczos")
    if (fw, fh) != (tgt.width, tgt.height):
        f.append(f"pad={tgt.width}:{tgt.height}:(ow-iw)/2:(oh-ih)/2")
    f += [f"fps={tgt.fps.numerator}/{tgt.fps.denominator}", f"format={_pix_fmt(tgt)}", "setsar=1"]
    if tgt.color == "sdr":
        f.append("setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv")
    return ",".join(f)


def _pix_fmt(tgt: VideoTarget) -> str:
    return "yuv420p" if tgt.color == "sdr" else "yuv420p10le"


def _video_codec(tgt: VideoTarget, draft: bool, gpu: bool) -> list[str]:
    gop = str(max(1, round(tgt.fps * 2)))
    sdr_tags = ["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"]
    hdr_tags = ["-color_primaries", "bt2020", "-color_trc", _HDR_TRC.get(tgt.color, ""), "-colorspace", "bt2020nc",
                "-color_range", "tv"]
    if gpu:
        # Constant quantiser, matched by measurement to the software settings below.
        common = ["-preset", "p5", "-tune", "hq", "-rc", "constqp", "-bf", "2", "-g", gop]
        if tgt.color == "sdr":
            return ["-c:v", "h264_nvenc", *common, "-qp", "20" if tgt.height >= 2160 else "18",
                    "-profile:v", "high", "-pix_fmt", "yuv420p", *sdr_tags]
        return ["-c:v", "hevc_nvenc", *common, "-qp", "20", "-profile:v", "main10", "-pix_fmt", "p010le",
                "-tag:v", "hvc1", *hdr_tags]
    if tgt.color == "sdr":
        quality = ["-preset", "ultrafast", "-crf", "28"] if draft else ["-preset", "medium", "-crf", "20" if tgt.height >= 2160 else "18"]
        return ["-c:v", "libx264", *quality, "-pix_fmt", "yuv420p", "-g", gop,
                "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"]
    return ["-c:v", "libx265", "-preset", "medium", "-crf", "20", "-pix_fmt", "yuv420p10le", "-tag:v", "hvc1",
            "-g", gop, "-x265-params", "log-level=error",
            "-color_primaries", "bt2020", "-color_trc", _HDR_TRC[tgt.color], "-colorspace", "bt2020nc",
            "-color_range", "tv"]


def _build_encode(plan: ExportPlan, paths: dict[str, str], out: str, caps: Capabilities, gpu: bool) -> Step:
    filters = caps.filters
    tgt = plan.video
    a = [*_BASE]
    needs_vulkan = tgt.color == "sdr" and "libplacebo" in filters and any(s.color.kind != "sdr" for s in plan.segments)
    if needs_vulkan:
        a += ["-init_hw_device", "vulkan"]
    graph = []
    n = 0
    # One input per use, each seeked on the input side: fast, and frame-accurate when re-encoding.
    # Decoding on the GPU gives the same frames sooner (checked: byte-identical exports);
    # ffmpeg falls back to the CPU per file if the GPU cannot decode it.
    hw = ["-hwaccel", caps.hw_decode] if caps.hw_decode and len(plan.segments) <= MAX_HW_DECODERS else []
    for seg in plan.segments:
        a += [*hw, "-ss", ts(seg.start), "-t", ts(seg.duration), "-an", "-i", paths[seg.media]]
        graph.append(f"[{n}:v:0]{_video_chain(seg, tgt, filters)}[v{n}]")
        n += 1
    nv = n
    if nv == 1:
        graph.append("[v0]null[v]")
    else:
        graph.append("".join(f"[v{i}]" for i in range(nv)) + f"concat=n={nv}:v=1:a=0[v]")

    for j, src in enumerate(plan.audio):
        dur = src.end - src.start
        a += ["-ss", ts(src.start), "-t", ts(dur), "-vn", "-i", paths[src.media]]
        fade = min(EDGE_FADE, dur / 2)
        f = [
            "asetpts=PTS-STARTPTS",
            f"aresample={AUDIO_RATE}:async=1",
            "aformat=sample_fmts=fltp:channel_layouts=stereo",
            f"afade=t=in:d={ts(fade)}",
            f"afade=t=out:st={ts(dur - fade)}:d={ts(fade)}",
        ]
        if src.gain_db:
            f.append(f"volume={src.gain_db:g}dB")
        if src.at > 0:
            f.append(f"adelay=delays={round(src.at * AUDIO_RATE)}S:all=1")
        graph.append(f"[{n}:a:0]{','.join(f)}[a{j}]")
        n += 1

    maps = ["-map", "[v]"]
    if plan.audio:
        na = len(plan.audio)
        tail = []
        if na > 1:
            # normalize=0: otherwise amix changes the volume whenever a source starts or ends.
            tail.append(f"amix=inputs={na}:normalize=0:dropout_transition=0")
        if na > 1 or any(s.gain_db > 0 for s in plan.audio):
            tail.append("alimiter=limit=0.89:level=disabled")
        tail += [f"apad=whole_dur={ts(plan.duration)}", f"atrim=duration={ts(plan.duration)}"]
        graph.append("".join(f"[a{j}]" for j in range(na)) + ",".join(tail) + "[a]")
        maps += ["-map", "[a]"]

    a += ["-filter_complex", ";".join(graph), *maps, *_video_codec(tgt, plan.draft, gpu), "-fps_mode", "cfr"]
    if plan.audio:
        a += ["-c:a", "aac", "-b:a", "128k" if plan.draft else "192k", "-ar", str(AUDIO_RATE)]
    a += _container_flags(out, faststart=not plan.draft)
    return Step(a + [out], plan.duration)


# ---------------------------------------------------------------- run


def run(plan: ExportPlan, paths: dict[str, str], out: str, on_progress: Callable[[float], None] | None = None,
        cancel: threading.Event | None = None, encoder: str = "auto") -> None:
    """Export to `out`. Writes to a temporary name and renames on success, so `out` is never half-written.

    Setting `cancel` (from another thread) stops the export and raises ExportCancelled.
    """
    out_path = Path(out)
    partial = out_path.with_name(f".{out_path.stem}.partial{out_path.suffix}")
    with tempfile.TemporaryDirectory(prefix=".pve-", dir=out_path.parent) as tmpdir:
        # A lossless copy needs no knowledge of encoders or filters, so skip the (0.4 s) probe of ffmpeg.
        caps = detect() if plan.kind == "encode" else Capabilities()
        steps = build(plan, paths, str(partial), caps, tmpdir, encoder)
        total = float(sum((s.duration for s in steps), Fraction(0))) or 1.0
        done = 0.0
        try:
            for step in steps:
                if step.list_file:
                    Path(step.list_file[0]).write_text(step.list_file[1])
                _run_step(step, lambda sec: on_progress and on_progress(min(1.0, (done + sec) / total)), cancel)
                done += float(step.duration)
            os.replace(partial, out_path)
        finally:
            partial.unlink(missing_ok=True)
    if on_progress:
        on_progress(1.0)


def _run_step(step: Step, on_seconds: Callable[[float], None], cancel: threading.Event | None = None) -> None:
    with tempfile.TemporaryFile(mode="w+") as err:
        proc = subprocess.Popen(step.argv, stdout=subprocess.PIPE, stderr=err, text=True)
        try:
            for line in proc.stdout:  # ffmpeg writes a progress block about twice a second
                if cancel is not None and cancel.is_set():
                    raise ExportCancelled("cancelled")
                key, _, val = line.strip().partition("=")
                if key == "out_time_us" and val.lstrip("-").isdigit():
                    on_seconds(max(0.0, int(val) / 1e6))
            code = proc.wait()
        except BaseException:
            proc.send_signal(signal.SIGINT)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
            raise
        if code != 0:
            err.seek(0)
            raise ExportError(err.read().strip()[-2000:] or f"ffmpeg exited with status {code}")
