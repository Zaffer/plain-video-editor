//! Preview: libmpv drawing into a GTK 4 GLArea, and the sound-lane players.
//!
//! Times are seconds from the start of the media, the same as the core uses
//! (mpv rebases timestamps to start at zero by default).

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::mpv::{Event, Mpv, Renderer};

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    Time(f64),
    Paused(bool),
    /// Playback ran into the end of the file and stopped there.
    Ended,
}

struct Inner {
    // Declared before `mpv` so it is dropped first.
    renderer: Option<Renderer>,
    mpv: Option<Mpv>,
    path: Option<String>,
    /// The current file is ready to seek.
    loaded: bool,
    /// A seek is in flight.
    busy: bool,
    /// Newest seek asked for while busy.
    pending: Option<(f64, bool)>,
    /// A load asked for before the GL context existed.
    deferred: Option<(String, f64, bool)>,
    paused: bool,
    /// The end of the file has been reached (mpv then stays on the last frame, paused).
    eof: bool,
    /// The app has been told so.
    eof_told: bool,
    /// What mpv was last told about sound, so it is only told again when it changes.
    sound: Option<(bool, f64)>,
    /// How many frames have been drawn, and (when asked for by a test) the centre pixel of the last one.
    drawn: u64,
    sample: Option<[u8; 4]>,
}

#[derive(Clone)]
pub struct Player {
    pub area: gtk::GLArea,
    inner: Rc<RefCell<Inner>>,
}

impl Player {
    pub fn new() -> Result<Player, String> {
        let mpv = Mpv::new(&[
            ("vo", "libmpv"),
            ("hwdec", "auto-safe"),
            ("keep-open", "yes"),
            ("pause", "yes"),
            ("osc", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            // PVE_MPV_LOG=1 prints mpv's own log to the terminal, for diagnosing playback problems.
            ("terminal", if std::env::var_os("PVE_MPV_LOG").is_some() { "yes" } else { "no" }),
            ("msg-level", if std::env::var_os("PVE_MPV_LOG").is_some() { "all=v" } else { "all=no" }),
        ])?;
        mpv.observe_number("time-pos");
        mpv.observe_flag("pause");
        mpv.observe_flag("eof-reached");
        let area = gtk::GLArea::builder().hexpand(true).vexpand(true).auto_render(false).build();
        let player = Player {
            area,
            inner: Rc::new(RefCell::new(Inner {
                renderer: None,
                mpv: Some(mpv),
                path: None,
                loaded: false,
                busy: false,
                pending: None,
                deferred: None,
                paused: true,
                eof: false,
                eof_told: false,
                sound: None,
                drawn: 0,
                sample: None,
            })),
        };

        let p = player.clone();
        player.area.connect_realize(move |area| {
            area.make_current();
            if area.error().is_some() {
                return;
            }
            let deferred = {
                let mut inner = p.inner.borrow_mut();
                let Some(mpv) = inner.mpv.as_ref() else { return };
                // The callback runs on an mpv thread: hop to the main loop before touching the widget.
                let weak = glib::SendWeakRef::from(area.downgrade());
                let renderer = Renderer::new(mpv, move || {
                    let weak = weak.clone();
                    glib::idle_add_once(move || {
                        if let Some(area) = weak.upgrade() {
                            area.queue_render();
                        }
                    });
                });
                match renderer {
                    Ok(r) => inner.renderer = Some(r),
                    Err(e) => eprintln!("{e}"),
                }
                inner.deferred.take()
            };
            if let Some((path, at, playing)) = deferred {
                p.load(&path, at, playing);
            }
        });

        let p = player.clone();
        let sampling = std::env::var_os("PVE_SELFTEST").is_some();
        player.area.connect_render(move |area, _| {
            let mut inner = p.inner.borrow_mut();
            if let Some(r) = inner.renderer.as_ref() {
                let s = area.scale_factor();
                let (w, h) = (area.width() * s, area.height() * s);
                let fbo = r.render(w, h);
                inner.drawn += 1;
                if sampling {
                    inner.sample = Some(crate::mpv::read_pixel(fbo, w / 2, h / 2));
                }
            }
            glib::Propagation::Stop
        });

        let p = player.clone();
        player.area.connect_unrealize(move |area| {
            area.make_current();
            p.close();
        });
        Ok(player)
    }

    /// Shut mpv down. The GL context must be current if anything was drawn.
    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.renderer = None;
        inner.mpv = None;
    }

    fn with<T>(&self, f: impl FnOnce(&Mpv) -> T) -> Option<T> {
        self.inner.borrow().mpv.as_ref().map(f)
    }

    /// Drain what mpv has reported since the last call.
    pub fn poll(&self) -> Vec<PlayerEvent> {
        let mut out = Vec::new();
        let mut reseek = None;
        {
            let mut inner = self.inner.borrow_mut();
            let mut latest = None;
            let mut ran_out = false;
            while let Some(ev) = inner.mpv.as_ref().and_then(Mpv::poll) {
                match ev {
                    Event::PlaybackRestart => {
                        inner.loaded = true;
                        inner.busy = false;
                        if let Some(p) = inner.pending.take() {
                            reseek = Some(p);
                        }
                    }
                    // ignore times left over from a file that is being replaced
                    Event::Number(name, Some(t)) if name == "time-pos" && inner.loaded => latest = Some(t),
                    // At the end of a file mpv says so and pauses, in either order.
                    Event::Flag(name, paused) if name == "pause" => {
                        ran_out |= paused && !inner.paused && inner.eof;
                        inner.paused = paused;
                        out.push(PlayerEvent::Paused(paused));
                    }
                    Event::Flag(name, false) if name == "eof-reached" => {
                        inner.eof = false;
                        inner.eof_told = false;
                    }
                    // ...and an end left over from before a seek or a load is not this one
                    Event::Flag(name, true) if name == "eof-reached" && inner.loaded && !inner.busy => {
                        inner.eof = true;
                        ran_out |= !inner.paused;
                    }
                    _ => {}
                }
            }
            if let Some(t) = latest {
                out.push(PlayerEvent::Time(t)); // one update per poll is enough
            }
            if ran_out && !inner.eof_told {
                inner.eof_told = true;
                out.push(PlayerEvent::Ended); // after the time, which may already have moved playback on
            }
        }
        if let Some((t, exact)) = reseek {
            self.seek(t, exact);
        }
        out
    }

    /// Show `path` at time `at`. If it is already loaded this is just a seek.
    pub fn load(&self, path: &str, at: f64, playing: bool) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.renderer.is_none() {
                inner.deferred = Some((path.to_owned(), at, playing));
                return;
            }
            if inner.path.as_deref() != Some(path) {
                inner.path = Some(path.to_owned());
                inner.loaded = false;
                inner.busy = true;
                inner.pending = None;
                inner.eof = false;
                inner.paused = !playing;
                if let Some(mpv) = inner.mpv.as_ref() {
                    mpv.set("start", &format!("{at:.6}"));
                    mpv.set_flag("pause", !playing);
                    mpv.command(&["loadfile", path]);
                }
                return;
            }
        }
        self.seek(at, true);
        self.set_playing(playing);
    }

    pub fn unload(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.path = None;
        inner.loaded = false;
        if let Some(mpv) = inner.mpv.as_ref() {
            mpv.command(&["stop"]);
        }
    }

    /// exact=false jumps to a keyframe (fast, for dragging); exact=true lands on the frame.
    pub fn seek(&self, t: f64, exact: bool) {
        let mut inner = self.inner.borrow_mut();
        if !inner.loaded || inner.busy {
            inner.pending = Some((t, exact)); // only the newest request matters
            return;
        }
        inner.busy = true;
        inner.eof = false;
        if let Some(mpv) = inner.mpv.as_ref() {
            mpv.command(&["seek", &format!("{:.6}", t.max(0.0)), if exact { "absolute+exact" } else { "absolute+keyframes" }]);
        }
    }

    pub fn step(&self, forward: bool) {
        if self.inner.borrow().loaded {
            self.with(|m| m.command(&[if forward { "frame-step" } else { "frame-back-step" }]));
        }
    }

    pub fn set_playing(&self, playing: bool) {
        self.inner.borrow_mut().paused = !playing;
        self.with(|m| m.set_flag("pause", !playing));
    }

    pub fn playing(&self) -> bool {
        let inner = self.inner.borrow();
        inner.mpv.is_some() && !inner.paused
    }

    /// The file is at its end: playing does nothing until it is moved back.
    pub fn ended(&self) -> bool {
        self.inner.borrow().eof
    }

    /// (frames drawn so far, centre pixel of the last one if a test asked for sampling)
    /// For tests: whether mpv was last told to play sound (None before it was told anything).
    pub fn sound_on(&self) -> Option<bool> {
        self.inner.borrow().sound.map(|s| s.0)
    }

    pub fn drawn(&self) -> (u64, Option<[u8; 4]>) {
        let inner = self.inner.borrow();
        (inner.drawn, inner.sample)
    }

    /// For tests: draw the current frame off screen and return its centre pixel.
    pub fn sample_offscreen(&self) -> Option<[u8; 4]> {
        self.area.make_current();
        self.inner.borrow().renderer.as_ref().map(crate::mpv::sample_offscreen)
    }

    /// The current file has been loaded and its first frame is ready.
    pub fn ready(&self) -> bool {
        self.inner.borrow().loaded
    }

    pub fn set_sound(&self, on: bool, gain_db: f64) {
        let mut inner = self.inner.borrow_mut();
        if inner.sound == Some((on, gain_db)) {
            return; // called on every redraw; only talk to mpv when something changed
        }
        inner.sound = Some((on, gain_db));
        if let Some(m) = inner.mpv.as_ref() {
            m.set_flag("mute", !on);
            m.set("volume-gain", &format!("{gain_db}"));
        }
    }
}

/// Plays one sound lane alongside the preview. No picture, no window.
///
/// sync() is called repeatedly during playback with what the lane should be
/// playing; it loads, seeks or pauses as needed. A second player drifts a
/// little against the first, so this is for listening while editing, not a
/// sample-accurate mix; the export is exact.
pub struct LanePlayer {
    mpv: Option<Mpv>,
    pub path: Option<String>,
    ready: bool,
    paused: bool,
    /// Where mpv last said it was, and when it said so.
    pos: Option<(f64, std::time::Instant)>,
    gain: Option<f64>,
}

impl LanePlayer {
    /// Seconds out of place before it re-seeks.
    const DRIFT: f64 = 0.12;

    /// mpv is only started when the lane first has something to play.
    pub fn new() -> LanePlayer {
        LanePlayer { mpv: None, path: None, ready: false, paused: true, pos: None, gain: None }
    }

    fn mpv(&mut self) -> Option<&Mpv> {
        if self.mpv.is_none() {
            self.mpv = Mpv::new(&[("vid", "no"), ("vo", "null"), ("keep-open", "yes"), ("pause", "yes"),
                                  ("input-default-bindings", "no"), ("terminal", "no")]).ok();
            if let Some(m) = self.mpv.as_ref() {
                m.observe_number("time-pos");
            }
        }
        self.mpv.as_ref()
    }

    pub fn sync(&mut self, want: Option<(&str, f64, f64)>) {
        let Some((path, t, gain_db)) = want else {
            if !self.paused {
                self.paused = true;
                if let Some(m) = self.mpv.as_ref() {
                    m.set_flag("pause", true);
                }
            }
            return;
        };
        if self.mpv().is_none() {
            return;
        }
        // note what mpv has reported since last time (never blocks)
        while let Some(ev) = self.mpv.as_ref().and_then(Mpv::poll) {
            match ev {
                Event::PlaybackRestart => self.ready = true,
                Event::Number(name, value) if name == "time-pos" => self.pos = value.map(|v| (v, std::time::Instant::now())),
                _ => {}
            }
        }
        let mpv = self.mpv.as_ref().expect("started above");
        if self.gain != Some(gain_db) {
            self.gain = Some(gain_db);
            mpv.set("volume-gain", &format!("{gain_db}"));
        }
        if self.path.as_deref() != Some(path) {
            self.path = Some(path.to_owned());
            self.ready = false;
            self.paused = false;
            self.pos = None;
            mpv.set("start", &format!("{t:.6}"));
            mpv.set_flag("pause", false);
            mpv.command(&["loadfile", path]);
            return;
        }
        if !self.ready {
            return;
        }
        // mpv reports its position every so often; it has moved on by the time since the last report
        let playing_at = self.pos.map(|(p, when)| p + if self.paused { 0.0 } else { when.elapsed().as_secs_f64() });
        if playing_at.is_none_or(|now| (now - t).abs() > Self::DRIFT) {
            self.ready = false;
            self.pos = None;
            mpv.command(&["seek", &format!("{:.6}", t.max(0.0)), "absolute+exact"]);
        }
        if self.paused {
            self.paused = false;
            mpv.set_flag("pause", false);
        }
    }

    /// Where the lane is playing right now (asks mpv and waits; for tests).
    pub fn position(&self) -> Option<f64> {
        self.mpv.as_ref().and_then(|m| m.get_f64("time-pos"))
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn started(&self) -> bool {
        self.mpv.is_some()
    }
}
