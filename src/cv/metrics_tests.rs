//! Tests for the focus metrics.
//!
//! The property that matters is not the value any metric returns — nothing ever
//! looks at an absolute focus number — but that **blurring an image lowers every
//! one of them**. That is the whole contract the controller relies on when it
//! treats a fall in the metric as defocus, and it is asserted for all five.
//!
//! The second property is the reason [`Metric::HighFreqRatio`] is the default:
//! halving an image's brightness, which is what an hour of bleaching does, must
//! leave it alone while it halves [`Metric::TopPercentile`]. That contrast is
//! asserted explicitly rather than left as a remark in a doc comment.
//!
//! The `HighFreqRatio` cases are in tests of their own because they are the only
//! ones that need [`super::super::fft::Fft2d`]. When that is unimplemented they
//! are the tests that fail, and keeping the other four metrics in separate cases
//! means the failure says which part is missing instead of hiding four working
//! metrics behind one `todo!()`.

use super::*;

/// A small deterministic generator. Not a good one — it only has to be the same
/// on every machine and every run, because a test that fails one time in twenty
/// on a property this sharp is worse than no test.
struct Lcg(u64);

impl Lcg {
    fn next_f(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as f32 / (1u64 << 31) as f32
    }
}

const W: usize = 64;
const H: usize = 64;

/// Something shaped like a two-photon frame: a dim background, small bright
/// blobs, and shot noise. The blobs matter — a metric can be fooled by pure
/// noise, which has no scale, while defocus is a blur of *structure*.
fn synthetic() -> Vec<f32> {
    let mut rng = Lcg(0x5eed_1234_abcd_0001);
    let mut img = vec![100.0f32; W * H];
    for _ in 0..40 {
        let cx = rng.next_f() * (W as f32 - 1.0);
        let cy = rng.next_f() * (H as f32 - 1.0);
        let amp = 200.0 + 400.0 * rng.next_f();
        let sigma = 1.0 + 0.6 * rng.next_f();
        let two_s2 = 2.0 * sigma * sigma;
        let reach = (4.0 * sigma).ceil() as i32;
        for dy in -reach..=reach {
            for dx in -reach..=reach {
                let x = cx.round() as i32 + dx;
                let y = cy.round() as i32 + dy;
                if x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
                    continue;
                }
                let fx = x as f32 - cx;
                let fy = y as f32 - cy;
                img[y as usize * W + x as usize] += amp * (-(fx * fx + fy * fy) / two_s2).exp();
            }
        }
    }
    for v in img.iter_mut() {
        *v += 10.0 * (rng.next_f() - 0.5);
    }
    img
}

/// A separable Gaussian blur, edges replicated.
///
/// Replicated rather than zero-padded: padding with zeros darkens the border,
/// which raises the local gradient there and could let a gradient metric come out
/// *higher* after blurring — the test would then be testing the padding.
fn blur(src: &[f32], sigma: f32) -> Vec<f32> {
    let reach = (3.0 * sigma).ceil() as i32;
    let kernel: Vec<f32> = (-reach..=reach)
        .map(|d| (-(d * d) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let norm: f32 = kernel.iter().sum();
    let clamp = |v: i32, hi: usize| v.max(0).min(hi as i32 - 1) as usize;

    let mut mid = vec![0.0f32; W * H];
    for y in 0..H {
        for x in 0..W {
            let mut acc = 0.0;
            for (k, &w) in kernel.iter().enumerate() {
                let sx = clamp(x as i32 + k as i32 - reach, W);
                acc += w * src[y * W + sx];
            }
            mid[y * W + x] = acc / norm;
        }
    }
    let mut out = vec![0.0f32; W * H];
    for y in 0..H {
        for x in 0..W {
            let mut acc = 0.0;
            for (k, &w) in kernel.iter().enumerate() {
                let sy = clamp(y as i32 + k as i32 - reach, H);
                acc += w * mid[sy * W + x];
            }
            out[y * W + x] = acc / norm;
        }
    }
    out
}

fn meter(metric: Metric) -> FocusMeter {
    FocusMeter::new(metric, W, H, &HighFreq::default())
}

/// The contract: sharper is larger, for the four metrics that need no transform.
#[test]
fn blur_lowers_the_four_cheap_metrics() {
    let sharp = synthetic();
    let soft = blur(&sharp, 1.2);
    for metric in [
        Metric::NormVariance,
        Metric::Brenner,
        Metric::Tenengrad,
        Metric::TopPercentile,
    ] {
        let mut m = meter(metric);
        let a = m.measure(&sharp);
        let b = m.measure(&soft);
        assert!(
            b < a,
            "{metric:?} did not fall when the image was blurred: {a} -> {b}"
        );
    }
}

/// The same contract for the default metric, which is the one that needs the FFT.
#[test]
fn blur_lowers_the_high_freq_ratio() {
    let sharp = synthetic();
    let soft = blur(&sharp, 1.2);
    let mut m = meter(Metric::HighFreqRatio);
    let a = m.measure(&sharp);
    let b = m.measure(&soft);
    assert!(a > 0.0, "a structured frame has high-frequency power: {a}");
    assert!(
        b < a,
        "HighFreqRatio did not fall when the image was blurred: {a} -> {b}"
    );
    // A blur of just over a pixel should be unmissable, not marginal: this is the
    // margin the dead band (3% by default) has to sit inside.
    assert!(
        b < 0.9 * a,
        "HighFreqRatio barely moved under a 1.2 px blur: {a} -> {b}"
    );
}

/// Why `HighFreqRatio` is the default: bleaching scales the image, and only the
/// ratio metrics survive it.
#[test]
fn high_freq_ratio_ignores_brightness_where_top_percentile_follows_it() {
    let sharp = synthetic();
    // A uniform scaling, which is what bleaching mostly does to a frame: the
    // structure is still exactly as sharp, so a focus metric must not move.
    let dim: Vec<f32> = sharp.iter().map(|&v| 0.5 * v).collect();

    let mut hf = meter(Metric::HighFreqRatio);
    let bright = hf.measure(&sharp);
    let faded = hf.measure(&dim);
    assert!(bright > 0.0);
    let drift = ((faded - bright) / bright).abs();
    assert!(
        drift < 1e-3,
        "HighFreqRatio moved {:.4}% when the frame was halved: {bright} -> {faded}",
        drift * 100.0
    );

    let mut tp = meter(Metric::TopPercentile);
    let bright_tp = tp.measure(&sharp);
    let faded_tp = tp.measure(&dim);
    let ratio = faded_tp / bright_tp;
    assert!(
        (ratio - 0.5).abs() < 0.01,
        "TopPercentile should halve with the brightness, got a factor of {ratio}"
    );

    // The other ratio metric, for the same reason. Kept here rather than in its
    // own test because it is the same property being asserted.
    let mut nv = meter(Metric::NormVariance);
    let a = nv.measure(&sharp);
    let b = nv.measure(&dim);
    assert!(
        ((b - a) / a).abs() < 1e-3,
        "NormVariance is meant to be scale-invariant: {a} -> {b}"
    );
}

/// The masks are the part of `HighFreqRatio` that is easy to get subtly wrong, so
/// they are checked directly rather than only through the metric.
#[test]
fn the_band_masks_exclude_dc_and_nest() {
    let m = meter(Metric::HighFreqRatio);
    assert!(!m.low_bins.is_empty() && !m.high_bins.is_empty());
    assert!(
        !m.low_bins.contains(&0) && !m.high_bins.contains(&0),
        "bin 0 is the mean, which is exactly what bleaching changes"
    );
    assert!(
        m.high_bins.len() < m.low_bins.len(),
        "the high band must be a proper subset of the low one"
    );
    let low: std::collections::HashSet<u32> = m.low_bins.iter().copied().collect();
    assert!(
        m.high_bins.iter().all(|b| low.contains(b)),
        "every high bin must also be a low bin, or the ratio can exceed 1"
    );
    // The wrap-around: the frequency of bin `n - 1` is that of bin 1, so the top
    // row of the spectrum must be treated as the lowest frequencies and left out
    // of the high band.
    assert!(!m.high_bins.contains(&((W - 1) as u32)));
}

/// A mask built with cuts that admit everything still leaves the mean out.
#[test]
fn a_zero_low_cut_still_excludes_the_mean() {
    let m = FocusMeter::new(
        Metric::HighFreqRatio,
        16,
        16,
        &HighFreq {
            low_cut: 0.0,
            high_cut: 0.35,
        },
    );
    assert_eq!(m.low_bins.len(), 16 * 16 - 1);
}

/// The other four metrics build no FFT plan and no masks at all.
#[test]
fn the_cheap_metrics_allocate_nothing() {
    let m = meter(Metric::Brenner);
    assert!(m.fft.is_none() && m.low_bins.is_empty() && m.scratch.is_empty());
}

#[test]
fn a_flat_window_has_no_structure_at_all() {
    let flat = vec![512.0f32; W * H];
    assert_eq!(norm_variance(&flat), 0.0);
    assert_eq!(brenner(&flat, W, H), 0.0);
    assert_eq!(tenengrad(&flat, W, H), 0.0);
    // Except in brightness, which is all `TopPercentile` ever measured.
    assert!((top_percentile(&flat, 0.01) - 512.0).abs() < 1e-3);
}

#[test]
fn top_percentile_averages_the_brightest_and_nothing_else() {
    // 1..=100, so the brightest tenth is 91..=100 and averages 95.5.
    let ramp: Vec<f32> = (1..=100).map(|v| v as f32).collect();
    assert!((top_percentile(&ramp, 0.1) - 95.5).abs() < 1e-3);
    // Order must not matter.
    let mut shuffled = ramp.clone();
    shuffled.reverse();
    assert!((top_percentile(&shuffled, 0.1) - 95.5).abs() < 1e-3);
    // A fraction too small to name a single pixel still names one: the brightest.
    assert!((top_percentile(&ramp, 0.0) - 100.0).abs() < 1e-3);
    // And a fraction of everything is the mean.
    assert!((top_percentile(&ramp, 1.0) - 50.5).abs() < 1e-3);
}

/// Brenner is a *sum* and Tenengrad a *mean*, as their docs say. Checked on a
/// window small enough to count by hand, because getting the interior bounds
/// wrong is invisible on a real frame.
#[test]
fn brenner_and_tenengrad_on_an_arithmetic_ramp() {
    // Each row is 0, 1, 2 — so every (x+2) - x difference is 2, and there is one
    // such pair per row.
    let w = 3;
    let h = 4;
    let img: Vec<f32> = (0..w * h).map(|i| (i % w) as f32).collect();
    assert!((brenner(&img, w, h) - (h as f32 * 4.0)).abs() < 1e-3);
    // The Sobel x response of a unit ramp is 8 over a 3x3 window, the y response
    // 0, and there is exactly one interior pixel per interior row.
    assert!((tenengrad(&img, w, h) - 64.0).abs() < 1e-3);
}

/// Sizes that leave no interior are answered with zero rather than a panic: a
/// silly ROI in the config must not take the session down mid-recording.
#[test]
fn degenerate_windows_do_not_panic() {
    assert_eq!(brenner(&[], 0, 0), 0.0);
    assert_eq!(tenengrad(&[], 0, 0), 0.0);
    assert_eq!(norm_variance(&[]), 0.0);
    assert_eq!(top_percentile(&[], 0.5), 0.0);
    assert_eq!(brenner(&[1.0, 2.0], 2, 1), 0.0);
    assert_eq!(tenengrad(&[1.0, 2.0, 3.0], 3, 1), 0.0);
    // A window shorter than it claims is zero-padded, not indexed out of bounds.
    let mut m = meter(Metric::Brenner);
    assert_eq!(m.measure(&[1.0, 2.0, 3.0]), 0.0);
}

/// The saturation test, and the trap in it: the full scale is the file's, and
/// getting it from the 16-bit container instead of the 10 bits actually used
/// would mean it never fires.
#[test]
fn saturation_is_a_fraction_against_the_files_own_full_scale() {
    let mut win = vec![300.0f32; 1000];
    for v in win.iter_mut().take(5) {
        *v = 1023.0;
    }
    assert!(
        !FocusMeter::saturated(&win, 1023.0, 0.01),
        "0.5% of the window at full scale is normal"
    );
    for v in win.iter_mut().take(30) {
        *v = 1023.0;
    }
    assert!(
        FocusMeter::saturated(&win, 1023.0, 0.01),
        "3% of the window at full scale is a blown frame"
    );
    assert!(
        !FocusMeter::saturated(&win, u16::MAX as f32, 0.01),
        "this is the bug: against a 16-bit full scale, 10-bit saturation never fires"
    );
    // Above full scale counts too, which is what a summed two-channel window does.
    let hot = vec![2046.0f32; 10];
    assert!(FocusMeter::saturated(&hot, 1023.0, 0.5));
    assert!(!FocusMeter::saturated(&[], 1023.0, 0.0));
}
