# Plain video editor

A minimal video editor for Linux: trim, crop, join clips into a montage, lay
sound under them, and export. When an edit is only trims and joins of matching
clips it is exported losslessly, without re-encoding.

There are two builds that behave the same and are held to the same test cases:

- **Python** in `pve/` — the one to iterate on.
- **Rust** in `rust/` — one small binary each for the window and the command line.

Both use mpv for the preview and ffmpeg for export.

## Requirements

- `ffmpeg` and `ffprobe` on the PATH
- GTK 4 and mpv's library: `sudo apt install libmpv2 python3-mpv`
- Python build: Python 3.11+, PyGObject (already present on a GNOME desktop)
- Rust build: a Rust toolchain, plus `sudo apt install libgtk-4-dev libmpv-dev`
  (or, without installing anything, `. rust/dev-env.sh` before building)

## Run it

```
python3 -m pve gui clip1.mp4 clip2.mp4          # Python

cd rust && cargo build --release --workspace     # Rust
rust/target/release/pve-gui clip1.mp4 clip2.mp4
```

Files can also be added with **Add video**, or dropped on the window. A video's
own sound appears in sound lane 1 under its clip. A sound file (or **Add
sound**) goes into whichever lane is free at the play position.

## The window

```
┌──────────────────────────────────────────────────────────────────┐
│ [Add video] [Add sound]                            [☰] [Export] │
│                                                                  │
│                           preview                                │
│                 crop rectangle drawn over it                     │
│                                                                  │
│ ▶  [position] / total  In [..] Out [..] length Sound[1][2]─●─ Crop│
│      ▼  play position: drag the triangle, or click this band     │
│  ░░░░│▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓│░░░░░░░   the selected clip's whole source │
│     ▶                  ◀         in and out: drag the triangles  │
│ [ clip 1   │    ][ clip 2      ]  the montage, in order; drag    │
│            │     [ clip 2 sound ]  lane 1: each clip's own sound  │
│ [ music    │           ]          lane 2; drag the line to scrub │
│ Lossless copy · 0:11.700 · cuts move out to keyframes, +1.7 s    │
└──────────────────────────────────────────────────────────────────┘
```

- **Trim bar.** The play position has a triangle above the bar; the in and out
  points have triangles below it, pointing at their lines. Dragging a cut point shows that frame but leaves
  the play position where it was. Clicking the bar itself, away from a line,
  does nothing. Scroll to zoom around the pointer; Shift+scroll pans.
- **Times.** Position, In and Out can be typed: `83.5`, `1:23.5`, `1:02:03.25`.
- **Crop.** Pick a shape; it applies to every clip unless **All clips** is off.
  Drag the rectangle or its corners. **Free** is any shape, for the selected clip.
- **Montage.** Click a clip to select it; drag to reorder. Drag the play line
  (anywhere along its height) to scrub through the whole montage. Playback
  carries on from one clip into the next.
- **Sound.** Every sound is an item in one of the two lanes. Click an item to
  select it; then the trim bar, the times and the volume act on it, Delete
  removes it, and dragging moves it (between lanes too). Items that overlap
  are mixed in the export, whether they share a lane or not.
- **A clip's own sound** sits in lane 1, drawn joined to its clip. While it is
  linked it follows the clip through trims, splits and reordering, and the
  export can still be a lossless copy. Moving or trimming it by itself unlinks
  it: from then on it is an ordinary item that stays where you put it (undo
  links it again).
- **Sound 1 / 2** choose which lanes you hear in the preview while editing.
  They do not change the export, which always has every sound shown.
- **Bottom line.** Always says what Export will do: a lossless copy, and how
  far the cuts move to reach keyframes (shown paler on the trim bar), or a
  re-encode with the output size and the reason. **Exact cuts** forces a re-encode.

| Key | Does |
|---|---|
| Space | play / pause |
| Left / Right | one frame back / forward (Shift: one second) |
| I / O | set the in / out point at the play position |
| S | split the clip at the play position |
| Delete | remove the selected clip or sound item |
| Home / End | go to the in / out point |
| Escape | leave a sound item and go back to the clips |
| Ctrl+Z / Ctrl+Shift+Z | undo / redo |
| Ctrl+O / Ctrl+S / Ctrl+E | add files or open a project / save project / export |

Closing the window never loses an edit: the session is kept and
**☰ → Reopen last session** brings it back. Projects are saved as `.pve` files
(JSON, with media paths relative to the project file).

## What export does

- **Lossless copy** when the edit is only trims and joins of clips with the
  same format, no crop, no volume change and no sound besides the clips' own. Cuts land on
  keyframes: the start moves earlier and the end later, never the other way.
- **Re-encode** otherwise: H.264 in MP4 at visually transparent quality
  (x264, CRF 18). The output is as large as the largest clip after cropping;
  smaller clips are scaled up, and clips of another shape get bars.
- **HDR** is kept (10-bit HEVC) only when every clip is the same kind of HDR;
  otherwise HDR clips are tone-mapped to standard range.
- **NVIDIA GPU**, when present: decoding always uses it (same pictures, sooner);
  the GPU *encoder* is used automatically for outputs larger than 1080p, where
  it saves real time. At equal quality it is 1.3–1.7× faster and about a third
  larger. `--encoder x264|nvenc` overrides this on the command line.

## Command line

```
pve probe clip1.mp4                          # what the planner knows about a file
pve plan  examples/montage.json              # what an export would do, and why
pve plan  examples/montage.json --commands   # ...and the ffmpeg commands
pve export examples/montage.json -o out.mp4
pve export examples/montage.json -o draft.mp4 --draft     # fast, small
```

`pve` is `python3 -m pve` or `rust/target/release/pve`; they print the same
thing. `--mode auto` (default) is lossless when possible, `--mode encode`
always re-encodes with exact cuts, `--mode copy` fails rather than re-encode.

In a project file, times are seconds from the start of the media (numbers, or
exact fractions like `"1001/30000"`). Crops are in display pixels
(`{"x","y","w","h"}`), or `{"aspect": "4:3"}` for the largest centred rectangle.

## Layout

| Part | Python | Rust |
|---|---|---|
| Project data, exact time | `pve/model.py` | `rust/pve-core/src/model.rs`, `time.rs` |
| Description of a media file | `pve/media.py` | `pve-core/src/media.rs` |
| Project → export plan (no ffmpeg syntax) | `pve/planner.py` | `pve-core/src/planner.rs` |
| Editing operations, undo, timeline | `pve/session.py` | `pve-core/src/session.rs` |
| ffprobe, ffmpeg | `pve/probe.py`, `pve/ffmpeg_backend.py` | `pve-native/src/` |
| Command line | `pve/cli.py` | `pve-native/src/main.rs` |
| Window | `pve/ui/` | `pve-gtk/src/` |

The core knows nothing about ffmpeg, mpv, GTK or the filesystem, and the Rust
core also builds for WebAssembly (`cargo check -p pve-core --target wasm32-unknown-unknown`).

## Tests

```
python3 -m unittest                                   # Python core
cd rust && cargo test --workspace                     # Rust core, geometry, shared cases
python3 tests/ui/drive.py /tmp/out clip1.mp4 clip2.mp4            # Python window, scripted
PVE_SELFTEST=/tmp/out rust/target/release/pve-gui clip1.mp4 clip2.mp4   # Rust window, scripted
```

`tests/cases.json` holds project inputs and `tests/golden/` the expected plan
and ffmpeg commands for each; both cores must reproduce them exactly.
Regenerate after an intended change with `PVE_UPDATE_GOLDEN=1 python3 -m unittest`.

The window tests open a real window and need two clips: the first without
sound, the second with. They drive the widgets' own press, drag and release
code with pixel coordinates; they do not send real pointer events.

## Known limits

- A lossless join of clips that have sound leaves a seam of about 0.1 s at
  each join; a lossless cut may carry up to a tenth of a second of extra sound
  at its start.
- A lossless copy keeps the picture and the first sound track only; extra
  tracks from the camera (metadata, timecode, thumbnails) are dropped.
- Frame-accurate lossless cutting ("smart cut") is not implemented.
- Lane sound in the preview is played by a second player and can be a few
  hundredths of a second out; the export is exact.
- The preview plays one item per lane at a time. Where two items overlap in
  the same lane, only one of them is heard in the preview; the export mixes
  both.
- Rotated phone clips and variable-frame-rate clips are handled by design but
  have not been tested on real files.
