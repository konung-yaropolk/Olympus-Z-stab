//! Tests for the console rendering and the adjustment bookkeeping.
//!
//! The console line has a width budget — it has to fit a window squeezed beside
//! the acquisition software — and a budget that nothing checks is a budget that
//! drifts. [`every_state_fits_the_column`] is the test that keeps it.

use super::*;

/// What the narrow layout allows. The header is `Logger::header()`, and the
/// fields before `state` occupy the rest.
const LINE_BUDGET: usize = 48;
const STATE_BUDGET: usize = 12;

// ---------------------------------------------------------- the short labels

fn short_of(d: Decision) -> String {
    describe_short(&d, None)
}

#[test]
fn every_state_fits_the_column() {
    // Every variant, including the ones with numbers in them at their widest.
    let states = [
        short_of(Decision::Hold(Hold::BuildingReference {
            have: 999,
            need: 999,
        })),
        short_of(Decision::Hold(Hold::BuildingWindow {
            have: 999,
            need: 999,
        })),
        short_of(Decision::Hold(Hold::InBand { metric_rel: 0.97 })),
        short_of(Decision::Hold(Hold::Confirming {
            windows: 99,
            need: 99,
        })),
        short_of(Decision::Hold(Hold::Cooldown { remaining_s: 120.0 })),
        short_of(Decision::Hold(Hold::Verifying)),
        short_of(Decision::Move {
            steps: -99,
            reason: "a reason that never reaches the console".into(),
        }),
        short_of(Decision::Stop("a long explanation".into())),
        describe_short(&Decision::Hold(Hold::Verifying), Some("xy_match_failed")),
        describe_short(&Decision::Hold(Hold::Verifying), Some("saturated")),
        describe_short(
            &Decision::Hold(Hold::Verifying),
            Some("shift_beyond_margin"),
        ),
        describe_short(&Decision::Hold(Hold::Verifying), Some("something_new")),
    ];
    for s in &states {
        assert!(
            s.len() <= STATE_BUDGET,
            "{s:?} is {} characters, over the {STATE_BUDGET} the column allows",
            s.len()
        );
        assert!(!s.is_empty(), "a state must say something");
    }
}

#[test]
fn the_header_fits_the_budget() {
    let h = log::Logger::header();
    assert!(
        h.len() + STATE_BUDGET <= LINE_BUDGET + STATE_BUDGET,
        "the header is {} characters",
        h.len()
    );
    // The columns the rows actually fill, in order.
    for want in ["t", "rel", "dx", "dy", "dz", "state"] {
        assert!(h.contains(want), "the header should name {want:?}: {h:?}");
    }
}

#[test]
fn a_rejected_frame_reports_why_not_the_decision() {
    // The controller holds for its own reasons on a frame that was never
    // measured; what the user needs to see is that the frame was skipped.
    let d = Decision::Hold(Hold::InBand { metric_rel: 1.0 });
    assert_eq!(describe_short(&d, Some("saturated")), "skip sat");
    assert_eq!(describe_short(&d, Some("xy_match_failed")), "skip xy");
}

#[test]
fn a_correction_is_shouted_and_a_hold_is_not() {
    // A screen of `ok` with one correction in it has to be scannable by eye.
    assert_eq!(
        short_of(Decision::Hold(Hold::InBand { metric_rel: 1.0 })),
        "ok"
    );
    assert_eq!(
        short_of(Decision::Move {
            steps: 2,
            reason: String::new()
        }),
        "MOVE +2"
    );
    assert_eq!(
        short_of(Decision::Move {
            steps: -1,
            reason: String::new()
        }),
        "MOVE -1"
    );
    assert_eq!(short_of(Decision::Stop(String::new())), "STOP");
}

#[test]
fn the_long_form_keeps_what_the_short_one_drops() {
    // The CSV is the record, so the reason a correction was made has to survive
    // there even though it never reaches the console.
    let d = Decision::Move {
        steps: 1,
        reason: "metric fell 24%".into(),
    };
    let long = describe(&d, None);
    assert!(long.contains("metric fell 24%"), "{long:?}");
    assert!(!describe_short(&d, None).contains("metric"));
}

// ----------------------------------------------------------------- direction

#[test]
fn direction_is_named_after_the_sequence_that_runs() {
    // Positive steps run `z_up`, so positive is up. Getting this backwards would
    // print the opposite of what the stage did.
    assert_eq!(direction(1), "up");
    assert_eq!(direction(3), "up");
    assert_eq!(direction(-1), "down");
    // Zero never reaches a summary, but it must not be called "down".
    assert_eq!(direction(0), "up");
}

// ---------------------------------------------------------- the adjustments

fn adj(steps: i32, applied: bool, before: Option<f64>, after: Option<f64>) -> Adjustment {
    Adjustment {
        timepoint: 1,
        elapsed_s: 0.0,
        steps,
        applied,
        z_before: before,
        z_after: after,
    }
}

#[test]
fn moved_um_needs_both_ends() {
    let moved = adj(1, true, Some(9741.19), Some(9741.89))
        .moved_um()
        .expect("both ends present");
    // An epsilon, not an equality: these are stage positions near 9741, so the
    // difference of two of them carries the rounding of both.
    assert!((moved - 0.70).abs() < 1e-9, "expected 0.70 um, got {moved}");
    assert_eq!(adj(1, true, Some(9741.19), None).moved_um(), None);
    assert_eq!(adj(1, true, None, Some(9741.89)).moved_um(), None);
    assert_eq!(adj(1, true, None, None).moved_um(), None);
}

#[test]
fn a_dry_run_movement_is_not_the_programs_own() {
    // The z either side of a *would-be* correction still moves, because the
    // operator moved it. `moved_um` reports it either way, so the summary is
    // what has to filter on `applied` — this test pins the distinction that
    // makes that necessary.
    let dry = adj(1, false, Some(9741.19), Some(9738.19));
    assert!(
        dry.moved_um().is_some(),
        "the z did change, and it was not us"
    );
    assert!(!dry.applied, "nothing was carried out");
}
