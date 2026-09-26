//! Z-plane stabilisation for a FluoView acquisition that is still being recorded.
//!
//! Point it at the `.oir` the acquisition software is writing. It follows the
//! file as it grows, measures how much sharper the start of the recording was
//! than the last few seconds of it, and — when it is armed — clicks the
//! acquisition software's own Z controls to put the focus back.
//!
//! ```text
//!   olympus-z-stab                     ask for the file, then start
//!   olympus-z-stab path\to\rec.oir     start on that file
//!   olympus-z-stab rec.oir --arm       ... and actually click
//!   olympus-z-stab --where             print the mouse position, to fill in config
//!   olympus-z-stab rec.oir --replay    rehearse on a finished recording
//! ```
//!
//! # The shape of the loop
//!
//! Poll the file. For every timepoint that has finished arriving: cancel its X/Y
//! movement against the reference, measure focus on the part that overlaps, fold
//! that into the moving average, and ask the controller what to do. Log the row.
//! Sleep. Again.
//!
//! The X/Y movement is measured and logged but **never corrected** — the
//! experimenter chose that field of view. It is cancelled only so that the focus
//! measurement compares the same tissue to itself.

mod actuator;
mod config;
mod control;
mod cv;
mod frame;
mod log;
mod oir;
mod replay;

use actuator::{Actuator, Applied};
use config::{Config, Mode};
use control::{Controller, Decision, Hold, Observation};
use cv::{aligned_window, FocusMeter, Registrar, Shift};
use frame::FrameMeta;
use oir::{LiveReader, Plane};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(test)]
#[path = "main_tests.rs"]
mod main_tests;

fn main() {
    let code = match real_main() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("\nerror: {e}");
            1
        }
    };
    // Started by dropping a file on it, this runs in a console window that
    // Explorer closes the moment the process exits — so a message nobody can read
    // is no message at all.
    if std::env::args().len() <= 1 {
        eprint!("\nPress ENTER to close... ");
        let _ = std::io::stderr().flush();
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    std::process::exit(code);
}

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "--where") {
        actuator::show_mouse_position();
        return Ok(());
    }

    let config_path = flag_value(&args, "--config")
        .map(PathBuf::from)
        .or_else(Config::default_path);
    let mut cfg = match &config_path {
        Some(p) => Config::load(p)?,
        None => {
            println!("No config.yaml found — using built-in defaults, and not armed.");
            println!("Write one out with --write-config to change anything.\n");
            Config::default()
        }
    };
    if args.iter().any(|a| a == "--arm") {
        cfg.actuator.arm = true;
    }
    if args.iter().any(|a| a == "--dry-run") {
        cfg.actuator.arm = false;
    }

    let problems = cfg.validate();
    if !problems.is_empty() {
        let mut msg = String::from("the configuration is not usable:\n");
        for p in &problems {
            msg.push_str(&format!("  - {p}\n"));
        }
        return Err(msg);
    }

    let path = match args.iter().find(|a| !a.starts_with('-')) {
        Some(p) => PathBuf::from(clean_path(p)),
        None => ask_for_path()?,
    };
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }

    // Rehearsal: copy a finished recording into a scratch file a piece at a time
    // and follow *that*, so the whole pipeline can be exercised on real data with
    // no microscope. The most useful thing available for checking thresholds
    // before a session.
    let (path, _replay) = if args.iter().any(|a| a == "--replay") {
        let r = replay::Replay::start(&path, &cfg)?;
        let p = r.output().to_path_buf();
        println!(
            "Rehearsing on a copy of a finished recording: {}",
            p.display()
        );
        (p, Some(r))
    } else {
        (path, None)
    };

    banner(&cfg, &path, config_path.as_deref());

    if cfg.actuator.arm {
        println!("ARMED — this will move the stage by clicking.");
        println!("Emergency stop: put the mouse pointer in the UPPER-LEFT corner.");
        print!("Press ENTER to start, or Ctrl-C to abort... ");
        let _ = std::io::stdout().flush();
        let _ = std::io::stdin().read_line(&mut String::new());
    }

    run(&path, &cfg)
}

/// The session.
fn run(path: &std::path::Path, cfg: &Config) -> Result<(), String> {
    let mut reader = LiveReader::open(path, cfg)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    let mut actuator = Actuator::new(cfg);
    let mut controller = Controller::new(cfg);
    let mut logger = log::Logger::open(cfg.log.csv.as_deref(), cfg.log.print_every);

    // Built once the file has said how big a frame is, which it does in the
    // metadata block that precedes the first frame's pixels.
    let mut registrar: Option<Registrar> = None;
    let mut meter: Option<FocusMeter> = None;
    let mut zstack: Option<cv::zstack::ZStack> = None;
    let mut margin = 0usize;

    // Planes waiting for the rest of their timepoint.
    let mut pending: BTreeMap<u64, Vec<Plane>> = BTreeMap::new();
    let mut first_meta: Option<FrameMeta> = None;
    let mut reference_taken = false;
    let mut frames_seen: u64 = 0;
    // The alignment reference goes stale as the sample changes, and a stale one
    // stops matching entirely rather than matching worse. These two track when to
    // replace it, and what the replacement is worth: `xy_base` accumulates the
    // shift the outgoing reference had drifted by, so the logged shift stays a
    // total since the start of the session rather than resetting each time.
    let mut xy_failures = 0usize;
    let mut xy_base = (0i32, 0i32);
    let mut last_good = Shift::default();
    let mut xy_refreshes = 0u32;
    let mut reference_announced = false;
    let mut warned_refusal = false;
    let mut adjustments: Vec<Adjustment> = Vec::new();
    // The stage position at the first frame that reported one, so the console can
    // show how far it has been moved instead of where it is.
    let mut z_start: Option<f64> = None;

    println!("Following the acquisition. Ctrl-C to stop.\n");

    let poll_wait = Duration::from_millis(cfg.input.poll_interval_ms);
    loop {
        if cfg.actuator.arm && actuator.check_stop() {
            println!("\nEmergency stop: pointer in the corner.");
            break;
        }

        let poll = reader
            .poll()
            .map_err(|e| format!("reading {}: {e}", reader.part_path().display()))?;

        if let Some(next) = &poll.rolled_over {
            // The file name only. The folder is in the banner, and repeating it
            // once per part is what would set the width of the whole window.
            println!(
                "  [part: {}]",
                next.file_name().unwrap_or_default().to_string_lossy()
            );
        }

        for plane in poll.planes {
            pending.entry(plane.timepoint).or_default().push(plane);
        }

        // A timepoint is done when a later one has started, or when the recording
        // has. Waiting for the next timepoint costs one frame of latency — 133 ms
        // at 7.5 Hz — and avoids having to know how many channels there are.
        let newest = pending.keys().next_back().copied().unwrap_or(0);
        let ready: Vec<u64> = pending
            .keys()
            .copied()
            .filter(|&t| t < newest || poll.finished)
            .collect();

        for t in ready {
            let planes = match pending.remove(&t) {
                Some(p) => p,
                None => continue,
            };
            let Some(geom) = reader.geometry() else {
                continue;
            };
            let Some(f) = oir::to_frame(&planes, geom, cfg) else {
                continue;
            };

            // Everything that depends on the frame size, built the first time a
            // frame size is known.
            if meter.is_none() {
                margin = cfg.measure.registration.max_shift_px;
                if f.width <= 2 * margin || f.height <= 2 * margin {
                    return Err(format!(
                        "measure.registration.max_shift_px is {margin}, which leaves no window \
                         inside a {}x{} frame",
                        f.width, f.height
                    ));
                }
                registrar = Some(Registrar::new(
                    f.width,
                    f.height,
                    cfg.measure.registration.taper,
                    margin,
                    cfg.measure.registration.min_peak,
                ));
                meter = Some(FocusMeter::new(
                    cfg.measure.metric,
                    f.width - 2 * margin,
                    f.height - 2 * margin,
                    &cfg.measure.high_freq,
                ));
                if cfg.control.mode == Mode::ReferenceStack {
                    zstack = Some(cv::zstack::ZStack::load(cfg, geom)?);
                    println!(
                        "  reference stack: {} planes at {} um",
                        zstack.as_ref().map(|z| z.planes.len()).unwrap_or(0),
                        cfg.control.reference_stack.step_um
                    );
                }
                println!(
                    "  {}x{}, {} ch -> {}x{} window, ch {:?}\n",
                    f.width,
                    f.height,
                    // Worth stating: it is how the user finds out that the
                    // channel they named in the config is not in this recording.
                    reader.channels().len(),
                    f.width - 2 * margin,
                    f.height - 2 * margin,
                    cfg.input.channel,
                );
                // Once, so every row below can drop its labels. `dz` is microns
                // from the stage position this session started at.
                println!("{}", log::Logger::header());
            }

            let Some(f) = oir::to_frame(&planes, geom, cfg) else {
                continue;
            };
            frames_seen += 1;
            if first_meta.is_none() {
                first_meta = Some(f.meta.clone());
            }
            if z_start.is_none() {
                z_start = f.meta.z_position;
            }
            let elapsed_s = first_meta
                .as_ref()
                .and_then(|m| f.meta.interval_s(m))
                .unwrap_or(0.0);

            let reg = registrar.as_mut().expect("built above");
            let met = meter.as_mut().expect("built above");

            // The first usable frame is the reference for the X/Y cancellation.
            // It is not the focus reference: that is an average of many frames,
            // built by the controller.
            if !reference_taken {
                reg.set_reference(&f.data);
                reference_taken = true;
            }

            let shift = if cfg.measure.registration.enabled {
                reg.shift(&f.data)
            } else {
                Shift {
                    dy: 0,
                    dx: 0,
                    peak: 0.0,
                    trusted: true,
                }
            };

            // A stale reference does not degrade gracefully: it stops matching at
            // all. So repeated failure is read as "the reference is no longer of
            // this sample" rather than as a run of bad frames.
            if shift.trusted {
                xy_failures = 0;
                last_good = shift;
            } else {
                xy_failures += 1;
                if xy_failures >= cfg.measure.registration.refresh_after {
                    reg.set_reference(&f.data);
                    xy_base.0 += last_good.dx;
                    xy_base.1 += last_good.dy;
                    last_good = Shift::default();
                    xy_failures = 0;
                    xy_refreshes += 1;
                    // Terse on purpose: the meaning is in the README, and this
                    // happens nine times in half an hour on a real recording.
                    println!("  [realign t{}]", f.index);
                }
            }

            let mut focus = None;
            let mut z_offset_um = None;
            let mut rejected = None;
            if !shift.trusted {
                rejected = Some("xy_match_failed");
            } else if let Some((win, _w, _h)) =
                aligned_window(&f.data, f.width, f.height, shift, margin)
            {
                if FocusMeter::saturated(&win, f.meta.full_scale(), cfg.measure.saturated_fraction)
                {
                    rejected = Some("saturated");
                } else {
                    focus = Some(met.measure(&win));
                    if let Some(zs) = zstack.as_ref() {
                        z_offset_um = zs
                            .locate(&win, cfg.control.reference_stack.min_score)
                            .map(|m| m.offset_um);
                    }
                }
            } else {
                rejected = Some("shift_beyond_margin");
            }

            let obs = Observation {
                timepoint: f.index,
                focus,
                z_offset_um,
                z_reported: f.meta.z_position,
                shift,
                elapsed_s,
            };
            let decision = controller.observe(&obs);

            let mut note = describe(&decision, rejected);
            let short = describe_short(&decision, rejected);
            let mut stop_after = None;

            // The stage position the file reported for the frame *after* a
            // correction is what says how far it really went, so a pending
            // adjustment is completed here rather than when it was decided.
            if let Some(z) = f.meta.z_position {
                if let Some(a) = adjustments.last_mut() {
                    if a.z_after.is_none() && a.timepoint < f.index {
                        a.z_after = Some(z);
                    }
                }
            }

            if let Decision::Move { steps, .. } = &decision {
                let target_z = f
                    .meta
                    .z_position
                    .zip(cfg.actuator.um_per_step.or(controller.stats().um_per_step))
                    .map(|(z, um)| z + *steps as f64 * um);
                let outcome = actuator.apply(*steps, target_z);
                // Recorded whether or not it happened. On a dry run these are the
                // entire point of the session.
                adjustments.push(Adjustment {
                    timepoint: f.index,
                    elapsed_s,
                    steps: *steps,
                    applied: matches!(outcome, Applied::Done { .. }),
                    z_before: f.meta.z_position,
                    z_after: None,
                });
                match outcome {
                    Applied::Done { steps } => {
                        controller.applied(steps, elapsed_s);
                        note = format!("move {steps:+} applied");
                    }
                    Applied::DryRun { steps } => {
                        // Not applied, so the controller must not judge the next
                        // window as though the stage had moved.
                        note = format!("move {steps:+} WOULD apply (not armed)");
                    }
                    Applied::Stopped => {
                        note = "stopped by user".into();
                        stop_after = Some("emergency stop".to_string());
                    }
                    Applied::Refused(why) => {
                        note = format!("move refused: {why}");
                        if cfg.actuator.arm {
                            // Armed and unable to click is fatal: the session
                            // cannot do the job it was started for.
                            stop_after = Some(format!("the actuator refused to move: {why}"));
                        } else if !warned_refusal {
                            // Unarmed, it is not. A first session on a new rig has
                            // no coordinates yet, and stopping at the first would-be
                            // correction would show exactly one of them — when the
                            // whole reason to run unarmed is to see all of them.
                            warned_refusal = true;
                            // Trimmed: the config key is the actionable part.
                            let key = why.split(':').next().unwrap_or(&why);
                            println!("  [{key}]");
                            println!("  [unarmed, so carrying on]");
                        }
                    }
                }
            }
            if let Decision::Stop(why) = &decision {
                stop_after = Some(why.clone());
            }

            // Said once, when the starting position has been established. Until
            // then every decision is a hold, and the user has no way to tell a
            // program that is still warming up from one that is stuck.
            if let (false, Some(r)) = (reference_announced, controller.reference()) {
                reference_announced = true;
                println!("  [reference set: {r:.4}]");
            }

            let is_event = !matches!(decision, Decision::Hold(_));
            logger.write(
                &log::Row {
                    timepoint: f.index,
                    elapsed_s,
                    timestamp: f.meta.created.as_deref(),
                    // Totals since the session started, across every realignment.
                    shift_x: shift.dx + xy_base.0,
                    shift_y: shift.dy + xy_base.1,
                    peak: shift.peak,
                    focus,
                    metric_rel: controller.metric_rel(),
                    z_offset_um,
                    z_reported: f.meta.z_position,
                    z_delta_um: f.meta.z_position.zip(z_start).map(|(z, s)| z - s),
                    decision: &note,
                    net_steps: controller.stats().net_steps,
                },
                &short,
                is_event,
            );

            if let Some(why) = stop_after {
                summary(&controller, frames_seen, xy_refreshes, &adjustments, cfg);
                println!("\nStopped: {why}");
                return Ok(());
            }
        }

        if poll.finished {
            println!("\nThe acquisition has finished.");
            break;
        }
        // `more` means the reader handed back a bounded batch and is holding the
        // rest — which happens when this is pointed at a recording that is
        // already complete, or at a replay that has run ahead. Sleeping a poll
        // interval per batch would then take minutes to catch up on a file that
        // is entirely on disk already.
        if !poll.more {
            std::thread::sleep(poll_wait);
        }
    }

    summary(&controller, frames_seen, xy_refreshes, &adjustments, cfg);
    Ok(())
}

/// One word plus detail for the log's `decision` column.
fn describe(d: &Decision, rejected: Option<&'static str>) -> String {
    if let Some(r) = rejected {
        return format!("skip {r}");
    }
    match d {
        Decision::Hold(h) => match h {
            Hold::BuildingReference { have, need } => format!("hold reference {have}/{need}"),
            Hold::BuildingWindow { have, need } => format!("hold window {have}/{need}"),
            Hold::InBand { .. } => "hold in_band".into(),
            Hold::Confirming { windows, need } => format!("hold confirming {windows}/{need}"),
            Hold::Cooldown { remaining_s } => format!("hold cooldown {remaining_s:.0}s"),
            Hold::FrameRejected(r) => format!("skip {r}"),
            Hold::Verifying => "hold verifying".into(),
        },
        Decision::Move { steps, reason } => format!("move {steps:+} {reason}"),
        Decision::Stop(why) => format!("stop {why}"),
    }
}

/// The same decision for the console, in as few characters as it can be said.
///
/// The long form goes to the CSV. This one has to sit in the last column of a
/// line narrow enough for a window squeezed beside the acquisition software, and
/// the states it shows most often — waiting for the window to fill, sitting
/// inside the dead band — are exactly the ones that do not need words.
fn describe_short(d: &Decision, rejected: Option<&'static str>) -> String {
    if let Some(r) = rejected {
        return short_skip(r);
    }
    match d {
        Decision::Hold(h) => match h {
            Hold::BuildingReference { have, need } => format!("ref {have}/{need}"),
            Hold::BuildingWindow { have, need } => format!("win {have}/{need}"),
            Hold::InBand { .. } => "ok".into(),
            Hold::Confirming { windows, need } => format!("CONF {windows}/{need}"),
            Hold::Cooldown { remaining_s } => format!("cool {remaining_s:.0}s"),
            Hold::FrameRejected(r) => short_skip(r),
            Hold::Verifying => "verify".into(),
        },
        // Upper case for the two that matter, so a correction is findable by eye
        // in a screen of `ok`.
        Decision::Move { steps, .. } => format!("MOVE {steps:+}"),
        Decision::Stop(_) => "STOP".into(),
    }
}

fn short_skip(reason: &str) -> String {
    match reason {
        "xy_match_failed" => "skip xy".into(),
        "saturated" => "skip sat".into(),
        "shift_beyond_margin" => "skip far".into(),
        // A reason added later and not given a short spelling here. Truncated
        // rather than left to run past the column and wrap every row it appears
        // on — the full text is in the CSV either way.
        other => format!("skip {:.6}", other),
    }
}

/// One time the program decided the focus needed moving.
///
/// Recorded whether or not it was carried out: on a dry run these are the entire
/// point — they are what the user is running the session to find out.
struct Adjustment {
    timepoint: u64,
    elapsed_s: f64,
    steps: i32,
    /// False on a dry run, and false when the actuator refused or was stopped.
    applied: bool,
    z_before: Option<f64>,
    /// Filled from the next frame that reports a position, so the movement is
    /// the one the file recorded rather than the one that was asked for.
    z_after: Option<f64>,
}

impl Adjustment {
    /// What the stage actually did, in microns, when the file says.
    fn moved_um(&self) -> Option<f64> {
        Some(self.z_after? - self.z_before?)
    }
}

/// `up` or `down`, named after the sequence that would run.
fn direction(steps: i32) -> &'static str {
    if steps >= 0 {
        "up"
    } else {
        "down"
    }
}

fn summary(c: &Controller, frames: u64, refreshes: u32, adjustments: &[Adjustment], cfg: &Config) {
    let s = c.stats();
    println!("\n{}", "-".repeat(46));
    println!("  frames measured  {frames}");
    if refreshes > 0 {
        // A handful is normal over half an hour; hundreds means the field is not
        // holding still enough to measure.
        println!("  realignments     {refreshes}");
    }
    if let Some(um) = s.um_per_step {
        println!("  learned step     {um:.3} um");
    }
    if let Some(p) = &cfg.log.csv {
        println!("  log              {}", p.display());
    }

    // ---- what happened to the focus, which is the question being asked -----
    println!();
    if adjustments.is_empty() {
        println!("  Z NOT ADJUSTED — the focus never left the dead band.");
        return;
    }

    let net: i32 = adjustments.iter().map(|a| a.steps).sum();
    let total: i32 = adjustments.iter().map(|a| a.steps.abs()).sum();
    // The movement the file recorded, which beats the movement that was asked
    // for: it is what the stage did rather than what was requested of it.
    //
    // Only over corrections that were actually carried out. On a dry run the z
    // either side of a would-be correction still moves — because the operator
    // moved it — and adding that up would report the program's own effect as
    // whatever a human happened to do.
    let applied_moves = adjustments.iter().filter(|a| a.applied);
    let measured: f64 = applied_moves.clone().filter_map(|a| a.moved_um()).sum();
    let any_measured = applied_moves.map(|a| a.moved_um()).any(|m| m.is_some());
    let um = if any_measured {
        Some(measured)
    } else {
        s.um_per_step.map(|u| net as f64 * u)
    };

    let armed = adjustments.iter().any(|a| a.applied);
    if armed {
        println!("  Z MOVED {}", direction(net).to_uppercase());
    } else {
        println!("  Z NOT ADJUSTED (dry run)");
        println!("  It would have moved {}.", direction(net));
    }
    println!(
        "  {} correction(s), {:+} step(s) net{}",
        adjustments.len(),
        net,
        match um {
            Some(u) if any_measured => format!(", {u:+.2} um"),
            Some(u) => format!(", ~{u:+.2} um"),
            None => String::new(),
        }
    );
    if !armed {
        // Without a correction actually happening the drift never goes away, so
        // the controller proposes it again after every cooldown. The count is
        // "times it would have acted", not a distance an armed run would travel.
        println!("  The drift was never corrected, so it came back");
        println!("  and was proposed again after every cooldown:");
        println!("  that is how often it would have acted, not how");
        println!("  far an armed run would have moved.");
    }
    if net != total {
        // Steps in both directions: the hill climb guessed wrong at least once,
        // which is normal but worth being able to see.
        println!("  {total} step(s) in total, so some were reversed.");
    }

    println!();
    println!("      t      at  steps    z moved");
    // The console shows the most recent handful; every one of them is in the CSV.
    const SHOWN: usize = 20;
    let skipped = adjustments.len().saturating_sub(SHOWN);
    if skipped > 0 {
        println!("  ... {skipped} earlier, see the log");
    }
    for a in adjustments.iter().skip(skipped) {
        let z = match (a.applied, a.z_before, a.z_after) {
            (true, Some(b), Some(x)) => format!("{b:.2} -> {x:.2}  {:+.2}", x - b),
            (true, Some(b), None) => format!("{b:.2} -> ?"),
            // Nothing was carried out, so there is no movement of ours to show —
            // only where the stage was when the decision was taken.
            (false, Some(b), _) => format!("{b:.2}"),
            _ => "-".into(),
        };
        println!(
            "  {:>5} {:>6.0}s  {:>+5}{}  {}",
            a.timepoint,
            a.elapsed_s,
            a.steps,
            if a.applied { " " } else { "*" },
            z
        );
    }
    if !armed {
        println!("\n  * not carried out: not armed.");
        println!("  Read the log, then run again with --arm.");
    } else if adjustments.iter().any(|a| !a.applied) {
        println!("\n  * decided but not carried out: the");
        println!("  actuator refused, or was stopped.");
    }
}

fn banner(cfg: &Config, path: &std::path::Path, config_path: Option<&std::path::Path>) {
    println!("Olympus Z-plane stabiliser {}", env!("CARGO_PKG_VERSION"));
    println!("{}", "=".repeat(46));
    // The file name, then the folder under it: a full acquisition path is 110
    // characters and would set the width of a window that every other line now
    // fits in 36.
    println!(
        "  file    {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    if let Some(dir) = path.parent() {
        println!("  in      {}", dir.display());
    }
    if let Some(c) = config_path {
        println!(
            "  config  {}",
            c.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    println!("  mode    {:?}, {:?}", cfg.control.mode, cfg.measure.metric);
    println!(
        "  window  {} frames vs first {} (skip {})",
        cfg.measure.window_frames, cfg.measure.reference_frames, cfg.measure.reference_skip
    );
    // Stated as a threshold on `rel`, which is the column printed on every row,
    // and not only as a percentage. `dead_band` is a *fraction*, so someone
    // thinking in percent writes 0.1 for "nought point one percent" and gets a
    // ten percent band that never fires — this line is what makes that visible
    // before the session rather than after it. The precision adapts so that a
    // genuine 0.001 reads as 0.1% rather than being rounded to 0%.
    let pct = cfg.control.dead_band * 100.0;
    let places = if pct < 1.0 { 2 } else { 1 };
    println!(
        "  band    {pct:.places$}% — acts when rel < {:.3}",
        1.0 - cfg.control.dead_band,
        places = places
    );
    println!(
        "  confirm {} window(s), {}s cooldown",
        cfg.control.confirm_windows, cfg.control.cooldown_s
    );
    println!(
        "  limits  {}/event, {}/session",
        cfg.control.max_steps_per_event, cfg.control.max_total_steps
    );
    println!(
        "  clicks  {}",
        if cfg.actuator.arm {
            "ARMED"
        } else {
            "dry run, nothing will be clicked"
        }
    );
    println!("{}\n", "=".repeat(46));
}

fn ask_for_path() -> Result<PathBuf, String> {
    println!("Olympus Z-plane stabiliser {}", env!("CARGO_PKG_VERSION"));
    println!("{}\n", "=".repeat(46));
    println!("Drag the .oir file that is being recorded into this window,");
    print!("or type its path, then press ENTER: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("could not read the path: {e}"))?;
    let p = clean_path(&line);
    if p.is_empty() {
        return Err("no file given".into());
    }
    Ok(PathBuf::from(p))
}

/// A path as a shell or Explorer hands it over: trailing newline, and wrapped in
/// quotes when it contains a space — which the real recordings' paths do.
fn clean_path(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).filter(|v| !v.starts_with('-')).cloned()
}

fn print_help() {
    println!(
        "\
Olympus Z-plane stabiliser {}

Follows an .oir while the acquisition software is writing it, measures focus
drift against the start of the recording, and corrects it by clicking the
acquisition software's own Z controls.

    olympus-z-stab [<file.oir>] [options]

With no file, it asks for one — drag the file into the window and press ENTER.

Options
    --arm               Actually click. Without this nothing touches the mouse.
    --dry-run           Never click, whatever config.yaml says.
    --config <path>     Settings file. Default: config.yaml beside the exe.
    --where             Print the mouse position, to fill in click coordinates.
    --replay            Rehearse: follow a growing copy of a finished recording.
    -h, --help          This.

Emergency stop: put the mouse pointer in the UPPER-LEFT corner of the screen.
",
        env!("CARGO_PKG_VERSION")
    );
}
