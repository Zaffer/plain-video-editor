//! pve-gui: the plain video editor window.
//!
//!   pve-gui [FILE...]     video files, sound files, or a project (.pve)

mod app;
mod geom;
mod mpv;
mod player;
mod selftest;
mod widgets;

use gtk::prelude::*;
use gtk::{gio, glib};

fn main() -> glib::ExitCode {
    let files: Vec<String> = std::env::args().skip(1).collect();
    let selftest = std::env::var("PVE_SELFTEST").ok();
    let app = gtk::Application::builder().application_id("app.plainvideo.Editor").flags(gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_activate(move |app| match app::App::new(app, files.clone()) {
        Ok(editor) => {
            editor.window.present();
            if std::env::var_os("PVE_BENCH").is_some() {
                selftest::bench(editor.clone());
            }
            if let Some(out) = &selftest {
                let absolute = |f: &String| std::path::absolute(f).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| f.clone());
                selftest::start(editor.clone(), out.clone(), files.iter().map(absolute).collect());
            }
            // The window owns the editor: keep it alive until the window goes away.
            let keep = std::cell::RefCell::new(Some(editor.clone()));
            editor.window.connect_destroy(move |_| {
                keep.borrow_mut().take();
            });
        }
        Err(e) => {
            eprintln!("error: {e}");
            app.quit();
        }
    });
    let code = app.run_with_args::<&str>(&[]);
    match selftest::EXIT_CODE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => code,
        _ => glib::ExitCode::FAILURE,
    }
}
