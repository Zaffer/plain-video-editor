"""Custom-drawn widgets: trim bar, montage timeline, crop overlay.

Each widget is told what to show with update(...) and reports gestures through
callbacks. None of them changes the session itself.

Drawing uses GTK's own snapshot calls (rectangles and text), so it needs no cairo bindings.
"""
from __future__ import annotations

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
gi.require_version("Graphene", "1.0")
from gi.repository import Gdk, Graphene, Gtk  # noqa: E402

from ..model import Rect  # noqa: E402

ACCENT = (0.21, 0.52, 0.89)
SOUND = (0.18, 0.62, 0.45)
PLAYHEAD = (0.9, 0.2, 0.2)
PAD = 14  # left/right margin of the bars
GRAB = 6  # how close (px) to a line counts as grabbing it
KNOB = 12  # knob size


def _rgba(r, g, b, a=1.0):
    c = Gdk.RGBA()
    c.red, c.green, c.blue, c.alpha = r, g, b, a
    return c


def _fg(widget, alpha=1.0):
    c = widget.get_color()
    return _rgba(c.red, c.green, c.blue, c.alpha * alpha)


def _fill(snapshot, color, x, y, w, h):
    if w > 0 and h > 0:
        snapshot.append_color(color, Graphene.Rect().init(x, y, w, h))


def _outline(snapshot, color, x, y, w, h, t=1.5):
    _fill(snapshot, color, x, y, w, t)
    _fill(snapshot, color, x, y + h - t, w, t)
    _fill(snapshot, color, x, y, t, h)
    _fill(snapshot, color, x + w - t, y, t, h)


def _text(widget, snapshot, text, color, x, y_mid, clip=None):
    layout = widget.create_pango_layout(text)
    if clip:
        snapshot.push_clip(Graphene.Rect().init(*clip))
    snapshot.save()
    snapshot.translate(Graphene.Point().init(x, y_mid - layout.get_pixel_size()[1] / 2))
    snapshot.append_layout(layout, color)
    snapshot.restore()
    if clip:
        snapshot.pop()


class TrimBar(Gtk.Widget):
    """The source of the thing being trimmed, in three bands.

      top     the play position: a knob to drag, and clicking here moves it
      middle  the source, with the kept range, keyframe ticks, and the three lines
      bottom  knobs for the in and out points

    Clicking the middle band away from a line does nothing, so a missed grab
    never moves anything. The scroll wheel zooms around the pointer;
    Shift+scroll pans.

    on_scrub(t, final)        the play position is dragged or clicked
    on_trim(which, t, final)  the "in" or "out" point is dragged
    """

    TOP, BOTTOM = 18, 18  # heights of the top and bottom bands
    MIN_SPAN = 2.0  # most zoomed-in view, seconds

    def __init__(self, on_scrub, on_trim):
        super().__init__(hexpand=True, height_request=66)
        self._on_scrub, self._on_trim = on_scrub, on_trim
        self.duration = 0.0
        self.start = self.end = self.pos = 0.0
        self.keyframes: tuple[float, ...] = ()
        self.snapped: tuple[float, float] | None = None  # where a lossless export would really cut
        self.view = (0.0, 0.0)  # visible part of the source
        self._key = None  # identifies what is shown, to reset the zoom when it changes
        self._drag: str | None = None
        self._x0 = 0.0
        self._pointer = (0.0, 0.0)

        g = Gtk.GestureDrag()
        g.connect("drag-begin", self._begin)
        g.connect("drag-update", lambda g, dx, dy: self._move(self._x0 + dx, False))
        g.connect("drag-end", lambda g, dx, dy: self._move(self._x0 + dx, True))
        self.add_controller(g)
        m = Gtk.EventControllerMotion()
        m.connect("motion", self._motion)
        self.add_controller(m)
        sc = Gtk.EventControllerScroll.new(Gtk.EventControllerScrollFlags.VERTICAL)
        sc.connect("scroll", self._scroll)
        self.add_controller(sc)

    def update(self, key, duration, start, end, pos, keyframes, snapped):
        if key != self._key or duration != self.duration:
            self._key, self.view = key, (0.0, duration)
        self.duration, self.start, self.end, self.pos = duration, start, end, pos
        self.keyframes, self.snapped = keyframes, snapped
        self.queue_draw()

    def set_pos(self, pos, follow=False):
        """Move the play position. follow=True scrolls a zoomed view to keep it visible (during playback)."""
        self.pos = pos
        v0, v1 = self.view
        if follow and self._drag is None and not (v0 <= pos <= v1) and v1 - v0 < self.duration:
            span = v1 - v0
            v0 = min(max(pos - span * 0.1, 0.0), self.duration - span)
            self.view = (v0, v0 + span)
        self.queue_draw()

    # ------------------------------------------------------------ geometry

    def _x(self, t):
        v0, v1 = self.view
        return PAD + (t - v0) / (v1 - v0) * (self.get_width() - 2 * PAD) if v1 > v0 else PAD

    def _t(self, x):
        v0, v1 = self.view
        w = self.get_width() - 2 * PAD
        return v0 + min(max((x - PAD) / w, 0.0), 1.0) * (v1 - v0) if w > 0 else v0

    def hit(self, x, y):
        """What a press at (x, y) grabs: "play", "in", "out" or None."""
        if self.duration <= 0:
            return None
        h = self.get_height()
        xp, xi, xo = self._x(self.pos), self._x(self.start), self._x(self.end)
        if y < self.TOP:
            return "play"  # anywhere in the top band moves the play position
        if y >= h - self.BOTTOM:
            # knobs hang outside the kept range: in to the left of its line, out to the right
            if xi - KNOB - 3 <= x <= xi + 3:
                return "in"
            if xo - 3 <= x <= xo + KNOB + 3:
                return "out"
            return None
        d_in, d_out, d_play = abs(x - xi), abs(x - xo), abs(x - xp)
        trim = None
        if min(d_in, d_out) <= GRAB:
            trim = "in" if (d_in < d_out or (d_in == d_out and x < xi)) else "out"
        if d_play <= GRAB and (trim is None or y < h / 2):
            return "play"  # where lines coincide, the upper half is the play line
        return trim

    # ------------------------------------------------------------ input

    def _motion(self, _c, x, y):
        self._pointer = (x, y)
        if self._drag is None:
            what = self.hit(x, y)
            near_knob = abs(x - self._x(self.pos)) <= KNOB
            name = {"in": "ew-resize", "out": "ew-resize"}.get(what) or (
                ("grab" if (y >= self.TOP or near_knob) else "pointer") if what == "play" else None)
            self.set_cursor_from_name(name)

    def _begin(self, _g, x, y):
        self._x0 = x
        self._drag = self.hit(x, y)
        if self._drag == "play":
            self.set_cursor_from_name("grabbing")
        self._move(x, False)

    def _move(self, x, final):
        if self._drag is None:
            return
        t = self._t(x)
        if self._drag == "play":
            self._on_scrub(t, final)
        else:
            self._on_trim(self._drag, t, final)
        if final:
            self._drag = None
            self._motion(None, *self._pointer)

    def _scroll(self, ctrl, _dx, dy):
        if self.duration <= 0:
            return False
        v0, v1 = self.view
        span = v1 - v0
        if ctrl.get_current_event_state() & Gdk.ModifierType.SHIFT_MASK:
            v0 = min(max(v0 + dy * span * 0.15, 0.0), self.duration - span)
            self.view = (v0, v0 + span)
        else:
            at = self._t(self._pointer[0])  # zoom around the pointer
            new = min(max(span * (1.25 ** dy), min(self.MIN_SPAN, self.duration)), self.duration)
            v0 = min(max(at - (at - v0) * new / span, 0.0), self.duration - new)
            self.view = (v0, v0 + new)
        self.queue_draw()
        return True

    # ------------------------------------------------------------ drawing

    def do_snapshot(self, snap):
        w, h = self.get_width(), self.get_height()
        if self.duration <= 0:
            return
        top, bot = self.TOP, h - self.BOTTOM
        v0, v1 = self.view
        left, right = PAD, w - PAD
        snap.push_clip(Graphene.Rect().init(left - KNOB - 2, 0, right - left + 2 * KNOB + 4, h))

        def span(a, b):
            """Clamp a time range to the visible part of the bar, as (x, width)."""
            xa, xb = max(self._x(a), left), min(self._x(b), right)
            return xa, xb - xa

        _fill(snap, _fg(self, 0.12), left, top, right - left, bot - top)  # the source
        if self.snapped:  # what a lossless export would include beyond the kept range
            for a, b in ((self.snapped[0], self.start), (self.end, self.snapped[1])):
                x, cw = span(a, b)
                _fill(snap, _rgba(*ACCENT, 0.28), x, top, cw, bot - top)
        x, cw = span(self.start, self.end)
        _fill(snap, _rgba(*ACCENT, 0.75), x, top, max(cw, 1) if cw > -1 else 0, bot - top)  # kept range

        # keyframe ticks, when not too dense to read
        visible = [k for k in self.keyframes if v0 <= k <= v1] if self.keyframes else []
        if visible and (right - left) / len(visible) >= 3:
            tick = _fg(self, 0.6)
            for k in visible:
                _fill(snap, tick, round(self._x(k)), bot - 5, 1, 5)

        # in and out: a line through the source and a knob below, hanging outside the kept range
        fg = _fg(self)
        for t, side in ((self.start, -1), (self.end, 1)):
            if v0 <= t <= v1:
                x = self._x(t)
                _fill(snap, fg, x - 1, top - 2, 2, bot - top + 4)
                _fill(snap, fg, x - KNOB if side < 0 else x, bot + 2, KNOB, KNOB)
        # play position: a line and a knob above
        if v0 <= self.pos <= v1:
            x = self._x(self.pos)
            red = _rgba(*PLAYHEAD)
            _fill(snap, red, x - 1, top - 2, 2, bot - top + 4)
            _fill(snap, red, x - KNOB / 2, 2, KNOB, KNOB)
        snap.pop()
        # when zoomed in: a thin strip showing which part of the source is visible
        if v1 - v0 < self.duration:
            _fill(snap, _fg(self, 0.15), left, h - 2, right - left, 2)
            _fill(snap, _fg(self, 0.7), left + v0 / self.duration * (right - left), h - 2,
                  max((v1 - v0) / self.duration * (right - left), 3), 2)


class Timeline(Gtk.Widget):
    """The montage: clips in order (widths in proportion to their length) and the sound lanes under them.

    on_select(index, fraction)        a clip is clicked: which, and how far along it (0..1)
    on_move(src, dst, final)          a clip is dragged to a new position
    on_sound_select(lane, index)      a sound item is pressed
    on_sound_move(at, lane, final)    the selected sound item is dragged (montage seconds, lane)
    """

    CLIP_Y, CLIP_H = 4, 30
    LANE_Y, LANE_H, LANE_GAP = 38, 18, 3

    def __init__(self, lanes, on_select, on_move, on_sound_select, on_sound_move):
        self.n_lanes = lanes
        super().__init__(hexpand=True, height_request=self.LANE_Y + lanes * (self.LANE_H + self.LANE_GAP) + 2)
        self._on_select, self._on_move = on_select, on_move
        self._on_sound_select, self._on_sound_move = on_sound_select, on_sound_move
        self.clips: list[tuple[str, float]] = []  # (label, seconds)
        self.sounds: list[list[tuple[str, float, float]]] = []  # per lane: (label, at, seconds)
        self.selected = -1
        self.sel_sound: tuple[int, int] | None = None
        self.pos = 0.0  # montage time of the playhead
        self._drag = None
        self._moved = False
        self._x0 = self._y0 = 0.0
        g = Gtk.GestureDrag()
        g.connect("drag-begin", self._begin)
        g.connect("drag-update", self._update)
        g.connect("drag-end", self._end)
        self.add_controller(g)
        m = Gtk.EventControllerMotion()
        m.connect("motion", lambda _c, x, y: self.set_cursor_from_name(
            "grab" if self._drag is None and self._sound_hit(x, y) else None))
        self.add_controller(m)

    def update(self, clips, selected, sounds, sel_sound, pos):
        self.clips, self.selected, self.sounds, self.sel_sound, self.pos = clips, selected, sounds, sel_sound, pos
        self.queue_draw()

    def set_pos(self, pos):
        self.pos = pos
        self.queue_draw()

    # ------------------------------------------------------------ geometry

    @property
    def total(self):
        return sum(d for _, d in self.clips)

    def _x(self, t):
        return PAD + t / self.total * (self.get_width() - 2 * PAD) if self.total > 0 else PAD

    def _t(self, x):
        w = self.get_width() - 2 * PAD
        return (x - PAD) / w * self.total if w > 0 else 0.0

    def _spans(self):
        """[(x, width)] per clip."""
        out, t = [], 0.0
        for _, d in self.clips:
            out.append((self._x(t), self._x(t + d) - self._x(t)))
            t += d
        return out if self.total > 0 else []

    def _clip_hit(self, x, y):
        if not (self.CLIP_Y <= y < self.CLIP_Y + self.CLIP_H):
            return None
        for i, (cx, cw) in enumerate(self._spans()):
            if cx <= x < cx + cw:
                return i, (x - cx) / cw
        return None

    def _lane_at(self, y):
        n = int((y - self.LANE_Y) // (self.LANE_H + self.LANE_GAP))
        return n if 0 <= n < self.n_lanes and y >= self.LANE_Y else None

    def _sound_hit(self, x, y):
        lane = self._lane_at(y)
        if lane is None or lane >= len(self.sounds) or self.total <= 0:
            return None
        for i in reversed(range(len(self.sounds[lane]))):
            _, at, d = self.sounds[lane][i]
            if self._x(at) <= x < max(self._x(at + d), self._x(at) + 4):
                return lane, i
        return None

    # ------------------------------------------------------------ input

    def _begin(self, _g, x, y):
        self._x0, self._y0, self._moved = x, y, False
        hit = self._sound_hit(x, y)
        if hit:
            lane, i = hit
            self._drag = ("sound", self._t(x) - self.sounds[lane][i][1])  # where on the item it was grabbed
            self._on_sound_select(lane, i)
            self.set_cursor_from_name("grabbing")
            return
        hit = self._clip_hit(x, y)
        self._drag = ("clip", hit[0]) if hit else None

    def _update(self, _g, dx, dy):
        if self._drag is None or (abs(dx) < 6 and abs(dy) < 6 and not self._moved):
            return
        x, y = self._x0 + dx, self._y0 + dy
        if self._drag[0] == "sound":
            self._moved = True
            self._on_sound_move(max(self._t(x) - self._drag[1], 0.0), self._lane_at(y), False)
        else:
            hit = self._clip_hit(x, self.CLIP_Y + 1)
            if hit and hit[0] != self._drag[1]:
                self._on_move(self._drag[1], hit[0], False)
                self._drag, self._moved = ("clip", hit[0]), True

    def _end(self, _g, dx, dy):
        if self._drag is None:
            return
        kind = self._drag[0]
        if self._moved:
            if kind == "sound":
                self._on_sound_move(None, None, True)
            else:
                self._on_move(self._drag[1], self._drag[1], True)
        elif kind == "clip":
            hit = self._clip_hit(self._x0, self._y0)
            if hit:
                self._on_select(*hit)
        self._drag = None
        self.set_cursor_from_name(None)

    # ------------------------------------------------------------ drawing

    def do_snapshot(self, snap):
        w, h = self.get_width(), self.get_height()
        clip_mode = self.sel_sound is None
        for i, ((x, cw), (label, secs)) in enumerate(zip(self._spans(), self.clips)):
            sel = i == self.selected
            box = (x + 1, self.CLIP_Y, max(cw - 2, 1), self.CLIP_H)
            _fill(snap, _rgba(*ACCENT, 0.75 if clip_mode else 0.35) if sel else _fg(self, 0.14), *box)
            _text(self, snap, f"{label}  {secs:.1f}s", _rgba(1, 1, 1) if sel else _fg(self),
                  x + 8, self.CLIP_Y + self.CLIP_H / 2, clip=box)
        for lane in range(self.n_lanes):
            y = self.LANE_Y + lane * (self.LANE_H + self.LANE_GAP)
            _fill(snap, _fg(self, 0.06), PAD, y, w - 2 * PAD, self.LANE_H)
            items = self.sounds[lane] if lane < len(self.sounds) else []
            if not items and lane == 0 and not any(self.sounds):
                _text(self, snap, "sound: press Add sound, or drop a sound file", _fg(self, 0.4), PAD + 8, y + self.LANE_H / 2)
            snap.push_clip(Graphene.Rect().init(PAD, y, w - 2 * PAD, self.LANE_H))
            for i, (label, at, d) in enumerate(items):
                sel = self.sel_sound == (lane, i)
                box = (self._x(at), y, max(self._x(at + d) - self._x(at), 4), self.LANE_H)
                _fill(snap, _rgba(*SOUND, 0.95 if sel else 0.5), *box)
                if sel:
                    _outline(snap, _rgba(1, 1, 1, 0.9), *box, t=1)
                _text(self, snap, label, _rgba(1, 1, 1), box[0] + 6, y + self.LANE_H / 2, clip=box)
            snap.pop()
        if self.total > 0:
            _fill(snap, _rgba(*PLAYHEAD), min(self._x(self.pos), w - PAD) - 1, 0, 2, h)


class CropOverlay(Gtk.Widget):
    """Crop rectangle drawn over the preview; everything outside it is dimmed.

    Coordinates are display pixels of the video, as in the project.
    on_change(rect, final) while dragging the rectangle or a corner.
    """

    def __init__(self, on_change):
        super().__init__(hexpand=True, vexpand=True)
        self._on_change = on_change
        self.video = (0.0, 0.0)  # display size of the video
        self.rect: Rect | None = None
        self.aspect: float | None = None  # w/h to hold, or None
        self._mode: str | None = None
        self._start: tuple[float, float] = (0, 0)
        self._orig: Rect | None = None
        self.set_can_target(False)
        g = Gtk.GestureDrag()
        g.connect("drag-begin", self._begin)
        g.connect("drag-update", lambda g, dx, dy: self._move(dx, dy, False))
        g.connect("drag-end", lambda g, dx, dy: self._move(dx, dy, True))
        self.add_controller(g)
        m = Gtk.EventControllerMotion()
        m.connect("motion", lambda _c, x, y: self._mode is None and self.set_cursor_from_name(
            {"nw": "nwse-resize", "se": "nwse-resize", "ne": "nesw-resize", "sw": "nesw-resize", "move": "move"}
            .get(self.hit(x, y))))
        self.add_controller(m)

    def update(self, video, rect, aspect):
        self.video, self.rect, self.aspect = video, rect, aspect
        self.set_can_target(rect is not None)  # with no crop, clicks fall through
        self.queue_draw()

    def _area(self):
        """Where mpv draws the video inside this widget: (x, y, scale)."""
        vw, vh = self.video
        w, h = self.get_width(), self.get_height()
        if vw <= 0 or vh <= 0 or w <= 0 or h <= 0:
            return 0.0, 0.0, 0.0
        s = min(w / vw, h / vh)
        return (w - vw * s) / 2, (h - vh * s) / 2, s

    def hit(self, x, y):
        """What a press at widget (x, y) grabs: a corner ("nw" etc.), "move", or None."""
        ox, oy, s = self._area()
        if self.rect is None or s <= 0:
            return None
        r = self.rect
        vx, vy = (x - ox) / s, (y - oy) / s
        grab = 14 / s
        for name, cx, cy in (("nw", r.x, r.y), ("ne", r.x + r.w, r.y), ("sw", r.x, r.y + r.h), ("se", r.x + r.w, r.y + r.h)):
            if abs(vx - cx) <= grab and abs(vy - cy) <= grab:
                return name
        return "move" if (r.x <= vx <= r.x + r.w and r.y <= vy <= r.y + r.h) else None

    def _begin(self, _g, x, y):
        ox, oy, s = self._area()
        self._mode = self.hit(x, y)
        if self._mode:
            self._start, self._orig = ((x - ox) / s, (y - oy) / s), self.rect

    def _move(self, dx, dy, final):
        if self._mode is None:
            return
        _, _, s = self._area()
        if s <= 0:
            return
        vw, vh = self.video
        o = self._orig
        dx, dy = dx / s, dy / s
        if self._mode == "move":
            x = min(max(o.x + dx, 0), vw - o.w)
            y = min(max(o.y + dy, 0), vh - o.h)
            rect = Rect(int(x), int(y), o.w, o.h)
        else:
            # The corner opposite the one being dragged stays put.
            ax = o.x + o.w if "w" in self._mode else o.x
            ay = o.y + o.h if "n" in self._mode else o.y
            px = min(max(self._start[0] + dx, 0), vw)
            py = min(max(self._start[1] + dy, 0), vh)
            w, h = abs(px - ax), abs(py - ay)
            if self.aspect:
                room_w = ax if "w" in self._mode else vw - ax
                room_h = ay if "n" in self._mode else vh - ay
                w = min(max(w, h * self.aspect), room_w, room_h * self.aspect)
                h = w / self.aspect
            w, h = max(w, 16), max(h, 16)
            x = ax - w if "w" in self._mode else ax
            y = ay - h if "n" in self._mode else ay
            rect = Rect(int(round(x)), int(round(y)), int(round(w)), int(round(h)))
        self._on_change(rect, final)
        if final:
            self._mode = None

    def do_snapshot(self, snap):
        ox, oy, s = self._area()
        if self.rect is None or s <= 0:
            return
        r = self.rect
        x, y, rw, rh = ox + r.x * s, oy + r.y * s, r.w * s, r.h * s
        vw, vh = self.video[0] * s, self.video[1] * s
        # dim the part of the picture outside the crop: four bands around it
        dim = _rgba(0, 0, 0, 0.6)
        _fill(snap, dim, ox, oy, vw, y - oy)
        _fill(snap, dim, ox, y + rh, vw, oy + vh - (y + rh))
        _fill(snap, dim, ox, y, x - ox, rh)
        _fill(snap, dim, x + rw, y, ox + vw - (x + rw), rh)
        white = _rgba(1, 1, 1, 0.95)
        _outline(snap, white, x, y, rw, rh)
        for cx, cy in ((x, y), (x + rw, y), (x, y + rh), (x + rw, y + rh)):
            _fill(snap, white, cx - 5, cy - 5, 10, 10)
