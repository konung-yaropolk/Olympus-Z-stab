//! The per-frame `lsmframe:frameProperties` XML.
//!
//! Each timepoint is preceded by a ~3.6 kB type-1 block holding one XML
//! document. The block has a 40-byte binary prefix of `u32`s before the
//! `<?xml` — so the XML is found, not assumed to start at byte zero.
//!
//! This reads the document by looking for tags, not by parsing XML. That is a
//! deliberate limit, not a shortcut: the fields wanted are a dozen leaf elements
//! with distinctive names in a document whose namespace prefixes vary between
//! Olympus versions, and matching `:<tag>>` ignores the prefix for free. It
//! would be wrong for a document where the same leaf name appears under
//! different parents with different meanings — so the two that do, `width` and
//! `height`, are taken from the *first* occurrence, which is the frame's own.
//!
//! # What the real document actually looks like
//!
//! Prefix-agnostic matching was written on the strength of the argument above,
//! and then the reference acquisition turned out to *need* it. Every field this
//! reads is carried under `base:` or `commonparam:`, not under the
//! `commonframe:` / `commonimage:` prefixes the container's own block names
//! would suggest:
//!
//! ```text
//!   <base:name>t001_0_1</base:name>
//!   <base:creationDateTime>2025-10-07T21:58:59.990-04:00</base:creationDateTime>
//!   <base:width>512</base:width>  <base:height>512</base:height>
//!   <base:depth>2</base:depth>    <base:bitCounts>10</base:bitCounts>
//!   <commonframe:axisType>TIMELAPSE</commonframe:axisType>
//!   <commonparam:shiftXPosition>8122</commonparam:shiftXPosition>
//!   <lsmframe:zPosition>9741.19</lsmframe:zPosition>
//!   <lsmframe:zBase>9712.19</lsmframe:zBase>
//! ```
//!
//! So a reader that had matched whole qualified names would have found nothing
//! at all in the file it was written for, and the "absent fields are `None`"
//! policy below would have turned that into a silent run with no geometry rather
//! than into an error.
//!
//! `bitCounts` is the field that makes "first occurrence" load-bearing rather
//! than merely tidy: the real document states it three times — once as
//! `base:bitCounts` for the frame, then once per channel as
//! `commonframe:bitCounts`. All three happen to say 10 here, but only the first
//! is the frame's.

use crate::frame::FrameMeta;

/// The text of the first XML document in a block payload.
///
/// Returns `None` when the block holds no XML, which is most of them.
///
/// Two details beyond finding `<?xml`. The payload is cut at a *second*
/// declaration if there is one, because a metadata block can hold several
/// documents laid end to end with binary padding between them and only the first
/// belongs to this frame. And a payload that is not valid UTF-8 is truncated at
/// the first bad byte rather than thrown away: these documents declare
/// `encoding="ASCII"` but carry operator-typed fields, and one bad byte in a
/// field nobody reads is no reason to lose the frame's geometry. Trailing binary
/// padding is left on the end, which is harmless — nothing here reads the
/// document as a whole, only searches it for named tags.
pub fn xml_of_block(payload: &[u8]) -> Option<&str> {
    const DECL: &[u8] = b"<?xml";
    let start = payload.windows(DECL.len()).position(|w| w == DECL)?;
    let rest = &payload[start..];
    // A second declaration ends this document. Searched from past the first one
    // so the first is not rediscovered.
    let end = rest
        .get(DECL.len()..)
        .and_then(|tail| tail.windows(DECL.len()).position(|w| w == DECL))
        .map(|at| at + DECL.len())
        .unwrap_or(rest.len());
    let doc = &rest[..end];
    match std::str::from_utf8(doc) {
        Ok(s) => Some(s),
        // `valid_up_to` is a char boundary by construction, so this cannot
        // panic, and it keeps everything before the offending byte.
        Err(e) => std::str::from_utf8(&doc[..e.valid_up_to()]).ok(),
    }
}

/// The text content of the first `<*:tag>` element, ignoring the namespace
/// prefix.
///
/// Matches on `:tag>` so that `<commonframe:name>` and `<base:name>` both answer
/// to `"name"`, and so that a tag whose name merely *ends* with the one asked
/// for — `<commonimage:bitCounts>` against `"Counts"` — does not match, because
/// the `:` has to be there.
///
/// A match is only taken when it really is an opening tag: `:zPosition>` occurs
/// in `</lsmframe:zPosition>` as well, and in a document where the value is
/// empty (`<a:x></a:x>`) the naive first match would be the closing tag and the
/// "content" everything after it, to the end of the file. So each candidate is
/// walked back to its `<` and skipped when what follows is a `/`.
pub fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!(":{name}>");
    let mut from = 0usize;
    while let Some(rel) = xml[from..].find(&needle) {
        let at = from + rel;
        let after = at + needle.len();
        // Walk back to the `<` that opens this tag. A `>` on the way means the
        // match is not inside a tag at all — it is text — so it is skipped.
        let open = xml[..at].rfind('<');
        let closing = match open {
            Some(o) => {
                if xml[o..at].contains('>') {
                    None
                } else {
                    Some(xml[o + 1..].starts_with('/'))
                }
            }
            None => None,
        };
        match closing {
            // An opening tag: the content runs to the next `<`.
            Some(false) => {
                let rest = &xml[after..];
                let end = rest.find('<').unwrap_or(rest.len());
                return Some(rest[..end].trim());
            }
            // A closing tag, or a match that is not a tag: keep looking.
            _ => from = after,
        }
    }
    None
}

/// Every field of a `frameProperties` document that this program uses.
///
/// Absent fields are `None` rather than an error: a file from a different
/// Olympus version may not carry all of them, and the program degrades — no
/// `zPosition` means no click verification, not a refusal to run.
///
/// The numeric fields are parsed rather than trusted. `zPosition` is written
/// `9741.19` in every file seen, but a version that wrote `9741,19` or
/// `9.74119E3` would otherwise become a `0.0` that reads as a stage at the
/// origin, and the click verification would then "confirm" every click.
pub fn parse_frame_properties(xml: &str) -> FrameMeta {
    FrameMeta {
        name: tag(xml, "name").map(str::to_string),
        created: tag(xml, "creationDateTime").map(str::to_string),
        // Sizes are refused at zero: everything downstream multiplies them
        // together to size a plane, and a stated `0` is worse than silence.
        width: tag(xml, "width")
            .and_then(|v| v.parse().ok())
            .filter(|w| *w > 0),
        height: tag(xml, "height")
            .and_then(|v| v.parse().ok())
            .filter(|h| *h > 0),
        depth: tag(xml, "depth")
            .and_then(|v| v.parse().ok())
            .filter(|d| *d > 0),
        bit_counts: tag(xml, "bitCounts").and_then(|v| v.parse().ok()),
        z_position: tag(xml, "zPosition").and_then(|v| v.parse().ok()),
        z_base: tag(xml, "zBase").and_then(|v| v.parse().ok()),
        shift_x: tag(xml, "shiftXPosition").and_then(|v| v.parse().ok()),
        shift_y: tag(xml, "shiftYPosition").and_then(|v| v.parse().ok()),
        axis_type: tag(xml, "axisType").map(str::to_string),
    }
}

#[cfg(test)]
#[path = "meta_tests.rs"]
mod meta_tests;
