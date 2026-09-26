//! Tests for the tail-following reader.
//!
//! The fixtures are synthetic, built to the layout the module documents. The
//! format was determined from a real acquisition and the walk was checked against
//! that file's own block index, but the file is unpublished research data and does
//! not belong in a repository — so what is committed is the *structure*, with
//! pixel values chosen so that a mis-assembled plane is obviously wrong rather
//! than plausible. A test against the real recording is at the bottom, `#[ignore]`d
//! because the file only exists on the acquisition machine.
//!
//! [`grows_a_block_at_a_time`] is the most important test here. Everything else in
//! this module could be got right by a reader that only ever sees finished files;
//! that one is the reason the module exists.

use super::*;
use std::io::Write;

// ------------------------------------------------------------- fixtures

/// A scratch directory of its own, so tests can run at the same time, removed
/// when the test ends.
///
/// A guard rather than a `remove_dir_all` at the end of each test: several of
/// these tests write a fixture of a few hundred kilobytes, and a test that fails
/// never reaches its own cleanup line — which is how a temp directory per failed
/// run per process id accumulates.
struct Scratch(PathBuf);

impl Scratch {
    fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!("oir-tests-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    Scratch(dir)
}

/// Builds an OIR the way the acquisition software lays one out.
struct Builder {
    body: Vec<u8>,
    offsets: Vec<u64>,
}

impl Builder {
    /// `first_block` is where the block stream starts: `0x60` as the real
    /// acquisition does, or `0x50` as FastTIFF's own fixtures do.
    fn new(first_block: u64) -> Builder {
        let mut body = Vec::new();
        body.extend(MAGIC);
        body.resize(first_block as usize, 0);
        if first_block >= 0x60 {
            // What the real file has between `FLUOVIEW` and its first block: a
            // `u32 3, u32 2` that reads as a 3-byte block of type 2, followed by
            // eight bytes of `0xFF`. A reader that accepted the first offset from
            // which one block parses would take 0x50 and then desynchronise, so
            // the fixture has to contain the trap.
            body[0x48..0x50].copy_from_slice(b"FLUOVIEW");
            body[0x50..0x54].copy_from_slice(&3u32.to_le_bytes());
            body[0x54..0x58].copy_from_slice(&2u32.to_le_bytes());
            body[0x58..0x60].copy_from_slice(&u64::MAX.to_le_bytes());
        }
        Builder {
            body,
            offsets: Vec::new(),
        }
    }

    fn block(&mut self, ty: u32, payload: &[u8]) {
        self.offsets.push(self.body.len() as u64);
        self.body.extend((payload.len() as u32).to_le_bytes());
        self.body.extend(ty.to_le_bytes());
        self.body.extend(payload);
    }

    /// A per-frame metadata block, with the 40-byte binary prefix the real one has.
    fn meta(&mut self, name: &str, w: usize, h: usize, z: f64) {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"ASCII\"?>\
             <lsmframe:frameProperties>\
             <base:name>{name}</base:name>\
             <base:creationDateTime>2025-10-07T21:58:59.990-04:00</base:creationDateTime>\
             <base:width>{w}</base:width><base:height>{h}</base:height>\
             <base:depth>2</base:depth><base:bitCounts>10</base:bitCounts>\
             <commonframe:axisType>TIMELAPSE</commonframe:axisType>\
             <lsmframe:zPosition>{z}</lsmframe:zPosition>\
             </lsmframe:frameProperties>"
        );
        let mut payload = vec![0u8; 40];
        payload.extend(xml.as_bytes());
        self.block(TYPE_META, &payload);
    }

    /// A descriptor naming `name`, then the data block it describes.
    fn chunk(&mut self, name: &str, at: u32, data: &[u8]) {
        let mut d = Vec::new();
        d.extend(at.to_le_bytes());
        d.extend((data.len() as u32).to_le_bytes());
        d.extend((name.len() as u32).to_le_bytes());
        d.extend(name.as_bytes());
        self.block(TYPE_DESCRIPTOR, &d);
        self.block(TYPE_DATA, data);
    }

    /// The bytes, with a valid trailing block index and the header patched.
    fn finish(mut self) -> Vec<u8> {
        let index_at = self.body.len() as u64;
        self.body.extend(0xFFFF_FFFFu32.to_le_bytes());
        self.body.extend(96u32.to_le_bytes());
        self.body.extend(0u32.to_le_bytes());
        for o in &self.offsets {
            self.body.extend(o.to_le_bytes());
        }
        let total = self.body.len() as u64;
        self.body[0x20..0x28].copy_from_slice(&total.to_le_bytes());
        self.body[0x28..0x30].copy_from_slice(&index_at.to_le_bytes());
        self.body
    }

    /// The bytes with **no** index, which is the state of a file being recorded.
    fn finish_unindexed(self) -> Vec<u8> {
        self.body
    }
}

/// `n` samples of plane `p`, as the little-endian `u16` bytes a data block holds.
fn plane_bytes(p: usize, n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n * 2);
    for i in 0..n {
        v.extend(((p * 1000 + i) as u16).to_le_bytes());
    }
    v
}

const UID_A: &str = "22601615-bb45-4964-b1e6-0115f6c0d477";
const UID_B: &str = "169fb3b9-96db-4305-85ff-b4dea2e5d13b";

/// One timepoint of a two-channel 4x4 recording, delivered the awkward way the
/// real file does it: the metadata first, then the channels interleaved, and the
/// **second** chunk of each plane before the first.
fn timepoint(b: &mut Builder, axis: char, n: u64, plane_a: usize, plane_b: usize, z: f64) {
    b.meta(&format!("{axis}{n:03}_0_1"), 4, 4, z);
    let a = plane_bytes(plane_a, 16);
    let c = plane_bytes(plane_b, 16);
    // 32 bytes per plane, split 20 + 12 — unequal, as the real 485376 + 38912 is.
    for (uid, bytes) in [(UID_A, &a), (UID_B, &c)] {
        b.chunk(&format!("{axis}{n:03}_0_1_{uid}_1"), 20, &bytes[20..]);
    }
    for (uid, bytes) in [(UID_A, &a), (UID_B, &c)] {
        b.chunk(&format!("{axis}{n:03}_0_1_{uid}_0"), 0, &bytes[..20]);
    }
    b.block(5, &[]);
}

/// A finished two-channel timelapse of `frames` timepoints, starting at `from`.
fn timelapse(first_block: u64, from: u64, frames: u64, indexed: bool) -> Vec<u8> {
    let mut b = Builder::new(first_block);
    // The reference snapshot, which must not become a frame.
    b.chunk(&format!("REF_LSM0_{UID_B}_0"), 0, &[0xEE; 32]);
    for k in 0..frames {
        let n = from + k;
        timepoint(
            &mut b,
            't',
            n,
            (2 * n) as usize,
            (2 * n + 1) as usize,
            9741.19,
        );
    }
    if indexed {
        b.finish()
    } else {
        b.finish_unindexed()
    }
}

fn cfg_for_tests() -> Config {
    let mut c = Config::default();
    c.input.roi = None;
    c.input.downsample = 1;
    c
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write fixture");
}

// --------------------------------------------------------------- plane_key

#[test]
fn plane_key_reads_the_axis_the_channel_and_not_the_chunk() {
    // Timepoint and channel, with the trailing chunk index excluded — it is what
    // makes a plane several blocks, so it is what must be grouped over.
    let (t, uid) = plane_key(&format!("t001_0_1_{UID_A}_0")).expect("a timelapse plane");
    assert_eq!(t, 1);
    assert_eq!(uid, UID_A);
    let (t2, uid2) = plane_key(&format!("t001_0_1_{UID_A}_7")).expect("another chunk of it");
    assert_eq!((t2, &uid2), (t, &uid), "chunks of one plane share a key");

    // Two channels of one timepoint differ only in the UID. Merging them would
    // overlay two images and produce a picture that is wrong without looking it.
    let (_, other) = plane_key(&format!("t001_0_1_{UID_B}_0")).expect("the other channel");
    assert_ne!(other, uid);
}

#[test]
fn plane_key_reads_the_z_axis_too() {
    // A z-stack names its planes `z001_...`. A reader that only knew `t` would
    // find nothing at all in one — which is how the reference stack is read.
    let (z, uid) = plane_key(&format!("z033_0_1_{UID_A}_2")).expect("a z-stack plane");
    assert_eq!(z, 33);
    assert_eq!(uid, UID_A);
}

#[test]
fn plane_key_numbers_are_not_fixed_width() {
    // The reference acquisition runs to `t12561`. Taking three characters would
    // fold `t1000` onto timepoint 100 and interleave it with the hundreds.
    assert_eq!(plane_key(&format!("t999_0_1_{UID_A}_0")).unwrap().0, 999);
    assert_eq!(plane_key(&format!("t1000_0_1_{UID_A}_0")).unwrap().0, 1000);
    assert_eq!(
        plane_key(&format!("t12561_0_1_{UID_A}_0")).unwrap().0,
        12561
    );
}

#[test]
fn plane_key_refuses_what_is_not_a_frame() {
    // The reference snapshot and the thumbnails. 18 chunks of one arrive before
    // the timelapse does, and a reader that took them would report an extra frame
    // of something that is not part of the recording.
    assert_eq!(plane_key(&format!("REF_LSM0_{UID_B}_0")), None);
    assert_eq!(plane_key("t_0_1_uid_0"), None, "no number after the axis");
    assert_eq!(plane_key("x001_0_1_uid_0"), None, "not a known axis");
    assert_eq!(plane_key("txxx_0_1_uid_0"), None, "not digits");
    assert_eq!(plane_key(""), None);
}

// -------------------------------------------------------- find_first_block

#[test]
fn finds_the_first_block_at_either_offset() {
    let dir = scratch("first-block");
    for (first, label) in [(0x50u64, "fixture layout"), (0x60, "real layout")] {
        let p = dir.join(format!("at-{first:x}.oir"));
        write(&p, &timelapse(first, 1, 1, true));
        let f = File::open(&p).unwrap();
        assert_eq!(
            find_first_block(&f, FIRST_BLOCK_PROBE).unwrap(),
            Some(first),
            "{label}: block stream should be found at {first:#x}"
        );
    }
}

#[test]
fn a_run_of_blocks_is_required_not_one() {
    // The real file has a `u32 3, u32 2` at 0x50 that parses as one valid block
    // and then desynchronises. Accepting the first offset that yields a single
    // block would misread the entire file, so this is the test that pins the
    // "demand a run" rule down.
    let dir = scratch("run-required");
    let p = dir.join("trap.oir");
    write(&p, &timelapse(0x60, 1, 2, true));
    let f = File::open(&p).unwrap();
    assert_eq!(
        find_first_block(&f, 1).unwrap(),
        Some(0x50),
        "one block is the trap"
    );
    assert_eq!(
        find_first_block(&f, FIRST_BLOCK_PROBE).unwrap(),
        Some(0x60),
        "a run of blocks finds the real start"
    );
}

#[test]
fn refuses_a_file_that_is_not_an_oir() {
    let dir = scratch("not-oir");
    let p = dir.join("nope.oir");
    write(
        &p,
        b"this is not a microscope file at all, whatever it is called",
    );
    assert!(LiveReader::open(&p, &cfg_for_tests()).is_err());
}

// ----------------------------------------------------- acquisition_parts

#[test]
fn parts_are_found_with_and_without_the_extension() {
    let dir = scratch("parts");
    let base = dir.join("rec.oir");
    write(&base, b"OLYMPUSRAWFORMAT");
    // The real acquisition writes continuations with no extension; other
    // installations write `.oir`. Both have to be found, or a reader opens a
    // quarter of a timelapse and calls it the whole thing.
    write(&dir.join("rec_00001"), b"OLYMPUSRAWFORMAT");
    write(&dir.join("rec_00002.oir"), b"OLYMPUSRAWFORMAT");
    let parts = acquisition_parts(&base);
    assert_eq!(parts.len(), 3, "got {parts:?}");
    assert_eq!(parts[0], base);
    assert!(parts[1].ends_with("rec_00001"));
    assert!(parts[2].ends_with("rec_00002.oir"));
}

#[test]
fn parts_stop_at_the_first_gap() {
    // A missing part means the set is incomplete. Reading across the hole would
    // join timepoints that are not adjacent.
    let dir = scratch("gap");
    let base = dir.join("rec.oir");
    write(&base, b"OLYMPUSRAWFORMAT");
    write(&dir.join("rec_00001"), b"OLYMPUSRAWFORMAT");
    write(&dir.join("rec_00003"), b"OLYMPUSRAWFORMAT");
    assert_eq!(acquisition_parts(&base).len(), 2);
}

#[test]
fn being_handed_a_part_continues_from_the_next_one() {
    // FastTIFF's importer returns only the named file here, on the grounds that
    // following on from `rec_00001` would mean looking for `rec_00001_00001`.
    // That is right for converting a finished acquisition and wrong for this
    // program: a user who drops the part that is *currently being written* onto
    // it wants the session followed onwards, and stopping at the end of that part
    // would silently end the session at the next rollover.
    //
    // So `rec_00001` continues with `rec_00002` — and, importantly, does not go
    // looking for `rec_00001_00001`.
    let dir = scratch("part-continues");
    write(&dir.join("rec.oir"), b"OLYMPUSRAWFORMAT");
    let part = dir.join("rec_00001");
    write(&part, b"OLYMPUSRAWFORMAT");
    write(&dir.join("rec_00002"), b"OLYMPUSRAWFORMAT");
    write(&dir.join("rec_00003"), b"OLYMPUSRAWFORMAT");
    // A file that would be found only by the wrong rule.
    write(&dir.join("rec_00001_00001"), b"OLYMPUSRAWFORMAT");

    let parts = acquisition_parts(&part);
    assert_eq!(parts.len(), 3, "itself and the two after it: {parts:?}");
    assert_eq!(parts[0], part);
    assert!(parts[1].ends_with("rec_00002"));
    assert!(parts[2].ends_with("rec_00003"));
    assert!(
        !parts.iter().any(|p| p.ends_with("rec_00001_00001")),
        "a part must not be treated as the base of a new set: {parts:?}"
    );
}

// ------------------------------------------------------ reading a whole file

/// Read everything a finished file has, in one poll.
fn read_all(path: &Path, cfg: &Config) -> (Vec<Plane>, Option<Geometry>, usize) {
    let mut r = LiveReader::open(path, cfg).expect("open");
    let p = r.poll().expect("poll");
    (p.planes, r.geometry(), r.channels().len())
}

#[test]
fn reassembles_scattered_out_of_order_chunks() {
    let dir = scratch("reassemble");
    let p = dir.join("rec.oir");
    write(&p, &timelapse(0x60, 1, 3, true));
    let (planes, geom, channels) = read_all(&p, &cfg_for_tests());

    assert_eq!(
        geom,
        Some(Geometry {
            width: 4,
            height: 4,
            depth: 2
        })
    );
    assert_eq!(channels, 2, "two channel UIDs");
    assert_eq!(planes.len(), 6, "3 timepoints x 2 channels");

    // In timepoint then channel order.
    let order: Vec<(u64, usize)> = planes.iter().map(|p| (p.timepoint, p.channel)).collect();
    assert_eq!(order, vec![(1, 0), (1, 1), (2, 0), (2, 1), (3, 0), (3, 1)]);

    // And every sample in its right place. The chunks were emitted second-half
    // first, so a reader that concatenated them in file order gets this wrong
    // while still producing a plausible-looking plane.
    for pl in &planes {
        let expect = 2 * pl.timepoint as usize + pl.channel;
        let want: Vec<u16> = (0..16).map(|i| (expect * 1000 + i) as u16).collect();
        assert_eq!(
            pl.samples, want,
            "timepoint {} channel {}",
            pl.timepoint, pl.channel
        );
    }
}

#[test]
fn the_reference_snapshot_is_not_a_frame() {
    let dir = scratch("ref-snapshot");
    let p = dir.join("rec.oir");
    write(&p, &timelapse(0x60, 1, 1, true));
    let (planes, _, channels) = read_all(&p, &cfg_for_tests());
    assert_eq!(
        planes.len(),
        2,
        "one timepoint, two channels, and no REF plane"
    );
    assert_eq!(channels, 2, "REF's UID must not count as a channel");
    assert!(planes.iter().all(|pl| pl.samples[0] != 0xEEEE));
}

#[test]
fn reads_a_z_stack() {
    // The reference-stack mode reads its stack with this reader, so the `z` axis
    // is not an optional extra.
    let dir = scratch("zstack");
    let p = dir.join("stack.oir");
    let mut b = Builder::new(0x60);
    for n in 1..=4u64 {
        timepoint(
            &mut b,
            'z',
            n,
            (2 * n) as usize,
            (2 * n + 1) as usize,
            9700.0 + n as f64,
        );
    }
    write(&p, &b.finish());
    let (planes, geom, _) = read_all(&p, &cfg_for_tests());
    assert_eq!(geom.map(|g| g.width), Some(4));
    assert_eq!(planes.len(), 8, "4 slices x 2 channels");
    assert_eq!(planes[0].timepoint, 1, "z001 is slice 1");
    assert_eq!(planes[6].timepoint, 4);
}

#[test]
fn reads_a_file_with_no_index_at_all() {
    // The state of every file that is still being recorded.
    let dir = scratch("no-index");
    let p = dir.join("rec.oir");
    write(&p, &timelapse(0x60, 1, 3, false));
    let (planes, _, _) = read_all(&p, &cfg_for_tests());
    assert_eq!(planes.len(), 6, "the walk does not need an index");
}

#[test]
fn a_lying_index_offset_does_not_lose_the_frames() {
    // Mid-recording the header's index offset is whatever was last flushed there.
    let dir = scratch("bad-index");
    for (label, patch) in [
        ("zero", 0u64),
        ("past the end", u64::MAX),
        ("inside the header", 0x10),
    ] {
        let p = dir.join(format!("rec-{}.oir", label.replace(' ', "-")));
        let mut bytes = timelapse(0x60, 1, 2, true);
        bytes[0x28..0x30].copy_from_slice(&patch.to_le_bytes());
        write(&p, &bytes);
        let (planes, _, _) = read_all(&p, &cfg_for_tests());
        assert_eq!(
            planes.len(),
            4,
            "index offset {label}: frames should still be read"
        );
    }
}

// ------------------------------------------------- the live case, the point

#[test]
fn grows_a_block_at_a_time() {
    // THE test. A reader that only ever sees finished files passes everything
    // above and fails this, and this is the case the module exists for.
    //
    // The file is revealed one byte at a time across the whole of the first
    // timepoint. At no point may a partial block be read as pixels, and by the
    // time the last byte of the timepoint has arrived the frames must be there.
    let dir = scratch("growing");
    let p = dir.join("rec.oir");
    let full = timelapse(0x60, 1, 2, false);

    // Enough to open: the header and a run of blocks for the start to be found.
    let head = 0x60 + 200;
    write(&p, &full[..head]);
    let mut r = LiveReader::open(&p, &cfg_for_tests()).expect("opens on a partial file");

    let mut got: Vec<Plane> = Vec::new();
    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    for at in head..full.len() {
        f.write_all(&full[at..at + 1]).unwrap();
        f.flush().unwrap();
        let poll = r.poll().expect("poll on a growing file");
        assert!(
            !poll.finished,
            "a file that just grew is not finished (at byte {at})"
        );
        got.extend(poll.planes);
    }

    assert_eq!(
        got.len(),
        4,
        "both timepoints, both channels, exactly once each"
    );
    let order: Vec<(u64, usize)> = got.iter().map(|p| (p.timepoint, p.channel)).collect();
    assert_eq!(order, vec![(1, 0), (1, 1), (2, 0), (2, 1)]);
    for pl in &got {
        let expect = 2 * pl.timepoint as usize + pl.channel;
        let want: Vec<u16> = (0..16).map(|i| (expect * 1000 + i) as u16).collect();
        assert_eq!(pl.samples, want, "byte-at-a-time gave a wrong plane");
    }
}

#[test]
fn a_truncated_trailing_block_is_left_alone() {
    // The narrow version of the above: a descriptor has arrived and the data
    // block it names is half there. Nothing may be emitted, and nothing may be
    // consumed, so that the rest of it is still read when it turns up.
    let dir = scratch("truncated");
    let p = dir.join("rec.oir");
    let full = timelapse(0x60, 1, 1, false);
    write(&p, &full[..full.len() - 9]);
    let mut r = LiveReader::open(&p, &cfg_for_tests()).expect("open");
    let first = r.poll().expect("poll");
    assert!(
        first.planes.len() < 2,
        "the last plane is incomplete and must not be emitted"
    );

    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(&full[full.len() - 9..]).unwrap();
    drop(f);
    let second = r.poll().expect("poll again");
    let total = first.planes.len() + second.planes.len();
    assert_eq!(total, 2, "once the rest arrives, both planes are there");
}

#[test]
fn nothing_is_emitted_twice() {
    // The cursor only ever moves forward, so polling a file that has not changed
    // must produce nothing at all.
    let dir = scratch("no-repeats");
    let p = dir.join("rec.oir");
    write(&p, &timelapse(0x60, 1, 2, true));
    let mut r = LiveReader::open(&p, &cfg_for_tests()).expect("open");
    assert_eq!(r.poll().unwrap().planes.len(), 4);
    for _ in 0..3 {
        assert!(
            r.poll().unwrap().planes.is_empty(),
            "a re-poll re-read the file"
        );
    }
}

#[test]
fn one_poll_returns_a_bounded_batch() {
    // Pointed at a recording that is already on disk, an unbounded poll would
    // return every plane of every part at once — 25,000 quarter-megabyte planes
    // for the reference recording. So a poll stops at its limit, says `more`, and
    // the next one carries on from where it was.
    let dir = scratch("batched");
    let p = dir.join("rec.oir");
    let frames = (MAX_PLANES_PER_POLL / 2) as u64 + 20;
    write(&p, &timelapse(0x60, 1, frames, true));

    let mut r = LiveReader::open(&p, &cfg_for_tests()).expect("open");
    let first = r.poll().expect("poll");
    assert!(
        first.planes.len() <= MAX_PLANES_PER_POLL,
        "a poll returned {} planes, past its own limit",
        first.planes.len()
    );
    assert!(first.more, "there is more to read, so `more` should say so");
    assert!(
        !first.finished,
        "a poll holding the rest of the file must never report the acquisition finished"
    );

    // Polling until it stops asking for more must yield every plane exactly once
    // and in order — nothing dropped at a batch boundary, nothing repeated.
    let mut all = first.planes;
    for _ in 0..64 {
        let next = r.poll().expect("poll");
        let more = next.more;
        all.extend(next.planes);
        if !more {
            break;
        }
    }
    assert_eq!(all.len(), frames as usize * 2, "every plane, exactly once");
    let tps: Vec<u64> = all.iter().map(|p| p.timepoint).collect();
    let mut sorted = tps.clone();
    sorted.sort_unstable();
    assert_eq!(tps, sorted, "batches came back out of order");
}

#[test]
fn finished_only_after_the_idle_timeout() {
    let dir = scratch("finished");
    let p = dir.join("rec.oir");
    write(&p, &timelapse(0x60, 1, 1, true));
    let mut cfg = cfg_for_tests();
    cfg.input.idle_timeout_s = 1; // clamped to a second, which is the floor
    let mut r = LiveReader::open(&p, &cfg).expect("open");
    assert!(!r.poll().unwrap().finished, "not finished immediately");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(
        r.poll().unwrap().finished,
        "finished once nothing has grown"
    );
}

// ------------------------------------------------------------- rollover

#[test]
fn rolls_over_and_keeps_continuous_numbering() {
    // The reference acquisition numbers its parts continuously: part 0 holds
    // t001..t1018 and part 1 begins at t1019. An offset added blindly at the
    // boundary would make that 2037 and every elapsed time downstream would be
    // wrong by a growing amount.
    let dir = scratch("rollover-continuous");
    let base = dir.join("rec.oir");
    write(&base, &timelapse(0x60, 1, 3, true));
    write(&dir.join("rec_00001"), &timelapse(0x60, 4, 2, true));
    let (planes, _, _) = read_all(&base, &cfg_for_tests());
    let tps: Vec<u64> = planes.iter().map(|p| p.timepoint).collect();
    assert_eq!(tps, vec![1, 1, 2, 2, 3, 3, 4, 4, 5, 5], "got {tps:?}");
}

#[test]
fn rolls_over_and_carries_a_restarting_numbering() {
    // The other convention, which FastTIFF's importer documents. One expression
    // has to cover both, so both are tested.
    let dir = scratch("rollover-restart");
    let base = dir.join("rec.oir");
    write(&base, &timelapse(0x60, 1, 3, true));
    write(&dir.join("rec_00001"), &timelapse(0x60, 1, 2, true));
    let (planes, _, _) = read_all(&base, &cfg_for_tests());
    let tps: Vec<u64> = planes.iter().map(|p| p.timepoint).collect();
    assert_eq!(
        tps,
        vec![1, 1, 2, 2, 3, 3, 4, 4, 5, 5],
        "a restarting part must continue the session's numbering, got {tps:?}"
    );
}

#[test]
fn reports_the_part_it_moved_to() {
    let dir = scratch("rollover-reported");
    let base = dir.join("rec.oir");
    write(&base, &timelapse(0x60, 1, 1, true));
    write(&dir.join("rec_00001"), &timelapse(0x60, 2, 1, true));
    let mut r = LiveReader::open(&base, &cfg_for_tests()).expect("open");
    let poll = r.poll().expect("poll");
    assert!(
        poll.rolled_over.is_some(),
        "the new part should be reported"
    );
    assert_eq!(r.parts_opened(), 2);
}

// -------------------------------------------------------------- to_frame

/// Two planes of one timepoint, 4x4, channel 0 all 10s and channel 1 all 100s
/// except for a gradient that makes cropping and striding visible.
fn two_planes() -> Vec<Plane> {
    (0..2)
        .map(|c| Plane {
            timepoint: 7,
            channel: c,
            channel_uid: format!("uid{c}"),
            meta: FrameMeta {
                width: Some(4),
                height: Some(4),
                depth: Some(2),
                bit_counts: Some(10),
                ..FrameMeta::default()
            },
            samples: (0..16u16).map(|i| i + 100 * c as u16).collect(),
        })
        .collect()
}

const GEOM4: Geometry = Geometry {
    width: 4,
    height: 4,
    depth: 2,
};

#[test]
fn to_frame_picks_the_named_channel() {
    let mut cfg = cfg_for_tests();
    cfg.input.channel = crate::config::ChannelPick::Index(1);
    let f = to_frame(&two_planes(), GEOM4, &cfg).expect("frame");
    assert_eq!((f.width, f.height), (4, 4));
    assert_eq!(f.index, 7);
    assert_eq!(f.data[0], 100.0, "channel 1, not channel 0");
    assert_eq!(f.data[15], 115.0);
}

#[test]
fn to_frame_sums_channels_on_request() {
    let mut cfg = cfg_for_tests();
    cfg.input.channel = crate::config::ChannelPick::Named("sum".into());
    let f = to_frame(&two_planes(), GEOM4, &cfg).expect("frame");
    assert_eq!(f.data[0], 100.0, "0 + 100");
    assert_eq!(f.data[3], 106.0, "3 + 103");
}

#[test]
fn to_frame_crops_to_the_roi() {
    let mut cfg = cfg_for_tests();
    cfg.input.roi = Some([1, 1, 2, 2]);
    let f = to_frame(&two_planes(), GEOM4, &cfg).expect("frame");
    assert_eq!((f.width, f.height), (2, 2));
    // Rows 1..3, columns 1..3 of 0..16 laid out 4 wide.
    assert_eq!(f.data, vec![5.0, 6.0, 9.0, 10.0]);
}

#[test]
fn to_frame_clips_an_roi_that_overruns() {
    // A few pixels over the edge is a typo in a config file, not a reason not to
    // stabilise a recording.
    let mut cfg = cfg_for_tests();
    cfg.input.roi = Some([2, 2, 99, 99]);
    let f = to_frame(&two_planes(), GEOM4, &cfg).expect("frame");
    assert_eq!((f.width, f.height), (2, 2));
}

#[test]
fn to_frame_refuses_an_roi_that_is_entirely_outside() {
    let mut cfg = cfg_for_tests();
    cfg.input.roi = Some([10, 10, 4, 4]);
    assert!(to_frame(&two_planes(), GEOM4, &cfg).is_none());
}

#[test]
fn to_frame_downsamples_by_stride_not_by_averaging() {
    // Averaging is a low-pass filter, and the focus metric reads exactly the
    // high-frequency content it would remove. So the values must be *samples*.
    let mut cfg = cfg_for_tests();
    cfg.input.downsample = 2;
    let f = to_frame(&two_planes(), GEOM4, &cfg).expect("frame");
    assert_eq!((f.width, f.height), (2, 2));
    assert_eq!(
        f.data,
        vec![0.0, 2.0, 8.0, 10.0],
        "every second pixel of every second row, unaveraged"
    );
}

#[test]
fn to_frame_is_none_when_the_channel_is_not_there() {
    // Worth a test because the symptom is subtle: the program follows the file
    // and never measures anything, which looks like it is working.
    let mut cfg = cfg_for_tests();
    cfg.input.channel = crate::config::ChannelPick::Index(5);
    assert!(to_frame(&two_planes(), GEOM4, &cfg).is_none());
    assert!(to_frame(&[], GEOM4, &cfg).is_none());
}

// --------------------------------------------------------- the real file

/// The reference acquisition, which only exists on the acquisition machine.
///
/// Run with `cargo test -- --ignored real_acquisition`. The numbers asserted are
/// the ones probing the file established, and they are what make the synthetic
/// fixtures above trustworthy: a walk that agrees with this file's own block index
/// is a walk that does not need one.
#[test]
#[ignore = "needs the reference acquisition on D:, which is not in the repository"]
fn real_acquisition() {
    let path = Path::new(
        "D:\\nastya\\2-photon\\LJA5 project CNO Dynorphin and PI DRS Polyrythm\\2025_10_07\
         \\Field_4_Dynorphin_application.oir",
    );
    if !path.is_file() {
        eprintln!("skipping: {} is not here", path.display());
        return;
    }
    let mut cfg = cfg_for_tests();
    cfg.input.idle_timeout_s = 1;
    let f = File::open(path).expect("open");
    assert_eq!(
        find_first_block(&f, FIRST_BLOCK_PROBE).unwrap(),
        Some(0x60),
        "the real acquisition starts its blocks at 0x60"
    );

    let mut r = LiveReader::open(path, &cfg).expect("open");
    // Polls come back in bounded batches, so the first part takes several. Read
    // until the whole of it has been seen rather than the whole 14 GB recording.
    let mut timepoints = std::collections::BTreeSet::new();
    let mut planes = 0usize;
    let mut first_plane: Option<Plane> = None;
    for _ in 0..40 {
        let poll = r.poll().expect("poll");
        for pl in poll.planes {
            timepoints.insert(pl.timepoint);
            planes += 1;
            if first_plane.is_none() {
                first_plane = Some(pl);
            }
        }
        if timepoints.len() >= 1018 || (!poll.more && poll.finished) {
            break;
        }
    }

    assert_eq!(
        r.geometry(),
        Some(Geometry {
            width: 512,
            height: 512,
            depth: 2
        }),
        "512x512, 16-bit words"
    );
    assert_eq!(r.channels().len(), 2, "two channels");
    assert!(
        timepoints.len() >= 1018,
        "expected at least the first part's 1018 timepoints, got {}",
        timepoints.len()
    );
    assert_eq!(
        planes,
        timepoints.len() * 2,
        "both channels of every timepoint"
    );
    // Continuous numbering: the first part runs t001..t1018, so nothing should be
    // numbered past that until a rollover, and the count and the range must agree.
    assert_eq!(timepoints.iter().next(), Some(&1));
    let first = first_plane.expect("a plane");
    assert_eq!(first.samples.len(), 512 * 512);
    assert_eq!(first.meta.bit_counts, Some(10), "10 bits in a 16-bit word");
    assert_eq!(
        first.meta.full_scale(),
        1023.0,
        "saturation is 1023, not 65535"
    );
    assert_eq!(first.meta.z_position, Some(9741.19));
    assert_eq!(first.meta.axis_type.as_deref(), Some("TIMELAPSE"));
    assert!(first.meta.created.is_some());
    // 10-bit data really does live in the bottom 10 bits.
    assert!(
        first.samples.iter().all(|&s| s <= 1023),
        "a sample exceeded the stated 10-bit range"
    );
}
