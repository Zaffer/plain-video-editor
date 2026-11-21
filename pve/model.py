"""Project model: pure data with exact rational time.

Nothing here knows about ffmpeg, mpv, or the filesystem beyond an opaque path
string per media id. This is the part that must port unchanged to Rust/WASM.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from fractions import Fraction

FORMAT_VERSION = 1


class ProjectError(ValueError):
    pass


def parse_time(v) -> Fraction:
    """Seconds as int, decimal or 'n/d' string. Floats go via str() so 7.5 is exact."""
    if v is None or isinstance(v, bool):
        raise ProjectError(f"not a time: {v!r}")
    try:
        return Fraction(str(v))
    except (ValueError, ZeroDivisionError) as e:
        raise ProjectError(f"not a time: {v!r}") from e


def time_str(t: Fraction) -> str:
    return str(t.numerator) if t.denominator == 1 else f"{t.numerator}/{t.denominator}"


def parse_ratio(v) -> Fraction:
    try:
        return Fraction(str(v).replace(":", "/"))
    except (ValueError, ZeroDivisionError) as e:
        raise ProjectError(f"not a ratio: {v!r}") from e


@dataclass(frozen=True)
class Rect:
    """Crop rectangle in display pixels: upright, square-pixel, as the viewer sees the clip."""
    x: int
    y: int
    w: int
    h: int


@dataclass(frozen=True)
class AspectCrop:
    """Largest centred rectangle of this width/height ratio."""
    ratio: Fraction


@dataclass
class Clip:
    media: str
    start: Fraction  # seconds from the start of the media
    end: Fraction
    crop: Rect | AspectCrop | None = None
    crop_aspect: Fraction | None = None  # shape a Rect crop is held to while editing; None = free
    audio: bool = True  # keep the clip's own sound
    gain_db: float = 0.0

    @property
    def duration(self) -> Fraction:
        return self.end - self.start


@dataclass
class AudioItem:
    media: str
    start: Fraction
    end: Fraction
    at: Fraction  # position on the montage timeline
    gain_db: float = 0.0


@dataclass
class AudioLane:
    items: list[AudioItem] = field(default_factory=list)
    gain_db: float = 0.0


@dataclass
class Project:
    media: dict[str, str]  # media id -> path (relative paths are relative to the project file)
    clips: list[Clip]
    lanes: list[AudioLane] = field(default_factory=list)
    output_size: tuple[int, int] | None = None  # None = decided from the clips

    def validate(self) -> None:
        if not self.clips:
            raise ProjectError("project has no clips")
        spans = [(f"clip {i + 1}", c.media, c.start, c.end) for i, c in enumerate(self.clips)]
        for li, lane in enumerate(self.lanes):
            for ii, it in enumerate(lane.items):
                spans.append((f"audio lane {li + 1} item {ii + 1}", it.media, it.start, it.end))
                if it.at < 0:
                    raise ProjectError(f"audio lane {li + 1} item {ii + 1}: 'at' is negative")
        for what, media, start, end in spans:
            if media not in self.media:
                raise ProjectError(f"{what}: unknown media {media!r}")
            if start < 0 or end <= start:
                raise ProjectError(f"{what}: needs 0 <= in < out")

    @classmethod
    def from_dict(cls, d: dict) -> "Project":
        if d.get("version") != FORMAT_VERSION:
            raise ProjectError(f"unsupported project version {d.get('version')!r}")
        try:
            media = {k: (v["path"] if isinstance(v, dict) else v) for k, v in d["media"].items()}
            clips = [
                Clip(
                    media=c["media"],
                    start=parse_time(c["in"]),
                    end=parse_time(c["out"]),
                    crop=_crop_from(c.get("crop")),
                    crop_aspect=parse_ratio(c["crop_aspect"]) if c.get("crop_aspect") else None,
                    audio=bool(c.get("audio", True)),
                    gain_db=float(c.get("gain_db", 0.0)),
                )
                for c in d["clips"]
            ]
            lanes = [
                AudioLane(
                    items=[
                        AudioItem(
                            media=i["media"],
                            start=parse_time(i["in"]),
                            end=parse_time(i["out"]),
                            at=parse_time(i.get("at", 0)),
                            gain_db=float(i.get("gain_db", 0.0)),
                        )
                        for i in lane.get("items", [])
                    ],
                    gain_db=float(lane.get("gain_db", 0.0)),
                )
                for lane in d.get("audio_lanes", [])
            ]
            out = d.get("output") or {}
            size = (int(out["width"]), int(out["height"])) if "width" in out else None
        except (KeyError, TypeError) as e:
            raise ProjectError(f"malformed project: {e!r}") from e
        p = cls(media=media, clips=clips, lanes=lanes, output_size=size)
        p.validate()
        return p

    def to_dict(self) -> dict:
        d: dict = {
            "version": FORMAT_VERSION,
            "media": {k: {"path": v} for k, v in self.media.items()},
            "clips": [
                {
                    "media": c.media,
                    "in": time_str(c.start),
                    "out": time_str(c.end),
                    **({"crop": _crop_to(c.crop)} if c.crop else {}),
                    **({"crop_aspect": f"{c.crop_aspect.numerator}:{c.crop_aspect.denominator}"} if c.crop_aspect else {}),
                    "audio": c.audio,
                    "gain_db": c.gain_db,
                }
                for c in self.clips
            ],
            "audio_lanes": [
                {
                    "gain_db": lane.gain_db,
                    "items": [
                        {
                            "media": i.media,
                            "in": time_str(i.start),
                            "out": time_str(i.end),
                            "at": time_str(i.at),
                            "gain_db": i.gain_db,
                        }
                        for i in lane.items
                    ],
                }
                for lane in self.lanes
            ],
        }
        if self.output_size:
            d["output"] = {"width": self.output_size[0], "height": self.output_size[1]}
        return d


def _crop_from(c) -> Rect | AspectCrop | None:
    if not c:
        return None
    if "aspect" in c:
        r = parse_ratio(c["aspect"])
        if r <= 0:
            raise ProjectError("crop aspect must be positive")
        return AspectCrop(r)
    r = Rect(int(c["x"]), int(c["y"]), int(c["w"]), int(c["h"]))
    if r.x < 0 or r.y < 0 or r.w <= 0 or r.h <= 0:
        raise ProjectError(f"bad crop rectangle {c!r}")
    return r


def _crop_to(c: Rect | AspectCrop) -> dict:
    if isinstance(c, AspectCrop):
        return {"aspect": f"{c.ratio.numerator}:{c.ratio.denominator}"}
    return {"x": c.x, "y": c.y, "w": c.w, "h": c.h}
