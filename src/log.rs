//! The session log.
//!
//! One row per measured frame, flushed as it is written rather than buffered,
//! because the interesting case is a session that ended badly — the acquisition
//! software crashed, the machine was rebooted, someone pulled the plug on a
//! program that was clicking. A log that was still in a buffer when that happened
//! is a log of nothing.
//!
//! The columns are chosen so the file answers the question a user actually has
//! after a session: *should I have trusted it?* `metric_rel` against
//! `control.dead_band` says whether the thresholds were right; `shift_x` /
//! `shift_y` say how much sideways movement there was to cancel; `z_reported`
//! says whether the clicks landed.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct Logger {
    out: Option<BufWriter<File>>,
    print_every: u64,
    rows: u64,
}

/// One row.
pub struct Row<'a> {
    pub timepoint: u64,
    pub elapsed_s: f64,
    /// The acquisition software's own timestamp for the frame.
    pub timestamp: Option<&'a str>,
    pub shift_x: i32,
    pub shift_y: i32,
    pub peak: f32,
    pub focus: Option<f32>,
    pub metric_rel: Option<f32>,
    pub z_offset_um: Option<f64>,
    pub z_reported: Option<f64>,
    /// `z_reported` minus the stage position at the first frame of the session —
    /// how far the stage has been moved so far. Shown on the console in place of
    /// the absolute position, which is the same six leading digits every row.
    pub z_delta_um: Option<f64>,
    /// The decision, one word plus detail: `hold in_band`, `move +1`, `stop …`.
    pub decision: &'a str,
    pub net_steps: i32,
}

impl Logger {
    /// Open the log. A path that cannot be written is reported and the session
    /// continues without a log — losing the log is bad, losing the stabilisation
    /// of a recording already in progress is worse.
    pub fn open(csv: Option<&Path>, print_every: u64) -> Logger {
        let out = csv.and_then(|p| match File::create(p) {
            Ok(f) => {
                let mut w = BufWriter::new(f);
                let header = "timepoint,elapsed_s,timestamp,shift_x,shift_y,peak,focus,\
                              metric_rel,z_offset_um,z_reported,decision,net_steps\n";
                if let Err(e) = w.write_all(header.as_bytes()) {
                    eprintln!(
                        "could not write {}: {e} — continuing without a log",
                        p.display()
                    );
                    return None;
                }
                let _ = w.flush();
                Some(w)
            }
            Err(e) => {
                eprintln!(
                    "could not create {}: {e} — continuing without a log",
                    p.display()
                );
                None
            }
        });
        Logger {
            out,
            print_every,
            rows: 0,
        }
    }

    /// The column header, printed once before the first row.
    ///
    /// Printing it once is what lets every row drop its labels — `shift`,
    /// `metric`, `rel`, `z` were two thirds of the old line's width and said the
    /// same thing on all 12,561 of them.
    pub fn header() -> &'static str {
        "     t    rel   dx  dy      dz  state"
    }

    /// Write a row, and print it if this is a printing frame or a decision.
    ///
    /// `short` is the console rendering of the decision and `row.decision` the
    /// full one: the console is read over someone's shoulder on a narrow window
    /// beside the acquisition software, and the CSV is the record. Nothing is
    /// lost by abbreviating the first — every field on screen is in the file at
    /// full precision, and several fields that are only ever read afterwards
    /// (the frame's own timestamp, the correlation peak, the absolute metric)
    /// are in the file and not on screen at all.
    pub fn write(&mut self, row: &Row<'_>, short: &str, force_print: bool) {
        self.rows += 1;
        if let Some(w) = self.out.as_mut() {
            let line = format!(
                "{},{:.3},{},{},{},{:.3},{},{},{},{},{},{},{}\n",
                row.timepoint,
                row.elapsed_s,
                row.timestamp.unwrap_or(""),
                row.shift_x,
                row.shift_y,
                row.peak,
                fmt_f32(row.focus),
                fmt_f32(row.metric_rel),
                fmt_f64(row.z_offset_um),
                fmt_f64(row.z_reported),
                fmt_f64(row.z_delta_um),
                row.decision,
                row.net_steps,
            );
            if w.write_all(line.as_bytes()).is_ok() {
                // Per row, deliberately. See the module note.
                let _ = w.flush();
            }
        }
        let periodic = self.print_every > 0 && self.rows % self.print_every == 0;
        if force_print || periodic {
            println!(
                "{:>6} {:>6} {:>+4}{:>+4} {:>7}  {}",
                row.timepoint,
                row.metric_rel
                    .map(|v| format!("{v:.3}"))
                    .unwrap_or_else(|| "-".into()),
                row.shift_x,
                row.shift_y,
                // Microns from where the recording started, not the absolute
                // stage position: on this line the question is always "how far
                // has it been moved", and 9741.19 spends six characters saying
                // the same thing on every row. The absolute is in the CSV.
                row.z_delta_um
                    .map(|v| format!("{v:+.2}"))
                    .unwrap_or_else(|| "-".into()),
                short,
            );
        }
    }
}

fn fmt_f32(v: Option<f32>) -> String {
    v.map(|v| format!("{:.6}", v)).unwrap_or_default()
}

fn fmt_f64(v: Option<f64>) -> String {
    v.map(|v| format!("{:.2}", v)).unwrap_or_default()
}
