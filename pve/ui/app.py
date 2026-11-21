"""The editor window: preview, trim bar, montage timeline with sound lanes, export line."""
from __future__ import annotations

import math
import os
import sys
import threading
import time
from fractions import Fraction
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, GLib, Gtk, Pango  # noqa: E402

from .. import ffmpeg_backend  # noqa: E402
from ..model import ProjectError, Rect  # noqa: E402
from ..planner import ExportPlan, PlanError  # noqa: E402
from ..probe import ProbeError, probe  # noqa: E402
from ..session import LANES, MIN_GAIN_DB, OWN, Session  # noqa: E402
from .player import LanePlayer, Player  # noqa: E402
from .widgets import CropOverlay, Timeline, TrimBar  # noqa: E402

SHAPES = [("Off", "off"), ("4:3", Fraction(4, 3)), ("16:9", Fraction(16, 9)), ("1:1", Fraction(1)),
          ("9:16", Fraction(9, 16)), ("Free", "free")]
PROJECT_EXT = ".pve"
APP_NAME = "Plain Video Editor"


def clock(t: float) -> str:
    t = max(float(t), 0.0)
    ms = round(t * 1000)
    h, rest = divmod(ms, 3_600_000)
    m, rest = divmod(rest, 60_000)
    return (f"{h}:{m:02d}" if h else f"{m}") + f":{rest // 1000:02d}.{rest % 1000:03d}"


def parse_clock(text: str) -> Fraction | None:
    """ "83.5", "1:23.5" or "1:02:03.25" -> seconds, exactly. None if it is not a time."""
    parts = text.strip().replace(",", ".").split(":")
    if not 1 <= len(parts) <= 3:
        return None
    try:
        nums = [Fraction(p) for p in parts]
    except (ValueError, ZeroDivisionError):
        return None
    if any(n < 0 for n in nums):
        return None
    t = Fraction(0)
    for n in nums:
        t = t * 60 + n
    return t


def floor_us(t: float) -> Fraction:
    """A player time as an exact fraction, rounded down so it never passes the frame it names."""
    return Fraction(math.floor(t * 1_000_000), 1_000_000)


def gain_to_percent(db: float) -> float:
    return 0.0 if db <= MIN_GAIN_DB else 100.0 * 10 ** (db / 20)


def percent_to_gain(pct: float) -> float:
    return MIN_GAIN_DB if pct < 0.5 else 20 * math.log10(pct / 100)


def autosave_file() -> Path:
    return Path(GLib.get_user_state_dir()) / "plain-video-editor" / f"last-session{PROJECT_EXT}"


class Window(Gtk.ApplicationWindow):
    def __init__(self, app, files):
        super().__init__(application=app, title=APP_NAME, default_width=1280, default_height=820)
        self.s = Session()
        self.pos = 0.0  # play position: source time within the target (selected clip, or sound item)
        self._last_pos = 0.0
        self._clip_pos = 0.0  # play position to go back to after working on a sound item
        self._plan: ExportPlan | None = None
        self._syncing = False  # widgets are being updated from the session, not by the user
        self._gesture = False  # a drag is in progress and has already been checkpointed
        self._dragging = False  # the play position follows the pointer, not the player
        self._detached = False  # the preview is showing a cut point, not the play position
        self._cancel: threading.Event | None = None
        self._volume_edit = 0.0  # when the volume was last changed (one undo step per adjustment)
        self._kf_cache: dict[str, tuple[int, tuple[float, ...]]] = {}
        self._caps = ffmpeg_backend.Capabilities()  # until the machine has been probed
        threading.Thread(target=self._detect, daemon=True).start()
        self.lane_players = [LanePlayer() for _ in range(LANES)]

        self._build()
        self.refresh()
        GLib.timeout_add(60, self._sync_lanes)
        if files:
            self.open_paths(files)

    # ------------------------------------------------------------ layout

    def _build(self):
        header = Gtk.HeaderBar()
        self.set_titlebar(header)
        add = Gtk.Button(label="Add video", tooltip_text="Add video files, or open a project (Ctrl+O). You can also drop files on the window.")
        add.connect("clicked", self._choose_files)
        header.pack_start(add)
        self.add_sound_btn = Gtk.Button(label="Add sound", tooltip_text="Put a file's sound in a lane under the clips, starting at the play position")
        self.add_sound_btn.connect("clicked", self._choose_sound)
        header.pack_start(self.add_sound_btn)
        self.export_btn = Gtk.Button(label="Export", tooltip_text="Export (Ctrl+E)")
        self.export_btn.add_css_class("suggested-action")
        self.export_btn.connect("clicked", self._export)
        header.pack_end(self.export_btn)

        menu = Gio.Menu()
        menu.append("Open project…", "win.open-project")
        menu.append("Save project", "win.save")
        menu.append("Save project as…", "win.save-as")
        menu.append("Reopen last session", "win.restore")
        header.pack_end(Gtk.MenuButton(icon_name="open-menu-symbolic", menu_model=menu, tooltip_text="Project"))
        for name, fn in (("open-project", self._choose_project), ("save", lambda *_: self.save()),
                         ("save-as", lambda *_: self.save(ask=True)), ("restore", lambda *_: self.restore())):
            action = Gio.SimpleAction.new(name, None)
            action.connect("activate", fn)
            self.add_action(action)

        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_child(root)

        self.player = Player(self._on_time, self._on_pause)
        self.crop = CropOverlay(self._on_crop)
        self.empty = Gtk.Label(label="Drop video files here, or press Add video")
        self.empty.add_css_class("title-2")
        stage = Gtk.Overlay(child=self.player, vexpand=True)
        stage.add_overlay(self.crop)
        stage.add_overlay(self.empty)
        root.append(stage)

        bar = Gtk.Box(spacing=6, margin_start=12, margin_end=12, margin_top=6, margin_bottom=2)
        root.append(bar)
        self.play_btn = Gtk.Button(icon_name="media-playback-start-symbolic", tooltip_text="Play / pause (Space)")
        self.play_btn.connect("clicked", lambda *_: self.toggle_play())
        bar.append(self.play_btn)

        def time_entry(tip, done):
            e = Gtk.Entry(width_chars=10, max_width_chars=10, tooltip_text=tip + " Type a time and press Enter.")
            e.add_css_class("numeric")
            e.connect("activate", done)
            return e

        self.pos_entry = time_entry("Play position.", lambda e: self._time_typed(e, "pos"))
        bar.append(self.pos_entry)
        self.total_lbl = Gtk.Label()
        self.total_lbl.add_css_class("numeric")
        self.total_lbl.add_css_class("dim-label")
        bar.append(self.total_lbl)
        bar.append(Gtk.Label(label="In", margin_start=14))
        self.in_entry = time_entry("Where the clip starts (I sets it at the play position).", lambda e: self._time_typed(e, "in"))
        bar.append(self.in_entry)
        bar.append(Gtk.Label(label="Out", margin_start=6))
        self.out_entry = time_entry("Where the clip ends (O sets it at the play position).", lambda e: self._time_typed(e, "out"))
        bar.append(self.out_entry)
        self.len_lbl = Gtk.Label(margin_start=4)
        self.len_lbl.add_css_class("numeric")
        self.len_lbl.add_css_class("dim-label")
        bar.append(self.len_lbl)

        bar.append(Gtk.Box(hexpand=True))
        bar.append(Gtk.Label(label="Sound"))
        hear = Gtk.Box(css_classes=["linked"])
        self.hear = []  # per lane: pressed = heard in the preview
        for n in range(LANES):
            b = Gtk.ToggleButton(label=str(n + 1), active=True,
                                 tooltip_text=f"Hear sound lane {n + 1} in the preview. This does not change the export.")
            b.connect("toggled", self._hear_toggled)
            hear.append(b)
            self.hear.append(b)
        bar.append(hear)
        self.volume = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 200, 1)
        self.volume.set_size_request(80, -1)
        self.volume.set_draw_value(False)
        self.volume.add_mark(100, Gtk.PositionType.BOTTOM, None)
        self.volume.set_tooltip_text("Volume of the selected clip or sound")
        self.volume.connect("value-changed", self._volume_changed)
        bar.append(self.volume)
        self.volume_lbl = Gtk.Label(width_chars=5, xalign=0)
        self.volume_lbl.add_css_class("numeric")
        bar.append(self.volume_lbl)

        bar.append(Gtk.Label(label="Crop", margin_start=8))
        shapes = Gtk.Box()
        shapes.add_css_class("linked")
        self.shape_btns = {}
        first = None
        for label, value in SHAPES:
            b = Gtk.ToggleButton(label=label)
            if first is None:
                first = b
            else:
                b.set_group(first)
            b.connect("toggled", self._shape_toggled, value)
            shapes.append(b)
            self.shape_btns[value] = b
        bar.append(shapes)
        self.lock = Gtk.CheckButton(label="All clips", active=True, tooltip_text="Use the same crop shape for every clip")
        self.lock.connect("toggled", lambda b: setattr(self.s, "lock_shape", b.get_active()))
        bar.append(self.lock)

        self.trim = TrimBar(self._on_scrub, self._on_trim)
        root.append(self.trim)
        self.timeline = Timeline(LANES, self._on_clip_click, self._on_clip_move, self._on_sound_press, self._on_sound_move,
                                 self._on_montage_scrub)
        root.append(self.timeline)

        foot = Gtk.Box(spacing=10, margin_start=12, margin_end=12, margin_top=2, margin_bottom=8)
        root.append(foot)
        self.status = Gtk.Label(xalign=0, hexpand=True, ellipsize=Pango.EllipsizeMode.END)
        foot.append(self.status)
        self.progress = Gtk.ProgressBar(valign=Gtk.Align.CENTER, visible=False, width_request=180)
        foot.append(self.progress)
        self.cancel_btn = Gtk.Button(label="Cancel", visible=False)
        self.cancel_btn.connect("clicked", lambda *_: self._cancel and self._cancel.set())
        foot.append(self.cancel_btn)
        self.exact = Gtk.CheckButton(label="Exact cuts", tooltip_text="Cut exactly where asked. This always re-encodes.")
        self.exact.connect("toggled", self._exact_toggled)
        foot.append(self.exact)

        self._entries = [self.pos_entry, self.in_entry, self.out_entry]
        self._needs_target = [self.play_btn, self.trim, self.timeline, self.volume, hear, *self._entries]
        self._needs_clip = [shapes, self.lock, self.exact, self.add_sound_btn]

        keys = Gtk.EventControllerKey()
        keys.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        keys.connect("key-pressed", self._key)
        self.add_controller(keys)

        drop = Gtk.DropTarget.new(Gdk.FileList, Gdk.DragAction.COPY)
        drop.connect("drop", self._dropped)
        self.add_controller(drop)
        self.connect("close-request", self._closing)

    def _detect(self):
        try:
            caps = ffmpeg_backend.detect()
        except ffmpeg_backend.ExportError:
            return
        GLib.idle_add(lambda: (setattr(self, "_caps", caps), self.refresh()) and False)

    # ------------------------------------------------------------ session -> widgets

    def _editing(self) -> Gtk.Entry | None:
        """The time entry being typed in, if any."""
        focus = self.get_focus()
        return next((e for e in self._entries if focus is not None and (focus is e or focus.is_ancestor(e))), None)

    def _keyframes(self, media: str) -> tuple[float, ...]:
        info = self.s.infos[media]
        cached = self._kf_cache.get(media)
        if cached is None or cached[0] != id(info):
            cached = self._kf_cache[media] = (id(info), tuple(float(k) for k in info.keyframes or ()))
        return cached[1]

    def montage_pos(self) -> Fraction:
        """Where the play position falls on the montage."""
        s = self.s
        if s.target is None:
            return Fraction(0)
        if s.sound is not None:
            return s.sound.at + max(floor_us(self.pos) - s.sound.start, Fraction(0))
        return s.montage_time(floor_us(self.pos))

    def refresh(self):
        """Redraw everything from the session and work out what an export would do."""
        s, t = self.s, self.s.target
        clip_mode = s.sound is None
        self._syncing = True
        self.empty.set_visible(t is None)
        for w in self._needs_target:
            w.set_sensitive(t is not None)
        for w in self._needs_clip:
            w.set_sensitive(s.clip is not None and clip_mode)

        try:
            self._plan, problem = s.plan(), None
        except PlanError as e:
            self._plan, problem = None, str(e)
        self.export_btn.set_sensitive(self._plan is not None and self._cancel is None)
        if self._cancel is None:
            self.status.set_text(f"Cannot export: {problem}" if problem else self._describe(self._plan))
            self.status.set_tooltip_text("\n".join(self._plan.reasons + self._plan.warnings) if self._plan else None)

        editing = self._editing()
        if t is not None:
            info = s.target_info()
            snapped = None
            if clip_mode and self._plan and self._plan.kind == "copy":
                seg = self._plan.segments[s.selected]
                if (seg.start, seg.end) != (t.start, t.end):
                    snapped = (float(seg.start), float(seg.end))
            self.trim.update((t.media, clip_mode), float(info.duration), float(t.start), float(t.end), self.pos,
                             self._keyframes(t.media) if clip_mode else (), snapped)
            if self.in_entry is not editing:
                self.in_entry.set_text(clock(t.start))
            if self.out_entry is not editing:
                self.out_entry.set_text(clock(t.end))
            self.len_lbl.set_text(f"{float(t.end - t.start):.3f} s")
            self.total_lbl.set_text(f"/ {clock(info.duration)}")
            pct = gain_to_percent(t.gain_db)
            self.volume.set_value(pct)
            self.volume_lbl.set_text(f"{pct:.0f}%")
        else:
            self.trim.update(None, 0.0, 0.0, 0.0, 0.0, (), None)
            for e in self._entries:
                e.set_text("")
            self.len_lbl.set_text("")
            self.total_lbl.set_text("")
            self.volume_lbl.set_text("")

        c = s.clip
        if c is not None and clip_mode:
            self.crop.update(s.display_size(), c.crop if isinstance(c.crop, Rect) else None,
                             float(c.crop_aspect) if c.crop_aspect else None)
            self.shape_btns[s.shape() if s.shape() in self.shape_btns else "free"].set_active(True)
            self.player.set_sound(s.own_sound(s.selected) is not None and self.hears(0), c.gain_db)
        else:
            self.crop.update((0.0, 0.0), None, None)
            if s.sound is not None:
                self.player.set_sound(self.hears(s.sound_lane), s.sound.gain_db)

        shown = [s.lane_items(n) for n in range(LANES)]
        ref = s.sound_ref
        self.timeline.update(
            [(Path(s.path(i)).stem, float(k.duration)) for i, k in enumerate(s.clips)], s.selected,
            [[(Path(s.project.media[it.media]).stem, float(it.at), float(it.end - it.start), linked) for _, it, linked in lane]
             for lane in shown],
            next(((n, i) for n, lane in enumerate(shown) for i, item in enumerate(lane) if item[0] == ref), None),
            float(self.montage_pos()), [not self.hears(n) for n in range(LANES)])
        self._show_time()
        name = Path(s.file).stem if s.file else None
        self.set_title(f"{'• ' if s.dirty and s.clips else ''}{name} — {APP_NAME}" if name else APP_NAME)
        self._syncing = False

    def _describe(self, p: ExportPlan | None) -> str:
        if p is None:
            return ""
        if p.kind == "copy":
            extra = float(p.duration - self.s.total)
            where = f"cuts move out to keyframes, +{extra:.1f} s" if extra > 0.0005 else "cuts are exact"
            return f"Lossless copy · {clock(p.duration)} · {where}"
        v = p.video
        kind = {"sdr": "standard range", "hlg": "HDR", "pq": "HDR"}[v.color]
        text = f"Re-encode · {clock(p.duration)} · {v.width}×{v.height} · {float(v.fps):g} fps · {kind}"
        if ffmpeg_backend.uses_gpu_encoder(p, self._caps):
            text += " · GPU encoder"
        if p.reasons and not self.s.exact:
            more = f" (+{len(p.reasons) - 1} more)" if len(p.reasons) > 1 else ""
            text += f" · not lossless: {p.reasons[0]}{more}"
        return text

    def _show_time(self):
        if self.s.target is not None and self.pos_entry is not self._editing():
            self.pos_entry.set_text(clock(self.pos))

    # ------------------------------------------------------------ player -> window

    def _on_time(self, t: float):
        s, target = self.s, self.s.target
        if target is None or self._dragging or self._detached:
            return
        self.pos = t
        self.trim.set_pos(t, follow=self.player.playing)
        self.timeline.set_pos(float(self.montage_pos()))
        self._show_time()
        if self.player.playing and self._last_pos < float(target.end) <= t:
            if s.sound is None and s.selected + 1 < len(s.clips):
                # Montage playback: on reaching this clip's out point, carry on into the next clip.
                self.select(s.selected + 1, playing=True)
                return
            self.player.set_playing(False)
        self._last_pos = t

    def _on_pause(self, paused: bool):
        self.play_btn.set_icon_name("media-playback-start-symbolic" if paused else "media-playback-pause-symbolic")

    def _sync_lanes(self):
        """Keep the sound lanes playing in step with the montage (a timer calls this)."""
        s = self.s
        active = s.clip is not None and s.sound is None and self.player.playing and not self._detached
        at = self.montage_pos() if active else None
        for n, lane_player in enumerate(self.lane_players):
            hit = s.sound_at(n, at) if active and self.hears(n) else None
            if hit:
                item, source_time = hit
                lane_player.sync(s.project.media[item.media], float(source_time), s.lanes[n].gain_db + item.gain_db)
            else:
                lane_player.sync(None)
        return True

    # ------------------------------------------------------------ moving around

    def _load(self, playing: bool = False):
        self._detached = False
        self._last_pos = self.pos
        self.player.load(self.s.target_path(), self.pos, playing)

    def select(self, i: int, pos: float | None = None, playing: bool = False):
        self.s.select(i)
        c = self.s.clip
        self.pos = float(c.start) if pos is None else pos
        self._load(playing)
        self.refresh()

    def _show_target(self):
        """After an edit that may have changed what is selected (undo, remove, open)."""
        t = self.s.target
        if t is None:
            self.player.unload()
            self.pos = 0.0
        else:
            self.pos = min(max(self.pos, 0.0), float(self.s.target_info().duration))
            self._load()
        self.refresh()

    def _back_to_clips(self):
        if self.s.sound is not None:
            self.s.clear_sound()
            self.pos = self._clip_pos
            self._show_target()

    def _reattach(self):
        """Put the preview back on the play position after it has been showing a cut point."""
        if self._detached:
            self._detached = False
            self.player.seek(self.pos, exact=True)

    def _preview(self, t: float):
        """Show the frame at t without moving the play position."""
        self._detached = True
        self.player.set_playing(False)
        self.player.seek(t, exact=True)

    def _seek(self, t: float, exact: bool = True):
        self._detached = False
        self.pos = self._last_pos = t
        self.player.seek(t, exact)
        self.trim.set_pos(t)
        self.timeline.set_pos(float(self.montage_pos()))
        self._show_time()

    def toggle_play(self):
        s, t = self.s, self.s.target
        if t is None:
            return
        if self.player.playing:
            self.player.set_playing(False)
            return
        self._reattach()
        at_end = abs(self.pos - float(t.end)) < float(s.frame()) * 1.5
        if at_end and s.sound is None and s.selected == len(s.clips) - 1:
            self.select(0, playing=True)  # at the very end: play the montage again
        elif at_end and s.sound is not None:
            self._seek(float(t.start))
            self.player.set_playing(True)
        else:
            self.player.set_playing(True)

    # ------------------------------------------------------------ editing

    def _begin_gesture(self):
        if not self._gesture:
            self.s.checkpoint()
            self._gesture = True

    def _on_scrub(self, t: float, final: bool):
        self._dragging = not final
        self.player.set_playing(False)
        self._seek(t, exact=final)  # keyframes while dragging, the exact frame on release

    def _on_montage_scrub(self, t: float, final: bool):
        """The play line on the montage is dragged: move through the clips."""
        s = self.s
        if not s.clips:
            return
        self._dragging = not final
        self.player.set_playing(False)
        i, source_time = s.locate(floor_us(t))
        if s.sound is not None or i != s.selected:
            self.select(i, float(source_time))
        else:
            self._seek(float(source_time), exact=final)

    def _cut_frame(self, which: str) -> float:
        """The frame to show for a cut point: the first frame kept, or the last frame kept."""
        t = self.s.target
        return float(t.start) if which == "in" else max(float(t.end - self.s.frame()), float(t.start))

    def _on_trim(self, which: str, t: float, final: bool):
        self._begin_gesture()
        (self.s.set_in if which == "in" else self.s.set_out)(floor_us(t))
        self._preview(self._cut_frame(which))  # the play position stays where it is
        if final:
            self._gesture = False
        self.refresh()

    def _time_typed(self, entry: Gtk.Entry, which: str):
        t = parse_clock(entry.get_text())
        self.set_focus(None)
        if t is not None and self.s.target is not None:
            if which == "pos":
                self.player.set_playing(False)
                self._seek(float(min(t, self.s.target_info().duration)))
            else:
                self.s.checkpoint()
                (self.s.set_in if which == "in" else self.s.set_out)(t)
                self._preview(self._cut_frame(which))
        self.refresh()

    def _on_crop(self, rect: Rect, final: bool):
        self._begin_gesture()
        self.s.set_crop(rect)
        if final:
            self._gesture = False
        self.refresh()

    def _on_clip_click(self, i: int, fraction: float):
        c = self.s.clips[i]
        self.select(i, float(c.start) + fraction * float(c.duration))

    def _on_clip_move(self, src: int, dst: int, final: bool):
        if final:
            self._gesture = False
            return
        self._begin_gesture()
        self.s.clear_sound()
        self.s.move(src, dst)
        self.refresh()

    def _on_sound_press(self, lane: int, i: int):
        s = self.s
        ref = s.lane_items(lane)[i][0]
        if s.sound_ref == ref:
            return
        if ref[0] == OWN and ref[1] != s.selected:
            self._clip_pos = float(s.clips[ref[1]].start)  # another clip's own sound: Escape goes back to that clip
        elif s.sound is None:
            self._clip_pos = self.pos
        s.select_sound(ref)
        self.pos = float(self.s.sound.start)
        self._load()
        self.refresh()

    def _on_sound_move(self, at: float | None, lane: int | None, final: bool):
        if final:
            self._gesture = False
            return
        self._begin_gesture()
        self.s.move_sound(floor_us(at), lane)
        self.refresh()

    def _shape_toggled(self, button, value):
        if self._syncing or not button.get_active():
            return
        self.s.set_shape(value)
        self.refresh()

    def hears(self, lane: int) -> bool:
        """Whether a sound lane is heard in the preview. The export always has every lane."""
        return self.hear[lane].get_active()

    def _hear_toggled(self, _button):
        self.refresh()
        self._sync_lanes()

    def _volume_changed(self, scale):
        if self._syncing or self.s.target is None:
            return
        now = time.monotonic()
        if now - self._volume_edit > 1.0:  # one undo step per adjustment, not per pixel of movement
            self.s.checkpoint()
        self._volume_edit = now
        self.s.set_gain(percent_to_gain(scale.get_value()))
        self.refresh()

    def _exact_toggled(self, button):
        self.s.exact = button.get_active()
        self.refresh()

    def _key(self, _ctrl, keyval, _code, state):
        s, t = self.s, self.s.target
        ctrl = bool(state & Gdk.ModifierType.CONTROL_MASK)
        shift = bool(state & Gdk.ModifierType.SHIFT_MASK)
        name = Gdk.keyval_name(keyval) or ""
        low = name.lower()
        if self._editing() is not None:  # typing a time: only Escape is ours
            if name == "Escape":
                self.set_focus(None)
                self.refresh()
                return True
            return False
        if ctrl and low == "z":
            if s.redo() if shift else s.undo():
                self._show_target()
            return True
        if ctrl and low == "o":
            self._choose_files()
            return True
        if ctrl and low == "e":
            self._export()
            return True
        if ctrl and low == "s":
            self.save(ask=shift)
            return True
        if t is None or ctrl:
            return False
        if name == "space":
            self.toggle_play()
        elif name in ("Left", "Right"):
            self.player.set_playing(False)
            self._reattach()
            if shift:
                step = 1.0 if name == "Right" else -1.0
                self._seek(min(max(self.pos + step, 0.0), float(s.target_info().duration)))
            else:
                self.player.step(name == "Right")
        elif low == "i":
            s.checkpoint()
            s.set_in(floor_us(self.pos))
            self.refresh()
        elif low == "o":
            s.checkpoint()
            s.set_out(floor_us(self.pos) + s.frame())  # the frame on screen is the last one kept
            self.refresh()
        elif low == "s":
            if s.split(floor_us(self.pos)):
                self.refresh()
        elif name in ("Delete", "BackSpace"):
            if s.sound is not None:
                s.remove_sound()
                self.pos = self._clip_pos
            else:
                s.remove(s.selected)
            self._show_target()
        elif name == "Escape":
            self._back_to_clips()
        elif name == "Home":
            self._seek(float(t.start))
        elif name == "End":
            self._seek(self._cut_frame("out"))
        else:
            return False
        return True

    # ------------------------------------------------------------ adding files and projects

    def _choose_files(self, *_):
        Gtk.FileDialog(title="Add video files or open a project").open_multiple(self, None, self._files_chosen, self.open_paths)

    def _choose_sound(self, *_):
        Gtk.FileDialog(title="Add sound from files").open_multiple(self, None, self._files_chosen, self.add_sounds)

    def _choose_project(self, *_):
        Gtk.FileDialog(title="Open project").open_multiple(self, None, self._files_chosen, self.open_paths)

    def _files_chosen(self, dialog, result, then):
        try:
            files = dialog.open_multiple_finish(result)
        except GLib.Error:
            return  # dismissed
        then([f.get_path() for f in files if f.get_path()])

    def _dropped(self, _target, value, _x, _y):
        self.open_paths([f.get_path() for f in value.get_files() if f.get_path()])
        return True

    def open_paths(self, paths):
        """Projects are opened; video files become clips; sound-only files go to a sound lane."""
        projects = [p for p in paths if p.lower().endswith((PROJECT_EXT, ".json"))]
        if projects:
            self.open_project(projects[0])
        self.add_files([p for p in paths if p not in projects])

    def add_files(self, paths, as_sound: bool = False):
        if not paths:
            return

        def work():
            for path in paths:  # one at a time, so clips arrive in the order given
                try:
                    quick = probe(path, keyframes=False)
                except ProbeError as e:
                    GLib.idle_add(self.status.set_text, f"Could not open {Path(path).name}: {e}")
                    continue
                GLib.idle_add(self._added, path, quick, as_sound)
                if quick.video is None or as_sound:
                    continue
                try:
                    full = probe(path)  # keyframe index; slower on big files, needed for lossless export
                except ProbeError:
                    continue
                GLib.idle_add(self._indexed, path, full)

        threading.Thread(target=work, daemon=True).start()

    def add_sounds(self, paths):
        self.add_files(paths, as_sound=True)

    def _added(self, path, info, as_sound):
        s = self.s
        if as_sound or info.video is None:
            if info.audio is None:
                self.status.set_text(f"{Path(path).name} has no sound")
            elif s.clip is None:
                self.status.set_text("Add a video first; sound goes under the clips")
            else:
                if s.sound is None:
                    self._clip_pos = self.pos
                s.add_sound(path, info, self.montage_pos())
                self.pos = 0.0
                self._load()
                self.refresh()
            return
        self.select(s.add_media(path, info))

    def _indexed(self, path, info):
        self.s.set_info(path, info)
        self.refresh()

    def open_project(self, file):
        def work():
            try:
                project = Session.read(file)
                infos = {k: probe(v) for k, v in project.media.items()}
            except (OSError, ValueError, ProjectError, ProbeError) as e:
                GLib.idle_add(self.status.set_text, f"Could not open {Path(file).name}: {e}")
                return
            GLib.idle_add(done, project, infos)

        def done(project, infos):
            self.s.open(file, project, infos)
            if Path(file) == autosave_file():
                self.s.file = None  # a restored session is not a named project
            self._kf_cache.clear()
            self.pos = float(self.s.clip.start)
            self._show_target()

        threading.Thread(target=work, daemon=True).start()

    def save(self, ask: bool = False):
        if not self.s.clips:
            return
        if self.s.file and not ask:
            self._save_to(self.s.file)
            return
        first = Path(self.s.path(0))
        dialog = Gtk.FileDialog(title="Save project", initial_name=first.stem + PROJECT_EXT,
                                initial_folder=Gio.File.new_for_path(str(first.parent)))
        dialog.save(self, None, self._save_chosen)

    def _save_chosen(self, dialog, result):
        try:
            path = dialog.save_finish(result).get_path()
        except GLib.Error:
            return
        if path:
            self._save_to(path if path.lower().endswith(PROJECT_EXT) else path + PROJECT_EXT)

    def _save_to(self, path):
        try:
            self.s.save(path)
        except OSError as e:
            self.status.set_text(f"Could not save: {e}")
            return
        self.refresh()
        self.status.set_text(f"Saved project {path}")

    def restore(self):
        if autosave_file().exists():
            self.open_project(str(autosave_file()))
        else:
            self.status.set_text("No earlier session to reopen")

    def _autosave(self):
        """Keep the last session, so closing the window never loses an edit."""
        s = self.s
        if not s.clips:
            return
        named, saved = s.file, s._saved
        try:
            autosave_file().parent.mkdir(parents=True, exist_ok=True)
            s.save(str(autosave_file()))
        except OSError:
            pass
        s.file, s._saved = named, saved  # autosaving is not saving the named project

    # ------------------------------------------------------------ export

    def _export(self, *_):
        if self._plan is None or self._cancel is not None:
            return
        suggested = Path(self.s.default_output(self._plan))
        dialog = Gtk.FileDialog(title="Export", initial_name=suggested.name,
                                initial_folder=Gio.File.new_for_path(str(suggested.parent)))
        dialog.save(self, None, self._export_to)

    def _export_to(self, dialog, result):
        try:
            out = dialog.save_finish(result).get_path()
        except GLib.Error:
            return  # dismissed
        s = self.s
        if out is None or str(Path(out).resolve()) in s.project.media.values():
            self.status.set_text("Cannot export over a source file. Choose another name.")
            return
        try:
            plan = s.plan()
        except PlanError as e:
            self.status.set_text(f"Cannot export: {e}")
            return
        paths = dict(s.project.media)
        self._cancel = cancel = threading.Event()
        self.player.set_playing(False)
        self.export_btn.set_sensitive(False)
        self.progress.set_fraction(0.0)
        self.progress.set_visible(True)
        self.cancel_btn.set_visible(True)
        self.status.set_text(f"Exporting {Path(out).name}…")
        started = time.monotonic()

        def work():
            try:
                ffmpeg_backend.run(plan, paths, out, lambda f: GLib.idle_add(self.progress.set_fraction, f), cancel)
                message = f"Saved {out} ({time.monotonic() - started:.1f} s)"
            except ffmpeg_backend.ExportCancelled:
                message = "Export cancelled"
            except (ffmpeg_backend.ExportError, OSError) as e:
                message = f"Export failed: {str(e).splitlines()[-1] if str(e) else e!r}"
            GLib.idle_add(self._export_done, message)

        threading.Thread(target=work, daemon=True).start()

    def _export_done(self, message):
        self._cancel = None
        self.progress.set_visible(False)
        self.cancel_btn.set_visible(False)
        self.export_btn.set_sensitive(self._plan is not None)
        self.status.set_text(message)
        self.status.set_tooltip_text(message)

    def _closing(self, *_):
        if self._cancel:
            self._cancel.set()
        self._autosave()
        for lane_player in self.lane_players:
            lane_player.close()
        return False


def main(files: list[str] | None = None) -> int:
    files = [os.path.abspath(f) for f in (sys.argv[1:] if files is None else files)]
    app = Gtk.Application(application_id="app.plainvideo.Editor", flags=Gio.ApplicationFlags.NON_UNIQUE)
    app.connect("activate", lambda a: Window(a, files).present())
    return app.run([])


if __name__ == "__main__":
    sys.exit(main())
