//! Geometry and hit-testing for the custom widgets, kept free of GTK so it can be unit-tested.

use pve_core::model::Rect;

/// Left/right margin of the bars.
pub const PAD: f64 = 14.0;
/// How close (px) to a line counts as grabbing it.
pub const GRAB: f64 = 6.0;
/// Knob size.
pub const KNOB: f64 = 12.0;

// ---------------------------------------------------------------- trim bar

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimHit {
    Play,
    In,
    Out,
}

/// The trim bar: the source of the thing being trimmed, in three bands.
///
///   top     the play position: a knob to drag, and clicking here moves it
///   middle  the source, with the kept range, keyframe ticks, and the three lines
///   bottom  knobs for the in and out points
///
/// Clicking the middle band away from a line does nothing, so a missed grab never moves anything.
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
    pub const TOP: f64 = 18.0;
    pub const BOTTOM: f64 = 18.0;
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
        if d_play <= GRAB && (trim.is_none() || y < self.height / 2.0) {
            return Some(TrimHit::Play); // where lines coincide, the upper half is the play line
        }
        trim
    }

    /// Mouse cursor to show at (x, y).
    pub fn cursor(&self, x: f64, y: f64) -> Option<&'static str> {
        match self.hit(x, y)? {
            TrimHit::In | TrimHit::Out => Some("ew-resize"),
            TrimHit::Play if y >= Self::TOP || (x - self.x(self.pos)).abs() <= KNOB => Some("grab"),
            TrimHit::Play => Some("pointer"),
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
}

/// The montage: clips in order (widths in proportion to their length) and the sound lanes under them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TimelineGeom {
    pub width: f64,
    /// (label, seconds)
    pub clips: Vec<(String, f64)>,
    pub sounds: Vec<Vec<SoundBox>>,
    pub lanes: usize,
}

impl TimelineGeom {
    pub const CLIP_Y: f64 = 4.0;
    pub const CLIP_H: f64 = 30.0;
    pub const LANE_Y: f64 = 38.0;
    pub const LANE_H: f64 = 18.0;
    pub const LANE_GAP: f64 = 3.0;

    pub fn height(lanes: usize) -> f64 {
        Self::LANE_Y + lanes as f64 * (Self::LANE_H + Self::LANE_GAP) + 2.0
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
        TrimGeom { width: 1028.0, height: 66.0, duration: 100.0, start: 20.0, end: 60.0, pos: 40.0, view: (0.0, 100.0) }
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
        let (mid, low) = (33.0, 58.0);
        // top band: always the play position
        assert_eq!(g.hit(g.x(90.0), 5.0), Some(TrimHit::Play));
        // middle band: only near a line
        assert_eq!(g.hit(g.x(80.0), mid), None);
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
        assert_eq!(g.hit(g.x(20.0), 22.0), Some(TrimHit::Play));
        assert_eq!(g.hit(g.x(20.0), 44.0), Some(TrimHit::In));
    }

    #[test]
    fn trim_bar_cursors() {
        let g = trim();
        assert_eq!(g.cursor(g.x(20.0) - 5.0, 58.0), Some("ew-resize"));
        assert_eq!(g.cursor(g.x(40.0), 6.0), Some("grab"));
        assert_eq!(g.cursor(g.x(80.0), 6.0), Some("pointer"));
        assert_eq!(g.cursor(g.x(80.0), 33.0), None);
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
            sounds: vec![vec![SoundBox { label: "m".into(), at: 5.0, seconds: 10.0 }], vec![]],
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
        assert_eq!((g.lane_at(40.0), g.lane_at(62.0), g.lane_at(90.0), g.lane_at(10.0)), (Some(0), Some(1), None, None));
        assert_eq!(g.sound_hit(g.x(6.0), 45.0), Some((0, 0)));
        assert_eq!(g.sound_hit(g.x(16.0), 45.0), None);
        assert_eq!(g.sound_hit(g.x(6.0), 65.0), None);
        assert!((g.t(g.x(12.5)) - 12.5).abs() < 1e-9);
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
