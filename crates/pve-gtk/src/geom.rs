//! Geometry and hit-testing for the custom widgets, kept free of GTK so it can be unit-tested.

use pve_core::model::Rect;

/// Left/right margin of the bars.
pub const PAD: f64 = 14.0;
/// How close (px) to a line counts as grabbing it.
pub const GRAB: f64 = 6.0;
/// Knob size.
pub const KNOB: f64 = 12.0;
/// Space between a knob and the time written beside it.
pub const LABEL_GAP: f64 = 5.0;

// ---------------------------------------------------------------- trim bar

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimHit {
    Play,
    In,
    Out,
}

/// The three times written on the trim bar. Each can be clicked and typed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimLabel {
    Pos,
    In,
    Out,
}

/// The trim bar: the source of the thing being trimmed, in three bands.
///
///   top     the play position: a down-pointing triangle to drag, with its time beside it
///   middle  the source, with the kept range, keyframe ticks, and the three lines
///   bottom  triangles for the in and out points, each pointing at its line from outside the kept range,
///           with their times beside them
///
/// A press in the top or middle band away from the cut lines puts the play position there.
/// The cut points move only by their own lines and knobs, so a missed grab never trims anything.
#[derive(Debug, Clone, PartialEq)]
pub struct TrimGeom {
    pub width: f64,
    pub height: f64,
    pub duration: f64,
    pub start: f64,
    pub end: f64,
    pub pos: f64,
    /// Visible part of the source (zoom).
    pub view: (f64, f64),
}

impl TrimGeom {
    /// Heights of the top and bottom bands.
    pub const TOP: f64 = 20.0;
    pub const BOTTOM: f64 = 20.0;
    pub const HEIGHT: f64 = 70.0;
    /// Most zoomed-in view, seconds.
    pub const MIN_SPAN: f64 = 2.0;

    pub fn x(&self, t: f64) -> f64 {
        let (v0, v1) = self.view;
        if v1 > v0 {
            PAD + (t - v0) / (v1 - v0) * (self.width - 2.0 * PAD)
        } else {
            PAD
        }
    }

    pub fn t(&self, x: f64) -> f64 {
        let (v0, v1) = self.view;
        let w = self.width - 2.0 * PAD;
        if w > 0.0 {
            v0 + ((x - PAD) / w).clamp(0.0, 1.0) * (v1 - v0)
        } else {
            v0
        }
    }

    /// What a press at (x, y) grabs.
    pub fn hit(&self, x: f64, y: f64) -> Option<TrimHit> {
        if self.duration <= 0.0 {
            return None;
        }
        let (xp, xi, xo) = (self.x(self.pos), self.x(self.start), self.x(self.end));
        if y < Self::TOP {
            return Some(TrimHit::Play); // anywhere in the top band moves the play position
        }
        if y >= self.height - Self::BOTTOM {
            // knobs hang outside the kept range: in to the left of its line, out to the right
            if (xi - KNOB - 3.0..=xi + 3.0).contains(&x) {
                return Some(TrimHit::In);
            }
            if (xo - 3.0..=xo + KNOB + 3.0).contains(&x) {
                return Some(TrimHit::Out);
            }
            return None;
        }
        let (d_in, d_out, d_play) = ((x - xi).abs(), (x - xo).abs(), (x - xp).abs());
        let trim = (d_in.min(d_out) <= GRAB).then_some(if d_in < d_out || (d_in == d_out && x < xi) { TrimHit::In } else { TrimHit::Out });
        if d_play <= GRAB && y < self.height / 2.0 {
            return Some(TrimHit::Play); // where lines coincide, the upper half is the play line
        }
        trim.or(Some(TrimHit::Play)) // anywhere else on the bar: the play position goes there
    }

    /// Mouse cursor to show at (x, y): the double arrow on anything that slides along the bar.
    pub fn cursor(&self, x: f64, y: f64) -> Option<&'static str> {
        let reach = if y < Self::TOP { KNOB } else { GRAB };
        match self.hit(x, y)? {
            // away from the play line itself, a press brings it here
            TrimHit::Play if (x - self.x(self.pos)).abs() > reach => Some("pointer"),
            _ => Some("ew-resize"),
        }
    }

    fn in_view(&self, t: f64) -> bool {
        (self.view.0..=self.view.1).contains(&t)
    }

    /// The play position as drawn. A player can report a time a little past the end of the
    /// file; the line stays on the bar.
    pub fn line_pos(&self) -> f64 {
        self.pos.clamp(0.0, self.duration.max(0.0))
    }

    /// Left edge of the play position's time, `w` wide: right of its knob, or left of it
    /// where the bar ends. None when the play position is outside the view.
    pub fn pos_label_x(&self, w: f64) -> Option<f64> {
        let pos = self.line_pos();
        if !self.in_view(pos) {
            return None;
        }
        let x = self.x(pos);
        let right = x + KNOB / 2.0 + LABEL_GAP;
        Some(if right + w <= self.width { right } else { x - KNOB / 2.0 - LABEL_GAP - w })
    }

    /// Left edges of the in and out times, `w_in` and `w_out` wide.
    ///
    /// Each sits on the outer side of its knob. Where the bar ends before there is room, it
    /// moves to the inner side of its line; and where the kept range is too narrow for that,
    /// the two sit together beyond the other knob, in then out. A cut point outside a
    /// zoomed view has its time at the edge it lies beyond.
    pub fn cut_label_x(&self, w_in: f64, w_out: f64) -> (Option<f64>, Option<f64>) {
        const APART: f64 = 10.0;
        let (xs, xe) = (self.x(self.start), self.x(self.end));
        let left = xs - KNOB - LABEL_GAP; // an in time on the outer side ends here
        let right = xe + KNOB + LABEL_GAP; // an out time on the outer side starts here
        let between = xe - xs - 2.0 * LABEL_GAP;
        let in_fits = left - w_in >= 0.0;
        let out_fits = right + w_out <= self.width;
        match (self.in_view(self.start), self.in_view(self.end)) {
            (true, true) => {
                let (i, o) = match (in_fits, out_fits) {
                    (true, true) => (left - w_in, right),
                    (false, false) => (xs + LABEL_GAP, xe - LABEL_GAP - w_out),
                    (false, true) if between >= w_in => (xs + LABEL_GAP, right),
                    (false, true) => (right, right + w_in + APART),
                    (true, false) if between >= w_out => (left - w_in, xe - LABEL_GAP - w_out),
                    (true, false) => (left - w_out - APART - w_in, left - w_out),
                };
                (Some(i), Some(o))
            }
            (true, false) => {
                let i = if in_fits { left - w_in } else { xs + LABEL_GAP };
                let o = self.width - w_out;
                (Some(i), (self.end > self.view.1 && i + w_in + APART <= o).then_some(o))
            }
            (false, true) => {
                let o = if out_fits { right } else { xe - LABEL_GAP - w_out };
                ((self.start < self.view.0 && w_in + APART <= o).then_some(0.0), Some(o))
            }
            (false, false) => ((self.start < self.view.0).then_some(0.0), (self.end > self.view.1).then_some(self.width - w_out)),
        }
    }

    /// Zoom by `steps` wheel notches (negative = in) around the time under x.
    pub fn zoom(&mut self, x: f64, steps: f64) {
        let (v0, v1) = self.view;
        let span = v1 - v0;
        if self.duration <= 0.0 || span <= 0.0 {
            return;
        }
        let at = self.t(x);
        let new = (span * 1.25f64.powf(steps)).clamp(Self::MIN_SPAN.min(self.duration), self.duration);
        let v0 = (at - (at - v0) * new / span).clamp(0.0, self.duration - new);
        self.view = (v0, v0 + new);
    }

    pub fn pan(&mut self, steps: f64) {
        let (v0, v1) = self.view;
        let span = v1 - v0;
        let v0 = (v0 + steps * span * 0.15).clamp(0.0, (self.duration - span).max(0.0));
        self.view = (v0, v0 + span);
    }

    /// During playback: scroll a zoomed view so the play position stays visible.
    pub fn follow(&mut self) {
        let (v0, v1) = self.view;
        let span = v1 - v0;
        if span < self.duration && !(v0..=v1).contains(&self.pos) {
            let v0 = (self.pos - span * 0.1).clamp(0.0, self.duration - span);
            self.view = (v0, v0 + span);
        }
    }
}

// ---------------------------------------------------------------- montage timeline

#[derive(Debug, Clone, PartialEq)]
pub struct SoundBox {
    pub label: String,
    pub at: f64,
    pub seconds: f64,
    /// A clip's own sound: drawn joined to its clip.
    pub linked: bool,
}

/// The montage: clips in order (widths in proportion to their length) and the sound lanes under them.
///
/// Sound items are addressed by (lane, index into what is shown in that lane).
/// The play line can be grabbed anywhere along its height.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TimelineGeom {
    pub width: f64,
    /// (label, seconds)
    pub clips: Vec<(String, f64)>,
    pub sounds: Vec<Vec<SoundBox>>,
    pub lanes: usize,
}

impl TimelineGeom {
    // The rows are as tall as the buttons that stand at their ends, with the same gaps (see app.rs).
    pub const CLIP_Y: f64 = 0.0;
    pub const CLIP_H: f64 = 30.0;
    pub const LANE_H: f64 = 26.0;
    pub const LANE_GAP: f64 = 4.0;
    pub const LANE_Y: f64 = Self::CLIP_Y + Self::CLIP_H + Self::LANE_GAP;

    pub fn height(lanes: usize) -> f64 {
        Self::LANE_Y + lanes as f64 * (Self::LANE_H + Self::LANE_GAP) - Self::LANE_GAP
    }

    pub fn total(&self) -> f64 {
        self.clips.iter().map(|c| c.1).sum()
    }

    pub fn x(&self, t: f64) -> f64 {
        let total = self.total();
        if total > 0.0 {
            PAD + t / total * (self.width - 2.0 * PAD)
        } else {
            PAD
        }
    }

    pub fn t(&self, x: f64) -> f64 {
        let w = self.width - 2.0 * PAD;
        if w > 0.0 {
            (x - PAD) / w * self.total()
        } else {
            0.0
        }
    }

    /// Montage time at x, kept inside the montage.
    pub fn time_at(&self, x: f64) -> f64 {
        self.t(x).clamp(0.0, self.total())
    }

    /// Where the play line is drawn for montage time `pos`.
    pub fn line_x(&self, pos: f64) -> f64 {
        self.x(pos).min(self.width - PAD)
    }

    pub fn line_hit(&self, pos: f64, x: f64) -> bool {
        self.total() > 0.0 && (x - self.line_x(pos)).abs() <= GRAB
    }

    /// Mouse cursor to show at (x, y) with the play line at `pos`: the double arrow on the
    /// line, which slides; the hand on clips and sounds, which are picked up and carried.
    pub fn cursor(&self, pos: f64, x: f64, y: f64) -> Option<&'static str> {
        if self.line_hit(pos, x) {
            Some("ew-resize")
        } else {
            (self.sound_hit(x, y).is_some() || self.clip_hit(x, y).is_some()).then_some("grab")
        }
    }

    /// Montage time at which each clip starts.
    pub fn starts(&self) -> Vec<f64> {
        let mut t = 0.0;
        self.clips.iter().map(|(_, d)| {
            let at = t;
            t += d;
            at
        }).collect()
    }

    /// (x, width) per clip.
    pub fn spans(&self) -> Vec<(f64, f64)> {
        if self.total() <= 0.0 {
            return Vec::new();
        }
        let mut t = 0.0;
        self.clips
            .iter()
            .map(|(_, d)| {
                let span = (self.x(t), self.x(t + d) - self.x(t));
                t += d;
                span
            })
            .collect()
    }

    /// Which clip is at (x, y), and how far along it (0..1).
    pub fn clip_hit(&self, x: f64, y: f64) -> Option<(usize, f64)> {
        if !(Self::CLIP_Y..Self::CLIP_Y + Self::CLIP_H).contains(&y) {
            return None;
        }
        self.spans().iter().enumerate().find(|(_, (cx, cw))| (*cx..cx + cw).contains(&x)).map(|(i, (cx, cw))| (i, (x - cx) / cw))
    }

    pub fn lane_at(&self, y: f64) -> Option<usize> {
        if y < Self::LANE_Y {
            return None;
        }
        let n = ((y - Self::LANE_Y) / (Self::LANE_H + Self::LANE_GAP)).floor() as usize;
        (n < self.lanes).then_some(n)
    }

    pub fn lane_y(lane: usize) -> f64 {
        Self::LANE_Y + lane as f64 * (Self::LANE_H + Self::LANE_GAP)
    }

    /// Which sound item is at (x, y): (lane, index). The last one added wins where they overlap.
    pub fn sound_hit(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        let lane = self.lane_at(y)?;
        if self.total() <= 0.0 {
            return None;
        }
        let items = self.sounds.get(lane)?;
        (0..items.len()).rev().find(|&i| {
            let s = &items[i];
            let x0 = self.x(s.at);
            (x0..self.x(s.at + s.seconds).max(x0 + 4.0)).contains(&x)
        }).map(|i| (lane, i))
    }
}

// ---------------------------------------------------------------- crop overlay

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropHit {
    Move,
    /// A corner: (west?, north?)
    Corner { west: bool, north: bool },
}

/// The crop rectangle over the preview. Rectangles are in display pixels of the video.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropGeom {
    /// Widget size.
    pub width: f64,
    pub height: f64,
    /// Display size of the video.
    pub video: (f64, f64),
}

impl CropGeom {
    /// Where mpv draws the video inside the widget: (x, y, scale).
    pub fn area(&self) -> (f64, f64, f64) {
        let (vw, vh) = self.video;
        if vw <= 0.0 || vh <= 0.0 || self.width <= 0.0 || self.height <= 0.0 {
            return (0.0, 0.0, 0.0);
        }
        let s = (self.width / vw).min(self.height / vh);
        ((self.width - vw * s) / 2.0, (self.height - vh * s) / 2.0, s)
    }

    /// Widget point -> video point.
    pub fn video_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (ox, oy, s) = self.area();
        (s > 0.0).then(|| ((x - ox) / s, (y - oy) / s))
    }

    pub fn hit(&self, rect: &Rect, x: f64, y: f64) -> Option<CropHit> {
        let (_, _, s) = self.area();
        let (vx, vy) = self.video_point(x, y)?;
        let grab = 14.0 / s;
        let (l, t, r, b) = (rect.x as f64, rect.y as f64, (rect.x + rect.w) as f64, (rect.y + rect.h) as f64);
        for (west, north, cx, cy) in [(true, true, l, t), (false, true, r, t), (true, false, l, b), (false, false, r, b)] {
            if (vx - cx).abs() <= grab && (vy - cy).abs() <= grab {
                return Some(CropHit::Corner { west, north });
            }
        }
        ((l..=r).contains(&vx) && (t..=b).contains(&vy)).then_some(CropHit::Move)
    }

    pub fn cursor(hit: CropHit) -> &'static str {
        match hit {
            CropHit::Move => "move",
            CropHit::Corner { west, north } if west == north => "nwse-resize",
            CropHit::Corner { .. } => "nesw-resize",
        }
    }

    /// The rectangle after dragging by (dx, dy) widget pixels from a press at video point `start`.
    /// `aspect` (w/h) is held if given.
    pub fn drag(&self, hit: CropHit, orig: &Rect, start: (f64, f64), dx: f64, dy: f64, aspect: Option<f64>) -> Option<Rect> {
        let (_, _, s) = self.area();
        if s <= 0.0 {
            return None;
        }
        let (vw, vh) = self.video;
        let (dx, dy) = (dx / s, dy / s);
        let (ox, oy, ow, oh) = (orig.x as f64, orig.y as f64, orig.w as f64, orig.h as f64);
        match hit {
            CropHit::Move => {
                let x = (ox + dx).clamp(0.0, (vw - ow).max(0.0));
                let y = (oy + dy).clamp(0.0, (vh - oh).max(0.0));
                Some(Rect { x: x as i64, y: y as i64, w: orig.w, h: orig.h })
            }
            CropHit::Corner { west, north } => {
                // The corner opposite the one being dragged stays put.
                let ax = if west { ox + ow } else { ox };
                let ay = if north { oy + oh } else { oy };
                let px = (start.0 + dx).clamp(0.0, vw);
                let py = (start.1 + dy).clamp(0.0, vh);
                let (mut w, mut h) = ((px - ax).abs(), (py - ay).abs());
                if let Some(a) = aspect {
                    let room_w = if west { ax } else { vw - ax };
                    let room_h = if north { ay } else { vh - ay };
                    w = w.max(h * a).min(room_w).min(room_h * a);
                    h = w / a;
                }
                let (w, h) = (w.max(16.0), h.max(16.0));
                let x = if west { ax - w } else { ax };
                let y = if north { ay - h } else { ay };
                Some(Rect { x: x.round() as i64, y: y.round() as i64, w: w.round() as i64, h: h.round() as i64 })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trim() -> TrimGeom {
        TrimGeom { width: 1028.0, height: TrimGeom::HEIGHT, duration: 100.0, start: 20.0, end: 60.0, pos: 40.0, view: (0.0, 100.0) }
    }

    #[test]
    fn trim_bar_maps_time_and_pixels() {
        let g = trim();
        assert_eq!((g.x(0.0), g.x(100.0), g.x(50.0)), (14.0, 1014.0, 514.0));
        assert_eq!((g.t(514.0), g.t(-50.0), g.t(5000.0)), (50.0, 0.0, 100.0));
    }

    #[test]
    fn trim_bar_bands() {
        let g = trim();
        let (mid, low) = (35.0, 62.0);
        // top band: always the play position
        assert_eq!(g.hit(g.x(90.0), 5.0), Some(TrimHit::Play));
        // middle band: a cut line when near one, otherwise the play position
        assert_eq!(g.hit(g.x(80.0), mid), Some(TrimHit::Play));
        assert_eq!(g.hit(g.x(20.0) + 3.0, mid), Some(TrimHit::In));
        assert_eq!(g.hit(g.x(60.0) - 3.0, mid), Some(TrimHit::Out));
        assert_eq!(g.hit(g.x(40.0) + 2.0, mid), Some(TrimHit::Play));
        // bottom band: the knobs hang outside the kept range
        assert_eq!(g.hit(g.x(20.0) - 6.0, low), Some(TrimHit::In));
        assert_eq!(g.hit(g.x(60.0) + 6.0, low), Some(TrimHit::Out));
        assert_eq!(g.hit(g.x(20.0) + 8.0, low), None);
        assert_eq!(g.hit(g.x(40.0), low), None);
    }

    #[test]
    fn where_lines_coincide_the_upper_half_is_the_play_line() {
        let mut g = trim();
        g.pos = 20.0;
        assert_eq!(g.hit(g.x(20.0), 24.0), Some(TrimHit::Play));
        assert_eq!(g.hit(g.x(20.0), 46.0), Some(TrimHit::In));
    }

    #[test]
    fn trim_bar_cursors() {
        let g = trim();
        // the double arrow on the cut knobs, and on the play knob and its line
        assert_eq!(g.cursor(g.x(20.0) - 5.0, 62.0), Some("ew-resize"));
        assert_eq!(g.cursor(g.x(40.0), 6.0), Some("ew-resize"));
        assert_eq!(g.cursor(g.x(40.0), 35.0), Some("ew-resize"));
        assert_eq!(g.cursor(g.x(80.0), 6.0), Some("pointer"));
        assert_eq!(g.cursor(g.x(80.0), 35.0), Some("pointer"));
        assert_eq!(g.cursor(g.x(80.0), 62.0), None);
    }

    #[test]
    fn the_play_line_stays_on_the_bar_past_the_end() {
        let mut g = trim();
        g.pos = 100.04; // a player can stop a little past the length of the file
        assert_eq!(g.line_pos(), 100.0);
        assert_eq!(g.pos_label_x(70.0), Some(g.x(100.0) - 6.0 - LABEL_GAP - 70.0)); // no room on the right: left of the knob
        g.pos = 40.0;
        assert_eq!(g.pos_label_x(70.0), Some(g.x(40.0) + 6.0 + LABEL_GAP));
        g.view = (50.0, 100.0);
        assert_eq!(g.pos_label_x(70.0), None);
    }

    #[test]
    fn cut_times_sit_beside_their_knobs() {
        let w = 70.0;
        let at = |start: f64, end: f64| {
            let g = TrimGeom { start, end, ..trim() };
            let (i, o) = g.cut_label_x(w, w);
            (i.unwrap() - g.x(start), o.unwrap() - g.x(end))
        };
        let (outer_in, outer_out) = (-(KNOB + LABEL_GAP + w), KNOB + LABEL_GAP);
        // room on both sides: each on the outer side of its knob
        assert_eq!(at(20.0, 60.0), (outer_in, outer_out));
        // nothing trimmed: no room outside, so both inside the kept range
        assert_eq!(at(0.0, 100.0), (LABEL_GAP, -LABEL_GAP - w));
        // a wide cut from the very start: the in time moves inside
        assert_eq!(at(0.0, 40.0), (LABEL_GAP, outer_out));
        // a short cut at the very start: both right of the knobs, in then out
        let g = TrimGeom { start: 0.0, end: 3.0, ..trim() };
        let (i, o) = g.cut_label_x(w, w);
        assert_eq!((i, o), (Some(g.x(3.0) + KNOB + LABEL_GAP), Some(g.x(3.0) + KNOB + LABEL_GAP + w + 10.0)));
        // and at the very end: both left of the knobs, in then out
        let g = TrimGeom { start: 97.0, end: 100.0, ..trim() };
        let (i, o) = g.cut_label_x(w, w);
        assert_eq!(o, Some(g.x(97.0) - KNOB - LABEL_GAP - w));
        assert_eq!(i, Some(o.unwrap() - 10.0 - w));
        // zoomed in between the cut points: each time at the edge its knob lies beyond
        let g = TrimGeom { view: (30.0, 50.0), ..trim() };
        assert_eq!(g.cut_label_x(w, w), (Some(0.0), Some(g.width - w)));
    }

    #[test]
    fn zoom_keeps_the_time_under_the_pointer() {
        let mut g = trim();
        let x = g.x(40.0);
        g.zoom(x, -8.0);
        assert!(g.view.1 - g.view.0 < 25.0);
        assert!((g.t(x) - 40.0).abs() < 1e-9);
        g.zoom(x, -100.0);
        assert!((g.view.1 - g.view.0 - TrimGeom::MIN_SPAN).abs() < 1e-9);
        g.zoom(x, 100.0);
        assert_eq!(g.view, (0.0, 100.0));
        g.zoom(x, -8.0);
        g.pos = 99.0;
        g.follow();
        assert!((g.view.0..=g.view.1).contains(&99.0));
    }

    fn timeline() -> TimelineGeom {
        TimelineGeom {
            width: 1028.0,
            clips: vec![("a".into(), 10.0), ("b".into(), 30.0)],
            sounds: vec![vec![SoundBox { label: "m".into(), at: 5.0, seconds: 10.0, linked: false }], vec![]],
            lanes: 2,
        }
    }

    #[test]
    fn timeline_hit_testing() {
        let g = timeline();
        assert_eq!(g.spans(), vec![(14.0, 250.0), (264.0, 750.0)]);
        assert_eq!(g.clip_hit(139.0, 10.0), Some((0, 0.5)));
        assert_eq!(g.clip_hit(300.0, 10.0).map(|h| h.0), Some(1));
        assert_eq!(g.clip_hit(300.0, 45.0), None);
        assert_eq!(TimelineGeom::height(2), 90.0);
        assert_eq!((TimelineGeom::lane_y(0), TimelineGeom::lane_y(1)), (34.0, 64.0));
        assert_eq!((g.lane_at(40.0), g.lane_at(70.0), g.lane_at(95.0), g.lane_at(10.0)), (Some(0), Some(1), None, None));
        assert_eq!(g.sound_hit(g.x(6.0), 45.0), Some((0, 0)));
        assert_eq!(g.sound_hit(g.x(16.0), 45.0), None);
        assert_eq!(g.sound_hit(g.x(6.0), 70.0), None);
        assert!((g.t(g.x(12.5)) - 12.5).abs() < 1e-9);
        assert_eq!(g.starts(), vec![0.0, 10.0]);
    }

    #[test]
    fn timeline_play_line() {
        let g = timeline();
        let x = g.line_x(20.0);
        assert_eq!(x, g.x(20.0));
        assert!(g.line_hit(20.0, x - 6.0) && g.line_hit(20.0, x + 6.0) && !g.line_hit(20.0, x + 7.0));
        // the whole height of the line is grabbable, and it comes before a clip or sound item under it
        assert_eq!((g.cursor(20.0, x, 10.0), g.cursor(20.0, x, 70.0), g.cursor(20.0, x + 30.0, 70.0)), (Some("ew-resize"), Some("ew-resize"), None));
        // clips and sound items are picked up with the hand
        assert_eq!((g.cursor(20.0, g.x(6.0), 45.0), g.cursor(20.0, g.x(30.0), 10.0)), (Some("grab"), Some("grab")));
        // past the end of the montage the line stays on the bar, and times stay inside the montage
        assert_eq!(g.line_x(55.0), 1014.0);
        assert_eq!((g.time_at(-50.0), g.time_at(5000.0), g.time_at(g.x(12.5))), (0.0, 40.0, 12.5));
        assert!(!TimelineGeom::default().line_hit(0.0, PAD));
    }

    #[test]
    fn crop_drag_moves_and_resizes() {
        // a 1080x1920 video in a 1000x960 widget: scale 0.5, centred
        let g = CropGeom { width: 1000.0, height: 960.0, video: (1080.0, 1920.0) };
        assert_eq!(g.area(), (230.0, 0.0, 0.5));
        let r = Rect { x: 0, y: 555, w: 1080, h: 810 };
        let centre = (230.0 + 540.0 * 0.5, 960.0 * 0.5);
        assert_eq!(g.hit(&r, centre.0, centre.1), Some(CropHit::Move));
        assert_eq!(g.hit(&r, 230.0, 555.0 * 0.5), Some(CropHit::Corner { west: true, north: true }));
        assert_eq!(g.hit(&r, 100.0, 100.0), None);
        // move up by 40 widget px = 80 video px; cannot leave the frame sideways
        let start = g.video_point(centre.0, centre.1).unwrap();
        assert_eq!(g.drag(CropHit::Move, &r, start, 300.0, -40.0, None), Some(Rect { x: 0, y: 475, w: 1080, h: 810 }));
        // drag the bottom-right corner inwards, holding 4:3
        let se = CropHit::Corner { west: false, north: false };
        let start = (1080.0, 1365.0);
        let out = g.drag(se, &r, start, -60.0, -60.0, Some(4.0 / 3.0)).unwrap();
        assert_eq!((out.x, out.y), (0, 555));
        assert!(out.w < 1080 && (out.w as f64 / out.h as f64 - 4.0 / 3.0).abs() < 0.01);
        // free shape: width and height follow the pointer independently
        let free = g.drag(se, &r, start, -100.0, -20.0, None).unwrap();
        assert_eq!((free.w, free.h), (880, 770));
        assert_eq!(CropGeom::cursor(se), "nwse-resize");
    }
}
