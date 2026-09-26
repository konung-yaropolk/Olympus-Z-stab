# Olympus Z-plane stabiliser

Keeps a two-photon timelapse in focus while it is being recorded.

Point it at the `.oir` the FluoView software is writing. It follows the file as it
grows, measures how much sharper the start of the recording was than the last few
seconds of it, and clicks the acquisition software's own Z controls to put the
focus back.

```
olympus-z-stab D:\data\Field_4.oir
```

Nothing is clicked until you pass `--arm`. The first session on a new rig should
always be a dry run.

---

## What it actually does

```
  the .oir, still being written
            |
            |  follow the file forward, never re-reading, never mapping
            v
  one frame per timepoint  ------------------------------> logged
            |
            |  cancel the X/Y movement (measured, never corrected)
            v
  the window both frames share
            |
            |  focus metric -> moving average of the last N frames
            v
  compared against the average of the first N frames
            |
            |  outside the dead band, for long enough, past the cooldown?
            v
  click z_up or z_down  ------> confirm it landed, using the file's own zPosition
```

Three parts of that are worth understanding before you trust it.

### It reads a file that is still open for writing

A finished `.oir` tells you where its block index is, and every OIR reader in
existence starts from that index. A file still being recorded has no index yet —
it is the last thing written.

So this reader does not use the index. OIR blocks are `u32 length, u32 type,
payload`, which makes them self-delimiting: the stream can be walked forward from
the first block with no index at all. On the 1.076 GB reference acquisition,
walking from the first block visits 10,219 blocks and lands **exactly** on the
offset the header gives for the index, and every one of the 10,218 offsets that
index lists is a block the walk visited. The walk and the index agree, so the walk
is trustworthy on a file where the index does not exist yet.

The reader also never memory-maps (a mapping's length is fixed when it is made, so
it would never see a byte written afterwards) and never consumes a block whose
declared length runs past the current end of the file — that block is left alone
until the scope has finished writing it.

A recording is split into ~1.08 GB parts named `<stem>.oir`, then `<stem>_00001`,
`<stem>_00002`, … with no extension. Running out of the current part is not the end
of the acquisition; the reader looks for the next part, and only calls it finished
when nothing has grown for `idle_timeout_s`.

### The X/Y movement is cancelled, not corrected

You said the field slides sideways during a recording and that this must not be
corrected. It is not: the measured shift is logged and nothing else.

But it does have to be cancelled *inside the measurement*, because a focus metric
computed over a field that has moved compares different tissue to itself. Each
frame is shifted back onto the reference by phase correlation before it is
measured, and then only the region the two have in common is used.

The shift is applied as a **whole number of pixels**, never interpolated. This is
the one design decision in the program that is not obvious and cannot be changed:
resampling an image to apply a sub-pixel shift smooths it, and smoothing is exactly
what a focus metric measures. A stabiliser that interpolated would read its own
interpolation as defocus and chase it.

### The focus metric has to survive bleaching

In a two-photon recording of living tissue three things change the picture over
minutes, and only one of them is focus:

| | effect on brightness and contrast |
|---|---|
| **bleaching** | falls steadily, all session, whether or not anything moved |
| **activity** | rises and falls — it is what you are recording |
| **defocus** | falls |

A metric that cannot tell the first two from the third will drive the stage all
session and call it stabilisation.

So the metrics were **measured** against a real 29-minute recording from this rig —
one in which a drug application nearly triples the fluorescence, which is a hard
case rather than a gentle one. For each metric: how far it wanders from its
starting value over the recording with nothing defocused, against how far a real
0.5 px defocus moves it.

| metric | fast noise | drift, nothing wrong | 0.5 px defocus | signal / drift |
|---|---|---|---|---|
| **`high_freq_ratio`** | 2.2% | **18%** | −32% | **1.8** |
| `brenner` | 4.4% | 169% | −33% | 0.2 |
| `tenengrad` | 4.5% | 154% | −33% | 0.2 |
| `top_percentile` | 2.5% | 169% | −3.7% | 0.0 |
| `norm_variance` | 3.8% | **415%** | −9.0% | 0.0 |

`high_freq_ratio` is the only one whose signal is larger than its own drift, and
the only one to use with a fixed start-of-recording reference. The rest respond to
defocus perfectly well — they are simply swamped by the sample getting brighter.

Two consequences worth knowing before you trust a session:

- **The default `dead_band` is 0.20, not the 3% that seems reasonable.** Running
  this program over the whole reference recording, `metric_rel` spanned 0.914 to
  1.042 across 12,492 measured frames with nothing defocused — a worst honest drop
  of 8.6%. A 3% band fires in the first minutes and then fires all session. 20%
  leaves better than a 2× margin and still catches about 0.45 px of blur.
- **`hill_climb` is on much firmer ground than the table suggests.** It compares a
  probe against a measurement ten seconds old, not against half an hour ago, and
  over ten seconds the noise is ~2%, not 8.6%. The dead band only decides *when to
  investigate*; the probe decides what to do.

### The alignment reference has to be refreshed

Aligning every frame against the *first* frame of the session works beautifully
for about four minutes and then stops working entirely. On the reference
recording the correlation peak fell from 200× the surface mean to a noise floor of
6× by frame 4500; past that the peak position was random, exceeded
`max_shift_px`, and every frame was rejected. Two thirds of a 12,561-frame
recording was skipped and the moving average silently froze.

The sample at frame 6000 matches frame 5900 perfectly well — it just does not
match frame 1. So after `measure.registration.refresh_after` consecutive failed
matches, the current frame becomes the new alignment reference. On the reference
recording that happens nine times in 29 minutes and lifts coverage from 34% to
91%.

Refreshing is safe for the focus measurement: the metric is a ratio of spatial
frequencies *within* one window, not a pixel-wise comparison against the
reference, so which window it is measured on does not matter as long as the frames
inside one moving average are aligned to each other. The starting-position focus
reference is a separate thing and is never refreshed.

### Saturation rejection is for a field clipped flat

A two-photon image of bright cell bodies always has some clipped pixels. On the
reference recording a frame from the brighter half clips 2.9% of them — and the
focus signal is completely unharmed: a 0.5 px defocus still moves the metric by
31% to 38%, exactly as it does in the dim half. The original 1% limit rejected a
sixth of the recording as unmeasurable while nothing was wrong with it, so the
default is `0.20`.

The frequency cuts were measured the same way. The obvious first choice, 0.05 to
0.35 of Nyquist, drifted 40% over the recording — the band above 0.35 Nyquist is
mostly shot noise, and shot noise shrinks as a fraction of the signal when the
signal brightens, so that choice was substantially measuring brightness. 0.12 to
0.30 is a broad plateau, so small changes are harmless.

### Which way is up

A sharpness metric tells you *that* focus moved, never which way. There are two
modes, chosen with `control.mode`:

**`hill_climb`** (default) — when the metric has dropped far enough for long
enough, step once in the direction that worked last time, look again, and if it got
worse reverse by twice as much. Needs no calibration and no z-stack, at the cost of
one wrong step per event. Thermal drift in a given rig is consistent, so after the
first event it is usually guessing right; set `initial_direction` to the direction
your rig actually drifts and it usually guesses right the first time too.

**`reference_stack`** — record a z-stack over the field of view before the
timelapse. Each window is correlated against every plane of it, and a parabola
through the best three gives a **signed** offset in microns, so the stage moves once
and in the right direction. More accurate, and it never probes; it costs you a
z-stack, it must be of this field of view, and you have to tell it
`actuator.um_per_step`.

---

## Setting it up

### 1. Build it

The acquisition machine is Windows 7, and **Rust dropped Windows 7 support in
1.78** — so this is pinned to 1.77 in `rust-toolchain.toml`, and every dependency
is pinned to a version that builds there. `rustup` will fetch the toolchain for you.

```bash
cargo build --release
```

The CRT is linked statically (`.cargo/config.toml`), so the resulting
`target\release\olympus-z-stab.exe` is one file with no Visual C++ redistributable
to install — which is the normal state of a machine that only ever runs the
microscope's own software. Copy the exe and `config.yaml` to the acquisition machine
side by side.

For a 32-bit machine: `cargo build --release --target i686-pc-windows-msvc`.

There is no OpenCV, deliberately. The `opencv` crate binds the real C++ library,
which would have to be built on and shipped to a machine that cannot be given a
modern toolchain — to get four operations (a 2-D FFT, a phase correlation, a
gradient, a normalised cross-correlation) that are a page of code each. `rustfft`
supplies the only genuinely hard part. `src/cv/` is the whole "CV library", sized to
the job.

### 2. Find the Z buttons

```bash
olympus-z-stab --where
```

Move the pointer onto the acquisition software's Z control and read the numbers
off. Put the pointer in the upper-left corner of the screen to stop.

Then write the sequences into `config.yaml`:

```yaml
actuator:
  z_up:
    - {step: click, x: 1850, y: 420, delay: 0.3}
  z_down:
    - {step: click, x: 1850, y: 470, delay: 0.3}
```

These use the same vocabulary as `autoclicker.exe`'s workflow files — `click`,
`right_click`, `text_input`, `press_key`, `hotkey`, each with a `delay` — so a
sequence recorded with that tool's recorder can be pasted straight in. If the
software is driven by a numeric Z field rather than step buttons, write the sequence
that clicks the field, types the value and presses Enter; `{z}` in a `text_input` is
replaced with the absolute z being asked for.

### 3. Rehearse on a recording you already have

```bash
olympus-z-stab D:\data\an_old_recording.oir --replay
```

`--replay` copies a finished recording into a scratch file a piece at a time, at the
rate it was originally recorded, and follows the copy exactly as it would follow a
live acquisition. Every byte the reader sees is a byte the scope wrote — it is a
rehearsal, not a simulation.

This is how you choose `control.dead_band`, which cannot be picked from first
principles: it depends on your sample, objective, laser power and noise. Run a
replay on a recording where nothing went wrong, open `zstab-log.csv`, and take the
largest **downward** excursion the `metric_rel` column makes. Your dead band goes
above that.

**`dead_band` is a fraction, not a percentage.** `0.20` is twenty per cent; a
tenth of a per cent is `0.001`. Writing `0.1` for "nought point one per cent"
gives a ten per cent band, and on a recording whose honest drift never exceeds
8.6% that corrects nothing at all while looking broken. The startup banner states
it both ways, against the `rel` column printed on every row:

```
  band    0.10% — acts when rel < 0.999
```

Check that line rather than the config.

Note also that lowering the band past the point where it fires buys very little:
on the reference recording `0.01` gives 243 corrections and `0.001` gives 244,
because once the metric is outside the band it is `cooldown_s` that decides how
often the program acts, not how far outside it is.

Do this. The shipped default of 0.20 came from one recording on one rig, and it is
nearly seven times larger than the value that looked obviously right before it was
measured. If your own number comes out so large that real drift would be missed,
that is the signal to use `mode: reference_stack` instead — it compares against
actual z planes and is not troubled by the sample changing brightness.

### 4. Dry run on a real session

Start a recording, start the program without `--arm`, and let it watch. It will
print and log every correction it *would* have made. Read the log afterwards. If the
decisions look right, arm it next time.

### 5. Arm it

```bash
olympus-z-stab D:\data\Field_4.oir --arm
```

or set `arm: true` in `config.yaml`. It asks for confirmation before it starts.

**Emergency stop: put the mouse pointer in the upper-left corner of the screen.**
Checked before every single click, and sticky once triggered.

---

## Running it

With no arguments it asks for the file, so the exe can be left on the desktop:
drag the `.oir` into the window and press Enter. Paths with spaces and surrounding
quotes are handled.

```
olympus-z-stab [<file.oir>] [options]

  --arm              Actually click. Without this nothing touches the mouse.
  --dry-run          Never click, whatever config.yaml says.
  --config <path>    Settings file. Default: config.yaml beside the exe.
  --where            Print the mouse position, to fill in click coordinates.
  --replay           Rehearse: follow a growing copy of a finished recording.
  -h, --help
```

### What it prints

Everything fits in 48 columns, so the window can sit beside the acquisition
software. The column header is printed once and the rows carry no labels:

```
  512x512, 2 ch -> 384x384 window, ch Index(0)

     t    rel   dx  dy      dz  state
  [reference set: 0.8637]
  [part: Field_4_Dynorphin_application_00001]
  1500  1.022   +0  +1   +0.00  ok
  3000  1.041   +1  +3   -0.60  ok
  [realign t4165]
  4465  0.978   +5  +4   +0.20  MOVE +1
```

`rel` is the metric as a fraction of the starting position — the number
`dead_band` is compared against. `dx`/`dy` are the lateral drift being cancelled,
cumulative since the session started. **`dz` is microns the stage has been moved
from where the recording began**, not the absolute position: on this line the
question is always how far it has been moved, and the absolute is in the CSV.

States are `ok` (inside the dead band), `ref n/m` and `win n/m` (still filling),
`CONF n/m` (outside the band, confirming), `cool Ns`, `verify`, `skip xy` /
`skip sat`, and `MOVE +n` / `STOP` in capitals so a correction is findable by eye
in a screen of `ok`.

Nothing is lost by the abbreviation: every field on screen is in the CSV at full
precision, along with several that are only ever read afterwards — the frame's own
timestamp, the correlation peak, the absolute metric and the absolute `z`.

### Did it move the focus?

Every run ends by saying so, whether or not anything happened:

```
  Z NOT ADJUSTED — the focus never left the dead band.
```

```
  Z MOVED UP
  3 correction(s), +2 step(s) net, +1.38 um

      t      at  steps    z moved
   4465    595s     +1  9741.19 -> 9741.89  +0.70
   5310    708s     -1  9741.89 -> 9741.19  -0.70
   6120    816s     +2  9741.19 -> 9742.57  +1.38
```

The microns are what the **file** recorded for the frame after each correction —
what the stage actually did, not what was asked of it.

An unarmed run reports what it *would* have done, marking each with `*`, and does
not stop at the first one even when no click sequence is configured — seeing all
of them is the reason to run unarmed:

```
  Z NOT ADJUSTED (dry run)
  It would have moved up.
  223 correction(s), +223 step(s) net
  The drift was never corrected, so it came back
  and was proposed again after every cooldown:
  that is how often it would have acted, not how
  far an armed run would have moved.
```

That last caveat matters: on a dry run nothing corrects the drift, so the same
drift is re-proposed after every cooldown. The count is occasions, not distance.

### The log

`zstab-log.csv`, one row per measured frame, flushed as it is written — the
interesting case is a session that ended badly, and a log still sitting in a buffer
when that happened is a log of nothing.

| column | |
|---|---|
| `timepoint`, `elapsed_s`, `timestamp` | from the file's own per-frame timestamps, not the watching machine's clock |
| `shift_x`, `shift_y`, `peak` | the X/Y movement that was cancelled, and how well the frames matched |
| `focus`, `metric_rel` | the metric, and it as a fraction of the starting-position reference |
| `z_delta_um` | microns the stage has moved since the session started |
| `z_offset_um` | signed drift in microns (`reference_stack` mode only) |
| `z_reported` | the stage position the file says the software commanded |
| `decision`, `net_steps` | what was decided, and where the session stands |

`metric_rel` is the one to read: it is what `control.dead_band` is compared against.

---

## What keeps it from doing damage

Driving a stage on its own, during an experiment that cannot be repeated, in
software whose buttons it finds by screen coordinate. The guards, in the order they
bite:

- **Unarmed by default.** `arm: false`, and a fresh config's coordinates are
  placeholders, so the first run on a new rig cannot click.
- **A confirmation prompt** before an armed session starts.
- **The dead band**, so noise is not a correction.
- **`confirm_windows`** — a drop must persist across consecutive full windows. At
  7.5 Hz with 30-frame windows that is eight seconds, so a bright transient, a
  passing bubble or one bad frame cannot trigger anything.
- **`cooldown_s`** after every correction, so the stage settles and the moving
  average refills before the next decision. Acting before both have happened
  corrects the same drift twice, which is how a stabiliser oscillates.
- **`max_steps_per_event`** and **`max_total_steps`** — a hard bound on how far the
  program can possibly drive the stage in one session, whatever it believes.
- **Click verification.** The file records a `zPosition` for every frame. If it does
  not change after a correction, the clicks are not reaching the software — a
  missed coordinate, a window that lost focus — and the program **stops and says
  so** rather than clicking harder.
- **The corner**, checked before every step of every sequence.
- **Rejected frames.** A saturated frame, or one whose X/Y match failed, is skipped
  rather than measured — it has no high-frequency content to read and would look
  like catastrophic defocus.

---

## Honest limitations

- **It cannot tell defocus from anything else that removes fine detail.** A metric
  is not a measurement of z. `high_freq_ratio` is robust to bleaching and to
  whole-field brightness changes; it is not robust to the sample genuinely changing —
  tissue swelling, a cell dying in the field, debris drifting through. The
  `max_total_steps` bound exists because of this.
- **The margin on a fixed reference is only about 1.8×.** On the one recording it
  has been measured against, a 0.5 px defocus moves the metric 32% and the sample
  changing on its own moves it 18%. That is enough to work with and it is not a
  comfortable margin, and it is the reason `dead_band` has to be large and the
  reason `reference_stack` exists. If the focus has to be held tighter than about
  half a pixel of blur, record the z-stack and use that mode — do not tighten the
  dead band to get there.
- **`hill_climb` disturbs the recording.** Each event costs at least one step in
  possibly the wrong direction. Frames during a correction are real frames with the
  focus moving through them. `reference_stack` avoids the probe entirely and is the
  better mode if you can record the stack.
- **It clicks by screen coordinate.** Move the acquisition window, change the
  display scaling or the resolution, and the coordinates are wrong. Verification
  catches a click that does nothing; it does not catch a click that hits something
  else. Do not rearrange windows during an armed session.
- **It takes the mouse.** There is no way around that with a click-driven actuator.
- **`zPosition` is what the software commanded, not where the focal plane is.** In
  the reference recording it is the same 9741.19 for all 1018 frames of a part.
  It confirms a click landed; it cannot detect drift, which is the entire reason an
  image-based metric is needed.
- **One field of view.** A multi-position acquisition is not handled.
- **It has not yet been run against a genuinely live acquisition.** Every claim about
  the live path is established against a real finished recording plus `--replay`,
  which is a strong test and not the same thing. Treat the first live session as an
  unarmed experiment.
- **It has never actually clicked anything.** The actuator has no coordinates to
  click on this machine, so every armed path is tested by unit tests and by dry
  runs, not by moving a stage. The first armed session is the first time the
  closed loop closes.
- **A rehearsal killed with Ctrl-C leaves its copy behind.** `--replay` removes its
  scratch directory when it exits normally; killed, it cannot. The path is printed
  when the rehearsal starts, and it is under your temp directory as
  `zstab-replay-<name>-<pid>-…`. It is as large as the part of the recording that
  had been copied, so up to a gigabyte or so.
- **`9%` of frames are still skipped** on the reference recording, in the frames
  just before each realignment. They are logged as `skip xy_match_failed`, and a
  much higher rate than that means the field is moving faster than the alignment
  can follow.

## What has been verified, and how

Against the real 13-part, 1.076 GB-per-part, 12,561-frame reference acquisition:

| | |
|---|---|
| block walk vs the file's own index | 10,219 blocks, landing **exactly** on the header's index offset; all 10,218 indexed offsets visited |
| geometry read from the file | 512×512, 2 channels, 10 bits in 16-bit words |
| frames read | 12,561 timepoints × 2 channels, across all 13 parts, in 1 m 40 s |
| part rollover | 12 boundaries, timepoint numbering continuous (`t1019` follows `t1018`) |
| `zPosition` tracking | 9741.19 → 9734.19, matching the file's own per-frame record |
| lateral drift measured, not corrected | +155, −1 px over 29 minutes |
| frames measured | 91% (100% / 90% / 88% / 86% by quarter) |
| corrections proposed | 0 — correctly, since `metric_rel` never left 0.914–1.042 |
| rehearsal rate read from the file | 7.52 Hz against a recorded 7.5 Hz |

Plus 170 unit tests, of which the one that matters most reveals a fixture **one
byte at a time** across two whole timepoints and asserts that no partial block is
ever read as pixels and that every plane arrives exactly once.

---

## Layout

```
build.rs         embeds icon/icon.ico and the version info (Windows only)
src/
  main.rs        CLI, and the poll -> measure -> decide -> act loop
  config.rs      config.yaml as types, with validation up front
  frame.rs       the types that pass between reading, measuring and deciding
  oir/
    mod.rs       the tail-following reader: block walk, part rollover
    meta.rs      the per-frame frameProperties XML
  cv/
    fft.rs       2-D FFT over rustfft, plans kept
    register.rs  phase correlation, and the aligned window
    metrics.rs   the five focus metrics
    zstack.rs    reference-stack matching, for a signed answer
  control.rs     when to move and which way
  actuator.rs    clicking, the autoclicker's mechanism
  replay.rs      --replay
  log.rs         the CSV
```

Tests sit beside each module as `<module>_tests.rs`.

## Provenance

The OIR container layout is as documented in
[FastTIFF](https://github.com/konung-yaropolk/FastTIFF)'s OIR importer, which
determined it from a real acquisition and checked it against the acquisition
software's own TIFF export. The live reader here is a different implementation for a
different problem — that one memory-maps a finished file and starts from its index,
neither of which is possible on a file being written.

The click mechanism is `autoclicker.exe`'s, including its key-name table and its
corner emergency stop, so that recorded sequences are interchangeable between the
two tools.

GPL-3.0-only.
