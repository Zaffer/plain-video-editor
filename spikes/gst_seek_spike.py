"""Spike: the same seek / frame-step measurements as mpv_gtk4_spike.py, on GStreamer.

Decode only (frames go to a fakesink, nothing is drawn), so it needs no window.
That flatters GStreamer slightly against the mpv spike, which also renders each frame.

Usage:  python3 spikes/gst_seek_spike.py FILE [--software] [--play 4] [--json OUT]
"""
import argparse
import json
import statistics
import sys
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402

HW_DECODERS = ("nvh265dec", "nvh264dec", "nvav1dec", "vah265dec", "vah264dec", "vulkanh265dec", "vulkanh264dec")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--software", action="store_true", help="disable hardware decoders")
    ap.add_argument("--play", type=float, default=4.0)
    ap.add_argument("--json")
    args = ap.parse_args()

    Gst.init(None)
    if args.software:
        reg = Gst.Registry.get()
        for name in HW_DECODERS:
            f = reg.lookup_feature(name)
            if f:
                f.set_rank(Gst.Rank.NONE)

    pipe = Gst.ElementFactory.make("playbin3")
    vsink = Gst.ElementFactory.make("fakesink", "vsink")
    vsink.set_property("sync", True)
    vsink.set_property("enable-last-sample", True)
    asink = Gst.ElementFactory.make("fakesink", "asink")
    asink.set_property("sync", True)
    pipe.set_property("video-sink", vsink)
    pipe.set_property("audio-sink", asink)
    pipe.set_property("uri", Gst.filename_to_uri(args.file))
    bus = pipe.get_bus()
    decoders = []

    def element_added(_bin, _sub, el):
        f = el.get_factory()
        if f and "Decoder/Video" in (f.get_metadata("klass") or ""):
            decoders.append(f.get_name())

    pipe.connect("deep-element-added", element_added)

    def wait_done(timeout=20.0):
        msg = bus.timed_pop_filtered(int(timeout * Gst.SECOND), Gst.MessageType.ASYNC_DONE | Gst.MessageType.ERROR)
        if msg is None:
            return False
        if msg.type == Gst.MessageType.ERROR:
            raise RuntimeError(msg.parse_error()[0].message)
        return True

    def shown_pts():
        sample = vsink.get_property("last-sample")
        return sample.get_buffer().pts / Gst.SECOND if sample else None

    def seek(t, flags):
        t0 = time.perf_counter()
        pipe.seek_simple(Gst.Format.TIME, Gst.SeekFlags.FLUSH | flags, int(t * Gst.SECOND))
        ok = wait_done()
        return (time.perf_counter() - t0) * 1000 if ok else float("nan")

    def stats(xs):
        xs = [x for x in xs if x == x]
        return {"median_ms": round(statistics.median(xs), 1), "max_ms": round(max(xs), 1), "n": len(xs)} if xs else None

    results = {"engine": "gstreamer " + Gst.version_string(), "software": args.software}
    try:
        measure(args, pipe, vsink, bus, wait_done, shown_pts, seek, stats, decoders, results)
    except Exception as e:  # report rather than hang with a live pipeline
        results["error"] = repr(e)
    finally:
        pipe.set_state(Gst.State.NULL)
    text = json.dumps(results, indent=2)
    print(text)
    if args.json:
        with open(args.json, "w") as f:
            f.write(text + "\n")


def measure(args, pipe, vsink, bus, wait_done, shown_pts, seek, stats, decoders, results):
    pipe.set_state(Gst.State.PAUSED)
    wait_done(30)

    caps = vsink.get_static_pad("sink").get_current_caps().get_structure(0)
    ok, fn, fd = caps.get_fraction("framerate")
    fps = fn / fd if ok and fd else 30.0
    dur = pipe.query_duration(Gst.Format.TIME)[1] / Gst.SECOND
    decoder = decoders[-1] if decoders else None
    results["file"] = {"w": caps.get_value("width"), "h": caps.get_value("height"), "fps": fps, "duration": dur,
                       "decoder": decoder, "format": caps.get_value("format")}

    # Playback: count frames reaching the sink in real time.
    count = [0]
    probe = vsink.get_static_pad("sink").add_probe(
        Gst.PadProbeType.BUFFER, lambda *_: (count.__setitem__(0, count[0] + 1), Gst.PadProbeReturn.OK)[1])
    pipe.set_state(Gst.State.PLAYING)
    pipe.get_state(5 * Gst.SECOND)
    count[0] = 0
    time.sleep(args.play)
    results["playback"] = {"seconds": args.play, "frames_per_second_reaching_sink": round(count[0] / args.play, 1)}
    pipe.set_state(Gst.State.PAUSED)
    pipe.get_state(5 * Gst.SECOND)
    vsink.get_static_pad("sink").remove_probe(probe)
    while bus.pop():
        pass

    points = [dur * f for f in (0.13, 0.71, 0.29, 0.86, 0.47, 0.05, 0.62, 0.93, 0.38, 0.19)]
    results["seek_keyframe"] = stats([seek(p, Gst.SeekFlags.KEY_UNIT) for p in points])
    results["seek_exact"] = stats([seek(p, Gst.SeekFlags.ACCURATE) for p in points])

    # Forward frame step from mid-GOP.
    seek(dur * 0.55, Gst.SeekFlags.ACCURATE)
    fwd = []
    for _ in range(15):
        t0 = time.perf_counter()
        pipe.send_event(Gst.Event.new_step(Gst.Format.BUFFERS, 1, 1.0, True, False))
        fwd.append((time.perf_counter() - t0) * 1000 if wait_done(5) else float("nan"))
    results["frame_step_forward"] = stats(fwd)

    # Backward step = exact seek to the previous frame (what an app would do).
    back = []
    for _ in range(15):
        pos = shown_pts()
        back.append(seek(max(pos - 1 / fps, 0) + 0.0001, Gst.SeekFlags.ACCURATE))
    results["frame_step_back (exact seek to previous frame)"] = stats(back)

    # Exactness is not measured here: after an accurate seek GStreamer clips the shown
    # buffer's timestamp to the seek position, so timestamps cannot identify the frame.
    # It was checked separately by comparing pixels with ffmpeg's frames (exact).


if __name__ == "__main__":
    sys.exit(main())
