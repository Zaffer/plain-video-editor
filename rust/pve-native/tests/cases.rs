//! The shared test cases: tests/cases.json holds the inputs and tests/golden/ the
//! expected plans and ffmpeg commands. The Python core is held to the same files.

use std::collections::BTreeMap;
use std::path::PathBuf;

use pve_core::media::MediaInfo;
use pve_core::model::Project;
use pve_core::planner::{plan, Mode};
use pve_native::ffmpeg::{build, Capabilities, Encoder};
use serde_json::{json, Value};

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests")
}

fn read(path: PathBuf) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).unwrap()
}

fn run_case(case: &Value, infos: &BTreeMap<String, MediaInfo>) -> Value {
    let paths: BTreeMap<String, String> =
        [("clip1", "/media/clip1.mp4"), ("clip2", "/media/clip2.mp4")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
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
            build(&p, &paths, "/out/result.mp4", &caps, "/tmp/x", enc).map(|steps| (p, steps)).map_err(|e| e.to_string())
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
fn shared_cases_match_the_expected_plans_and_commands() {
    let dir = tests_dir();
    let infos: BTreeMap<String, MediaInfo> = ["clip1", "clip2"]
        .iter()
        .map(|n| (n.to_string(), MediaInfo::from_json(&read(dir.join(format!("fixtures/{n}.json")))).unwrap()))
        .collect();
    let cases = read(dir.join("cases.json"));
    let mut failures = Vec::new();
    for case in cases.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let got = run_case(case, &infos);
        let want = read(dir.join(format!("golden/{name}.json")));
        if got != want {
            failures.push(format!("--- {name}\n got: {got}\nwant: {want}"));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.as_array().unwrap().len(), failures.join("\n"));
    assert!(cases.as_array().unwrap().len() >= 28);
}

#[test]
fn media_descriptions_round_trip() {
    for n in ["clip1", "clip2"] {
        let v = read(tests_dir().join(format!("fixtures/{n}.json")));
        assert_eq!(MediaInfo::from_json(&v).unwrap().to_json(), v, "{n}");
    }
}
