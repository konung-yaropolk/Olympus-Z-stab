//! Tests for the settings file.
//!
//! The one that matters most is [`shipped_config_matches_the_built_in_defaults`].
//! `config.rs` claims the numbers in `config.yaml` and the numbers in `Default`
//! are the same, and nothing else checks it — so the two drift, someone reads the
//! documented value out of the YAML, deletes the key because it is "already the
//! default", and the program runs on a different number than the one they read.
//! That happened to `dead_band` and `high_freq` during development, when both were
//! re-measured against a real recording.

use super::*;

/// The `config.yaml` that ships beside the exe.
fn shipped() -> Config {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.yaml");
    Config::load(&path).unwrap_or_else(|e| panic!("the shipped config.yaml does not parse: {e}"))
}

// ------------------------------------------------ the shipped file itself

#[test]
fn shipped_config_parses() {
    let _ = shipped();
}

#[test]
fn shipped_config_matches_the_built_in_defaults() {
    let y = shipped();
    let d = Config::default();

    // input
    assert_eq!(y.input.channel, d.input.channel, "input.channel");
    assert_eq!(y.input.roi, d.input.roi, "input.roi");
    assert_eq!(y.input.downsample, d.input.downsample, "input.downsample");
    assert_eq!(
        y.input.poll_interval_ms, d.input.poll_interval_ms,
        "input.poll_interval_ms"
    );
    assert_eq!(
        y.input.idle_timeout_s, d.input.idle_timeout_s,
        "input.idle_timeout_s"
    );

    // measure
    assert_eq!(
        y.measure.reference_frames, d.measure.reference_frames,
        "measure.reference_frames"
    );
    assert_eq!(
        y.measure.reference_skip, d.measure.reference_skip,
        "measure.reference_skip"
    );
    assert_eq!(
        y.measure.window_frames, d.measure.window_frames,
        "measure.window_frames"
    );
    assert_eq!(y.measure.metric, d.measure.metric, "measure.metric");
    assert_eq!(
        y.measure.saturated_fraction, d.measure.saturated_fraction,
        "measure.saturated_fraction"
    );
    let (ry, rd) = (&y.measure.registration, &d.measure.registration);
    assert_eq!(ry.enabled, rd.enabled, "registration.enabled");
    assert_eq!(
        ry.max_shift_px, rd.max_shift_px,
        "registration.max_shift_px"
    );
    assert_eq!(ry.taper, rd.taper, "registration.taper");
    assert_eq!(ry.min_peak, rd.min_peak, "registration.min_peak");
    // Measured values, and the pair most likely to drift: both were changed once
    // already, after being measured against a real 29-minute recording.
    assert_eq!(
        y.measure.high_freq.low_cut, d.measure.high_freq.low_cut,
        "measure.high_freq.low_cut"
    );
    assert_eq!(
        y.measure.high_freq.high_cut, d.measure.high_freq.high_cut,
        "measure.high_freq.high_cut"
    );

    // control
    assert_eq!(y.control.mode, d.control.mode, "control.mode");
    assert_eq!(
        y.control.dead_band, d.control.dead_band,
        "control.dead_band"
    );
    assert_eq!(
        y.control.confirm_windows, d.control.confirm_windows,
        "control.confirm_windows"
    );
    assert_eq!(
        y.control.cooldown_s, d.control.cooldown_s,
        "control.cooldown_s"
    );
    assert_eq!(
        y.control.max_steps_per_event, d.control.max_steps_per_event,
        "control.max_steps_per_event"
    );
    assert_eq!(
        y.control.max_total_steps, d.control.max_total_steps,
        "control.max_total_steps"
    );
    assert_eq!(
        y.control.hill_climb.probe_steps, d.control.hill_climb.probe_steps,
        "hill_climb.probe_steps"
    );
    assert_eq!(
        y.control.hill_climb.initial_direction, d.control.hill_climb.initial_direction,
        "hill_climb.initial_direction"
    );
    assert_eq!(
        y.control.reference_stack.path, d.control.reference_stack.path,
        "reference_stack.path"
    );
    assert_eq!(
        y.control.reference_stack.step_um, d.control.reference_stack.step_um,
        "reference_stack.step_um"
    );
    assert_eq!(
        y.control.reference_stack.min_score, d.control.reference_stack.min_score,
        "reference_stack.min_score"
    );

    // actuator, except the click sequences — see the test below
    assert_eq!(y.actuator.arm, d.actuator.arm, "actuator.arm");
    assert_eq!(
        y.actuator.um_per_step, d.actuator.um_per_step,
        "actuator.um_per_step"
    );
    assert_eq!(
        y.actuator.verify_with_zposition, d.actuator.verify_with_zposition,
        "actuator.verify_with_zposition"
    );
    assert_eq!(
        y.actuator.emergency_stop_corner, d.actuator.emergency_stop_corner,
        "actuator.emergency_stop_corner"
    );
    assert_eq!(
        y.actuator.settle_s, d.actuator.settle_s,
        "actuator.settle_s"
    );

    // log
    assert_eq!(y.log.csv, d.log.csv, "log.csv");
    assert_eq!(y.log.print_every, d.log.print_every, "log.print_every");
}

#[test]
fn the_click_sequences_are_the_one_deliberate_difference() {
    // `Default` has none, because a program with no config file must not have a
    // coordinate it might click. The shipped file has placeholders, because a
    // file with no `z_up` at all does not show the user what one looks like.
    // Neither can be changed to match the other, so the difference is asserted
    // rather than left as a hole in the test above.
    assert!(
        Config::default().actuator.z_up.is_empty(),
        "the built-in default must never carry a clickable coordinate"
    );
    assert!(
        Config::default().actuator.z_down.is_empty(),
        "the built-in default must never carry a clickable coordinate"
    );
    let y = shipped();
    assert!(!y.actuator.z_up.is_empty(), "z_up should show an example");
    assert!(
        !y.actuator.z_down.is_empty(),
        "z_down should show an example"
    );
}

#[test]
fn the_shipped_config_is_not_armed() {
    // The single most important property of the file. If this ever fails, a user
    // who copies it beside the exe and drops a recording on the program has a
    // clicking rig with placeholder coordinates.
    assert!(!shipped().actuator.arm);
}

#[test]
fn the_shipped_config_validates() {
    assert!(
        shipped().validate().is_empty(),
        "the shipped config should not report problems: {:?}",
        shipped().validate()
    );
}

// ------------------------------------------------------------- parsing

#[test]
fn a_partial_config_takes_defaults_for_the_rest() {
    let c: Config = serde_yaml::from_str("control:\n  dead_band: 0.5\n").expect("parses");
    assert_eq!(c.control.dead_band, 0.5);
    // Untouched keys, nested and top level.
    assert_eq!(
        c.control.confirm_windows,
        Control::default().confirm_windows
    );
    assert_eq!(c.measure.window_frames, Measure::default().window_frames);
    assert!(!c.actuator.arm);
}

#[test]
fn an_empty_config_is_the_default() {
    // `{}` rather than `""`: an empty document is a YAML null, which is its own
    // case and not what a user writing an empty file means.
    let c: Config = serde_yaml::from_str("{}").expect("parses");
    assert_eq!(c.control.dead_band, Config::default().control.dead_band);
}

#[test]
fn a_misspelled_key_is_refused() {
    // The failure `deny_unknown_fields` exists to prevent: a setting that looks
    // changed, reads as changed, and is not.
    let bad = serde_yaml::from_str::<Config>("control:\n  dead_zone: 0.5\n");
    assert!(
        bad.is_err(),
        "dead_zone should not be accepted as dead_band"
    );
    let bad = serde_yaml::from_str::<Config>("measure:\n  metrics: brenner\n");
    assert!(bad.is_err(), "a misspelled section key should be refused");
    let bad = serde_yaml::from_str::<Config>("nonsense: 1\n");
    assert!(bad.is_err(), "an unknown top-level key should be refused");
}

#[test]
fn channel_accepts_a_number_or_sum_and_nothing_else() {
    let c: Config = serde_yaml::from_str("input:\n  channel: 1\n").expect("parses");
    assert_eq!(c.input.channel, ChannelPick::Index(1));
    assert_eq!(c.input.channel.index(), Some(1));
    assert!(c.validate().is_empty());

    let c: Config = serde_yaml::from_str("input:\n  channel: sum\n").expect("parses");
    assert_eq!(c.input.channel.index(), None, "sum means add them all");
    assert!(c.validate().is_empty());

    // Anything else parses as a string — the untagged enum cannot reject it — so
    // `validate` is what has to catch it.
    let c: Config = serde_yaml::from_str("input:\n  channel: green\n").expect("parses");
    assert!(
        c.validate().iter().any(|p| p.contains("input.channel")),
        "a channel name that is not `sum` should be reported: {:?}",
        c.validate()
    );
}

#[test]
fn the_metrics_all_have_the_spelling_the_config_documents() {
    for (text, want) in [
        ("high_freq_ratio", Metric::HighFreqRatio),
        ("norm_variance", Metric::NormVariance),
        ("brenner", Metric::Brenner),
        ("tenengrad", Metric::Tenengrad),
        ("top_percentile", Metric::TopPercentile),
    ] {
        let c: Config = serde_yaml::from_str(&format!("measure:\n  metric: {text}\n"))
            .unwrap_or_else(|e| panic!("metric: {text} should parse: {e}"));
        assert_eq!(c.measure.metric, want);
    }
    assert!(serde_yaml::from_str::<Config>("measure:\n  metric: sharpness\n").is_err());
}

#[test]
fn the_step_vocabulary_is_the_autoclickers() {
    // A sequence recorded with `autoclicker.exe` must paste in unchanged, so these
    // five spellings and the defaulted `delay` are a compatibility contract, not a
    // choice this program gets to revisit.
    let yaml = "\
actuator:
  z_up:
    - {step: click, x: 10, y: 20, delay: 0.5}
    - {step: right_click, x: 30, y: 40}
    - {step: text_input, text: \"9741.19\"}
    - {step: press_key, key: enter}
    - {step: hotkey, keys: [ctrl, shift, u]}
";
    let c: Config = serde_yaml::from_str(yaml).expect("the autoclicker's spellings should parse");
    assert_eq!(c.actuator.z_up.len(), 5);
    assert_eq!(
        c.actuator.z_up[0],
        Step::Click {
            x: 10,
            y: 20,
            delay: 0.5
        }
    );
    // The omitted delay takes the autoclicker's own default.
    assert_eq!(
        c.actuator.z_up[1],
        Step::RightClick {
            x: 30,
            y: 40,
            delay: 0.2
        }
    );
    assert_eq!(
        c.actuator.z_up[3],
        Step::PressKey {
            key: "enter".into(),
            delay: 0.2
        }
    );

    // And it round-trips, so a config this program writes can be read back.
    let text = serde_yaml::to_string(&c).expect("serialises");
    let back: Config = serde_yaml::from_str(&text).expect("re-parses");
    assert_eq!(back.actuator.z_up, c.actuator.z_up);
}

// ------------------------------------------------------------ validate

/// A config that is valid, to perturb one field of at a time.
fn ok() -> Config {
    Config::default()
}

#[test]
fn validate_is_quiet_on_a_good_config() {
    assert!(ok().validate().is_empty());
}

#[test]
fn validate_reports_every_problem_not_just_the_first() {
    let mut c = ok();
    c.input.downsample = 0;
    c.measure.window_frames = 0;
    c.control.confirm_windows = 0;
    let problems = c.validate();
    assert!(
        problems.len() >= 3,
        "three things are wrong, {} reported: {problems:?}",
        problems.len()
    );
}

#[test]
fn validate_catches_the_numeric_bounds() {
    let cases: Vec<(&str, fn(&mut Config))> = vec![
        ("input.downsample", |c| c.input.downsample = 0),
        ("input.roi", |c| c.input.roi = Some([0, 0, 0, 16])),
        ("input.poll_interval_ms", |c| c.input.poll_interval_ms = 0),
        ("measure.reference_frames", |c| {
            c.measure.reference_frames = 0
        }),
        ("measure.window_frames", |c| c.measure.window_frames = 0),
        ("measure.high_freq", |c| c.measure.high_freq.high_cut = 0.01),
        ("measure.high_freq", |c| c.measure.high_freq.low_cut = 1.5),
        ("control.dead_band", |c| c.control.dead_band = 1.0),
        ("control.dead_band", |c| c.control.dead_band = -0.1),
        ("control.confirm_windows", |c| c.control.confirm_windows = 0),
        ("control.max_steps_per_event", |c| {
            c.control.max_steps_per_event = 0
        }),
        ("control.max_total_steps", |c| c.control.max_total_steps = 0),
        ("actuator.um_per_step", |c| {
            c.actuator.um_per_step = Some(0.0)
        }),
    ];
    for (key, break_it) in cases {
        let mut c = ok();
        break_it(&mut c);
        let problems = c.validate();
        assert!(
            problems.iter().any(|p| p.starts_with(key)),
            "breaking {key} should be reported, got {problems:?}"
        );
    }
}

#[test]
fn an_unarmed_config_tolerates_empty_click_sequences() {
    // The first session on a new rig has no coordinates yet and is the most
    // valuable one to be able to run.
    let mut c = ok();
    c.actuator.arm = false;
    c.actuator.z_up.clear();
    c.actuator.z_down.clear();
    assert!(
        c.validate().is_empty(),
        "a dry run needs no click sequences: {:?}",
        c.validate()
    );
}

#[test]
fn an_armed_config_demands_both_click_sequences() {
    let mut c = ok();
    c.actuator.arm = true;
    let problems = c.validate();
    assert!(problems.iter().any(|p| p.starts_with("actuator.z_up")));
    assert!(problems.iter().any(|p| p.starts_with("actuator.z_down")));

    // One of the two is not enough: a stabiliser that can only move up would
    // drive the stage to a stop the first time it guessed wrong.
    c.actuator.z_up = vec![Step::Click {
        x: 1,
        y: 2,
        delay: 0.1,
    }];
    let problems = c.validate();
    assert!(!problems.iter().any(|p| p.starts_with("actuator.z_up")));
    assert!(
        problems.iter().any(|p| p.starts_with("actuator.z_down")),
        "z_down is still missing: {problems:?}"
    );
}

#[test]
fn hill_climb_checks_its_own_settings_only() {
    let mut c = ok();
    c.control.mode = Mode::HillClimb;
    c.control.hill_climb.initial_direction = 0;
    assert!(c
        .validate()
        .iter()
        .any(|p| p.starts_with("control.hill_climb.initial_direction")));

    let mut c = ok();
    c.control.hill_climb.probe_steps = 0;
    assert!(c
        .validate()
        .iter()
        .any(|p| p.starts_with("control.hill_climb.probe_steps")));

    // A missing reference stack is not hill climb's problem.
    let c = ok();
    assert!(c.control.reference_stack.path.is_none());
    assert!(c.validate().is_empty());
}

#[test]
fn reference_stack_mode_demands_a_stack_and_a_step_size() {
    let mut c = ok();
    c.control.mode = Mode::ReferenceStack;
    let problems = c.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("control.reference_stack.path")),
        "a missing stack path must be reported: {problems:?}"
    );
    // This mode measures the drift in microns, so it cannot convert that into
    // clicks without being told what a click is worth. Learning it from a probe
    // is a hill-climb trick and is not available here.
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("actuator.um_per_step")),
        "um_per_step must be required in this mode: {problems:?}"
    );

    c.control.reference_stack.step_um = 0.0;
    assert!(c
        .validate()
        .iter()
        .any(|p| p.starts_with("control.reference_stack.step_um")));
}

#[test]
fn reference_stack_mode_reports_a_path_that_is_not_there() {
    let mut c = ok();
    c.control.mode = Mode::ReferenceStack;
    c.control.reference_stack.path = Some("no-such-stack-file-here.oir".into());
    c.actuator.um_per_step = Some(1.0);
    let problems = c.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("no-such-stack-file-here.oir")),
        "the missing file should be named: {problems:?}"
    );
}
