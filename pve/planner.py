"""Export planner: Project + MediaInfo -> a neutral ExportPlan.

The plan says *what* to do (copy these packet ranges, or re-encode these
ranges to this target and mix this audio). It contains no ffmpeg syntax; a
backend lowers it (ffmpeg_backend.py natively).
"""
from __future__ import annotations

from bisect import bisect_left, bisect_right
from dataclasses import asdict, dataclass, field
from fractions import Fraction

from .media import MediaInfo, VideoInfo
from .model import AspectCrop, Project, Rect

DRAFT_MAX_HEIGHT = 480
DRAFT_MAX_FPS = 30


class PlanError(ValueError):
    pass


@dataclass(frozen=True)
class SourceColor:
    kind: str  # "sdr" | "hlg" | "pq"
    matrix: str  # "bt709" | "bt601" | "bt2020nc" (resolved; never empty)
    full_range: bool
    bit_depth: int


@dataclass(frozen=True)
class Segment:
    media: str
    start: Fraction  # what will actually be used
    end: Fraction
    requested_start: Fraction  # differs from start/end only when a copy snaps to keyframes
    requested_end: Fraction
    # Copy only: decode time of the first packet that is NOT copied (the keyframe
    # at `end`). None = copy to the end of the file.
    end_decode: Fraction | None = None
    crop: Rect | None = None  # in decoded pixels (upright, before pixel-aspect correction)
    fit: tuple[int, int] | None = None  # size inside the canvas; smaller than canvas = padded
    color: SourceColor | None = None

    @property
    def duration(self) -> Fraction:
        return self.end - self.start


@dataclass(frozen=True)
class AudioSource:
    media: str
    start: Fraction
    end: Fraction
    at: Fraction  # position in the output
    gain_db: float


@dataclass(frozen=True)
class VideoTarget:
    width: int
    height: int
    fps: Fraction
    color: str  # "sdr" | "hlg" | "pq"


@dataclass
class ExportPlan:
    kind: str  # "copy" (lossless) | "encode"
    duration: Fraction
    segments: list[Segment]
    video: VideoTarget | None = None  # encode only
    audio: list[AudioSource] = field(default_factory=list)  # encode only
    copy_audio: bool = False  # copy only
    reasons: list[str] = field(default_factory=list)  # why an encode plan is not lossless
    warnings: list[str] = field(default_factory=list)
    draft: bool = False

    def to_dict(self) -> dict:
        return _plain(asdict(self))


def _plain(v):
    if isinstance(v, Fraction):
        return str(v)
    if isinstance(v, dict):
        return {k: _plain(x) for k, x in v.items()}
    if isinstance(v, (list, tuple)):
        return [_plain(x) for x in v]
    return v


def _even(x, minimum=2) -> int:
    return max(minimum, 2 * round(Fraction(x) / 2))


def source_color(v: VideoInfo) -> SourceColor:
    matrix = v.matrix
    if matrix in ("smpte170m", "bt470bg"):
        matrix = "bt601"
    elif matrix not in ("bt709", "bt2020nc"):
        # Untagged: same guess players make. HD is BT.709, SD is BT.601.
        matrix = "bt709" if (v.width >= 1280 or v.height > 576) else "bt601"
    return SourceColor(kind=v.color_kind, matrix=matrix, full_range=v.full_range, bit_depth=v.bit_depth)


def resolve_crop(crop: Rect | AspectCrop | None, v: VideoInfo) -> Rect | None:
    """Project crop (display pixels) -> crop in decoded pixels, or None for the full frame."""
    if crop is None:
        return None
    dw, dh = v.decoded_size
    sx, sy = v.display_scale
    disp_w, disp_h = v.display_size
    if isinstance(crop, AspectCrop):
        if disp_w / disp_h > crop.ratio:
            w, h = disp_h * crop.ratio / sx, Fraction(dh)
        else:
            w, h = Fraction(dw), disp_w / crop.ratio / sy
        w, h = min(_even(w), dw), min(_even(h), dh)
        x, y = _even((dw - w) / 2, 0), _even((dh - h) / 2, 0)
    else:
        x, w = _even(crop.x / sx, 0), _even(crop.w / sx)
        y, h = _even(crop.y / sy, 0), _even(crop.h / sy)
        if x + w > dw or y + h > dh:
            raise PlanError(f"crop {crop} is outside the {float(disp_w):g}x{float(disp_h):g} frame")
    if (x, y, w, h) == (0, 0, dw, dh):
        return None
    return Rect(x, y, w, h)


def _copy_blockers(project: Project, infos: dict[str, MediaInfo], crops: list[Rect | None]) -> list[str]:
    """Reasons this project cannot be exported by stream copy. Empty = lossless is possible."""
    why: list[str] = []
    if project.output_size:
        why.append("an output size is set")
    for i, c in enumerate(crops):
        if c is not None:
            why.append(f"clip {i + 1} is cropped")
    if any(lane.items for lane in project.lanes):
        why.append("audio lanes need mixing")
    if any(c.gain_db for c in project.clips):
        why.append("a clip's volume is changed")

    has_audio = [c.audio and infos[c.media].audio is not None for c in project.clips]
    if any(has_audio) and not all(has_audio):
        why.append("some clips have sound and others do not")

    first = infos[project.clips[0].media]
    for i, c in enumerate(project.clips[1:], start=2):
        if c.media == project.clips[0].media:
            continue
        diff = _differences(first, infos[c.media], with_audio=all(has_audio))
        if diff:
            why.append(f"clip {i} differs from clip 1 ({', '.join(diff)})")
    if any(infos[c.media].keyframes is None for c in project.clips):
        why.append("keyframes are not indexed")
    return why


def _differences(a: MediaInfo, b: MediaInfo, with_audio: bool) -> list[str]:
    names = {
        "codec": "codec", "profile": "profile", "level": "level", "width": "width", "height": "height",
        "sar": "pixel shape", "rotation": "rotation", "fps": "frame rate", "pix_fmt": "pixel format",
        "primaries": "colour", "transfer": "colour", "matrix": "colour", "full_range": "colour range",
        "time_base": "time base", "field_order": "field order", "codec_tag": "codec tag",
        "extradata_hash": "encoder settings",
    }
    out: list[str] = []
    for attr, label in names.items():
        if getattr(a.video, attr) != getattr(b.video, attr) and label not in out:
            out.append(label)
    if with_audio and a.audio and b.audio:
        for attr, label in {
            "codec": "audio codec", "profile": "audio codec", "sample_rate": "audio sample rate",
            "layout": "audio channels", "time_base": "audio time base", "extradata_hash": "audio settings",
        }.items():
            if getattr(a.audio, attr) != getattr(b.audio, attr) and label not in out:
                out.append(label)
    return out


def _snap(info: MediaInfo, start: Fraction, end: Fraction) -> tuple[Fraction, Fraction, Fraction | None]:
    """Widen [start, end] to keyframes: start to the keyframe at or before, end to the one at or after.

    Returns (start, end, decode time of the end keyframe or None when the cut runs to the end of the file).
    """
    kf = info.keyframes or (Fraction(0),)
    i = bisect_right(kf, start) - 1
    s = kf[i] if i >= 0 else Fraction(0)
    j = bisect_left(kf, end)
    if j >= len(kf):
        return s, info.duration, None
    return s, kf[j], (info.keyframe_dts[j] if info.keyframe_dts else kf[j])


def plan(project: Project, infos: dict[str, MediaInfo], mode: str = "auto", draft: bool = False) -> ExportPlan:
    """mode: "auto" (copy when possible), "copy" (fail if not possible), "encode" (always exact)."""
    if mode not in ("auto", "copy", "encode"):
        raise PlanError(f"unknown mode {mode!r}")
    project.validate()
    for i, c in enumerate(project.clips):
        info = infos[c.media]
        if info.video is None:
            raise PlanError(f"clip {i + 1}: {c.media!r} has no picture")
        if c.end > info.duration + info.video.fps ** -1:
            raise PlanError(f"clip {i + 1}: out point {float(c.end):g}s is past the end ({float(info.duration):.3f}s)")
    crops = [resolve_crop(c.crop, infos[c.media].video) for c in project.clips]

    blockers = _copy_blockers(project, infos, crops)
    if mode == "copy" and blockers:
        raise PlanError("cannot export losslessly: " + "; ".join(blockers))
    if mode != "encode" and not blockers and not draft:
        return _plan_copy(project, infos)
    return _plan_encode(project, infos, crops, blockers if mode != "encode" else ["exact cuts were requested"], draft)


def _plan_copy(project: Project, infos: dict[str, MediaInfo]) -> ExportPlan:
    segs = []
    for c in project.clips:
        s, e, e_dts = _snap(infos[c.media], c.start, c.end)
        segs.append(
            Segment(media=c.media, start=s, end=e, requested_start=c.start, requested_end=c.end, end_decode=e_dts)
        )
    warnings = []
    if len(segs) > 1 and infos[segs[0].media].video.codec == "hevc":
        warnings.append("joining HEVC losslessly can glitch at the joins on some files; check the result")
    first = project.clips[0]
    return ExportPlan(
        kind="copy",
        duration=sum((s.duration for s in segs), Fraction(0)),
        segments=segs,
        copy_audio=first.audio and infos[first.media].audio is not None,
        warnings=warnings,
    )


def _plan_encode(project, infos, crops, reasons, draft) -> ExportPlan:
    # Display size of each clip after its crop.
    sizes: list[tuple[Fraction, Fraction]] = []
    for c, crop in zip(project.clips, crops):
        v = infos[c.media].video
        w, h = (crop.w, crop.h) if crop else v.decoded_size
        sx, sy = v.display_scale
        sizes.append((w * sx, h * sy))

    if project.output_size:
        cw, ch = project.output_size
    else:
        # Canvas = the largest cropped clip, so nothing is downscaled.
        cw, ch = max(sizes, key=lambda s: s[0] * s[1])
    if draft and ch > DRAFT_MAX_HEIGHT:
        cw, ch = cw * DRAFT_MAX_HEIGHT / ch, DRAFT_MAX_HEIGHT
    cw, ch = _even(cw), _even(ch)

    kinds = {infos[c.media].video.color_kind for c in project.clips}
    # Keep HDR only when every clip is the same kind of HDR; otherwise standard range.
    color = kinds.pop() if len(kinds) == 1 and not draft else "sdr"

    fps = max(infos[c.media].video.fps for c in project.clips)
    if draft:
        fps = min(fps, Fraction(DRAFT_MAX_FPS))

    segments, audio, t = [], [], Fraction(0)
    for c, crop, (w, h) in zip(project.clips, crops, sizes):
        info = infos[c.media]
        scale = min(cw / w, ch / h)
        fw, fh = min(_even(w * scale), cw), min(_even(h * scale), ch)
        if cw - fw <= 2 and ch - fh <= 2:  # near enough: fill the canvas rather than pad a 1px border
            fw, fh = cw, ch
        segments.append(
            Segment(
                media=c.media, start=c.start, end=c.end, requested_start=c.start, requested_end=c.end,
                crop=crop, fit=(fw, fh), color=source_color(info.video),
            )
        )
        if c.audio and info.audio is not None:
            audio.append(AudioSource(media=c.media, start=c.start, end=c.end, at=t, gain_db=c.gain_db))
        t += c.duration

    for li, lane in enumerate(project.lanes):
        for ii, it in enumerate(lane.items):
            info = infos[it.media]
            if info.audio is None:
                raise PlanError(f"audio lane {li + 1} item {ii + 1}: {it.media!r} has no sound")
            if it.at >= t:
                continue  # entirely after the end of the video
            audio.append(
                AudioSource(media=it.media, start=it.start, end=min(it.end, it.start + (t - it.at)),
                            at=it.at, gain_db=lane.gain_db + it.gain_db)
            )

    return ExportPlan(
        kind="encode", duration=t, segments=segments,
        video=VideoTarget(width=cw, height=ch, fps=fps, color=color),
        audio=audio, reasons=reasons, draft=draft,
    )
