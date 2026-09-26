//! How sharp a frame is.
//!
//! # The problem these have to survive
//!
//! In a two-photon recording of living tissue, three things change the picture
//! over minutes, and only one of them is focus:
//!
//! * **Bleaching.** Fluorophore is destroyed as it is imaged. Brightness and
//!   absolute contrast fall steadily, all session, whether or not anything moved.
//! * **Activity.** The cells being recorded get brighter and dimmer; that is the
//!   point of the recording.
//! * **Defocus.** What this program is here to correct.
//!
//! A metric that cannot tell the first two from the third will drive the stage
//! all session and call it stabilisation. That is why the default is
//! [`Metric::HighFreqRatio`]: it is the **ratio** of high spatial frequency power
//! to total, so scaling the whole image — which is what bleaching and a
//! whole-field activity change mostly do — leaves it unchanged, while defocus,
//! which is a low-pass filter, moves it a lot.
//!
//! The others are offered because they are cheaper and are the right answer on a
//! bright, stable, structured sample. None of them is bleaching-proof.
//!
//! # Why the ratio's denominator is not the total power
//!
//! "High over total" is the textbook phrasing, and it is not quite what this
//! computes: the denominator is the power above `low_cut`, not everything. Two
//! reasons, both learnt from what the lowest bins of a real frame contain.
//!
//! Bin 0 is the mean. Including it would put the one quantity bleaching changes
//! most directly into the denominator, which is the opposite of the intent — a
//! 10% dimmer frame would read as *sharper*. The few bins around it are the
//! illumination profile and the shading across the field, which are properties of
//! the optics and the laser rather than of the sample, and which drift on their
//! own schedule. Everything from `low_cut` up is sample structure, and the ratio
//! of two bands of sample structure is what actually tracks focus.
//!
//! Scale invariance survives this unchanged: multiplying every sample by `k`
//! multiplies the power in every bin by `k²`, so both bands scale together and
//! the ratio does not move. That is the property the tests assert explicitly,
//! because it is the entire reason this metric is the default.
//!
//! # Why there is no window function
//!
//! A frame's edges do not match where the transform wraps them round, and that
//! discontinuity puts power into every bin. It is tempting to taper it away as
//! [`super::register`] has to. It is not necessary here, and would cost accuracy:
//! the leakage is broadband, so it lands in the numerator and the denominator in
//! much the same proportion and largely divides out, while a taper would darken
//! the edges of the window and make the metric depend on where in the frame the
//! structure happens to sit. A mean subtraction is likewise pointless — for a
//! discrete transform the mean is *exactly* bin 0 and nothing else, and bin 0 is
//! already excluded.

use super::fft::Fft2d;
use crate::config::{HighFreq, Metric};
use rustfft::num_complex::Complex32;

/// The fraction of the window [`Metric::TopPercentile`] averages over.
///
/// Not a config knob, because there is nothing to tune: one per cent of a
/// 450x450 window is two thousand pixels, enough that shot noise averages out
/// and few enough that it is the bright structure rather than the background. The
/// metric's weakness is bleaching, not this number.
const TOP_FRACTION: f32 = 0.01;

/// Measures one metric over one window size, keeping whatever that metric needs
/// between frames — for [`Metric::HighFreqRatio`], an FFT plan and the radial
/// frequency masks.
pub struct FocusMeter {
    metric: Metric,
    width: usize,
    height: usize,
    /// Only for `HighFreqRatio`.
    fft: Option<Fft2d>,
    /// Indices of the bins above `low_cut` and above `high_cut`, precomputed:
    /// the masks depend only on the window size, and recomputing them per frame
    /// costs more than the transform.
    ///
    /// Bin 0 is in neither list at any cut, including `low_cut: 0.0` — it is the
    /// mean, and the mean is what bleaching takes away. `high_bins` is a subset
    /// of `low_bins`, so the ratio is always in `0..=1`.
    low_bins: Vec<u32>,
    high_bins: Vec<u32>,
    scratch: Vec<rustfft::num_complex::Complex32>,
}

impl FocusMeter {
    pub fn new(metric: Metric, width: usize, height: usize, hf: &HighFreq) -> FocusMeter {
        let n = width.saturating_mul(height);
        let mut low_bins = Vec::new();
        let mut high_bins = Vec::new();
        let mut fft = None;
        let mut scratch = Vec::new();

        // Everything in this block is dead weight for the other four metrics: the
        // plan is a few hundred kilobytes and the two index lists are a megabyte
        // at 512x512, so they are built only when they will be used.
        if metric == Metric::HighFreqRatio && n > 0 {
            fft = Some(Fft2d::new(width, height));
            scratch = vec![Complex32::new(0.0, 0.0); n];
            // A frequency-sorted mask would let both sums share one pass, but the
            // pass order is then no longer the memory order of the spectrum, and
            // on the fifteen-year-old processor this runs on the cache miss costs
            // more than the second pass. Left in row-major order on purpose.
            low_bins.reserve(n);
            for v in 0..height {
                let fy = nyquist_fraction(v, height);
                for u in 0..width {
                    if u == 0 && v == 0 {
                        continue;
                    }
                    let fx = nyquist_fraction(u, width);
                    let r = (fx * fx + fy * fy).sqrt();
                    let bin = (v * width + u) as u32;
                    if r >= hf.low_cut {
                        low_bins.push(bin);
                    }
                    if r >= hf.high_cut {
                        high_bins.push(bin);
                    }
                }
            }
            low_bins.shrink_to_fit();
            high_bins.shrink_to_fit();
        }

        FocusMeter {
            metric,
            width,
            height,
            fft,
            low_bins,
            high_bins,
            scratch,
        }
    }

    /// The metric over `win`, which must be `width * height`.
    ///
    /// Larger is always sharper, for every metric here — the controller compares
    /// this against the reference as a ratio and never needs to know which metric
    /// it is looking at.
    pub fn measure(&mut self, win: &[f32]) -> f32 {
        match self.metric {
            Metric::HighFreqRatio => self.high_freq_ratio(win),
            Metric::NormVariance => norm_variance(win),
            Metric::Brenner => brenner(win, self.width, self.height),
            Metric::Tenengrad => tenengrad(win, self.width, self.height),
            Metric::TopPercentile => top_percentile(win, TOP_FRACTION),
        }
    }

    /// The band ratio. See the module notes for why the denominator starts at
    /// `low_cut` rather than at zero.
    ///
    /// A window of the wrong size is zero-padded rather than being a panic: this
    /// runs inside a recording that cannot be restarted, and a metric of nearly
    /// zero makes the controller hold, while a panic in the measurement loses the
    /// session.
    fn high_freq_ratio(&mut self, win: &[f32]) -> f32 {
        let n = self.width * self.height;
        if n == 0 || self.low_bins.is_empty() {
            return 0.0;
        }
        if self.scratch.len() != n {
            self.scratch.resize(n, Complex32::new(0.0, 0.0));
        }
        let take = win.len().min(n);
        for (dst, &src) in self.scratch.iter_mut().zip(win[..take].iter()) {
            *dst = Complex32::new(src, 0.0);
        }
        // The scratch is reused between frames, so anything the short window did
        // not cover is last frame's data and has to be cleared.
        for c in self.scratch[take..].iter_mut() {
            *c = Complex32::new(0.0, 0.0);
        }

        let fft = match self.fft.as_mut() {
            Some(f) => f,
            None => return 0.0,
        };
        fft.forward(&mut self.scratch);

        // f64 accumulators, not f32. At 512x512 with 10-bit samples the sums run
        // to about 1e22 over a quarter of a million terms, and the signal being
        // looked for is a change of a few per cent in their ratio; f32 loses
        // enough of the small terms to blur exactly that.
        let mut low = 0.0f64;
        for &i in &self.low_bins {
            let c = self.scratch[i as usize];
            low += c.re as f64 * c.re as f64 + c.im as f64 * c.im as f64;
        }
        let mut high = 0.0f64;
        for &i in &self.high_bins {
            let c = self.scratch[i as usize];
            high += c.re as f64 * c.re as f64 + c.im as f64 * c.im as f64;
        }

        if low > 0.0 {
            (high / low) as f32
        } else {
            // A perfectly flat window. No structure at any frequency, so there is
            // no ratio to report; zero reads as "as defocused as it gets", which
            // is the safe direction to be wrong in because the controller's dead
            // band and confirmation windows then hold rather than act.
            0.0
        }
    }

    /// Whether the window is dominated by saturated pixels, in which case it has
    /// no high-frequency content left and reads as catastrophic defocus. The
    /// caller supplies `full_scale` from the file's own bit depth — 1023 for the
    /// 10-bit recordings this was built against, not 65535.
    pub fn saturated(win: &[f32], full_scale: f32, limit: f32) -> bool {
        if win.is_empty() {
            return false;
        }
        // At or above, not equal to: the samples arrive as integers so equality
        // would do, but a summed multi-channel window can land a hair above the
        // single-channel full scale and must still count.
        let hot = win.iter().filter(|&&v| v >= full_scale).count();
        hot as f32 / win.len() as f32 > limit
    }
}

/// A bin's distance from DC along one axis, as a fraction of Nyquist.
///
/// Bin `i` of an `n`-point transform holds `min(i, n - i) / n` cycles per pixel
/// and Nyquist is half a cycle per pixel, so the fraction is twice that. The
/// `min` is the wrap-around: bin `n - 1` is the same frequency as bin 1 with the
/// other sign, and a mask that forgot it would keep the top half of the spectrum
/// at every cut and measure noise.
///
/// The two-dimensional measure the masks use is the Euclidean length of the pair,
/// which runs to `sqrt(2)` in the corners rather than to 1 — so `high_cut: 1.0`
/// selects the corners rather than nothing at all.
fn nyquist_fraction(i: usize, n: usize) -> f32 {
    if n == 0 {
        return 0.0;
    }
    let folded = i.min(n - i);
    2.0 * folded as f32 / n as f32
}

/// Variance divided by the square of the mean. Scale-invariant, so partly
/// bleaching-tolerant, and cheap.
pub fn norm_variance(win: &[f32]) -> f32 {
    if win.is_empty() {
        return 0.0;
    }
    let n = win.len() as f64;
    let mean = win.iter().map(|&v| v as f64).sum::<f64>() / n;
    // Two passes. The one-pass sum-of-squares form loses most of its significant
    // digits when the mean is large and the variance small, which on a 10-bit
    // frame with a bright background is the normal case.
    let var = win
        .iter()
        .map(|&v| {
            let d = v as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n;
    if mean.abs() > f64::EPSILON {
        (var / (mean * mean)) as f32
    } else {
        0.0
    }
}

/// Brenner gradient: the sum of squared differences between pixels two apart
/// along each row. Sharp and classic, and sensitive to shot noise — which is why
/// it is measured on an average of frames rather than one.
pub fn brenner(win: &[f32], width: usize, height: usize) -> f32 {
    if width < 3 || height == 0 || win.len() < width * height {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for row in win.chunks_exact(width).take(height) {
        for x in 0..width - 2 {
            let d = row[x + 2] as f64 - row[x] as f64;
            sum += d * d;
        }
    }
    sum as f32
}

/// Tenengrad: the mean squared Sobel gradient magnitude. Like [`brenner`] but
/// smoother, because the Sobel kernel averages across rows as it differentiates.
pub fn tenengrad(win: &[f32], width: usize, height: usize) -> f32 {
    if width < 3 || height < 3 || win.len() < width * height {
        return 0.0;
    }
    let at = |y: usize, x: usize| win[y * width + x] as f64;
    let mut sum = 0.0f64;
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let gx = (at(y - 1, x + 1) + 2.0 * at(y, x + 1) + at(y + 1, x + 1))
                - (at(y - 1, x - 1) + 2.0 * at(y, x - 1) + at(y + 1, x - 1));
            let gy = (at(y + 1, x - 1) + 2.0 * at(y + 1, x) + at(y + 1, x + 1))
                - (at(y - 1, x - 1) + 2.0 * at(y - 1, x) + at(y - 1, x + 1));
            sum += gx * gx + gy * gy;
        }
    }
    // The mean over the interior only. The border is left out rather than
    // extended, because an extended border is a made-up gradient and this metric
    // is a sum of gradients.
    (sum / ((width - 2) * (height - 2)) as f64) as f32
}

/// The mean of the brightest `fraction` of pixels. The simplest thing that
/// tracks focus in a two-photon image, and the easiest for bleaching to fool.
pub fn top_percentile(win: &[f32], fraction: f32) -> f32 {
    if win.is_empty() {
        return 0.0;
    }
    let n = win.len();
    // At least one pixel: a fraction so small that it rounds to none would
    // otherwise make this return the mean of nothing.
    let k = ((n as f32 * fraction.max(0.0)).round() as usize).clamp(1, n);
    let mean = |s: &[f32]| s.iter().map(|&v| v as f64).sum::<f64>() / s.len() as f64;
    if k >= n {
        return mean(win) as f32;
    }
    let mut v = win.to_vec();
    // A partial selection, not a sort: linear rather than n log n, which at
    // 200k pixels a frame on this machine is the difference between free and
    // noticeable. `total_cmp` rather than `partial_cmp().unwrap()` because a NaN
    // in the window would otherwise panic inside the measurement loop.
    let idx = n - k;
    v.select_nth_unstable_by(idx, |a, b| a.total_cmp(b));
    mean(&v[idx..]) as f32
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod metrics_tests;
