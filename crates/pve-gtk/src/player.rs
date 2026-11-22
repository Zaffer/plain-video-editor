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
    /// The time mpv gave while a seek or load was under way: where it has landed, to be
    /// passed on once that is complete. Until then it may be a time from before the seek.
    landed: Option<f64>,
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
                landed: None,
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
                        latest = inner.landed.take().or(latest);
                        if let Some(p) = inner.pending.take() {
                            reseek = Some(p);
                        }
                    }
                    // A time given while a file is being replaced or a place is being found may
                    // be from before: acting on it would put the play position back where it was.
                    Event::Number(name, t) if name == "time-pos" && (inner.busy || !inner.loaded) => inner.landed = t,
                    Event::Number(name, Some(t)) if name == "time-pos" => latest = Some(t),
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
            if let Some(t) = latest.filter(|_| reseek.is_none()) {
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
                inner.landed = None;
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
        inner.landed = None;
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

    /// The file is loaded and no seek is under way: playing can start at once.
    pub fn settled(&self) -> bool {
        let inner = self.inner.borrow();
        inner.mpv.is_some() && inner.loaded && !inner.busy && inner.pending.is_none()
    }

    /// Playing, and the picture is moving: not still loading a file or finding a place in it.
    pub fn running(&self) -> bool {
        let inner = self.inner.borrow();
        inner.mpv.is_some() && !inner.paused && inner.loaded && !inner.busy
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

/// What to do to a lane's player to keep it in step with the preview.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Jump to this time in the sound.
    Seek(f64),
    /// Play at this speed for now (1.0 is normal).
    Speed(f64),
    Hold,
}

/// Keeps a lane's player in step without making the sound stutter.
///
/// A jump is not instant: the sound is heard a moment after the seek (a quarter of a second
/// with an MP3). Judged naively, that lateness looks like being out of step and asks for
/// another jump, and so on for ever. So a jump aims ahead by the delay learnt from the jumps
/// before it, a jump is judged once, and what is left over is closed by playing very slightly
/// faster or slower, which cannot be heard as a break.
#[derive(Debug, Clone, PartialEq)]
pub struct Steer {
    /// Seconds from a jump to the sound being heard, as learnt so far.
    lead: f64,
    /// How far out of step, smoothed; positive when the lane is behind.
    error: f64,
    speed: f64,
    /// A jump has been made and has not been judged yet.
    judging: bool,
}

impl Steer {
    /// Further out of step than this, jump.
    const FAR: f64 = 0.35;
    /// Closer than this is in step; once catching up, carry on until `CLOSE`.
    const NEAR: f64 = 0.04;
    const CLOSE: f64 = 0.012;
    /// The most the speed is changed by.
    const NUDGE: f64 = 0.04;

    pub fn new() -> Steer {
        Steer { lead: 0.2, error: 0.0, speed: 1.0, judging: false }
    }

    /// Where to start so that `want` is what is heard once the sound has started.
    pub fn aim(&mut self, want: f64) -> f64 {
        self.judging = true;
        self.error = 0.0;
        self.speed = 1.0;
        want + self.lead
    }

    /// `error` is the wanted time less the time being heard, measured once the player has
    /// settled after any jump.
    pub fn step(&mut self, want: f64, error: f64) -> Step {
        if self.judging {
            // the first look after a jump: what is left over is how wrong the aim was
            self.judging = false;
            self.lead = (self.lead + 0.8 * error).clamp(0.0, 1.0);
            self.error = error;
        } else {
            self.error += 0.2 * (error - self.error);
        }
        if self.error.abs() > Self::FAR {
            return Step::Seek(self.aim(want));
        }
        let catching_up = self.speed != 1.0;
        let speed = if self.error.abs() > if catching_up { Self::CLOSE } else { Self::NEAR } {
            1.0 + (self.error * 0.5).clamp(-Self::NUDGE, Self::NUDGE)
        } else {
            1.0
        };
        // not worth telling the player about, unless it is the return to normal speed
        if speed == self.speed || (speed != 1.0 && (speed - self.speed).abs() < 0.004) {
            return Step::Hold;
        }
        self.speed = speed;
        Step::Speed(speed)
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// Playing starts again from where the sound was waiting: nothing to learn from that.
    pub fn resume(&mut self) {
        self.judging = false;
        self.error = 0.0;
        self.speed = 1.0;
    }
}

/// Plays one sound lane alongside the preview. No picture, no window.
///
/// sync() is called repeatedly with what the lane should be playing, or should
/// have waiting. While the preview is paused the sound waits, paused, at the
/// very place it will be wanted, so that playing only has to let it go. Even
/// that takes a moment to be heard (a fifth of a second or more), so the sound
/// is let go that much before its picture: see start_lead(). While the preview
/// plays, sync() jumps, nudges the speed or pauses as needed (see Steer). A second player is never exactly in step with the first, so this is
/// for listening while editing, not a sample-accurate mix; the export is exact.
///
/// PVE_LANE_LOG=1 prints every jump and change of speed to the terminal.
pub struct LanePlayer {
    mpv: Option<Mpv>,
    pub path: Option<String>,
    ready: bool,
    paused: bool,
    /// Where mpv last said it was, and when it said so.
    pos: Option<(f64, std::time::Instant)>,
    /// When the player last became ready after a load or a jump.
    settled: Option<std::time::Instant>,
    /// Where the sound has been put to wait while paused.
    waiting_at: Option<f64>,
    /// Seconds from letting a waiting sound go to its being heard, as learnt so far.
    start_lead: f64,
    /// When a waiting sound was last let go, and how far ahead of its picture.
    let_go: Option<(std::time::Instant, f64)>,
    steer: Steer,
    gain: Option<f64>,
    /// How many jumps have been made (for tests).
    pub jumps: u32,
}

impl LanePlayer {
    /// How long after a jump the sound is given to start before it is judged.
    const SETTLE: f64 = 0.15;

    /// mpv is only started when the lane first has something to play.
    pub fn new() -> LanePlayer {
        LanePlayer { mpv: None, path: None, ready: false, paused: true, pos: None, settled: None, waiting_at: None, start_lead: 0.25, let_go: None, steer: Steer::new(), gain: None, jumps: 0 }
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

    /// How long before its picture a waiting sound should be let go, to be heard on time.
    pub fn start_lead(&self) -> f64 {
        self.start_lead
    }

    /// `want` is (file, time within it, gain). With `play` it is to be heard; without, it is
    /// to wait there, paused. `moving` says the preview's picture is moving, or is about to:
    /// a waiting sound is not let go before that, or it would run ahead while a file loads.
    /// `lead` is how long from now the picture will be at `want` (0: it is there now).
    /// None: the lane has nothing coming.
    pub fn sync(&mut self, want: Option<(&str, f64, f64)>, play: bool, moving: bool, lead: f64) {
        /// Nearer than this to where it is wanted, a waiting sound is left where it is.
        const IN_PLACE: f64 = 0.03;
        /// Nearer than this, a waiting sound is let go as it is, so that it is heard from the
        /// very place it waited at; what it is late by is closed by steering.
        const NEAR_ENOUGH: f64 = 0.3;
        let play = play && (moving || !self.paused);
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
                Event::PlaybackRestart => {
                    self.ready = true;
                    self.settled = Some(std::time::Instant::now());
                }
                Event::Number(name, value) if name == "time-pos" => self.pos = value.map(|v| (v, std::time::Instant::now())),
                _ => {}
            }
        }
        let log = std::env::var_os("PVE_LANE_LOG").is_some();
        let mpv = self.mpv.as_ref().expect("started above");
        if self.gain != Some(gain_db) {
            self.gain = Some(gain_db);
            mpv.set("volume-gain", &format!("{gain_db}"));
        }
        if self.path.as_deref() != Some(path) {
            self.path = Some(path.to_owned());
            self.ready = false;
            self.paused = !play;
            self.pos = None;
            // wanted at once: aim ahead by the time it takes to be heard; to wait: exactly there
            let start = if play { self.steer.aim(t) } else { t };
            self.waiting_at = (!play).then_some(t);
            mpv.set("speed", "1");
            mpv.set("start", &format!("{start:.6}"));
            mpv.set_flag("pause", !play);
            mpv.command(&["loadfile", path]);
            return;
        }
        if !self.ready {
            return;
        }
        // Where the sound is waiting: where it was last put, or where it stopped.
        let resting = self.waiting_at.or(self.pos.map(|p| p.0));
        let off = resting.map_or(f64::INFINITY, |at| (at - t).abs());
        let in_place = off <= if play { NEAR_ENOUGH } else { IN_PLACE };
        if !play {
            if !self.paused {
                self.paused = true;
                mpv.set_flag("pause", true);
            }
            if !in_place {
                self.waiting_at = Some(t);
                self.ready = false;
                self.pos = None;
                mpv.set("speed", "1");
                mpv.command(&["seek", &format!("{:.6}", t.max(0.0)), "absolute+exact"]);
            }
            return;
        }
        if self.paused {
            // Let it go. If it is not waiting where it is wanted, move it first, so that
            // nothing from the old place is heard.
            self.paused = false;
            self.waiting_at = None;
            if in_place {
                self.steer.resume();
                self.let_go = Some((std::time::Instant::now(), lead));
            } else {
                self.let_go = None;
                let to = self.steer.aim(t);
                if log {
                    eprintln!("lane starts at {to:.3} (wanted {t:.3}, was waiting at {resting:?})");
                }
                self.ready = false;
                self.pos = None;
                mpv.set("speed", "1");
                mpv.command(&["seek", &format!("{:.6}", to.max(0.0)), "absolute+exact"]);
            }
            mpv.set_flag("pause", false);
            self.settled = Some(std::time::Instant::now()); // starting takes a moment too
            return;
        }
        // Judge only what mpv has said since it settled: before that it is still starting.
        let Some((heard, said)) = self.pos else { return };
        if self.settled.is_none_or(|at| at.elapsed().as_secs_f64() < Self::SETTLE || said < at) {
            return;
        }
        // A sound let go ahead of its picture is not heard, and so cannot be judged, until the picture is there.
        if self.let_go.is_some_and(|(when, given)| when.elapsed().as_secs_f64() < given + Self::SETTLE) {
            return;
        }
        // mpv reports its position every so often; it has moved on by the time since the last report
        let now = heard + said.elapsed().as_secs_f64() * self.steer.speed();
        // Once a sound that was let go is surely being heard: how late was it, lead and all?
        if let Some((_, given)) = self.let_go.filter(|(when, given)| when.elapsed().as_secs_f64() >= given + 0.35) {
            self.let_go = None;
            self.start_lead = (0.3 * self.start_lead + 0.7 * (given + (t - now))).clamp(0.0, 0.6);
            if log {
                eprintln!("lane was let go {given:.3} early and was {:.3} late: next time {:.3} early", t - now, self.start_lead);
            }
        }
        match self.steer.step(t, t - now) {
            Step::Seek(to) => {
                if log {
                    eprintln!("lane jumps to {to:.3} (wanted {t:.3}, was at {now:.3})");
                }
                self.jumps += 1;
                self.ready = false;
                self.pos = None;
                mpv.set("speed", "1");
                mpv.command(&["seek", &format!("{:.6}", to.max(0.0)), "absolute+exact"]);
            }
            Step::Speed(speed) => {
                if log {
                    eprintln!("lane speed {speed:.3} (wanted {t:.3}, was at {now:.3})");
                }
                mpv.set("speed", &format!("{speed:.4}"));
            }
            Step::Hold => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a lane's player: after a jump the sound is heard `delay` seconds late.
    /// Runs `seconds` of playback in steps of 64 ms, as the window does, and returns how many
    /// jumps were made and how far out of step it ended.
    fn run(steer: &mut Steer, delay: f64, start_error: f64, seconds: f64) -> (u32, f64) {
        let (dt, mut want, mut heard, mut jumps) = (0.064, 10.0, 10.0 - start_error, 0);
        let mut settling = 0.0; // time left until the player has settled after a jump and is judged again
        for _ in 0..(seconds / dt) as usize {
            want += dt;
            heard += dt * steer.speed();
            if settling > 0.0 {
                settling -= dt;
            } else if let Step::Seek(to) = steer.step(want, want - heard) {
                jumps += 1;
                settling = 0.3;
                heard = to - delay; // what was aimed at is heard `delay` from now
            }
        }
        (jumps, want - heard)
    }

    #[test]
    fn a_late_start_is_closed_without_jumping() {
        // a quarter of a second behind, as after starting an MP3: no jump, and in step within a few seconds
        let mut steer = Steer::new();
        let (jumps, error) = run(&mut steer, 0.28, 0.25, 12.0);
        assert_eq!(jumps, 0);
        assert!(error.abs() < Steer::NEAR, "{error}");
        assert_eq!(steer.speed(), 1.0);
    }

    #[test]
    fn far_out_of_step_takes_one_jump_and_learns_the_delay() {
        let mut steer = Steer::new();
        let (jumps, error) = run(&mut steer, 0.28, 3.0, 12.0);
        assert_eq!(jumps, 1);
        assert!(error.abs() < Steer::NEAR, "{error}");
        // the next jump lands closer, because the delay has been learnt
        assert!((steer.lead - 0.28).abs() < 0.05, "{}", steer.lead);
        let (jumps, error) = run(&mut steer, 0.28, -5.0, 1.0);
        assert_eq!(jumps, 1);
        assert!(error.abs() < 0.06, "{error}");
    }

    #[test]
    fn in_step_is_left_alone() {
        let mut steer = Steer::new();
        for wobble in [0.02, -0.03, 0.01, -0.02, 0.03, 0.0] {
            assert_eq!(steer.step(5.0, wobble), Step::Hold);
        }
        assert_eq!(steer.speed(), 1.0);
    }
}
