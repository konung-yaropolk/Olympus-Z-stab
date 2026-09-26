//! Reading an OIR **while the acquisition software is still writing it**.
//!
//! # Why this cannot be FastTIFF's OIR reader
//!
//! The container is the one FastTIFF's importer documents, and this module reads
//! it the same way — but it cannot reuse that reader, for two reasons that are
//! both about the file not being finished:
//!
//! 1. **The block index is the last thing written.** A finished OIR states at
//!    `0x28` where its index lives, and the index runs from there to the end of
//!    the file, one `u64` per block. A file still being written has no index yet
//!    and that offset is not yet meaningful. FastTIFF's reader starts from the
//!    index, so on a growing file it has nothing to start from.
//!
//! 2. **It memory-maps.** Its own safety comment is explicit that the file must
//!    not be written while mapped, which is exactly the case here. A mapping's
//!    length is also fixed when it is made, so it would never see a byte the
//!    scope wrote afterwards.
//!
//! # What this does instead
//!
//! Blocks are `u32 length, u32 type, length bytes` — self-delimiting — so the
//! stream can be walked forward from the first block without any index at all.
//! On the real 1.076 GB acquisition this was built against, walking from `0x60`
//! visits 10,219 blocks and lands **exactly** on the offset the header gives for
//! the index, and every one of the 10,218 offsets the index lists is a block the
//! walk visited. The walk and the index agree, so the walk is sound on a file
//! that has no index yet.
//!
//! Reads are plain buffered reads at an offset, never a mapping, and never
//! re-read: the reader keeps a cursor and only ever moves forward. There is
//! exactly one backwards read in the module, [`LiveReader::declared_index`],
//! which re-reads the eight bytes at `0x28` once per poll — the field is written
//! when the part is *closed*, so its value can only be learned by looking again.
//!
//! Blocks whose contents are of no interest are never read at all, only stepped
//! over: on the reference acquisition that skips the reference snapshot's 524 kB,
//! two multi-megabyte lookup-table blobs and a BMP thumbnail, and it means the
//! bytes this module actually copies are the pixels it was asked for and nothing
//! else.
//!
//! ## Where the first block is
//!
//! Not at a fixed offset. `0x00` is the signature, `0x20` the total size, `0x28`
//! the index offset, `0x48` the string `FLUOVIEW` — and then the real file has a
//! 16-byte `u32 3, u32 2, u64 -1` structure at `0x50` before its first block at
//! `0x60`, while the synthetic fixtures in FastTIFF's own tests begin at `0x50`.
//! So the start is **found, not assumed**: see [`find_first_block`], which tries
//! each candidate and keeps the one from which a run of blocks parses.
//!
//! ## What a timepoint looks like
//!
//! In file order, per timepoint, from the real recording:
//!
//! ```text
//!   type 1   frameProperties XML for t00N, ~3.6 kB   <- arrives BEFORE the pixels
//!   type 3   descriptor  t00N_0_1_<uidA>  at=0       run=485376
//!   type 4   data                                    485376 bytes
//!   type 3   descriptor  t00N_0_1_<uidB>  at=0       run=485376
//!   type 4   data
//!   type 3   descriptor  t00N_0_1_<uidA>  at=485376  run=38912
//!   type 4   data
//!   type 3   descriptor  t00N_0_1_<uidB>  at=485376  run=38912
//!   type 4   data
//!   type 5   empty, length 0
//! ```
//!
//! Three things in that are load-bearing. The **XML comes first**, so the frame
//! size is known before the first pixel arrives and nothing has to be guessed.
//! The **channels interleave**, so a plane's chunks are not contiguous and each
//! must be placed at its declared offset. And a plane is `485376 + 38912 =
//! 524288` bytes — `512 * 512 * 2` — so a plane is **complete when its declared
//! runs cover `width * height * depth` bytes**, which is the only completeness
//! test that does not depend on the chunking staying the way it is.
//!
//! A leading `REF_LSM0_<uid>` plane, delivered as 18 chunks before the
//! timelapse, is the reference snapshot and must not be mistaken for a frame.
//!
//! ## Rolling over to the next part
//!
//! A long recording is split at about 1.076 GB into `<stem>.oir`, then
//! `<stem>_00001`, `<stem>_00002`, … **with no extension** — the real
//! acquisition writes them that way, and the 13-part recording this was built
//! against is named exactly so.
//!
//! During a live recording the next part does not exist yet. So exhausting the
//! current part is never treated as the end: the reader looks for the next part,
//! and only when neither the current part nor a successor has grown for
//! `idle_timeout_s` does it report the acquisition finished.
//!
//! ### The numbering does not restart per part
//!
//! It is natural to assume each part starts again at `t001` and that a reader
//! should therefore add a running offset. The reference acquisition says
//! otherwise: its thirteen parts are numbered **continuously**:
//! part 0 holds `t001`..`t1018`, part 1 holds `t1019`..`t2037`, and so on to
//! `t12561` in part 12. An offset added blindly at each boundary would therefore
//! *double* the numbering — part 1's `t1019` would become 2037 — and every
//! elapsed time and every log line downstream would be wrong by a growing
//! amount.
//!
//! So the offset is not "the timepoints seen so far". It is pinned once per
//! part, from the first raw number that part mentions, to whatever makes that
//! frame exactly one past the highest the session has already seen:
//!
//! ```text
//!   offset = (highest session timepoint so far) + 1 - (this part's first raw n)
//! ```
//!
//! which is zero when the numbering continues, and the previous part's maximum
//! when it restarts at `t001`. One expression covers both, and neither case
//! needs to be detected. See `session_timepoint`.
//!
//! Note also that the axis numbers are **not fixed-width**: `t999` is followed
//! by `t1000`, so the digits after the axis letter have to be parsed as a
//! number. A reader that took three characters would read `t1000` as timepoint
//! 100 and quietly interleave it with the hundreds.

use crate::config::Config;
use crate::frame::{Frame, FrameMeta};
use std::fs::File;
use std::io;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The signature every OIR starts with.
pub const MAGIC: &[u8] = b"OLYMPUSRAWFORMAT";
/// Where the header states the block index begins — meaningless until the file
/// is finished, and so never relied on by this module.
pub const INDEX_OFFSET_AT: u64 = 0x28;
/// A descriptor block, naming the data block that follows it.
pub const TYPE_DESCRIPTOR: u32 = 3;
/// The block carrying pixel bytes.
pub const TYPE_DATA: u32 = 4;
/// A metadata block. The per-frame `frameProperties` XML arrives as this type.
pub const TYPE_META: u32 = 1;
/// Refusals for structures that cannot be real, so a half-written block is
/// rejected on a bound rather than becoming an allocation.
pub const MAX_BLOCK_BYTES: u32 = 64 << 20;
pub const MAX_PLANE_BYTES: usize = 1 << 30;
/// Parts of one acquisition to look for.
pub const MAX_PARTS: usize = 9_999;

/// The earliest a block can begin: `0x50` is the end of the fixed header, and
/// FastTIFF's own fixtures start there.
const FIRST_BLOCK_FROM: u64 = 0x50;
/// Candidate starts are eight-byte aligned, because every field in the header
/// and in the `u32 3, u32 2, u64 -1` structure at `0x50` is.
const FIRST_BLOCK_STEP: u64 = 8;
/// How far past the header to look. The real file's first block is at `0x60`,
/// two candidates in; this is room for a header structure an order of magnitude
/// larger than any seen, and a bound so a file of rubbish ends rather than
/// hangs.
const FIRST_BLOCK_CANDIDATES: usize = 64;
/// Consecutive plausible blocks [`LiveReader::open`] demands before believing a
/// candidate. One is not enough — see [`find_first_block`].
const FIRST_BLOCK_PROBE: usize = 4;
/// Block types seen in real files are 0 to 5. This is the bound
/// [`find_first_block`] uses to tell a real block header from two halves of
/// something else; the walk itself never checks the type, because an unknown
/// type is simply stepped over.
const MAX_BLOCK_TYPE: u32 = 15;
/// A descriptor is twelve bytes and a name. Anything larger is not one, and is
/// stepped over rather than read.
const MAX_DESCRIPTOR_BYTES: u64 = 64 << 10;
/// Metadata blocks in the reference acquisition are 3.6 kB. This is the largest
/// one that will be read as XML; the multi-megabyte lookup-table blobs arrive as
/// type 0 and are never read at all.
const MAX_META_BYTES: u64 = 4 << 20;
/// Channels are distinct UIDs in the block names. A file that invents more than
/// this is not a recording, and the excess is ignored rather than allocated for.
const MAX_CHANNELS: usize = 64;
/// Planes held part-assembled at once. Two are normally in flight (one per
/// channel of one timepoint); this is room for a scope that interleaves far more
/// aggressively, and a bound on memory rather than a limit anyone should reach.
const MAX_BUILDING: usize = 64;
/// How far behind the newest timepoint a part-assembled plane, or the metadata
/// of a timepoint, is kept before being dropped.
///
/// The acquisition writes timepoints in order and never returns to one, so a
/// plane this far behind will never be completed — it is what an acquisition
/// stopped mid-frame leaves behind, and keeping it would leak half a megabyte
/// per channel for the rest of the session.
const KEEP_TIMEPOINTS: u64 = 8;
/// Consecutive zero-length blocks tolerated before the walk gives up.
///
/// A zero-length block is legitimate — one type-5 marker ends every timepoint —
/// but a region of zeros parses as an endless run of them, and stepping eight
/// bytes at a time through a gigabyte of it is a hang, not an error.
const MAX_EMPTY_RUN: usize = 4_096;

/// Planes one poll will return before handing back what it has.
///
/// A live poll never reaches this: at 7.5 Hz with two channels and a 500 ms poll
/// interval, about eight planes arrive between polls. It bounds the case the
/// reader is *also* pointed at — a recording that is already complete, where one
/// unbounded poll would walk all thirteen parts and return every plane at once.
/// That is 25,000 planes of a quarter-megabyte for the reference recording, far
/// more memory than the machine beside the microscope has.
///
/// 256 planes of 512x512 is 64 MB in flight, and [`Poll::more`] tells the caller
/// to come straight back rather than sleeping out its poll interval.
const MAX_PLANES_PER_POLL: usize = 256;

/// One finished plane: one channel of one timepoint.
#[derive(Debug, Clone)]
pub struct Plane {
    /// Session-wide timepoint number, already offset past earlier parts.
    pub timepoint: u64,
    /// Channel index, assigned in the order the file first mentions each UID.
    pub channel: usize,
    /// The channel's UID from the block name, kept for the log.
    pub channel_uid: String,
    /// The `frameProperties` for this timepoint.
    pub meta: FrameMeta,
    /// Raw samples, `width * height` of them, little-endian `u16` as stored.
    pub samples: Vec<u16>,
}

/// What a poll found.
#[derive(Debug, Default)]
pub struct Poll {
    /// Planes finished since the last poll, in timepoint then channel order.
    pub planes: Vec<Plane>,
    /// True once the acquisition has stopped growing past the idle timeout.
    pub finished: bool,
    /// Set when the reader moved on to the next part file. A poll that crosses
    /// several boundaries at once — which only a replay of a finished recording
    /// does — reports the last of them.
    pub rolled_over: Option<PathBuf>,
    /// The poll stopped on its own batch limit, so more is waiting to be read
    /// right now. The caller should poll again immediately rather than sleeping.
    pub more: bool,
}

/// A tail-following reader over one acquisition.
pub struct LiveReader {
    /// Every part opened so far, the named file first.
    parts: Vec<PathBuf>,
    /// The part being read now, and the handle on it.
    current: usize,
    file: File,
    /// Offset in the current part of the next block to read.
    cursor: u64,
    /// Bytes the current part had when it was last looked at.
    known_len: u64,
    /// Timepoint numbers seen in earlier parts, added to this part's own.
    timepoint_offset: u64,
    /// Highest timepoint seen in the current part, for the offset above. Zero
    /// means none yet, which is also the flag that the offset for this part has
    /// not been pinned — axis numbers are one-based in every file seen.
    part_max_timepoint: u64,
    /// Geometry, from the first `frameProperties` seen.
    geometry: Option<Geometry>,
    /// Channel UIDs in first-seen order.
    channels: Vec<String>,
    /// Planes being reassembled, keyed by `(timepoint, channel)`.
    building: std::collections::BTreeMap<(u64, usize), Building>,
    /// The `frameProperties` of each timepoint, until its planes are done.
    frame_meta: std::collections::BTreeMap<u64, FrameMeta>,
    /// A descriptor that has been read and whose bytes have not arrived yet.
    ///
    /// The one piece of state the scaffold did not anticipate, and the module
    /// cannot honour "never read a byte twice" without it: a descriptor and the
    /// data block it names are a pair, and the tail of a growing file falls
    /// between them about as often as anywhere else. The alternative is to leave
    /// the cursor before the descriptor and read it again next poll, which is
    /// cheap — 67 bytes — but is still a re-read, and it puts the invariant
    /// "the cursor sits on a block that is part of a complete pair" in place of
    /// the simpler "the cursor sits on the next block".
    pending: Option<PendingDescriptor>,
    /// When the file was last seen to grow, for the idle timeout.
    last_growth: std::time::Instant,
    idle_timeout: std::time::Duration,
    /// Scratch buffer, reused across reads so a poll does not allocate.
    scratch: Vec<u8>,
}

/// Frame geometry, read from the file rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    /// Bytes per sample.
    pub depth: usize,
}

impl Geometry {
    /// Bytes one complete plane occupies — the completeness test.
    pub fn plane_bytes(&self) -> usize {
        self.width * self.height * self.depth
    }
}

/// A plane part-way through being reassembled.
struct Building {
    /// Bytes, sized to `Geometry::plane_bytes` the moment geometry is known.
    bytes: Vec<u8>,
    /// Bytes actually written, to test completeness against `plane_bytes`.
    ///
    /// A sum of the runs placed, each clipped to the plane, and capped at the
    /// plane's size. That is exact for a file whose chunks tile the plane, which
    /// every file seen does; two descriptors naming the *same* stretch twice
    /// would complete a plane while part of it was still zero. Nothing cheap
    /// distinguishes a written zero from an unwritten one, and the alternative —
    /// keeping every run and merging intervals — costs more than the failure it
    /// would catch is worth.
    covered: usize,
    uid: String,
}

/// A descriptor waiting for the data block that follows it.
struct PendingDescriptor {
    /// Offset within the plane these bytes belong at.
    at: usize,
    /// How many bytes the data block must hold for this pairing to be believed.
    run: u64,
    /// Session timepoint and channel index, resolved when the name was read.
    timepoint: u64,
    channel: usize,
    uid: String,
}

/// Why the walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Something that cannot be a block header — the index's `0xFFFFFFFF`
    /// marker, or the offset the header declares for it. This part is over and
    /// nothing more will ever be appended to it.
    Terminator,
    /// The part simply has no more *complete* blocks yet. It may well grow.
    Short,
    /// Enough planes for one poll. Says nothing about the part, which certainly
    /// has more to give — so this must never roll over and must never be reported
    /// as the acquisition having finished.
    Batch,
}

impl LiveReader {
    /// Open an acquisition and position the cursor at its first block.
    ///
    /// Opening must not require the file to be finished, and must not take any
    /// lock that would disturb the acquisition software writing it. Rust's
    /// `File::open` already asks Windows for `FILE_SHARE_READ | FILE_SHARE_WRITE
    /// | FILE_SHARE_DELETE`, which is what makes this safe to point at a file
    /// being recorded; nothing here should reach for a mapping or an exclusive
    /// handle.
    pub fn open(path: &Path, cfg: &Config) -> io::Result<LiveReader> {
        let mut file = File::open(path)?;
        let mut head = [0u8; 16];
        if file.read_exact(&mut head).is_err() || head[..] != *MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an OIR file: the OLYMPUSRAWFORMAT signature is missing",
            ));
        }
        let first = find_first_block(&file, FIRST_BLOCK_PROBE)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "this OIR's block stream does not begin anywhere this reader recognises, just \
                 past its header",
            )
        })?;
        let known_len = file.metadata()?.len();
        Ok(LiveReader {
            parts: vec![path.to_path_buf()],
            current: 0,
            file,
            cursor: first,
            known_len,
            timepoint_offset: 0,
            part_max_timepoint: 0,
            geometry: None,
            channels: Vec::new(),
            building: std::collections::BTreeMap::new(),
            frame_meta: std::collections::BTreeMap::new(),
            pending: None,
            last_growth: std::time::Instant::now(),
            // Clamped to a second. A zero timeout would report the acquisition
            // finished on the very first poll, before a frame had been read, and
            // the program would exit looking as though it had worked.
            idle_timeout: std::time::Duration::from_secs(cfg.input.idle_timeout_s.max(1)),
            scratch: Vec::new(),
        })
    }

    /// Look at the file once and return whatever has finished since last time.
    ///
    /// Must never block, never re-read a byte it has already consumed, and never
    /// consume a block that is not yet fully present: a block whose declared
    /// length runs past the current end of file is left alone, to be read on a
    /// later poll once the scope has written it.
    ///
    /// The index, once written, is its own terminator — its leading marker is
    /// `0xFFFFFFFF`, which as a block length exceeds [`MAX_BLOCK_BYTES`] and so
    /// stops the walk without needing to know it is there.
    ///
    /// Rolling over is deliberately conservative. The walk reaching a terminator
    /// means the part is closed for good, so a successor is taken up at once —
    /// which is what lets a replay of a finished thirteen-part recording cross
    /// every boundary in one poll. A walk that merely ran out of bytes in a part
    /// that *grew during this poll* is the ordinary live case, and it waits: a
    /// part still being appended to must not be abandoned because the next one
    /// has appeared beside it.
    pub fn poll(&mut self) -> io::Result<Poll> {
        let mut out = Poll::default();
        // Set when the walk stopped only because it had enough planes for one
        // poll. There is certainly more to read, so the acquisition cannot be
        // reported finished however long the file has sat still.
        let mut batched = false;
        loop {
            // Re-stat every time round. The length is the only thing that says
            // how much the scope has written, and it is the one piece of state
            // that cannot be cached.
            let len = match self.file.metadata() {
                Ok(m) => m.len(),
                // A part on a share that blinked out. Nothing has been read
                // wrongly, so carry on with what was known and look again next
                // poll rather than failing a recording over a hiccup.
                Err(_) => self.known_len,
            };
            let grew = len > self.known_len;
            if grew {
                self.known_len = len;
                self.last_growth = std::time::Instant::now();
            }
            let index_at = self.declared_index(len);
            let stop = self.walk(len, index_at, &mut out.planes)?;
            let exhausted = match stop {
                Stop::Terminator => true,
                Stop::Short => !grew,
                Stop::Batch => false,
            };
            batched |= stop == Stop::Batch;
            if !exhausted {
                break;
            }
            match self.roll_over()? {
                Some(next) => out.rolled_over = Some(next),
                None => break,
            }
        }
        // The walk completes planes in file order, which for every file seen is
        // already timepoint then channel order — but only because the channels
        // happen to interleave evenly. Sorting makes the promise in `Poll` true
        // of any interleaving.
        out.planes
            .sort_by(|a, b| (a.timepoint, a.channel).cmp(&(b.timepoint, b.channel)));
        self.prune();
        out.more = batched;
        out.more = batched;
        out.finished = !batched && self.last_growth.elapsed() >= self.idle_timeout;
        Ok(out)
    }

    /// Frame geometry, once the first `frameProperties` has been read.
    pub fn geometry(&self) -> Option<Geometry> {
        self.geometry
    }

    /// Channel UIDs in first-seen order; the index into this is `Plane::channel`.
    pub fn channels(&self) -> &[String] {
        &self.channels
    }

    /// The part being read now.
    pub fn part_path(&self) -> &Path {
        &self.parts[self.current]
    }

    pub fn parts_opened(&self) -> usize {
        self.parts.len()
    }

    /// Bytes consumed of the current part.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Where the header says this part's index begins, if that is a place a
    /// block could end.
    ///
    /// The only backwards read in the module, and the only byte read twice. It
    /// has to be: the field is zero while the part is being written and is
    /// filled in when the part is closed, so the *value* is new information even
    /// though the offset is not. Eight bytes once per poll.
    ///
    /// A value that is not plausible — zero while recording, or rubbish — is
    /// discarded and the walk falls back to the [`MAX_BLOCK_BYTES`] bound, which
    /// is what actually stops it on every file seen. A plausible value is used
    /// only as an exact stopping point, never to shorten the walk: a garbage
    /// offset that happens to land inside the pixel data would otherwise
    /// truncate the recording, whereas it can only stop a walk that arrives at
    /// precisely that byte.
    fn declared_index(&self, len: u64) -> Option<u64> {
        let mut file = &self.file;
        let mut buf = [0u8; 8];
        if file.seek(SeekFrom::Start(INDEX_OFFSET_AT)).is_err() {
            return None;
        }
        if file.read_exact(&mut buf).is_err() {
            return None;
        }
        let at = u64::from_le_bytes(buf);
        (at >= FIRST_BLOCK_FROM && at <= len).then_some(at)
    }

    /// Walk forward through the current part, taking every complete block.
    ///
    /// Everything that could make this consume a block that is not fully there
    /// happens *before* the descriptor in hand is taken out of `pending`, so
    /// every early return leaves the reader exactly where it was.
    fn walk(&mut self, len: u64, index_at: Option<u64>, out: &mut Vec<Plane>) -> io::Result<Stop> {
        // Field-by-field so the borrow checker can see that reading into the
        // scratch buffer and placing bytes into a plane touch different things.
        let LiveReader {
            file,
            cursor,
            scratch,
            building,
            frame_meta,
            geometry,
            channels,
            pending,
            timepoint_offset,
            part_max_timepoint,
            ..
        } = self;
        // The file's own position, so a forward walk costs no seeks at all: each
        // read leaves the handle exactly where the next one starts. `u64::MAX`
        // is "unknown", which forces the first read to seek.
        let mut pos = u64::MAX;
        let mut empty_run = 0usize;
        loop {
            if index_at == Some(*cursor) {
                return Ok(Stop::Terminator);
            }
            if cursor.saturating_add(8) > len {
                return Ok(Stop::Short);
            }
            let mut head = [0u8; 8];
            if !read_head(file, &mut pos, *cursor, &mut head)? {
                return Ok(Stop::Short);
            }
            let declared = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
            let btype = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
            if declared > MAX_BLOCK_BYTES {
                return Ok(Stop::Terminator);
            }
            let blen = declared as u64;
            let body_at = *cursor + 8;
            let next = body_at + blen;
            // THE case this module exists for: the scope has not written this
            // block yet. Leave it entirely alone.
            if next > len {
                return Ok(Stop::Short);
            }
            if blen == 0 {
                empty_run += 1;
                if empty_run > MAX_EMPTY_RUN {
                    return Ok(Stop::Terminator);
                }
            } else {
                empty_run = 0;
            }

            // A descriptor read earlier — possibly on an earlier poll — is
            // waiting for the bytes it named, and they are in this block.
            if pending.is_some() {
                let p = pending.take().expect("just tested");
                // A descriptor's data block follows it immediately and states
                // the same length. Anything else means the pairing is not what
                // it looked like, and the safe thing is to drop the descriptor
                // rather than fill a plane from the wrong block.
                if btype == TYPE_DATA && blen == p.run {
                    let n = p.run as usize;
                    if read_into(file, &mut pos, body_at, n, scratch)? {
                        if let Some(plane) =
                            place_chunk(building, frame_meta, *geometry, &p, &scratch[..n])
                        {
                            out.push(plane);
                        }
                    }
                }
                *cursor = next;
                // Hand back what is finished rather than reading to the end of
                // the acquisition. A live poll never reaches this — a few frames
                // arrive between polls — but pointed at a recording that is
                // already complete, one poll would otherwise walk every part and
                // return every plane at once: 25,000 planes of a quarter-megabyte
                // each for the thirteen-part reference recording, which is more
                // memory than the machine beside the microscope has. The cursor
                // is already past what is being returned, so the next poll simply
                // carries on.
                if out.len() >= MAX_PLANES_PER_POLL {
                    return Ok(Stop::Batch);
                }
                continue;
            }

            match btype {
                TYPE_DESCRIPTOR if (12..=MAX_DESCRIPTOR_BYTES).contains(&blen) => {
                    let n = blen as usize;
                    if !read_into(file, &mut pos, body_at, n, scratch)? {
                        return Ok(Stop::Short);
                    }
                    *pending = parse_descriptor(
                        &scratch[..n],
                        channels,
                        timepoint_offset,
                        part_max_timepoint,
                    );
                    *cursor = next;
                }
                TYPE_META if (1..=MAX_META_BYTES).contains(&blen) => {
                    let n = blen as usize;
                    if !read_into(file, &mut pos, body_at, n, scratch)? {
                        return Ok(Stop::Short);
                    }
                    let parsed =
                        meta::xml_of_block(&scratch[..n]).map(meta::parse_frame_properties);
                    if let Some(m) = parsed {
                        // Geometry is taken from the first document that states
                        // a usable one and then left alone: a recording does not
                        // change frame size part-way, and everything downstream
                        // — the FFT plan, the registrar, the reference stack —
                        // is built once from it.
                        if geometry.is_none() {
                            *geometry = geometry_of(&m);
                        }
                        // The document names its own frame, which is how it is
                        // attached to a timepoint rather than by position. When
                        // it does not, it belongs to the timepoint about to
                        // arrive, because the XML always precedes the pixels.
                        // `base:name` is the whole frame name, `t004_0_1`, not
                        // just its axis field — so it has to be split first.
                        // Feeding the whole string to `axis_number` returns
                        // `None` (the rest is not digits), which sends this down
                        // the fallback below; that reads correctly in the first
                        // part, where the numbering happens to start at 1 and
                        // run consecutively, and mis-pins the part offset at
                        // every rollover of a continuously-numbered recording.
                        let raw = m
                            .name
                            .as_deref()
                            .and_then(|n| n.split('_').next())
                            .and_then(axis_number)
                            .unwrap_or_else(|| (*part_max_timepoint).saturating_add(1));
                        let tp = session_timepoint(timepoint_offset, part_max_timepoint, raw);
                        frame_meta.insert(tp, m);
                    }
                    *cursor = next;
                }
                // Everything else is stepped over without being read: pixel
                // blocks of planes this reader is not collecting, the lookup
                // tables, the thumbnail, the empty timepoint markers.
                _ => *cursor = next,
            }
        }
    }

    /// Take up the next part of the acquisition, if there is one to take up.
    ///
    /// A part the scope has only just created is empty for a moment, and a part
    /// whose header is not yet written cannot be walked — neither is an error,
    /// and neither is the end of the acquisition. They simply mean "not yet",
    /// and the next poll asks again.
    fn roll_over(&mut self) -> io::Result<Option<PathBuf>> {
        let all = acquisition_parts(&self.parts[0]);
        let Some(next) = all.get(self.current + 1).cloned() else {
            return Ok(None);
        };
        let Ok(file) = File::open(&next) else {
            return Ok(None);
        };
        let mut head = [0u8; 16];
        let mut handle = &file;
        if handle.read_exact(&mut head).is_err() || head[..] != *MAGIC {
            return Ok(None);
        }
        let Ok(Some(first)) = find_first_block(&file, FIRST_BLOCK_PROBE) else {
            return Ok(None);
        };
        // Carry the session's numbering across the boundary. `timepoint_offset`
        // becomes the highest session timepoint reached so far, and the first
        // raw number the new part mentions turns that into the part's actual
        // offset — zero if the file keeps counting, the carry if it restarts.
        self.timepoint_offset = self
            .timepoint_offset
            .saturating_add(self.part_max_timepoint);
        self.part_max_timepoint = 0;
        self.parts.push(next.clone());
        self.current += 1;
        self.file = file;
        self.cursor = first;
        self.known_len = 0;
        // A descriptor left unpaired at the end of a part names bytes that will
        // never arrive; the next part's blocks are not its data.
        self.pending = None;
        Ok(Some(next))
    }

    /// Drop part-assembled planes and metadata that can no longer be used.
    fn prune(&mut self) {
        let newest = self
            .timepoint_offset
            .saturating_add(self.part_max_timepoint);
        let floor = newest.saturating_sub(KEEP_TIMEPOINTS);
        self.building.retain(|k, _| k.0 >= floor);
        self.frame_meta.retain(|k, _| *k >= floor);
    }
}

/// Where the block stream starts in `path`.
///
/// The real acquisition begins its blocks at `0x60` and FastTIFF's synthetic
/// fixtures at `0x50`, so this tries each candidate just past the header and
/// keeps the first from which `probe` consecutive blocks parse with plausible
/// lengths and types. Returning the wrong answer here misreads the whole file,
/// so it demands a run of agreeing blocks rather than one.
///
/// That is not caution for its own sake. On the reference file, `0x50` holds
/// `u32 3, u32 2` — which reads perfectly well as a three-byte block of type 2,
/// and a reader satisfied with one block would take it, land at `0x5b`, read the
/// `0xFFFFFFFF` there as a length and desynchronise from the whole file. The run
/// is what rejects it, because the second block never parses.
///
/// Two further rules earn their keep. A candidate is rejected the moment a
/// header is implausible, but merely **running out of file** is not a rejection:
/// a part that has only just been created has one block in it and nothing wrong
/// with it, and the best such candidate is the answer when no candidate can show
/// a full run. And a run must contain at least one **non-empty** block, because
/// a stretch of zeros parses as an unlimited run of zero-length type-0 blocks
/// and would otherwise beat the real answer simply by coming first.
pub fn find_first_block(file: &File, probe: usize) -> io::Result<Option<u64>> {
    let len = file.metadata()?.len();
    let want = probe.max(1);
    // The best candidate that ran out of file rather than failing, for a part
    // too new to have a full run in it yet.
    let mut partial: Option<(usize, u64)> = None;
    let mut at = FIRST_BLOCK_FROM;
    for _ in 0..FIRST_BLOCK_CANDIDATES {
        if at.saturating_add(8) > len {
            break;
        }
        let (count, substantial, ran_out) = probe_blocks(file, at, len, want)?;
        if count >= want && substantial {
            return Ok(Some(at));
        }
        if ran_out && substantial && partial.map(|(c, _)| count > c).unwrap_or(true) {
            partial = Some((count, at));
        }
        at += FIRST_BLOCK_STEP;
    }
    Ok(partial.map(|(_, at)| at))
}

/// Walk at most `want` blocks from `from`, reporting how many parsed, whether
/// any of them carried a payload, and whether the walk ended at the end of the
/// file rather than at something implausible.
fn probe_blocks(file: &File, from: u64, len: u64, want: usize) -> io::Result<(usize, bool, bool)> {
    let mut handle = file;
    let mut cur = from;
    let mut seen = 0usize;
    let mut substantial = false;
    while seen < want {
        if cur.saturating_add(8) > len {
            return Ok((seen, substantial, true));
        }
        if handle.seek(SeekFrom::Start(cur)).is_err() {
            return Ok((seen, substantial, true));
        }
        let mut head = [0u8; 8];
        if handle.read_exact(&mut head).is_err() {
            return Ok((seen, substantial, true));
        }
        let blen = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        let btype = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if blen > MAX_BLOCK_BYTES || btype > MAX_BLOCK_TYPE {
            return Ok((seen, substantial, false));
        }
        if cur.saturating_add(8).saturating_add(blen as u64) > len {
            return Ok((seen, substantial, true));
        }
        substantial |= blen > 0;
        cur += 8 + blen as u64;
        seen += 1;
    }
    Ok((seen, substantial, true))
}

/// Every part of the acquisition that exists **right now**, the named one first.
///
/// Continuations are `<stem>_00001` with and without a `.oir` extension — the
/// real acquisition writes them with none. Numbering stops at the first gap. A
/// part that does not exist yet is not an error: during a recording, it does not
/// exist until the scope rolls over to it.
///
/// Where this differs from FastTIFF's importer: being handed a file that is
/// itself a continuation — `<stem>_00005` — is not treated as the end of the
/// set. That importer stops there, correctly, because it reads a whole
/// acquisition and a part is not one. This reader follows a recording forward
/// from wherever it was pointed, and someone who starts it half an hour into a
/// session will point it at the part being written; refusing to roll over from
/// there would silently stop stabilising at the next boundary. Earlier parts are
/// still not read — they are in the past, and the named file always comes first.
pub fn acquisition_parts(path: &Path) -> Vec<PathBuf> {
    let mut parts = vec![path.to_path_buf()];
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem()) else {
        return parts;
    };
    let stem = stem.to_string_lossy();
    // Being handed `<base>_00005` means continuing from `<base>_00006`, not
    // looking for `<base>_00005_00006`.
    let (base, from) = match stem.rsplit_once('_') {
        Some((head, tail))
            if tail.len() == 5 && tail.bytes().all(|b| b.is_ascii_digit()) && !head.is_empty() =>
        {
            match tail.parse::<usize>() {
                Ok(n) => (head.to_string(), n.saturating_add(1)),
                Err(_) => (stem.to_string(), 1),
            }
        }
        _ => (stem.to_string(), 1),
    };
    for n in from..=MAX_PARTS {
        let named = format!("{base}_{n:05}");
        let next = [dir.join(&named), dir.join(format!("{named}.oir"))]
            .into_iter()
            .find(|p| p.is_file());
        match next {
            Some(p) => parts.push(p),
            None => break,
        }
    }
    parts
}

/// The `(timepoint, channel_uid)` a block name belongs to, or `None` if the name
/// is not an image plane.
///
/// Names are `<axis><n>_<a>_<b>_<uid>_<chunk>`: `t001_0_1_22601615-…-477_0`. The
/// axis is `t` in a timelapse and `z` in a z-stack — a reader that only knew `t`
/// would find no planes at all in a z-stack, which is how the reference stack is
/// read. The **UID is the channel**: two channels of one timepoint differ only
/// there. The **trailing chunk index is not part of the key** — it is what makes
/// a plane several blocks, so it is what must be grouped over.
///
/// `REF_LSM0_<uid>_<chunk>` is the reference snapshot, not a frame, and must
/// return `None`.
///
/// For a z-stack the number returned is the slice, not a time. Nothing
/// downstream needs to know which: it is the axis the planes of one image are
/// gathered by, and a stack read by [`crate::cv::zstack`] wants its slices in
/// exactly the order a timelapse wants its frames.
///
/// A name with no non-numeric field at all — `t001_0_1`, which is what the
/// metadata document calls the frame — yields an empty UID rather than `None`.
/// Every descriptor in every file seen carries one, so this is a shape no
/// recording has; answering with a single unnamed channel keeps such a file
/// readable instead of reporting it as carrying no images.
pub fn plane_key(name: &str) -> Option<(u64, String)> {
    let mut fields = name.split('_');
    let n = axis_number(fields.next()?)?;
    // The first field that is not a number is the UID. Everything after it is
    // the chunk index, which is not part of the plane's identity.
    for field in fields {
        if field.parse::<u64>().is_err() {
            return Some((n, field.to_string()));
        }
    }
    Some((n, String::new()))
}

/// The number after a leading `t` or `z`, or `None` for anything else.
///
/// The digits are parsed rather than counted: the reference acquisition runs
/// `t999`, `t1000`, … `t12561`, so a fixed three-digit read would fold `t1000`
/// onto timepoint 100.
fn axis_number(field: &str) -> Option<u64> {
    let mut chars = field.chars();
    match chars.next()? {
        't' | 'z' => {}
        _ => return None,
    }
    let digits = field.get(1..)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// This part's raw axis number turned into the session's numbering.
///
/// Pinned once per part, on the first number that part mentions, to whatever
/// makes that frame one past the highest the session has already reached. See
/// the module docs: the reference acquisition numbers its parts continuously and
/// an unconditional offset would double the count, while a restarting file needs
/// the full carry. This is the expression that is right for both.
fn session_timepoint(offset: &mut u64, part_max: &mut u64, raw: u64) -> u64 {
    if *part_max == 0 {
        // `offset` holds the highest session timepoint reached so far.
        *offset = offset.saturating_add(1).saturating_sub(raw);
    }
    // Clamped to one so that a hypothetical `t000` cannot leave `part_max` at
    // zero and re-pin the offset on every block of the part.
    *part_max = (*part_max).max(raw).max(1);
    offset.saturating_add(raw)
}

/// Read a descriptor payload and resolve it to the chunk it names, if that chunk
/// is one this reader collects.
///
/// `u32 offset-within-plane, u32 run length, u32 name length, name`. Every field
/// is checked against a bound before it is believed: a half-written descriptor
/// can claim any of them, and the run length in particular becomes the size of a
/// read.
fn parse_descriptor(
    body: &[u8],
    channels: &mut Vec<String>,
    offset: &mut u64,
    part_max: &mut u64,
) -> Option<PendingDescriptor> {
    let at = u32_at(body, 0)? as usize;
    let run = u32_at(body, 4)?;
    let nlen = u32_at(body, 8)? as usize;
    let name = std::str::from_utf8(body.get(12..12usize.checked_add(nlen)?)?).ok()?;
    let (raw, uid) = plane_key(name)?;
    // A run of nothing, or one that starts past the largest plane this reader
    // will hold, is refused on the bound rather than turned into an allocation.
    if run == 0 || run > MAX_BLOCK_BYTES || at >= MAX_PLANE_BYTES {
        return None;
    }
    let timepoint = session_timepoint(offset, part_max, raw);
    let channel = channel_index(channels, &uid)?;
    Some(PendingDescriptor {
        at,
        run: run as u64,
        timepoint,
        channel,
        uid,
    })
}

/// The index of `uid` among the channels, assigning it the next one if it is new.
///
/// First-seen order, and stable for the session: `input.channel` in the config is
/// a number, so a channel that changed index part-way through a recording would
/// silently change which one is being measured.
fn channel_index(channels: &mut Vec<String>, uid: &str) -> Option<usize> {
    if let Some(i) = channels.iter().position(|c| c == uid) {
        return Some(i);
    }
    if channels.len() >= MAX_CHANNELS {
        return None;
    }
    channels.push(uid.to_string());
    Some(channels.len() - 1)
}

/// Place one chunk into the plane it belongs to, and return the plane if that
/// completed it.
fn place_chunk(
    building: &mut std::collections::BTreeMap<(u64, usize), Building>,
    frame_meta: &std::collections::BTreeMap<u64, FrameMeta>,
    geometry: Option<Geometry>,
    p: &PendingDescriptor,
    src: &[u8],
) -> Option<Plane> {
    let plane_bytes = geometry.map(|g| g.plane_bytes());
    let key = (p.timepoint, p.channel);
    if !building.contains_key(&key) && building.len() >= MAX_BUILDING {
        return None;
    }
    let complete = {
        let entry = building.entry(key).or_insert_with(|| Building {
            // Sized to the whole plane the moment the geometry is known, so a
            // plane is allocated once rather than regrown per chunk.
            bytes: match plane_bytes {
                Some(n) => vec![0u8; n],
                None => Vec::new(),
            },
            covered: 0,
            uid: p.uid.clone(),
        });
        place(entry, plane_bytes, p.at, src)
    };
    if !complete {
        return None;
    }
    let done = building.remove(&key)?;
    // `complete` is only ever true when the geometry is known.
    let geom = geometry?;
    Some(Plane {
        timepoint: p.timepoint,
        channel: p.channel,
        channel_uid: done.uid,
        // A timepoint whose metadata block was unreadable still yields its
        // pixels; the fields of a default `FrameMeta` are all `None`, and every
        // consumer of them already treats absence as "cannot say".
        meta: frame_meta.get(&p.timepoint).cloned().unwrap_or_default(),
        samples: samples_of(&done.bytes, geom),
    })
}

/// Copy one run into a plane, returning whether the plane is now complete.
///
/// A run that reaches past the end of the plane writes only the part that is
/// inside it — which is what a descriptor claiming more than the frame holds
/// means, and it must not grow the buffer past the frame.
fn place(b: &mut Building, plane_bytes: Option<usize>, at: usize, src: &[u8]) -> bool {
    let cap = plane_bytes.unwrap_or(MAX_PLANE_BYTES);
    if at >= cap {
        return false;
    }
    let n = src.len().min(cap - at);
    let end = at + n;
    if b.bytes.len() < end {
        // Without a geometry there is nothing to size to, so the buffer grows to
        // what has arrived — bounded by `cap`, which is `MAX_PLANE_BYTES`.
        b.bytes.resize(plane_bytes.unwrap_or(end).max(end), 0);
    }
    b.bytes[at..end].copy_from_slice(&src[..n]);
    b.covered = b.covered.saturating_add(n).min(cap);
    plane_bytes.is_some_and(|want| b.covered >= want)
}

/// A plane's bytes as samples, exactly `width * height` of them.
///
/// Samples are little-endian in the file, which is also the only byte order this
/// ever runs on; the conversion is written out rather than transmuted so that it
/// is correct anywhere and so that nothing depends on the buffer's alignment.
fn samples_of(bytes: &[u8], geom: Geometry) -> Vec<u16> {
    let n = geom.width.saturating_mul(geom.height);
    let mut out = Vec::with_capacity(n);
    match geom.depth {
        1 => out.extend(bytes.iter().take(n).map(|&b| b as u16)),
        _ => out.extend(
            bytes
                .chunks_exact(2)
                .take(n)
                .map(|c| u16::from_le_bytes([c[0], c[1]])),
        ),
    }
    // A plane short of its declared size cannot normally reach here — that is
    // what completeness means — but padding rather than returning something of
    // the wrong length keeps every `width * height` index downstream in bounds.
    out.resize(n, 0);
    out
}

/// The frame geometry a `frameProperties` document states, if it states a usable
/// one.
///
/// Checked rather than trusted, because these three numbers are multiplied
/// together to size every plane buffer in the session.
fn geometry_of(m: &FrameMeta) -> Option<Geometry> {
    const MAX_SIDE: usize = 1 << 16;
    let (w, h, d) = (m.width?, m.height?, m.depth?);
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || !(1..=2).contains(&d) {
        return None;
    }
    let bytes = w.checked_mul(h)?.checked_mul(d)?;
    (bytes <= MAX_PLANE_BYTES).then_some(Geometry {
        width: w,
        height: h,
        depth: d,
    })
}

/// Turn a finished plane into the [`Frame`] the rest of the program measures:
/// pick or sum channels, crop to the ROI, downsample, convert to `f32`.
///
/// Cropping and downsampling happen here, once, before anything measures — and
/// downsampling is a plain stride, not an average, because averaging is a
/// low-pass filter and would flatten the very high-frequency content the focus
/// metric reads.
///
/// `None` means "nothing measurable here", and the caller skips the timepoint.
/// The case worth knowing about is `input.channel: 2` on a two-channel recording:
/// every timepoint returns `None` and the program follows the file without ever
/// measuring anything. Falling back to channel 0 would be worse — it would
/// silently measure a channel nobody asked for — but the symptom is a program
/// that looks like it is working and is not.
pub fn to_frame(planes: &[Plane], geom: Geometry, cfg: &Config) -> Option<Frame> {
    if planes.is_empty() || geom.width == 0 || geom.height == 0 {
        return None;
    }
    let (gw, gh) = (geom.width, geom.height);
    let [rx, ry, rw, rh] = cfg.input.roi.unwrap_or([0, 0, gw, gh]);
    // Clipped to the frame rather than refused: an ROI a few pixels over the
    // edge is a typo in a config file, not a reason not to stabilise.
    let x0 = rx.min(gw);
    let y0 = ry.min(gh);
    let x1 = x0.saturating_add(rw).min(gw);
    let y1 = y0.saturating_add(rh).min(gh);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let stride = cfg.input.downsample.max(1);
    let width = (x1 - x0).div_ceil(stride);
    let height = (y1 - y0).div_ceil(stride);
    if width == 0 || height == 0 {
        return None;
    }

    let want = gw.checked_mul(gh)?;
    let picked: Vec<&Plane> = match cfg.input.channel.index() {
        Some(i) => planes.iter().filter(|p| p.channel == i).collect(),
        None => planes.iter().collect(),
    };
    // A plane shorter than the geometry would mean indexing off the end of it.
    if picked.is_empty() || picked.iter().any(|p| p.samples.len() < want) {
        return None;
    }

    let mut data = Vec::with_capacity(width * height);
    for y in (y0..y1).step_by(stride) {
        let row = y * gw;
        for x in (x0..x1).step_by(stride) {
            let mut v = 0f32;
            for p in &picked {
                v += p.samples[row + x] as f32;
            }
            data.push(v);
        }
    }

    // The planes of one timepoint all carry the same metadata; taking it from
    // the lowest channel makes the choice deterministic when they somehow differ.
    let lead = picked.iter().min_by_key(|p| (p.timepoint, p.channel))?;
    Some(Frame {
        width,
        height,
        data,
        index: lead.timepoint,
        meta: lead.meta.clone(),
    })
}

// -------------------------------------------------------------------- reading

/// Read a block header, seeking only if the handle is not already there.
fn read_head(file: &mut File, pos: &mut u64, at: u64, head: &mut [u8; 8]) -> io::Result<bool> {
    if *pos != at {
        file.seek(SeekFrom::Start(at))?;
        *pos = at;
    }
    match file.read_exact(head) {
        Ok(()) => {
            *pos += 8;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            // The length said these bytes were there and the read says they are
            // not. Nothing has been consumed, so this is the same case as a
            // block that has not been written: leave it and look again.
            *pos = u64::MAX;
            Ok(false)
        }
        Err(e) => {
            *pos = u64::MAX;
            Err(e)
        }
    }
}

/// Read `len` bytes at `at` into `buf`, which is left at least `len` long.
///
/// The buffer is grown but never re-zeroed: it is reused for every block of the
/// session, and zeroing half a megabyte before overwriting all of it would be a
/// second pass over every byte of a gigabyte recording.
fn read_into(
    file: &mut File,
    pos: &mut u64,
    at: u64,
    len: usize,
    buf: &mut Vec<u8>,
) -> io::Result<bool> {
    if buf.len() < len {
        buf.resize(len, 0);
    }
    if *pos != at {
        file.seek(SeekFrom::Start(at))?;
        *pos = at;
    }
    match file.read_exact(&mut buf[..len]) {
        Ok(()) => {
            *pos += len as u64;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            *pos = u64::MAX;
            Ok(false)
        }
        Err(e) => {
            *pos = u64::MAX;
            Err(e)
        }
    }
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;

pub mod meta;
