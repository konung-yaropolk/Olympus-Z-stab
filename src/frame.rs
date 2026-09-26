//! The types that pass between reading, measuring and deciding.
//!
//! A [`Frame`] is one timepoint of one channel, already turned into `f32` and
//! already cropped and downsampled if the config asked for it. Everything
//! downstream of the reader works in `f32` because the metrics and the FFT do;
//! converting once, here, is cheaper than converting in four places.

/// One timepoint, ready to measure.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// Row-major, `width * height` samples. Raw sensor counts, converted to
    /// `f32` but **not** normalised: the metrics that care about scale are
    /// ratios, and the ones that do not are compared only against themselves.
    pub data: Vec<f32>,
    /// The timepoint number as the file names it — `t001` is 1. Not an index
    /// into anything: a part file of a split recording starts again at 1, and
    /// the reader offsets it so the session's numbering keeps rising.
    pub index: u64,
    /// What the file says about this frame.
    pub meta: FrameMeta,
}

/// The fields of one `lsmframe:frameProperties` document that this program uses.
///
/// The file carries far more than this per frame — laser powers, PMT voltages,
/// galvo positions, ROI definitions — and none of the rest is read. What matters
/// here:
///
/// * `z_position` is the stage position the acquisition software **commanded**,
///   not a measurement of where the focal plane is. In the recording this was
///   built against it is the same 9741.19 for all 1018 frames of a part, because
///   nobody touched the stage. So it cannot detect drift — but it is exactly
///   what is needed to confirm that a click *landed*, and to learn how many
///   microns one click is worth.
/// * `created` is the acquisition software's own timestamp for the frame, to the
///   millisecond, and is what the frame interval is measured from rather than
///   the wall clock of the machine watching the file.
/// The namespace prefixes below are the ones the reference acquisition actually
/// uses, which are **not** the ones the container's block names would suggest:
/// the frame's own geometry is under `base:`, not `commonimage:`. That is why
/// [`crate::oir::meta::tag`] matches `:<name>>` and ignores the prefix entirely —
/// a reader that matched qualified names would find nothing at all in this file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameMeta {
    /// `base:name`, e.g. `t001_0_1`.
    pub name: Option<String>,
    /// `base:creationDateTime`, ISO 8601 with offset, as written.
    pub created: Option<String>,
    /// `base:width`.
    pub width: Option<usize>,
    /// `base:height`.
    pub height: Option<usize>,
    /// Bytes per sample: `base:depth`, 2 in every file seen.
    pub depth: Option<usize>,
    /// Bits actually used of those bytes: `base:bitCounts`, 10 in the recordings
    /// this was built against. Saturation is at `2^bits - 1`, not at `u16::MAX`,
    /// and a saturation check that got that wrong would never fire.
    ///
    /// This is the field that makes "first occurrence" load-bearing: the real
    /// document states it three times, once as `base:bitCounts` for the frame and
    /// then once per channel as `commonframe:bitCounts`. Only the first is the
    /// frame's.
    pub bit_counts: Option<u32>,
    /// `lsmframe:zPosition`, microns.
    pub z_position: Option<f64>,
    /// `lsmframe:zBase`, microns — the reference the z position is expressed
    /// against.
    pub z_base: Option<f64>,
    /// `commonparam:shiftXPosition` — a galvo offset, not sample movement.
    pub shift_x: Option<i64>,
    /// `commonparam:shiftYPosition`.
    pub shift_y: Option<i64>,
    /// `commonframe:axisType`: `TIMELAPSE` in a timelapse, `ZSTACK` in a z-stack.
    pub axis_type: Option<String>,
}

impl FrameMeta {
    /// The largest sample value that is not saturated, from `bit_counts`.
    ///
    /// Falls back to the full 16-bit range when the file does not say, which
    /// makes the saturation check inert rather than wrong.
    pub fn full_scale(&self) -> f32 {
        match self.bit_counts {
            Some(b) if (1..=16).contains(&b) => ((1u32 << b) - 1) as f32,
            _ => u16::MAX as f32,
        }
    }

    /// Seconds between two frames, from their own timestamps.
    ///
    /// The timestamps are ISO 8601 with a UTC offset
    /// (`2025-10-07T21:58:59.990-04:00`). Only the difference is ever wanted, so
    /// this parses the fields it needs rather than pulling in a date library for
    /// a subtraction — and it returns `None` rather than a wrong number if the
    /// two fall on different days or offsets, which for a difference of
    /// milliseconds inside one recording does not happen.
    pub fn interval_s(&self, earlier: &FrameMeta) -> Option<f64> {
        let (a, b) = (earlier.created.as_deref()?, self.created.as_deref()?);
        Some(seconds_of_day(b)? - seconds_of_day(a)?)
    }
}

/// Seconds since midnight from the time part of an ISO 8601 timestamp.
fn seconds_of_day(iso: &str) -> Option<f64> {
    let time = iso.split('T').nth(1)?;
    // Stop at the zone offset, which may be `Z`, `+hh:mm` or `-hh:mm`. The `-`
    // case is stripped last and from the right, because a `-` is only an offset
    // when there is one: `21:58:59.990` on its own must survive unchanged.
    let head = time.split(['Z', '+']).next()?;
    let head = match head.rsplit_once('-') {
        Some((before, _)) => before,
        None => head,
    };
    let mut parts = head.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod frame_tests;
