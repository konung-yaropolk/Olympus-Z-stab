//! The image processing: an FFT, a phase correlation, some focus metrics, and a
//! correlation against a reference z-stack.
//!
//! # Why there is no OpenCV here
//!
//! The obvious answer to "OpenCV, but in Rust" is the `opencv` crate, and it is
//! the wrong answer for this program. It binds the real C++ OpenCV, which has to
//! be present to build and shipped as DLLs beside the exe to run — on a Windows 7
//! machine that cannot be given a modern toolchain, next to acquisition software
//! nobody wants to risk disturbing. The alternatives that are pure Rust
//! (`imageproc`, `ndarray`) are real options, but this needs four operations:
//! a 2-D FFT, a phase correlation, a couple of gradient sums and a normalised
//! cross-correlation. Each is a page. `rustfft` supplies the only part that is
//! genuinely hard to write, and it is MIT/Apache and builds on Rust 1.77.
//!
//! So this module *is* the CV library, sized to the job.
//!
//! # The order operations happen in, and why it cannot change
//!
//! 1. Cancel the X/Y movement — measured by phase correlation, applied as a
//!    **whole number of pixels**.
//! 2. Take the window the shifted frame and the reference have in common.
//! 3. Measure focus on that window.
//!
//! Step 1 must not interpolate. Resampling a frame to apply a sub-pixel shift
//! smooths it, and smoothing is precisely what a focus metric measures; a
//! stabiliser that interpolated would read its own interpolation as defocus and
//! chase it. The sub-pixel remainder is left in, and is harmless: it is a
//! fraction of a pixel of blur, constant in expectation, against a signal that
//! is a percentage change in high-frequency power.
//!
//! Step 2 matters because a shifted frame has a strip along one edge that the
//! reference never saw. Measuring focus over that strip compares tissue against
//! nothing.

pub mod fft;
pub mod metrics;
pub mod register;
pub mod zstack;

pub use metrics::FocusMeter;
pub use register::{aligned_window, Registrar, Shift};
