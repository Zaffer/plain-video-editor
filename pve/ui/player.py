"""Preview: libmpv drawing into a GTK4 GLArea.

Times are seconds from the start of the media, the same as the core uses
(mpv rebases timestamps to start at zero by default).
"""
from __future__ import annotations

import ctypes

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk  # noqa: E402

import mpv  # noqa: E402

GL_DRAW_FRAMEBUFFER_BINDING = 0x8CA6

_egl = ctypes.CDLL("libEGL.so.1")
_egl.eglGetProcAddress.restype = ctypes.c_void_p
_egl.eglGetProcAddress.argtypes = [ctypes.c_char_p]
_gl = ctypes.CDLL("libGL.so.1")


class Player(Gtk.GLArea):
    """on_time(seconds) and on_pause(paused) are called on the GTK main thread."""

    def __init__(self, on_time, on_pause):
        super().__init__(hexpand=True, vexpand=True)
        self.set_auto_render(False)
        self._on_time, self._on_pause = on_time, on_pause
        self.mpv = mpv.MPV(vo="libmpv", hwdec="auto-safe", keep_open="yes", pause=True, osc=False,
                           input_default_bindings=False, input_vo_keyboard=False)
        self._ctx = None
        self.path: str | None = None
        self._loaded = False  # current file is ready to seek
        self._busy = False  # a seek is in flight
        self._pending: tuple[float, bool] | None = None  # newest seek asked for while busy
        self._time_queued = False
        self._latest = 0.0
        self._deferred: tuple[str, float, bool] | None = None  # load asked for before the GL context existed
        self._alive = True  # False once mpv has been shut down
        self._sound: tuple[bool, float] | None = None  # what mpv was last told, so it is only told on a change
        self.connect("realize", self._realize)
        self.connect("render", self._render)
        self.connect("unrealize", self._unrealize)

        self.mpv.observe_property("time-pos", self._time_changed)
        self.mpv.observe_property("pause", lambda _n, v: GLib.idle_add(lambda: self._alive and self._on_pause(bool(v)) and False))
        self.mpv.event_callback("playback-restart")(lambda _e: GLib.idle_add(self._restarted))

    # ------------------------------------------------------------ rendering

    def _realize(self, _):
        self.make_current()
        if self.get_error():
            return
        # eglGetProcAddress rather than libepoxy: epoxy on glvnd builds makes mpv report "unsupported".
        self._proc = mpv.MpvGlGetProcAddressFn(lambda _ctx, name: _egl.eglGetProcAddress(name))
        self._ctx = mpv.MpvRenderContext(self.mpv, "opengl", opengl_init_params={"get_proc_address": self._proc})
        self._ctx.update_cb = lambda: GLib.idle_add(self.queue_render)
        if self._deferred:
            args, self._deferred = self._deferred, None
            self.load(*args)

    def _render(self, _area, _glctx):
        if not self._ctx:
            return False
        fbo = ctypes.c_int(0)
        _gl.glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, ctypes.byref(fbo))
        s = self.get_scale_factor()
        self._ctx.render(flip_y=True, opengl_fbo={"w": self.get_width() * s, "h": self.get_height() * s, "fbo": fbo.value})
        self._ctx.report_swap()
        return True

    def _unrealize(self, _):
        if self._ctx:
            self.make_current()
            self._ctx.free()
            self._ctx = None
        self._alive = False
        self.mpv.terminate()

    # ------------------------------------------------------------ events from mpv (its thread)

    def _time_changed(self, _name, value):
        if value is None:
            return
        self._latest = float(value)
        if not self._time_queued:  # coalesce: one update per main-loop turn
            self._time_queued = True
            GLib.idle_add(self._emit_time)

    def _emit_time(self):
        self._time_queued = False
        if self._loaded and self._alive:  # ignore times left over from a file that is being replaced or closed
            self._on_time(self._latest)
        return False

    def _restarted(self):
        if not self._alive:
            return False
        self._loaded = True
        self._busy = False
        if self._pending is not None:
            (t, exact), self._pending = self._pending, None
            self.seek(t, exact)
        return False

    # ------------------------------------------------------------ control

    def load(self, path: str, at: float, playing: bool = False) -> None:
        """Show `path` at time `at`. If it is already loaded this is just a seek."""
        if self._ctx is None:
            self._deferred = (path, at, playing)
            return
        if path == self.path:
            self.seek(at, exact=True)
            self.mpv.pause = not playing
            return
        self.path, self._loaded, self._busy, self._pending = path, False, True, None
        self.mpv["start"] = f"{at:.6f}"
        self.mpv.pause = not playing
        self.mpv.loadfile(path)

    def unload(self) -> None:
        self.path, self._loaded = None, False
        self.mpv.command("stop")

    def seek(self, t: float, exact: bool = True) -> None:
        """exact=False jumps to a keyframe (fast, for dragging); exact=True lands on the frame."""
        if not self._loaded or self._busy:
            self._pending = (t, exact)  # only the newest request matters
            return
        self._busy = True
        self.mpv.command("seek", f"{max(t, 0.0):.6f}", "absolute+exact" if exact else "absolute+keyframes")

    def step(self, forward: bool) -> None:
        if self._loaded:
            self.mpv.command("frame-step" if forward else "frame-back-step")

    def set_playing(self, playing: bool) -> None:
        self.mpv.pause = not playing

    @property
    def playing(self) -> bool:
        return self._alive and not self.mpv.pause

    def set_sound(self, on: bool, gain_db: float = 0.0) -> None:
        if self._sound == (on, gain_db):
            return  # called on every redraw; each call into mpv waits for its thread
        self._sound = (on, gain_db)
        self.mpv.mute = not on
        self.mpv["volume-gain"] = gain_db


class LanePlayer:
    """Plays one sound lane alongside the preview. No picture, no window.

    sync() is called repeatedly during playback with what the lane should be
    playing; it loads, seeks or pauses as needed. A second player drifts a
    little against the first, so this is for listening while editing, not a
    sample-accurate mix; the export is exact.
    """

    DRIFT = 0.12  # seconds out of place before it re-seeks

    def __init__(self):
        self.mpv = None  # started only when the lane first has something to play
        self.path: str | None = None
        self._ready = False
        self._alive = True
        self._paused = True
        self._gain: float | None = None

    def sync(self, path: str | None, t: float = 0.0, gain_db: float = 0.0) -> None:
        if not self._alive:
            return
        if path is None:
            if not self._paused:
                self._paused = True
                self.mpv.pause = True
            return
        if self.mpv is None:
            self.mpv = mpv.MPV(vid="no", vo="null", keep_open="yes", pause=True,
                               input_default_bindings=False, input_vo_keyboard=False)
            self.mpv.event_callback("playback-restart")(lambda _e: setattr(self, "_ready", True))
        if self._gain != gain_db:
            self._gain = gain_db
            self.mpv["volume-gain"] = gain_db
        if path != self.path:
            self.path, self._ready, self._paused = path, False, False
            self.mpv["start"] = f"{t:.6f}"
            self.mpv.pause = False
            self.mpv.loadfile(path)
            return
        if not self._ready:
            return
        now = self.mpv.time_pos
        if now is None or abs(now - t) > self.DRIFT:
            self._ready = False
            self.mpv.command("seek", f"{max(t, 0.0):.6f}", "absolute+exact")
        if self._paused:
            self._paused = False
            self.mpv.pause = False

    @property
    def paused(self) -> bool:
        return self._paused

    def position(self) -> float | None:
        return self.mpv.time_pos if self.mpv is not None else None

    def close(self) -> None:
        if self._alive:
            self._alive = False
            if self.mpv is not None:
                self.mpv.terminate()
