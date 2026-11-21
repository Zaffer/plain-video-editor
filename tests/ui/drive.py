"""Scripted test of the editor window. Opens a real window and drives it.

    python3 tests/ui/drive.py OUT_PREFIX clip1.mp4 clip2.mp4

Gestures are simulated by calling the widgets' own press/drag/release handlers
with pixel coordinates, so hit-testing and geometry are exercised; real pointer
events are not. Writes OUT_PREFIX.png (a snapshot), .mp4 (an export), .json.
"""
import json
import os
import subprocess
import sys
import time
import traceback
from fractions import Fraction
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, GLib, Gtk  # noqa: E402

from pve.ui.app import Window  # noqa: E402

OUT = sys.argv[1]
FILES = [os.path.abspath(f) for f in sys.argv[2:]]
report = {"steps": [], "errors": []}
NONE = Gdk.ModifierType(0)


def check(name, cond, detail=""):
    report["steps"].append({"step": name, "ok": bool(cond), "detail": str(detail)})


def snapshot(win, path):
    p = Gtk.WidgetPaintable.new(win)
    snap = Gtk.Snapshot()
    p.snapshot(snap, win.get_width(), win.get_height())
    node = snap.to_node()
    if node is None:  # the window is hidden behind another one, so nothing is drawn
        return False
    win.get_renderer().render_texture(node, None).save_to_png(path)
    return True


def snapshot_widgets(win, widgets, path):
    """Draw our own widgets into a picture directly. Works even when the window is hidden."""
    gi.require_version("Graphene", "1.0")
    from gi.repository import Graphene

    width = max(wd.get_width() for wd in widgets)
    snap, y = Gtk.Snapshot(), 0
    bg = Gdk.RGBA()
    bg.parse("#242424")
    snap.append_color(bg, Graphene.Rect().init(0, 0, width, sum(wd.get_height() + 6 for wd in widgets)))
    for wd in widgets:
        snap.save()
        snap.translate(Graphene.Point().init(0, y))
        type(wd).do_snapshot(wd, snap)
        snap.restore()
        y += wd.get_height() + 6
    win.get_renderer().render_texture(snap.to_node(), None).save_to_png(path)


class FakeDialog:
    def __init__(self, path):
        self.path = path

    def save_finish(self, _):
        return Gio.File.new_for_path(self.path)


class FakeScroll:
    def get_current_event_state(self):
        return NONE


def cursor(widget):
    c = widget.get_cursor()
    return c.get_name() if c else None


def type_time(w, entry, text):
    entry.grab_focus()
    entry.set_text(text)
    entry.emit("activate")


def key(w, keyval, state=NONE):
    return w._key(None, keyval, 0, state)


def script(w):
    s, trim, tl = w.s, w.trim, w.timeline
    t0 = time.monotonic()
    while len(s.clips) < len(FILES) and time.monotonic() - t0 < 20:
        yield 0.2
    check("clips added", len(s.clips) == len(FILES), [c.media for c in s.clips])
    while any(s.infos[c.media].keyframes is None for c in s.clips) and time.monotonic() - t0 < 40:
        yield 0.2
    check("keyframes indexed", all(s.infos[c.media].keyframes is not None for c in s.clips))
    yield 1.0
    report["window"] = {"active": w.is_active(), "suspended": w.is_suspended(), "size": (w.get_width(), w.get_height())}

    # --- timeline: click clip 1
    spans = tl._spans()
    x0 = spans[0][0] + spans[0][1] / 2
    tl._begin(None, x0, tl.CLIP_Y + 5); tl._end(None, 0, 0)
    yield 0.6
    check("click selects clip 1", s.selected == 0 and s.sound is None, s.selected)
    dur = float(s.info().duration)

    # --- trim bar: a press in the middle band away from every line does nothing
    h = trim.get_height()
    mid_y = trim.TOP + (h - trim.TOP - trim.BOTTOM) / 2
    before = (w.pos, s.clip.start, s.clip.end, len(s._undo))
    x_free = trim._x(dur * 0.8)  # clear of the in, out and play lines
    check("middle band away from lines grabs nothing", trim.hit(x_free, mid_y) is None)
    trim._begin(None, x_free, mid_y); trim._move(x_free + 60, False); trim._move(x_free + 60, True)
    yield 0.2
    check("stray press changes nothing", before == (w.pos, s.clip.start, s.clip.end, len(s._undo)), before)

    # --- cursors
    trim._motion(None, trim._x(float(s.clip.start)) - 5, h - 8)
    c_in = cursor(trim)
    trim._motion(None, trim._x(w.pos), 6)
    c_play = cursor(trim)
    trim._motion(None, x_free, mid_y)
    c_none = cursor(trim)
    trim._motion(None, x_free, 6)
    c_ruler = cursor(trim)
    check("cursor changes over knobs and lines", (c_in, c_play, c_none, c_ruler) == ("ew-resize", "grab", None, "pointer"),
          (c_in, c_play, c_none, c_ruler))

    # --- drag the in knob (below the bar): the play position must not move
    pos_before = w.pos
    xi = trim._x(float(s.clip.start))
    check("in knob is grabbable below the bar", trim.hit(xi - 5, h - 8) == "in")
    trim._begin(None, xi - 5, h - 8)
    target_x = trim._x(5.0)
    trim._move(target_x, False); trim._move(target_x, True)
    yield 0.6
    check("in knob drag trims", abs(float(s.clip.start) - 5.0) < 0.05, float(s.clip.start))
    check("trimming leaves the play position alone", w.pos == pos_before and w._detached, (w.pos, w._detached))
    xo = trim._x(float(s.clip.end))
    check("out knob is grabbable below the bar", trim.hit(xo + 5, h - 8) == "out")
    check("lines are grabbable in the bar", trim.hit(xo, h - trim.BOTTOM - 3) == "out" and trim.hit(trim._x(5.0), mid_y + 8) == "in")

    # --- typed timestamps are exact
    type_time(w, w.in_entry, "0:05.000"); yield 0.3
    type_time(w, w.out_entry, "15"); yield 0.3
    check("typed in/out are exact", (s.clip.start, s.clip.end) == (5, 15), (s.clip.start, s.clip.end))
    check("entries show the times", (w.in_entry.get_text(), w.out_entry.get_text(), w.len_lbl.get_text()) == ("0:05.000", "0:15.000", "10.000 s"),
          (w.in_entry.get_text(), w.out_entry.get_text(), w.len_lbl.get_text()))
    type_time(w, w.out_entry, "nonsense"); yield 0.2
    check("a bad time is ignored", s.clip.end == 15 and w.out_entry.get_text() == "0:15.000", w.out_entry.get_text())

    # --- the top band moves the play position
    trim._begin(None, trim._x(8.0), 5); trim._move(trim._x(8.0), True)
    yield 0.8
    check("top band moves the play position", abs(w.pos - 8.0) < 0.06 and not w._detached, w.pos)
    type_time(w, w.pos_entry, "9.5"); yield 0.8
    check("typed play position", abs(w.pos - 9.5) < 0.04, w.pos)

    # --- zoom
    trim._pointer = (trim._x(9.5), mid_y)
    for _ in range(8):
        trim._scroll(FakeScroll(), 0, -1)
    v0, v1 = trim.view
    check("scroll zooms around the pointer", v1 - v0 < dur / 4 and v0 <= 9.5 <= v1, trim.view)
    trim._begin(None, trim._x(10.0), 5); trim._move(trim._x(10.0), True)
    yield 0.6
    check("zoomed bar still maps positions", abs(w.pos - 10.0) < 0.02, w.pos)
    for _ in range(12):
        trim._scroll(FakeScroll(), 0, 1)
    check("zooms back out fully", trim.view == (0.0, dur), trim.view)

    # --- clip 2
    spans = tl._spans()
    tl._begin(None, spans[1][0] + 5, tl.CLIP_Y + 5); tl._end(None, 0, 0)
    yield 0.6
    type_time(w, w.in_entry, "7"); type_time(w, w.out_entry, "17"); yield 0.4
    check("clip 2 trimmed by typing", s.selected == 1 and (s.clip.start, s.clip.end) == (7, 17))

    # --- crop: one shape for all, then drag with the overlay's own handlers
    w.shape_btns[Fraction(4, 3)].set_active(True); yield 0.4
    check("4:3 applied to all", all(c.crop_aspect == Fraction(4, 3) for c in s.clips), [c.crop for c in s.clips])
    ox, oy, sc = w.crop._area()
    r = s.clip.crop
    cx, cy = ox + (r.x + r.w / 2) * sc, oy + (r.y + r.h / 2) * sc
    check("crop hit-testing", w.crop.hit(cx, cy) == "move" and w.crop.hit(ox + r.x * sc, oy + r.y * sc) == "nw")
    w.crop._begin(None, cx, cy); w.crop._move(0, -40, False); w.crop._move(0, -40, True)
    yield 0.3
    check("crop drag moves the rectangle", s.clip.crop.y < r.y and (s.clip.crop.w, s.clip.crop.h) == (r.w, r.h), s.clip.crop)
    r = s.clip.crop
    w.crop._begin(None, ox + (r.x + r.w) * sc, oy + (r.y + r.h) * sc); w.crop._move(-60, -60, True)
    yield 0.3
    r2 = s.clip.crop
    check("corner drag keeps the shape", r2.w < r.w and abs(r2.w / r2.h - 4 / 3) < 0.01, r2)
    check("plan is an encode at clip 2's size", w._plan and w._plan.kind == "encode", w.status.get_text())

    # --- split and undo
    w._seek(10.0); yield 0.6
    check("split", key(w, Gdk.KEY_s) and len(s.clips) == 3 and s.clips[1].end == 10)
    key(w, Gdk.KEY_z, Gdk.ModifierType.CONTROL_MASK); yield 0.4
    check("undo split", len(s.clips) == 2 and s.clips[1].end == 17)

    # --- sound lane: clip 2's sound under clip 1
    tl._begin(None, tl._spans()[0][0] + 1, tl.CLIP_Y + 5); tl._end(None, 0, 0)
    yield 0.5
    w._seek(5.0); yield 0.4
    w.add_sounds([FILES[1]])
    t1 = time.monotonic()
    while s.sound is None and time.monotonic() - t1 < 8:
        yield 0.2
    check("sound added at the play position", s.sound is not None and s.sound.at == 0 and s.sel_audio == (0, 0), (s.sel_audio, s.sound and s.sound.at))
    type_time(w, w.in_entry, "7"); type_time(w, w.out_entry, "17"); yield 0.4
    check("sound item trimmed by typing", (s.sound.start, s.sound.end) == (7, 17))
    check("sound mode disables clip-only controls", not w.sound.get_sensitive() and w.crop.rect is None)
    key(w, Gdk.KEY_Escape); yield 0.5
    check("Escape returns to the clip", s.sound is None and s.selected == 0)
    check("plan mixes the lane and clip 2's own sound", w._plan and sorted(float(a.at) for a in w._plan.audio) == [0.0, 10.0],
          [str(a) for a in (w._plan.audio if w._plan else [])])

    # --- drag the sound item along the montage and to the other lane
    lane_y = tl.LANE_Y + 5
    xa = tl._x(1.0)
    check("sound item hit-testing", tl._sound_hit(xa, lane_y) == (0, 0), tl._sound_hit(xa, lane_y))
    tl._begin(None, xa, lane_y); tl._update(None, 60, 0); tl._end(None, 60, 0)
    yield 0.4
    moved = float(s.lanes[0].items[0].at)
    check("sound drag moves it in time", moved > 0.5 and s.sel_audio == (0, 0), moved)
    xa = tl._x(moved + 1.0)
    tl._begin(None, xa, lane_y); tl._update(None, 0, tl.LANE_H + tl.LANE_GAP); tl._end(None, 0, tl.LANE_H + tl.LANE_GAP)
    yield 0.4
    check("sound drag changes lane", s.sel_audio == (1, 0) and not s.lanes[0].items, s.sel_audio)
    key(w, Gdk.KEY_z, Gdk.ModifierType.CONTROL_MASK); key(w, Gdk.KEY_z, Gdk.ModifierType.CONTROL_MASK)
    yield 0.5
    check("undo puts the sound back", len(s.lanes[0].items) == 1 and s.lanes[0].items[0].at == 0, [str(i) for i in s.lanes[0].items])
    key(w, Gdk.KEY_Escape); yield 0.4

    # --- volume
    w.volume.set_value(50); yield 0.3
    check("volume sets the clip's gain", abs(s.clip.gain_db - (-6.02)) < 0.01 and w.volume_lbl.get_text() == "50%", s.clip.gain_db)
    key(w, Gdk.KEY_z, Gdk.ModifierType.CONTROL_MASK); yield 0.3
    check("undo volume", s.clip.gain_db == 0.0)

    report["snapshot"] = snapshot(w, OUT + ".png")
    snapshot_widgets(w, [trim, tl], OUT + "-bars.png")

    # --- project: save, change, reopen
    proj = OUT + ".pve"
    w._save_to(proj); yield 0.3
    check("saved, not dirty", os.path.exists(proj) and not s.dirty and s.file == os.path.abspath(proj))
    saved = json.load(open(proj))
    check("project stores relative media paths", all(not os.path.isabs(m["path"]) for m in saved["media"].values()), saved["media"])
    key(w, Gdk.KEY_Delete); yield 0.5
    check("an edit makes it dirty", s.dirty and len(s.clips) == 1 and w.get_title().startswith("•"), w.get_title())
    w.open_project(proj)
    t1 = time.monotonic()
    while (len(s.clips) != 2 or s.dirty) and time.monotonic() - t1 < 10:
        yield 0.2
    yield 0.5
    check("reopened project", len(s.clips) == 2 and len(s.lanes[0].items) == 1 and s.clips[0].crop_aspect == Fraction(4, 3) and not s.dirty)

    # --- playback: carries on into clip 2, with the lane playing in step
    w._seek(14.5); yield 0.6
    w.toggle_play()
    yield 0.35
    lp = w.lane_players[0]
    lane_now, want = lp.position(), 7 + (w.pos - 5)
    check("lane sound plays in step with the clip", lp.path == FILES[1] and lane_now is not None and abs(lane_now - want) < 0.3,
          (lp.path, lane_now, round(want, 3)))
    t1 = time.monotonic()
    while s.selected == 0 and time.monotonic() - t1 < 6:
        yield 0.1
    yield 0.8
    check("playback carried on into clip 2", s.selected == 1 and w.pos >= 7.0, (s.selected, round(w.pos, 3)))
    w.player.set_playing(False); yield 0.3
    check("lane pauses with the clip", lp.paused)
    check("second lane's player was never started", w.lane_players[1].mpv is None)

    # --- export through the window
    out = OUT + ".mp4"
    if os.path.exists(out):
        os.remove(out)
    w._export_to(FakeDialog(out), None)
    t2 = time.monotonic()
    while w._cancel is not None and time.monotonic() - t2 < 120:
        yield 0.2
    check("export finished", w.status.get_text().startswith("Saved"), w.status.get_text())
    if os.path.exists(out):
        probe = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type,nb_frames,duration",
                                "-of", "csv=p=0", out], capture_output=True, text=True).stdout.split()
        vol = subprocess.run(["ffmpeg", "-v", "info", "-t", "9", "-i", out, "-vn", "-af", "volumedetect", "-f", "null", "-"],
                             capture_output=True, text=True).stderr
        mean = [float(l.split("mean_volume:")[1].split()[0]) for l in vol.splitlines() if "mean_volume:" in l]
        check("export has picture and sound", len(probe) == 2 and "video,20.000000,600" in probe, probe)
        check("the first clip carries the lane's sound", mean and mean[0] > -60, mean)


def run(app):
    w = Window(app, FILES)
    w.present()
    gen = script(w)

    def step():
        try:
            delay = next(gen)
        except StopIteration:
            w.close()
            return False
        except Exception:
            report["errors"].append(traceback.format_exc())
            w.close()
            return False
        GLib.timeout_add(int(delay * 1000), step)
        return False

    GLib.timeout_add(300, step)


app = Gtk.Application(application_id="app.plainvideo.EditorTest", flags=Gio.ApplicationFlags.NON_UNIQUE)
app.connect("activate", run)
app.run([])
json.dump(report, open(OUT + ".json", "w"), indent=1)
bad = [s for s in report["steps"] if not s["ok"]]
for s in report["steps"]:
    print(("ok   " if s["ok"] else "FAIL ") + s["step"] + ("" if s["ok"] else "  " + s["detail"][:300]))
for e in report["errors"]:
    print(e)
print(report.get("window"))
print(f"{len(report['steps']) - len(bad)} passed, {len(bad)} failed, {len(report['errors'])} errors")
sys.exit(1 if bad or report["errors"] else 0)
