//! Tests for the shared types.
//!
//! Two things here are worth testing and easy to get wrong. `full_scale` decides
//! what counts as a saturated pixel, and the recordings this runs on are 10-bit
//! in 16-bit words — a check that assumed `u16::MAX` would never fire, so a
//! saturated frame would be measured as catastrophic defocus and drive the stage.
//! And `interval_s` parses a timestamp by hand rather than with a date library,
//! which is fine for a difference inside one recording and needs its edges
//! pinned down.

use super::*;

fn meta_at(created: &str) -> FrameMeta {
    FrameMeta {
        created: Some(created.to_string()),
        ..FrameMeta::default()
    }
}

// ------------------------------------------------------------- full_scale

#[test]
fn full_scale_uses_the_stated_bit_depth() {
    // 10 bits is what the reference acquisition states, and 1023 is the value a
    // saturated pixel actually takes in it.
    let m = FrameMeta {
        bit_counts: Some(10),
        ..FrameMeta::default()
    };
    assert_eq!(m.full_scale(), 1023.0);

    let m = FrameMeta {
        bit_counts: Some(12),
        ..FrameMeta::default()
    };
    assert_eq!(m.full_scale(), 4095.0);

    let m = FrameMeta {
        bit_counts: Some(16),
        ..FrameMeta::default()
    };
    assert_eq!(m.full_scale(), 65535.0);
}

#[test]
fn full_scale_falls_back_to_sixteen_bits_when_unstated() {
    // Inert rather than wrong: with no bit depth the saturation check should
    // never fire, not fire on everything.
    assert_eq!(FrameMeta::default().full_scale(), 65535.0);
}

#[test]
fn full_scale_refuses_an_impossible_bit_depth() {
    // A half-read metadata block can state anything. Zero bits would give a full
    // scale of zero, against which every pixel is saturated and every frame is
    // rejected — the recording would be watched and never measured.
    for bad in [0u32, 17, 32, u32::MAX] {
        let m = FrameMeta {
            bit_counts: Some(bad),
            ..FrameMeta::default()
        };
        assert_eq!(
            m.full_scale(),
            65535.0,
            "bit_counts {bad} should fall back, not be believed"
        );
    }
}

// ------------------------------------------------------------- interval_s

#[test]
fn interval_s_on_the_real_timestamp_format() {
    // Two consecutive frames of the reference acquisition, 7.5 Hz.
    let a = meta_at("2025-10-07T21:58:59.990-04:00");
    let b = meta_at("2025-10-07T21:59:00.123-04:00");
    let dt = b.interval_s(&a).expect("both timestamps parse");
    assert!(
        (dt - 0.133).abs() < 1e-6,
        "expected 0.133 s between frames, got {dt}"
    );
}

#[test]
fn interval_s_handles_every_zone_form() {
    // The `+hh:mm` case had a bug once: the offset was stripped by splitting on
    // `+`, and then the `-` strip fell back to the *unstripped* string, so the
    // seconds field came out as `00.123+04` and failed to parse. All three forms
    // must give the same answer.
    let pairs = [
        (
            "2025-10-07T21:58:59.990-04:00",
            "2025-10-07T21:59:00.123-04:00",
        ),
        (
            "2025-10-07T21:58:59.990+04:00",
            "2025-10-07T21:59:00.123+04:00",
        ),
        ("2025-10-07T21:58:59.990Z", "2025-10-07T21:59:00.123Z"),
        // No zone at all.
        ("2025-10-07T21:58:59.990", "2025-10-07T21:59:00.123"),
    ];
    for (a, b) in pairs {
        let dt = meta_at(b)
            .interval_s(&meta_at(a))
            .unwrap_or_else(|| panic!("{a} .. {b} did not parse"));
        assert!(
            (dt - 0.133).abs() < 1e-6,
            "{a} .. {b} gave {dt}, expected 0.133"
        );
    }
}

#[test]
fn interval_s_crosses_a_minute_and_an_hour() {
    let dt = meta_at("2025-10-07T22:00:00.000-04:00")
        .interval_s(&meta_at("2025-10-07T21:59:59.900-04:00"))
        .expect("parses");
    assert!((dt - 0.1).abs() < 1e-6, "across a minute: {dt}");

    // A part of the reference recording is 136 s, and a session is half an hour,
    // so an hour boundary inside one session is reachable.
    let dt = meta_at("2025-10-07T23:00:01.000-04:00")
        .interval_s(&meta_at("2025-10-07T22:59:59.000-04:00"))
        .expect("parses");
    assert!((dt - 2.0).abs() < 1e-6, "across an hour: {dt}");
}

#[test]
fn interval_s_is_signed() {
    // The caller subtracts the session's first frame from the current one, so a
    // negative answer is what a frame *before* the reference must give — not an
    // absolute value that would make time run backwards look like progress.
    let dt = meta_at("2025-10-07T21:58:59.990-04:00")
        .interval_s(&meta_at("2025-10-07T21:59:00.123-04:00"))
        .expect("parses");
    assert!((dt + 0.133).abs() < 1e-6, "expected -0.133, got {dt}");
}

#[test]
fn interval_s_is_none_without_two_timestamps() {
    let with = meta_at("2025-10-07T21:58:59.990-04:00");
    let without = FrameMeta::default();
    assert_eq!(with.interval_s(&without), None, "missing earlier timestamp");
    assert_eq!(without.interval_s(&with), None, "missing later timestamp");
    assert_eq!(without.interval_s(&without), None, "neither present");
}

#[test]
fn interval_s_is_none_on_malformed_input() {
    // A truncated metadata block can hand over anything. None is right: the
    // caller shows elapsed time as zero, which is visibly wrong in a log rather
    // than silently wrong in a number.
    let good = meta_at("2025-10-07T21:58:59.990-04:00");
    for bad in [
        "",
        "2025-10-07",          // date only, no `T`
        "2025-10-07T21:58",    // no seconds field
        "2025-10-07Txx:yy:zz", // not numbers
        "T::",
        "2025-10-07T21;58;59.990", // wrong separators
    ] {
        assert_eq!(
            meta_at(bad).interval_s(&good),
            None,
            "{bad:?} should not parse"
        );
        assert_eq!(
            good.interval_s(&meta_at(bad)),
            None,
            "{bad:?} should not parse as the earlier one either"
        );
    }
}
