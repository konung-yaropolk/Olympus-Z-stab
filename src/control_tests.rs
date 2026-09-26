//! Synthetic metric sequences, driven through the controller a frame at a time.
//!
//! The controller's whole input is one number per frame, which makes it the part
//! of this program that can be tested properly — and the part that most needs to
//! be, because it is the part that moves the stage. There is no image processing
//! and no microscope here: just sequences, and assertions about what is decided.
//!
//! The windows are deliberately tiny — two frames, at a frame a second — so that
//! a whole confirm/cooldown/judge cycle is a handful of lines instead of four
//! hundred frames of 7.5 Hz. The arithmetic of a two-frame window is the same
//! arithmetic as a thirty-frame one; what these tests are checking is the shape of
//! the state machine, and the shape does not depend on the size.
//!
//! Frame numbering, because several tests depend on it: with `reference_skip: 0`
//! and `reference_frames: 2`, frames 1 and 2 build the reference. Frame 3 is the
//! first to enter the window, frame 4 is the first full window, and every second
//! frame after that is another one.

use super::*;

/// Small enough to read, with the verification off unless a test is about it.
fn base() -> Config {
    let mut c = Config::default();
    c.measure.reference_skip = 0;
    c.measure.reference_frames = 2;
    c.measure.window_frames = 2;
    c.control.dead_band = 0.05;
    c.control.confirm_windows = 2;
    c.control.cooldown_s = 10.0;
    c.control.max_steps_per_event = 3;
    c.control.max_total_steps = 40;
    c.control.hill_climb.probe_steps = 1;
    c.control.hill_climb.initial_direction = 1;
    // Most of these tests have nothing to say about `zPosition`, and a
    // verification that stopped the session would mask what they do check.
    c.actuator.verify_with_zposition = false;
    c
}

/// A controller and a clock: one frame per second unless `dt` says otherwise, and
/// a `zPosition` the test can move when it wants the stage to have moved.
struct Rig {
    c: Controller,
    t: u64,
    now: f64,
    dt: f64,
    z: Option<f64>,
}

impl Rig {
    fn new(cfg: &Config) -> Rig {
        Rig {
            c: Controller::new(cfg),
            t: 0,
            now: 0.0,
            dt: 1.0,
            z: None,
        }
    }

    fn observe(&mut self, focus: Option<f32>, offset_um: Option<f64>, trusted: bool) -> Decision {
        self.t += 1;
        self.now += self.dt;
        let obs = Observation {
            timepoint: self.t,
            focus,
            z_offset_um: offset_um,
            z_reported: self.z,
            shift: Shift {
                dy: 0,
                dx: 0,
                peak: 9.0,
                trusted,
            },
            elapsed_s: self.now,
        };
        self.c.observe(&obs)
    }

    /// One frame with this focus metric.
    fn feed(&mut self, focus: f32) -> Decision {
        self.observe(Some(focus), None, true)
    }

    /// One frame with a signed offset read off a reference stack.
    fn feed_off(&mut self, focus: f32, offset_um: f64) -> Decision {
        self.observe(Some(focus), Some(offset_um), true)
    }

    /// A frame the measurement threw away: the X/Y match failed, so there is no
    /// metric for it at all.
    fn feed_rejected(&mut self) -> Decision {
        self.observe(None, None, false)
    }

    fn feed_n(&mut self, n: usize, focus: f32) -> Vec<Decision> {
        (0..n).map(|_| self.feed(focus)).collect()
    }

    fn feed_off_n(&mut self, n: usize, focus: f32, offset_um: f64) -> Vec<Decision> {
        (0..n).map(|_| self.feed_off(focus, offset_um)).collect()
    }

    /// What the actuator would tell it after actually clicking, at the time of the
    /// frame that asked for it.
    fn apply(&mut self, steps: i32) {
        self.c.applied(steps, self.now);
    }
}

fn is_move(d: &Decision) -> Option<i32> {
    match d {
        Decision::Move { steps, .. } => Some(*steps),
        _ => None,
    }
}

fn moves(ds: &[Decision]) -> Vec<i32> {
    ds.iter().filter_map(is_move).collect()
}

fn stop_reason(ds: &[Decision]) -> Option<String> {
    ds.iter().find_map(|d| match d {
        Decision::Stop(why) => Some(why.clone()),
        _ => None,
    })
}

#[test]
fn the_reference_skips_first_then_averages() {
    let mut cfg = base();
    cfg.measure.reference_skip = 2;
    cfg.measure.reference_frames = 2;
    let mut r = Rig::new(&cfg);

    // The settling frames must not reach the average, however extreme they are —
    // that is the entire point of `reference_skip`.
    let d = r.feed(1000.0);
    assert!(
        matches!(
            d,
            Decision::Hold(Hold::BuildingReference { have: 1, need: 4 })
        ),
        "{d:?}"
    );
    r.feed(1000.0);
    assert_eq!(r.c.reference(), None);

    r.feed(100.0);
    assert_eq!(r.c.reference(), None, "one of two reference frames");
    r.feed(200.0);
    assert_eq!(r.c.reference(), Some(150.0));

    // The frames that built the reference are not also the first window.
    assert_eq!(r.c.current(), None);
    assert_eq!(r.c.metric_rel(), None);
}

#[test]
fn nothing_happens_inside_the_dead_band() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);

    // Two percent down, against a five percent band, for twenty frames.
    let ds = r.feed_n(20, 98.0);
    assert!(moves(&ds).is_empty());
    assert!(ds.iter().all(|d| matches!(d, Decision::Hold(_))), "{ds:?}");
    let rel = r.c.metric_rel().expect("window is full");
    assert!((rel - 0.98).abs() < 1e-4, "{rel}");
    assert_eq!(r.c.stats().corrections, 0);
    assert_eq!(r.c.stats().total_steps, 0);
}

#[test]
fn one_bad_window_does_not_trigger() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);

    let bad = r.feed_n(2, 80.0);
    assert!(moves(&bad).is_empty());
    assert!(
        matches!(
            bad[1],
            Decision::Hold(Hold::Confirming {
                windows: 1,
                need: 2
            })
        ),
        "{bad:?}"
    );

    // Back inside the band, and the evidence is gone with it.
    let good = r.feed_n(2, 100.0);
    assert!(
        matches!(good[1], Decision::Hold(Hold::InBand { .. })),
        "{good:?}"
    );

    let again = r.feed_n(2, 80.0);
    assert!(moves(&again).is_empty(), "the count starts again from one");
    assert!(
        matches!(
            again[1],
            Decision::Hold(Hold::Confirming {
                windows: 1,
                need: 2
            })
        ),
        "{again:?}"
    );
}

#[test]
fn a_sustained_drop_acts_after_exactly_confirm_windows() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);

    let ds = r.feed_n(4, 80.0);
    // Frame 3 fills the window, frame 4 is the first full one, frame 6 the second.
    assert!(
        moves(&ds[..3]).is_empty(),
        "not before two windows: {:?}",
        &ds[..3]
    );
    assert_eq!(
        is_move(&ds[3]),
        Some(1),
        "initial_direction is 1, so up is tried first: {:?}",
        ds[3]
    );

    // Proposed, not done: nothing has told the controller it happened.
    assert_eq!(r.c.stats().corrections, 0);
    assert_eq!(r.c.stats().net_steps, 0);
}

#[test]
fn a_dry_run_keeps_proposing_the_same_correction() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);

    // Nothing calls `applied`, which is exactly what an unarmed session does.
    let ds = r.feed_n(12, 80.0);
    let m = moves(&ds);
    assert!(m.len() >= 4, "a dry run must keep asking: {m:?}");
    assert!(
        m.iter().all(|&s| s == 1),
        "and always the same correction: {m:?}"
    );
    assert_eq!(
        r.c.stats().corrections,
        0,
        "nothing happened, so nothing is counted"
    );
    assert_eq!(r.c.stats().total_steps, 0);
    assert!(!ds.iter().any(|d| matches!(d, Decision::Stop(_))), "{ds:?}");
}

#[test]
fn the_cooldown_suppresses_a_second_correction() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);
    let ds = r.feed_n(4, 80.0);
    assert_eq!(is_move(&ds[3]), Some(1));
    r.apply(1);

    // Ten seconds of cooldown at a frame a second: nothing but cooldown, however
    // far below the reference the metric stays.
    let ds = r.feed_n(9, 80.0);
    assert!(moves(&ds).is_empty());
    assert!(
        ds.iter()
            .all(|d| matches!(d, Decision::Hold(Hold::Cooldown { .. }))),
        "{ds:?}"
    );

    // The first frame past the cooldown starts a fresh window rather than judging
    // one measured while the stage was still arriving.
    assert!(matches!(r.feed(80.0), Decision::Hold(Hold::Verifying)));
    assert_eq!(r.c.stats().corrections, 1);
    assert_eq!(r.c.stats().net_steps, 1);

    // Then the judging window, another window of confirmation, and it corrects
    // again — in the direction that did not make things worse.
    let ds = r.feed_n(3, 80.0);
    assert_eq!(moves(&ds), vec![1], "{ds:?}");
}

#[test]
fn a_wrong_guess_reverses_by_twice_and_remembers_the_direction() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);
    let ds = r.feed_n(4, 80.0);
    assert_eq!(is_move(&ds[3]), Some(1));
    r.apply(1);

    // Up made it worse, and it stays worse for the whole cooldown.
    let ds = r.feed_n(10, 70.0);
    assert!(
        moves(&ds).is_empty(),
        "nothing is judged during the cooldown: {ds:?}"
    );

    // The first full settled window judges it: undo the step and go the other way,
    // which is twice the last move, in one decision.
    let d = r.feed(70.0);
    assert_eq!(is_move(&d), Some(-2), "{d:?}");
    r.apply(-2);
    assert_eq!(r.c.stats().reversals, 1);
    assert_eq!(r.c.stats().corrections, 2);
    assert_eq!(r.c.stats().net_steps, -1, "one up then two down");
    assert_eq!(
        r.c.stats().total_steps,
        3,
        "and three steps of the budget spent"
    );

    // The reversal worked and the event ends inside the band.
    let ds = r.feed_n(14, 99.0);
    assert!(moves(&ds).is_empty(), "{ds:?}");

    // The new direction is remembered: the next event's first guess is down.
    let ds = r.feed_n(4, 80.0);
    assert_eq!(moves(&ds), vec![-1], "{ds:?}");
}

#[test]
fn a_reversal_that_also_fails_stops_rather_than_ping_ponging() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);
    let ds = r.feed_n(4, 80.0);
    assert_eq!(is_move(&ds[3]), Some(1));
    r.apply(1);

    // Worse after the probe.
    let ds = r.feed_n(10, 70.0);
    assert!(moves(&ds).is_empty(), "{ds:?}");
    assert_eq!(is_move(&r.feed(70.0)), Some(-2));
    r.apply(-2);

    // Worse again after the reversal, so both neighbours of where the event
    // started are worse than it was. That is not defocus, and one bounded event
    // must not become a stage oscillating for the rest of the recording.
    let ds = r.feed_n(12, 60.0);
    let why = stop_reason(&ds).expect("expected a stop, not a third guess");
    assert!(why.contains("not focus"), "{why}");
    assert_eq!(moves(&ds), Vec::<i32>::new(), "{ds:?}");
    assert_eq!(r.c.stats().reversals, 1);
}

#[test]
fn the_session_stops_when_the_step_budget_is_spent() {
    let mut cfg = base();
    cfg.control.max_total_steps = 1;
    let mut r = Rig::new(&cfg);
    r.feed_n(2, 100.0);

    let ds = r.feed_n(4, 80.0);
    assert_eq!(is_move(&ds[3]), Some(1));
    r.apply(1);

    // The drift carries on and the budget is gone. That has to be a stop: a
    // session that quietly went on printing rows while correcting nothing would be
    // read as a session that was working.
    let ds = r.feed_n(20, 80.0);
    let why = stop_reason(&ds).expect("expected a stop once the budget was spent");
    assert!(why.contains("max_total_steps"), "{why}");
    assert!(moves(&ds).is_empty(), "{ds:?}");
    assert_eq!(r.c.stats().total_steps, 1);
}

#[test]
fn verification_stops_the_session_when_z_never_moves() {
    let mut cfg = base();
    cfg.actuator.verify_with_zposition = true;
    let mut r = Rig::new(&cfg);
    r.z = Some(9741.19);
    r.feed_n(2, 100.0);

    let ds = r.feed_n(4, 80.0);
    assert_eq!(is_move(&ds[3]), Some(1));
    r.apply(1);

    // The clicks went somewhere else: the file's `zPosition` is unchanged.
    let ds = r.feed_n(11, 80.0);
    let why = stop_reason(&ds).expect("expected a stop: the clicks are not landing");
    assert!(why.contains("zPosition"), "{why}");
    assert!(
        why.contains("--where"),
        "the message has to say what to check: {why}"
    );
    assert!(
        moves(&ds).is_empty(),
        "and it must not click harder: {ds:?}"
    );
}

#[test]
fn verification_stops_when_the_file_reports_no_z_at_all() {
    let mut cfg = base();
    cfg.actuator.verify_with_zposition = true;
    let mut r = Rig::new(&cfg);
    // `z` left as None: nothing to verify against.
    r.feed_n(2, 100.0);
    let ds = r.feed_n(4, 80.0);
    assert_eq!(is_move(&ds[3]), Some(1));
    r.apply(1);

    let ds = r.feed_n(11, 80.0);
    let why = stop_reason(&ds).expect("expected a stop: verification is impossible");
    assert!(why.contains("verify_with_zposition"), "{why}");
}

#[test]
fn um_per_step_is_learned_from_the_z_either_side_of_a_correction() {
    let mut cfg = base();
    cfg.actuator.verify_with_zposition = true;
    let mut r = Rig::new(&cfg);
    r.z = Some(9741.19);
    r.feed_n(2, 100.0);

    let ds = r.feed_n(4, 80.0);
    assert_eq!(is_move(&ds[3]), Some(1));
    r.apply(1);
    assert_eq!(
        r.c.stats().um_per_step,
        None,
        "not until the stage has moved"
    );

    // The stage moved 0.4 um for the one step it was given.
    r.z = Some(9741.59);
    let ds = r.feed_n(11, 80.0);
    assert!(stop_reason(&ds).is_none(), "{ds:?}");
    let um =
        r.c.stats()
            .um_per_step
            .expect("learned at the judging window");
    assert!((um - 0.4).abs() < 1e-6, "{um}");

    // A second correction, moving a different amount, does not overwrite it: the
    // figure quoted in the summary is the one measured either side of the first
    // correction that moved, and a session that kept revising it would be quoting
    // whichever click happened last.
    let ds = r.feed_n(3, 80.0);
    assert_eq!(moves(&ds), vec![1], "{ds:?}");
    r.apply(1);
    r.z = Some(9742.59);
    let ds = r.feed_n(11, 80.0);
    assert!(stop_reason(&ds).is_none(), "{ds:?}");
    let um = r.c.stats().um_per_step.expect("still learned");
    assert!((um - 0.4).abs() < 1e-6, "{um}");
}

#[test]
fn a_rejected_frame_neither_averages_nor_counts() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);

    let d = r.feed(80.0);
    assert!(
        matches!(d, Decision::Hold(Hold::BuildingWindow { have: 1, need: 2 })),
        "{d:?}"
    );

    let d = r.feed_rejected();
    assert!(
        matches!(d, Decision::Hold(Hold::FrameRejected("xy_match_failed"))),
        "{d:?}"
    );
    assert_eq!(r.c.current(), None, "the window is still one frame short");

    // Had the rejected frame counted, this would be the second full window and the
    // confirmation count would be at two.
    let d = r.feed(80.0);
    assert!(
        matches!(
            d,
            Decision::Hold(Hold::Confirming {
                windows: 1,
                need: 2
            })
        ),
        "{d:?}"
    );
    assert_eq!(
        r.c.current(),
        Some(80.0),
        "and only the measured frames are in it"
    );
}

#[test]
fn a_metric_that_is_not_finite_is_rejected_rather_than_averaged() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);
    r.feed(80.0);

    // One NaN in the average is every later average: the mean is NaN, every
    // comparison against the dead band is false, and the controller holds for the
    // rest of the session without anything in the log saying why.
    let d = r.feed(f32::NAN);
    assert!(
        matches!(d, Decision::Hold(Hold::FrameRejected("metric_not_finite"))),
        "{d:?}"
    );
    let d = r.feed(f32::INFINITY);
    assert!(
        matches!(d, Decision::Hold(Hold::FrameRejected("metric_not_finite"))),
        "{d:?}"
    );
    assert_eq!(r.c.current(), None, "neither reached the window");

    let d = r.feed(80.0);
    assert!(
        matches!(
            d,
            Decision::Hold(Hold::Confirming {
                windows: 1,
                need: 2
            })
        ),
        "{d:?}"
    );
    assert_eq!(r.c.current(), Some(80.0));
    assert_eq!(r.c.metric_rel(), Some(0.8));
}

#[test]
fn reference_stack_converts_microns_to_steps_and_clamps() {
    let mut cfg = base();
    cfg.control.mode = Mode::ReferenceStack;
    cfg.actuator.um_per_step = Some(0.5);
    cfg.control.max_steps_per_event = 3;

    // Four microns up at half a micron per step is eight steps, clamped to the
    // three one event is allowed. The offset says where the focal plane went, so
    // the correction is the other way: positive offset, negative steps.
    let mut r = Rig::new(&cfg);
    r.feed_n(2, 100.0);
    let ds = r.feed_off_n(4, 80.0, 4.0);
    assert_eq!(moves(&ds), vec![-3], "{ds:?}");

    // A micron down is exactly two steps up, and inside the per-event limit.
    let mut r = Rig::new(&cfg);
    r.feed_n(2, 100.0);
    let ds = r.feed_off_n(4, 80.0, -1.0);
    assert_eq!(moves(&ds), vec![2], "{ds:?}");

    // Less than half a step of drift is not a click.
    let mut r = Rig::new(&cfg);
    r.feed_n(2, 100.0);
    let ds = r.feed_off_n(4, 80.0, 0.2);
    assert!(moves(&ds).is_empty(), "{ds:?}");
    assert!(
        matches!(ds[3], Decision::Hold(Hold::InBand { .. })),
        "{:?}",
        ds[3]
    );

    // No usable match against the stack: a confirmed drop with no direction is a
    // decision that could not be made, not a hold inside the band.
    let mut r = Rig::new(&cfg);
    r.feed_n(2, 100.0);
    let ds = r.feed_n(4, 80.0);
    assert!(moves(&ds).is_empty(), "{ds:?}");
    assert!(
        matches!(
            ds[3],
            Decision::Hold(Hold::FrameRejected("no_zstack_match"))
        ),
        "{:?}",
        ds[3]
    );
}

#[test]
fn reference_stack_never_probes() {
    let mut cfg = base();
    cfg.control.mode = Mode::ReferenceStack;
    cfg.actuator.um_per_step = Some(0.5);
    let mut r = Rig::new(&cfg);
    r.feed_n(2, 100.0);

    let ds = r.feed_off_n(4, 80.0, 4.0);
    assert_eq!(moves(&ds), vec![-3], "{ds:?}");
    r.apply(-3);

    // Worse after the move, which in hill-climb mode is what triggers a reversal.
    // Here it must not: the stack has already said which way, and a metric that
    // fell further is a reason to keep going, not to turn round.
    let ds = r.feed_off_n(16, 70.0, 4.0);
    let m = moves(&ds);
    assert!(
        !m.is_empty(),
        "the offset is still there, so it should act again: {ds:?}"
    );
    assert!(
        m.iter().all(|&s| s < 0),
        "and never back the other way: {m:?}"
    );
    assert_eq!(r.c.stats().reversals, 0);
}

#[test]
fn metric_rel_is_current_over_reference() {
    let mut r = Rig::new(&base());
    r.feed_n(2, 100.0);
    assert_eq!(r.c.reference(), Some(100.0));
    assert_eq!(r.c.metric_rel(), None, "not until the window is full");

    r.feed(90.0);
    r.feed(70.0);
    assert_eq!(r.c.current(), Some(80.0));
    let rel = r.c.metric_rel().expect("both exist");
    assert!((rel - 0.8).abs() < 1e-6, "{rel}");
}

#[test]
fn a_zero_reference_is_not_divided_by() {
    let mut r = Rig::new(&base());
    // A start of recording that measured nothing at all.
    r.feed_n(2, 0.0);
    assert_eq!(r.c.reference(), Some(0.0));
    let d = r.feed(0.0);
    assert!(
        matches!(d, Decision::Hold(Hold::BuildingWindow { .. })),
        "{d:?}"
    );
    let d = r.feed(0.0);
    assert!(
        matches!(
            d,
            Decision::Hold(Hold::FrameRejected("degenerate_reference"))
        ),
        "{d:?}"
    );
    assert_eq!(r.c.metric_rel(), None);
}
