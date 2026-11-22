//! The editor window: preview, trim bar, montage timeline with sound lanes, export line.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use pve_core::media::MediaInfo;
use pve_core::model::{Crop, Project, Rect};
use pve_core::planner::{ExportPlan, Infos, Kind};
use pve_core::session::{Session, Shape, SoundRef, Target, LANES, MIN_GAIN_DB};
use pve_core::time::{self, r, to_f64, R};
use pve_native::ffmpeg::{self, Capabilities, Encoder, ExportError};
use pve_native::probe::probe;

use crate::geom::{SoundBox, TimelineGeom, TrimHit, TrimLabel};
use crate::player::{LanePlayer, Player, PlayerEvent};
use crate::widgets::{CropOverlay, Timeline, TrimBar};

pub const PROJECT_EXT: &str = ".pve";
const APP_NAME: &str = "Plain Video Editor";

fn shapes() -> Vec<(&'static str, Shape)> {
    vec![
        ("Off", Shape::Off),
        ("4:3", Shape::Ratio(r(4, 3))),
        ("16:9", Shape::Ratio(r(16, 9))),
        ("1:1", Shape::Ratio(r(1, 1))),
        ("9:16", Shape::Ratio(r(9, 16))),
        ("Free", Shape::Free),
    ]
}

pub fn clock(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, frac) = (ms / 3_600_000, ms % 3_600_000 / 60_000, ms % 60_000 / 1000, ms % 1000);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{frac:03}")
    } else {
        format!("{m}:{s:02}.{frac:03}")
    }
}

/// A time to the tenth of a second, for the names on the montage: "0:07.4".
pub fn short_clock(t: f64) -> String {
    let tenths = (t.max(0.0) * 10.0).round() as u64;
    let (h, m, s, frac) = (tenths / 36_000, tenths % 36_000 / 600, tenths % 600 / 10, tenths % 10);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{frac}")
    } else {
        format!("{m}:{s:02}.{frac}")
    }
}

/// "83.5", "1:23.5" or "1:02:03.25" -> seconds, exactly. None if it is not a time.
pub fn parse_clock(text: &str) -> Option<R> {
    let cleaned = text.trim().replace(',', ".");
    let parts: Vec<&str> = cleaned.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = time::zero();
    for p in parts {
        if p.contains('/') {
            return None;
        }
        let n = time::parse(p)?;
        if n < time::zero() {
            return None;
        }
        total = total * time::int(60) + n;
    }
    Some(total)
}

/// A player time as an exact fraction, rounded down so it never passes the frame it names.
pub fn floor_us(t: f64) -> R {
    r((t * 1_000_000.0).floor() as i128, 1_000_000)
}

pub fn gain_to_percent(db: f64) -> f64 {
    if db <= MIN_GAIN_DB {
        0.0
    } else {
        100.0 * 10f64.powf(db / 20.0)
    }
}

pub fn percent_to_gain(pct: f64) -> f64 {
    if pct < 0.5 {
        MIN_GAIN_DB
    } else {
        20.0 * (pct / 100.0).log10()
    }
}

pub fn autosave_file() -> PathBuf {
    // $XDG_STATE_HOME, or ~/.local/state
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| glib::home_dir().join(".local/state"));
    state.join("plain-video-editor").join(format!("last-session{PROJECT_EXT}"))
}

fn absolute(path: &str) -> String {
    std::path::absolute(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_owned())
}

fn stem(path: &str) -> String {
    Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_owned())
}

/// What to make of a file that is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Add {
    /// A video becomes a clip; a file with sound only goes into a lane that is free.
    Auto,
    /// Its sound goes into this lane.
    Lane(usize),
}

/// Results coming back from worker threads.
enum Msg {
    Added { path: String, info: Box<MediaInfo>, add: Add },
    Indexed { path: String, info: Box<MediaInfo> },
    Opened { file: String, project: Box<Project>, infos: Infos },
    Note(String),
    Caps(Capabilities),
    Progress(f64),
    ExportDone(String),
}

/// Playing the finished video has been asked for and the picture is not moving yet.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Starting {
    No,
    /// The player is still loading the file or finding the place.
    Finding,
    /// The sounds in the lanes have been let go; the picture follows at this moment, when
    /// they are heard. The second number is how long that wait is in all.
    Sounds(Instant, f64),
}

pub struct St {
    pub s: Session,
    /// Play position: source time within the target (selected clip, or sound item).
    pub pos: f64,
    /// When the player last said where it was: while it plays it has moved on since.
    pos_at: Instant,
    last_pos: f64,
    /// Play position to go back to after working on a sound item.
    clip_pos: f64,
    /// Where the play line of the finished video is held, when it is not in what is selected:
    /// clicking a clip or a sound selects it and leaves that line alone. It is let go as soon
    /// as the play position is moved or played.
    held: Option<R>,
    pub plan: Option<ExportPlan>,
    /// A drag is in progress and has already been checkpointed.
    gesture: bool,
    /// The play position follows the pointer, not the player.
    dragging: bool,
    /// The preview is showing a cut point, not the play position.
    pub detached: bool,
    /// Which play is running, or ran last: what the trim bar shows by itself (true), or the finished video.
    pub source_play: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    /// When the volume was last changed (one undo step per adjustment).
    volume_edit: Option<Instant>,
    /// When a sound was last slid along its lane with the arrow keys (one undo step per run of presses).
    nudge_edit: Option<Instant>,
    caps: Capabilities,
    kf_cache: HashMap<String, (Option<usize>, Rc<Vec<f64>>)>,
    ticks: u64,
    /// Whether the preview's picture was moving at the last tick.
    was_moving: bool,
    starting: Starting,
}

pub struct App {
    pub window: gtk::ApplicationWindow,
    pub player: Player,
    pub lane_players: RefCell<Vec<LanePlayer>>,
    pub crop: CropOverlay,
    pub trim: TrimBar,
    pub timeline: Timeline,
    empty: gtk::Label,
    /// At the start of the trim bar: play what the bar shows, by itself.
    clip_play_btn: gtk::Button,
    /// At its end: the bar shows only the kept range.
    pub fit_btn: gtk::ToggleButton,
    /// At the start of the montage: play the finished video.
    play_btn: gtk::Button,
    /// Per lane, at its start: pressed = not heard in the preview.
    pub mute: Vec<gtk::ToggleButton>,
    /// Per lane, at its end.
    add_sound_btns: Vec<gtk::Button>,
    /// The text box that covers a time on the trim bar while it is typed over, and which time that is.
    pub time_entry: gtk::Entry,
    editing: Cell<Option<TrimLabel>>,
    /// Play position in the finished video, and its length.
    pub montage_lbl: gtk::Label,
    pub volume: gtk::Scale,
    pub volume_lbl: gtk::Label,
    pub shape_btns: Vec<(Shape, gtk::ToggleButton)>,
    shapes_box: gtk::Box,
    pub lock: gtk::CheckButton,
    pub status: gtk::Label,
    pub progress: gtk::ProgressBar,
    cancel_btn: gtk::Button,
    pub exact: gtk::CheckButton,
    export_btn: gtk::Button,
    pub st: RefCell<St>,
    syncing: Cell<bool>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    pub fn new(app: &gtk::Application, files: Vec<String>) -> Result<Rc<App>, String> {
        let window = gtk::ApplicationWindow::builder().application(app).title(APP_NAME).default_width(1280).default_height(820).build();
        let header = gtk::HeaderBar::new();
        window.set_titlebar(Some(&header));
        let menu = gio::Menu::new();
        menu.append(Some("Open project…"), Some("win.open-project"));
        menu.append(Some("Save project"), Some("win.save"));
        menu.append(Some("Save project as…"), Some("win.save-as"));
        menu.append(Some("Reopen last session"), Some("win.restore"));
        header.pack_end(&gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Project").build());
        let export_face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        export_face.append(&gtk::Image::from_icon_name("media-eject-symbolic"));
        export_face.append(&gtk::Label::new(Some("Export")));
        let export_btn = gtk::Button::builder().child(&export_face).tooltip_text("Export (Ctrl+E)").build();
        export_btn.add_css_class("suggested-action");
        header.pack_end(&export_btn);

        // The buttons at the ends of the rows are as tall as the rows, not as tall as buttons like to be.
        let css = gtk::CssProvider::new();
        #[allow(deprecated)] // load_from_string needs GTK 4.12
        css.load_from_data("button.pve-row { min-height: 0; min-width: 0; padding: 0 8px; } entry.pve-time { min-height: 0; padding: 0 4px; }");
        gtk::style_context_add_provider_for_display(&WidgetExt::display(&window), &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.set_child(Some(&root));

        let player = Player::new()?;
        let crop = CropOverlay::new();
        let empty = gtk::Label::new(Some("Drop video files here, or press + at the end of the timeline"));
        empty.add_css_class("title-2");
        let stage = gtk::Overlay::builder().vexpand(true).build();
        stage.set_child(Some(&player.area));
        stage.add_overlay(&crop.area);
        stage.add_overlay(&empty);
        root.append(&stage);

        let bar = gtk::Box::builder().spacing(6).margin_start(12).margin_end(12).margin_top(6).margin_bottom(2).build();
        root.append(&bar);
        bar.append(&gtk::Label::new(Some("Volume")));
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 200.0, 1.0);
        volume.set_size_request(260, -1);
        volume.set_draw_value(false);
        volume.add_mark(100.0, gtk::PositionType::Bottom, None);
        volume.set_tooltip_text(Some("Volume of the selected clip or sound"));
        bar.append(&volume);
        let volume_lbl = gtk::Label::builder().width_chars(5).xalign(0.0).build();
        volume_lbl.add_css_class("numeric");
        bar.append(&volume_lbl);
        bar.append(&gtk::Box::builder().hexpand(true).build());
        bar.append(&gtk::Label::new(Some("Crop")));
        let shapes_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        shapes_box.add_css_class("linked");
        let mut shape_btns: Vec<(Shape, gtk::ToggleButton)> = Vec::new();
        for (label, value) in shapes() {
            let b = gtk::ToggleButton::with_label(label);
            if let Some((_, first)) = shape_btns.first() {
                b.set_group(Some(first));
            }
            shapes_box.append(&b);
            shape_btns.push((value, b));
        }
        bar.append(&shapes_box);
        let lock = gtk::CheckButton::builder().label("All clips").active(true).tooltip_text("Use the same crop shape for every clip").build();
        bar.append(&lock);

        // Four rows, each with a button at its start and one at its end: the trim bar, the
        // montage, and the two sound lanes. The montage and the lanes are one drawing, three rows tall.
        let rows = gtk::Grid::builder().column_spacing(4).row_spacing(TimelineGeom::LANE_GAP as i32).margin_start(12).margin_end(12).margin_top(4).build();
        root.append(&rows);
        let row_button = |icon: &str, tip: &str, height: f64| {
            let b = gtk::Button::builder().icon_name(icon).tooltip_text(tip).height_request(height as i32).valign(gtk::Align::Center).build();
            b.add_css_class("pve-row");
            b
        };
        let row_toggle = |icon: &str, tip: &str, height: f64| {
            let b = gtk::ToggleButton::builder().icon_name(icon).tooltip_text(tip).height_request(height as i32).valign(gtk::Align::Center).build();
            b.add_css_class("pve-row");
            b
        };

        let trim = TrimBar::new();
        let time_entry = gtk::Entry::builder().visible(false).halign(gtk::Align::Start).width_chars(11).max_width_chars(11).build();
        time_entry.add_css_class("numeric");
        time_entry.add_css_class("pve-time");
        let trim_stack = gtk::Overlay::builder().hexpand(true).child(&trim.area).build();
        trim_stack.add_overlay(&time_entry);
        let clip_play_btn = row_button("media-playback-start-symbolic", "Play what this bar shows, by itself, past the in and out points (Shift+Space)", TimelineGeom::CLIP_H);
        let fit_btn = row_toggle("zoom-fit-best-symbolic", "Show only the kept part, so that it fills the bar. Press again for the whole file.", TimelineGeom::CLIP_H);
        rows.attach(&clip_play_btn, 0, 0, 1, 1);
        rows.attach(&trim_stack, 1, 0, 1, 1);
        rows.attach(&fit_btn, 2, 0, 1, 1);

        let timeline = Timeline::new(LANES);
        let play_btn = row_button("media-playback-start-symbolic", "Play the finished video (Space)", TimelineGeom::CLIP_H);
        let add = row_button("list-add-symbolic", "Add video files (Ctrl+O). You can also drop files on the window.", TimelineGeom::CLIP_H);
        rows.attach(&play_btn, 0, 1, 1, 1);
        rows.attach(&timeline.area, 1, 1, 1, 1 + LANES as i32);
        rows.attach(&add, 2, 1, 1, 1);
        let mut mute = Vec::new();
        let mut add_sound_btns = Vec::new();
        for n in 0..LANES {
            let m = row_toggle("audio-volume-high-symbolic", &format!("Mute sound lane {} in the preview. This does not change the export.", n + 1), TimelineGeom::LANE_H);
            let plus = row_button("list-add-symbolic", &format!("Add a file's sound to lane {}, starting at the play line", n + 1), TimelineGeom::LANE_H);
            rows.attach(&m, 0, 2 + n as i32, 1, 1);
            rows.attach(&plus, 2, 2 + n as i32, 1, 1);
            mute.push(m);
            add_sound_btns.push(plus);
        }

        let foot = gtk::Box::builder().spacing(10).margin_start(12).margin_end(12).margin_top(8).margin_bottom(8).build();
        root.append(&foot);
        let montage_lbl = gtk::Label::builder().tooltip_text("Play position in the finished video, and its length").build();
        montage_lbl.add_css_class("numeric");
        foot.append(&montage_lbl);
        let status = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(pango::EllipsizeMode::End).build();
        foot.append(&status);
        let progress = gtk::ProgressBar::builder().valign(gtk::Align::Center).visible(false).width_request(180).build();
        foot.append(&progress);
        let cancel_btn = gtk::Button::builder().label("Cancel").visible(false).build();
        foot.append(&cancel_btn);
        let exact = gtk::CheckButton::builder().label("Exact cuts").tooltip_text("Cut exactly where asked. This always re-encodes.").build();
        foot.append(&exact);

        let (tx, rx) = channel();
        let me = Rc::new(App {
            window,
            player,
            lane_players: RefCell::new((0..LANES).map(|_| LanePlayer::new()).collect()),
            crop,
            trim,
            timeline,
            empty,
            clip_play_btn,
            fit_btn,
            play_btn,
            mute,
            add_sound_btns,
            time_entry,
            editing: Cell::new(None),
            montage_lbl,
            volume,
            volume_lbl,
            shape_btns,
            shapes_box,
            lock,
            status,
            progress,
            cancel_btn,
            exact,
            export_btn,
            st: RefCell::new(St {
                s: Session::new(),
                pos: 0.0,
                pos_at: Instant::now(),
                last_pos: 0.0,
                clip_pos: 0.0,
                held: None,
                plan: None,
                gesture: false,
                dragging: false,
                detached: false,
                source_play: false,
                cancel: None,
                volume_edit: None,
                nudge_edit: None,
                caps: Capabilities::default(),
                kf_cache: HashMap::new(),
                ticks: 0,
                was_moving: false,
                starting: Starting::No,
            }),
            syncing: Cell::new(false),
            tx,
            rx,
        });
        me.connect(&add);
        me.refresh();

        // What this machine's ffmpeg can do, found out in the background.
        let tx = me.tx.clone();
        std::thread::spawn(move || {
            if let Ok(caps) = ffmpeg::detect() {
                let _ = tx.send(Msg::Caps(caps.clone()));
            }
        });
        if !files.is_empty() {
            me.open_paths(files);
        }
        Ok(me)
    }

    /// Wire every signal to a method. Closures hold a weak reference so the window can be freed.
    fn connect(self: &Rc<Self>, add: &gtk::Button) {
        macro_rules! on {
            ($me:ident => $body:expr) => {{
                let weak = Rc::downgrade(self);
                move || {
                    if let Some($me) = weak.upgrade() {
                        $body;
                    }
                }
            }};
        }
        let f = on!(a => a.choose_files("Add video files or open a project", Add::Auto));
        add.connect_clicked(move |_| f());
        for (n, b) in self.add_sound_btns.iter().enumerate() {
            let f = on!(a => a.choose_files(&format!("Add sound to lane {}", n + 1), Add::Lane(n)));
            b.connect_clicked(move |_| f());
        }
        let f = on!(a => a.export());
        self.export_btn.connect_clicked(move |_| f());
        let f = on!(a => a.play_source());
        self.clip_play_btn.connect_clicked(move |_| f());
        let f = on!(a => a.play_montage());
        self.play_btn.connect_clicked(move |_| f());
        let weak = Rc::downgrade(self);
        self.fit_btn.connect_toggled(move |b| {
            if let Some(a) = weak.upgrade() {
                a.stop_editing();
                a.trim.set_fit(b.is_active());
            }
        });
        let f = on!(a => a.cancel_export());
        self.cancel_btn.connect_clicked(move |_| f());

        for (name, which) in [("open-project", 0), ("save", 1), ("save-as", 2), ("restore", 3)] {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(a) = weak.upgrade() {
                    match which {
                        0 => a.choose_files("Open project", Add::Auto),
                        1 => a.save(false),
                        2 => a.save(true),
                        _ => a.restore(),
                    }
                }
            });
            self.window.add_action(&action);
        }

        // A time on the trim bar is typed over in a text box put on top of it.
        let weak = Rc::downgrade(self);
        *self.trim.on_edit.borrow_mut() = Some(Box::new(move |(which, x, _)| {
            if let Some(a) = weak.upgrade() {
                a.edit_time(which, x);
            }
        }));
        let weak = Rc::downgrade(self);
        self.time_entry.connect_activate(move |e| {
            if let (Some(a), text) = (weak.upgrade(), e.text()) {
                if let Some(which) = a.editing.get() {
                    a.time_typed(&text, match which {
                        TrimLabel::Pos => "pos",
                        TrimLabel::In => "in",
                        TrimLabel::Out => "out",
                    });
                }
            }
        });
        let left = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        left.connect_leave(move |_| {
            if let Some(a) = weak.upgrade() {
                a.stop_editing();
            }
        });
        self.time_entry.add_controller(left);
        let weak = Rc::downgrade(self);
        *self.trim.on_unfit.borrow_mut() = Some(Box::new(move |()| {
            if let Some(a) = weak.upgrade() {
                a.fit_btn.set_active(false);
            }
        }));
        for b in &self.mute {
            let weak = Rc::downgrade(self);
            b.connect_toggled(move |b| {
                b.set_icon_name(if b.is_active() { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
                if let Some(a) = weak.upgrade() {
                    a.refresh();
                    a.sync_lanes();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.volume.connect_value_changed(move |s| {
            if let Some(a) = weak.upgrade() {
                a.volume_changed(s.value());
            }
        });
        for (shape, button) in &self.shape_btns {
            let (weak, shape) = (Rc::downgrade(self), shape.clone());
            button.connect_toggled(move |b| {
                if let (Some(a), true) = (weak.upgrade(), b.is_active()) {
                    a.shape_chosen(&shape);
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.lock.connect_toggled(move |b| {
            if let Some(a) = weak.upgrade() {
                a.st.borrow_mut().s.lock_shape = b.is_active();
            }
        });
        let weak = Rc::downgrade(self);
        self.exact.connect_toggled(move |b| {
            if let Some(a) = weak.upgrade() {
                a.st.borrow_mut().s.exact = b.is_active();
                a.refresh();
            }
        });

        let weak = Rc::downgrade(self);
        *self.trim.on_scrub.borrow_mut() = Some(Box::new(move |(t, last)| {
            if let Some(a) = weak.upgrade() {
                a.on_scrub(t, last);
            }
        }));
        let weak = Rc::downgrade(self);
        *self.trim.on_trim.borrow_mut() = Some(Box::new(move |(which, t, last)| {
            if let Some(a) = weak.upgrade() {
                a.on_trim(which, t, last);
            }
        }));
        let weak = Rc::downgrade(self);
        *self.crop.on_change.borrow_mut() = Some(Box::new(move |(rect, last)| {
            if let Some(a) = weak.upgrade() {
                a.on_crop(rect, last);
            }
        }));
        let weak = Rc::downgrade(self);
        *self.timeline.on_select.borrow_mut() = Some(Box::new(move |i| {
            if let Some(a) = weak.upgrade() {
                a.on_clip_click(i);
            }
        }));
        let weak = Rc::downgrade(self);
        *self.timeline.on_move.borrow_mut() = Some(Box::new(move |(src, dst, last)| {
            if let Some(a) = weak.upgrade() {
                a.on_clip_move(src, dst, last);
            }
        }));
        let weak = Rc::downgrade(self);
        *self.timeline.on_sound_select.borrow_mut() = Some(Box::new(move |(lane, i)| {
            if let Some(a) = weak.upgrade() {
                a.on_sound_press(lane, i);
            }
        }));
        let weak = Rc::downgrade(self);
        *self.timeline.on_sound_move.borrow_mut() = Some(Box::new(move |(at, lane, last)| {
            if let Some(a) = weak.upgrade() {
                a.on_sound_move(at, lane, last);
            }
        }));
        let weak = Rc::downgrade(self);
        *self.timeline.on_scrub.borrow_mut() = Some(Box::new(move |(t, last)| {
            if let Some(a) = weak.upgrade() {
                a.on_montage_scrub(t, last);
            }
        }));

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| match weak.upgrade() {
            Some(a) if a.key(key, state) => glib::Propagation::Stop,
            _ => glib::Propagation::Proceed,
        });
        self.window.add_controller(keys);

        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        drop.connect_drop(move |_, value, _, _| {
            let (Some(a), Ok(list)) = (weak.upgrade(), value.get::<gdk::FileList>()) else { return false };
            a.open_paths(list.files().iter().filter_map(|f| f.path()).map(|p| p.to_string_lossy().into_owned()).collect());
            true
        });
        self.window.add_controller(drop);

        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(a) = weak.upgrade() {
                a.cancel_export();
                a.autosave();
                a.lane_players.borrow_mut().clear();
            }
            glib::Propagation::Proceed
        });

        // One timer drives everything that arrives from outside the main loop.
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(16), move || match weak.upgrade() {
            Some(a) => {
                a.tick();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    fn tick(self: &Rc<Self>) {
        for ev in self.player.poll() {
            match ev {
                PlayerEvent::Time(t) => self.on_time(t),
                PlayerEvent::Paused(_) => self.show_playing(),
                PlayerEvent::Ended => self.on_ended(),
            }
        }
        while let Ok(msg) = self.rx.try_recv() {
            self.message(msg);
        }
        self.carry_on_starting();
        // The lanes are looked at a few times a second, and at once when the picture starts
        // or stops moving: a sound that starts a moment after its picture is heard as late.
        let moving = self.player.running();
        let due = {
            let mut st = self.st.borrow_mut();
            st.ticks += 1;
            let turned = std::mem::replace(&mut st.was_moving, moving) != moving;
            turned || st.ticks.is_multiple_of(4)
        };
        if due {
            self.sync_lanes();
        }
    }

    fn message(self: &Rc<Self>, msg: Msg) {
        match msg {
            Msg::Added { path, info, add } => self.added(&path, *info, add),
            Msg::Indexed { path, info } => {
                self.st.borrow_mut().s.set_info(&path, *info);
                self.refresh();
            }
            Msg::Opened { file, project, infos } => {
                {
                    let mut st = self.st.borrow_mut();
                    st.s.open(&file, *project, infos);
                    if Path::new(&file) == autosave_file() {
                        st.s.file = None; // a restored session is not a named project
                    }
                    st.kf_cache.clear();
                    st.held = None;
                    st.pos = st.s.clip().map_or(0.0, |c| to_f64(&c.start));
                }
                self.show_target();
            }
            Msg::Note(text) => self.status.set_text(&text),
            Msg::Caps(caps) => {
                self.st.borrow_mut().caps = caps;
                self.refresh();
            }
            Msg::Progress(f) => self.progress.set_fraction(f),
            Msg::ExportDone(text) => {
                self.st.borrow_mut().cancel = None;
                self.progress.set_visible(false);
                self.cancel_btn.set_visible(false);
                self.export_btn.set_sensitive(self.st.borrow().plan.is_some());
                self.status.set_text(&text);
                self.status.set_tooltip_text(Some(&text));
            }
        }
    }

    // ------------------------------------------------------------ session -> widgets

    /// Put the text box over one of the times on the trim bar, holding that time, ready to type over.
    pub fn edit_time(&self, which: TrimLabel, x: f64) {
        let value = {
            let st = self.st.borrow();
            let Some((_, start, end, _)) = st.s.target_span() else { return };
            match which {
                TrimLabel::Pos => st.pos,
                TrimLabel::In => to_f64(&start),
                TrimLabel::Out => to_f64(&end),
            }
        };
        self.editing.set(Some(which));
        self.trim.set_editing(Some(which));
        let e = &self.time_entry;
        e.set_text(&clock(value));
        let room = (self.trim.area.width() - e.measure(gtk::Orientation::Horizontal, -1).1).max(0);
        e.set_margin_start(((x - 5.0) as i32).clamp(0, room));
        e.set_valign(if which == TrimLabel::Pos { gtk::Align::Start } else { gtk::Align::End });
        e.set_visible(true);
        e.grab_focus();
    }

    /// For tests: the text box is up.
    pub fn typing_time(&self) -> bool {
        self.editing.get().is_some() && WidgetExt::is_visible(&self.time_entry)
    }

    /// Take the text box away again. What was typed is used only when Enter is pressed.
    pub fn stop_editing(&self) {
        if self.editing.take().is_some() {
            self.trim.set_editing(None);
            self.time_entry.set_visible(false);
        }
    }

    /// Each play button shows a pause sign while its kind of play is running.
    fn show_playing(&self) {
        let (playing, source) = (self.playing(), self.st.borrow().source_play);
        let icon = |on: bool| if on { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" };
        self.clip_play_btn.set_icon_name(icon(playing && source));
        self.play_btn.set_icon_name(icon(playing && !source));
    }

    /// Where the play line of the finished video is.
    pub fn montage_pos(&self) -> R {
        let st = self.st.borrow();
        match st.held {
            Some(line) => line.min(st.s.total()),
            None => line_of(&st),
        }
    }

    /// Redraw everything from the session and work out what an export would do.
    pub fn refresh(&self) {
        self.syncing.set(true);
        let montage = to_f64(&self.montage_pos());
        let mut st = self.st.borrow_mut();
        let st = &mut *st;
        let (plan, problem) = match st.s.plan() {
            Ok(p) => (p, None),
            Err(e) => (None, Some(e)),
        };
        st.plan = plan;
        let target = st.s.target();
        let clip_mode = matches!(target, Some(Target::Clip(_)) | None);
        let has_clip = st.s.clip().is_some() && clip_mode;

        self.empty.set_visible(target.is_none());
        for w in [self.clip_play_btn.upcast_ref::<gtk::Widget>(), self.fit_btn.upcast_ref(), self.play_btn.upcast_ref(),
                  self.trim.area.upcast_ref(), self.timeline.area.upcast_ref()] {
            w.set_sensitive(target.is_some());
        }
        for b in &self.mute {
            b.set_sensitive(target.is_some());
        }
        for b in &self.add_sound_btns {
            b.set_sensitive(target.is_some()); // sound goes under clips: there must be one
        }
        // a clip with no sound of its own has no volume to set
        let has_sound = st.s.target_has_sound();
        self.volume.set_sensitive(has_sound);
        for w in [self.shapes_box.upcast_ref::<gtk::Widget>(), self.lock.upcast_ref(), self.exact.upcast_ref()] {
            w.set_sensitive(has_clip);
        }
        self.export_btn.set_sensitive(st.plan.is_some() && st.cancel.is_none());
        if st.cancel.is_none() {
            match &problem {
                Some(p) => self.status.set_text(&format!("Cannot export: {p}")),
                None => self.status.set_text(&st.plan.as_ref().map(|p| describe(p, &st.s, &st.caps)).unwrap_or_default()),
            }
            let tip = st.plan.as_ref().map(|p| p.reasons.iter().chain(&p.warnings).cloned().collect::<Vec<_>>().join("\n")).filter(|t| !t.is_empty());
            self.status.set_tooltip_text(tip.as_deref());
        }

        if let (Some(target), Some((media, start, end, gain))) = (target, st.s.target_span().map(|(m, a, b, g)| (m.to_owned(), a, b, g))) {
            let duration = st.s.target_info().map_or(time::zero(), |i| i.duration);
            let mut snapped = None;
            if let (Target::Clip(i), Some(p)) = (target, st.plan.as_ref().filter(|p| p.kind == Kind::Copy)) {
                let seg = &p.segments[i];
                if (seg.start, seg.end) != (start, end) {
                    snapped = Some((to_f64(&seg.start), to_f64(&seg.end)));
                }
            }
            let keyframes = if clip_mode { keyframes_of(st, &media) } else { Rc::new(Vec::new()) };
            self.trim.update(Some((media, clip_mode)), to_f64(&duration), to_f64(&start), to_f64(&end), st.pos, keyframes, snapped);
            let pct = gain_to_percent(gain);
            self.volume.set_value(pct);
            self.volume_lbl.set_text(&if has_sound { format!("{pct:.0}%") } else { String::new() });
            self.montage_lbl.set_text(&format!("{} / {}", clock(montage), clock(to_f64(&st.s.total()))));
        } else {
            self.trim.update(None, 0.0, 0.0, 0.0, 0.0, Rc::new(Vec::new()), None);
            self.volume_lbl.set_text("");
            self.montage_lbl.set_text("");
        }

        match (st.s.selected, st.s.clip(), clip_mode) {
            (Some(i), Some(c), true) => {
                let rect = match c.crop {
                    Some(Crop::Rect(rect)) => Some(rect),
                    _ => None,
                };
                self.crop.update(st.s.display_size(i), rect, c.crop_aspect.as_ref().map(to_f64));
                let shape = st.s.shape();
                if let Some((_, b)) = self.shape_btns.iter().find(|(v, _)| *v == shape).or(self.shape_btns.last()) {
                    b.set_active(true);
                }
                self.player.set_sound(st.s.own_sound(i).is_some() && self.hears(0), c.gain_db);
            }
            _ => {
                self.crop.update((0.0, 0.0), None, None);
                if let (Some(sound), Some(lane)) = (st.s.sound(), st.s.sound_lane()) {
                    self.player.set_sound(self.hears(lane), sound.gain_db);
                }
            }
        }

        let clips = st.s.clips().iter().enumerate().map(|(i, c)| (stem(st.s.path(i)), to_f64(&c.duration()))).collect();
        let shown: Vec<_> = (0..LANES).map(|n| st.s.lane_items(n)).collect();
        let selected_sound = st.s.sound_ref().and_then(|sel| {
            shown.iter().enumerate().find_map(|(n, lane)| Some((n, lane.iter().position(|(r, _, _)| *r == sel)?)))
        });
        let sounds = shown.iter().map(|lane| {
            lane.iter().map(|(_, it, linked)| SoundBox {
                label: stem(&st.s.project.media[&it.media]),
                at: to_f64(&it.at),
                seconds: to_f64(&it.duration()),
                linked: *linked,
            }).collect()
        }).collect();
        self.timeline.update(clips, st.s.selected, sounds, selected_sound, montage, (0..LANES).map(|n| !self.hears(n)).collect());

        let name = st.s.file.as_deref().map(stem);
        self.window.set_title(Some(&match name {
            Some(n) => format!("{}{n} — {APP_NAME}", if st.s.dirty() && !st.s.clips().is_empty() { "• " } else { "" }),
            None => APP_NAME.to_owned(),
        }));
        self.syncing.set(false);
    }

    /// Move both play lines to the play position, and say where it is in the finished video.
    fn show_time(&self) {
        let montage = to_f64(&self.montage_pos());
        let (pos, total) = {
            let st = self.st.borrow();
            (st.pos, to_f64(&st.s.total()))
        };
        self.trim.set_pos(pos, self.player.playing());
        self.timeline.set_pos(montage);
        self.montage_lbl.set_text(&format!("{} / {}", clock(montage), clock(total)));
    }

    // ------------------------------------------------------------ player -> window

    fn on_time(self: &Rc<Self>, t: f64) {
        let next = {
            let mut st = self.st.borrow_mut();
            let Some((_, _, end, _)) = st.s.target_span() else { return };
            if st.dragging || st.detached {
                return;
            }
            st.pos = t;
            st.pos_at = Instant::now();
            if self.player.playing() {
                st.held = None;
            }
            let crossed = self.player.playing() && st.last_pos < to_f64(&end) && to_f64(&end) <= t;
            st.last_pos = t;
            match (crossed, st.source_play, st.s.target()) {
                // Playing the bar by itself runs past the out point, unless the bar shows only the kept range.
                (true, true, _) if !self.trim.fit() => None,
                // The finished video: on reaching this clip's out point, carry on into the next clip.
                (true, false, Some(Target::Clip(i))) if i + 1 < st.s.clips().len() => Some(Some(i + 1)),
                (true, _, _) => Some(None),
                _ => None,
            }
        };
        self.show_time();
        match next {
            Some(Some(i)) => self.select(i, None, true),
            Some(None) => {
                // The player is by now a frame or so past the out point: rest on the last frame kept.
                self.pause();
                self.seek(self.cut_frame(TrimHit::Out), true);
            }
            None => {}
        }
    }

    /// The player ran into the end of the file before the clip's out point was seen to pass
    /// (a file can be a little shorter than it says): carry on as if it had passed.
    fn on_ended(&self) {
        if !self.player.ended() {
            return; // playback has already moved on
        }
        let next = {
            let st = self.st.borrow();
            match st.s.target() {
                Some(Target::Clip(i)) if !st.source_play && i + 1 < st.s.clips().len() => Some(i + 1),
                _ => None,
            }
        };
        if let Some(i) = next {
            self.select(i, None, true);
        }
        self.show_playing();
    }

    /// Whether a sound lane is heard in the preview. The export always has every lane.
    pub fn hears(&self, lane: usize) -> bool {
        self.mute.get(lane).is_none_or(|b| !b.is_active())
    }

    /// Keep the sound lanes in step with the finished video: playing along with it, and
    /// while it is paused, waiting at the play line for it to start.
    fn sync_lanes(&self) {
        let line = self.montage_pos();
        let st = self.st.borrow();
        // The lanes go with the clips: not while one sound is worked on, nor while a line is dragged.
        let live = st.s.clip().is_some() && st.s.sound().is_none() && !st.dragging;
        // About to play: the sounds go first, by the time they take to be heard.
        let ahead = match st.starting {
            Starting::Sounds(_, lead) if live => Some(lead),
            _ => None,
        };
        let rolling = live && self.player.playing() && !st.detached && !st.source_play;
        // The player reports a time once per picture; between reports it has moved on. Without
        // this the lanes would be steered by a clock that moves in steps.
        let moving = self.player.running();
        let since = if moving { st.pos_at.elapsed().as_secs_f64().min(0.06) } else { 0.0 };
        let at = if rolling { st.s.montage_time(&floor_us(st.pos + since)) } else { line };
        for (n, lane_player) in self.lane_players.borrow_mut().iter_mut().enumerate() {
            match st.s.sound_cue(n, &at).filter(|_| live && self.hears(n)) {
                Some((item, source_time, sounding)) => {
                    let want = (st.s.project.media[&item.media].as_str(), to_f64(&source_time), st.s.lanes()[n].gain_db + item.gain_db);
                    // a sound that begins during play is likewise let go a little before its moment
                    let until = to_f64(&(item.at - at));
                    let soon = !sounding && rolling && moving && until <= lane_player.start_lead();
                    match ahead {
                        Some(lead) => lane_player.sync(Some(want), sounding, true, lead),
                        None => lane_player.sync(Some(want), rolling && (sounding || soon), moving, if soon { until } else { 0.0 }),
                    }
                }
                None => lane_player.sync(None, false, false, 0.0),
            }
        }
    }

    /// Playing, or about to: what the buttons and the space bar go by.
    pub fn playing(&self) -> bool {
        self.player.playing() || self.st.borrow().starting != Starting::No
    }

    /// Stop the picture, and any start that was under way.
    fn stop(&self) {
        self.st.borrow_mut().starting = Starting::No;
        self.player.set_playing(false);
    }

    /// Start the finished video. Sound in a lane is heard a moment after it is let go, so
    /// when a lane has something to play here, the sounds go first and the picture follows
    /// when they are heard. Called on every tick until the picture moves.
    fn carry_on_starting(&self) {
        let starting = self.st.borrow().starting;
        let go = match starting {
            Starting::No => return,
            Starting::Finding if !self.player.settled() => return,
            Starting::Finding => {
                let lead = self.lanes_lead(&self.montage_pos());
                if lead > 0.02 {
                    self.st.borrow_mut().starting = Starting::Sounds(Instant::now() + Duration::from_secs_f64(lead), lead);
                    self.sync_lanes();
                }
                lead <= 0.02
            }
            Starting::Sounds(at, _) => Instant::now() >= at,
        };
        if go {
            self.st.borrow_mut().starting = Starting::No;
            self.player.set_playing(true);
            self.sync_lanes();
            self.show_playing();
        }
    }

    // ------------------------------------------------------------ moving around

    fn load(&self, playing: bool) {
        let (path, pos) = {
            let mut st = self.st.borrow_mut();
            st.starting = Starting::No;
            st.detached = false;
            st.last_pos = st.pos;
            (st.s.target_path().map(str::to_owned), st.pos)
        };
        if let Some(path) = path {
            self.player.load(&path, pos, playing);
        }
    }

    pub fn select(&self, i: usize, pos: Option<f64>, playing: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.held = None;
            st.s.select(i);
            st.pos = pos.unwrap_or_else(|| st.s.clip().map_or(0.0, |c| to_f64(&c.start)));
        }
        self.load(playing);
        self.refresh();
    }

    /// After an edit that may have changed what is selected (undo, remove, open).
    fn show_target(&self) {
        let has = {
            let mut st = self.st.borrow_mut();
            match st.s.target_info().map(|i| to_f64(&i.duration)) {
                Some(duration) => {
                    st.pos = st.pos.clamp(0.0, duration);
                    true
                }
                None => {
                    st.pos = 0.0;
                    false
                }
            }
        };
        if has {
            self.load(false);
        } else {
            self.player.unload();
        }
        self.refresh();
    }

    fn back_to_clips(&self) {
        let line = self.montage_pos();
        {
            let mut st = self.st.borrow_mut();
            if st.s.sound().is_none() {
                return;
            }
            st.s.clear_sound();
            let back = st.clip_pos;
            keep_line(&mut st, line, back);
        }
        self.show_target();
    }

    /// Put the preview back on the play position after it has been showing a cut point.
    fn reattach(&self) {
        let pos = {
            let mut st = self.st.borrow_mut();
            if !st.detached {
                return;
            }
            st.detached = false;
            st.pos
        };
        self.player.seek(pos, true);
    }

    /// Show the frame at t without moving the play position.
    fn preview(&self, t: f64) {
        self.st.borrow_mut().detached = true;
        self.stop();
        self.player.seek(t, true);
    }

    pub fn seek(&self, t: f64, exact: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.detached = false;
            st.held = None;
            st.pos = t;
            st.last_pos = t;
        }
        self.player.seek(t, exact);
        self.show_time();
    }

    pub fn pause(&self) {
        self.stop();
        self.sync_lanes();
        self.show_playing();
    }

    /// The button at the start of the trim bar, and Shift+Space: play what the bar shows, by
    /// itself. That is the whole file, past the in and out points, or only the kept range when
    /// the bar shows only that. At the end it starts again from the beginning.
    pub fn play_source(&self) {
        self.stop_editing();
        let restart = {
            let mut st = self.st.borrow_mut();
            if st.s.target().is_none() {
                return;
            }
            let playing = self.player.playing() || st.starting != Starting::No;
            if playing && st.source_play {
                drop(st);
                return self.pause();
            }
            st.starting = Starting::No;
            st.source_play = true;
            let (lo, hi) = self.trim.span();
            let near = to_f64(&st.s.frame()) * 1.5;
            let ended = self.player.ended() && !st.detached;
            (ended || st.pos >= hi - near || st.pos < lo - near).then_some(lo)
        };
        match restart {
            Some(lo) => self.seek(lo, true),
            None => self.reattach(),
        }
        self.player.set_playing(true);
        self.show_playing();
    }

    /// The button at the start of the montage, and Space: play the finished video from the
    /// play line, through every clip. At the end it starts again from the beginning.
    pub fn play_montage(&self) {
        self.stop_editing();
        let jump = {
            let mut st = self.st.borrow_mut();
            if st.s.clips().is_empty() {
                return;
            }
            let playing = self.player.playing() || st.starting != Starting::No;
            if playing && !st.source_play {
                drop(st);
                return self.pause();
            }
            st.source_play = false;
            let pos = floor_us(st.pos);
            let near = st.s.frame() * r(3, 2);
            let ended = self.player.ended() && !st.detached;
            // where the play line is on the montage, or None when the preview is already there
            let at = match (st.held, st.s.sound(), st.s.selected, st.s.clip()) {
                (Some(line), ..) => Some(line),
                (None, Some(sound), _, _) => Some(sound.at + (pos - sound.start).max(time::zero())),
                (None, None, Some(i), Some(c)) if ended || pos >= c.end - near => Some(st.s.offsets()[i] + c.duration()),
                (None, None, Some(i), Some(c)) if pos < c.start => Some(st.s.offsets()[i]),
                (None, None, Some(_), Some(_)) => None,
                _ => return,
            };
            // ...and where playing will begin: at the end, the beginning
            let from = at.map(|at| if at >= st.s.total() - near { time::zero() } else { at });
            (from.and_then(|from| st.s.locate(&from)), from.unwrap_or_else(|| line_of(&st)))
        };
        let (jump, from) = jump;
        if self.lanes_lead(&from) <= 0.02 {
            // nothing in the lanes to wait for: the picture starts at once
            match jump {
                Some((i, source)) => self.select(i, Some(to_f64(&source)), true),
                None => {
                    self.reattach();
                    self.player.set_playing(true);
                }
            }
        } else {
            // the sounds go first; see carry_on_starting
            match jump {
                Some((i, source)) => self.select(i, Some(to_f64(&source)), false),
                None if self.player.playing() => {} // it was playing the bar by itself: carry straight on
                None => self.reattach(),
            }
            if !self.player.playing() {
                self.st.borrow_mut().starting = Starting::Finding;
                self.carry_on_starting();
            }
        }
        self.sync_lanes();
        self.show_playing();
    }

    /// How long before the picture the lanes' sounds must be let go, to be heard with it when
    /// playing starts at `line` of the finished video. Zero when no lane has a sound there.
    fn lanes_lead(&self, line: &R) -> f64 {
        let st = self.st.borrow();
        let players = self.lane_players.borrow();
        (0..LANES)
            .filter(|&n| self.hears(n) && st.s.sound_cue(n, line).is_some_and(|cue| cue.2))
            .map(|n| players[n].start_lead())
            .fold(0.0, f64::max)
    }

    // ------------------------------------------------------------ editing

    fn begin_gesture(&self) {
        let mut st = self.st.borrow_mut();
        if !st.gesture {
            st.s.checkpoint();
            st.gesture = true;
        }
    }

    fn on_scrub(&self, t: f64, last: bool) {
        self.stop_editing();
        self.st.borrow_mut().dragging = !last;
        self.stop();
        self.seek(t, last); // keyframes while dragging, the exact frame on release
    }

    /// The play line on the montage is dragged: move through the clips.
    fn on_montage_scrub(&self, t: f64, last: bool) {
        self.stop_editing();
        let (i, source, change) = {
            let mut st = self.st.borrow_mut();
            let Some((i, source)) = st.s.locate(&floor_us(t)) else { return };
            st.dragging = !last;
            (i, to_f64(&source), st.s.sound().is_some() || st.s.selected != Some(i))
        };
        self.stop();
        if change {
            self.select(i, Some(source), false);
        } else {
            self.seek(source, last);
        }
    }

    /// The frame to show for a cut point: the first frame kept, or the last frame kept.
    fn cut_frame(&self, which: TrimHit) -> f64 {
        let st = self.st.borrow();
        let Some((_, start, end, _)) = st.s.target_span() else { return 0.0 };
        if which == TrimHit::In {
            to_f64(&start)
        } else {
            to_f64(&(end - st.s.frame()).max(start))
        }
    }

    fn on_trim(&self, which: TrimHit, t: f64, last: bool) {
        self.stop_editing();
        self.begin_gesture();
        {
            let mut st = self.st.borrow_mut();
            if which == TrimHit::In {
                st.s.set_in(floor_us(t));
            } else {
                st.s.set_out(floor_us(t));
            }
            if last {
                st.gesture = false;
            }
        }
        self.preview(self.cut_frame(which)); // the play position stays where it is
        self.refresh();
    }

    pub fn time_typed(&self, text: &str, which: &str) {
        let t = parse_clock(text);
        self.stop_editing();
        let has = self.st.borrow().s.target().is_some();
        if let (Some(t), true) = (t, has) {
            if which == "pos" {
                let duration = self.st.borrow().s.target_info().map_or(time::zero(), |i| i.duration);
                self.stop();
                self.seek(to_f64(&t.min(duration)), true);
            } else {
                let hit = if which == "in" { TrimHit::In } else { TrimHit::Out };
                {
                    let mut st = self.st.borrow_mut();
                    st.s.checkpoint();
                    if hit == TrimHit::In {
                        st.s.set_in(t);
                    } else {
                        st.s.set_out(t);
                    }
                }
                self.preview(self.cut_frame(hit));
            }
        }
        self.refresh();
    }

    fn on_crop(&self, rect: Rect, last: bool) {
        self.begin_gesture();
        {
            let mut st = self.st.borrow_mut();
            st.s.set_crop(rect);
            if last {
                st.gesture = false;
            }
        }
        self.refresh();
    }

    /// A click on a clip selects it, and leaves the play line of the finished video where it is.
    fn on_clip_click(&self, i: usize) {
        self.stop_editing();
        let line = self.montage_pos();
        {
            let mut st = self.st.borrow_mut();
            let same = st.s.selected == Some(i);
            if i >= st.s.clips().len() || (same && st.s.sound().is_none()) {
                return; // it is already the one being worked on
            }
            st.s.select(i);
            let at = if same { st.clip_pos } else { to_f64(&st.s.clips()[i].start) };
            keep_line(&mut st, line, at);
        }
        self.load(false);
        self.refresh();
    }

    fn on_clip_move(&self, src: usize, dst: usize, last: bool) {
        if last {
            self.st.borrow_mut().gesture = false;
            return;
        }
        self.begin_gesture();
        {
            let mut st = self.st.borrow_mut();
            st.held = None;
            st.s.clear_sound();
            st.s.move_clip(src, dst);
        }
        self.refresh();
    }

    /// A press on a sound selects it, and leaves the play line of the finished video where it is.
    fn on_sound_press(&self, lane: usize, i: usize) {
        self.stop_editing();
        let line = self.montage_pos();
        {
            let mut st = self.st.borrow_mut();
            let Some(sound) = st.s.lane_items(lane).get(i).map(|item| item.0) else { return };
            if st.s.sound_ref() == Some(sound) {
                return;
            }
            match sound {
                // another clip's own sound: Escape goes back to that clip
                SoundRef::Own(clip) if st.s.selected != Some(clip) => st.clip_pos = to_f64(&st.s.clips()[clip].start),
                _ if st.s.sound().is_none() => st.clip_pos = st.pos,
                _ => {}
            }
            st.s.select_sound(sound);
            let at = st.s.sound().map_or(0.0, |s| to_f64(&s.start));
            keep_line(&mut st, line, at);
        }
        self.load(false);
        self.refresh();
    }

    /// The selected sound is dragged. The play line of the finished video stays where it is,
    /// so the play position within the sound changes instead: the trim bar shows which part
    /// of the sound is now under the line.
    fn on_sound_move(&self, at: f64, lane: Option<usize>, last: bool) {
        if last {
            self.st.borrow_mut().gesture = false;
            self.load(false); // and the preview goes there once the sound is let go of
            self.refresh();
            return;
        }
        self.begin_gesture();
        let line = self.montage_pos();
        {
            let mut st = self.st.borrow_mut();
            st.s.move_sound(floor_us(at), lane);
            let pos = st.pos;
            keep_line(&mut st, line, pos);
        }
        self.refresh();
    }

    /// Left and Right with a sound selected: slide it along its lane by one picture of the
    /// clip, or with Shift by a second. The play line stays where it is, as when dragging.
    fn nudge_sound(&self, later: bool, second: bool) {
        self.stop_editing();
        self.stop();
        let line = self.montage_pos();
        {
            let mut st = self.st.borrow_mut();
            let picture = st.s.clip().and_then(|c| st.s.infos.get(&c.media)).and_then(|i| i.video.as_ref()).map_or(r(1, 30), |v| v.fps.recip());
            let step = if second { time::int(1) } else { picture };
            // one undo step per run of presses, not per press
            if st.nudge_edit.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                st.s.checkpoint();
            }
            st.nudge_edit = Some(Instant::now());
            st.s.nudge_sound(if later { step } else { -step });
            let pos = st.pos;
            keep_line(&mut st, line, pos);
        }
        self.load(false);
        self.refresh();
    }

    fn shape_chosen(&self, shape: &Shape) {
        if self.syncing.get() {
            return;
        }
        self.st.borrow_mut().s.set_shape(shape);
        self.refresh();
    }

    fn volume_changed(&self, percent: f64) {
        if self.syncing.get() {
            return;
        }
        {
            let mut st = self.st.borrow_mut();
            if !st.s.target_has_sound() {
                return;
            }
            // one undo step per adjustment, not per pixel of movement
            if st.volume_edit.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
                st.s.checkpoint();
            }
            st.volume_edit = Some(Instant::now());
            st.s.set_gain(percent_to_gain(percent));
        }
        self.refresh();
    }

    /// Returns true when the key was ours.
    pub fn key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> bool {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let name = key.name().map(|n| n.to_string()).unwrap_or_default();
        let low = name.to_lowercase();
        if self.editing.get().is_some() {
            // typing a time: only Escape is ours
            if name == "Escape" {
                self.stop_editing();
                return true;
            }
            return false;
        }
        if ctrl {
            match low.as_str() {
                "z" => {
                    let changed = {
                        let mut st = self.st.borrow_mut();
                        st.held = None;
                        if shift { st.s.redo() } else { st.s.undo() }
                    };
                    if changed {
                        self.show_target();
                    }
                }
                "o" => self.choose_files("Add video files or open a project", Add::Auto),
                "e" => self.export(),
                "s" => self.save(shift),
                _ => return false,
            }
            return true;
        }
        let (has, pos, sound_mode) = {
            let st = self.st.borrow();
            (st.s.target().is_some(), st.pos, st.s.sound().is_some())
        };
        if !has {
            return false;
        }
        match name.as_str() {
            "space" if self.playing() => self.pause(),
            "space" if shift => self.play_source(),
            "space" => self.play_montage(),
            "Left" | "Right" if sound_mode => self.nudge_sound(name == "Right", shift),
            "Left" | "Right" => {
                self.stop();
                self.reattach();
                if shift {
                    let duration = self.st.borrow().s.target_info().map_or(0.0, |i| to_f64(&i.duration));
                    self.seek((pos + if name == "Right" { 1.0 } else { -1.0 }).clamp(0.0, duration), true);
                } else {
                    self.st.borrow_mut().held = None;
                    self.player.step(name == "Right");
                }
            }
            "Delete" | "BackSpace" => {
                {
                    let mut st = self.st.borrow_mut();
                    st.held = None;
                    if sound_mode {
                        st.s.remove_sound();
                        st.pos = st.clip_pos;
                    } else if let Some(i) = st.s.selected {
                        st.s.remove(i);
                    }
                }
                self.show_target();
            }
            "Escape" => self.back_to_clips(),
            "Home" => self.seek(self.cut_frame(TrimHit::In), true),
            "End" => self.seek(self.cut_frame(TrimHit::Out), true),
            _ => match low.as_str() {
                "i" => {
                    {
                        let mut st = self.st.borrow_mut();
                        st.s.checkpoint();
                        st.s.set_in(floor_us(pos));
                    }
                    self.refresh();
                }
                "o" => {
                    {
                        let mut st = self.st.borrow_mut();
                        st.s.checkpoint();
                        let frame = st.s.frame();
                        st.s.set_out(floor_us(pos) + frame); // the frame on screen is the last one kept
                    }
                    self.refresh();
                }
                "s" => {
                    if self.st.borrow_mut().s.split(floor_us(pos)) {
                        self.refresh();
                    }
                }
                _ => return false,
            },
        }
        true
    }

    // ------------------------------------------------------------ adding files and projects

    fn choose_files(self: &Rc<Self>, title: &str, add: Add) {
        let weak = Rc::downgrade(self);
        gtk::FileDialog::builder().title(title).build().open_multiple(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let (Some(a), Ok(files)) = (weak.upgrade(), result) else { return }; // dismissed
            let paths: Vec<String> = (0..files.n_items())
                .filter_map(|i| files.item(i).and_downcast::<gio::File>()?.path())
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            match add {
                Add::Auto => a.open_paths(paths),
                lane => a.add_files(paths, lane),
            }
        });
    }

    /// Projects are opened; video files become clips; sound-only files go to a sound lane.
    pub fn open_paths(&self, paths: Vec<String>) {
        let is_project = |p: &String| p.to_lowercase().ends_with(PROJECT_EXT) || p.to_lowercase().ends_with(".json");
        if let Some(project) = paths.iter().find(|p| is_project(p)) {
            self.open_project(project);
        }
        self.add_files(paths.into_iter().filter(|p| !is_project(p)).collect(), Add::Auto);
    }

    pub fn add_files(&self, paths: Vec<String>, add: Add) {
        if paths.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for path in paths {
                // one at a time, so clips arrive in the order given
                let path = absolute(&path);
                let quick = match probe(&path, false) {
                    Ok(info) => info,
                    Err(e) => {
                        let _ = tx.send(Msg::Note(format!("Could not open {}: {e}", file_name(&path))));
                        continue;
                    }
                };
                let wants_index = quick.video.is_some() && add == Add::Auto;
                let _ = tx.send(Msg::Added { path: path.clone(), info: Box::new(quick), add });
                if wants_index {
                    // keyframe index; slower on big files, needed for lossless export
                    if let Ok(full) = probe(&path, true) {
                        let _ = tx.send(Msg::Indexed { path, info: Box::new(full) });
                    }
                }
            }
        });
    }

    fn added(&self, path: &str, info: MediaInfo, add: Add) {
        let lane = match add {
            Add::Lane(n) => Some(n),
            Add::Auto => None,
        };
        if lane.is_some() || info.video.is_none() {
            let at = self.montage_pos();
            let result = {
                let mut st = self.st.borrow_mut();
                if info.audio.is_none() {
                    Err(format!("{} has no sound", file_name(path)))
                } else if st.s.clip().is_none() {
                    Err("Add a video first; sound goes under the clips".to_string())
                } else {
                    if st.s.sound().is_none() {
                        st.clip_pos = st.pos;
                    }
                    // the new item starts at the play line, and the play position at the item's start
                    st.s.add_sound(path, info, at, lane).map(|_| {
                        st.pos = 0.0;
                        st.held = None;
                    })
                }
            };
            match result {
                Ok(()) => {
                    self.load(false);
                    self.refresh();
                }
                Err(e) => self.status.set_text(&e),
            }
            return;
        }
        let added = self.st.borrow_mut().s.add_media(path, info);
        match added {
            Ok(i) => self.select(i, None, false),
            Err(e) => self.status.set_text(&e),
        }
    }

    pub fn open_project(&self, file: &str) {
        let (tx, file) = (self.tx.clone(), absolute(file));
        std::thread::spawn(move || {
            let read = || -> Result<(Project, Infos), String> {
                let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
                let json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                let project = Session::read(&file, &json)?;
                let mut infos = Infos::new();
                for (id, path) in &project.media {
                    infos.insert(id.clone(), probe(path, true)?);
                }
                Ok((project, infos))
            };
            let _ = tx.send(match read() {
                Ok((project, infos)) => Msg::Opened { file, project: Box::new(project), infos },
                Err(e) => Msg::Note(format!("Could not open {}: {e}", file_name(&file))),
            });
        });
    }

    pub fn save(self: &Rc<Self>, ask: bool) {
        let (file, first) = {
            let st = self.st.borrow();
            if st.s.clips().is_empty() {
                return;
            }
            (st.s.file.clone(), st.s.path(0).to_owned())
        };
        if let (Some(file), false) = (file, ask) {
            self.save_to(&file);
            return;
        }
        let dialog = gtk::FileDialog::builder().title("Save project").initial_name(format!("{}{PROJECT_EXT}", stem(&first))).build();
        if let Some(dir) = Path::new(&first).parent() {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let weak = Rc::downgrade(self);
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let (Some(a), Some(path)) = (weak.upgrade(), result.ok().and_then(|f| f.path())) else { return };
            let path = path.to_string_lossy().into_owned();
            a.save_to(&if path.to_lowercase().ends_with(PROJECT_EXT) { path } else { format!("{path}{PROJECT_EXT}") });
        });
    }

    pub fn save_to(&self, path: &str) {
        let path = absolute(path);
        let written = write_project(&self.st.borrow().s, &path); // the borrow ends here, before the state is changed below
        match written {
            Ok(()) => {
                self.st.borrow_mut().s.mark_saved(&path);
                self.refresh();
                self.status.set_text(&format!("Saved project {path}"));
            }
            Err(e) => self.status.set_text(&format!("Could not save: {e}")),
        }
    }

    pub fn restore(&self) {
        let file = autosave_file();
        if file.exists() {
            self.open_project(&file.to_string_lossy());
        } else {
            self.status.set_text("No earlier session to reopen");
        }
    }

    /// Keep the last session, so closing the window never loses an edit.
    /// This does not count as saving the named project.
    pub fn autosave(&self) {
        let st = self.st.borrow();
        if st.s.clips().is_empty() {
            return;
        }
        let file = autosave_file();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = write_project(&st.s, &file.to_string_lossy());
    }

    // ------------------------------------------------------------ export

    fn export(self: &Rc<Self>) {
        let suggested = {
            let st = self.st.borrow();
            match (&st.plan, &st.cancel) {
                (Some(p), None) => PathBuf::from(st.s.default_output(p)),
                _ => return,
            }
        };
        let dialog = gtk::FileDialog::builder().title("Export").initial_name(file_name(&suggested.to_string_lossy())).build();
        if let Some(dir) = suggested.parent() {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let weak = Rc::downgrade(self);
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let (Some(a), Some(path)) = (weak.upgrade(), result.ok().and_then(|f| f.path())) else { return };
            a.export_to(&path.to_string_lossy());
        });
    }

    pub fn export_to(&self, out: &str) {
        let out = absolute(out);
        let (plan, paths, cancel) = {
            let mut st = self.st.borrow_mut();
            if st.cancel.is_some() {
                return;
            }
            if st.s.project.media.values().any(|p| *p == out) {
                drop(st);
                self.status.set_text("Cannot export over a source file. Choose another name.");
                return;
            }
            let plan = match st.s.plan() {
                Ok(Some(p)) => p,
                Ok(None) => return,
                Err(e) => {
                    drop(st);
                    self.status.set_text(&format!("Cannot export: {e}"));
                    return;
                }
            };
            let cancel = Arc::new(AtomicBool::new(false));
            st.cancel = Some(cancel.clone());
            (plan, st.s.project.media.clone(), cancel)
        };
        self.stop();
        self.export_btn.set_sensitive(false);
        self.progress.set_fraction(0.0);
        self.progress.set_visible(true);
        self.cancel_btn.set_visible(true);
        self.status.set_text(&format!("Exporting {}…", file_name(&out)));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let progress_tx = tx.clone();
            let result = ffmpeg::run(&plan, &paths, &out, move |f| { let _ = progress_tx.send(Msg::Progress(f)); }, &cancel, Encoder::Auto);
            let _ = tx.send(Msg::ExportDone(match result {
                Ok(()) => format!("Saved {out} ({:.1} s)", started.elapsed().as_secs_f64()),
                Err(ExportError::Cancelled) => "Export cancelled".to_string(),
                Err(ExportError::Failed(e)) => format!("Export failed: {}", e.lines().last().unwrap_or(&e)),
            }));
        });
    }

    pub fn cancel_export(&self) {
        if let Some(c) = &self.st.borrow().cancel {
            c.store(true, Ordering::Relaxed);
        }
    }
}

/// Where the play position falls in the finished video.
fn line_of(st: &St) -> R {
    match st.s.sound() {
        Some(sound) => sound.at + (floor_us(st.pos) - sound.start).max(time::zero()),
        None if st.s.clip().is_some() => st.s.montage_time(&floor_us(st.pos)),
        None => time::zero(),
    }
}

/// After a click has changed what is selected: keep the play line of the finished video at
/// `line`. If that moment lies within what is now selected, the play position becomes that
/// same moment. Otherwise the play position goes to `at`, and the line is held where it was.
fn keep_line(st: &mut St, line: R, at: f64) {
    let span = match (st.s.sound(), st.s.selected, st.s.clip()) {
        (Some(s), _, _) => Some((s.at, s.start, s.end)),
        (None, Some(i), Some(c)) => Some((st.s.offsets()[i], c.start, c.end)),
        _ => None,
    };
    match span {
        Some((from, start, end)) if from <= line && line < from + (end - start) => {
            st.pos = to_f64(&(start + (line - from)));
            st.held = None;
        }
        _ => {
            st.pos = at;
            st.held = (line_of(st) != line).then_some(line);
        }
    }
}

fn keyframes_of(st: &mut St, media: &str) -> Rc<Vec<f64>> {
    let marker = st.s.infos.get(media).and_then(|i| i.keyframes.as_ref()).map(Vec::len);
    if let Some((m, cached)) = st.kf_cache.get(media) {
        if *m == marker {
            return cached.clone();
        }
    }
    let list: Rc<Vec<f64>> = Rc::new(st.s.infos.get(media).and_then(|i| i.keyframes.as_ref()).map(|k| k.iter().map(to_f64).collect()).unwrap_or_default());
    st.kf_cache.insert(media.to_owned(), (marker, list.clone()));
    list
}

fn describe(p: &ExportPlan, s: &Session, caps: &Capabilities) -> String {
    if p.kind == Kind::Copy {
        let extra = to_f64(&(p.duration - s.total()));
        let cuts = if extra > 0.0005 { format!("cuts move out to keyframes, +{extra:.1} s") } else { "cuts are exact".to_string() };
        return format!("Lossless copy · {} · {cuts}", clock(to_f64(&p.duration)));
    }
    let v = p.video.as_ref().expect("an encode plan has a target");
    let kind = if v.color == "sdr" { "standard range" } else { "HDR" };
    let mut text = format!("Re-encode · {} · {}×{} · {} fps · {kind}", clock(to_f64(&p.duration)), v.width, v.height, pve_core::planner::fmt_g(to_f64(&v.fps)));
    if ffmpeg::uses_gpu_encoder(p, caps, Encoder::Auto).unwrap_or(false) {
        text += " · GPU encoder";
    }
    if !p.reasons.is_empty() && !s.exact {
        let more = if p.reasons.len() > 1 { format!(" (+{} more)", p.reasons.len() - 1) } else { String::new() };
        text += &format!(" · not lossless: {}{more}", p.reasons[0]);
    }
    text
}

/// Write the project next to its final name first, then rename, so a crash cannot leave half a file.
fn write_project(s: &Session, file: &str) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&s.file_json(file)).map_err(|e| e.to_string())? + "\n";
    let tmp = format!("{file}.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, file).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_and_parse() {
        assert_eq!((short_clock(0.0), short_clock(7.36), short_clock(75.0), short_clock(3723.96)), ("0:00.0".into(), "0:07.4".into(), "1:15.0".into(), "1:02:04.0".into()));
        assert_eq!(clock(0.0), "0:00.000");
        assert_eq!(clock(75.5), "1:15.500");
        assert_eq!(clock(3723.5), "1:02:03.500");
        assert_eq!(parse_clock("83.5"), Some(r(167, 2)));
        assert_eq!(parse_clock("1:23.5"), Some(r(167, 2)));
        assert_eq!(parse_clock("1:02:03.25"), Some(r(14893, 4)));
        assert_eq!(parse_clock(" 15 "), Some(time::int(15)));
        assert_eq!(parse_clock("1,5"), Some(r(3, 2)));
        for bad in ["", "nonsense", "1:2:3:4", "-5", "1/2", "1:-2"] {
            assert_eq!(parse_clock(bad), None, "{bad}");
        }
    }

    #[test]
    fn volume_scale() {
        assert_eq!(gain_to_percent(0.0), 100.0);
        assert!((gain_to_percent(percent_to_gain(50.0)) - 50.0).abs() < 1e-9);
        assert!((percent_to_gain(50.0) + 6.0206).abs() < 1e-3);
        assert_eq!(percent_to_gain(0.0), MIN_GAIN_DB);
        assert_eq!(gain_to_percent(MIN_GAIN_DB), 0.0);
    }

    #[test]
    fn player_times_round_down_to_microseconds() {
        assert_eq!(floor_us(5.0666669), r(5_066_666, 1_000_000));
        assert_eq!(floor_us(5.0), time::int(5));
    }
}
