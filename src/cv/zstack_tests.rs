//! Tests for the reference-stack match.
//!
//! [`ZStack::load`] itself is not tested here: it needs an OIR on disk and the
//! whole [`crate::oir`] reader behind it, so it belongs to the integration tests.
//! What is tested is everything that decides whether the number it produces is
//! *right* — the matching, the interpolation and the normalisation — against a
//! synthetic stack whose true answer is known.
//!
//! The stack is built by construction rather than loaded, which the public fields
//! of [`ZStack`] allow. That is deliberate: it keeps this file from depending on a
//! file format, and it means a failure here is a failure of the matching and
//! nothing else.

use super::*;
use crate::config::Config;
use crate::oir::Geometry;

const W: usize = 48;
const H: usize = 48;

struct Lcg(u64);

impl Lcg {
    fn next_f(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as f64 / (1u64 << 31) as f64
    }
}

/// A fluorescent point somewhere in the tissue.
struct Source {
    x: f64,
    y: f64,
    z: f64,
    amp: f64,
}

/// Sources spread through a volume ten planes deep.
///
/// Spread in *z* as well as x and y, which is the whole point: a synthetic stack
/// whose planes differ only by how blurred one picture is has two planes for every
/// blur — one either side of focus — and no amount of correlation can tell them
/// apart. Real tissue has different structure at different depths, and that is
/// what gives the match its sign.
fn volume() -> Vec<Source> {
    let mut rng = Lcg(0xbeef_0f1e_2d3c_4b5a);
    (0..70)
        .map(|_| Source {
            x: rng.next_f() * (W as f64 - 1.0),
            y: rng.next_f() * (H as f64 - 1.0),
            z: rng.next_f() * 10.0,
            amp: 200.0 + 600.0 * rng.next_f(),
        })
        .collect()
}

/// What the microscope would see with its focal plane at `z`.
///
/// A source out of focus by `dz` planes spreads to `sqrt(s0² + (k dz)²)` and its
/// peak falls as `1/sigma²`, so the light is conserved rather than invented. That
/// second part matters: a model that blurred without dimming would let a metric
/// that only looks at brightness pass a test it should fail.
fn plane_at(z: f64, srcs: &[Source]) -> Vec<f32> {
    const S0: f64 = 0.9;
    const K: f64 = 0.8;
    let mut img = vec![80.0f32; W * H];
    for s in srcs {
        let dz = z - s.z;
        let sigma = (S0 * S0 + (K * dz) * (K * dz)).sqrt();
        let two_s2 = 2.0 * sigma * sigma;
        let peak = s.amp / (sigma * sigma);
        let reach = (3.5 * sigma).ceil() as i64;
        let cx = s.x.round() as i64;
        let cy = s.y.round() as i64;
        for yy in (cy - reach).max(0)..=(cy + reach).min(H as i64 - 1) {
            for xx in (cx - reach).max(0)..=(cx + reach).min(W as i64 - 1) {
                let fx = xx as f64 - s.x;
                let fy = yy as f64 - s.y;
                img[yy as usize * W + xx as usize] +=
                    (peak * (-(fx * fx + fy * fy) / two_s2).exp()) as f32;
            }
        }
    }
    img
}

/// An eleven-plane stack at 1 µm, planes at z = 0..10, so the centre plane is
/// z = 5 and its offset is 0.
fn stack(srcs: &[Source]) -> ZStack {
    let planes = (0..11)
        .map(|i| StackPlane {
            offset_um: i as f64 - 5.0,
            normalised: ZStack::normalise(&plane_at(i as f64, srcs)),
        })
        .collect();
    ZStack {
        width: W,
        height: H,
        step_um: 1.0,
        planes,
    }
}

#[test]
fn a_plane_of_the_stack_matches_itself_exactly() {
    let srcs = volume();
    let zs = stack(&srcs);
    for i in 0..zs.planes.len() {
        let win = plane_at(i as f64, &srcs);
        let m = zs.locate(&win, 0.3).expect("a plane matches its own stack");
        assert_eq!(m.best_plane, i, "plane {i} matched plane {}", m.best_plane);
        assert!(
            m.score > 0.999,
            "a plane against itself is a correlation of 1, got {}",
            m.score
        );
        // The interpolation must not move a peak that is already on a plane by
        // much — but "by much" is a quarter of a plane, not nothing, and that is
        // worth knowing rather than asserting away. The parabola is fitted to the
        // two neighbours' scores, and those are only equal when the sample's
        // structure is symmetric about this plane. Real tissue is not, so a plane
        // with more structure below it than above reads slightly low. Measured on
        // this volume the bias is under 0.21 of a plane, and it is bounded by the
        // real thing it comes from, so a stack stepped fine enough to matter is
        // also a stack whose neighbours are more nearly symmetric.
        assert!(
            (m.offset_um - (i as f64 - 5.0)).abs() < 0.25,
            "plane {i} came out at {} µm",
            m.offset_um
        );
    }
}

/// The sign, which is the entire reason this mode exists.
#[test]
fn a_window_either_side_of_focus_gets_the_right_sign() {
    let srcs = volume();
    let zs = stack(&srcs);

    let below = zs.locate(&plane_at(3.0, &srcs), 0.3).expect("matched");
    assert!(
        below.offset_um < -1.5,
        "a focal plane two microns below centre must read about -2, got {}",
        below.offset_um
    );
    let above = zs.locate(&plane_at(7.0, &srcs), 0.3).expect("matched");
    assert!(
        above.offset_um > 1.5,
        "a focal plane two microns above centre must read about +2, got {}",
        above.offset_um
    );
    assert!(below.offset_um < above.offset_um);
}

/// Sub-plane accuracy: a window taken between two planes must read as being
/// between them, or a 1 µm stack could never resolve better than 1 µm.
#[test]
fn a_window_between_planes_is_interpolated() {
    let srcs = volume();
    let zs = stack(&srcs);
    for (z, want) in [(5.4, 0.4), (4.5, -0.5), (6.75, 1.75)] {
        let m = zs.locate(&plane_at(z, &srcs), 0.3).expect("matched");
        // A tenth of a plane. Measured, not hoped for: on this volume the three
        // cases come out at 0.408, -0.488 and 1.738 — the interpolation between
        // planes is far more accurate than the pull it puts on a plane that is
        // already the answer, because here the neighbours' asymmetry is the signal
        // rather than an error in it.
        assert!(
            (m.offset_um - want).abs() < 0.1,
            "a window at z = {z} should read {want} µm, got {} (best plane {})",
            m.offset_um,
            m.best_plane
        );
        // And it must not be the bare plane index, which is what a broken
        // interpolation would leave behind.
        assert!(
            (m.offset_um.fract()).abs() > 1e-6 || want.fract() == 0.0,
            "the offset {} is suspiciously exactly a plane",
            m.offset_um
        );
    }
}

/// A field of view the stack was not taken of must be refused, not guessed at.
#[test]
fn an_unrelated_window_scores_below_the_threshold() {
    let srcs = volume();
    let zs = stack(&srcs);
    let mut rng = Lcg(0x1234_5678_9abc_def0);
    let noise: Vec<f32> = (0..W * H).map(|_| (rng.next_f() * 500.0) as f32).collect();
    let score = zs
        .locate(&noise, -2.0)
        .expect("a threshold below -1 accepts anything")
        .score;
    assert!(
        score.abs() < 0.3,
        "unrelated noise correlated {score} with the stack"
    );
    assert!(
        zs.locate(&noise, 0.3).is_none(),
        "a score of {score} is below min_score and must be refused"
    );
}

#[test]
fn a_window_of_the_wrong_size_is_refused() {
    let zs = stack(&volume());
    assert!(zs.locate(&vec![1.0; W * H - 1], -2.0).is_none());
    assert!(zs.locate(&[], -2.0).is_none());
}

#[test]
fn an_empty_stack_matches_nothing() {
    let zs = ZStack {
        width: W,
        height: H,
        step_um: 1.0,
        planes: Vec::new(),
    };
    assert!(zs.locate(&vec![1.0; W * H], -2.0).is_none());
}

#[test]
fn normalise_gives_zero_mean_and_unit_norm() {
    let srcs = volume();
    let win = plane_at(4.0, &srcs);
    let n = ZStack::normalise(&win);
    assert_eq!(n.len(), win.len());
    let mean: f64 = n.iter().map(|&v| v as f64).sum::<f64>() / n.len() as f64;
    let norm: f64 = n.iter().map(|&v| v as f64 * v as f64).sum::<f64>();
    assert!(mean.abs() < 1e-6, "mean {mean}");
    assert!((norm - 1.0).abs() < 1e-5, "squared norm {norm}");

    // Scale and offset invariance, which is what makes the score a correlation:
    // the same field, dimmer and with a pedestal, normalises to the same vector.
    let changed: Vec<f32> = win.iter().map(|&v| 0.4 * v + 37.0).collect();
    let m = ZStack::normalise(&changed);
    for (a, b) in n.iter().zip(m.iter()) {
        assert!((a - b).abs() < 1e-4, "{a} vs {b}");
    }
}

#[test]
fn normalise_of_a_featureless_window_is_zeros_not_nans() {
    let flat = ZStack::normalise(&vec![512.0f32; 64]);
    assert!(flat.iter().all(|&v| v == 0.0));
    assert!(ZStack::normalise(&[]).is_empty());
    // Which then scores zero against everything, and so is refused.
    let zs = stack(&volume());
    assert!(zs.locate(&vec![512.0f32; W * H], 0.3).is_none());
}

/// The parabola, against one whose peak is known by construction.
#[test]
fn parabolic_peak_finds_the_vertex_of_a_known_parabola() {
    // y = -3 (x - 0.25)² + 2, sampled at -1, 0, 1.
    let f = |x: f64| -3.0 * (x - 0.25) * (x - 0.25) + 2.0;
    let got = parabolic_peak(f(-1.0) as f32, f(0.0) as f32, f(1.0) as f32);
    assert!((got - 0.25).abs() < 1e-5, "got {got}");

    // And the other way, to be sure the sign is not mirrored.
    let g = |x: f64| -(x + 0.5) * (x + 0.5);
    let got = parabolic_peak(g(-1.0) as f32, g(0.0) as f32, g(1.0) as f32);
    assert!((got + 0.5).abs() < 1e-5, "got {got}");
}

#[test]
fn parabolic_peak_returns_zero_when_there_is_no_peak() {
    // A valley: the fit has a minimum, not a maximum.
    assert_eq!(parabolic_peak(5.0, 1.0, 5.0), 0.0);
    // Flat. The denominator would be zero.
    assert_eq!(parabolic_peak(2.0, 2.0, 2.0), 0.0);
    assert_eq!(parabolic_peak(0.0, 0.0, 0.0), 0.0);
    // A straight line has no vertex at all.
    assert_eq!(parabolic_peak(0.0, 1.0, 2.0), 0.0);
    // Symmetric about the middle: the middle is the answer.
    assert_eq!(parabolic_peak(0.0, 1.0, 0.0), 0.0);
}

#[test]
fn parabolic_peak_stays_within_one_plane() {
    // A vertex far outside the three samples, which cannot be trusted that far:
    // the answer is clamped to the neighbouring plane rather than extrapolated.
    let got = parabolic_peak(0.0, 0.6, 1.0);
    assert!((-1.0..=1.0).contains(&got), "got {got}");
    assert!(
        (got - 1.0).abs() < 1e-9,
        "the clamp should bite here, got {got}"
    );
    assert!(parabolic_peak(1.0, 0.6, 0.0) >= -1.0);
}

/// `load` refuses a config that names no stack, before it opens anything.
#[test]
fn load_without_a_path_is_an_error_not_a_panic() {
    let cfg = Config::default();
    assert!(cfg.control.reference_stack.path.is_none());
    let geom = Geometry {
        width: 512,
        height: 512,
        depth: 2,
    };
    // Not `expect_err`, which would want `ZStack: Debug`, and deriving that would
    // print a megabyte of pixels.
    let err = match ZStack::load(&cfg, geom) {
        Ok(_) => panic!("a config naming no stack must not produce one"),
        Err(e) => e,
    };
    assert!(
        err.contains("reference_stack.path"),
        "unhelpful error: {err}"
    );
}
