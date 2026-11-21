"""Spike: libmpv embedded in a GTK4 GtkGLArea, with timing measurements.

Answers three questions on the target machine:
  1. Does embedded mpv render 4K HEVC 10-bit with hardware decode, and does it drop frames?
  2. How fast are scrubbing (keyframe seeks), exact seeks and frame steps?
  3. Does mpv's crop property work as the crop preview?

Usage:  python3 spikes/mpv_gtk4_spike.py FILE [--hwdec nvdec] [--play 6] [--json OUT]

The window must be visible while it measures: GTK does not draw hidden windows,
and a window started from a background process opens behind the focused one.
Click it (or its "is ready" notification) if it does not come to the front.
If libmpv is not installed system-wide, set LIBMPV=/path/to/libmpv.so.2 and
PYMPV=/dir/containing/mpv.py.

The window closes by itself when the measurements finish.
"""
import argparse
import ctypes
import ctypes.util
import json
import os
import statistics
import sys
import threading
import time

if os.environ.get("LIBMPV"):
    _find = ctypes.util.find_library
    ctypes.util.find_library = lambda n: os.environ["LIBMPV"] if n == "mpv" else _find(n)
if os.environ.get("PYMPV"):
    sys.path.insert(0, os.environ["PYMPV"])

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402

import mpv  # noqa: E402

GL_DRAW_FRAMEBUFFER_BINDING = 0x8CA6
GL_READ_FRAMEBUFFER = 0x8CA8
GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, GL_RGBA8 = 0x8D40, 0x8CE0, 0x0DE1, 0x8058
OFFSCREEN_SIZE = (1280, 720)
GL_RGBA, GL_UNSIGNED_BYTE = 0x1908, 0x1401

_egl = ctypes.CDLL("libEGL.so.1")
_egl.eglGetProcAddress.restype = ctypes.c_void_p
_egl.eglGetProcAddress.argtypes = [ctypes.c_char_p]
_gl = ctypes.CDLL("libGL.so.1")


class Player(Gtk.GLArea):
    """The embedding itself: an mpv render context drawing into this GLArea."""

    def __init__(self, hwdec):
        super().__init__(hexpand=True, vexpand=True)
        self.set_auto_render(False)
        self.mpv = mpv.MPV(vo="libmpv", hwdec=hwdec, keep_open="yes", pause=True, msg_level="all=warn")
        self.offscreen = False  # True: render into our own buffer, not the (hidden) window
        self._fbo = None
        self.ctx = None
        self.pixel = None  # centre pixel read back after the last render
        self.renders = 0
        self.ticks = 0  # GTK frame-clock ticks: only advance while the window is visible
        self.add_tick_callback(self._tick)
        self.connect("realize", self._realize)
        self.connect("render", self._render)
        self.connect("unrealize", self._unrealize)

    def _tick(self, *_):
        self.ticks += 1
        return True

    def _realize(self, _):
        self.make_current()
        # eglGetProcAddress rather than libepoxy: epoxy on glvnd builds makes mpv report "unsupported".
        self._proc = mpv.MpvGlGetProcAddressFn(lambda _ctx, name: _egl.eglGetProcAddress(name))
        self.ctx = mpv.MpvRenderContext(self.mpv, "opengl", opengl_init_params={"get_proc_address": self._proc})
        self.ctx.update_cb = lambda: GLib.idle_add(self._on_update)

    def _on_update(self):
        if self.offscreen:
            self._render_offscreen()
        else:
            self.queue_render()
        return False

    def _render_offscreen(self):
        """Same decode and render path, but into a buffer we own, so it runs while the window is hidden."""
        if not self.ctx:
            return
        self.make_current()
        w, h = OFFSCREEN_SIZE
        if self._fbo is None:
            tex, fbo = ctypes.c_uint(0), ctypes.c_uint(0)
            _gl.glGenTextures(1, ctypes.byref(tex))
            _gl.glBindTexture(GL_TEXTURE_2D, tex.value)
            _gl.glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, None)
            _gl.glGenFramebuffers(1, ctypes.byref(fbo))
            _gl.glBindFramebuffer(GL_FRAMEBUFFER, fbo.value)
            _gl.glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex.value, 0)
            self._fbo = fbo.value
        self.ctx.render(flip_y=False, opengl_fbo={"w": w, "h": h, "fbo": self._fbo})
        px = (ctypes.c_ubyte * 4)()
        _gl.glBindFramebuffer(GL_READ_FRAMEBUFFER, self._fbo)
        _gl.glReadPixels(w // 2, h // 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px)  # also forces the render to finish
        self.pixel = tuple(px)
        self.renders += 1
        self.ctx.report_swap()

    def _render(self, _area, _glctx):
        if not self.ctx:
            return False
        fbo = ctypes.c_int(0)
        _gl.glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, ctypes.byref(fbo))
        s = self.get_scale_factor()
        w, h = self.get_width() * s, self.get_height() * s
        self.ctx.render(flip_y=True, opengl_fbo={"w": w, "h": h, "fbo": fbo.value})
        px = (ctypes.c_ubyte * 4)()
        _gl.glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo.value)
        _gl.glReadPixels(w // 2, h // 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px)
        self.pixel = tuple(px)
        self.renders += 1
        self.ctx.report_swap()
        return True

    def _unrealize(self, _):
        if self.ctx:
            self.make_current()
            self.ctx.free()
            self.ctx = None
        self.mpv.terminate()


def measure(player: Player, path: str, play_seconds: float, results: dict):
    m = player.mpv
    restarted = threading.Event()
    m.event_callback("playback-restart")(lambda _e: restarted.set())

    def wait_restart(timeout=10.0):
        ok = restarted.wait(timeout)
        if not ok:
            before = player.ticks
            time.sleep(0.5)
            if player.ticks == before and not player.offscreen:
                raise RuntimeError("the window was hidden during the test; results so far are partial")
        return ok

    def timed(cmd, *args):
        restarted.clear()
        t = time.perf_counter()
        m.command(cmd, *args)
        ok = wait_restart()
        return (time.perf_counter() - t) * 1000 if ok else float("nan")

    def timed_step(cmd):
        """Frame steps do not send playback-restart; time until the position changes."""
        before = m.time_pos
        t = time.perf_counter()
        m.command(cmd)
        while m.time_pos == before:
            if time.perf_counter() - t > 5:
                return float("nan")
            time.sleep(0.0005)
        return (time.perf_counter() - t) * 1000

    def stats(xs):
        xs = [x for x in xs if x == x]
        if not xs:
            return None
        return {"median_ms": round(statistics.median(xs), 1), "max_ms": round(max(xs), 1), "n": len(xs)}

    restarted.clear()
    m.command("loadfile", path)
    wait_restart(20)
    dur = m.duration or 0
    fps = m.container_fps or 30
    results["file"] = {"w": m.width, "h": m.height, "fps": fps, "duration": dur,
                       "codec": m.video_codec, "pixfmt": (m.video_params or {}).get("pixelformat")}

    # 1. Playback
    ticks0, renders0 = player.ticks, player.renders
    m.pause = False
    time.sleep(play_seconds)
    results["playback"] = {
        "widget_frame_clock_hz": round((player.ticks - ticks0) / play_seconds, 1),
        "renders_per_second": round((player.renders - renders0) / play_seconds, 1),
        "seconds": play_seconds,
        "hwdec_current": m.hwdec_current,
        "frame_drop_count (display)": m.frame_drop_count,
        "decoder_frame_drop_count": m.decoder_frame_drop_count,
        "mistimed_frame_count": m.mistimed_frame_count,
        "estimated_vf_fps": round(m.estimated_vf_fps or 0, 2),
        "display_fps": round(m.display_fps or 0, 2),
        "centre_pixel_rgba": player.pixel,
        "render_px": OFFSCREEN_SIZE if player.offscreen
        else (player.get_width() * player.get_scale_factor(), player.get_height() * player.get_scale_factor()),
    }
    m.pause = True
    time.sleep(0.2)

    # 2. Scrubbing: what dragging the playhead would do (keyframe seeks), then exact seeks.
    points = [dur * f for f in (0.13, 0.71, 0.29, 0.86, 0.47, 0.05, 0.62, 0.93, 0.38, 0.19)]
    results["seek_keyframe"] = stats([timed("seek", str(p), "absolute+keyframes") for p in points])
    results["seek_exact"] = stats([timed("seek", str(p), "absolute+exact") for p in points])

    # Frame stepping from the middle of a group of pictures.
    timed("seek", str(dur * 0.55), "absolute+exact")
    results["frame_step_forward"] = stats([timed_step("frame-step") for _ in range(15)])
    results["frame_step_back"] = stats([timed_step("frame-back-step") for _ in range(15)])

    # Are exact seeks really exact? Ask for specific frame times and read back.
    errs = []
    for n in (37, 301, 644, 911):
        want = n / fps
        if want < dur:
            timed("seek", f"{want:.6f}", "absolute+exact")
            errs.append(abs((m.time_pos or 0) - want) * fps)
    results["exact_seek_error_frames_max"] = round(max(errs), 3) if errs else None

    # 3. Crop preview via the video-crop property.
    w, h = m.width, m.height
    cw, ch = (h * 4 // 3) // 2 * 2, h
    restarted.clear()
    m["video-crop"] = f"{cw}x{ch}+{(w - cw) // 2}+0"
    time.sleep(0.6)
    out = m.video_out_params or {}
    results["crop"] = {"asked": f"{cw}x{ch}", "video_out": f"{out.get('dw')}x{out.get('dh')}",
                       "ok": (out.get("dw"), out.get("dh")) == (cw, ch)}
    m["video-crop"] = ""
    time.sleep(0.2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file", nargs="+", help="one or more clips, measured one after another in the same window")
    ap.add_argument("--hwdec", default="nvdec")
    ap.add_argument("--play", type=float, default=6.0)
    ap.add_argument("--json")
    ap.add_argument("--wait", type=float, default=60.0, help="seconds to wait for the window to become visible")
    ap.add_argument("--offscreen", action="store_true",
                    help="do not need a visible window: render into an off-screen buffer (no on-screen pacing numbers)")
    args = ap.parse_args()

    results = {"hwdec_requested": args.hwdec, "gsk_renderer": os.environ.get("GSK_RENDERER", "(default)")}
    app = Gtk.Application(application_id="dev.plain.videoeditor.spike")

    def activate(app):
        win = Gtk.ApplicationWindow(application=app, title="pve spike: click this window to start the test",
                                    default_width=1280, default_height=720)
        player = Player(args.hwdec)
        if not args.offscreen:
            player.mpv["video-sync"] = "display-resample"
        win.set_child(player)
        win.present()

        def worker():
            try:
                # Wait until the window is really on screen (frame clock running, not suspended).
                # A hidden window still ticks briefly when first mapped, so require it to keep going.
                deadline = time.monotonic() + args.wait
                time.sleep(1.5)
                steady = 0
                player.offscreen = args.offscreen
                results["mode"] = "offscreen" if args.offscreen else "on screen"
                while steady < 4 and not args.offscreen:
                    before = player.ticks
                    time.sleep(0.5)
                    steady = steady + 1 if (player.ticks - before >= 10 and not win.is_suspended()) else 0
                    if time.monotonic() > deadline:
                        results["error"] = "the window never became visible, so nothing was measured"
                        GLib.idle_add(win.close)
                        return
                GLib.idle_add(win.set_title, "pve spike: measuring, closes by itself")
                if not args.offscreen:
                    # mpv cannot see the monitor through the render API, so tell it the refresh rate.
                    got = threading.Event()

                    def refresh():
                        mon = Gdk.Display.get_default().get_monitor_at_surface(win.get_surface())
                        results["monitor_hz"] = round(mon.get_refresh_rate() / 1000, 3) if mon else None
                        got.set()

                    GLib.idle_add(refresh)
                    got.wait(2)
                    if results.get("monitor_hz"):
                        player.mpv["display-fps-override"] = results["monitor_hz"]
                results["runs"] = {}
                for f in args.file:
                    run = results["runs"][os.path.basename(f)] = {}
                    measure(player, os.path.abspath(f), args.play, run)
            except Exception as e:  # report rather than hang the window
                results["error"] = repr(e)
            GLib.idle_add(win.close)

        threading.Thread(target=worker, daemon=True).start()

    app.connect("activate", activate)
    app.run([])
    text = json.dumps(results, indent=2)
    print(text)
    if args.json:
        with open(args.json, "w") as f:
            f.write(text + "\n")


if __name__ == "__main__":
    main()
