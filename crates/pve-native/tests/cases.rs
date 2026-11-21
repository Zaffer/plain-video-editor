//! The plan cases. `testdata/cases.json` holds projects with the settings to
//! export them under, `testdata/media/` describes the two files they use, and
//! `testdata/golden/` holds the expected plan and ffmpeg commands for each case.
//! No media files and no ffmpeg are needed.
//!
//! After an intended change, rewrite the expected files and read the diff:
//!
//!     PVE_UPDATE_GOLDEN=1 cargo test -p pve-native --test cases

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use pve_core::media::MediaInfo;
use pve_core::model::Project;
use pve_core::planner::{plan, Mode};
use pve_native::ffmpeg::{build, uses_gpu_encoder, Capabilities, Encoder};
use serde_json::{json, Value};

fn testdata() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata")
}

fn read(path: PathBuf) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).unwrap()
}

fn infos() -> BTreeMap<String, MediaInfo> {
    ["clip1", "clip2"].iter().map(|n| (n.to_string(), MediaInfo::from_json(&read(testdata().join(format!("media/{n}.json")))).unwrap())).collect()
}

fn paths() -> BTreeMap<String, String> {
    [("clip1", "/media/clip1.mp4"), ("clip2", "/media/clip2.mp4")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn run_case(case: &Value, infos: &BTreeMap<String, MediaInfo>) -> Value {
    let c = &case["caps"];
    let caps = Capabilities {
        filters: c["filters"].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_owned()).collect(),
        nvenc: c["nvenc"].as_bool().unwrap(),
        hw_decode: c["hw_decode"].as_str().map(str::to_owned),
    };
    let result = Project::from_json(&case["project"])
        .and_then(|p| plan(&p, infos, Mode::parse(case["mode"].as_str().unwrap()).unwrap(), case["draft"].as_bool().unwrap()))
        .and_then(|p| {
            let enc = Encoder::parse(case["encoder"].as_str().unwrap()).unwrap();
            build(&p, &paths(), "/out/result.mp4", &caps, "/tmp/x", enc).map(|steps| (p, steps)).map_err(|e| e.to_string())
        });
    match result {
        Err(_) => json!({ "error": true }),
        Ok((p, steps)) => json!({
            "plan": p.to_json(),
            "commands": steps.iter().map(|s| s.argv.clone()).collect::<Vec<_>>(),
            "lists": steps.iter().filter_map(|s| s.list_file.as_ref().map(|l| l.1.clone())).collect::<Vec<_>>(),
        }),
    }
}

#[test]
fn cases_match_the_expected_plans_and_commands() {
    let dir = testdata();
    let infos = infos();
    let cases = read(dir.join("cases.json"));
    let cases = cases.as_array().unwrap();
    let update = std::env::var_os("PVE_UPDATE_GOLDEN").is_some();
    let mut names = BTreeSet::new();
    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert!(names.insert(name), "two cases are called {name}");
        let got = run_case(case, &infos);
        if got.get("error").is_some() != case["error"].as_bool().unwrap() {
            failures.push(format!("--- {name}: {}", if got.get("error").is_some() { "failed, but should work" } else { "worked, but should fail" }));
            continue;
        }
        let path = dir.join(format!("golden/{name}.json"));
        if update {
            std::fs::write(&path, serde_json::to_string_pretty(&got).unwrap() + "\n").unwrap();
        }
        let want = read(path);
        if got != want {
            failures.push(format!("--- {name}\n got: {got}\nwant: {want}"));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
    // every expected file belongs to a case
    for entry in std::fs::read_dir(dir.join("golden")).unwrap() {
        let file = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(names.contains(file.trim_end_matches(".json")), "testdata/golden/{file} has no case");
    }
}

#[test]
fn media_descriptions_round_trip() {
    for n in ["clip1", "clip2"] {
        let v = read(testdata().join(format!("media/{n}.json")));
        assert_eq!(MediaInfo::from_json(&v).unwrap().to_json(), v, "{n}");
    }
}

#[test]
fn the_gpu_decodes_always_and_encodes_only_above_1080p() {
    let all = read(testdata().join("cases.json"));
    let case = |name: &str| all.as_array().unwrap().iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("no case {name}"));
    let plan_of = |name: &str| plan(&Project::from_json(&case(name)["project"]).unwrap(), &infos(), Mode::Auto, false).unwrap();
    let gpu = Capabilities { filters: ["zscale", "libplacebo", "tonemap"].map(str::to_owned).into(), nvenc: true, hw_decode: Some("cuda".into()) };
    let none = Capabilities::default();

    // a 1080x810 output: the software encoder, with both picture inputs (not the sound inputs) decoded on the GPU
    let small = plan_of("montage");
    let argv = &build(&small, &paths(), "/out/r.mp4", &gpu, "/tmp/x", Encoder::Auto).unwrap()[0].argv;
    assert!(argv.iter().any(|a| a == "libx264"));
    assert_eq!(argv.iter().filter(|a| *a == "-hwaccel").count(), 2);
    assert_eq!(uses_gpu_encoder(&small, &gpu, Encoder::Auto).ok(), Some(false));

    let big = plan_of("montage_4k_auto_gpu");
    assert_eq!(uses_gpu_encoder(&big, &gpu, Encoder::Auto).ok(), Some(true));
    assert_eq!(uses_gpu_encoder(&big, &gpu, Encoder::X264).ok(), Some(false));
    assert_eq!(uses_gpu_encoder(&big, &none, Encoder::Auto).ok(), Some(false));
    assert!(uses_gpu_encoder(&big, &none, Encoder::Nvenc).is_err());
}
