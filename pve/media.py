"""Neutral description of a media file.

The planner only ever sees this. Natively it is filled from ffprobe
(see probe.py); a browser build would fill it from a demuxer library.
"""
from __future__ import annotations

from dataclasses import dataclass, fields
from fractions import Fraction


@dataclass(frozen=True)
class VideoInfo:
    codec: str
    profile: str
    level: int
    width: int  # stored pixels, before rotation and pixel-aspect correction
    height: int
    sar: Fraction  # pixel aspect ratio (1 = square)
    rotation: int  # degrees, 0/90/180/270
    fps: Fraction  # average frame rate
    pix_fmt: str
    bit_depth: int
    primaries: str  # "" when untagged
    transfer: str
    matrix: str
    full_range: bool
    time_base: Fraction
    field_order: str
    codec_tag: str
    extradata_hash: str

    @property
    def decoded_size(self) -> tuple[int, int]:
        """Size after rotation, still in stored (possibly non-square) pixels."""
        return (self.height, self.width) if self.rotation % 180 == 90 else (self.width, self.height)

    @property
    def decoded_sar(self) -> Fraction:
        return 1 / self.sar if self.rotation % 180 == 90 else self.sar

    @property
    def display_scale(self) -> tuple[Fraction, Fraction]:
        """(x, y) factors from decoded pixels to display (square) pixels.

        Non-square pixels are stretched, never squeezed, so no stored detail is
        lost: 1080x1080 with 9:16 pixels displays as 1080x1920, not 608x1080.
        This is also how mpv sizes the picture, so preview and crop coordinates agree.
        """
        sar = self.decoded_sar
        return (sar, Fraction(1)) if sar >= 1 else (Fraction(1), 1 / sar)

    @property
    def display_size(self) -> tuple[Fraction, Fraction]:
        """Upright, square-pixel size. May be fractional for odd pixel shapes."""
        w, h = self.decoded_size
        sx, sy = self.display_scale
        return w * sx, h * sy

    @property
    def color_kind(self) -> str:
        return {"arib-std-b67": "hlg", "smpte2084": "pq"}.get(self.transfer, "sdr")


@dataclass(frozen=True)
class AudioInfo:
    codec: str
    profile: str
    sample_rate: int
    channels: int
    layout: str
    time_base: Fraction
    extradata_hash: str


@dataclass(frozen=True)
class MediaInfo:
    container: str
    duration: Fraction  # seconds, measured from the first timestamp in the file
    video: VideoInfo | None  # None for sound-only files
    audio: AudioInfo | None
    # Video keyframe times in seconds from the start of the media, ascending.
    # None = not indexed (lossless planning is then unavailable).
    keyframes: tuple[Fraction, ...] | None = None
    # Decode time of each keyframe, parallel to `keyframes`. With reordered
    # (B) frames a keyframe is decoded slightly before it is shown.
    keyframe_dts: tuple[Fraction, ...] | None = None

    def to_dict(self) -> dict:
        return {
            "container": self.container,
            "duration": str(self.duration),
            "video": _to(self.video) if self.video else None,
            "audio": _to(self.audio) if self.audio else None,
            "keyframes": [str(k) for k in self.keyframes] if self.keyframes is not None else None,
            "keyframe_dts": [str(k) for k in self.keyframe_dts] if self.keyframe_dts is not None else None,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "MediaInfo":
        kf, kd = d.get("keyframes"), d.get("keyframe_dts")
        return cls(
            container=d["container"],
            duration=Fraction(d["duration"]),
            video=_from(VideoInfo, d["video"]) if d.get("video") else None,
            audio=_from(AudioInfo, d["audio"]) if d.get("audio") else None,
            keyframes=tuple(Fraction(k) for k in kf) if kf is not None else None,
            keyframe_dts=tuple(Fraction(k) for k in kd) if kd is not None else None,
        )


def _to(obj) -> dict:
    return {
        f.name: (str(v) if isinstance(v := getattr(obj, f.name), Fraction) else v) for f in fields(obj)
    }


def _from(cls, d: dict):
    return cls(**{f.name: (Fraction(d[f.name]) if f.type == "Fraction" else d[f.name]) for f in fields(cls)})
