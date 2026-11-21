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

use crate::geom::{SoundBox, TrimHit};
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

/// Results coming back from worker threads.
enum Msg {
    Added { path: String, info: Box<MediaInfo>, as_sound: bool },
    Indexed { path: String, info: Box<MediaInfo> },
    Opened { file: String, project: Box<Project>, infos: Infos },
    Note(String),
    Caps(Capabilities),
    Progress(f64),
    ExportDone(String),
}

pub struct St {
    pub s: Session,
    /// Play position: source time within the target (selected clip, or sound item).
    pub pos: f64,
    last_pos: f64,
    /// Play position to go back to after working on a sound item.
    clip_pos: f64,
    pub plan: Option<ExportPlan>,
    /// A drag is in progress and has already been checkpointed.
    gesture: bool,
    /// The play position follows the pointer, not the player.
    dragging: bool,
    /// The preview is showing a cut point, not the play position.
    pub detached: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    /// When the volume was last changed (one undo step per adjustment).
    volume_edit: Option<Instant>,
    caps: Capabilities,
    kf_cache: HashMap<String, (Option<usize>, Rc<Vec<f64>>)>,
    ticks: u64,
}

pub struct App {
    pub window: gtk::ApplicationWindow,
    pub player: Player,
    pub lane_players: RefCell<Vec<LanePlayer>>,
    pub crop: CropOverlay,
    pub trim: TrimBar,
    pub timeline: Timeline,
    empty: gtk::Label,
    play_btn: gtk::Button,
    pub pos_entry: gtk::Entry,
    pub in_entry: gtk::Entry,
    pub out_entry: gtk::Entry,
    pub len_lbl: gtk::Label,
    total_lbl: gtk::Label,
    /// Per lane: pressed = heard in the preview.
    pub hear: Vec<gtk::ToggleButton>,
    hear_box: gtk::Box,
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
    add_sound_btn: gtk::Button,
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
        let add = gtk::Button::builder().label("Add video").tooltip_text("Add video files, or open a project (Ctrl+O). You can also drop files on the window.").build();
        header.pack_start(&add);
        let add_sound_btn = gtk::Button::builder().label("Add sound").tooltip_text("Put a file's sound in a lane under the clips, starting at the play position").build();
        header.pack_start(&add_sound_btn);
        let export_btn = gtk::Button::builder().label("Export").tooltip_text("Export (Ctrl+E)").build();
        export_btn.add_css_class("suggested-action");
        header.pack_end(&export_btn);
        let menu = gio::Menu::new();
        menu.append(Some("Open project…"), Some("win.open-project"));
        menu.append(Some("Save project"), Some("win.save"));
        menu.append(Some("Save project as…"), Some("win.save-as"));
        menu.append(Some("Reopen last session"), Some("win.restore"));
        header.pack_end(&gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Project").build());

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.set_child(Some(&root));

        let player = Player::new()?;
        let crop = CropOverlay::new();
        let empty = gtk::Label::new(Some("Drop video files here, or press Add video"));
        empty.add_css_class("title-2");
        let stage = gtk::Overlay::builder().vexpand(true).build();
        stage.set_child(Some(&player.area));
        stage.add_overlay(&crop.area);
        stage.add_overlay(&empty);
        root.append(&stage);

        let bar = gtk::Box::builder().spacing(6).margin_start(12).margin_end(12).margin_top(6).margin_bottom(2).build();
        root.append(&bar);
        let play_btn = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Play / pause (Space)").build();
        bar.append(&play_btn);
        let time_entry = |tip: &str| {
            let e = gtk::Entry::builder().width_chars(10).max_width_chars(10).tooltip_text(format!("{tip} Type a time and press Enter.")).build();
            e.add_css_class("numeric");
            e
        };
        let dim_label = |margin: i32| {
            let l = gtk::Label::builder().margin_start(margin).build();
            l.add_css_class("numeric");
            l.add_css_class("dim-label");
            l
        };
        let pos_entry = time_entry("Play position.");
        bar.append(&pos_entry);
        let total_lbl = dim_label(0);
        bar.append(&total_lbl);
        bar.append(&gtk::Label::builder().label("In").margin_start(14).build());
        let in_entry = time_entry("Where the clip starts (I sets it at the play position).");
        bar.append(&in_entry);
        bar.append(&gtk::Label::builder().label("Out").margin_start(6).build());
        let out_entry = time_entry("Where the clip ends (O sets it at the play position).");
        bar.append(&out_entry);
        let len_lbl = dim_label(4);
        bar.append(&len_lbl);
        bar.append(&gtk::Box::builder().hexpand(true).build());
        bar.append(&gtk::Label::new(Some("Sound")));
        let hear_box = gtk::Box::builder().css_classes(["linked"]).build();
        let hear: Vec<gtk::ToggleButton> = (1..=LANES)
            .map(|n| {
                let b = gtk::ToggleButton::builder()
                    .label(n.to_string())
                    .active(true)
                    .tooltip_text(format!("Hear sound lane {n} in the preview. This does not change the export."))
                    .build();
                hear_box.append(&b);
                b
            })
            .collect();
        bar.append(&hear_box);
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 200.0, 1.0);
        volume.set_size_request(80, -1);
        volume.set_draw_value(false);
        volume.add_mark(100.0, gtk::PositionType::Bottom, None);
        volume.set_tooltip_text(Some("Volume of the selected clip or sound"));
        bar.append(&volume);
        let volume_lbl = gtk::Label::builder().width_chars(5).xalign(0.0).build();
        volume_lbl.add_css_class("numeric");
        bar.append(&volume_lbl);
        bar.append(&gtk::Label::builder().label("Crop").margin_start(8).build());
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

        let trim = TrimBar::new();
        root.append(&trim.area);
        let timeline = Timeline::new(LANES);
        root.append(&timeline.area);

        let foot = gtk::Box::builder().spacing(10).margin_start(12).margin_end(12).margin_top(2).margin_bottom(8).build();
        root.append(&foot);
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
            play_btn,
            pos_entry,
            in_entry,
            out_entry,
            len_lbl,
            total_lbl,
            hear,
            hear_box,
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
            add_sound_btn,
            st: RefCell::new(St {
                s: Session::new(),
                pos: 0.0,
                last_pos: 0.0,
                clip_pos: 0.0,
                plan: None,
                gesture: false,
                dragging: false,
                detached: false,
                cancel: None,
                volume_edit: None,
                caps: Capabilities::default(),
                kf_cache: HashMap::new(),
                ticks: 0,
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
        let f = on!(a => a.choose_files("Add video files or open a project", false));
        add.connect_clicked(move |_| f());
        let f = on!(a => a.choose_files("Add sound from files", true));
        self.add_sound_btn.connect_clicked(move |_| f());
        let f = on!(a => a.export());
        self.export_btn.connect_clicked(move |_| f());
        let f = on!(a => a.toggle_play());
        self.play_btn.connect_clicked(move |_| f());
        let f = on!(a => a.cancel_export());
        self.cancel_btn.connect_clicked(move |_| f());

        for (name, which) in [("open-project", 0), ("save", 1), ("save-as", 2), ("restore", 3)] {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(a) = weak.upgrade() {
                    match which {
                        0 => a.choose_files("Open project", false),
                        1 => a.save(false),
                        2 => a.save(true),
                        _ => a.restore(),
                    }
                }
            });
            self.window.add_action(&action);
        }

        for (entry, which) in [(&self.pos_entry, "pos"), (&self.in_entry, "in"), (&self.out_entry, "out")] {
            let weak = Rc::downgrade(self);
            entry.connect_activate(move |e| {
                if let Some(a) = weak.upgrade() {
                    a.time_typed(&e.text(), which);
                }
            });
        }
        for b in &self.hear {
            let weak = Rc::downgrade(self);
            b.connect_toggled(move |_| {
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
        *self.timeline.on_select.borrow_mut() = Some(Box::new(move |(i, fraction)| {
            if let Some(a) = weak.upgrade() {
                a.on_clip_click(i, fraction);
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
                PlayerEvent::Paused(paused) => {
                    self.play_btn.set_icon_name(if paused { "media-playback-start-symbolic" } else { "media-playback-pause-symbolic" })
                }
            }
        }
        while let Ok(msg) = self.rx.try_recv() {
            self.message(msg);
        }
        let ticks = {
            let mut st = self.st.borrow_mut();
            st.ticks += 1;
            st.ticks
        };
        if ticks % 4 == 0 {
            self.sync_lanes();
        }
    }

    fn message(self: &Rc<Self>, msg: Msg) {
        match msg {
            Msg::Added { path, info, as_sound } => self.added(&path, *info, as_sound),
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

    /// The time entry being typed in, if any.
    fn editing(&self) -> Option<&gtk::Entry> {
        let focus = gtk::prelude::GtkWindowExt::focus(&self.window)?;
        [&self.pos_entry, &self.in_entry, &self.out_entry].into_iter().find(|e| focus == **e || focus.is_ancestor(*e))
    }

    /// Where the play position falls on the montage.
    pub fn montage_pos(&self) -> R {
        let st = self.st.borrow();
        match st.s.sound() {
            Some(sound) => sound.at + (floor_us(st.pos) - sound.start).max(time::zero()),
            None if st.s.clip().is_some() => st.s.montage_time(&floor_us(st.pos)),
            None => time::zero(),
        }
    }

    /// Redraw everything from the session and work out what an export would do.
    pub fn refresh(&self) {
        self.syncing.set(true);
        let editing = self.editing().cloned();
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
        for w in [self.play_btn.upcast_ref::<gtk::Widget>(), self.trim.area.upcast_ref(), self.timeline.area.upcast_ref(),
                  self.volume.upcast_ref(), self.hear_box.upcast_ref(), self.pos_entry.upcast_ref(), self.in_entry.upcast_ref(),
                  self.out_entry.upcast_ref()] {
            w.set_sensitive(target.is_some());
        }
        for w in [self.shapes_box.upcast_ref::<gtk::Widget>(), self.lock.upcast_ref(), self.exact.upcast_ref(), self.add_sound_btn.upcast_ref()] {
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
            if editing.as_ref() != Some(&self.in_entry) {
                self.in_entry.set_text(&clock(to_f64(&start)));
            }
            if editing.as_ref() != Some(&self.out_entry) {
                self.out_entry.set_text(&clock(to_f64(&end)));
            }
            self.len_lbl.set_text(&format!("{:.3} s", to_f64(&(end - start))));
            self.total_lbl.set_text(&format!("/ {}", clock(to_f64(&duration))));
            let pct = gain_to_percent(gain);
            self.volume.set_value(pct);
            self.volume_lbl.set_text(&format!("{pct:.0}%"));
            if editing.as_ref() != Some(&self.pos_entry) {
                self.pos_entry.set_text(&clock(st.pos));
            }
        } else {
            self.trim.update(None, 0.0, 0.0, 0.0, 0.0, Rc::new(Vec::new()), None);
            for e in [&self.pos_entry, &self.in_entry, &self.out_entry] {
                e.set_text("");
            }
            for l in [&self.len_lbl, &self.total_lbl, &self.volume_lbl] {
                l.set_text("");
            }
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

    fn show_time(&self) {
        let (pos, has) = {
            let st = self.st.borrow();
            (st.pos, st.s.target().is_some())
        };
        if has && self.editing() != Some(&self.pos_entry) {
            self.pos_entry.set_text(&clock(pos));
        }
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
            let crossed = self.player.playing() && st.last_pos < to_f64(&end) && to_f64(&end) <= t;
            st.last_pos = t;
            match (crossed, st.s.target()) {
                // Montage playback: on reaching this clip's out point, carry on into the next clip.
                (true, Some(Target::Clip(i))) if i + 1 < st.s.clips().len() => Some(Some(i + 1)),
                (true, _) => Some(None),
                _ => None,
            }
        };
        self.trim.set_pos(t, self.player.playing());
        self.timeline.set_pos(to_f64(&self.montage_pos()));
        self.show_time();
        match next {
            Some(Some(i)) => self.select(i, None, true),
            Some(None) => self.player.set_playing(false),
            None => {}
        }
    }

    /// Whether a sound lane is heard in the preview. The export always has every lane.
    pub fn hears(&self, lane: usize) -> bool {
        self.hear.get(lane).is_none_or(|b| b.is_active())
    }

    /// Keep the sound lanes playing in step with the montage.
    fn sync_lanes(&self) {
        let st = self.st.borrow();
        let active = st.s.clip().is_some() && st.s.sound().is_none() && self.player.playing() && !st.detached;
        let at = if active { Some(st.s.montage_time(&floor_us(st.pos))) } else { None };
        for (n, lane_player) in self.lane_players.borrow_mut().iter_mut().enumerate() {
            let hit = at.as_ref().filter(|_| self.hears(n)).and_then(|at| st.s.sound_at(n, at));
            match hit {
                Some((item, source_time)) => {
                    lane_player.sync(Some((&st.s.project.media[&item.media], to_f64(&source_time), st.s.lanes()[n].gain_db + item.gain_db)))
                }
                None => lane_player.sync(None),
            }
        }
    }

    // ------------------------------------------------------------ moving around

    fn load(&self, playing: bool) {
        let (path, pos) = {
            let mut st = self.st.borrow_mut();
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
        {
            let mut st = self.st.borrow_mut();
            if st.s.sound().is_none() {
                return;
            }
            st.s.clear_sound();
            st.pos = st.clip_pos;
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
        self.player.set_playing(false);
        self.player.seek(t, true);
    }

    pub fn seek(&self, t: f64, exact: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.detached = false;
            st.pos = t;
            st.last_pos = t;
        }
        self.player.seek(t, exact);
        self.trim.set_pos(t, false);
        self.timeline.set_pos(to_f64(&self.montage_pos()));
        self.show_time();
    }

    pub fn toggle_play(&self) {
        enum Do {
            Pause,
            Replay,
            RestartSound(f64),
            Play,
        }
        let action = {
            let st = self.st.borrow();
            let Some((_, start, end, _)) = st.s.target_span() else { return };
            let at_end = (st.pos - to_f64(&end)).abs() < to_f64(&st.s.frame()) * 1.5;
            let sound_mode = st.s.sound().is_some();
            if self.player.playing() {
                Do::Pause
            } else if at_end && !sound_mode && st.s.selected.map(|i| i + 1) == Some(st.s.clips().len()) {
                Do::Replay // at the very end: play the montage again
            } else if at_end && sound_mode {
                Do::RestartSound(to_f64(&start))
            } else {
                Do::Play
            }
        };
        match action {
            Do::Pause => self.player.set_playing(false),
            Do::Replay => self.select(0, None, true),
            Do::RestartSound(start) => {
                self.seek(start, true);
                self.player.set_playing(true);
            }
            Do::Play => {
                self.reattach();
                self.player.set_playing(true);
            }
        }
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
        self.st.borrow_mut().dragging = !last;
        self.player.set_playing(false);
        self.seek(t, last); // keyframes while dragging, the exact frame on release
    }

    /// The play line on the montage is dragged: move through the clips.
    fn on_montage_scrub(&self, t: f64, last: bool) {
        let (i, source, change) = {
            let mut st = self.st.borrow_mut();
            let Some((i, source)) = st.s.locate(&floor_us(t)) else { return };
            st.dragging = !last;
            (i, to_f64(&source), st.s.sound().is_some() || st.s.selected != Some(i))
        };
        self.player.set_playing(false);
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
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        let has = self.st.borrow().s.target().is_some();
        if let (Some(t), true) = (t, has) {
            if which == "pos" {
                let duration = self.st.borrow().s.target_info().map_or(time::zero(), |i| i.duration);
                self.player.set_playing(false);
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

    fn on_clip_click(&self, i: usize, fraction: f64) {
        let pos = {
            let st = self.st.borrow();
            let Some(c) = st.s.clips().get(i) else { return };
            to_f64(&c.start) + fraction * to_f64(&c.duration())
        };
        self.select(i, Some(pos), false);
    }

    fn on_clip_move(&self, src: usize, dst: usize, last: bool) {
        if last {
            self.st.borrow_mut().gesture = false;
            return;
        }
        self.begin_gesture();
        {
            let mut st = self.st.borrow_mut();
            st.s.clear_sound();
            st.s.move_clip(src, dst);
        }
        self.refresh();
    }

    fn on_sound_press(&self, lane: usize, i: usize) {
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
            st.pos = st.s.sound().map_or(0.0, |s| to_f64(&s.start));
        }
        self.load(false);
        self.refresh();
    }

    fn on_sound_move(&self, at: f64, lane: Option<usize>, last: bool) {
        if last {
            self.st.borrow_mut().gesture = false;
            return;
        }
        self.begin_gesture();
        self.st.borrow_mut().s.move_sound(floor_us(at), lane);
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
            if st.s.target().is_none() {
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
        if self.editing().is_some() {
            // typing a time: only Escape is ours
            if name == "Escape" {
                gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
                self.refresh();
                return true;
            }
            return false;
        }
        if ctrl {
            match low.as_str() {
                "z" => {
                    let changed = {
                        let mut st = self.st.borrow_mut();
                        if shift { st.s.redo() } else { st.s.undo() }
                    };
                    if changed {
                        self.show_target();
                    }
                }
                "o" => self.choose_files("Add video files or open a project", false),
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
            "space" => self.toggle_play(),
            "Left" | "Right" => {
                self.player.set_playing(false);
                self.reattach();
                if shift {
                    let duration = self.st.borrow().s.target_info().map_or(0.0, |i| to_f64(&i.duration));
                    self.seek((pos + if name == "Right" { 1.0 } else { -1.0 }).clamp(0.0, duration), true);
                } else {
                    self.player.step(name == "Right");
                }
            }
            "Delete" | "BackSpace" => {
                {
                    let mut st = self.st.borrow_mut();
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

    fn choose_files(self: &Rc<Self>, title: &str, as_sound: bool) {
        let weak = Rc::downgrade(self);
        gtk::FileDialog::builder().title(title).build().open_multiple(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let (Some(a), Ok(files)) = (weak.upgrade(), result) else { return }; // dismissed
            let paths: Vec<String> = (0..files.n_items())
                .filter_map(|i| files.item(i).and_downcast::<gio::File>()?.path())
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            if as_sound {
                a.add_files(paths, true);
            } else {
                a.open_paths(paths);
            }
        });
    }

    /// Projects are opened; video files become clips; sound-only files go to a sound lane.
    pub fn open_paths(&self, paths: Vec<String>) {
        let is_project = |p: &String| p.to_lowercase().ends_with(PROJECT_EXT) || p.to_lowercase().ends_with(".json");
        if let Some(project) = paths.iter().find(|p| is_project(p)) {
            self.open_project(project);
        }
        self.add_files(paths.into_iter().filter(|p| !is_project(p)).collect(), false);
    }

    pub fn add_files(&self, paths: Vec<String>, as_sound: bool) {
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
                let wants_index = quick.video.is_some() && !as_sound;
                let _ = tx.send(Msg::Added { path: path.clone(), info: Box::new(quick), as_sound });
                if wants_index {
                    // keyframe index; slower on big files, needed for lossless export
                    if let Ok(full) = probe(&path, true) {
                        let _ = tx.send(Msg::Indexed { path, info: Box::new(full) });
                    }
                }
            }
        });
    }

    fn added(&self, path: &str, info: MediaInfo, as_sound: bool) {
        if as_sound || info.video.is_none() {
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
                    st.s.add_sound(path, info, at).map(|_| st.pos = 0.0)
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
        self.player.set_playing(false);
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
