//! A 2-D FFT over `rustfft`, with its plans kept.
//!
//! `rustfft` plans a transform once and reuses it; re-planning per frame is most
//! of the cost of an FFT this size. At 7.5 Hz with a 512x512 frame there is
//! plenty of time either way, but the plans also have to be reused because the
//! program runs on a machine whose processor is fifteen years old.
//!
//! Real input is transformed as complex with a zero imaginary part rather than
//! with a real-to-complex transform. Twice the work and half the code, and the
//! work is 12 ms where the frame interval is 133 ms.

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// A planned 2-D FFT for one frame size.
pub struct Fft2d {
    pub width: usize,
    pub height: usize,
    row_fwd: Arc<dyn Fft<f32>>,
    col_fwd: Arc<dyn Fft<f32>>,
    row_inv: Arc<dyn Fft<f32>>,
    col_inv: Arc<dyn Fft<f32>>,
    /// Scratch for the column pass, so a transform does not allocate.
    ///
    /// Two things share this one buffer: the first `height` entries are where a
    /// column is gathered to, because `rustfft` 6 transforms a contiguous slice
    /// and has no strided entry point, and everything after that is the scratch
    /// the algorithm itself asks for via `get_inplace_scratch_len`.
    scratch: Vec<Complex32>,
}

impl Fft2d {
    pub fn new(width: usize, height: usize) -> Fft2d {
        let mut planner = FftPlanner::new();
        // Four plans, not two: in `rustfft` the direction is baked into the
        // planned instance, so an inverse transform is a different object rather
        // than a flag at call time. The planner caches by (length, direction),
        // so the row and column plans of equal length are shared behind the Arc.
        let row_fwd = planner.plan_fft_forward(width.max(1));
        let col_fwd = planner.plan_fft_forward(height.max(1));
        let row_inv = planner.plan_fft_inverse(width.max(1));
        let col_inv = planner.plan_fft_inverse(height.max(1));

        // `process_with_scratch` panics if given less than it asked for, and the
        // amount differs per algorithm — a Bluestein plan for an awkward ROI width
        // wants far more than the radix-4 plan a 512 gets. Take the largest of the
        // four so one buffer serves every pass.
        let algo = row_fwd
            .get_inplace_scratch_len()
            .max(col_fwd.get_inplace_scratch_len())
            .max(row_inv.get_inplace_scratch_len())
            .max(col_inv.get_inplace_scratch_len());

        Fft2d {
            width,
            height,
            row_fwd,
            col_fwd,
            row_inv,
            col_inv,
            scratch: vec![Complex32::new(0.0, 0.0); height + algo],
        }
    }

    /// Real samples to a complex spectrum, row-major, in place.
    ///
    /// `buf` must be `width * height` long.
    pub fn forward(&mut self, buf: &mut [Complex32]) {
        // Destructured rather than `self.row_fwd` / `self.scratch` in place: the
        // pass needs the plans immutably and the scratch mutably at the same
        // time, and splitting the fields is how borrowck is told those are
        // disjoint without cloning an Arc per frame.
        let Fft2d {
            width,
            height,
            row_fwd,
            col_fwd,
            scratch,
            ..
        } = self;
        pass(*width, *height, row_fwd, col_fwd, scratch, buf);
    }

    /// The inverse, **unnormalised** — `rustfft` does not scale, and neither does
    /// this. Phase correlation only ever wants the location of the peak, so the
    /// scale never matters; anything that does want it must divide by
    /// `width * height` itself.
    pub fn inverse(&mut self, buf: &mut [Complex32]) {
        let Fft2d {
            width,
            height,
            row_inv,
            col_inv,
            scratch,
            ..
        } = self;
        pass(*width, *height, row_inv, col_inv, scratch, buf);
    }

    /// `real` as a complex buffer, imaginary parts zeroed.
    #[allow(dead_code)] // used by the tests; part of the type's contract.
    pub fn to_complex(real: &[f32]) -> Vec<Complex32> {
        real.iter().map(|&v| Complex32::new(v, 0.0)).collect()
    }
}

/// Rows then columns, with whichever pair of plans was handed in.
///
/// A 2-D DFT is separable, so the direction lives entirely in the plans and this
/// is the same code for forward and inverse. The row pass is over contiguous
/// memory; the column pass cannot be, so each column is gathered into the front
/// of `scratch`, transformed, and scattered back. Gathering beats transposing the
/// whole frame twice: one column is 2 kB and stays in cache, where a 512x512
/// transpose touches 1 MB with a stride that defeats the prefetcher on the
/// fifteen-year-old processor this has to run on.
fn pass(
    width: usize,
    height: usize,
    rows: &Arc<dyn Fft<f32>>,
    cols: &Arc<dyn Fft<f32>>,
    scratch: &mut [Complex32],
    buf: &mut [Complex32],
) {
    assert_eq!(
        buf.len(),
        width * height,
        "Fft2d is planned for {width}x{height} and was given {} samples",
        buf.len()
    );
    if width == 0 || height == 0 {
        return;
    }

    let (column, algo) = scratch.split_at_mut(height);

    for row in buf.chunks_exact_mut(width) {
        rows.process_with_scratch(row, algo);
    }

    for x in 0..width {
        for (y, slot) in column.iter_mut().enumerate() {
            *slot = buf[y * width + x];
        }
        cols.process_with_scratch(column, algo);
        for (y, &v) in column.iter().enumerate() {
            buf[y * width + x] = v;
        }
    }
}

#[cfg(test)]
#[path = "fft_tests.rs"]
mod fft_tests;
