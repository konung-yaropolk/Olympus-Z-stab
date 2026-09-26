//! Deciding whether to move, and which way.
//!
//! This is the part that has to be conservative. Everything upstream of it only
//! reads a file; everything downstream of it moves a microscope stage during an
//! experiment that cannot be repeated. The controller's job is as much to *not*
//! act as to act.
//!
//! # What it compares
//!
//! The moving average of the last `window_frames` frames against the average of
//! the `reference_frames` frames at the start of the recording — the
//! "starting position" — as a ratio. `metric_rel = current / reference`, so 0.95
//! means the metric has fallen 5%.
//!
//! # What has to be true before it acts
//!
//! * The drop exceeds `dead_band`.
//! * It has exceeded it for `confirm_windows` consecutive full windows. At 7.5 Hz
//!   with 30-frame windows, two windows is eight seconds — long enough that a
//!   bright transient, a passing bubble or one bad frame cannot trigger a
//!   correction.
//! * `cooldown_s` has passed since the last correction, so that the stage has
//!   settled and the moving average has refilled with post-correction frames.
//!   Acting before both have happened corrects the same drift twice, which is how
//!   a stabiliser oscillates.
//! * The session's total step count is below `max_total_steps`.
//!
//! # The two modes
//!
//! **Hill climb.** Unsigned metric, so the direction is found by trying. On the
//! first correction it steps `initial_direction`; then it looks at the metric
//! again. Better, and it keeps going the same way. Worse, and it reverses by
//! twice what it just did — once to undo, once to go the other way — and
//! remembers the new direction for next time. Thermal drift in a rig is
//! consistent, so after the first event it is usually guessing right.
//!
//! **Reference stack.** Signed offset in microns, straight from
//! [`crate::cv::zstack`]. No probing, no wrong first step: it converts microns to
//! steps with `um_per_step` and moves once.
//!
//! # The stop conditions
//!
//! A controller that keeps clicking when its clicks are not working is the worst
//! outcome available here — it is indistinguishable from a program driving the
//! stage into the sample. So: `max_total_steps` bounds the session absolutely,
//! and if `verify_with_zposition` is on and the file's own `zPosition` does not
//! change after a correction, the controller stops and says so rather than
//! trying harder.
//!
//! # One window is a window's worth of frames
//!
//! "`confirm_windows` consecutive full windows" counts *windows*, not frames that
//! happen to have a full window behind them. The moving average is recomputed
//! every frame — the log's `metric_rel` column is live on every row — but the
//! confirmation count advances once per `window_frames` fresh frames. The
//! difference is the whole safety margin: at 7.5 Hz with 30-frame windows,
//! counting windows makes `confirm_windows: 2` eight seconds of sustained
//! defocus, and counting frames would make it a quarter of a second, leaving the
//! dead band as the only thing between measurement noise and the stage.
//!
//! The same counter is what the cooldown works through. Frames measured while the
//! stage is still arriving are real frames with the focus moving through them, so
//! they are folded into the average (the log stays honest) but the window counter
//! is held at zero until the cooldown is over. The window that judges a
//! correction is therefore made only of frames from after it settled.
//!
//! # Nothing here records that a correction happened
//!
//! [`Controller::observe`] only ever *proposes*. Every piece of state that means
//! "a correction was made" — the cooldown clock, the step counters, the in-flight
//! [`Pending`] that gets judged — is written by [`Controller::applied`], and
//! nothing else. The actuator can be unarmed, refuse a sequence with an unknown
//! key in it, or be stopped by the user half way through, and a controller that
//! had believed its own decisions would then judge the next window against a move
//! that never happened.
//!
//! An unarmed session falls out of that for free: with nothing calling `applied`,
//! the state never advances past "this window wants a correction", so the same
//! correction is proposed again at every window until the drop goes away. That is
//! the right behaviour and it is the useful one — the point of a dry run is to
//! find out how often it would have clicked, and a design that started the
//! cooldown at the moment of deciding would show one tidy proposal and hide the
//! rest.

use crate::config::{Config, Mode};
use crate::cv::Shift;

/// The smallest change in the file's `zPosition` that counts as the stage having
/// moved. The acquisition software writes z to two decimals — 9741.19 — so
/// anything below half of that last digit is not a change the file could have
/// expressed in the first place.
const Z_MOVED_UM: f64 = 0.005;

/// Reversals allowed inside one correction event.
///
/// One. The probe either helped or it did not; if the opposite direction is worse
/// as well, then both neighbours of where the event started are worse than where
/// it started, which is not what defocus looks like. Something else is taking the
/// metric down — bleaching, tissue moving, a cell dying in the field — and a
/// third guess would be clicking on noise.
const MAX_REVERSALS_PER_EVENT: u32 = 1;

/// What the controller wants done.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Nothing to do. Carries why, for the log.
    Hold(Hold),
    /// Move by this many steps: positive runs `z_up`, negative `z_down`.
    Move { steps: i32, reason: String },
    /// Stop the session. The reason is shown to the user, not just logged.
    Stop(String),
}

/// Why the controller is not acting. Worth distinguishing in the log: "still
/// filling the first window" and "the drift is inside the dead band" look the
/// same from outside and mean very different things about a session.
#[derive(Debug, Clone, PartialEq)]
pub enum Hold {
    /// Not enough frames yet to have a reference.
    BuildingReference { have: usize, need: usize },
    /// Reference built, still filling the first comparison window.
    BuildingWindow { have: usize, need: usize },
    /// Inside the dead band.
    InBand { metric_rel: f32 },
    /// Outside the band, but not for long enough yet.
    Confirming { windows: usize, need: usize },
    /// Cooling down after a correction.
    Cooldown { remaining_s: f64 },
    /// The frame was skipped: saturated, or the X/Y match failed.
    FrameRejected(&'static str),
    /// Waiting to see whether the last correction changed `zPosition`.
    Verifying,
}

/// One frame's worth of what the controller needs to know.
#[derive(Debug, Clone)]
pub struct Observation {
    pub timepoint: u64,
    /// The focus metric of the current moving average, or `None` if the frame was
    /// rejected.
    pub focus: Option<f32>,
    /// Signed drift in microns, in `reference_stack` mode only.
    pub z_offset_um: Option<f64>,
    /// What the file says the stage position is, for verification.
    pub z_reported: Option<f64>,
    /// The measured X/Y shift, logged but never corrected.
    pub shift: Shift,
    /// Seconds since the session started, from the frames' own timestamps rather
    /// than the wall clock — the file's timing is the recording's timing.
    pub elapsed_s: f64,
}

/// What the controller has decided so far, for the log and the closing summary.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub corrections: u32,
    pub net_steps: i32,
    pub total_steps: u32,
    pub reversals: u32,
    /// Learned from `zPosition` either side of the first correction that moved.
    pub um_per_step: Option<f64>,
}

pub struct Controller {
    cfg: Config,
    mode: Mode,
    /// The starting-position reference metric, once built.
    reference: Option<f32>,
    /// Frames folded into the reference so far.
    reference_count: usize,
    reference_sum: f64,
    /// Frames to skip before the reference starts.
    skipped: usize,
    /// The moving average of the metric.
    window: std::collections::VecDeque<f32>,
    /// Frames accepted since the last window was judged. A full window of *fresh*
    /// frames is one unit of evidence; see the module note on what a window means.
    since_window: usize,
    /// Consecutive full windows outside the dead band.
    confirming: usize,
    /// Direction to try first at the next event: +1 or -1.
    direction: i32,
    /// The correction [`Controller::observe`] has asked for and not yet been told
    /// the outcome of. It holds the metric and `zPosition` as they were at the
    /// moment of deciding, because that is when they are known — `applied` is told
    /// what the stage did, not what the frame looked like before it.
    proposal: Option<Pending>,
    /// The correction in flight, if one is being judged.
    pending: Option<Pending>,
    last_action_s: Option<f64>,
    stats: Stats,
}

/// A correction whose effect has not been judged yet.
struct Pending {
    steps: i32,
    /// The metric before it, to compare against.
    metric_before: f32,
    /// `zPosition` before it, to confirm the stage actually moved.
    z_before: Option<f64>,
    /// Elapsed time it was issued at.
    at_s: f64,
    /// How many times this event has already reversed, so it cannot ping-pong.
    reversals: u32,
}

impl Controller {
    pub fn new(cfg: &Config) -> Controller {
        Controller {
            mode: cfg.control.mode,
            reference: None,
            reference_count: 0,
            reference_sum: 0.0,
            skipped: 0,
            window: std::collections::VecDeque::with_capacity(cfg.measure.window_frames.max(1)),
            since_window: 0,
            confirming: 0,
            // `validate` has already refused anything but +1 or -1, but a zero
            // here would mean a correction of zero steps that reads as a decision
            // and does nothing, so it is normalised rather than trusted.
            direction: if cfg.control.hill_climb.initial_direction < 0 {
                -1
            } else {
                1
            },
            proposal: None,
            pending: None,
            last_action_s: None,
            stats: Stats::default(),
            cfg: cfg.clone(),
        }
    }

    /// Fold in one frame and say what to do about it.
    ///
    /// This proposes; it never records. See the module note — everything that
    /// means "a correction happened" is written by [`Controller::applied`].
    pub fn observe(&mut self, obs: &Observation) -> Decision {
        let w = self.window_frames();

        // A rejected frame is not an observation, and this returns before touching
        // any state at all: not the average, not the confirmation count, not even
        // the reference skip. A saturated frame has no high-frequency content left
        // to measure and a frame whose X/Y match failed was measured over the
        // wrong tissue, so either one entering the average is worse than the frame
        // simply being missing.
        let focus = match obs.focus {
            // A metric that is not finite would poison the average permanently —
            // one NaN and every later mean is NaN, every comparison is false, and
            // the controller holds for the rest of the session without saying why.
            Some(f) if f.is_finite() => f,
            Some(_) => return Decision::Hold(Hold::FrameRejected("metric_not_finite")),
            None => {
                return Decision::Hold(Hold::FrameRejected(if obs.shift.trusted {
                    "no_metric"
                } else {
                    "xy_match_failed"
                }))
            }
        };

        // ---------------------------------------------------- the starting position
        if self.reference.is_none() {
            let skip = self.cfg.measure.reference_skip;
            let need = self.cfg.measure.reference_frames.max(1);
            if self.skipped < skip {
                self.skipped += 1;
            } else {
                self.reference_sum += focus as f64;
                self.reference_count += 1;
                if self.reference_count >= need {
                    self.reference =
                        Some((self.reference_sum / self.reference_count as f64) as f32);
                }
            }
            // The skip counts as progress towards the same total. Two counters
            // that each run up from one, in a column someone is watching while a
            // session starts, look like a program that restarted itself.
            return Decision::Hold(Hold::BuildingReference {
                have: self.skipped + self.reference_count,
                need: skip + need,
            });
        }

        // ------------------------------------------------------ the moving average
        self.window.push_back(focus);
        while self.window.len() > w {
            self.window.pop_front();
        }
        self.since_window += 1;
        if self.window.len() < w {
            return Decision::Hold(Hold::BuildingWindow {
                have: self.window.len(),
                need: w,
            });
        }

        // ------------------------------------------------- the stage still arriving
        if let Some(p) = &self.pending {
            let waited = obs.elapsed_s - p.at_s;
            // `waited > 0.0` is not paranoia about ordering. The elapsed time comes
            // from the frames' own timestamps, and a recording whose timestamps
            // cannot be parsed reports 0.0 for every frame — against a clock that
            // never advances, a time gate never opens, and the session would make
            // exactly one correction and then hold in silence for the rest of the
            // recording. A clock that has not moved at all therefore falls back to
            // the window count alone, which is frame-based, still needs
            // `confirm_windows` of agreement and is still bounded by
            // `max_total_steps`.
            if waited > 0.0 && waited < self.cfg.control.cooldown_s {
                self.since_window = 0;
                return Decision::Hold(Hold::Cooldown {
                    remaining_s: self.cfg.control.cooldown_s - waited,
                });
            }
        }

        let full_window = self.since_window >= w;
        if full_window {
            self.since_window = 0;
        }

        // --------------------------------------- the correction in flight, if any
        if self.pending.is_some() {
            if !full_window {
                return Decision::Hold(Hold::Verifying);
            }
            // Taken, not borrowed: whatever the judgement is, this correction is no
            // longer in flight afterwards — either it is settled or it has been
            // replaced by the reversal of it.
            let p = match self.pending.take() {
                Some(p) => p,
                None => return Decision::Hold(Hold::Verifying),
            };
            if let Some(d) = self.judge(&p, obs) {
                return d;
            }
            // Settled, and this window is as good as any other: fall through and
            // decide it on its own merits.
        }

        // ---------------------------------------------------------- the decision
        let rel = match self.metric_rel() {
            Some(r) => r,
            // A reference of zero: the start of the recording measured nothing at
            // all. There is no ratio to be had, and inventing one would read as a
            // total loss of focus.
            None => return Decision::Hold(Hold::FrameRejected("degenerate_reference")),
        };
        let drop = 1.0 - rel;
        let need_windows = self.cfg.control.confirm_windows.max(1);

        if drop <= self.cfg.control.dead_band {
            // Only a full window clears the count, for the same reason only a full
            // window adds to it: the unit of evidence is a window.
            if full_window {
                self.confirming = 0;
            }
            return Decision::Hold(Hold::InBand { metric_rel: rel });
        }
        if !full_window {
            return Decision::Hold(Hold::Confirming {
                windows: self.confirming,
                need: need_windows,
            });
        }
        self.confirming += 1;
        if self.confirming < need_windows {
            return Decision::Hold(Hold::Confirming {
                windows: self.confirming,
                need: need_windows,
            });
        }

        // The cooldown again, this time against the last correction rather than one
        // in flight. In a normal session the branch above has already covered it —
        // a pending correction is only cleared at a window past the cooldown — but
        // this is the gate that matters, so it is checked where the decision is
        // made and not only where the bookkeeping happens to put it.
        if let Some(last) = self.last_action_s {
            let waited = obs.elapsed_s - last;
            if waited > 0.0 && waited < self.cfg.control.cooldown_s {
                return Decision::Hold(Hold::Cooldown {
                    remaining_s: self.cfg.control.cooldown_s - waited,
                });
            }
        }

        if self.stats.total_steps as i32 >= self.cfg.control.max_total_steps {
            return Decision::Stop(self.budget_spent(drop));
        }

        let steps = match self.mode {
            Mode::HillClimb => {
                let probe = self.cfg.control.hill_climb.probe_steps.max(1);
                self.bounded(self.direction * probe)
            }
            Mode::ReferenceStack => {
                let Some(offset_um) = obs.z_offset_um else {
                    // The window did not resemble any plane of the stack well
                    // enough to be worth acting on. That is not a hold inside the
                    // band — the drop is real and confirmed — it is a decision
                    // that could not be made, so it is reported as a skip.
                    return Decision::Hold(Hold::FrameRejected("no_zstack_match"));
                };
                let um = match self.cfg.actuator.um_per_step.or(self.stats.um_per_step) {
                    Some(u) if u != 0.0 => u,
                    // `validate` refuses this mode without `um_per_step`, so this
                    // is only reachable if validation was skipped — and guessing a
                    // step size on a real stage is not an option.
                    _ => {
                        return Decision::Stop(
                            "mode: reference_stack measures the drift in microns and needs \
                             actuator.um_per_step to turn it into clicks"
                                .into(),
                        )
                    }
                };
                // The offset says where the focal plane has gone, so the correction
                // is the other way: one negation, and it is the only sign work in
                // this file. It assumes `z_up` raises the same z the stack was
                // recorded along — which is what a positive `um_per_step` asserts,
                // and the one thing here a rig can be wired to disagree with. The
                // learned `um_per_step` in the summary is how that gets caught: it
                // comes out negative if `z_up` lowers the reported z.
                let steps = (-offset_um / um).round() as i32;
                if steps == 0 {
                    // Less than half a click's worth of drift. Nothing to do, and
                    // no reason to keep evidence for a correction this small.
                    self.confirming = 0;
                    return Decision::Hold(Hold::InBand { metric_rel: rel });
                }
                self.bounded(steps)
            }
        };
        if steps == 0 {
            // `bounded` only returns zero when there is no budget left, which the
            // check above should already have caught. Saying so is better than a
            // `Move { steps: 0 }` that looks like a correction and moves nothing.
            return Decision::Stop(self.budget_spent(drop));
        }

        self.proposal = Some(Pending {
            steps,
            metric_before: self.current().unwrap_or(0.0),
            z_before: obs.z_reported,
            at_s: obs.elapsed_s,
            reversals: 0,
        });
        Decision::Move {
            steps,
            reason: format!(
                "{:.1}% below the reference over {} window(s)",
                drop * 100.0,
                self.confirming
            ),
        }
    }

    /// Verify and judge the correction that has just had a full settled window
    /// measured after it.
    ///
    /// `Some` is a decision to return now; `None` means the correction is settled
    /// and this window can be decided on from scratch.
    fn judge(&mut self, p: &Pending, obs: &Observation) -> Option<Decision> {
        let dz = match (p.z_before, obs.z_reported) {
            (Some(before), Some(now)) => Some(now - before),
            _ => None,
        };

        // What one step is worth, from the first correction that moved. Signed on
        // purpose: a rig wired so that `z_up` lowers the reported z is something
        // the operator needs told, and an `abs()` here would swallow it.
        if self.stats.um_per_step.is_none() && p.steps != 0 {
            if let Some(dz) = dz {
                if dz.abs() >= Z_MOVED_UM {
                    self.stats.um_per_step = Some(dz / p.steps as f64);
                }
            }
        }

        if self.cfg.actuator.verify_with_zposition {
            match dz {
                Some(dz) if dz.abs() >= Z_MOVED_UM => {}
                Some(_) => {
                    return Some(Decision::Stop(format!(
                        "the last correction ({:+} step(s)) did not change zPosition — the file \
                         still reports {:.2} um — so the clicks are not reaching the acquisition \
                         software. Check the z_up/z_down coordinates with --where, and that \
                         nothing has moved or covered the acquisition window.",
                        p.steps,
                        p.z_before.unwrap_or(0.0)
                    )))
                }
                // Verification is on and there is nothing to verify against. This
                // stops rather than carrying on quietly, because an armed session
                // whose one real check is unavailable is not the session the
                // config asked for.
                None => {
                    return Some(Decision::Stop(
                        "verify_with_zposition is on, but this recording reports no zPosition to \
                         check a correction against. Set verify_with_zposition: false to run \
                         without that check — and then nothing will notice a click that misses."
                            .into(),
                    ))
                }
            }
        }

        if self.mode == Mode::ReferenceStack {
            // Nothing to judge: the stack said which way, so there was no guess in
            // it. If drift remains, the next windows will confirm it again and the
            // offset will size the next move.
            return None;
        }

        // The window is full here, so `current` is `Some`.
        let now = self.current().unwrap_or(0.0);
        if now >= p.metric_before {
            // The guess was right, and `direction` already holds it. Keeping it is
            // the point: thermal drift in a rig is one direction, so the next event
            // starts by trying what worked.
            return None;
        }

        if p.reversals >= MAX_REVERSALS_PER_EVENT {
            return Some(Decision::Stop(format!(
                "both directions made it worse: the metric is {now:.4} after undoing and \
                 reversing, against {:.4} before this event started. Whatever is falling, it is \
                 not focus — bleaching, the sample moving, a cell dying in the field — and \
                 probing further would be clicking on noise. The stage is {:+} step(s) from where \
                 the session started.",
                p.metric_before, self.stats.net_steps
            )));
        }

        // Reverse by twice the last move: once to undo it, once to go the other
        // way. The baseline carries over unchanged — the question a reversal asks
        // is whether the stage is better than where the *event* started, not
        // whether it is better than where the wrong probe left it.
        let steps = self.bounded(-2 * p.steps);
        if steps == 0 {
            return Some(Decision::Stop(self.budget_spent(0.0)));
        }
        self.direction = if p.steps < 0 { 1 } else { -1 };
        self.proposal = Some(Pending {
            steps,
            metric_before: p.metric_before,
            z_before: obs.z_reported,
            at_s: obs.elapsed_s,
            reversals: p.reversals + 1,
        });
        Some(Decision::Move {
            steps,
            reason: format!(
                "the last {:+} made it worse ({now:.4} against {:.4}), so undo it and try the \
                 other way",
                p.steps, p.metric_before
            ),
        })
    }

    /// `want` clamped to what one event and the session are allowed, sign kept.
    /// Zero means the session has no budget left.
    fn bounded(&self, want: i32) -> i32 {
        let per_event = self.cfg.control.max_steps_per_event.max(1);
        let remaining = self.cfg.control.max_total_steps - self.stats.total_steps as i32;
        let magnitude = want.abs().min(per_event).min(remaining.max(0));
        want.signum() * magnitude
    }

    /// Why the session is over, when the step budget is what ended it.
    ///
    /// Reaching `max_total_steps` is a stop and not a quiet hold: the program has
    /// been asked to correct something it has already spent its whole allowance
    /// on, and a session that silently stopped correcting while still printing
    /// rows would be read as a session that was working.
    fn budget_spent(&self, drop: f32) -> String {
        format!(
            "the step budget is spent: {} of max_total_steps {} used, and the metric is still \
             {:.1}% below the reference. Nothing further will be clicked. If the drift is real, \
             correct it by hand and start a new session; if it is not, the metric is measuring \
             something other than focus.",
            self.stats.total_steps,
            self.cfg.control.max_total_steps,
            drop * 100.0
        )
    }

    fn window_frames(&self) -> usize {
        self.cfg.measure.window_frames.max(1)
    }

    /// Tell the controller a correction was actually carried out — the actuator
    /// may have refused, been disarmed, or been stopped by the user, and a
    /// controller that assumed its decisions happened would judge the next
    /// measurement against a move that never occurred.
    pub fn applied(&mut self, steps: i32, at_s: f64) {
        // Nothing moved, so there is nothing to judge, cool down from or count.
        if steps == 0 {
            return;
        }

        // `steps` is what happened and wins over what was asked for: a sequence can
        // be stopped part way through, and the correction that has to be judged is
        // the one the stage made. The rest comes from the proposal, because the
        // metric and the `zPosition` from before the move are only knowable at the
        // frame that proposed it.
        let proposed = self.proposal.take();
        let metric_before = proposed
            .as_ref()
            .map(|p| p.metric_before)
            .or_else(|| self.current())
            .unwrap_or(0.0);
        let z_before = proposed.as_ref().and_then(|p| p.z_before);
        let reversals = proposed.as_ref().map(|p| p.reversals).unwrap_or(0);

        // Only a reversal's proposal carries a non-zero count. Counting it here
        // rather than where it was decided keeps every number in `Stats` a record
        // of what the stage did, not of what was suggested.
        if reversals > 0 {
            self.stats.reversals += 1;
        }
        self.stats.corrections += 1;
        self.stats.net_steps += steps;
        self.stats.total_steps += steps.unsigned_abs();
        self.last_action_s = Some(at_s);
        self.pending = Some(Pending {
            steps,
            metric_before,
            z_before,
            at_s,
            reversals,
        });

        // The evidence has been spent: another correction needs another
        // `confirm_windows` of it, and the window that judges this one has to be
        // made of frames measured after it.
        self.confirming = 0;
        self.since_window = 0;
    }

    /// The reference metric, once built.
    pub fn reference(&self) -> Option<f32> {
        self.reference
    }

    /// The current moving average, once the window is full.
    ///
    /// `None` while it is filling rather than the average of what has arrived so
    /// far: a mean of three frames and a mean of thirty are not comparable, and
    /// comparing them is how a stabiliser corrects at the start of a recording.
    pub fn current(&self) -> Option<f32> {
        if self.window.len() < self.window_frames() {
            return None;
        }
        // Summed in f64. Thirty f32 metrics is not much to accumulate, but the
        // metrics themselves can be large (a Brenner sum over 400k pixels), and
        // the whole signal here is a percentage change in that number.
        let sum: f64 = self.window.iter().map(|&v| v as f64).sum();
        Some((sum / self.window.len() as f64) as f32)
    }

    /// Current over reference. `None` until both exist.
    pub fn metric_rel(&self) -> Option<f32> {
        let current = self.current()?;
        let reference = self.reference?;
        // A zero reference makes the ratio an infinity, which every comparison
        // below the dead band would read as a total loss of focus.
        (reference > 0.0).then_some(current / reference)
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod control_tests;
