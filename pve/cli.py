"""pve: headless command-line front end to the core.

  pve probe FILE
  pve plan PROJECT.json [--mode auto|copy|encode] [--draft] [--json] [--commands]
  pve export PROJECT.json -o OUT [--mode ...] [--draft] [--force]
  pve gui [FILE...]
"""
from __future__ import annotations

import argparse
import json
import shlex
import sys
import time
from pathlib import Path

from . import ffmpeg_backend
from .media import MediaInfo
from .model import Project, ProjectError
from .planner import ExportPlan, PlanError, plan as make_plan
from .probe import ProbeError, probe


def _t(x) -> str:
    return f"{float(x):.3f}"


def load(project_file: str) -> tuple[Project, dict[str, str], dict[str, MediaInfo]]:
    pf = Path(project_file)
    project = Project.from_dict(json.loads(pf.read_text()))
    paths = {k: str((pf.parent / v).resolve()) for k, v in project.media.items()}
    used = {c.media for c in project.clips} | {i.media for lane in project.lanes for i in lane.items}
    infos = {k: probe(paths[k]) for k in sorted(used)}
    return project, paths, infos


def describe(p: ExportPlan) -> str:
    out = []
    if p.kind == "copy":
        out.append(f"Lossless copy, {_t(p.duration)} s. Cuts are moved outwards to keyframes:")
        for i, s in enumerate(p.segments, 1):
            moved = (s.start, s.end) != (s.requested_start, s.requested_end)
            line = f"  {i}. {s.media}  {_t(s.start)} -> {_t(s.end)}  ({_t(s.duration)} s)"
            if moved:
                line += f"   asked for {_t(s.requested_start)} -> {_t(s.requested_end)}"
            out.append(line)
        out.append("  sound: " + ("copied" if p.copy_audio else "none"))
    else:
        v = p.video
        label = {"sdr": "standard range, H.264", "hlg": "HDR (HLG), HEVC 10-bit", "pq": "HDR (PQ), HEVC 10-bit"}[v.color]
        out.append(
            f"{'Draft re-encode' if p.draft else 'Re-encode'}, {_t(p.duration)} s: "
            f"{v.width}x{v.height} at {float(v.fps):g} fps, {label}"
        )
        if p.reasons:
            out.append("  not lossless because: " + "; ".join(p.reasons))
        for i, s in enumerate(p.segments, 1):
            notes = []
            if s.crop:
                notes.append(f"crop {s.crop.w}x{s.crop.h}+{s.crop.x}+{s.crop.y}")
            if s.fit != (v.width, v.height):
                notes.append(f"fitted to {s.fit[0]}x{s.fit[1]} with bars")
            if s.color.kind != v.color:
                notes.append(f"{s.color.kind.upper()} tone-mapped to standard range")
            out.append(f"  {i}. {s.media}  {_t(s.start)} -> {_t(s.end)}  " + ", ".join(notes))
        if p.audio:
            out.append("  sound:")
            for a in p.audio:
                gain = f", {a.gain_db:+g} dB" if a.gain_db else ""
                out.append(f"     {a.media}  {_t(a.start)} -> {_t(a.end)}  at {_t(a.at)}{gain}")
        else:
            out.append("  sound: none")
    out += [f"  warning: {w}" for w in p.warnings]
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="pve", description="Plain video editor: headless core.")
    sub = ap.add_subparsers(dest="cmd", required=True)

    sp = sub.add_parser("probe", help="show what the planner knows about a file")
    sp.add_argument("file")

    for name, text in (("plan", "show what an export would do"), ("export", "export a project")):
        sp = sub.add_parser(name, help=text)
        sp.add_argument("project")
        sp.add_argument("--mode", choices=("auto", "copy", "encode"), default="auto",
                        help="auto: lossless when possible; copy: lossless or fail; encode: always exact")
        sp.add_argument("--draft", action="store_true", help="fast low-resolution render for previewing")
        sp.add_argument("--encoder", choices=("auto", "x264", "nvenc"), default="auto",
                        help="auto: NVIDIA GPU encoder for outputs larger than 1080p when available, else x264")
        if name == "plan":
            sp.add_argument("--json", action="store_true", help="print the plan as JSON")
            sp.add_argument("--commands", action="store_true", help="print the ffmpeg commands")
        else:
            sp.add_argument("-o", "--output", required=True)
            sp.add_argument("--force", action="store_true", help="overwrite the output file")

    sp = sub.add_parser("gui", help="open the editor window")
    sp.add_argument("files", nargs="*")

    args = ap.parse_args(argv)
    if args.cmd == "gui":
        from .ui.app import main as gui_main  # imported here so the command line works without GTK or mpv

        return gui_main(args.files)
    try:
        if args.cmd == "probe":
            print(json.dumps(probe(args.file).to_dict(), indent=2))
            return 0

        project, paths, infos = load(args.project)
        p = make_plan(project, infos, mode=args.mode, draft=args.draft)

        if args.cmd == "plan":
            if args.json:
                print(json.dumps(p.to_dict(), indent=2))
            else:
                print(describe(p))
            if args.commands:
                for step in ffmpeg_backend.build(p, paths, "OUTPUT.mp4", ffmpeg_backend.detect(), "TMP", args.encoder):
                    print("\n" + shlex.join(step.argv))
            return 0

        out = Path(args.output).resolve()
        if str(out) in paths.values():
            print("error: the output would overwrite a source file", file=sys.stderr)
            return 2
        if out.exists() and not args.force:
            print(f"error: {out} exists (use --force to overwrite)", file=sys.stderr)
            return 2
        print(describe(p))
        if p.kind == "encode":
            gpu = ffmpeg_backend.uses_gpu_encoder(p, ffmpeg_backend.detect(), args.encoder)
            print("  encoder: " + ("NVIDIA GPU" if gpu else "x264"))
        t0 = time.monotonic()
        tty = sys.stderr.isatty()

        def progress(f: float) -> None:
            if tty:
                print(f"\r  {f * 100:5.1f}%", end="", file=sys.stderr, flush=True)

        ffmpeg_backend.run(p, paths, str(out), progress, encoder=args.encoder)
        if tty:
            print("\r", end="", file=sys.stderr)
        print(f"wrote {out} in {time.monotonic() - t0:.1f} s")
        return 0
    except (ProjectError, PlanError, ProbeError, ffmpeg_backend.ExportError, OSError, json.JSONDecodeError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("\ncancelled", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
