"""Core tests: project JSON + media descriptions in, plan and ffmpeg commands out.

No media files or ffmpeg needed: media is described by the JSON fixtures.
The golden files are the contract a port of the core (e.g. to Rust) must meet.
Regenerate them after an intended change with:  PVE_UPDATE_GOLDEN=1 python3 -m unittest
"""
import json
import os
import unittest
from fractions import Fraction
from pathlib import Path

from pve import ffmpeg_backend
from pve.media import MediaInfo
from pve.model import Project, ProjectError, Rect
from pve.planner import PlanError, plan
from pve.session import Session, centered_rect

HERE = Path(__file__).parent
INFOS = {n: MediaInfo.from_dict(json.loads((HERE / "fixtures" / f"{n}.json").read_text())) for n in ("clip1", "clip2")}
PATHS = {"clip1": "/media/clip1.mp4", "clip2": "/media/clip2.mp4"}
FILTERS = {"zscale", "libplacebo", "tonemap"}
Caps = ffmpeg_backend.Capabilities


def project(clips, lanes=(), **extra):
    return Project.from_dict(
        {"version": 1, "media": {"clip1": {"path": "clip1.mp4"}, "clip2": {"path": "clip2.mp4"}},
         "clips": clips, "audio_lanes": list(lanes), **extra}
    )


MONTAGE = dict(
    clips=[
        {"media": "clip1", "in": 5, "out": 15, "crop": {"aspect": "4:3"}},
        {"media": "clip2", "in": 7, "out": 17, "crop": {"aspect": "4:3"}},
    ],
    lanes=[{"items": [{"media": "clip2", "in": 7, "out": 17, "at": 0}]}],
)


def run_case(case):
    """One shared test case -> what the core produces. Both the Python and Rust cores must agree on this."""
    c = case["caps"]
    caps = Caps(filters=frozenset(c["filters"]), nvenc=c["nvenc"], hw_decode=c["hw_decode"])
    try:
        p = plan(Project.from_dict(case["project"]), INFOS, mode=case["mode"], draft=case["draft"])
        steps = ffmpeg_backend.build(p, PATHS, "/out/result.mp4", caps, "/tmp/x", case["encoder"])
    except (PlanError, ProjectError, ffmpeg_backend.ExportError):
        return {"error": True}
    return {"plan": p.to_dict(), "commands": [s.argv for s in steps],
            "lists": [s.list_file[1] for s in steps if s.list_file]}


class Cases(unittest.TestCase):
    """tests/cases.json holds the inputs, tests/golden/ the expected plans and ffmpeg commands."""

    def test_cases(self):
        cases = json.loads((HERE / "cases.json").read_text())
        self.assertEqual(len({c["name"] for c in cases}), len(cases))
        for case in cases:
            with self.subTest(case["name"]):
                got = run_case(case)
                self.assertEqual("error" in got, case["error"], "expected failure" if case["error"] else "unexpected failure")
                path = HERE / "golden" / f"{case['name']}.json"
                if os.environ.get("PVE_UPDATE_GOLDEN"):
                    path.write_text(json.dumps(got, indent=2) + "\n")
                self.assertEqual(got, json.loads(path.read_text()))


class Golden(unittest.TestCase):
    def check(self, name, proj, **kw):
        return plan(proj, INFOS, **{k: v for k, v in kw.items() if k in ("mode", "draft")})

    def test_montage(self):
        p = self.check("montage", project(**MONTAGE))
        self.assertEqual((p.kind, p.duration), ("encode", 20))
        # clip2's 4:3 crop (1080x810 on screen) is larger than clip1 (640x480), so it sets the size
        self.assertEqual((p.video.width, p.video.height, p.video.color), (1080, 810, "sdr"))
        # clip 2's sound under clip 1 (lane) and again under clip 2 (its own)
        self.assertEqual(sorted(a.at for a in p.audio), [0, 10])

    def test_gpu_paths(self):
        gpu = Caps(filters=frozenset(FILTERS), nvenc=True, hw_decode="cuda")
        # 1080x810 output: auto keeps x264 but decodes on the GPU
        p = plan(project(**MONTAGE), INFOS)
        argv = ffmpeg_backend.build(p, PATHS, "/out/r.mp4", gpu)[0].argv
        self.assertIn("libx264", argv)
        self.assertEqual(argv.count("-hwaccel"), 2)  # the two picture inputs, not the sound inputs
        self.assertFalse(ffmpeg_backend.uses_gpu_encoder(p, gpu))
        big = plan(project(**MONTAGE, output={"width": 3840, "height": 2160}), INFOS)
        self.assertTrue(ffmpeg_backend.uses_gpu_encoder(big, gpu))
        self.assertFalse(ffmpeg_backend.uses_gpu_encoder(big, gpu, "x264"))
        self.assertFalse(ffmpeg_backend.uses_gpu_encoder(big, Caps()))
        with self.assertRaises(ffmpeg_backend.ExportError):
            ffmpeg_backend.uses_gpu_encoder(big, Caps(), "nvenc")

    def test_montage_draft(self):
        self.assertTrue(self.check("montage_draft", project(**MONTAGE), draft=True).draft)

    def test_trim_is_lossless_and_snaps_outwards(self):
        p = self.check("trim_copy", project([{"media": "clip1", "in": 5, "out": 15}]))
        s = p.segments[0]
        self.assertEqual(p.kind, "copy")
        self.assertLessEqual(s.start, 5)
        self.assertGreaterEqual(s.end, 15)
        self.assertIn(s.start, INFOS["clip1"].keyframes)
        self.assertIn(s.end, INFOS["clip1"].keyframes)
        self.assertLess(s.end_decode, s.end)  # clip1 has reordered frames

    def test_trim_to_end_of_file_has_no_stop_point(self):
        p = plan(project([{"media": "clip2", "in": 25, "out": 31}]), INFOS)
        self.assertEqual((p.segments[0].start, p.segments[0].end_decode), (20, None))
        self.assertEqual(p.segments[0].end, INFOS["clip2"].duration)

    def test_join_same_source_is_lossless(self):
        p = self.check("join_copy", project([{"media": "clip1", "in": 5, "out": 7}, {"media": "clip1", "in": 18, "out": 20}]))
        self.assertEqual(p.kind, "copy")

    def test_exact_mode_reencodes(self):
        p = self.check("trim_exact", project([{"media": "clip1", "in": 5, "out": 15}]), mode="encode")
        self.assertEqual((p.kind, p.segments[0].start, p.segments[0].end), ("encode", 5, 15))

    def test_mismatched_clips_fall_back_and_say_why(self):
        proj = project([{"media": "clip1", "in": 5, "out": 7}, {"media": "clip2", "in": 1, "out": 3}])
        p = plan(proj, INFOS)
        self.assertEqual(p.kind, "encode")
        self.assertTrue(any("differs from clip 1" in r for r in p.reasons))
        with self.assertRaises(PlanError):
            plan(proj, INFOS, mode="copy")

    def test_hdr_kept_only_when_every_clip_is_the_same_hdr(self):
        only_hdr = project([{"media": "clip2", "in": 7, "out": 10, "crop": {"aspect": "1:1"}}])
        p = self.check("hdr_keep", only_hdr)
        self.assertEqual(p.video.color, "hlg")
        self.assertEqual(plan(project(**MONTAGE), INFOS).video.color, "sdr")

    def test_crop_accounts_for_non_square_pixels(self):
        # clip2 is stored 1080x1080 with 9:16 pixels: it displays as 1080x1920 (stretched, never squeezed).
        self.assertEqual(INFOS["clip2"].video.display_size, (1080, 1920))
        p = plan(project([{"media": "clip2", "in": 0, "out": 1, "crop": {"x": 0, "y": 320, "w": 540, "h": 800}}]), INFOS)
        c = p.segments[0].crop
        self.assertEqual((c.x, c.y, c.w, c.h), (0, 180, 540, 450))
        self.assertEqual((p.video.width, p.video.height), (540, 800))
        with self.assertRaises(PlanError):
            plan(project([{"media": "clip2", "in": 0, "out": 1, "crop": {"x": 800, "y": 0, "w": 400, "h": 400}}]), INFOS)

    def test_different_shapes_are_fitted_with_bars(self):
        p = plan(project([{"media": "clip1", "in": 5, "out": 7}, {"media": "clip2", "in": 1, "out": 3}]), INFOS)
        self.assertEqual((p.video.width, p.video.height), (1080, 1920))
        self.assertEqual(p.segments[0].fit, (1080, 810))

    def test_out_point_past_end_is_rejected(self):
        with self.assertRaises(PlanError):
            plan(project([{"media": "clip1", "in": 5, "out": 99}]), INFOS)

    def test_lane_item_needs_sound(self):
        with self.assertRaises(PlanError):
            plan(project([{"media": "clip2", "in": 0, "out": 5}], [{"items": [{"media": "clip1", "in": 0, "out": 5}]}]), INFOS)


class Model(unittest.TestCase):
    def test_round_trip_and_exact_time(self):
        p = project(**MONTAGE)
        self.assertEqual(Project.from_dict(p.to_dict()).to_dict(), p.to_dict())
        q = project([{"media": "clip1", "in": "1001/30000", "out": 7.5}])
        self.assertEqual((q.clips[0].start, q.clips[0].end), (Fraction(1001, 30000), Fraction(15, 2)))

    def test_rejects_bad_projects(self):
        for clips in ([], [{"media": "nope", "in": 0, "out": 1}], [{"media": "clip1", "in": 3, "out": 3}],
                      [{"media": "clip1", "in": -1, "out": 3}], [{"media": "clip1", "in": "x", "out": 3}]):
            with self.assertRaises(ProjectError):
                project(clips)
        with self.assertRaises(ProjectError):
            Project.from_dict({"version": 2, "media": {}, "clips": []})


class Editing(unittest.TestCase):
    def session(self):
        s = Session()
        s.add_media("/media/clip1.mp4", INFOS["clip1"])
        s.add_media("/media/clip2.mp4", INFOS["clip2"])
        return s

    def test_new_clips_cover_the_whole_file(self):
        s = self.session()
        self.assertEqual([(c.start, c.end) for c in s.clips], [(0, INFOS["clip1"].duration), (0, INFOS["clip2"].duration)])
        self.assertEqual((s.selected, s.clips[0].audio, s.clips[1].audio), (1, False, True))

    def test_trim_split_and_undo(self):
        s = self.session()
        s.selected = 0
        s.checkpoint(); s.set_in(Fraction(5)); s.set_out(Fraction(15))
        self.assertTrue(s.split(Fraction(9)))
        self.assertEqual([(c.media, c.start, c.end) for c in s.clips[:2]], [("clip1", 5, 9), ("clip1", 9, 15)])
        self.assertEqual(s.selected, 1)
        self.assertFalse(s.split(Fraction(9)))  # nothing to split at a clip's own edge
        self.assertTrue(s.undo())
        self.assertEqual((len(s.clips), s.clips[0].end), (2, 15))
        self.assertTrue(s.undo())
        self.assertEqual((s.clips[0].start, s.clips[0].end), (0, INFOS["clip1"].duration))
        self.assertTrue(s.redo())
        self.assertEqual(s.clips[0].start, 5)

    def test_trim_is_clamped(self):
        s = self.session()
        s.selected = 0
        s.set_out(Fraction(999)); s.set_in(Fraction(999))
        c = s.clips[0]
        self.assertEqual(c.end, INFOS["clip1"].duration)
        self.assertEqual(c.end - c.start, s.frame())

    def test_one_shape_for_all_clips_and_unlock(self):
        s = self.session()
        s.set_shape(Fraction(4, 3))
        a, b = s.clips
        self.assertEqual((a.crop.w, a.crop.h), (640, 480))
        self.assertEqual((b.crop.x, b.crop.y, b.crop.w, b.crop.h), (0, 555, 1080, 810))
        self.assertEqual(plan(s.project, INFOS, mode="encode").video.width, 1080)
        s.lock_shape = False
        s.set_shape(Fraction(1))  # only the selected clip (clip2)
        self.assertEqual((s.clips[0].crop_aspect, s.clips[1].crop_aspect), (Fraction(4, 3), Fraction(1)))
        s.lock_shape = True
        s.set_shape("off")
        self.assertEqual([c.crop for c in s.clips], [None, None])

    def test_crop_stays_inside_the_frame(self):
        s = self.session()
        s.selected = 0
        s.set_crop(Rect(600, -50, 9999, 100))
        self.assertEqual(s.clip.crop, Rect(0, 0, 640, 100))

    def test_reorder_remove_and_montage_time(self):
        s = self.session()
        s.selected = 0
        s.set_in(Fraction(5)); s.set_out(Fraction(15))
        s.selected = 1
        s.set_in(Fraction(7)); s.set_out(Fraction(17))
        self.assertEqual((s.offsets(), s.total), ([0, 10], 20))
        self.assertEqual(s.montage_time(Fraction(9)), 12)
        s.move(1, 0)
        self.assertEqual(([c.media for c in s.clips], s.selected), (["clip2", "clip1"], 0))
        s.remove(0)
        self.assertEqual(([c.media for c in s.clips], s.selected), (["clip1"], 0))

    def test_plan_follows_the_exact_switch(self):
        s = Session()
        s.add_media("/media/clip1.mp4", INFOS["clip1"])
        s.set_in(Fraction(5)); s.set_out(Fraction(15))
        self.assertEqual(s.plan().kind, "copy")
        s.exact = True
        self.assertEqual(s.plan().kind, "encode")
        self.assertTrue(s.default_output(s.plan()).endswith("clip1-cut.mp4"))

    def test_sound_lanes(self):
        s = self.session()
        self.assertEqual(s.add_sound("/media/clip2.mp4", INFOS["clip2"], Fraction(0)), (0, 0))
        s.set_in(Fraction(7)); s.set_out(Fraction(17))  # trims the sound item, not the clip
        self.assertEqual((s.sound.start, s.sound.end, s.clips[1].start), (7, 17, 0))
        self.assertEqual(s.add_sound("/media/clip2.mp4", INFOS["clip2"], Fraction(5)), (1, 0))  # overlaps: other lane
        self.assertEqual(s.sound_at(0, Fraction(3)), (s.lanes[0].items[0], 10))
        self.assertIsNone(s.sound_at(0, Fraction(10)))
        s.select_sound(0, 0)
        s.move_sound(Fraction(40), 1)
        self.assertEqual((s.sel_audio, s.sound.at, len(s.lanes[0].items)), ((1, 1), 40, 0))
        s.set_gain(-200); self.assertEqual(s.sound.gain_db, -60)
        s.remove_sound()
        self.assertEqual((s.sel_audio, len(s.lanes[1].items)), (None, 1))
        self.assertTrue(s.undo())
        self.assertEqual(len(s.lanes[1].items), 2)
        with self.assertRaises(ValueError):
            s.add_sound("/media/clip1.mp4", INFOS["clip1"], Fraction(0))  # clip1 has no sound

    def test_sound_under_both_clips_plans_two_sources(self):
        s = self.session()
        s.select(0); s.set_in(Fraction(5)); s.set_out(Fraction(15))
        s.select(1); s.set_in(Fraction(7)); s.set_out(Fraction(17))
        s.add_sound("/media/clip2.mp4", INFOS["clip2"], Fraction(0))
        s.set_in(Fraction(7)); s.set_out(Fraction(17))
        p = s.plan()
        self.assertEqual((p.kind, sorted(a.at for a in p.audio)), ("encode", [0, 10]))

    def test_locate(self):
        s = self.session()
        s.select(0); s.set_in(Fraction(5)); s.set_out(Fraction(15))
        s.select(1); s.set_in(Fraction(7)); s.set_out(Fraction(17))
        self.assertEqual((s.locate(Fraction(3)), s.locate(Fraction(12)), s.locate(Fraction(99))), ((0, 8), (1, 9), (1, 17)))

    def test_project_file_round_trip(self):
        import tempfile
        s = self.session()
        s.select(0); s.set_in(Fraction(5)); s.set_out(Fraction(15))
        s.set_shape(Fraction(4, 3))
        s.add_sound("/media/clip2.mp4", INFOS["clip2"], Fraction(2))
        self.assertTrue(s.dirty)
        with tempfile.TemporaryDirectory() as d:
            f = f"{d}/edit.pve"
            s.save(f)
            self.assertFalse(s.dirty)
            stored = json.loads(Path(f).read_text())
            self.assertTrue(all(not m["path"].startswith("/") for m in stored["media"].values()))
            project = Session.read(f)
            self.assertEqual(project.media, s.project.media)  # absolute again
            t = Session()
            t.open(f, project, dict(s.infos))
            self.assertEqual(t.project.to_dict(), s.project.to_dict())
            self.assertFalse(t.dirty)
            t.set_out(Fraction(14))
            self.assertTrue(t.dirty)

    def test_centered_rect(self):
        self.assertEqual(centered_rect(1920, 1080, Fraction(4, 3)), Rect(240, 0, 1440, 1080))
        self.assertEqual(centered_rect(1080, 1920, Fraction(1)), Rect(0, 420, 1080, 1080))


if __name__ == "__main__":
    unittest.main()
