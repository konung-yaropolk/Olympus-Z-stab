//! Tests for the actuator.
//!
//! The rule here is that `cargo test` must never move the mouse or press a key on
//! the machine it runs on — which on the acquisition machine is the machine
//! driving the microscope. Every test in this file that is not `#[ignore]`d
//! therefore builds an *unarmed* actuator, which by construction has no `Enigo`
//! at all and so has nothing to reach the input system with.
//!
//! That leaves a real gap, and it is worth naming rather than pretending
//! otherwise: nothing here proves that an armed sequence clicks the right things
//! in the right order. What can be proved without input is everything that
//! decides *whether* to click — the key table, the refusals, the dry-run
//! reporting, the stickiness of the stop, and the `{z}` substitution — and those
//! are the parts that a mistake in would be silent. The two `#[ignore]`d tests at
//! the bottom cover what only an `Enigo` can show, for a human to run by hand on
//! a machine that is not in the middle of an experiment.

use super::*;
use crate::config::{Config, Step};

/// An unarmed config with the two sequences filled in.
fn cfg_with(up: Vec<Step>, down: Vec<Step>) -> Config {
    let mut cfg = Config::default();
    cfg.actuator.z_up = up;
    cfg.actuator.z_down = down;
    cfg
}

fn click(x: i32, y: i32) -> Step {
    Step::Click { x, y, delay: 0.0 }
}

fn press(key: &str) -> Step {
    Step::PressKey {
        key: key.to_string(),
        delay: 0.0,
    }
}

fn refusal(applied: Applied) -> String {
    match applied {
        Applied::Refused(why) => why,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ----------------------------------------------------------------- parse_key

#[test]
fn parse_key_is_the_autoclicker_table() {
    use enigo::Key;

    // Every arm of the table, aliases included, because the point of copying it
    // verbatim is that a sequence recorded with the autoclicker means the same
    // thing here — and an alias quietly missing from this table would turn into
    // "move refused" in the middle of a recording.
    let table: &[(&str, Key)] = &[
        // Navigation
        ("tab", Key::Tab),
        ("escape", Key::Escape),
        ("esc", Key::Escape),
        ("space", Key::Space),
        ("backspace", Key::Backspace),
        ("delete", Key::Delete),
        ("del", Key::Delete),
        ("insert", Key::Insert),
        ("ins", Key::Insert),
        ("up", Key::UpArrow),
        ("down", Key::DownArrow),
        ("left", Key::LeftArrow),
        ("right", Key::RightArrow),
        ("home", Key::Home),
        ("end", Key::End),
        ("pageup", Key::PageUp),
        ("page_up", Key::PageUp),
        ("pagedown", Key::PageDown),
        ("page_down", Key::PageDown),
        // Modifiers
        ("ctrl", Key::Control),
        ("control", Key::Control),
        ("lctrl", Key::LControl),
        ("lcontrol", Key::LControl),
        ("rctrl", Key::RControl),
        ("rcontrol", Key::RControl),
        ("alt", Key::Alt),
        ("shift", Key::Shift),
        ("lshift", Key::LShift),
        ("rshift", Key::RShift),
        ("super", Key::Meta),
        ("win", Key::Meta),
        ("meta", Key::Meta),
        ("capslock", Key::CapsLock),
        ("caps", Key::CapsLock),
        ("numlock", Key::Numlock),
        // System / misc
        ("return", Key::Return),
        ("enter", Key::Return),
        ("pause", Key::Pause),
        ("print", Key::Print),
        ("printscreen", Key::Print),
        ("help", Key::Help),
        ("select", Key::Select),
        ("execute", Key::Execute),
        ("clear", Key::Clear),
        ("cancel", Key::Cancel),
        // Media
        ("volup", Key::VolumeUp),
        ("volumeup", Key::VolumeUp),
        ("voldown", Key::VolumeDown),
        ("volumedown", Key::VolumeDown),
        ("mute", Key::VolumeMute),
        ("volumemute", Key::VolumeMute),
        ("medianext", Key::MediaNextTrack),
        ("nexttrack", Key::MediaNextTrack),
        ("mediaprev", Key::MediaPrevTrack),
        ("prevtrack", Key::MediaPrevTrack),
        ("mediastop", Key::MediaStop),
        ("mediaplay", Key::MediaPlayPause),
        ("playpause", Key::MediaPlayPause),
        // Numpad
        ("num0", Key::Numpad0),
        ("numpad0", Key::Numpad0),
        ("num1", Key::Numpad1),
        ("numpad1", Key::Numpad1),
        ("num2", Key::Numpad2),
        ("numpad2", Key::Numpad2),
        ("num3", Key::Numpad3),
        ("numpad3", Key::Numpad3),
        ("num4", Key::Numpad4),
        ("numpad4", Key::Numpad4),
        ("num5", Key::Numpad5),
        ("numpad5", Key::Numpad5),
        ("num6", Key::Numpad6),
        ("numpad6", Key::Numpad6),
        ("num7", Key::Numpad7),
        ("numpad7", Key::Numpad7),
        ("num8", Key::Numpad8),
        ("numpad8", Key::Numpad8),
        ("num9", Key::Numpad9),
        ("numpad9", Key::Numpad9),
        ("numadd", Key::Add),
        ("numplus", Key::Add),
        ("numsub", Key::Subtract),
        ("numminus", Key::Subtract),
        ("nummul", Key::Multiply),
        ("nummultiply", Key::Multiply),
        ("numdiv", Key::Divide),
        ("numdivide", Key::Divide),
        ("numdec", Key::Decimal),
        ("numdecimal", Key::Decimal),
        // F-keys
        ("f1", Key::F1),
        ("f2", Key::F2),
        ("f3", Key::F3),
        ("f4", Key::F4),
        ("f5", Key::F5),
        ("f6", Key::F6),
        ("f7", Key::F7),
        ("f8", Key::F8),
        ("f9", Key::F9),
        ("f10", Key::F10),
        ("f11", Key::F11),
        ("f12", Key::F12),
        ("f13", Key::F13),
        ("f14", Key::F14),
        ("f15", Key::F15),
        ("f16", Key::F16),
        ("f17", Key::F17),
        ("f18", Key::F18),
        ("f19", Key::F19),
        ("f20", Key::F20),
    ];

    for (name, key) in table {
        assert_eq!(parse_key(name), Some(*key), "{name}");
        // The table lower-cases before matching, so a workflow written in any
        // capitalisation resolves the same way.
        assert_eq!(
            parse_key(&name.to_uppercase()),
            Some(*key),
            "{name} upper-cased"
        );
    }

    // 108 names, which is every string literal in `parse_key` bar the single-char
    // fallback. Asserted so that a name deleted from the table above is a failure
    // rather than a silently smaller test.
    assert_eq!(table.len(), 108, "an arm of the table has gone missing");
}

#[test]
fn a_single_character_is_a_layout_key() {
    use enigo::Key;
    assert_eq!(parse_key("a"), Some(Key::Layout('a')));
    assert_eq!(parse_key("A"), Some(Key::Layout('a')), "lower-cased first");
    assert_eq!(parse_key("1"), Some(Key::Layout('1')));
    assert_eq!(parse_key("+"), Some(Key::Layout('+')));
    assert_eq!(parse_key("."), Some(Key::Layout('.')));
}

#[test]
fn unknown_names_are_none() {
    assert_eq!(parse_key(""), None, "the empty name is not a one-char key");
    assert_eq!(parse_key("nope"), None);
    assert_eq!(
        parse_key("ctrl+c"),
        None,
        "a combo is a hotkey, not a key name"
    );
    // Two quirks inherited from the autoclicker on purpose: the name is not
    // trimmed, and the table stops at f20 although enigo knows F21 upwards.
    assert_eq!(parse_key(" esc "), None);
    assert_eq!(parse_key("esc\n"), None);
    assert_eq!(parse_key("f21"), None);
}

// ------------------------------------------------------------- the dry run

#[test]
fn unarmed_reports_the_move_and_touches_nothing() {
    let cfg = cfg_with(vec![click(1850, 420)], vec![click(1850, 470)]);
    assert!(!cfg.actuator.arm, "Config::default must not be armed");

    let mut a = Actuator::new(&cfg);
    assert!(
        a.enigo.is_none(),
        "an unarmed actuator must not take hold of the input system"
    );

    // The count is reported signed, because that is what the caller logs.
    assert_eq!(a.apply(3, None), Applied::DryRun { steps: 3 });
    assert_eq!(a.apply(-2, None), Applied::DryRun { steps: -2 });

    assert!(
        a.enigo.is_none(),
        "still nothing created after two corrections"
    );
    assert!(!a.stopped());
    assert!(!a.check_stop(), "with no Enigo there is no corner to see");
}

#[test]
fn zero_steps_needs_no_sequence() {
    // A correction of nothing is not a configuration error, so it is not refused
    // even with both sequences empty.
    let mut a = Actuator::new(&Config::default());
    assert_eq!(a.apply(0, None), Applied::DryRun { steps: 0 });
}

#[test]
fn a_long_dry_run_sequence_does_not_sleep() {
    // Delays belong to the clicking, and an unarmed run does not click. If this
    // ever regresses, a dry run at 7.5 Hz falls behind the file it is following.
    let slow = vec![
        Step::Click {
            x: 1,
            y: 2,
            delay: 30.0,
        },
        Step::TextInput {
            text: "9741.19".into(),
            delay: 30.0,
        },
    ];
    let cfg = cfg_with(slow.clone(), slow);
    let mut a = Actuator::new(&cfg);

    let t0 = std::time::Instant::now();
    assert_eq!(a.apply(3, None), Applied::DryRun { steps: 3 });
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(1),
        "it waited"
    );
}

// -------------------------------------------------------------- refusals

#[test]
fn an_empty_sequence_is_refused_and_names_the_direction() {
    // The built-in defaults have no sequences at all, which is the state of a run
    // with no config.yaml beside the exe.
    let mut a = Actuator::new(&Config::default());

    let why = refusal(a.apply(1, None));
    assert!(why.contains("actuator.z_up"), "{why}");
    assert!(why.contains("empty"), "{why}");

    let why = refusal(a.apply(-1, None));
    assert!(why.contains("actuator.z_down"), "{why}");

    // One direction being configured does not excuse the other.
    let cfg = cfg_with(vec![click(1, 2)], Vec::new());
    let mut a = Actuator::new(&cfg);
    assert_eq!(a.apply(1, None), Applied::DryRun { steps: 1 });
    assert!(refusal(a.apply(-1, None)).contains("actuator.z_down"));
}

#[test]
fn an_unknown_key_name_refuses_the_whole_sequence() {
    // The bad name is last, behind two steps that would have run: what is being
    // tested is that the sequence is resolved as a whole before any of it starts,
    // because a half-run sequence leaves the software's Z field half-typed.
    let cfg = cfg_with(
        vec![click(1850, 420), press("enter"), press("entr")],
        vec![click(1850, 470)],
    );
    let mut a = Actuator::new(&cfg);

    let why = refusal(a.apply(1, None));
    assert!(why.contains("actuator.z_up"), "{why}");
    assert!(
        why.contains("entr"),
        "the message must name the bad key: {why}"
    );

    // The other direction is untouched by the first one's problem.
    assert_eq!(a.apply(-1, None), Applied::DryRun { steps: -1 });
}

#[test]
fn an_unknown_hotkey_member_refuses_the_combo() {
    let hotkey = Step::Hotkey {
        keys: vec!["ctrl".into(), "fnord".into()],
        delay: 0.0,
    };
    let cfg = cfg_with(vec![hotkey], vec![click(1, 2)]);
    let mut a = Actuator::new(&cfg);

    let why = refusal(a.apply(2, None));
    assert!(why.contains("fnord"), "{why}");
    assert!(why.contains("hotkey"), "{why}");
}

#[test]
fn an_empty_hotkey_is_refused() {
    let cfg = cfg_with(
        vec![Step::Hotkey {
            keys: Vec::new(),
            delay: 0.0,
        }],
        vec![click(1, 2)],
    );
    let mut a = Actuator::new(&cfg);
    assert!(refusal(a.apply(1, None)).contains("hotkey"));
}

#[test]
fn refusal_does_not_depend_on_being_armed() {
    // The check runs before the armed/unarmed branch on purpose: a dry run that
    // answered DryRun where an armed run would have answered Refused would hide
    // the very thing the dry run exists to find.
    let cfg = cfg_with(vec![press("entr")], vec![press("entr")]);
    let mut unarmed = Actuator::new(&cfg);
    assert!(matches!(unarmed.apply(1, None), Applied::Refused(_)));

    let mut armed = cfg.clone();
    armed.actuator.arm = true;
    // Not constructed here — that would create an Enigo. What is asserted is that
    // the decision is made by a function of the sequence alone, which is the only
    // thing the armed path could consult before its first click.
    assert!(check_sequence("actuator.z_up", &armed.actuator.z_up, None).is_err());
}

// ------------------------------------------------------- {z} substitution

#[test]
fn z_is_written_with_two_decimals() {
    // As the acquisition software writes it: 9741.19, not 9741.1900000001 and not
    // 9741.2.
    assert_eq!(substitute("{z}", Some(9741.19)), "9741.19");
    assert_eq!(substitute("{z}", Some(9712.0)), "9712.00");
    assert_eq!(substitute("{z}", Some(-3.456)), "-3.46");
    assert_eq!(substitute("{z}", Some(0.0)), "0.00");
    assert_eq!(substitute("z={z} um", Some(9741.19)), "z=9741.19 um");
    assert_eq!(
        substitute("{z} {z}", Some(5.5)),
        "5.50 5.50",
        "every occurrence"
    );
    assert_eq!(substitute("no placeholder", Some(1.0)), "no placeholder");
    assert_eq!(
        substitute("{Z}", Some(1.0)),
        "{Z}",
        "the placeholder is lower case"
    );
    assert_eq!(substitute("{z}", None), "{z}", "nothing to put in");
}

#[test]
fn a_text_input_wanting_z_is_refused_until_z_is_known() {
    let seq = vec![
        click(1500, 300),
        Step::TextInput {
            text: "{z}".into(),
            delay: 0.0,
        },
        press("enter"),
    ];
    let cfg = cfg_with(seq.clone(), seq);
    let mut a = Actuator::new(&cfg);

    // Early in a recording there is no zPosition yet, and typing the literal
    // characters "{z}" into the Z field would be a silent disaster.
    let why = refusal(a.apply(1, None));
    assert!(why.contains("{z}"), "{why}");
    assert!(why.contains("zPosition"), "the message must say why: {why}");

    // With a z, the same sequence is fine.
    assert_eq!(a.apply(1, Some(9741.19)), Applied::DryRun { steps: 1 });

    // A sequence with no placeholder does not need a z at all.
    let plain = vec![Step::TextInput {
        text: "up".into(),
        delay: 0.0,
    }];
    let cfg = cfg_with(plain.clone(), plain);
    let mut a = Actuator::new(&cfg);
    assert_eq!(a.apply(1, None), Applied::DryRun { steps: 1 });
}

// ------------------------------------------------------- the emergency stop

#[test]
fn the_stop_is_sticky() {
    let cfg = cfg_with(vec![click(1, 1)], vec![click(1, 2)]);
    let mut a = Actuator::new(&cfg);
    assert!(!a.stopped());

    // Setting the flag is the only way to reach this state without an Enigo to
    // put a pointer in front of; what is under test is that nothing un-sets it.
    a.stopped = true;

    assert!(a.stopped());
    assert!(
        a.check_stop(),
        "the pointer is nowhere near the corner, and it stays stopped"
    );
    assert!(a.stopped(), "asking must not clear it either");
    assert_eq!(a.apply(1, Some(9741.19)), Applied::Stopped);
    assert_eq!(a.apply(-1, None), Applied::Stopped);
    assert_eq!(a.apply(0, None), Applied::Stopped, "not even a no-op");
}

#[test]
fn a_stopped_actuator_does_not_argue_about_the_config() {
    // Stopped beats Refused: once the user has reached for the corner, the state
    // of config.yaml is not the news.
    let mut a = Actuator::new(&Config::default());
    a.stopped = true;
    assert_eq!(a.apply(1, None), Applied::Stopped);
}

#[test]
fn the_corner_can_be_turned_off() {
    let mut cfg = cfg_with(vec![click(1, 1)], vec![click(1, 2)]);
    cfg.actuator.emergency_stop_corner = false;
    let mut a = Actuator::new(&cfg);
    assert!(!a.check_stop());
    assert!(!a.stopped());
}

// ---------------------------------------------------------------- durations

#[test]
fn nonsense_delays_do_not_panic() {
    use std::time::Duration;
    // All of these can be typed into config.yaml — `.inf` and `.nan` are valid
    // YAML floats — and Duration::from_secs_f64 panics on every one of them. A
    // panic here would be a panic half-way through a click sequence.
    assert_eq!(duration_secs(f64::NAN), Duration::ZERO);
    // Non-finite becomes zero rather than the cap: nonsense should not hang the
    // stabiliser for a day, and zero at least stays predictable.
    assert_eq!(duration_secs(f64::INFINITY), Duration::ZERO);
    assert_eq!(duration_secs(f64::NEG_INFINITY), Duration::ZERO);
    assert_eq!(duration_secs(-1.0), Duration::ZERO);
    assert_eq!(duration_secs(0.0), Duration::ZERO);
    assert_eq!(duration_secs(1.5), Duration::from_millis(1500));
    assert_eq!(duration_secs(1.0e300), Duration::from_secs(86_400));
}

#[test]
fn settle_comes_from_the_config() {
    let mut cfg = cfg_with(vec![click(1, 1)], vec![click(1, 2)]);
    cfg.actuator.settle_s = 2.5;
    assert_eq!(
        Actuator::new(&cfg).settle,
        std::time::Duration::from_millis(2500)
    );

    cfg.actuator.settle_s = -1.0;
    assert_eq!(Actuator::new(&cfg).settle, std::time::Duration::ZERO);
}

// --------------------------------------------- what only an Enigo can show

#[test]
#[ignore = "creates an Enigo, which takes hold of the input system"]
fn armed_creates_an_enigo_up_front() {
    // Up front, not on the first correction: the session polls check_stop from
    // its first loop, and a stop that only started working after the first click
    // would be no stop at all.
    let mut cfg = cfg_with(vec![click(1850, 420)], vec![click(1850, 470)]);
    cfg.actuator.arm = true;
    let a = Actuator::new(&cfg);
    assert!(a.enigo.is_some());
}

#[test]
#[ignore = "creates an Enigo, and moves the pointer if it regresses"]
fn armed_refusal_happens_before_the_first_click() {
    // The real proof of the ordering, which the unarmed path cannot give because
    // it never sleeps: a sequence carrying a minute of delays has to come back
    // refused immediately. Run this by hand, not on the acquisition machine
    // mid-experiment — if it fails it will have clicked at (1, 2).
    let mut cfg = cfg_with(
        vec![
            Step::Click {
                x: 1,
                y: 2,
                delay: 60.0,
            },
            press("entr"),
        ],
        vec![click(1, 3)],
    );
    cfg.actuator.arm = true;
    let mut a = Actuator::new(&cfg);

    let t0 = std::time::Instant::now();
    assert!(matches!(a.apply(1, None), Applied::Refused(_)));
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(1),
        "it clicked first"
    );
}
