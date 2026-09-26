//! Tests for the per-frame XML.
//!
//! The fixtures here use the prefixes the reference acquisition actually uses —
//! `base:` for the geometry, `lsmframe:` for the stage, `commonparam:` for the
//! galvo offsets — because that is the fact the whole prefix-agnostic approach
//! rests on. A test written against `commonimage:width` would pass while the
//! reader found nothing in a real file.

use super::*;

/// A `frameProperties` document in the shape the reference acquisition writes,
/// abridged to the fields that are read.
fn real_shaped_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"ASCII\"?>\r\n\
     <lsmframe:frameProperties xmlns:base=\"http://www.olympus.co.jp/hpf/model/base\">\
     <base:name>t001_0_1</base:name>\
     <base:creationDateTime>2025-10-07T21:58:59.990-04:00</base:creationDateTime>\
     <base:width>512</base:width>\
     <base:height>512</base:height>\
     <base:depth>2</base:depth>\
     <base:bitCounts>10</base:bitCounts>\
     <commonframe:axisType>TIMELAPSE</commonframe:axisType>\
     <commonparam:shiftXPosition>8122</commonparam:shiftXPosition>\
     <commonparam:shiftYPosition>7286</commonparam:shiftYPosition>\
     <lsmframe:zPosition>9741.19</lsmframe:zPosition>\
     <lsmframe:zBase>9712.19</lsmframe:zBase>\
     <commonframe:channel><commonframe:bitCounts>10</commonframe:bitCounts></commonframe:channel>\
     <commonframe:channel><commonframe:bitCounts>10</commonframe:bitCounts></commonframe:channel>\
     </lsmframe:frameProperties>"
        .to_string()
}

/// The 40-byte binary prefix of `u32`s the real block carries before its XML.
fn block_payload(xml: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for n in [1u32, 2, 1, 4, 3400, 1, 1, 1, 1, 3558] {
        v.extend(n.to_le_bytes());
    }
    v.extend(xml.as_bytes());
    v
}

// ----------------------------------------------------------- xml_of_block

#[test]
fn finds_xml_after_the_binary_prefix() {
    let xml = real_shaped_xml();
    let payload = block_payload(&xml);
    let found = xml_of_block(&payload).expect("the declaration is in there");
    assert!(found.starts_with("<?xml"));
    assert!(found.contains("<base:width>512</base:width>"));
}

#[test]
fn no_xml_in_a_pixel_block() {
    // Most blocks are pixels, and this is called on all of them.
    let pixels: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    assert!(xml_of_block(&pixels).is_none());
    assert!(xml_of_block(&[]).is_none());
    // A partial declaration is not one.
    assert!(xml_of_block(b"<?xm").is_none());
}

#[test]
fn stops_at_a_second_document() {
    // A metadata block can hold several documents end to end; only the first
    // belongs to this frame. Without the cut, a `width` from a later document
    // could be picked up as the frame's.
    let payload = block_payload(&format!(
        "{}\u{0}\u{0}<?xml version=\"1.0\"?><other:doc><base:width>9999</base:width></other:doc>",
        real_shaped_xml()
    ));
    let found = xml_of_block(&payload).expect("finds the first");
    assert!(
        !found.contains("9999"),
        "the second document leaked in: {found}"
    );
    assert_eq!(found.matches("<?xml").count(), 1);
}

#[test]
fn survives_a_bad_byte() {
    // These declare ASCII but carry operator-typed fields. One bad byte in a
    // field nobody reads is no reason to lose the frame's geometry.
    let mut payload = block_payload(&real_shaped_xml());
    payload.extend([0xFF, 0xFE]);
    let found = xml_of_block(&payload).expect("truncated at the bad byte, not discarded");
    assert!(found.contains("<base:height>512</base:height>"));
}

// ------------------------------------------------------------------- tag

#[test]
fn tag_ignores_the_namespace_prefix() {
    let xml = real_shaped_xml();
    assert_eq!(tag(&xml, "width"), Some("512"));
    assert_eq!(tag(&xml, "zPosition"), Some("9741.19"));
    assert_eq!(tag(&xml, "shiftXPosition"), Some("8122"));
    assert_eq!(tag(&xml, "axisType"), Some("TIMELAPSE"));
}

#[test]
fn tag_takes_the_first_occurrence() {
    // `bitCounts` is stated three times in a real document: once under `base:`
    // for the frame, then once per channel. Only the first is the frame's.
    let xml = real_shaped_xml();
    assert_eq!(
        xml.matches("bitCounts>").count(),
        6,
        "three open, three close"
    );
    assert_eq!(tag(&xml, "bitCounts"), Some("10"));

    // Made distinguishable, so the test would notice if it took the last.
    let xml = xml.replace(
        "<commonframe:bitCounts>10</commonframe:bitCounts>",
        "<commonframe:bitCounts>12</commonframe:bitCounts>",
    );
    assert_eq!(
        tag(&xml, "bitCounts"),
        Some("10"),
        "the frame's own bit depth, not a channel's"
    );
}

#[test]
fn tag_requires_the_colon() {
    // `:tag>` rather than `tag>`, so a longer name that merely ends with the one
    // asked for does not match.
    let xml = "<base:bitCounts>10</base:bitCounts>";
    assert_eq!(tag(xml, "Counts"), None);
    assert_eq!(tag(xml, "bitCounts"), Some("10"));
}

#[test]
fn tag_skips_a_closing_tag() {
    // `:zPosition>` occurs in `</lsmframe:zPosition>` too. A naive first match
    // would find the closing tag of an empty element and return everything after
    // it, to the end of the document.
    let xml = "<a:zPosition></a:zPosition><a:next>7</a:next>";
    assert_eq!(
        tag(xml, "zPosition"),
        Some(""),
        "an empty element is empty, not the rest of the file"
    );
    assert_eq!(tag(xml, "next"), Some("7"));
}

#[test]
fn tag_is_none_when_absent() {
    let xml = real_shaped_xml();
    assert_eq!(tag(&xml, "nothingLikeThis"), None);
    assert_eq!(tag("", "width"), None);
}

// -------------------------------------------------- parse_frame_properties

#[test]
fn reads_every_field_from_a_real_shaped_document() {
    let m = parse_frame_properties(&real_shaped_xml());
    assert_eq!(m.name.as_deref(), Some("t001_0_1"));
    assert_eq!(m.created.as_deref(), Some("2025-10-07T21:58:59.990-04:00"));
    assert_eq!(m.width, Some(512));
    assert_eq!(m.height, Some(512));
    assert_eq!(m.depth, Some(2));
    assert_eq!(m.bit_counts, Some(10));
    assert_eq!(m.z_position, Some(9741.19));
    assert_eq!(m.z_base, Some(9712.19));
    assert_eq!(m.shift_x, Some(8122));
    assert_eq!(m.shift_y, Some(7286));
    assert_eq!(m.axis_type.as_deref(), Some("TIMELAPSE"));
    // And the bit depth it read is the one the saturation check needs.
    assert_eq!(m.full_scale(), 1023.0);
}

#[test]
fn missing_fields_are_none_not_an_error() {
    // A file from another Olympus version may not carry all of them. No
    // zPosition means no click verification, not a refusal to run.
    let m = parse_frame_properties(
        "<?xml version=\"1.0\"?><a:doc><base:width>256</base:width></a:doc>",
    );
    assert_eq!(m.width, Some(256));
    assert_eq!(m.height, None);
    assert_eq!(m.z_position, None);
    assert_eq!(m.name, None);
}

#[test]
fn a_stated_zero_size_is_refused() {
    // Everything downstream multiplies these to size a plane. A stated zero is
    // worse than silence: it would make a plane of no bytes look complete.
    let m = parse_frame_properties(
        "<a:d><base:width>0</base:width><base:height>0</base:height><base:depth>0</base:depth></a:d>",
    );
    assert_eq!(m.width, None);
    assert_eq!(m.height, None);
    assert_eq!(m.depth, None);
}

#[test]
fn a_non_numeric_z_is_none_not_zero() {
    // A `0.0` here would read as a stage at the origin, and the click
    // verification would then "confirm" every click by comparing 0.0 with 0.0.
    let m = parse_frame_properties("<a:d><lsmframe:zPosition>9741,19</lsmframe:zPosition></a:d>");
    assert_eq!(m.z_position, None);
    let m = parse_frame_properties("<a:d><lsmframe:zPosition></lsmframe:zPosition></a:d>");
    assert_eq!(m.z_position, None);
}

#[test]
fn garbage_does_not_panic() {
    // This is fed whatever a half-written block contains.
    for junk in [
        "",
        "<",
        "<<<>>>",
        "<a:><b:>",
        "<?xml",
        "<a:width>abc</a:width>",
    ] {
        let _ = parse_frame_properties(junk);
    }
}
