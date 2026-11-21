//! Times the core's hot paths on a real file:  cargo run --release --example planbench -- big.mp4

use std::time::Instant;

use pve_core::session::Session;
use pve_core::time::int;
use pve_native::probe::probe;

fn time<T>(label: &str, n: u32, mut f: impl FnMut() -> T) {
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(f());
    }
    println!("{label:28} {:9.2} us", t.elapsed().as_secs_f64() / n as f64 * 1e6);
}

fn main() {
    let path = std::env::args().nth(1).expect("a media file");
    let path = std::path::absolute(&path).unwrap().to_string_lossy().into_owned();
    let t = Instant::now();
    let info = probe(&path, true).expect("probe");
    println!("probe with keyframes: {:.2} s, {} keyframes", t.elapsed().as_secs_f64(), info.keyframes.as_ref().map_or(0, Vec::len));
    let mut s = Session::new();
    s.add_media(&path, info).unwrap();
    let end = s.clips()[0].end;
    s.set_in(end / int(3));
    s.set_out(end / int(2));
    time("plan (lossless)", 2000, || s.plan().unwrap());
    s.exact = true;
    time("plan (re-encode)", 2000, || s.plan().unwrap());
    s.exact = false;
    time("dirty check", 2000, || s.dirty());
    time("project to JSON", 2000, || s.project.to_json());
    time("validate", 2000, || s.project.validate());
    time("montage_time", 2000, || s.montage_time(&int(100)));
    time("checkpoint (undo snapshot)", 200, || s.checkpoint());
}
