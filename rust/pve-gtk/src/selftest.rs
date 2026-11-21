//! A scripted run of the real window, for testing:  PVE_SELFTEST=/tmp/prefix pve-gui clip1.mp4 clip2.mp4
//!
//! It expects two clips: the first without sound, the second with. Gestures are
//! simulated by calling the widgets' own press/drag/release code with pixel
//! coordinates, so hit-testing and geometry are exercised; real pointer events
//! are not. Writes PREFIX.pve (a project) and PREFIX.mp4 (an export).

use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};
use pve_core::model::Crop;
use pve_core::planner::Kind;
use pve_core::session::Shape;
use pve_core::time::{int, r, to_f64};

use crate::app::App;
use crate::geom::{TimelineGeom, TrimGeom, TrimHit};

pub static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

async fn sleep(ms: u64) {
    glib::timeout_future(Duration::from_millis(ms)).await;
}

struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Debug) {
        if ok {
            self.passed += 1;
            println!("ok   {name}");
        } else {
            self.failed += 1;
            println!("FAIL {name}  {detail:?}");
        }
    }
}

const NONE: gdk::ModifierType = gdk::ModifierType::empty();
const CTRL: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK;

fn click_clip(a: &App, i: usize, dx: f64) {
    let g = a.timeline.geom();
    let (x, _) = g.spans()[i];
    a.timeline.press(x + dx, TimelineGeom::CLIP_Y + 5.0);
    a.timeline.release();
}

pub fn start(a: Rc<App>, out: String, files: Vec<String>) {
    glib::MainContext::default().spawn_local(async move {
        let mut rep = Report { passed: 0, failed: 0 };
        run(&a, &out, &files, &mut rep).await;
        println!("{} passed, {} failed", rep.passed, rep.failed);
        EXIT_CODE.store(if rep.failed == 0 { 0 } else { 1 }, Ordering::Relaxed);
        a.window.close();
    });
}

async fn run(a: &Rc<App>, out: &str, files: &[String], rep: &mut Report) {
    let n_clips = |a: &App| a.st.borrow().s.clips().len();
    let t0 = Instant::now();
    while n_clips(a) < files.len() && t0.elapsed() < Duration::from_secs(20) {
        sleep(100).await;
    }
    rep.check("clips added", n_clips(a) == files.len(), n_clips(a));
    if n_clips(a) < 2 {
        return;
    }
    while a.st.borrow().s.infos.values().any(|i| i.video.is_some() && i.keyframes.is_none()) && t0.elapsed() < Duration::from_secs(40) {
        sleep(100).await;
    }
    rep.check("keyframes indexed", a.st.borrow().s.infos.values().all(|i| i.keyframes.is_some()), "");
    sleep(800).await;
    println!("window: active {} size {}x{}", a.window.is_active(), a.window.width(), a.window.height());
    let (drawn, pixel) = a.player.drawn();
    println!("preview: {drawn} frames drawn so far, centre pixel {pixel:?}");
    if a.window.is_active() {
        rep.check("the preview draws a picture on screen", drawn > 0 && pixel.is_some_and(|p| p[..3] != [0, 0, 0]), (drawn, pixel));
    }
    // The same drawing path into a buffer of our own, which works even when the window is hidden.
    let mut offscreen = None;
    for _ in 0..30 {
        offscreen = a.player.sample_offscreen();
        if offscreen.is_some_and(|p| p[..3] != [0, 0, 0]) {
            break;
        }
        sleep(100).await; // mpv hands over a frame only after the previous one has been drawn
    }
    rep.check("mpv draws the clip's picture through OpenGL", offscreen.is_some_and(|p| p[..3] != [0, 0, 0]), (offscreen, a.player.ready()));

    // --- timeline: click clip 1
    click_clip(a, 0, 40.0);
    sleep(500).await;
    rep.check("click selects clip 1", a.st.borrow().s.selected == Some(0) && a.st.borrow().s.sound().is_none(), a.st.borrow().s.selected);
    let dur = to_f64(&a.st.borrow().s.info(0).duration);

    // --- trim bar: a press in the middle band away from every line does nothing
    let g = a.trim.geom();
    let (mid, low) = (TrimGeom::TOP + (g.height - TrimGeom::TOP - TrimGeom::BOTTOM) / 2.0, g.height - 8.0);
    let x_free = g.x(dur * 0.8);
    let before = { let st = a.st.borrow(); (st.pos, st.s.clips()[0].start, st.s.clips()[0].end, st.s.undo_depth()) };
    rep.check("middle band away from lines grabs nothing", g.hit(x_free, mid).is_none(), g.hit(x_free, mid));
    a.trim.simulate_drag(x_free, mid, x_free + 60.0);
    sleep(100).await;
    let after = { let st = a.st.borrow(); (st.pos, st.s.clips()[0].start, st.s.clips()[0].end, st.s.undo_depth()) };
    rep.check("stray press changes nothing", before == after, (&before, &after));
    let cursors = (g.cursor(g.x(g.start) - 5.0, low), g.cursor(g.x(g.pos), 6.0), g.cursor(x_free, mid), g.cursor(x_free, 6.0));
    rep.check("cursor changes over knobs and lines", cursors == (Some("ew-resize"), Some("grab"), None, Some("pointer")), cursors);

    // --- drag the in knob (below the bar): the play position must not move
    let pos_before = a.st.borrow().pos;
    rep.check("in knob is grabbable below the bar", g.hit(g.x(g.start) - 5.0, low) == Some(TrimHit::In), "");
    a.trim.simulate_drag(g.x(g.start) - 5.0, low, g.x(5.0));
    sleep(500).await;
    let start = to_f64(&a.st.borrow().s.clips()[0].start);
    rep.check("in knob drag trims", (start - 5.0).abs() < 0.05, start);
    rep.check("trimming leaves the play position alone", a.st.borrow().pos == pos_before && a.st.borrow().detached, a.st.borrow().pos);
    let g = a.trim.geom();
    rep.check("out knob is grabbable below the bar", g.hit(g.x(g.end) + 5.0, low) == Some(TrimHit::Out), "");

    // --- typed timestamps are exact
    a.time_typed("0:05.000", "in");
    a.time_typed("15", "out");
    sleep(300).await;
    let span = { let st = a.st.borrow(); (st.s.clips()[0].start, st.s.clips()[0].end) };
    rep.check("typed in/out are exact", span == (int(5), int(15)), span);
    let shown = (a.in_entry.text().to_string(), a.out_entry.text().to_string(), a.len_lbl.text().to_string());
    rep.check("entries show the times", shown == ("0:05.000".to_string(), "0:15.000".to_string(), "10.000 s".to_string()), &shown);
    a.time_typed("nonsense", "out");
    rep.check("a bad time is ignored", a.st.borrow().s.clips()[0].end == int(15) && a.out_entry.text() == "0:15.000", a.out_entry.text());

    // --- the top band moves the play position
    let g = a.trim.geom();
    a.trim.simulate_drag(g.x(8.0), 5.0, g.x(8.0));
    sleep(700).await;
    rep.check("top band moves the play position", (a.st.borrow().pos - 8.0).abs() < 0.06 && !a.st.borrow().detached, a.st.borrow().pos);
    a.time_typed("9.5", "pos");
    sleep(700).await;
    rep.check("typed play position", (a.st.borrow().pos - 9.5).abs() < 0.04, a.st.borrow().pos);

    // --- zoom
    a.trim.set_pointer(a.trim.geom().x(9.5), mid);
    for _ in 0..8 {
        a.trim.scroll(-1.0, false);
    }
    let v = a.trim.geom().view;
    rep.check("scroll zooms around the pointer", v.1 - v.0 < dur / 4.0 && v.0 <= 9.5 && 9.5 <= v.1, v);
    let g = a.trim.geom();
    a.trim.simulate_drag(g.x(10.0), 5.0, g.x(10.0));
    sleep(600).await;
    rep.check("zoomed bar still maps positions", (a.st.borrow().pos - 10.0).abs() < 0.02, a.st.borrow().pos);
    for _ in 0..12 {
        a.trim.scroll(1.0, false);
    }
    rep.check("zooms back out fully", a.trim.geom().view == (0.0, dur), a.trim.geom().view);

    // --- clip 2
    click_clip(a, 1, 5.0);
    sleep(500).await;
    a.time_typed("7", "in");
    a.time_typed("17", "out");
    sleep(300).await;
    let ok = { let st = a.st.borrow(); st.s.selected == Some(1) && (st.s.clips()[1].start, st.s.clips()[1].end) == (int(7), int(17)) };
    rep.check("clip 2 trimmed by typing", ok, "");

    // --- crop: one shape for all, then drag with the overlay's own code
    let four_three = Shape::Ratio(r(4, 3));
    a.shape_btns.iter().find(|(s, _)| *s == four_three).unwrap().1.set_active(true);
    sleep(300).await;
    rep.check("4:3 applied to all", a.st.borrow().s.clips().iter().all(|c| c.crop_aspect == Some(r(4, 3))), "");
    let rect = a.crop.rect().expect("a crop rectangle");
    let cg = a.crop.geom();
    let (ox, oy, sc) = cg.area();
    let (cx, cy) = (ox + (rect.x as f64 + rect.w as f64 / 2.0) * sc, oy + (rect.y as f64 + rect.h as f64 / 2.0) * sc);
    a.crop.press(cx, cy);
    a.crop.drag(0.0, -40.0, false);
    a.crop.drag(0.0, -40.0, true);
    sleep(200).await;
    let moved = a.crop.rect().unwrap();
    rep.check("crop drag moves the rectangle", moved.y < rect.y && (moved.w, moved.h) == (rect.w, rect.h), moved);
    a.crop.press(ox + (moved.x + moved.w) as f64 * sc, oy + (moved.y + moved.h) as f64 * sc);
    a.crop.drag(-60.0, -60.0, true);
    sleep(200).await;
    let resized = a.crop.rect().unwrap();
    rep.check("corner drag keeps the shape", resized.w < moved.w && (resized.w as f64 / resized.h as f64 - 4.0 / 3.0).abs() < 0.01, resized);
    rep.check("plan is an encode", a.st.borrow().plan.as_ref().is_some_and(|p| p.kind == Kind::Encode), a.status.text());

    // --- split and undo
    a.seek(10.0, true);
    sleep(500).await;
    let handled = a.key(gdk::Key::s, NONE);
    let ok = { let st = a.st.borrow(); handled && st.s.clips().len() == 3 && st.s.clips()[1].end == int(10) };
    rep.check("split", ok, "");
    a.key(gdk::Key::z, CTRL);
    sleep(300).await;
    let ok = { let st = a.st.borrow(); st.s.clips().len() == 2 && st.s.clips()[1].end == int(17) };
    rep.check("undo split", ok, "");

    // --- sound lane: clip 2's sound under clip 1
    click_clip(a, 0, 1.0);
    sleep(400).await;
    a.seek(5.0, true);
    sleep(300).await;
    a.add_files(vec![files[1].clone()], true);
    let t1 = Instant::now();
    while a.st.borrow().s.sound().is_none() && t1.elapsed() < Duration::from_secs(8) {
        sleep(100).await;
    }
    let ok = { let st = a.st.borrow(); st.s.sound().is_some_and(|s| s.at == int(0)) && st.s.sel_audio == Some((0, 0)) };
    rep.check("sound added at the play position", ok, a.st.borrow().s.sel_audio);
    a.time_typed("7", "in");
    a.time_typed("17", "out");
    sleep(300).await;
    rep.check("sound item trimmed by typing", a.st.borrow().s.sound().is_some_and(|s| (s.start, s.end) == (int(7), int(17))), "");
    rep.check("sound mode disables clip-only controls", !a.sound.is_sensitive() && a.crop.rect().is_none(), "");
    a.key(gdk::Key::Escape, NONE);
    sleep(400).await;
    rep.check("Escape returns to the clip", a.st.borrow().s.sound().is_none() && a.st.borrow().s.selected == Some(0), "");
    let mut at: Vec<f64> = a.st.borrow().plan.as_ref().map(|p| p.audio.iter().map(|s| to_f64(&s.at)).collect()).unwrap_or_default();
    at.sort_by(f64::total_cmp);
    rep.check("plan mixes the lane and clip 2's own sound", at == [0.0, 10.0], &at);

    // --- drag the sound item along the montage and to the other lane
    let tg = a.timeline.geom();
    let lane_y = TimelineGeom::lane_y(0) + 5.0;
    rep.check("sound item hit-testing", tg.sound_hit(tg.x(1.0), lane_y) == Some((0, 0)), tg.sound_hit(tg.x(1.0), lane_y));
    a.timeline.press(tg.x(1.0), lane_y);
    a.timeline.drag(60.0, 0.0);
    a.timeline.release();
    sleep(300).await;
    let moved_at = to_f64(&a.st.borrow().s.lanes()[0].items[0].at);
    rep.check("sound drag moves it in time", moved_at > 0.5 && a.st.borrow().s.sel_audio == Some((0, 0)), moved_at);
    let tg = a.timeline.geom();
    a.timeline.press(tg.x(moved_at + 1.0), lane_y);
    a.timeline.drag(0.0, TimelineGeom::LANE_H + TimelineGeom::LANE_GAP);
    a.timeline.release();
    sleep(300).await;
    rep.check("sound drag changes lane", a.st.borrow().s.sel_audio == Some((1, 0)) && a.st.borrow().s.lanes()[0].items.is_empty(), a.st.borrow().s.sel_audio);
    a.key(gdk::Key::z, CTRL);
    a.key(gdk::Key::z, CTRL);
    sleep(400).await;
    let ok = { let st = a.st.borrow(); st.s.lanes()[0].items.len() == 1 && st.s.lanes()[0].items[0].at == int(0) };
    rep.check("undo puts the sound back", ok, "");
    a.key(gdk::Key::Escape, NONE);
    sleep(300).await;

    // --- volume
    a.volume.set_value(50.0);
    sleep(200).await;
    let gain = a.st.borrow().s.clip().map(|c| c.gain_db);
    rep.check("volume sets the clip's gain", gain.is_some_and(|g| (g + 6.02).abs() < 0.01) && a.volume_lbl.text() == "50%", gain);
    a.key(gdk::Key::z, CTRL);
    sleep(200).await;
    rep.check("undo volume", a.st.borrow().s.clip().map(|c| c.gain_db) == Some(0.0), "");

    // --- project: save, change, reopen
    let project = format!("{out}.pve");
    a.save_to(&project);
    sleep(200).await;
    rep.check("saved, not dirty", std::path::Path::new(&project).exists() && !a.st.borrow().s.dirty(), a.status.text());
    let stored: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&project).unwrap_or_default()).unwrap_or_default();
    let relative = stored["media"].as_object().is_some_and(|m| m.values().all(|v| !v["path"].as_str().unwrap_or("/").starts_with('/')));
    rep.check("project stores relative media paths", relative, &stored["media"]);
    a.key(gdk::Key::Delete, NONE);
    sleep(400).await;
    let title = a.window.title().map(|t| t.to_string()).unwrap_or_default();
    rep.check("an edit makes it dirty", a.st.borrow().s.dirty() && n_clips(a) == 1 && title.starts_with('•'), &title);
    a.open_project(&project);
    let t1 = Instant::now();
    while (n_clips(a) != 2 || a.st.borrow().s.dirty()) && t1.elapsed() < Duration::from_secs(10) {
        sleep(100).await;
    }
    sleep(400).await;
    let ok = { let st = a.st.borrow(); st.s.clips().len() == 2 && st.s.lanes()[0].items.len() == 1 && st.s.clips()[0].crop_aspect == Some(r(4, 3)) && !st.s.dirty()
                && matches!(st.s.clips()[1].crop, Some(Crop::Rect(_))) };
    rep.check("reopened project", ok, "");

    // --- playback: carries on into clip 2, with the lane playing in step
    a.seek(14.5, true);
    sleep(500).await;
    a.toggle_play();
    sleep(400).await;
    let (lane_path, lane_now, want) = {
        let players = a.lane_players.borrow();
        (players[0].path.clone(), players[0].position(), 7.0 + (a.st.borrow().pos - 5.0))
    };
    rep.check("lane sound plays in step with the clip", lane_path.as_deref() == Some(files[1].as_str()) && lane_now.is_some_and(|n| (n - want).abs() < 0.3),
              (&lane_path, lane_now, want));
    rep.check("second lane's player was never started", !a.lane_players.borrow()[1].started(), "");
    let t1 = Instant::now();
    while a.st.borrow().s.selected == Some(0) && t1.elapsed() < Duration::from_secs(6) {
        sleep(50).await;
    }
    sleep(800).await;
    let state = (a.st.borrow().s.selected, a.st.borrow().pos);
    rep.check("playback carried on into clip 2", state.0 == Some(1) && state.1 >= 7.0, state);
    a.player.set_playing(false);
    sleep(300).await;
    rep.check("lane pauses with the clip", a.lane_players.borrow()[0].is_paused(), "");

    // --- export through the window
    let export = format!("{out}.mp4");
    let _ = std::fs::remove_file(&export);
    a.export_to(&export);
    let t1 = Instant::now();
    while a.st.borrow().cancel.is_some() && t1.elapsed() < Duration::from_secs(120) {
        sleep(100).await;
    }
    rep.check("export finished", a.status.text().starts_with("Saved"), a.status.text());
    let probe = Command::new("ffprobe").args(["-v", "error", "-show_entries", "stream=codec_type,nb_frames,duration", "-of", "csv=p=0", &export]).output();
    let text = probe.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    rep.check("export has picture and sound", text.contains("video,20.000000,600") && text.contains("audio"), &text);
}

/// Timings with one (large) file, for comparing builds:  PVE_BENCH=1 pve-gui big.mp4
pub fn bench(a: Rc<App>) {
    glib::MainContext::default().spawn_local(async move {
        let t0 = Instant::now();
        let secs = |t: Instant| format!("{:.2}", t.elapsed().as_secs_f64());
        while a.st.borrow().s.clips().is_empty() {
            sleep(5).await;
        }
        println!("clip appears after s: {}", secs(t0));
        while a.st.borrow().s.infos.values().any(|i| i.keyframes.is_none()) {
            sleep(5).await;
        }
        let keyframes = a.st.borrow().s.infos.values().next().and_then(|i| i.keyframes.as_ref().map(Vec::len));
        println!("keyframes indexed after s: {} ({:?})", secs(t0), keyframes);
        while !a.player.ready() && t0.elapsed() < Duration::from_secs(15) {
            sleep(5).await;
        }
        println!("first frame ready after s: {}", secs(t0));
        sleep(400).await;
        a.time_typed("1:02:03.5", "in");
        a.time_typed("1:04:10", "out");
        sleep(300).await;
        println!("status: {}", a.status.text());
        let t = Instant::now();
        for _ in 0..300 {
            a.refresh();
        }
        println!("refresh ms: {:.3}", t.elapsed().as_secs_f64() / 300.0 * 1000.0);
        let each = |label: &str, f: &dyn Fn()| {
            let t = Instant::now();
            for _ in 0..300 {
                f();
            }
            println!("  {label}: {:.3} ms", t.elapsed().as_secs_f64() / 300.0 * 1000.0);
        };
        each("player.set_sound", &|| a.player.set_sound(true, 0.0));
        each("status.set_text", &|| a.status.set_text("Lossless copy · 2:08.128 · cuts move out to keyframes, +1.6 s"));
        each("window.set_title", &|| a.window.set_title(Some("Plain Video Editor")));
        each("in_entry.set_text", &|| a.in_entry.set_text("1:02:03.500"));
        each("volume.set_value", &|| a.volume.set_value(100.0));
        each("montage_pos", &|| { a.montage_pos(); });
        each("plan", &|| { let _ = a.st.borrow().s.plan(); });
        let t = Instant::now();
        for i in 0..200 {
            let g = a.trim.geom(); // the knob moves with each drag
            a.trim.simulate_drag(g.x(g.end) + 4.0, g.height - 8.0, g.x(g.end) + (i % 7) as f64 - 3.0);
        }
        println!("trim drag ms per step: {:.3}", t.elapsed().as_secs_f64() / 600.0 * 1000.0);
        println!("undo steps from 200 drags: {}", a.st.borrow().s.undo_depth());
        sleep(3000).await; // settle, so memory and idle CPU can be read from outside
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for key in ["VmRSS", "VmHWM"] {
            if let Some(line) = status.lines().find(|l| l.starts_with(key)) {
                println!("{}", line.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
        a.window.close();
    });
}
