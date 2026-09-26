//! Tests for the phase correlation.
//!
//! The pattern throughout is: build one large synthetic "sample", take two crops
//! of it at a known separation, and demand the measured shift back exactly. A
//! crop pair is the honest test rather than a circular roll of one frame, because
//! a circular roll is precisely what the DFT assumes and would pass with the edge
//! handling wrong. Cropping brings new tissue in at one edge and loses it at the
//! other, which is what the microscope actually does.
//!
//! # The sample is smoothed noise, and the first attempt at it was a trap
//!
//! The first version of [`sample`] was a sum of sinusoids plus a term of the form
//! `(x * 31 + y * 17) % 29`, which looked pleasantly tissue-like and made every
//! shift test fail in a way that took a while to pin on the test rather than the
//! code. That modular term is a periodic lattice: it is *invariant* under a shift
//! of one row and six columns, and under any multiple of it. So the pair really
//! was ambiguous, and the correlation was answering correctly — it reported
//! `(0, -1)` for a true `(5, 0)`, which is `(5, 0)` minus five times `(1, 6)`,
//! wrapped. Synthetic texture for a registration test has to be *aperiodic*, and
//! smoothed noise is the easy way to get that.

use super::*;

const SRC: usize = 128;

/// A deterministic synthetic "sample", `SRC` square: white noise smoothed by
/// `smoothing` passes of a 3x3 box, normalised, on a pedestal.
///
/// Aperiodic and broadband, so the correlation has an unambiguous answer. The
/// pedestal is there because a real two-photon frame has one — dark offset plus
/// out-of-focus haze — and because the pedestal is what breaks a taper that has
/// not had the mean taken off first. No `rand` dependency: an LCG with a fixed
/// seed makes a failure reproducible.
fn sample(smoothing: usize, pedestal: f32) -> Vec<f32> {
    let mut state = 7u32;
    let mut v: Vec<f32> = (0..SRC * SRC)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / 16_777_216.0 - 0.5
        })
        .collect();

    for _ in 0..smoothing {
        let prev = v.clone();
        for y in 0..SRC {
            for x in 0..SRC {
                let mut total = 0.0;
                let mut n = 0.0;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let (yy, xx) = (y as i32 + dy, x as i32 + dx);
                        if yy >= 0 && yy < SRC as i32 && xx >= 0 && xx < SRC as i32 {
                            total += prev[yy as usize * SRC + xx as usize];
                            n += 1.0;
                        }
                    }
                }
                v[y * SRC + x] = total / n;
            }
        }
    }

    let peak = v.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-9);
    v.iter().map(|x| pedestal + 300.0 * x / peak).collect()
}

/// A `w` by `h` crop of `src` with its top-left at `(oy, ox)`.
fn crop(src: &[f32], oy: usize, ox: usize, w: usize, h: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(w * h);
    for y in 0..h {
        let start = (oy + y) * SRC + ox;
        v.extend_from_slice(&src[start..start + w]);
    }
    v
}

/// The reference crop and a crop whose content sits `dy` rows and `dx` columns
/// further down and right — the sign convention [`Shift`] documents.
///
/// Content moving *down* by `dy` means the window has to be taken from `dy` rows
/// *higher* in the sample, so the offsets subtract. Getting this backwards in the
/// test would hide a sign error in the code rather than catch it, so it is worth
/// reading twice.
fn pair(dy: i32, dx: i32, n: usize) -> (Vec<f32>, Vec<f32>) {
    let src = sample(2, 1000.0);
    let base = (SRC - n) as i32 / 2;
    let reference = crop(&src, base as usize, base as usize, n, n);
    let moved = crop(&src, (base - dy) as usize, (base - dx) as usize, n, n);
    (reference, moved)
}

/// A 5 px taper on a 64 px frame — the same *proportion* of the frame as the
/// configured 5 px on a real 512 px frame is not achievable at this size, so this
/// tapers rather harder than production does. `max_shift` is a quarter of the
/// frame and `min_peak` is above the ~4 an uncorrelated pair reaches.
fn registrar(n: usize) -> Registrar {
    Registrar::new(n, n, 5.0, 16, 6.0)
}

#[test]
fn measures_a_known_shift_in_both_signs_of_both_axes() {
    let n = 64usize;
    for &(dy, dx) in &[
        (0, 0),
        (5, 0),
        (-5, 0),
        (0, 7),
        (0, -7),
        (6, 9),
        (-6, 9),
        (6, -9),
        (-11, -13),
    ] {
        let (reference, moved) = pair(dy, dx, n);
        let mut reg = registrar(n);
        reg.set_reference(&reference);
        let s = reg.shift(&moved);
        assert!(
            s.trusted,
            "({dy},{dx}) came back untrusted, peak {}",
            s.peak
        );
        assert_eq!(
            (s.dy, s.dx),
            (dy, dx),
            "wanted ({dy},{dx}), got ({},{}) at peak {}",
            s.dy,
            s.dx,
            s.peak
        );
    }
}

#[test]
fn brightness_does_not_change_the_answer() {
    // The reason for normalising every bin to unit magnitude. The reference is
    // taken minutes before the frame it is compared with, and in between the
    // fluorophore has bleached: the frame is dimmer, and sits on a different
    // offset. Neither may move the measured shift by a pixel.
    let n = 64usize;
    let (reference, moved) = pair(-4, 6, n);
    let dimmed: Vec<f32> = moved.iter().map(|v| v * 0.35 + 420.0).collect();

    let mut reg = registrar(n);
    reg.set_reference(&reference);
    let plain = reg.shift(&moved);
    let scaled = reg.shift(&dimmed);

    assert!(plain.trusted && scaled.trusted);
    assert_eq!((plain.dy, plain.dx), (-4, 6));
    assert_eq!(
        (scaled.dy, scaled.dx),
        (plain.dy, plain.dx),
        "a third as bright and offset moved the answer to ({},{})",
        scaled.dy,
        scaled.dx
    );
}

/// Unsmoothed noise: two of these are as unrelated as two frames can be.
fn noise(n: usize, seed: u32) -> Vec<f32> {
    let mut s = seed | 1;
    (0..n * n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            1000.0 + (s >> 8) as f32 / 55_924.0
        })
        .collect()
}

#[test]
fn an_uncorrelated_pair_is_untrusted() {
    let n = 64usize;
    let mut reg = registrar(n);
    reg.set_reference(&noise(n, 12345));
    let s = reg.shift(&noise(n, 98765));

    assert!(
        !s.trusted,
        "two unrelated frames matched at ({},{}) with peak {}",
        s.dy, s.dx, s.peak
    );
    // Untrusted means the shift is zeroed, not merely flagged: the caller logs
    // both, and a nonzero shift on an untrusted row would be read as a real
    // measurement by whoever reads the log afterwards.
    assert_eq!((s.dy, s.dx), (0, 0));

    // And the peak really is feeble next to a true match, which is what makes
    // `min_peak` a usable knob rather than a guess.
    let (reference, moved) = pair(3, 3, n);
    let mut good = registrar(n);
    good.set_reference(&reference);
    let g = good.shift(&moved);
    assert!(
        g.peak > 3.0 * s.peak,
        "a true match peaked at {} and noise at {} — too close to separate",
        g.peak,
        s.peak
    );
}

#[test]
fn a_shift_larger_than_max_shift_is_untrusted() {
    let n = 64usize;
    let (reference, moved) = pair(20, 0, n);
    // A real, strong, correctly located 20 px peak with `max_shift` at 8. It must
    // still be rejected: the caller's measurement window is inset by `max_shift`
    // and cannot be moved 20 px, so there is nothing useful to be done with it.
    let mut reg = Registrar::new(n, n, 5.0, 8, 6.0);
    reg.set_reference(&reference);
    let s = reg.shift(&moved);
    assert!(!s.trusted);
    assert_eq!((s.dy, s.dx), (0, 0));
    assert!(s.peak > 6.0, "the peak itself was fine: {}", s.peak);
}

#[test]
fn weak_peaks_are_rejected_by_min_peak() {
    let n = 64usize;
    let (reference, moved) = pair(2, -3, n);
    // The same pair and the same everything, but a threshold nothing can clear.
    // Isolates the `min_peak` branch from the `max_shift` one.
    let mut reg = Registrar::new(n, n, 5.0, 16, 1.0e9);
    reg.set_reference(&reference);
    let s = reg.shift(&moved);
    assert!(!s.trusted);
    assert_eq!((s.dy, s.dx), (0, 0));
    assert!(
        s.peak > 0.0,
        "the peak should still be reported, for the log"
    );
}

#[test]
fn the_taper_is_doing_something() {
    // The failure the taper exists to prevent, reproduced. Content that is
    // low-pass — twelve passes of smoothing here, a diffraction-limited and
    // slightly defocused two-photon frame in life — has real signal only in the
    // low-frequency bins, and those are exactly the bins the frame's wrap-around
    // edge step lands in. The step is in the same place in both frames, so it
    // votes for zero shift, and with nothing else competing at those frequencies
    // it wins.
    //
    // Untapered, this pair reports "no movement" with a confident peak while the
    // sample has really moved five rows. That is the most dangerous way for the
    // whole program to fail, because a stabiliser that believes nothing moved does
    // nothing and says everything is fine.
    let n = 64usize;
    let (dy, dx) = (5i32, 0i32);
    let src = sample(12, 1000.0);
    let base = (SRC - n) as i32 / 2;
    let reference = crop(&src, base as usize, base as usize, n, n);
    let moved = crop(&src, (base - dy) as usize, (base - dx) as usize, n, n);

    let mut tapered = Registrar::new(n, n, 5.0, 16, 0.0);
    tapered.set_reference(&reference);
    let with = tapered.shift(&moved);

    // `taper_px` of zero is no taper at all, which is the untapered case.
    let mut bare = Registrar::new(n, n, 0.0, 16, 0.0);
    bare.set_reference(&reference);
    let without = bare.shift(&moved);

    assert_eq!(
        (with.dy, with.dx),
        (dy, dx),
        "the tapered measurement should be exact, got ({},{}) peak {}",
        with.dy,
        with.dx,
        with.peak
    );
    assert_ne!(
        (without.dy, without.dx),
        (dy, dx),
        "the untapered measurement was expected to be wrong and was not; \
         if the taper has stopped mattering, this test has stopped testing it"
    );
    // Specifically wrong in the dangerous direction: within a pixel of no
    // movement at all, and confident about it.
    assert!(
        without.dy.abs() <= 1 && without.dx.abs() <= 1,
        "expected the untapered failure to be a peak at zero shift, got ({},{})",
        without.dy,
        without.dx
    );
    assert!(
        without.peak > 10.0,
        "and expected it to be confidently wrong, peak was {}",
        without.peak
    );
}

#[test]
fn the_mean_is_removed_before_tapering() {
    // Same pair twice, the second on a large pedestal. A pedestal is not
    // information — it is dark offset and haze — and it must not change the
    // answer. It does change it, badly, if the frame is tapered without taking
    // the mean off first: `taper * constant` is a broadband shape that is
    // identical in every frame and stationary, so after whitening it votes for
    // zero shift in every bin.
    let n = 64usize;
    let src = sample(4, 0.0);
    let base = (SRC - n) as i32 / 2;
    let (dy, dx) = (5i32, 3i32);
    let flat_ref = crop(&src, base as usize, base as usize, n, n);
    let flat_moved = crop(&src, (base - dy) as usize, (base - dx) as usize, n, n);
    let high_ref: Vec<f32> = flat_ref.iter().map(|v| v + 4000.0).collect();
    let high_moved: Vec<f32> = flat_moved.iter().map(|v| v + 4000.0).collect();

    let mut flat = registrar(n);
    flat.set_reference(&flat_ref);
    let a = flat.shift(&flat_moved);

    let mut high = registrar(n);
    high.set_reference(&high_ref);
    let b = high.shift(&high_moved);

    assert_eq!((a.dy, a.dx), (dy, dx));
    assert_eq!(
        (b.dy, b.dx),
        (dy, dx),
        "a pedestal moved the answer to ({},{}) at peak {}",
        b.dy,
        b.dx,
        b.peak
    );
    // The peak should barely notice the pedestal either.
    assert!(
        (a.peak - b.peak).abs() < 0.25 * a.peak,
        "peak {} without a pedestal, {} with one",
        a.peak,
        b.peak
    );
}

#[test]
fn identical_frames_give_a_perfect_peak_at_zero() {
    // A whitened self-correlation is a single nonzero bin, so the peak over the
    // mean is the number of bins. A cheap check that the whitening really does
    // normalise and that nothing is smearing the surface.
    let n = 32usize;
    let (reference, _) = pair(0, 0, n);
    let mut reg = registrar(n);
    reg.set_reference(&reference);
    let s = reg.shift(&reference);
    assert!(s.trusted);
    assert_eq!((s.dy, s.dx), (0, 0));
    assert!(
        s.peak > 0.9 * (n * n) as f32,
        "a frame against itself peaked at only {}",
        s.peak
    );
}

#[test]
fn no_reference_means_untrusted_rather_than_a_panic() {
    let n = 32usize;
    let mut reg = registrar(n);
    assert!(!reg.has_reference());
    let s = reg.shift(&vec![1.0; n * n]);
    assert!(!s.trusted);
    assert_eq!((s.dy, s.dx), (0, 0));

    reg.set_reference(&vec![1.0; n * n]);
    assert!(reg.has_reference());
}

#[test]
fn a_wrongly_sized_frame_is_untrusted_rather_than_a_panic() {
    let n = 32usize;
    let mut reg = registrar(n);
    reg.set_reference(&vec![1.0; n * n]);
    assert!(!reg.shift(&vec![1.0; 10]).trusted);
    // And a short reference is refused rather than half-installed.
    let mut other = registrar(n);
    other.set_reference(&vec![1.0; 10]);
    assert!(!other.has_reference());
}

#[test]
fn a_blank_frame_does_not_divide_by_zero() {
    // Every bin of the cross-power is zero, so every bin hits the guard. The guard
    // must send them to zero rather than to unit magnitude: unit magnitude is the
    // phase of zero shift, and a surface built entirely out of it would be a
    // confident peak at (0,0) invented from nothing.
    let n = 16usize;
    let mut reg = registrar(n);
    reg.set_reference(&vec![7.0; n * n]);
    let s = reg.shift(&vec![7.0; n * n]);
    assert!(!s.trusted, "a blank pair matched with peak {}", s.peak);
    assert_eq!((s.dy, s.dx), (0, 0));
    assert!(s.peak.is_finite(), "peak was {}", s.peak);
}

#[test]
fn aligned_window_copies_the_overlapping_pixels() {
    // A frame numbered by position, so a mis-taken window is obvious rather than
    // merely different.
    let (w, h, margin) = (10usize, 8usize, 2usize);
    let frame: Vec<f32> = (0..w * h).map(|i| i as f32).collect();

    let shift = Shift {
        dy: 1,
        dx: -1,
        peak: 9.0,
        trusted: true,
    };
    let (win, ww, wh) = aligned_window(&frame, w, h, shift, margin).expect("within the margin");
    assert_eq!((ww, wh), (w - 2 * margin, h - 2 * margin));

    // Content sits one row lower and one column further left than the reference's,
    // so the window is read from row margin+1, column margin-1.
    for j in 0..wh {
        for i in 0..ww {
            let want = ((margin + 1 + j) * w + (margin - 1 + i)) as f32;
            assert_eq!(win[j * ww + i], want, "window pixel ({j},{i})");
        }
    }
}

#[test]
fn aligned_window_of_no_shift_is_the_plain_inset() {
    let (w, h, margin) = (7usize, 6usize, 1usize);
    let frame: Vec<f32> = (0..w * h).map(|i| i as f32).collect();
    let (win, ww, wh) =
        aligned_window(&frame, w, h, Shift::default(), margin).expect("zero shift always fits");
    assert_eq!((ww, wh), (5, 4));
    assert_eq!(win[0], (w + 1) as f32);
    assert_eq!(win[ww * wh - 1], ((h - 2) * w + (w - 2)) as f32);
}

#[test]
fn aligned_window_refuses_past_the_margin() {
    let (w, h, margin) = (16usize, 16usize, 3usize);
    let frame: Vec<f32> = (0..w * h).map(|i| i as f32).collect();

    // Exactly on the margin is still inside both frames.
    for &(dy, dx) in &[(3, 0), (-3, 0), (0, 3), (0, -3), (3, -3), (-3, 3)] {
        let s = Shift {
            dy,
            dx,
            peak: 9.0,
            trusted: true,
        };
        assert!(
            aligned_window(&frame, w, h, s, margin).is_some(),
            "({dy},{dx}) is on the margin and should fit"
        );
    }
    // One pixel past it is not, and must be refused rather than padded.
    for &(dy, dx) in &[(4, 0), (-4, 0), (0, 4), (0, -4), (9, 9)] {
        let s = Shift {
            dy,
            dx,
            peak: 9.0,
            trusted: true,
        };
        assert!(
            aligned_window(&frame, w, h, s, margin).is_none(),
            "({dy},{dx}) is past the margin and should be refused"
        );
    }
}

#[test]
fn aligned_window_refuses_a_frame_too_small_for_the_margin() {
    let frame = vec![0.0f32; 8 * 8];
    // Nothing left after insetting: a zero-sized window would otherwise reach the
    // focus metric, which divides by its area.
    assert!(aligned_window(&frame, 8, 8, Shift::default(), 4).is_none());
    // And a buffer that is not the size it claims to be.
    assert!(aligned_window(&frame, 9, 8, Shift::default(), 1).is_none());
}

#[test]
fn a_registration_and_window_round_trip_recovers_the_same_tissue() {
    // The two halves together: measure the shift, take the window, and the window
    // from the moved frame must hold the same sample values as the window from the
    // reference at zero shift. That is the property the focus metric depends on,
    // and neither half proves it alone — a sign error in either one cancels
    // nothing, and doubles the movement instead.
    let n = 64usize;
    let margin = 16usize;
    let (dy, dx) = (-9i32, 12i32);
    let (reference, moved) = pair(dy, dx, n);

    let mut reg = registrar(n);
    reg.set_reference(&reference);
    let s = reg.shift(&moved);
    assert!(s.trusted);
    assert_eq!((s.dy, s.dx), (dy, dx));

    let (win, ww, wh) = aligned_window(&moved, n, n, s, margin).expect("within the margin");
    let (base, _, _) =
        aligned_window(&reference, n, n, Shift::default(), margin).expect("zero shift fits");
    assert_eq!((ww, wh), (n - 2 * margin, n - 2 * margin));
    assert_eq!(win.len(), base.len());
    for (i, (a, b)) in win.iter().zip(base.iter()).enumerate() {
        assert!(
            (a - b).abs() < 1e-3,
            "pixel {i} of the aligned window is {a}, the reference has {b}"
        );
    }
}
