//! Custom-drawn widgets: trim bar, montage timeline, crop overlay.
//!
//! Each widget is told what to show with update(...) and reports gestures through
//! callbacks. None of them changes the session itself. The geometry lives in geom.rs.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{cairo, gdk, glib};
use pve_core::model::Rect;

use crate::geom::{CropGeom, CropHit, SoundBox, TimelineGeom, TrimGeom, TrimHit, KNOB, PAD};

const ACCENT: (f64, f64, f64) = (0.21, 0.52, 0.89);
const SOUND: (f64, f64, f64) = (0.18, 0.62, 0.45);
const PLAYHEAD: (f64, f64, f64) = (0.9, 0.2, 0.2);

type Rgba = (f64, f64, f64, f64);

fn fg(w: &impl IsA<gtk::Widget>, alpha: f64) -> Rgba {
    let c = w.color();
    (c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64 * alpha)
}

fn with(c: (f64, f64, f64), a: f64) -> Rgba {
    (c.0, c.1, c.2, a)
}

fn fill(cr: &cairo::Context, c: Rgba, x: f64, y: f64, w: f64, h: f64) {
    if w > 0.0 && h > 0.0 {
        cr.set_source_rgba(c.0, c.1, c.2, c.3);
        cr.rectangle(x, y, w, h);
        let _ = cr.fill();
    }
}

fn text(area: &gtk::DrawingArea, cr: &cairo::Context, s: &str, c: Rgba, x: f64, y_mid: f64, clip: Option<(f64, f64, f64, f64)>) {
    let layout = area.create_pango_layout(Some(s));
    let _ = cr.save();
    if let Some((cx, cy, cw, ch)) = clip {
        cr.rectangle(cx, cy, cw.max(0.0), ch.max(0.0));
        cr.clip();
    }
    cr.set_source_rgba(c.0, c.1, c.2, c.3);
    cr.move_to(x, y_mid - layout.pixel_size().1 as f64 / 2.0);
    pangocairo::functions::show_layout(cr, &layout);
    let _ = cr.restore();
}

type Callback<A> = RefCell<Option<Box<dyn Fn(A)>>>;

fn call<A>(cb: &Callback<A>, a: A) {
    if let Some(f) = cb.borrow().as_ref() {
        f(a);
    }
}

// ---------------------------------------------------------------- trim bar

struct TrimState {
    geom: TrimGeom,
    keyframes: Rc<Vec<f64>>,
    /// Where a lossless export would really cut.
    snapped: Option<(f64, f64)>,
    /// Identifies what is shown, to reset the zoom when it changes.
    key: Option<(String, bool)>,
    drag: Option<TrimHit>,
    x0: f64,
    pointer: (f64, f64),
}

/// The scroll wheel zooms around the pointer; Shift+scroll pans.
#[derive(Clone)]
pub struct TrimBar {
    pub area: gtk::DrawingArea,
    st: Rc<RefCell<TrimState>>,
    /// (time, final): the play position is dragged or clicked.
    pub on_scrub: Rc<Callback<(f64, bool)>>,
    /// (which, time, final): the in or out point is dragged.
    pub on_trim: Rc<Callback<(TrimHit, f64, bool)>>,
}

impl TrimBar {
    pub fn new() -> TrimBar {
        let area = gtk::DrawingArea::builder().hexpand(true).content_height(66).build();
        let st = Rc::new(RefCell::new(TrimState {
            geom: TrimGeom { width: 0.0, height: 66.0, duration: 0.0, start: 0.0, end: 0.0, pos: 0.0, view: (0.0, 0.0) },
            keyframes: Rc::new(Vec::new()),
            snapped: None,
            key: None,
            drag: None,
            x0: 0.0,
            pointer: (0.0, 0.0),
        }));
        let bar = TrimBar { area, st, on_scrub: Rc::new(RefCell::new(None)), on_trim: Rc::new(RefCell::new(None)) };

        let s = bar.st.clone();
        bar.area.set_draw_func(move |area, cr, w, h| {
            let mut st = s.borrow_mut();
            st.geom.width = w as f64;
            st.geom.height = h as f64;
            draw_trim(area, cr, &st);
        });

        let drag = gtk::GestureDrag::new();
        let b = bar.clone();
        drag.connect_drag_begin(move |_, x, y| {
            let hit = {
                let mut st = b.st.borrow_mut();
                st.geom.width = b.area.width() as f64;
                st.x0 = x;
                st.drag = st.geom.hit(x, y);
                st.drag
            };
            if hit == Some(TrimHit::Play) {
                b.area.set_cursor_from_name(Some("grabbing"));
            }
            b.moved(x, false);
        });
        let b = bar.clone();
        drag.connect_drag_update(move |_, dx, _| {
            let x = b.st.borrow().x0 + dx;
            b.moved(x, false);
        });
        let b = bar.clone();
        drag.connect_drag_end(move |_, dx, _| {
            let x = b.st.borrow().x0 + dx;
            b.moved(x, true);
        });
        bar.area.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        let b = bar.clone();
        motion.connect_motion(move |_, x, y| {
            b.st.borrow_mut().pointer = (x, y);
            b.update_cursor();
        });
        bar.area.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let b = bar.clone();
        scroll.connect_scroll(move |c, _, dy| {
            let shift = c.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            b.scroll(dy, shift);
            glib::Propagation::Stop
        });
        bar.area.add_controller(scroll);
        bar
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(&self, key: Option<(String, bool)>, duration: f64, start: f64, end: f64, pos: f64, keyframes: Rc<Vec<f64>>, snapped: Option<(f64, f64)>) {
        {
            let mut st = self.st.borrow_mut();
            if key != st.key || duration != st.geom.duration {
                st.key = key;
                st.geom.view = (0.0, duration);
            }
            st.geom.duration = duration;
            st.geom.start = start;
            st.geom.end = end;
            st.geom.pos = pos;
            st.keyframes = keyframes;
            st.snapped = snapped;
        }
        self.area.queue_draw();
    }

    /// Move the play position. `follow` scrolls a zoomed view to keep it visible (during playback).
    pub fn set_pos(&self, pos: f64, follow: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.geom.pos = pos;
            if follow && st.drag.is_none() {
                st.geom.follow();
            }
        }
        self.area.queue_draw();
    }

    pub fn geom(&self) -> TrimGeom {
        let mut g = self.st.borrow().geom.clone();
        g.width = self.area.width() as f64;
        g.height = self.area.height() as f64;
        g
    }

    pub fn scroll(&self, dy: f64, shift: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.geom.width = self.area.width() as f64;
            let x = st.pointer.0;
            if shift {
                st.geom.pan(dy);
            } else {
                st.geom.zoom(x, dy);
            }
        }
        self.area.queue_draw();
    }

    fn update_cursor(&self) {
        let name = {
            let mut st = self.st.borrow_mut();
            if st.drag.is_some() {
                return;
            }
            st.geom.width = self.area.width() as f64;
            st.geom.height = self.area.height() as f64;
            st.geom.cursor(st.pointer.0, st.pointer.1)
        };
        self.area.set_cursor_from_name(name);
    }

    fn moved(&self, x: f64, last: bool) {
        let (drag, t) = {
            let mut st = self.st.borrow_mut();
            let Some(drag) = st.drag else { return };
            let t = st.geom.t(x);
            if last {
                st.drag = None;
            }
            (drag, t)
        };
        match drag {
            TrimHit::Play => call(&self.on_scrub, (t, last)),
            which => call(&self.on_trim, (which, t, last)),
        }
        if last {
            self.update_cursor();
        }
    }

    /// For tests: a press, a move and a release at widget x positions.
    pub fn simulate_drag(&self, x0: f64, y0: f64, x1: f64) {
        {
            let mut st = self.st.borrow_mut();
            st.geom.width = self.area.width() as f64;
            st.geom.height = self.area.height() as f64;
            st.x0 = x0;
            st.drag = st.geom.hit(x0, y0);
        }
        self.moved(x0, false);
        self.moved(x1, false);
        self.moved(x1, true);
    }

    pub fn set_pointer(&self, x: f64, y: f64) {
        self.st.borrow_mut().pointer = (x, y);
    }
}

fn draw_trim(area: &gtk::DrawingArea, cr: &cairo::Context, st: &TrimState) {
    let g = &st.geom;
    if g.duration <= 0.0 {
        return;
    }
    let (w, h) = (g.width, g.height);
    let (top, bot) = (TrimGeom::TOP, h - TrimGeom::BOTTOM);
    let (v0, v1) = g.view;
    let (left, right) = (PAD, w - PAD);
    // Clamp a time range to the visible part of the bar, as (x, width).
    let span = |a: f64, b: f64| {
        let (xa, xb) = (g.x(a).max(left), g.x(b).min(right));
        (xa, xb - xa)
    };
    fill(cr, fg(area, 0.12), left, top, right - left, bot - top); // the source
    if let Some((s0, s1)) = st.snapped {
        // what a lossless export would include beyond the kept range
        for (a, b) in [(s0, g.start), (g.end, s1)] {
            let (x, cw) = span(a, b);
            fill(cr, with(ACCENT, 0.28), x, top, cw, bot - top);
        }
    }
    let (x, cw) = span(g.start, g.end);
    if cw > -1.0 {
        fill(cr, with(ACCENT, 0.75), x, top, cw.max(1.0), bot - top); // kept range
    }
    // keyframe ticks, when not too dense to read
    let lo = st.keyframes.partition_point(|k| *k < v0);
    let hi = st.keyframes.partition_point(|k| *k <= v1);
    if hi > lo && (right - left) / (hi - lo) as f64 >= 3.0 {
        let tick = fg(area, 0.6);
        for k in &st.keyframes[lo..hi] {
            fill(cr, tick, g.x(*k).round(), bot - 5.0, 1.0, 5.0);
        }
    }
    // in and out: a line through the source and a knob below, hanging outside the kept range
    let white = fg(area, 1.0);
    for (t, west) in [(g.start, true), (g.end, false)] {
        if (v0..=v1).contains(&t) {
            let x = g.x(t);
            fill(cr, white, x - 1.0, top - 2.0, 2.0, bot - top + 4.0);
            fill(cr, white, if west { x - KNOB } else { x }, bot + 2.0, KNOB, KNOB);
        }
    }
    // play position: a line and a knob above
    if (v0..=v1).contains(&g.pos) {
        let x = g.x(g.pos);
        fill(cr, with(PLAYHEAD, 1.0), x - 1.0, top - 2.0, 2.0, bot - top + 4.0);
        fill(cr, with(PLAYHEAD, 1.0), x - KNOB / 2.0, 2.0, KNOB, KNOB);
    }
    // when zoomed in: a thin strip showing which part of the source is visible
    if v1 - v0 < g.duration {
        fill(cr, fg(area, 0.15), left, h - 2.0, right - left, 2.0);
        fill(cr, fg(area, 0.7), left + v0 / g.duration * (right - left), h - 2.0, ((v1 - v0) / g.duration * (right - left)).max(3.0), 2.0);
    }
}

// ---------------------------------------------------------------- montage timeline

#[derive(Clone, Copy, PartialEq)]
enum TimelineDrag {
    /// Index of the clip being dragged.
    Clip(usize),
    /// Seconds from the start of the sound item to where it was grabbed.
    Sound(f64),
}

struct TimelineState {
    geom: TimelineGeom,
    selected: Option<usize>,
    sel_sound: Option<(usize, usize)>,
    pos: f64,
    drag: Option<TimelineDrag>,
    moved: bool,
    origin: (f64, f64),
}

#[derive(Clone)]
pub struct Timeline {
    pub area: gtk::DrawingArea,
    st: Rc<RefCell<TimelineState>>,
    /// (index, fraction): a clip is clicked.
    pub on_select: Rc<Callback<(usize, f64)>>,
    /// (src, dst, final): a clip is dragged to a new position.
    pub on_move: Rc<Callback<(usize, usize, bool)>>,
    /// (lane, index): a sound item is pressed.
    pub on_sound_select: Rc<Callback<(usize, usize)>>,
    /// (at, lane, final): the selected sound item is dragged.
    pub on_sound_move: Rc<Callback<(f64, Option<usize>, bool)>>,
}

impl Timeline {
    pub fn new(lanes: usize) -> Timeline {
        let area = gtk::DrawingArea::builder().hexpand(true).content_height(TimelineGeom::height(lanes) as i32).build();
        let st = Rc::new(RefCell::new(TimelineState {
            geom: TimelineGeom { lanes, ..TimelineGeom::default() },
            selected: None,
            sel_sound: None,
            pos: 0.0,
            drag: None,
            moved: false,
            origin: (0.0, 0.0),
        }));
        let tl = Timeline {
            area,
            st,
            on_select: Rc::new(RefCell::new(None)),
            on_move: Rc::new(RefCell::new(None)),
            on_sound_select: Rc::new(RefCell::new(None)),
            on_sound_move: Rc::new(RefCell::new(None)),
        };
        let s = tl.st.clone();
        tl.area.set_draw_func(move |area, cr, w, h| {
            let mut st = s.borrow_mut();
            st.geom.width = w as f64;
            draw_timeline(area, cr, &st, h as f64);
        });

        let drag = gtk::GestureDrag::new();
        let t = tl.clone();
        drag.connect_drag_begin(move |_, x, y| t.press(x, y));
        let t = tl.clone();
        drag.connect_drag_update(move |_, dx, dy| t.drag(dx, dy));
        let t = tl.clone();
        drag.connect_drag_end(move |_, _, _| t.release());
        tl.area.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        let t = tl.clone();
        motion.connect_motion(move |_, x, y| {
            let over = {
                let mut st = t.st.borrow_mut();
                st.geom.width = t.area.width() as f64;
                st.drag.is_none() && st.geom.sound_hit(x, y).is_some()
            };
            if t.st.borrow().drag.is_none() {
                t.area.set_cursor_from_name(over.then_some("grab"));
            }
        });
        tl.area.add_controller(motion);
        tl
    }

    pub fn update(&self, clips: Vec<(String, f64)>, selected: Option<usize>, sounds: Vec<Vec<SoundBox>>, sel_sound: Option<(usize, usize)>, pos: f64) {
        {
            let mut st = self.st.borrow_mut();
            st.geom.clips = clips;
            st.geom.sounds = sounds;
            st.selected = selected;
            st.sel_sound = sel_sound;
            st.pos = pos;
        }
        self.area.queue_draw();
    }

    pub fn set_pos(&self, pos: f64) {
        self.st.borrow_mut().pos = pos;
        self.area.queue_draw();
    }

    pub fn geom(&self) -> TimelineGeom {
        let mut g = self.st.borrow().geom.clone();
        g.width = self.area.width() as f64;
        g
    }

    pub fn press(&self, x: f64, y: f64) {
        let sound = {
            let mut st = self.st.borrow_mut();
            st.geom.width = self.area.width() as f64;
            st.origin = (x, y);
            st.moved = false;
            match st.geom.sound_hit(x, y) {
                Some((lane, i)) => {
                    st.drag = Some(TimelineDrag::Sound(st.geom.t(x) - st.geom.sounds[lane][i].at));
                    Some((lane, i))
                }
                None => {
                    st.drag = st.geom.clip_hit(x, y).map(|h| TimelineDrag::Clip(h.0));
                    None
                }
            }
        };
        if let Some(hit) = sound {
            call(&self.on_sound_select, hit);
            self.area.set_cursor_from_name(Some("grabbing"));
        }
    }

    pub fn drag(&self, dx: f64, dy: f64) {
        enum Act {
            Sound(f64, Option<usize>),
            Clip(usize, usize),
        }
        let act = {
            let mut st = self.st.borrow_mut();
            let Some(drag) = st.drag else { return };
            if dx.abs() < 6.0 && dy.abs() < 6.0 && !st.moved {
                return;
            }
            let (x, y) = (st.origin.0 + dx, st.origin.1 + dy);
            match drag {
                TimelineDrag::Sound(grab) => {
                    st.moved = true;
                    Some(Act::Sound((st.geom.t(x) - grab).max(0.0), st.geom.lane_at(y)))
                }
                TimelineDrag::Clip(src) => match st.geom.clip_hit(x, TimelineGeom::CLIP_Y + 1.0) {
                    Some((dst, _)) if dst != src => {
                        st.drag = Some(TimelineDrag::Clip(dst));
                        st.moved = true;
                        Some(Act::Clip(src, dst))
                    }
                    _ => None,
                },
            }
        };
        match act {
            Some(Act::Sound(at, lane)) => call(&self.on_sound_move, (at, lane, false)),
            Some(Act::Clip(src, dst)) => call(&self.on_move, (src, dst, false)),
            None => {}
        }
    }

    pub fn release(&self) {
        let (drag, moved, origin, click) = {
            let mut st = self.st.borrow_mut();
            let Some(drag) = st.drag.take() else { return };
            (drag, st.moved, st.origin, st.geom.clip_hit(st.origin.0, st.origin.1))
        };
        let _ = origin;
        match (drag, moved) {
            (TimelineDrag::Sound(_), true) => call(&self.on_sound_move, (0.0, None, true)),
            (TimelineDrag::Clip(i), true) => call(&self.on_move, (i, i, true)),
            (TimelineDrag::Clip(_), false) => {
                if let Some(hit) = click {
                    call(&self.on_select, hit);
                }
            }
            _ => {}
        }
        self.area.set_cursor_from_name(None);
    }
}

fn draw_timeline(area: &gtk::DrawingArea, cr: &cairo::Context, st: &TimelineState, h: f64) {
    let g = &st.geom;
    let w = g.width;
    let clip_mode = st.sel_sound.is_none();
    for (i, ((x, cw), (label, secs))) in g.spans().iter().zip(&g.clips).enumerate() {
        let sel = st.selected == Some(i);
        let b = (x + 1.0, TimelineGeom::CLIP_Y, (cw - 2.0).max(1.0), TimelineGeom::CLIP_H);
        fill(cr, if sel { with(ACCENT, if clip_mode { 0.75 } else { 0.35 }) } else { fg(area, 0.14) }, b.0, b.1, b.2, b.3);
        text(area, cr, &format!("{label}  {secs:.1}s"), if sel { (1.0, 1.0, 1.0, 1.0) } else { fg(area, 1.0) }, x + 8.0, b.1 + b.3 / 2.0, Some(b));
    }
    let any_sound = g.sounds.iter().any(|l| !l.is_empty());
    for lane in 0..g.lanes {
        let y = TimelineGeom::lane_y(lane);
        fill(cr, fg(area, 0.06), PAD, y, w - 2.0 * PAD, TimelineGeom::LANE_H);
        let items: &[SoundBox] = g.sounds.get(lane).map_or(&[], |v| v.as_slice());
        if lane == 0 && !any_sound {
            text(area, cr, "sound: press Add sound, or drop a sound file", fg(area, 0.4), PAD + 8.0, y + TimelineGeom::LANE_H / 2.0, None);
        }
        let lane_clip = (PAD, y, w - 2.0 * PAD, TimelineGeom::LANE_H);
        for (i, s) in items.iter().enumerate() {
            let sel = st.sel_sound == Some((lane, i));
            let x0 = g.x(s.at).max(PAD);
            let x1 = g.x(s.at + s.seconds).max(g.x(s.at) + 4.0).min(w - PAD);
            if x1 <= x0 {
                continue;
            }
            let b = (x0, y, x1 - x0, TimelineGeom::LANE_H);
            fill(cr, with(SOUND, if sel { 0.95 } else { 0.5 }), b.0, b.1, b.2, b.3);
            if sel {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
                cr.set_line_width(1.0);
                cr.rectangle(b.0 + 0.5, b.1 + 0.5, b.2 - 1.0, b.3 - 1.0);
                let _ = cr.stroke();
            }
            text(area, cr, &s.label, (1.0, 1.0, 1.0, 1.0), b.0 + 6.0, y + TimelineGeom::LANE_H / 2.0, Some(b));
        }
        let _ = lane_clip;
    }
    if g.total() > 0.0 {
        fill(cr, with(PLAYHEAD, 1.0), g.x(st.pos).min(w - PAD) - 1.0, 0.0, 2.0, h);
    }
}

// ---------------------------------------------------------------- crop overlay

struct CropState {
    video: (f64, f64),
    rect: Option<Rect>,
    /// w/h to hold, or None.
    aspect: Option<f64>,
    drag: Option<(CropHit, (f64, f64), Rect)>,
}

/// Crop rectangle drawn over the preview; everything outside it is dimmed.
#[derive(Clone)]
pub struct CropOverlay {
    pub area: gtk::DrawingArea,
    st: Rc<RefCell<CropState>>,
    /// (rect, final) while dragging the rectangle or a corner.
    pub on_change: Rc<Callback<(Rect, bool)>>,
}

impl CropOverlay {
    pub fn new() -> CropOverlay {
        let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).can_target(false).build();
        let st = Rc::new(RefCell::new(CropState { video: (0.0, 0.0), rect: None, aspect: None, drag: None }));
        let ov = CropOverlay { area, st, on_change: Rc::new(RefCell::new(None)) };

        let s = ov.st.clone();
        ov.area.set_draw_func(move |_, cr, w, h| draw_crop(cr, &s.borrow(), w as f64, h as f64));

        let drag = gtk::GestureDrag::new();
        let o = ov.clone();
        drag.connect_drag_begin(move |_, x, y| o.press(x, y));
        let o = ov.clone();
        drag.connect_drag_update(move |_, dx, dy| o.drag(dx, dy, false));
        let o = ov.clone();
        drag.connect_drag_end(move |_, dx, dy| o.drag(dx, dy, true));
        ov.area.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        let o = ov.clone();
        motion.connect_motion(move |_, x, y| {
            let name = {
                let st = o.st.borrow();
                if st.drag.is_some() {
                    return;
                }
                st.rect.and_then(|r| o.geom_with(st.video).hit(&r, x, y)).map(CropGeom::cursor)
            };
            o.area.set_cursor_from_name(name);
        });
        ov.area.add_controller(motion);
        ov
    }

    fn geom_with(&self, video: (f64, f64)) -> CropGeom {
        CropGeom { width: self.area.width() as f64, height: self.area.height() as f64, video }
    }

    pub fn geom(&self) -> CropGeom {
        self.geom_with(self.st.borrow().video)
    }

    pub fn rect(&self) -> Option<Rect> {
        self.st.borrow().rect
    }

    pub fn update(&self, video: (f64, f64), rect: Option<Rect>, aspect: Option<f64>) {
        {
            let mut st = self.st.borrow_mut();
            st.video = video;
            st.rect = rect;
            st.aspect = aspect;
        }
        self.area.set_can_target(rect.is_some()); // with no crop, clicks fall through
        self.area.queue_draw();
    }

    pub fn press(&self, x: f64, y: f64) {
        let mut st = self.st.borrow_mut();
        let g = self.geom_with(st.video);
        st.drag = st.rect.and_then(|r| Some((g.hit(&r, x, y)?, g.video_point(x, y)?, r)));
    }

    pub fn drag(&self, dx: f64, dy: f64, last: bool) {
        let rect = {
            let mut st = self.st.borrow_mut();
            let Some((hit, start, orig)) = st.drag else { return };
            if last {
                st.drag = None;
            }
            self.geom_with(st.video).drag(hit, &orig, start, dx, dy, st.aspect)
        };
        if let Some(r) = rect {
            call(&self.on_change, (r, last));
        }
    }
}

fn draw_crop(cr: &cairo::Context, st: &CropState, w: f64, h: f64) {
    let g = CropGeom { width: w, height: h, video: st.video };
    let (ox, oy, s) = g.area();
    let Some(r) = st.rect.filter(|_| s > 0.0) else { return };
    let (x, y, rw, rh) = (ox + r.x as f64 * s, oy + r.y as f64 * s, r.w as f64 * s, r.h as f64 * s);
    let (vw, vh) = (st.video.0 * s, st.video.1 * s);
    // dim the part of the picture outside the crop: four bands around it
    let dim = (0.0, 0.0, 0.0, 0.6);
    fill(cr, dim, ox, oy, vw, y - oy);
    fill(cr, dim, ox, y + rh, vw, oy + vh - (y + rh));
    fill(cr, dim, ox, y, x - ox, rh);
    fill(cr, dim, x + rw, y, ox + vw - (x + rw), rh);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
    cr.set_line_width(1.5);
    cr.rectangle(x, y, rw, rh);
    let _ = cr.stroke();
    for (cx, cy) in [(x, y), (x + rw, y), (x, y + rh), (x + rw, y + rh)] {
        fill(cr, (1.0, 1.0, 1.0, 0.95), cx - 5.0, cy - 5.0, 10.0, 10.0);
    }
}
