//! Reading the drift off a reference z-stack, with a sign.
//!
//! A focus metric says *that* the plane moved, never which way. The hill-climb
//! controller finds the direction by trying one — which costs a wrong step per
//! event. This is the other answer: record a z-stack over the field of view
//! before the timelapse, and the shape of the sample against z becomes a lookup
//! table. Correlate the current window against every plane of the stack, and the
//! plane that matches best says where the focus is, signed, in microns.
//!
//! # What it takes to be right
//!
//! * **The stack must be of this field of view**, recorded just before the
//!   timelapse. A stack from a different field correlates best with whichever
//!   plane happens to share its brightness, and would be confidently wrong —
//!   hence `min_score`, below which the program holds rather than acting.
//! * **The stack's planes are registered to each other in X/Y** before use, for
//!   the same reason every other comparison here is: a stack acquired while the
//!   sample drifted sideways would otherwise encode that drift as a z signal.
//! * **The match is interpolated.** A stack stepped at 1 µm cannot resolve
//!   better than 1 µm by picking a plane, so a parabola is fitted through the
//!   best plane and its two neighbours. That is what turns a 1 µm stack into a
//!   roughly 0.2 µm reading, and it is why the stack does not need to be fine.
//!
//! The stack is read with the same [`crate::oir`] reader, which is why that
//! reader has to understand the `z` axis as well as `t`: a z-stack's planes are
//! named `z001_…`, and a reader that only knew about timelapses would find
//! nothing in one.
//!
//! # How the stack's pixels are kept in step with the live window's
//!
//! This is the failure this module is most likely to have, and it is silent: a
//! dot product between two windows that were cropped differently is a number, it
//! is just not a correlation. Nothing downstream can tell.
//!
//! So the stack does not reimplement the crop. [`ZStack::load`] runs each plane
//! through [`crate::oir::to_frame`] — the very function the live loop uses, with
//! the same [`Config`] and the same [`Geometry`] — and then insets by
//! `measure.registration.max_shift_px`, which is the same `margin` the live loop
//! passes to [`crate::cv::aligned_window`]. Channel choice, ROI, downsampling
//! stride and inset therefore cannot drift apart without the live path changing
//! too. The stack's own geometry is checked against the recording's first, because
//! a crop that is *identical* is only useful if the frames it starts from are the
//! same size.
//!
//! The one thing that is deliberately not shared is the X/Y registration
//! reference: live frames are registered against the first frame of the
//! recording, and the stack's planes against the stack's own centre plane. They
//! do not need to agree, because a whole-stack X/Y offset relative to the
//! timelapse shifts every plane's score equally and so cannot move the peak.

use crate::config::Config;
use crate::cv::{aligned_window, Registrar, Shift};
use crate::oir::{self, Geometry, LiveReader, Plane};
use std::collections::BTreeMap;

/// A stack this small cannot be interpolated and probably is not a stack at all
/// — one plane has no neighbours to fit a parabola through, and two have no
/// middle.
const MIN_PLANES: usize = 3;

/// Polls allowed while reading a stack that is already on disk.
///
/// A finished file is normally drained in one or two polls; the bound only exists
/// so that a reader which somehow stops making progress without saying so ends as
/// an error rather than as a hang before a recording.
const MAX_POLLS: usize = 100_000;

/// One plane of the reference stack, already registered, cropped and normalised.
pub struct StackPlane {
    /// Microns relative to the stack's centre plane; negative is below.
    pub offset_um: f64,
    /// Zero-mean, unit-norm samples, so a correlation against it is a dot
    /// product. Normalising once here rather than per comparison is what makes
    /// matching a 40-plane stack per window affordable.
    pub normalised: Vec<f32>,
}

/// A reference z-stack.
pub struct ZStack {
    pub width: usize,
    pub height: usize,
    pub step_um: f64,
    pub planes: Vec<StackPlane>,
}

/// Where the current window sits in the stack.
#[derive(Debug, Clone, Copy)]
pub struct ZMatch {
    /// Signed microns from the stack's centre. Positive means the focal plane has
    /// moved in the direction of increasing z in the stack.
    pub offset_um: f64,
    /// Index of the best-matching plane, before interpolation.
    pub best_plane: usize,
    /// Normalised cross-correlation with that plane, `-1..1`.
    pub score: f32,
}

impl ZStack {
    /// Read a z-stack acquisition and prepare it for matching.
    ///
    /// The window it will be matched against is `geom`-shaped after the ROI and
    /// downsampling in `cfg` — so the stack has to be cropped and downsampled the
    /// same way, or the dot products compare different pixels. That is the one
    /// thing most likely to be got wrong here.
    ///
    /// The stack is **not** required to be finished-and-indexed: it is read with
    /// the same tail-following reader, which reads a complete file perfectly well
    /// and does not depend on the index.
    pub fn load(cfg: &Config, geom: Geometry) -> Result<ZStack, String> {
        let path = cfg
            .control
            .reference_stack
            .path
            .as_ref()
            .ok_or_else(|| "control.reference_stack.path: no stack to load".to_string())?;

        let mut reader = LiveReader::open(path, cfg)
            .map_err(|e| format!("could not open the reference stack {}: {e}", path.display()))?;

        // The stack is a file that has finished being written, but it is read with
        // the live reader, whose `finished` flag only arrives after
        // `input.idle_timeout_s` of silence — a minute of staring at a complete
        // file before a recording starts. So the drain ends at the first poll that
        // makes no progress of any kind instead: no plane, no cursor movement, no
        // roll-over. On a complete file that means everything has been read, and
        // on a stack that is somehow still being written it means waiting is
        // pointless anyway.
        let mut by_index: BTreeMap<u64, Vec<Plane>> = BTreeMap::new();
        let mut polls = 0usize;
        loop {
            let before = (reader.parts_opened(), reader.cursor());
            let poll = reader
                .poll()
                .map_err(|e| format!("reading the reference stack {}: {e}", path.display()))?;
            let got = poll.planes.len();
            for plane in poll.planes {
                by_index.entry(plane.timepoint).or_default().push(plane);
            }
            if poll.finished {
                break;
            }
            let after = (reader.parts_opened(), reader.cursor());
            if got == 0 && poll.rolled_over.is_none() && after == before {
                break;
            }
            polls += 1;
            if polls >= MAX_POLLS {
                return Err(format!(
                    "the reference stack {} never stopped producing planes",
                    path.display()
                ));
            }
        }

        let stack_geom = reader.geometry().ok_or_else(|| {
            format!(
                "the reference stack {} has no frame properties in it — is it an OIR?",
                path.display()
            )
        })?;
        // Not a warning. A stack of a different frame size cannot be compared
        // pixel for pixel with the recording however it is cropped, and the
        // failure would look like a plausible z reading rather than like an error.
        if stack_geom != geom {
            return Err(format!(
                "the reference stack {} is {}x{}x{}B but the recording is {}x{}x{}B: a stack of a \
                 different frame size cannot be correlated against it",
                path.display(),
                stack_geom.width,
                stack_geom.height,
                stack_geom.depth,
                geom.width,
                geom.height,
                geom.depth
            ));
        }

        // `geom` rather than `stack_geom` — they are equal, and passing the
        // recording's own geometry makes it impossible for this crop to diverge
        // from the live one even if the equality test above is ever relaxed.
        let frames: Vec<crate::frame::Frame> = by_index
            .into_iter()
            .filter_map(|(_, planes)| oir::to_frame(&planes, geom, cfg))
            .collect();

        if frames.len() < MIN_PLANES {
            return Err(format!(
                "the reference stack {} has {} usable plane(s); at least {MIN_PLANES} are needed \
                 to interpolate a position in it",
                path.display(),
                frames.len()
            ));
        }

        let margin = cfg.measure.registration.max_shift_px;
        let (fw, fh) = (frames[0].width, frames[0].height);
        if fw <= 2 * margin || fh <= 2 * margin {
            return Err(format!(
                "the reference stack's frames are {fw}x{fh} after cropping, which leaves no window \
                 inside a margin of {margin}"
            ));
        }
        // Any plane of a different size would index into the wrong rows. It cannot
        // happen when they all came from one file through one crop, so this is a
        // guard against a future change rather than against the data.
        if frames.iter().any(|f| f.width != fw || f.height != fh) {
            return Err(format!(
                "the reference stack {} does not have one frame size throughout",
                path.display()
            ));
        }

        let mut reg = Registrar::new(
            fw,
            fh,
            cfg.measure.registration.taper,
            margin,
            cfg.measure.registration.min_peak,
        );
        // The centre plane is the registration reference because it is the one the
        // live window will sit nearest: the planes furthest from it are the most
        // defocused, and a phase correlation between two heavily defocused planes
        // at opposite ends of the stack is the weakest match in it.
        let centre = frames.len() / 2;
        reg.set_reference(&frames[centre].data);

        // Zero is the middle of the stack, which for an even number of planes lies
        // between two of them — hence `(n - 1) / 2` as a float rather than the
        // integer `centre` used above. Registration reference and offset origin
        // are independent: shifting every plane in X/Y cannot move the z peak.
        let mid = (frames.len() as f64 - 1.0) / 2.0;
        let step_um = cfg.control.reference_stack.step_um;

        let mut planes = Vec::with_capacity(frames.len());
        for (i, f) in frames.iter().enumerate() {
            let shift = if cfg.measure.registration.enabled && i != centre {
                let s = reg.shift(&f.data);
                // A plane whose X/Y match failed is kept unshifted rather than
                // dropped. Dropping it would renumber every plane above it and so
                // corrupt the microns-per-plane mapping for the whole stack, which
                // is far worse than one plane of the table being a little off.
                if s.trusted {
                    s
                } else {
                    Shift {
                        dy: 0,
                        dx: 0,
                        peak: s.peak,
                        trusted: true,
                    }
                }
            } else {
                Shift {
                    dy: 0,
                    dx: 0,
                    peak: 0.0,
                    trusted: true,
                }
            };

            let win = match aligned_window(&f.data, f.width, f.height, shift, margin) {
                Some((w, _, _)) => w,
                // The registrar never reports a shift beyond `max_shift`, which is
                // this same margin, so this is unreachable on its output; falling
                // back to the unshifted window keeps the plane in the table.
                None => match aligned_window(
                    &f.data,
                    f.width,
                    f.height,
                    Shift {
                        dy: 0,
                        dx: 0,
                        peak: 0.0,
                        trusted: true,
                    },
                    margin,
                ) {
                    Some((w, _, _)) => w,
                    None => {
                        return Err(format!(
                            "could not take a {}x{} window out of the reference stack's \
                             {fw}x{fh} planes",
                            fw.saturating_sub(2 * margin),
                            fh.saturating_sub(2 * margin)
                        ))
                    }
                },
            };

            planes.push(StackPlane {
                offset_um: (i as f64 - mid) * step_um,
                normalised: ZStack::normalise(&win),
            });
        }

        Ok(ZStack {
            width: fw - 2 * margin,
            height: fh - 2 * margin,
            step_um,
            planes,
        })
    }

    /// Match a window against the stack.
    ///
    /// `None` when the best score is below `min_score` — the field of view is not
    /// the one the stack was taken of, and acting on the number would drive the
    /// stage somewhere arbitrary.
    pub fn locate(&self, win: &[f32], min_score: f32) -> Option<ZMatch> {
        if self.planes.is_empty() || win.len() != self.width * self.height {
            return None;
        }
        let query = ZStack::normalise(win);

        let mut scores = Vec::with_capacity(self.planes.len());
        let mut best = 0usize;
        let mut best_score = f32::NEG_INFINITY;
        for (i, plane) in self.planes.iter().enumerate() {
            let s = dot(&query, &plane.normalised);
            if s > best_score {
                best_score = s;
                best = i;
            }
            scores.push(s);
        }

        // The `is_finite` is not decoration: a NaN score compares false against
        // everything, so `best_score < min_score` alone would let it *through*.
        // One would arrive if the normalisation ever divided by a zero norm, which
        // is exactly what a blank or blocked field of view produces.
        if !best_score.is_finite() || best_score < min_score {
            return None;
        }

        // No parabola at the ends of the stack: there is no third point, and the
        // true peak is probably outside the stack anyway, which is worth seeing in
        // the log as a pinned end plane rather than smoothed over.
        let delta = if best == 0 || best + 1 == self.planes.len() {
            0.0
        } else {
            parabolic_peak(scores[best - 1], scores[best], scores[best + 1])
        };

        Some(ZMatch {
            offset_um: self.planes[best].offset_um + delta * self.step_um,
            best_plane: best,
            score: best_score,
        })
    }

    /// Zero-mean, unit-norm copy of `win`, so that a dot product against a
    /// [`StackPlane`] is a normalised cross-correlation.
    pub fn normalise(win: &[f32]) -> Vec<f32> {
        if win.is_empty() {
            return Vec::new();
        }
        // The mean and the norm are accumulated in f64. Subtracting a mean of a
        // few hundred from 200,000 samples in f32 throws away most of the small
        // differences between neighbouring planes, which is the entire signal
        // here.
        let n = win.len() as f64;
        let mean = win.iter().map(|&v| v as f64).sum::<f64>() / n;
        let norm = win
            .iter()
            .map(|&v| {
                let d = v as f64 - mean;
                d * d
            })
            .sum::<f64>()
            .sqrt();
        if norm > 0.0 {
            win.iter()
                .map(|&v| ((v as f64 - mean) / norm) as f32)
                .collect()
        } else {
            // A featureless window. Returning zeros rather than dividing by zero
            // makes every score exactly 0, which is below any sane `min_score`,
            // so a blank or blocked field reports "no idea" instead of a NaN.
            vec![0.0; win.len()]
        }
    }
}

/// The dot product of two equal-length normalised windows, in f64.
///
/// Truncates to the shorter of the two rather than panicking: `locate` has
/// already checked the length, and a panic here would be inside the measurement
/// loop of a live recording.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| x as f64 * y as f64)
        .sum::<f64>() as f32
}

/// Sub-plane peak position from three correlation scores, by fitting a parabola.
///
/// Returns the offset from the middle sample, in plane units, clamped to
/// `-1..1` — a fit that lands outside that means the three points were not a
/// peak, and following it would be worse than taking the best plane as it stands.
pub fn parabolic_peak(left: f32, middle: f32, right: f32) -> f64 {
    let (l, m, r) = (left as f64, middle as f64, right as f64);
    // Through (-1, l), (0, m), (1, r): y = a x² + b x + c with a = (l + r - 2m)/2,
    // b = (r - l)/2, so the extremum is at (r - l) / (2 (2m - l - r)).
    let curve = 2.0 * m - l - r;
    // curve <= 0 is a valley or a straight line: the extremum is a minimum, or
    // there is none. Either way these three points do not bracket a peak, and the
    // best plane as it stands is the better answer. This also catches the flat
    // case, where the denominator would be zero, and the NaN — which fails the
    // `<= 0.0` test rather than passing it, hence the second clause.
    if curve <= 0.0 || !curve.is_finite() {
        return 0.0;
    }
    let offset = (r - l) / (2.0 * curve);
    if !offset.is_finite() {
        return 0.0;
    }
    offset.clamp(-1.0, 1.0)
}

#[cfg(test)]
#[path = "zstack_tests.rs"]
mod zstack_tests;
