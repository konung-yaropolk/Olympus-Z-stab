//! Tests for the 2-D FFT.
//!
//! The point of these is not that `rustfft` works — it does — but that this
//! wrapper's two passes, shared scratch and reused plans do not corrupt anything,
//! and that the direction convention is the one the phase correlation assumes.

use super::*;

/// A deterministic textured image. No `rand` dependency, and a fixed pattern
/// makes a failure reproducible.
fn texture(width: usize, height: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let a = (x as f32 * 0.37).sin() * (y as f32 * 0.21).cos();
            let b = ((x * 7 + y * 13) % 17) as f32 / 17.0;
            v.push(20.0 + 40.0 * a + 30.0 * b);
        }
    }
    v
}

#[test]
fn round_trips_after_dividing_by_n() {
    let (w, h) = (16usize, 12usize);
    let src = texture(w, h);
    let mut fft = Fft2d::new(w, h);
    let mut buf = Fft2d::to_complex(&src);

    fft.forward(&mut buf);
    fft.inverse(&mut buf);

    let n = (w * h) as f32;
    for (i, (c, &s)) in buf.iter().zip(src.iter()).enumerate() {
        let re = c.re / n;
        let im = c.im / n;
        assert!(
            (re - s).abs() < 1e-2,
            "sample {i}: round trip gave {re}, wanted {s}"
        );
        assert!(
            im.abs() < 1e-2,
            "sample {i}: imaginary part {im} should vanish"
        );
    }
}

#[test]
fn plans_survive_reuse() {
    // The scratch is shared between the row pass and the column pass and between
    // calls; if either pass trod on the other's part of it, the second transform
    // through the same instance would differ from the first.
    let (w, h) = (9usize, 6usize);
    let src = texture(w, h);
    let mut fft = Fft2d::new(w, h);

    let mut first = Fft2d::to_complex(&src);
    fft.forward(&mut first);
    let mut second = Fft2d::to_complex(&src);
    fft.forward(&mut second);

    for (a, b) in first.iter().zip(second.iter()) {
        assert!((a - b).norm() < 1e-4, "{a} != {b} on the second transform");
    }
}

#[test]
fn dc_bin_is_the_sum() {
    // Cheapest possible check that the two passes compose into a real 2-D
    // transform rather than a row transform applied twice: bin 0 must be the sum
    // of every sample.
    let (w, h) = (8usize, 5usize);
    let src = texture(w, h);
    let mut fft = Fft2d::new(w, h);
    let mut buf = Fft2d::to_complex(&src);
    fft.forward(&mut buf);

    let sum: f32 = src.iter().sum();
    assert!(
        (buf[0].re - sum).abs() < 1e-1,
        "DC bin {} should be the total {sum}",
        buf[0].re
    );
    assert!(buf[0].im.abs() < 1e-1);
}

#[test]
fn shift_theorem_holds_with_the_expected_sign() {
    // This is the test the phase correlation actually depends on. A circular
    // shift of the input must multiply bin k by exp(-2i.pi.k.s/n) — the *negative*
    // exponent for the forward transform. If `rustfft` used the other convention,
    // the correlation's peak would appear at the negation of the true shift and
    // every correction would be applied backwards, which is the one failure mode
    // that makes a stabiliser worse than no stabiliser.
    let n = 16usize;
    let src = texture(n, 1);
    let s = 3usize;
    let rolled: Vec<f32> = (0..n).map(|x| src[(x + n - s) % n]).collect();

    let mut fft = Fft2d::new(n, 1);
    let mut a = Fft2d::to_complex(&src);
    let mut b = Fft2d::to_complex(&rolled);
    fft.forward(&mut a);
    fft.forward(&mut b);

    for k in 1..n {
        let phase = -2.0 * std::f32::consts::PI * (k * s) as f32 / n as f32;
        let expect = a[k] * Complex32::new(phase.cos(), phase.sin());
        assert!(
            (b[k] - expect).norm() < 1e-2,
            "bin {k}: got {}, expected {expect} — sign convention is inverted",
            b[k]
        );
    }
}

#[test]
fn non_power_of_two_sizes_work() {
    // A cropped ROI is not a power of two in either axis, which sends `rustfft`
    // down its Bluestein path — a path with a much larger scratch requirement,
    // and the reason `new` takes the maximum over all four plans.
    let (w, h) = (23usize, 7usize);
    let src = texture(w, h);
    let mut fft = Fft2d::new(w, h);
    let mut buf = Fft2d::to_complex(&src);
    fft.forward(&mut buf);
    fft.inverse(&mut buf);

    let n = (w * h) as f32;
    for (c, &s) in buf.iter().zip(src.iter()) {
        assert!((c.re / n - s).abs() < 1e-2);
    }
}
