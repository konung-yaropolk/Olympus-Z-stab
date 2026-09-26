//! Cancelling the X/Y movement, by phase correlation.
//!
//! The sample slides sideways during a recording. That sideways movement is
//! **not** corrected — the experimenter does not want the field of view driven
//! around — but it has to be cancelled *in the measurement*, because a focus
//! metric computed over a field that has moved is comparing different tissue.
//!
//! # The method
//!
//! Phase correlation: take both frames' spectra, multiply one by the other's
//! conjugate, normalise every bin to unit magnitude, transform back. The result
//! is sharply peaked at the shift between them. Normalising the magnitude is
//! what makes it a *phase* correlation and what makes it indifferent to
//! brightness — which matters here, because the frames being compared are
//! minutes apart in a bleaching recording.
//!
//! The peak is located to the nearest pixel and **used to the nearest pixel**.
//! No sub-pixel fit, on purpose: see the note in [`super`] about interpolation
//! and the focus metric. The peak's height relative to the mean of the
//! correlation surface is returned so a failed match can be rejected rather than
//! acted on.
//!
//! # The taper
//!
//! A frame's edges are a discontinuity the transform reads as strong content at
//! every frequency, and it lands at zero shift — so an untapered phase
//! correlation reports "no movement" rather often, which is the most dangerous
//! way for this to fail. Both frames get a cosine edge taper first.
//!
//! Two things about that were learned by measuring rather than by reasoning, and
//! both are in the tests:
//!
//! * The edge step hurts most when the *content* is low-pass, which a
//!   diffraction-limited and slightly defocused two-photon frame is. Whitening
//!   gives every bin one vote, and the edge step is loudest at low frequencies —
//!   exactly the bins where a blurred sample has its only real signal, so there is
//!   nothing to outvote it. On sharp broadband texture the untapered correlation
//!   gets the right answer anyway, so a test built on crisp synthetic texture will
//!   cheerfully pass with no taper at all and prove nothing.
//! * The mean has to come off the frame *before* the taper is applied, or the
//!   taper makes the pedestal into a stationary broadband shape of its own and
//!   reintroduces the same failure it was added to prevent. See the note on
//!   `Registrar::load`.

use super::fft::Fft2d;
use rustfft::num_complex::Complex32;

/// A whole-frame shift.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Shift {
    /// Rows the frame has moved relative to the reference. Positive means the
    /// frame's content sits further down than the reference's.
    pub dy: i32,
    pub dx: i32,
    /// The correlation peak, as a multiple of the mean of the surface. Around 1
    /// means no peak at all; a real match on real tissue is many times that.
    pub peak: f32,
    /// False when the peak was too weak or the shift implausibly large, in which
    /// case `dy`/`dx` are zero and the frame should be skipped, not measured.
    pub trusted: bool,
}

/// Measures shifts against one reference.
pub struct Registrar {
    fft: Fft2d,
    width: usize,
    height: usize,
    /// Cosine edge taper, `width * height`, applied to every frame including the
    /// reference.
    taper: Vec<f32>,
    /// The reference's conjugated spectrum, ready to multiply against.
    reference: Option<Vec<rustfft::num_complex::Complex32>>,
    max_shift: usize,
    min_peak: f32,
    /// Scratch, so measuring a frame does not allocate.
    scratch: Vec<rustfft::num_complex::Complex32>,
}

impl Registrar {
    /// `taper_px` is the width of the cosine edge roll-off in pixels.
    pub fn new(
        width: usize,
        height: usize,
        taper_px: f32,
        max_shift: usize,
        min_peak: f32,
    ) -> Registrar {
        Registrar {
            fft: Fft2d::new(width, height),
            width,
            height,
            taper: build_taper(width, height, taper_px),
            reference: None,
            max_shift,
            min_peak,
            scratch: vec![Complex32::new(0.0, 0.0); width * height],
        }
    }

    /// Set the frame every later frame is measured against.
    ///
    /// The reference is stored **already conjugated**, so the per-frame work is a
    /// plain multiply. A frame of the wrong size leaves the old reference in
    /// place — see the note in [`Self::shift`] about why nothing here panics.
    pub fn set_reference(&mut self, frame: &[f32]) {
        if frame.len() != self.width * self.height {
            return;
        }
        self.load(frame);
        let mut spectrum = self.scratch.clone();
        for bin in spectrum.iter_mut() {
            *bin = bin.conj();
        }
        self.reference = Some(spectrum);
    }

    /// The shift of `frame` relative to the reference.
    ///
    /// Returns an untrusted [`Shift`] rather than an error when the match fails:
    /// a single bad frame in a recording is normal and must not stop the session.
    /// The same goes for the cases that would be programming errors anywhere else
    /// — no reference yet, or a frame that is not the size this was planned for.
    /// The alternative is a panic beside a running microscope, which is worse than
    /// a skipped frame even when it is deserved.
    pub fn shift(&mut self, frame: &[f32]) -> Shift {
        let n = self.width * self.height;
        if n == 0 || frame.len() != n || self.reference.is_none() {
            return Shift::default();
        }

        self.load(frame);

        // Cross-power spectrum, then whitened: every bin to unit magnitude. This
        // is the step that makes it a *phase* correlation. Without it a few
        // enormous low-frequency bins decide the answer and the surface is a broad
        // hill rather than a spike; with it, every spatial frequency votes once,
        // and a frame that is half as bright as the reference votes the same way.
        let Some(reference) = self.reference.as_ref() else {
            return Shift::default();
        };
        for (bin, refbin) in self.scratch.iter_mut().zip(reference.iter()) {
            let cross = *bin * *refbin;
            let magnitude = cross.norm();
            // A bin with no magnitude has no phase to normalise. It must go to
            // zero, not to unit magnitude: `1 + 0i` is the phase of *zero shift*,
            // so filling dead bins with it would build a spike at zero shift out
            // of nothing — the exact failure the taper is here to avoid, smuggled
            // back in by a divide-by-zero guard. The taper zeroes the frame's
            // outermost pixels, so dead bins do occur.
            *bin = if magnitude > 1e-20 {
                cross / magnitude
            } else {
                Complex32::new(0.0, 0.0)
            };
        }

        self.fft.inverse(&mut self.scratch);

        // Peak of the correlation surface, and its height relative to the mean.
        // Magnitude, not the real part: the surface is real to within rounding
        // only when the pair is a clean translation, and taking |c| costs nothing
        // and never turns a real match into a negative lobe.
        //
        // The mean accumulates in f64. In f32 a 512x512 surface sums 262144 terms
        // that are all the same order of magnitude, and the running total stops
        // being able to see them long before the end — the mean comes out low and
        // every peak ratio comes out flatteringly high.
        let mut total = 0.0f64;
        let mut best = -1.0f32;
        let mut best_at = 0usize;
        for (i, c) in self.scratch.iter().enumerate() {
            let m = c.norm();
            total += m as f64;
            if m > best {
                best = m;
                best_at = i;
            }
        }
        let mean = (total / n as f64) as f32;
        let peak = if mean > 0.0 { best / mean } else { 0.0 };

        // The surface is periodic, so the second half of each axis is the negative
        // shifts: index height-1 is -1 row, not +511. Reading it as an unsigned
        // index is how a -12 pixel drift becomes a +500 pixel one, and a +500 is
        // then rejected as implausible — so the bug does not announce itself as a
        // wrong correction, it announces itself as a recording where nothing ever
        // matches.
        let dy = unwrap_index(best_at / self.width, self.height);
        let dx = unwrap_index(best_at % self.width, self.width);

        let too_far = dy.unsigned_abs() as usize > self.max_shift
            || dx.unsigned_abs() as usize > self.max_shift;
        if too_far || peak < self.min_peak {
            // `peak` is still reported: the log wants to show how weak the match
            // that was thrown away actually was.
            Shift {
                dy: 0,
                dx: 0,
                peak,
                trusted: false,
            }
        } else {
            Shift {
                dy,
                dx,
                peak,
                trusted: true,
            }
        }
    }

    #[allow(dead_code)] // used by the tests; part of the type's contract.
    pub fn has_reference(&self) -> bool {
        self.reference.is_some()
    }

    /// Mean-subtract and taper `frame` into `scratch`, then transform it.
    ///
    /// Caller has already checked the length.
    ///
    /// # The mean has to come off first, and it is not obvious
    ///
    /// Tapering the frame as it stands multiplies the *pedestal* by the taper as
    /// well as the structure, and a two-photon frame is mostly pedestal: dark
    /// offset plus out-of-focus haze, with the interesting modulation riding on
    /// top. `taper * constant` is a shape that is identical in every frame and
    /// does not move with the sample, and because the taper is mostly flat with a
    /// narrow roll-off its spectrum is broad — so that shape appears in *every*
    /// bin, and after whitening it votes for zero shift in every bin, at equal
    /// weight to the real content. Measured, on a synthetic pair with a pedestal of
    /// 2000 under a modulation of +/-300 and a 3 px taper: zero shift, at 84x the
    /// mean, for a pair that had really moved five rows. Worse than no taper at
    /// all, and wrong in the direction that reports "nothing moved".
    ///
    /// Subtracting the mean before tapering removes that term, and then the taper
    /// does only what it is there for.
    fn load(&mut self, frame: &[f32]) {
        // f64 accumulation: 262144 f32 pixels of similar size lose the tail of the
        // sum, and this mean has to be good enough that the pedestal really cancels.
        let mut total = 0.0f64;
        for &v in frame.iter() {
            total += v as f64;
        }
        let mean = (total / frame.len() as f64) as f32;

        for (i, (&v, &w)) in frame.iter().zip(self.taper.iter()).enumerate() {
            self.scratch[i] = Complex32::new((v - mean) * w, 0.0);
        }
        self.fft.forward(&mut self.scratch);
    }
}

/// A raised-cosine (Tukey) edge taper, separable, `width * height`.
///
/// The roll-off runs from zero at the outermost pixel to one at `taper_px` in, on
/// each edge, and the 2-D window is the product of the two axes' windows. A
/// `taper_px` of zero or less means no taper at all — all ones — which exists so
/// that a configuration can switch it off and so that a test can show what
/// happens when it is off.
///
/// Distance is measured to the *nearer* edge, which keeps the window sane if
/// `taper_px` is set wider than half the frame: it degenerates into a full Hann
/// window rather than producing a ramp that overshoots and folds back.
fn build_taper(width: usize, height: usize, taper_px: f32) -> Vec<f32> {
    let ramp = |i: usize, n: usize| -> f32 {
        if n == 0 || !taper_px.is_finite() || taper_px <= 0.0 {
            return 1.0;
        }
        let d = i.min(n - 1 - i) as f32;
        if d >= taper_px {
            1.0
        } else {
            0.5 * (1.0 - (std::f32::consts::PI * d / taper_px).cos())
        }
    };
    let rows: Vec<f32> = (0..height).map(|y| ramp(y, height)).collect();
    let cols: Vec<f32> = (0..width).map(|x| ramp(x, width)).collect();
    let mut taper = Vec::with_capacity(width * height);
    for wy in rows {
        for &wx in cols.iter() {
            taper.push(wy * wx);
        }
    }
    taper
}

/// A correlation-surface index as a signed shift.
///
/// Anything past the halfway point is a negative shift. `n / 2` itself is
/// genuinely ambiguous on an even axis — it is both `+n/2` and `-n/2` — and is
/// called positive here, which is arbitrary and does not matter: a shift of a
/// quarter of a frame is rejected by `max_shift` long before the sign is used.
fn unwrap_index(i: usize, n: usize) -> i32 {
    if i > n / 2 {
        i as i32 - n as i32
    } else {
        i as i32
    }
}

/// The part of `frame` that overlaps the reference once `shift` is undone.
///
/// Copies, with an integer offset only, into a window inset by `margin` on every
/// side. The margin must be at least the largest shift being corrected, so that
/// the window always lies inside both frames; a shift larger than the margin
/// returns `None` rather than a window padded with edge pixels, because padding
/// invents high-frequency content exactly where the metric would read it.
///
/// Returns the window and its dimensions.
pub fn aligned_window(
    frame: &[f32],
    width: usize,
    height: usize,
    shift: Shift,
    margin: usize,
) -> Option<(Vec<f32>, usize, usize)> {
    if frame.len() != width * height || width <= 2 * margin || height <= 2 * margin {
        return None;
    }
    let (ww, wh) = (width - 2 * margin, height - 2 * margin);

    // The window is defined in the *reference's* coordinates: rows `margin ..
    // height - margin`. The frame's content sits `dy` rows further down than the
    // reference's, so the same tissue is found `dy` rows further down in the
    // frame. Hence reading from `margin + dy`, not `margin - dy`. Both signs of
    // that mistake compile, both produce a window full of plausible tissue, and
    // both double the apparent movement instead of cancelling it.
    if shift.dy.unsigned_abs() as usize > margin || shift.dx.unsigned_abs() as usize > margin {
        return None;
    }
    // Both offsets are within `margin` of zero, so these cannot go negative and
    // `y0 + wh <= height`, `x0 + ww <= width` both hold — the copy below needs no
    // further bounds work.
    let y0 = (margin as i64 + shift.dy as i64) as usize;
    let x0 = (margin as i64 + shift.dx as i64) as usize;

    let mut out = Vec::with_capacity(ww * wh);
    for j in 0..wh {
        let start = (y0 + j) * width + x0;
        out.extend_from_slice(&frame[start..start + ww]);
    }
    Some((out, ww, wh))
}

#[cfg(test)]
#[path = "register_tests.rs"]
mod register_tests;
