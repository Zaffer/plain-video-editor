"""Editing session: the project being edited, the selection, undo, and timeline maths.

Pure logic with no GUI or player types, so it can be tested headlessly and
ported with the rest of the core. The GUI calls these operations and redraws.

Either a video clip or a sound item is "the target": the thing the trim bar,
the timestamps and the volume act on.

Every sound is shown in a lane. A clip's own sound sits in lane 1 as a linked
item: it is not stored as an item but follows the clip (trim, split, reorder)
and stays part of a lossless copy. Moving or trimming it by itself turns it
into an ordinary item, which no longer follows the clip.
"""
from __future__ import annotations

import copy
import json
import os
from fractions import Fraction
from pathlib import Path

from .media import MediaInfo
from .model import AudioItem, AudioLane, Clip, Project, Rect
from .planner import ExportPlan, plan

MIN_CROP = 16  # display pixels
MIN_SOUND = Fraction(1, 20)  # shortest sound item, seconds
UNDO_LIMIT = 200
LANES = 2  # sound lanes; the clips' own sound shows in the first
OWN = "own"  # a sound reference is (OWN, clip index) or (lane, item index)
MIN_GAIN_DB, MAX_GAIN_DB = -60.0, 12.0


def centered_rect(dw: float, dh: float, ratio: Fraction) -> Rect:
    """Largest centred rectangle of `ratio` (w/h) inside a dw x dh frame, in whole display pixels."""
    r = float(ratio)
    w, h = (dh * r, dh) if dw / dh > r else (dw, dw / r)
    w, h = int(w), int(h)
    return Rect(int((dw - w) / 2), int((dh - h) / 2), w, h)


class Session:
    def __init__(self) -> None:
        self.project = Project(media={}, clips=[], lanes=[AudioLane() for _ in range(LANES)])
        self.infos: dict[str, MediaInfo] = {}
        self.selected = -1  # selected clip
        self.sel_audio: tuple[int, int] | None = None  # (lane, item) when a sound item is the target
        self.sel_own = False  # the selected clip's own sound is the target
        self.exact = False  # force exact cuts (always re-encode)
        self.lock_shape = True  # a crop shape applies to every clip
        self.file: str | None = None  # project file, once saved or opened
        self._saved = self._state()
        self._undo: list[tuple] = []
        self._redo: list[tuple] = []

    # ------------------------------------------------------------ basics

    @property
    def clips(self) -> list[Clip]:
        return self.project.clips

    @property
    def lanes(self) -> list[AudioLane]:
        return self.project.lanes

    @property
    def clip(self) -> Clip | None:
        return self.clips[self.selected] if 0 <= self.selected < len(self.clips) else None

    @property
    def sound(self) -> AudioItem | None:
        """The selected sound item, if a sound item is the target.

        For a clip's own sound this is a copy made from the clip: read it, but
        change it only through the methods below.
        """
        if self.sel_own:
            return self.own_sound(self.selected)
        if self.sel_audio is None:
            return None
        lane, i = self.sel_audio
        return self.lanes[lane].items[i]

    def own_sound(self, i: int) -> AudioItem | None:
        """Clip i's own sound as it sits in lane 1, or None if the clip has none (or it was removed)."""
        if not (0 <= i < len(self.clips)):
            return None
        c = self.clips[i]
        if not c.audio or self.infos[c.media].audio is None:
            return None
        return AudioItem(media=c.media, start=c.start, end=c.end, at=self.offsets()[i], gain_db=c.gain_db)

    def lane_items(self, lane: int) -> list[tuple[tuple, AudioItem, bool]]:
        """Everything shown in a lane, as (reference, item, linked): for lane 1 the clips' own sounds, then its items."""
        out = []
        if lane == 0:
            out += [((OWN, i), it, True) for i in range(len(self.clips)) if (it := self.own_sound(i)) is not None]
        if lane < len(self.lanes):
            out += [((lane, i), it, False) for i, it in enumerate(self.lanes[lane].items)]
        return out

    @property
    def sound_ref(self) -> tuple | None:
        """Which sound is the target: (OWN, clip index), (lane, item index), or None."""
        if self.sel_own:
            return (OWN, self.selected) if self.sound is not None else None
        return self.sel_audio

    @property
    def sound_lane(self) -> int | None:
        """The lane the selected sound is shown in."""
        ref = self.sound_ref
        return None if ref is None else 0 if ref[0] == OWN else ref[0]

    @property
    def target(self) -> Clip | AudioItem | None:
        return self.sound or self.clip

    def target_info(self) -> MediaInfo:
        return self.infos[self.target.media]

    def target_path(self) -> str:
        return self.project.media[self.target.media]

    def info(self, i: int | None = None) -> MediaInfo:
        return self.infos[self.clips[self.selected if i is None else i].media]

    def path(self, i: int | None = None) -> str:
        return self.project.media[self.clips[self.selected if i is None else i].media]

    def frame(self) -> Fraction:
        """Smallest step for the target: one video frame, or a short moment for sound."""
        info = self.target_info()
        return 1 / info.video.fps if (self.sound is None and info.video) else MIN_SOUND

    def display_size(self, i: int | None = None) -> tuple[float, float]:
        w, h = self.info(i).video.display_size
        return float(w), float(h)

    def select(self, i: int) -> None:
        self.selected, self.sel_audio, self.sel_own = i, None, False

    def select_sound(self, ref: tuple) -> None:
        """Make a sound the target: ref is (OWN, clip index) or (lane, item index)."""
        if ref[0] == OWN:
            self.selected, self.sel_audio, self.sel_own = ref[1], None, True
        else:
            self.sel_audio, self.sel_own = ref, False

    def clear_sound(self) -> None:
        """Back to the selected clip as the target."""
        self.sel_audio, self.sel_own = None, False

    # ------------------------------------------------------------ undo

    def _snapshot(self) -> tuple:
        return copy.deepcopy(self.clips), copy.deepcopy(self.lanes), self.selected, self.sel_audio, self.sel_own

    def checkpoint(self) -> None:
        """Call before a change (once per gesture) so it can be undone."""
        self._undo.append(self._snapshot())
        del self._undo[:-UNDO_LIMIT]
        self._redo.clear()

    def _swap(self, src: list, dst: list) -> bool:
        if not src:
            return False
        dst.append(self._snapshot())
        self.project.clips, self.project.lanes, self.selected, self.sel_audio, self.sel_own = src.pop()
        return True

    def undo(self) -> bool:
        return self._swap(self._undo, self._redo)

    def redo(self) -> bool:
        return self._swap(self._redo, self._undo)

    # ------------------------------------------------------------ media and clips

    def _media_id(self, path: str, info: MediaInfo) -> str:
        path = str(Path(path).resolve())
        for k, v in self.project.media.items():
            if v == path:
                return k
        stem = Path(path).stem or "clip"
        media, n = stem, 1
        while media in self.project.media:
            n += 1
            media = f"{stem}-{n}"
        self.project.media[media] = path
        self.infos[media] = info
        return media

    def add_media(self, path: str, info: MediaInfo) -> int:
        """Add a video file as a new clip covering all of it. Returns the clip's index."""
        if info.video is None:
            raise ValueError("this file has no picture; add it as sound instead")
        media = self._media_id(path, info)
        self.checkpoint()
        clip = Clip(media=media, start=Fraction(0), end=self.infos[media].duration,
                    audio=self.infos[media].audio is not None)
        if self.lock_shape and self.clips and self.clips[0].crop_aspect:
            # Joining a montage that has a shape: take the same shape.
            ratio = self.clips[0].crop_aspect
            dw, dh = (float(x) for x in self.infos[media].video.display_size)
            clip.crop, clip.crop_aspect = centered_rect(dw, dh, ratio), ratio
        self.clips.append(clip)
        self.select(len(self.clips) - 1)
        return self.selected

    def set_info(self, path: str, info: MediaInfo) -> None:
        """Replace a file's description (e.g. once its keyframes are indexed)."""
        path = str(Path(path).resolve())
        for k, v in self.project.media.items():
            if v == path:
                self.infos[k] = info

    def remove(self, i: int) -> None:
        self.checkpoint()
        del self.clips[i]
        self.selected = min(self.selected if self.selected < i else max(self.selected - 1, 0), len(self.clips) - 1)
        self.clear_sound()

    def move(self, i: int, j: int) -> None:
        """Move clip i to position j. The caller checkpoints once per drag."""
        j = max(0, min(j, len(self.clips) - 1))
        if i != j:
            self.clips.insert(j, self.clips.pop(i))
            self.selected = j

    # ------------------------------------------------------------ trimming (clip or sound item)

    def _unlink(self) -> None:
        """Turn the selected clip's own sound into an ordinary item in lane 1. It no longer follows the clip."""
        item, c = self.own_sound(self.selected), self.clip
        c.audio, c.gain_db = False, 0.0
        while len(self.lanes) < LANES:
            self.lanes.append(AudioLane())
        self.lanes[0].items.append(item)
        self.sel_audio, self.sel_own = (0, len(self.lanes[0].items) - 1), False

    def _editable(self) -> Clip | AudioItem:
        """The target, as something that can be changed in place."""
        if self.sel_own:
            self._unlink()
        return self.target

    def set_in(self, t: Fraction) -> None:
        c = self._editable()
        c.start = max(Fraction(0), min(t, c.end - self.frame()))

    def set_out(self, t: Fraction) -> None:
        c = self._editable()
        c.end = min(self.target_info().duration, max(t, c.start + self.frame()))

    def split(self, t: Fraction) -> bool:
        """Cut the selected clip in two at source time t. The second half becomes selected."""
        c = self.clip
        if c is None or self.sound is not None or not (c.start + self.frame() <= t <= c.end - self.frame()):
            return False
        self.checkpoint()
        second = copy.deepcopy(c)
        c.end, second.start = t, t
        self.clips.insert(self.selected + 1, second)
        self.selected += 1
        return True

    def set_gain(self, db: float) -> None:
        target = self.clip if self.sel_own else self.target  # a clip's own sound keeps its volume on the clip
        target.gain_db = round(max(MIN_GAIN_DB, min(MAX_GAIN_DB, db)), 2)

    # ------------------------------------------------------------ sound lanes

    def add_sound(self, path: str, info: MediaInfo, at: Fraction) -> tuple[int, int]:
        """Add a file's sound at montage time `at`, in the first lane where it overlaps nothing (else the last lane)."""
        if info.audio is None:
            raise ValueError("this file has no sound")
        media = self._media_id(path, info)
        self.checkpoint()
        while len(self.lanes) < LANES:
            self.lanes.append(AudioLane())
        item = AudioItem(media=media, start=Fraction(0), end=self.infos[media].duration, at=max(at, Fraction(0)))
        lane = next((n for n in range(LANES) if not self._overlaps(n, item)), LANES - 1)
        self.lanes[lane].items.append(item)
        self.sel_audio, self.sel_own = (lane, len(self.lanes[lane].items) - 1), False
        return self.sel_audio

    def _overlaps(self, lane: int, item: AudioItem) -> bool:
        a0, a1 = item.at, item.at + (item.end - item.start)
        return any(o.at < a1 and a0 < o.at + (o.end - o.start) for _, o, _ in self.lane_items(lane))

    def move_sound(self, at: Fraction, lane: int | None = None) -> None:
        """Move the selected sound item along the montage, and optionally to another lane."""
        if self.sel_own:
            self._unlink()
        cur, i = self.sel_audio
        item = self.lanes[cur].items[i]
        item.at = max(at, Fraction(0))
        if lane is not None and lane != cur and 0 <= lane < len(self.lanes):
            self.lanes[cur].items.pop(i)
            self.lanes[lane].items.append(item)
            self.sel_audio = (lane, len(self.lanes[lane].items) - 1)

    def remove_sound(self) -> None:
        self.checkpoint()
        if self.sel_own:
            self.clip.audio, self.clip.gain_db = False, 0.0
        else:
            lane, i = self.sel_audio
            del self.lanes[lane].items[i]
        self.clear_sound()

    def sound_at(self, lane: int, t: Fraction) -> tuple[AudioItem, Fraction] | None:
        """The stored item playing in `lane` at montage time t, and the time within its source.

        Clips' own sounds are not included: they play with their clip.
        """
        for it in self.lanes[lane].items if lane < len(self.lanes) else []:
            if it.at <= t < it.at + (it.end - it.start):
                return it, it.start + (t - it.at)
        return None

    # ------------------------------------------------------------ crop

    def set_shape(self, shape: Fraction | str) -> None:
        """shape: "off", "free", or a width/height ratio. With lock_shape, ratios and "off" apply to every clip."""
        if self.clip is None:
            return
        self.checkpoint()
        everyone = self.lock_shape and shape != "free"
        for i in range(len(self.clips)) if everyone else [self.selected]:
            c = self.clips[i]
            dw, dh = self.display_size(i)
            if shape == "off":
                c.crop, c.crop_aspect = None, None
            elif shape == "free":
                if not isinstance(c.crop, Rect):
                    c.crop = Rect(int(dw * 0.1), int(dh * 0.1), int(dw * 0.8), int(dh * 0.8))
                c.crop_aspect = None
            elif c.crop_aspect != shape or not isinstance(c.crop, Rect):
                c.crop, c.crop_aspect = centered_rect(dw, dh, shape), shape

    def shape(self) -> Fraction | str:
        c = self.clip
        if c is None or c.crop is None:
            return "off"
        return c.crop_aspect or "free"

    def set_crop(self, rect: Rect) -> None:
        """Set the selected clip's crop, kept inside the frame. The caller checkpoints once per drag."""
        dw, dh = self.display_size()
        w = max(MIN_CROP, min(rect.w, int(dw)))
        h = max(MIN_CROP, min(rect.h, int(dh)))
        x = max(0, min(rect.x, int(dw) - w))
        y = max(0, min(rect.y, int(dh) - h))
        self.clip.crop = Rect(x, y, w, h)

    # ------------------------------------------------------------ montage timeline

    def offsets(self) -> list[Fraction]:
        out, t = [], Fraction(0)
        for c in self.clips:
            out.append(t)
            t += c.duration
        return out

    @property
    def total(self) -> Fraction:
        return sum((c.duration for c in self.clips), Fraction(0))

    def montage_time(self, source_time: Fraction) -> Fraction:
        """Where the selected clip's source time falls on the montage (clamped into the clip)."""
        c = self.clip
        return self.offsets()[self.selected] + max(Fraction(0), min(source_time, c.end) - c.start)

    def locate(self, t: Fraction) -> tuple[int, Fraction]:
        """Montage time -> (clip index, source time). Past the end gives the last clip's out point."""
        for i, (off, c) in enumerate(zip(self.offsets(), self.clips)):
            if t < off + c.duration:
                return i, c.start + max(t - off, Fraction(0))
        return len(self.clips) - 1, self.clips[-1].end

    # ------------------------------------------------------------ export

    def plan(self) -> ExportPlan | None:
        """What an export would do now. None when there are no clips. Raises PlanError."""
        if not self.clips:
            return None
        return plan(self.project, self.infos, mode="encode" if self.exact else "auto")

    def default_output(self, p: ExportPlan) -> str:
        first = Path(self.path(0))
        suffix = first.suffix if p.kind == "copy" else ".mp4"
        name = f"{first.stem}-cut" if len(self.clips) == 1 else f"{first.stem}-montage"
        return str(first.with_name(name + suffix))

    # ------------------------------------------------------------ project files

    def _state(self) -> str:
        return json.dumps(self.project.to_dict(), sort_keys=True) if self.project.clips else ""

    @property
    def dirty(self) -> bool:
        """There are edits that have not been saved to a project file."""
        return self._state() != self._saved

    def save(self, file: str) -> None:
        """Write the project. Media paths are stored relative to the project file."""
        base = Path(file).resolve().parent
        d = self.project.to_dict()
        d["media"] = {k: {"path": os.path.relpath(v, base)} for k, v in self.project.media.items()}
        tmp = Path(file).with_name(Path(file).name + ".tmp")
        tmp.write_text(json.dumps(d, indent=2) + "\n")
        os.replace(tmp, file)
        self.file, self._saved = str(Path(file).resolve()), self._state()

    @staticmethod
    def read(file: str) -> Project:
        """Read a project file; media paths come back absolute."""
        base = Path(file).resolve().parent
        project = Project.from_dict(json.loads(Path(file).read_text()))
        project.media = {k: str((base / v).resolve()) for k, v in project.media.items()}
        return project

    def open(self, file: str, project: Project, infos: dict[str, MediaInfo]) -> None:
        """Replace the session with a project that has been read and whose media has been probed."""
        while len(project.lanes) < LANES:
            project.lanes.append(AudioLane())
        self.project, self.infos = project, infos
        self.selected, self.sel_audio, self.sel_own = (0 if project.clips else -1), None, False
        self._undo.clear()
        self._redo.clear()
        self.file, self._saved = str(Path(file).resolve()), self._state()
