"""Native backend: fill MediaInfo from ffprobe."""
from __future__ import annotations

import json
import re
import subprocess
from fractions import Fraction

from .media import AudioInfo, MediaInfo, VideoInfo


class ProbeError(RuntimeError):
    pass


def _ffprobe(args: list[str]) -> str:
    try:
        r = subprocess.run(["ffprobe", "-v", "error", *args], capture_output=True, text=True)
    except FileNotFoundError as e:
        raise ProbeError("ffprobe not found on PATH") from e
    if r.returncode != 0:
        raise ProbeError(r.stderr.strip() or f"ffprobe failed on {args[-1]}")
    return r.stdout


def _frac(s, default=Fraction(0)) -> Fraction:
    try:
        return Fraction(str(s).replace(":", "/"))
    except (ValueError, ZeroDivisionError):
        return default


def _tag(s) -> str:
    return "" if s in (None, "unknown", "unspecified") else s


def _bit_depth(st: dict) -> int:
    if str(st.get("bits_per_raw_sample", "")).isdigit() and int(st["bits_per_raw_sample"]) > 0:
        return int(st["bits_per_raw_sample"])
    m = re.search(r"p0?(\d\d)(le|be)?$", st.get("pix_fmt", ""))
    return int(m.group(1)) if m else 8


def probe(path: str, keyframes: bool = True) -> MediaInfo:
    """Describe a file. Sound-only files are accepted (video is then None)."""
    d = json.loads(_ffprobe(["-show_format", "-show_streams", "-show_data_hash", "sha256", "-of", "json", path]))
    streams = d.get("streams", [])
    vs = next(
        (s for s in streams if s.get("codec_type") == "video" and not s.get("disposition", {}).get("attached_pic")),
        None,
    )
    as_ = next((s for s in streams if s.get("codec_type") == "audio"), None)
    if vs is None and as_ is None:
        raise ProbeError(f"{path}: no video or sound found")
    fmt = d["format"]
    # Exact rational times from integer timestamps; ffprobe's decimal strings are rounded.
    starts = [
        Fraction(int(s["start_pts"])) * _frac(s["time_base"])
        for s in streams
        if s.get("codec_type") in ("video", "audio") and "start_pts" in s and _frac(s.get("time_base")) > 0
    ]
    file_start = min(starts) if starts else _frac(fmt.get("start_time", 0))

    audio = None
    if as_ is not None:
        audio = AudioInfo(
            codec=as_["codec_name"],
            profile=as_.get("profile", ""),
            sample_rate=int(as_.get("sample_rate", 0)),
            channels=int(as_.get("channels", 0)),
            layout=as_.get("channel_layout", ""),
            time_base=_frac(as_.get("time_base"), Fraction(1, 48000)),
            extradata_hash=as_.get("extradata_hash", ""),
        )
    if vs is None:
        return MediaInfo(container=fmt.get("format_name", ""), duration=_frac(fmt.get("duration", 0)),
                         video=None, audio=audio)

    v_tb = _frac(vs.get("time_base"), Fraction(1, 90000))
    duration = Fraction(int(vs["duration_ts"])) * v_tb if "duration_ts" in vs else _frac(fmt.get("duration", 0))

    rotation = 0
    for sd in vs.get("side_data_list", []):
        if "rotation" in sd:
            rotation = round(float(sd["rotation"])) % 360

    fps = _frac(vs.get("avg_frame_rate"))
    if fps <= 0:
        fps = _frac(vs.get("r_frame_rate"), Fraction(30))

    video = VideoInfo(
        codec=vs["codec_name"],
        profile=vs.get("profile", ""),
        level=int(vs.get("level", 0)),
        width=int(vs["width"]),
        height=int(vs["height"]),
        sar=_frac(vs.get("sample_aspect_ratio"), Fraction(1)) or Fraction(1),
        rotation=rotation,
        fps=fps,
        pix_fmt=vs.get("pix_fmt", ""),
        bit_depth=_bit_depth(vs),
        primaries=_tag(vs.get("color_primaries")),
        transfer=_tag(vs.get("color_transfer")),
        matrix=_tag(vs.get("color_space")),
        full_range=vs.get("color_range") == "pc" or vs.get("pix_fmt", "").startswith("yuvj"),
        time_base=v_tb,
        field_order=vs.get("field_order", ""),
        codec_tag=vs.get("codec_tag_string", ""),
        extradata_hash=vs.get("extradata_hash", ""),
    )

    kf = kd = None
    if keyframes:
        # Packet flags only: demuxes without decoding. Times are rebased to the
        # file's first timestamp, which is also what ffmpeg's -ss counts from.
        out = _ffprobe(
            ["-select_streams", f"{vs['index']}", "-show_entries", "packet=pts,dts,flags", "-of", "csv=p=0", path]
        )
        index = {}
        for line in out.splitlines():
            pts, dts, flags = (line.split(",") + ["", ""])[:3]
            if "K" in flags and pts.lstrip("-").isdigit():
                p = int(pts) * v_tb - file_start
                index[max(p, Fraction(0))] = (int(dts) * v_tb - file_start) if dts.lstrip("-").isdigit() else p
        kf = tuple(sorted(index))
        kd = tuple(index[k] for k in kf)

    return MediaInfo(
        container=fmt.get("format_name", ""),
        duration=duration,
        video=video,
        audio=audio,
        keyframes=kf,
        keyframe_dts=kd,
    )
