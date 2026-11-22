//! Custom-drawn widgets: trim bar, montage timeline, crop overlay.
//!
//! Each widget is told what to show with update(...) and reports gestures through
//! callbacks. None of them changes the session itself. The geometry lives in geom.rs.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{cairo, gdk, glib, pango};
use pve_core::model::Rect;

use crate::app::{clock, short_clock};
use crate::geom::{CropGeom, CropHit, SoundBox, TimelineGeom, TrimGeom, TrimHit, TrimLabel, KNOB, PAD};

const ACCENT: (f64, f64, f64) = (0.21, 0.52, 0.89);
const SOUND: (f64, f64, f64) = (0.18, 0.62, 0.45);
const PLAYHEAD: (f64, f64, f64) = (0.9, 0.2, 0.2);
/// Half way between the clips' colour and the sounds': the link between a clip and its own sound.
const LINK: (f64, f64, f64) = ((ACCENT.0 + SOUND.0) / 2.0, (ACCENT.1 + SOUND.1) / 2.0, (ACCENT.2 + SOUND.2) / 2.0);

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

fn triangle(cr: &cairo::Context, c: Rgba, p: [(f64, f64); 3]) {
    cr.set_source_rgba(c.0, c.1, c.2, c.3);
    cr.move_to(p[0].0, p[0].1);
    cr.line_to(p[1].0, p[1].1);
    cr.line_to(p[2].0, p[2].1);
    cr.close_path();
    let _ = cr.fill();
}

/// Text as the bars write it: a little smaller than the window's, with digits of equal width
/// so a running time does not shiver.
fn layout(area: &gtk::DrawingArea, s: &str) -> pango::Layout {
    let layout = area.create_pango_layout(Some(s));
    let attrs = pango::AttrList::new();
    attrs.insert(pango::AttrFloat::new_scale(0.9));
    attrs.insert(pango::AttrFontFeatures::new("tnum"));
    layout.set_attributes(Some(&attrs));
    layout
}

fn text_width(area: &gtk::DrawingArea, s: &str) -> f64 {
    layout(area, s).pixel_size().0 as f64
}

fn text(area: &gtk::DrawingArea, cr: &cairo::Context, s: &str, c: Rgba, x: f64, y_mid: f64, clip: Option<(f64, f64, f64, f64)>) {
    let layout = layout(area, s);
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
    /// The bar shows only the kept range.
    fit: bool,
    drag: Option<TrimHit>,
    /// A time was pressed; on release it is typed over.
    pressed: Option<TrimLabel>,
    /// The time being typed over: a text box covers it, so it is not drawn.
    editing: Option<TrimLabel>,
    x0: f64,
    pointer: (f64, f64),
}

/// One of the times written on the bar: which, its text, its left edge and its width.
type Label = (TrimLabel, String, f64, f64);

/// The scroll wheel zooms around the pointer; Shift+scroll pans.
#[derive(Clone)]
pub struct TrimBar {
    pub area: gtk::DrawingArea,
    st: Rc<RefCell<TrimState>>,
    /// (time, final): the play position is dragged or clicked.
    pub on_scrub: Rc<Callback<(f64, bool)>>,
    /// (which, time, final): the in or out point is dragged.
    pub on_trim: Rc<Callback<(TrimHit, f64, bool)>>,
    /// (which, left edge, width): one of the times is clicked, to type over it.
    pub on_edit: Rc<Callback<(TrimLabel, f64, f64)>>,
    /// The wheel was turned while the bar showed only the kept range: it no longer does.
    pub on_unfit: Rc<Callback<()>>,
}

impl TrimBar {
    pub fn new() -> TrimBar {
        let area = gtk::DrawingArea::builder().hexpand(true).content_height(TrimGeom::HEIGHT as i32).build();
        let st = Rc::new(RefCell::new(TrimState {
            geom: TrimGeom { width: 0.0, height: TrimGeom::HEIGHT, duration: 0.0, start: 0.0, end: 0.0, pos: 0.0, view: (0.0, 0.0) },
            keyframes: Rc::new(Vec::new()),
            snapped: None,
            key: None,
            fit: false,
            drag: None,
            pressed: None,
            editing: None,
            x0: 0.0,
            pointer: (0.0, 0.0),
        }));
        let bar = TrimBar {
            area,
            st,
            on_scrub: Rc::new(RefCell::new(None)),
            on_trim: Rc::new(RefCell::new(None)),
            on_edit: Rc::new(RefCell::new(None)),
            on_unfit: Rc::new(RefCell::new(None)),
        };

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
            let label = b.label_at(x, y);
            {
                let mut st = b.st.borrow_mut();
                st.geom.width = b.area.width() as f64;
                st.x0 = x;
                st.pressed = label;
                st.drag = if label.is_some() { None } else { st.geom.hit(x, y) };
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
            let pressed = b.st.borrow_mut().pressed.take();
            if let Some(which) = pressed.filter(|_| dx.abs() < 4.0) {
                b.edit(which);
            }
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
            // While a knob is being dragged the bar keeps still under it; it fits again on release.
            if st.fit && st.drag.is_none() && end > start {
                st.geom.view = (start, end);
            }
        }
        self.area.queue_draw();
    }

    /// Show only the kept range (true), or the whole source.
    pub fn set_fit(&self, fit: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.fit = fit;
            let g = &mut st.geom;
            g.view = if fit && g.end > g.start { (g.start, g.end) } else { (0.0, g.duration) };
        }
        self.area.queue_draw();
    }

    pub fn fit(&self) -> bool {
        self.st.borrow().fit
    }

    /// The span "play this clip" covers: what the bar stands for. The whole source, or the
    /// kept range when the bar shows only that.
    pub fn span(&self) -> (f64, f64) {
        let st = self.st.borrow();
        if st.fit {
            (st.geom.start, st.geom.end)
        } else {
            (0.0, st.geom.duration)
        }
    }

    /// The times written on the bar now, and where.
    fn labels(&self) -> Vec<Label> {
        let st = self.st.borrow();
        let mut g = st.geom.clone();
        g.width = self.area.width() as f64;
        trim_labels(&self.area, &g)
    }

    fn label_at(&self, x: f64, y: f64) -> Option<TrimLabel> {
        let h = self.area.height() as f64;
        self.labels().into_iter().find(|(which, _, lx, lw)| {
            let band = if *which == TrimLabel::Pos { y < TrimGeom::TOP } else { y >= h - TrimGeom::BOTTOM };
            band && (lx - 2.0..=lx + lw + 2.0).contains(&x)
        }).map(|l| l.0)
    }

    /// Ask for one of the times to be typed over (a click on it does this).
    pub fn edit(&self, which: TrimLabel) {
        if let Some((_, _, x, w)) = self.labels().into_iter().find(|l| l.0 == which) {
            call(&self.on_edit, (which, x, w));
        }
    }

    /// The time that a text box is covering, if any.
    pub fn set_editing(&self, which: Option<TrimLabel>) {
        self.st.borrow_mut().editing = which;
        self.area.queue_draw();
    }

    /// For tests: the times as written: (play position, in, out, length).
    pub fn texts(&self) -> (String, String, String, String) {
        let find = |which| self.labels().into_iter().find(|l| l.0 == which).map(|l| l.1).unwrap_or_default();
        let g = self.st.borrow().geom.clone();
        (find(TrimLabel::Pos), find(TrimLabel::In), find(TrimLabel::Out), length_text(&g))
    }

    /// Move the play position. `follow` scrolls a zoomed view to keep it visible (during playback).
    /// A bar that shows only the kept range stays on it.
    pub fn set_pos(&self, pos: f64, follow: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.geom.pos = pos;
            if follow && st.drag.is_none() && !st.fit {
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
        let unfit = {
            let mut st = self.st.borrow_mut();
            st.geom.width = self.area.width() as f64;
            let x = st.pointer.0;
            if shift {
                st.geom.pan(dy);
            } else {
                st.geom.zoom(x, dy);
            }
            std::mem::take(&mut st.fit)
        };
        if unfit {
            call(&self.on_unfit, ());
        }
        self.area.queue_draw();
    }

    fn update_cursor(&self) {
        let pointer = self.st.borrow().pointer;
        let over_time = self.label_at(pointer.0, pointer.1).is_some();
        let name = {
            let mut st = self.st.borrow_mut();
            if st.drag.is_some() {
                return;
            }
            st.geom.width = self.area.width() as f64;
            st.geom.height = self.area.height() as f64;
            if over_time { Some("text") } else { st.geom.cursor(pointer.0, pointer.1) }
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

    /// For tests: draw the bar as it is now. Works while the window is hidden.
    pub fn paint(&self, cr: &cairo::Context) {
        let mut st = self.st.borrow_mut();
        st.geom.width = self.area.width() as f64;
        st.geom.height = self.area.height() as f64;
        draw_trim(&self.area, cr, &st);
    }
}

fn length_text(g: &TrimGeom) -> String {
    format!("{:.3} s", g.end - g.start)
}

/// The three times and where they go. A cut point outside a zoomed view has its time at
/// the edge it lies beyond, with an arrow.
fn trim_labels(area: &gtk::DrawingArea, g: &TrimGeom) -> Vec<Label> {
    let mut out = Vec::new();
    if g.duration <= 0.0 {
        return out;
    }
    let pos = clock(g.line_pos());
    let w = text_width(area, &pos);
    if let Some(x) = g.pos_label_x(w) {
        out.push((TrimLabel::Pos, pos, x, w));
    }
    let start = if g.start < g.view.0 { format!("◂ {}", clock(g.start)) } else { clock(g.start) };
    let end = if g.end > g.view.1 { format!("{} ▸", clock(g.end)) } else { clock(g.end) };
    let (w_in, w_out) = (text_width(area, &start), text_width(area, &end));
    let (x_in, x_out) = g.cut_label_x(w_in, w_out);
    if let Some(x) = x_in {
        out.push((TrimLabel::In, start, x, w_in));
    }
    if let Some(x) = x_out {
        out.push((TrimLabel::Out, end, x, w_out));
    }
    out
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
    // the kept range has the colour of what is being trimmed: a clip, or a sound
    let kind = if st.key.as_ref().is_some_and(|(_, clip)| !clip) { SOUND } else { ACCENT };
    if let Some((s0, s1)) = st.snapped {
        // what a lossless export would include beyond the kept range
        for (a, b) in [(s0, g.start), (g.end, s1)] {
            let (x, cw) = span(a, b);
            fill(cr, with(kind, 0.28), x, top, cw, bot - top);
        }
    }
    let (x, cw) = span(g.start, g.end);
    if cw > -1.0 {
        fill(cr, with(kind, 0.75), x, top, cw.max(1.0), bot - top); // kept range
    }
    // read-only times, inside the bar: the length of the kept range in the middle of it,
    // and the length of the source where the source ends
    let mid = (top + bot) / 2.0;
    let length = length_text(g);
    let lw = text_width(area, &length);
    let length_end = if lw + 12.0 <= cw {
        text(area, cr, &length, (1.0, 1.0, 1.0, 0.95), x + (cw - lw) / 2.0, mid, None);
        x + (cw + lw) / 2.0
    } else {
        left
    };
    if v1 >= g.duration {
        let total = clock(g.duration);
        let tx = right - 6.0 - text_width(area, &total);
        if tx >= length_end + 12.0 {
            text(area, cr, &total, fg(area, 0.65), tx, mid, None);
        }
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
    // in and out: a line through the source, and below it a triangle pointing at the line
    // from outside the kept range (in points right, out points left)
    let white = fg(area, 1.0);
    let ky = bot + 3.0;
    for (t, side) in [(g.start, -1.0), (g.end, 1.0)] {
        if (v0..=v1).contains(&t) {
            let x = g.x(t);
            fill(cr, white, x - 1.0, top - 2.0, 2.0, ky + KNOB - top + 2.0);
            triangle(cr, white, [(x + side * KNOB, ky), (x + side * KNOB, ky + KNOB), (x, ky + KNOB / 2.0)]);
        }
    }
    // play position: a line, and a triangle above pointing down at it
    let pos = g.line_pos();
    if (v0..=v1).contains(&pos) {
        let x = g.x(pos);
        let red = with(PLAYHEAD, 1.0);
        fill(cr, red, x - 1.0, 2.0 + KNOB / 2.0, 2.0, bot + 2.0 - (2.0 + KNOB / 2.0));
        triangle(cr, red, [(x - KNOB / 2.0, 2.0), (x + KNOB / 2.0, 2.0), (x, 2.0 + KNOB)]);
    }
    // the times that can be typed over, each beside its knob
    for (which, label, x, _) in trim_labels(area, g) {
        if st.editing != Some(which) {
            let y = if which == TrimLabel::Pos { 2.0 + KNOB / 2.0 } else { ky + KNOB / 2.0 };
            text(area, cr, &label, fg(area, 0.9), x, y, None);
        }
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
    /// The play line; pixels from the pointer to the line where it was grabbed.
    Play(f64),
}

struct TimelineState {
    geom: TimelineGeom,
    selected: Option<usize>,
    sel_sound: Option<(usize, usize)>,
    /// Per lane: not heard in the preview.
    muted: Vec<bool>,
    pos: f64,
    drag: Option<TimelineDrag>,
    moved: bool,
    origin: (f64, f64),
    /// Where the pointer was last seen during a drag.
    last: (f64, f64),
}

#[derive(Clone)]
pub struct Timeline {
    pub area: gtk::DrawingArea,
    st: Rc<RefCell<TimelineState>>,
    /// A clip is clicked.
    pub on_select: Rc<Callback<usize>>,
    /// (src, dst, final): a clip is dragged to a new position.
    pub on_move: Rc<Callback<(usize, usize, bool)>>,
    /// (lane, index): a sound item is pressed.
    pub on_sound_select: Rc<Callback<(usize, usize)>>,
    /// (at, lane, final): the selected sound item is dragged.
    pub on_sound_move: Rc<Callback<(f64, Option<usize>, bool)>>,
    /// (montage time, final): the play line is dragged.
    pub on_scrub: Rc<Callback<(f64, bool)>>,
}

impl Timeline {
    pub fn new(lanes: usize) -> Timeline {
        let area = gtk::DrawingArea::builder().hexpand(true).content_height(TimelineGeom::height(lanes) as i32).build();
        let st = Rc::new(RefCell::new(TimelineState {
            geom: TimelineGeom { lanes, ..TimelineGeom::default() },
            selected: None,
            sel_sound: None,
            muted: Vec::new(),
            pos: 0.0,
            drag: None,
            moved: false,
            origin: (0.0, 0.0),
            last: (0.0, 0.0),
        }));
        let tl = Timeline {
            area,
            st,
            on_select: Rc::new(RefCell::new(None)),
            on_move: Rc::new(RefCell::new(None)),
            on_sound_select: Rc::new(RefCell::new(None)),
            on_sound_move: Rc::new(RefCell::new(None)),
            on_scrub: Rc::new(RefCell::new(None)),
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
            let name = {
                let mut st = t.st.borrow_mut();
                if st.drag.is_some() {
                    return;
                }
                st.geom.width = t.area.width() as f64;
                st.geom.cursor(st.pos, x, y)
            };
            t.area.set_cursor_from_name(name);
        });
        tl.area.add_controller(motion);
        tl
    }

    pub fn update(&self, clips: Vec<(String, f64)>, selected: Option<usize>, sounds: Vec<Vec<SoundBox>>, sel_sound: Option<(usize, usize)>, pos: f64, muted: Vec<bool>) {
        {
            let mut st = self.st.borrow_mut();
            st.geom.clips = clips;
            st.geom.sounds = sounds;
            st.selected = selected;
            st.sel_sound = sel_sound;
            st.muted = muted;
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

    /// Montage time of the play line.
    pub fn pos(&self) -> f64 {
        self.st.borrow().pos
    }

    /// For tests: which sound is drawn selected, and which lanes are drawn muted.
    pub fn shown(&self) -> (Option<(usize, usize)>, Vec<bool>) {
        let st = self.st.borrow();
        (st.sel_sound, st.muted.clone())
    }

    /// For tests: draw the timeline as it is now. Works while the window is hidden.
    pub fn paint(&self, cr: &cairo::Context) {
        let mut st = self.st.borrow_mut();
        st.geom.width = self.area.width() as f64;
        draw_timeline(&self.area, cr, &st, self.area.height() as f64);
    }

    pub fn press(&self, x: f64, y: f64) {
        let sound = {
            let mut st = self.st.borrow_mut();
            st.geom.width = self.area.width() as f64;
            st.origin = (x, y);
            st.last = (x, y);
            st.moved = false;
            if st.geom.line_hit(st.pos, x) {
                // keep the line where it was grabbed, not under the pointer
                st.drag = Some(TimelineDrag::Play(st.geom.line_x(st.pos) - x));
                return;
            }
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
        if self.st.borrow().drag.is_some() {
            self.area.set_cursor_from_name(Some("grabbing")); // a clip or a sound is picked up
        }
        if let Some(hit) = sound {
            call(&self.on_sound_select, hit);
        }
    }

    pub fn drag(&self, dx: f64, dy: f64) {
        enum Act {
            Sound(f64, Option<usize>),
            Clip(usize, usize),
            Scrub(f64),
        }
        let act = {
            let mut st = self.st.borrow_mut();
            let Some(drag) = st.drag else { return };
            let (x, y) = (st.origin.0 + dx, st.origin.1 + dy);
            st.last = (x, y);
            let still = match drag {
                TimelineDrag::Play(_) => dx.abs() < 2.0,
                _ => dx.abs() < 6.0 && dy.abs() < 6.0,
            };
            if still && !st.moved {
                return;
            }
            match drag {
                TimelineDrag::Play(offset) => {
                    st.moved = true;
                    Some(Act::Scrub(st.geom.time_at(x + offset)))
                }
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
            Some(Act::Scrub(t)) => call(&self.on_scrub, (t, false)),
            None => {}
        }
    }

    pub fn release(&self) {
        let (drag, moved, last, click, sound) = {
            let mut st = self.st.borrow_mut();
            let Some(drag) = st.drag.take() else { return };
            let (x, y) = st.origin;
            (drag, st.moved, st.last, st.geom.clip_hit(x, y), st.geom.sound_hit(x, y))
        };
        match (drag, moved, sound) {
            (TimelineDrag::Play(offset), true, _) => {
                let t = self.st.borrow().geom.time_at(last.0 + offset);
                call(&self.on_scrub, (t, true));
            }
            (TimelineDrag::Sound(_), true, _) => call(&self.on_sound_move, (0.0, None, true)),
            (TimelineDrag::Clip(i), true, _) => call(&self.on_move, (i, i, true)),
            // pressing the play line without dragging is a click on whatever is under it
            (TimelineDrag::Play(_), false, Some(hit)) => call(&self.on_sound_select, hit),
            (TimelineDrag::Clip(_) | TimelineDrag::Play(_), false, _) => {
                if let Some(hit) = click {
                    call(&self.on_select, hit.0);
                }
            }
            _ => {}
        }
        let name = {
            let st = self.st.borrow();
            st.geom.cursor(st.pos, last.0, last.1)
        };
        self.area.set_cursor_from_name(name);
    }
}

fn draw_timeline(area: &gtk::DrawingArea, cr: &cairo::Context, st: &TimelineState, h: f64) {
    let g = &st.geom;
    let w = g.width;
    let clip_mode = st.sel_sound.is_none();
    // Every item says when it starts in the finished video, then its name. Where an item is
    // narrow the time is what stays.
    let named = |start: f64, name: &str, c: Rgba, x: f64, y_mid: f64, clip: (f64, f64, f64, f64)| {
        let when = short_clock(start);
        text(area, cr, &when, (c.0, c.1, c.2, c.3 * 0.7), x, y_mid, Some(clip));
        text(area, cr, name, c, x + text_width(area, &when) + 6.0, y_mid, Some(clip));
    };
    for (i, (((x, cw), (label, _)), start)) in g.spans().iter().zip(&g.clips).zip(g.starts()).enumerate() {
        let sel = st.selected == Some(i);
        let b = (x + 1.0, TimelineGeom::CLIP_Y, (cw - 2.0).max(1.0), TimelineGeom::CLIP_H);
        // the selected clip is full blue, the others faded; a little less faded is the clip
        // that is waiting while one of the sounds is worked on
        let strength = match (sel, clip_mode) {
            (true, true) => 0.85,
            (true, false) => 0.5,
            (false, _) => 0.35,
        };
        fill(cr, with(ACCENT, strength), b.0, b.1, b.2, b.3);
        named(start, label, if sel { (1.0, 1.0, 1.0, 1.0) } else { fg(area, 1.0) }, x + 8.0, b.1 + b.3 / 2.0, b);
    }
    let any_sound = g.sounds.iter().any(|l| !l.is_empty());
    for lane in 0..g.lanes {
        let y = TimelineGeom::lane_y(lane);
        fill(cr, fg(area, 0.06), PAD, y, w - 2.0 * PAD, TimelineGeom::LANE_H);
        let items: &[SoundBox] = g.sounds.get(lane).map_or(&[], |v| v.as_slice());
        if lane == 0 && !any_sound {
            text(area, cr, "sound: press + at the end of a lane, or drop a sound file", fg(area, 0.4), PAD + 8.0, y + TimelineGeom::LANE_H / 2.0, None);
        }
        let quiet = st.muted.get(lane).copied().unwrap_or(false);
        for (i, s) in items.iter().enumerate() {
            let sel = st.sel_sound == Some((lane, i));
            let x0 = g.x(s.at).max(PAD);
            let x1 = g.x(s.at + s.seconds).max(g.x(s.at) + 4.0).min(w - PAD);
            if x1 <= x0 {
                continue;
            }
            let b = (x0, y, x1 - x0, TimelineGeom::LANE_H);
            let color = with(SOUND, (if sel { 0.95 } else { 0.5 }) * if quiet { 0.35 } else { 1.0 });
            fill(cr, color, b.0, b.1, b.2, b.3);
            if s.linked {
                // joined to the clip above it
                let top = TimelineGeom::CLIP_Y + TimelineGeom::CLIP_H;
                fill(cr, with(LINK, color.3), b.0 + 1.0, top, b.2 - 2.0, y - top);
            }
            if sel {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
                cr.set_line_width(1.0);
                cr.rectangle(b.0 + 0.5, b.1 + 0.5, b.2 - 1.0, b.3 - 1.0);
                let _ = cr.stroke();
            }
            named(s.at, &s.label, (1.0, 1.0, 1.0, if quiet { 0.45 } else { 1.0 }), b.0 + 6.0, y + TimelineGeom::LANE_H / 2.0, b);
        }
    }
    if g.total() > 0.0 {
        fill(cr, with(PLAYHEAD, 1.0), g.line_x(st.pos) - 1.0, 0.0, 2.0, h);
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
