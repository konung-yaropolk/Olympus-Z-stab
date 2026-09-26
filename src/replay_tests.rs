//! Tests for the rehearsal.
//!
//! What has to be true of a replay is narrow but not obvious: the copy must be
//! openable before it is complete, it must arrive in pieces that land inside
//! blocks, continuation parts must appear one at a time, the rate must come from
//! the recording rather than from the disk's speed, and nothing must be left in
//! the temp directory afterwards. Each of those is a test here.
//!
//! None of the fixtures below carries a type-1 metadata block, which is
//! deliberate: that is the one thing in the probe path that goes through
//! [`crate::oir::meta`], and these tests were written while that module was still
//! `todo!()`. A fixture with no metadata blocks exercises the fallback rate and
//! never calls into it. The one test that does need it is marked `#[ignore]`, and
//! the arithmetic it would exercise is tested directly instead, on the real
//! recording's own numbers.

use super::*;

/// A container shaped like a real acquisition's, built block by block.
struct Builder {
    body: Vec<u8>,
    offsets: Vec<u64>,
}

impl Builder {
    /// The real acquisition's header: the signature, `FLUOVIEW` at `0x48`, the
    /// three-byte type-2 block at `0x50` with its five bytes of `0xFF` filler,
    /// and so the first real block at `0x60`.
    fn real() -> Builder {
        let mut body = Vec::new();
        body.extend(oir::MAGIC);
        body.resize(0x48, 0);
        body.extend(b"FLUOVIEW");
        body.extend(3u32.to_le_bytes());
        body.extend(2u32.to_le_bytes());
        body.extend([0xFFu8; 8]);
        assert_eq!(body.len(), 0x60, "the first block belongs at 0x60");
        Builder {
            body,
            offsets: Vec::new(),
        }
    }

    /// FastTIFF's synthetic shape: blocks straight after the header, at `0x50`.
    fn fixture() -> Builder {
        let mut body = Vec::new();
        body.extend(oir::MAGIC);
        body.resize(0x50, 0);
        Builder {
            body,
            offsets: Vec::new(),
        }
    }

    /// One `u32 length, u32 type, payload` block.
    fn block(&mut self, ty: u32, payload: &[u8]) -> u64 {
        let at = self.body.len() as u64;
        self.body.extend((payload.len() as u32).to_le_bytes());
        self.body.extend(ty.to_le_bytes());
        self.body.extend(payload);
        self.offsets.push(at);
        at
    }

    /// A descriptor naming `name`, then the data block it describes.
    fn plane_chunk(&mut self, name: &str, at: u32, data: &[u8]) {
        let mut d = Vec::new();
        d.extend(at.to_le_bytes());
        d.extend((data.len() as u32).to_le_bytes());
        d.extend((name.len() as u32).to_le_bytes());
        d.extend(name.as_bytes());
        self.block(oir::TYPE_DESCRIPTOR, &d);
        self.block(oir::TYPE_DATA, data);
    }

    /// A per-frame metadata block, with the 40-byte binary prefix the real ones
    /// carry before the `<?xml`.
    fn frame_properties(&mut self, name: &str, created: &str) {
        let mut payload = vec![0u8; 40];
        payload.extend(
            format!(
                "<?xml version=\"1.0\"?><lsmframe:frameProperties>\
                 <commonframe:name>{name}</commonframe:name>\
                 <commonframe:creationDateTime>{created}</commonframe:creationDateTime>\
                 <commonimage:width>512</commonimage:width>\
                 <commonimage:height>512</commonimage:height>\
                 <commonimage:depth>2</commonimage:depth>\
                 <commonimage:bitCounts>10</commonimage:bitCounts>\
                 </lsmframe:frameProperties>"
            )
            .as_bytes(),
        );
        self.block(oir::TYPE_META, &payload);
    }

    /// The index, which a finished file has and a growing one does not — and
    /// which the replay therefore delivers last, exactly as the scope does.
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
}

/// A part file of `frames` interleaved two-channel timepoints, with no metadata
/// blocks — see this module's header for why.
///
/// The chunk size is deliberately not a round number and not a factor of
/// [`PRIME_BYTES`], so that a write boundary falling on a block boundary would be
/// a coincidence rather than the arrangement.
fn part_bytes(first_timepoint: usize, frames: usize) -> Vec<u8> {
    let mut b = Builder::real();
    // The reference snapshot, which is not a frame — and which is why the first
    // gap between two frames is not a frame's worth of bytes.
    b.plane_chunk("REF_LSM0_aaaa-bbbb_0", 0, &[0xEEu8; 30_720]);
    for f in 0..frames {
        let t = first_timepoint + f;
        for (c, uid) in ["1111-aaaa", "2222-bbbb"].iter().enumerate() {
            let data: Vec<u8> = (0..50_000u32)
                .map(|i| (i as u8).wrapping_add((t * 2 + c) as u8))
                .collect();
            b.plane_chunk(&format!("t{t:03}_0_1_{uid}_0"), 0, &data);
        }
        b.block(5, &[]);
    }
    b.finish()
}

/// Somewhere to keep a source recording. One directory per test, because the
/// scratch directory a replay picks is named after the source's stem and two
/// tests sharing a stem would be sharing a name.
fn source_dir(test: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zstab-replay-src-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("a temp directory");
    d
}

/// A config that replays quickly. With no timestamps to read, the fallback rate
/// is a chunk per poll interval, so a 1 ms poll interval is about 32 MB/s.
fn fast_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.input.poll_interval_ms = 1;
    cfg
}

fn wait_for(mut done: impl FnMut() -> bool, within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    done()
}

fn len_of(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Every offset at which a block begins, walked independently of the code under
/// test so that a bug in the walk cannot hide a bug in the chunking.
fn block_boundaries(bytes: &[u8], from: usize) -> Vec<u64> {
    let mut at = from;
    let mut found = vec![at as u64];
    while at + 8 <= bytes.len() {
        let len = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        if len > oir::MAX_BLOCK_BYTES {
            break;
        }
        at += 8 + len as usize;
        if at > bytes.len() {
            break;
        }
        found.push(at as u64);
    }
    found
}

#[test]
fn the_copy_can_be_opened_before_it_is_finished() {
    // The caller opens `output()` the moment `start` returns and reads the
    // signature out of it. If the copy were created empty and filled in by the
    // thread, that read would race and usually lose.
    let dir = source_dir("openable");
    let src = dir.join("rec.oir");
    // Comfortably longer than the primed head plus a chunk or two, so that "not
    // the whole file" is a real distinction rather than a coincidence of size.
    let bytes = part_bytes(1, 12);
    std::fs::write(&src, &bytes).unwrap();
    assert!(
        (bytes.len() as u64) > PRIME_BYTES + 2 * CHUNK_BYTES as u64,
        "the fixture is too short for this test to mean anything: {} bytes",
        bytes.len()
    );

    // Deliberately NOT `fast_cfg()`. With a 1 ms poll interval the fallback rate
    // is a chunk per millisecond — 32 MB/s — and the copying thread runs past the
    // primed head in the time it takes this test to call `read`, so the
    // assertions below would fail on a fast machine and pass on a slow one. A
    // long poll interval makes the rate a chunk per second, which is slow enough
    // that what `start` left behind is still what is there.
    let mut slow = fast_cfg();
    slow.input.poll_interval_ms = 1_000;

    let replay = Replay::start(&src, &slow).expect("a replay");
    let head = std::fs::read(replay.output()).expect("the copy exists already");
    assert!(
        head.len() >= oir::MAGIC.len(),
        "only {} bytes were in place when start returned",
        head.len()
    );
    assert_eq!(&head[..oir::MAGIC.len()], oir::MAGIC);
    // The primed head, plus at most the one chunk the copying thread writes
    // before it first sleeps. What is being asserted is that `start` did not
    // write the *whole file* — that is what would make the rehearsal not a
    // rehearsal — not that it wrote exactly `PRIME_BYTES`.
    assert!(
        (head.len() as u64) <= PRIME_BYTES + 2 * CHUNK_BYTES as u64,
        "{} bytes were in place when start returned, against a primed head of {PRIME_BYTES}: \
         the copy is not being paced",
        head.len()
    );
    assert!(
        (head.len() as u64) < bytes.len() as u64,
        "the source must be longer than the primed head for this test to mean anything"
    );

    drop(replay);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_write_boundary_falls_inside_a_block() {
    // The whole point of replaying in pieces. A reader handed whole blocks never
    // meets a partial one, and the partial one is the case the live reader exists
    // to handle.
    let dir = source_dir("midblock");
    let src = dir.join("rec.oir");
    let bytes = part_bytes(1, 6);
    std::fs::write(&src, &bytes).unwrap();

    let boundaries = block_boundaries(&bytes, 0x60);
    assert!(boundaries.len() > 4, "the fixture should have real blocks");
    assert!(
        !boundaries.contains(&PRIME_BYTES),
        "the primed head happens to end exactly on a block boundary, so this test \
         proves nothing — change the fixture's chunk size"
    );

    let replay = Replay::start(&src, &fast_cfg()).expect("a replay");
    let head = len_of(replay.output());
    assert!(
        !boundaries.contains(&head),
        "the copy stopped at {head}, which is a block boundary"
    );
    // And the chunk itself is smaller than a real data block, so this holds for
    // the real recording and not only for the fixture.
    assert!(
        CHUNK_BYTES < 485_376,
        "a chunk of {CHUNK_BYTES} is bigger than a real data block"
    );

    drop(replay);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_recording_arrives_byte_for_byte_index_and_all() {
    // Nothing is synthesised: the copy must be the source, including the block
    // index that only a finished file has.
    let dir = source_dir("exact");
    let src = dir.join("rec.oir");
    let bytes = part_bytes(1, 6);
    std::fs::write(&src, &bytes).unwrap();

    let replay = Replay::start(&src, &fast_cfg()).expect("a replay");
    let out = replay.output().to_path_buf();
    assert!(
        wait_for(
            || len_of(&out) == bytes.len() as u64,
            Duration::from_secs(20)
        ),
        "the copy stopped at {} of {} bytes",
        len_of(&out),
        bytes.len()
    );
    assert_eq!(std::fs::read(&out).unwrap(), bytes);

    drop(replay);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn continuation_parts_appear_one_at_a_time_and_in_order() {
    // The rollover path is half the reason the rehearsal exists: it is the one
    // part of the reader that cannot be exercised on a single file, and the one
    // that fails forty minutes into a recording.
    let dir = source_dir("parts");
    let src = dir.join("rec.oir");
    let first = part_bytes(1, 6);
    let second = part_bytes(1, 4);
    std::fs::write(&src, &first).unwrap();
    std::fs::write(dir.join("rec_00001"), &second).unwrap();

    let mut cfg = Config::default();
    // Slow enough that the first part is still arriving when the assertion below
    // runs, and fast enough that the test is over in a second.
    cfg.input.poll_interval_ms = 50;

    let replay = Replay::start(&src, &cfg).expect("a replay");
    let scratch = replay.scratch.clone();
    let part_two = scratch.join("rec_00001");
    assert!(
        !part_two.exists(),
        "the second part was there from the start, so the reader never sees it appear"
    );

    assert!(
        wait_for(
            || len_of(&part_two) == second.len() as u64,
            Duration::from_secs(30)
        ),
        "the second part stopped at {} of {} bytes",
        len_of(&part_two),
        second.len()
    );
    // The first part must be complete by then: parts are copied in order.
    assert_eq!(std::fs::read(replay.output()).unwrap(), first);
    assert_eq!(std::fs::read(&part_two).unwrap(), second);

    drop(replay);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_the_replay_takes_the_scratch_directory_with_it() {
    let dir = source_dir("cleanup");
    let src = dir.join("rec.oir");
    std::fs::write(&src, part_bytes(1, 6)).unwrap();

    let (scratch, output) = {
        let replay = Replay::start(&src, &fast_cfg()).expect("a replay");
        (replay.scratch.clone(), replay.output().to_path_buf())
    };
    assert!(
        !scratch.exists(),
        "{} outlived the replay — a gigabyte would be left in the temp directory",
        scratch.display()
    );
    assert!(!output.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_scratch_directory_is_named_from_the_stem_and_the_process() {
    // Predictable rather than random, so that a rehearsal interrupted with
    // Ctrl-C leaves something findable rather than an unidentifiable directory.
    let dir = source_dir("naming");
    let src = dir.join("Field_4_Dynorphin_application.oir");
    std::fs::write(&src, part_bytes(1, 2)).unwrap();

    let replay = Replay::start(&src, &fast_cfg()).expect("a replay");
    let name = replay
        .scratch
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        name.contains("Field_4_Dynorphin_application"),
        "{name} does not name the recording"
    );
    assert!(
        name.contains(&std::process::id().to_string()),
        "{name} does not name the process"
    );
    assert_eq!(
        replay.output().file_name().unwrap(),
        src.file_name().unwrap(),
        "the copy must keep the recording's own name, or the reader will not find \
         its continuation parts"
    );

    drop(replay);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_rehearsals_of_one_recording_do_not_share_a_directory() {
    // They would otherwise: the name is the stem plus the process id, and both of
    // those are the same. One replay would then be writing into the other's copy,
    // and the first to be dropped would delete it.
    let dir = source_dir("twice");
    let src = dir.join("rec.oir");
    std::fs::write(&src, part_bytes(1, 2)).unwrap();

    let a = Replay::start(&src, &fast_cfg()).expect("a replay");
    let b = Replay::start(&src, &fast_cfg()).expect("a second replay");
    assert_ne!(a.scratch, b.scratch);
    assert_ne!(a.output(), b.output());
    assert!(a.output().exists() && b.output().exists());

    drop(a);
    assert!(
        b.output().exists(),
        "dropping one replay removed the other's copy"
    );
    drop(b);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_source_that_is_not_an_oir_is_refused_by_signature() {
    let dir = source_dir("notanoir");
    let src = dir.join("notes.txt");
    std::fs::write(&src, b"this is not a recording, it is a note about one").unwrap();

    let err = Replay::start(&src, &fast_cfg()).expect_err("should be refused");
    assert!(
        err.contains("OLYMPUSRAWFORMAT"),
        "the refusal should say what was missing: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_parts_of_a_split_recording_are_found_in_order_and_stop_at_a_gap() {
    let dir = source_dir("partlist");
    let src = dir.join("rec.oir");
    std::fs::write(&src, b"x").unwrap();
    // The real acquisition writes continuations with no extension; some exports
    // add one, so both spellings count.
    std::fs::write(dir.join("rec_00001"), b"x").unwrap();
    std::fs::write(dir.join("rec_00002.oir"), b"x").unwrap();
    // 00003 is missing, so 00004 is not a continuation of anything.
    std::fs::write(dir.join("rec_00004"), b"x").unwrap();

    let parts = source_parts(&src);
    assert_eq!(
        parts,
        vec![
            src.clone(),
            dir.join("rec_00001"),
            dir.join("rec_00002.oir")
        ],
        "numbering must stop at the first gap"
    );

    // A recording of one part is a recording of one part, not an error.
    let lone = dir.join("single.oir");
    std::fs::write(&lone, b"x").unwrap();
    assert_eq!(source_parts(&lone), vec![lone]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_first_block_is_found_at_0x60_although_0x50_parses_one_block_too() {
    // The trap. At 0x50 the real file holds `u32 3, u32 2` and then five bytes of
    // 0xFF filler, which parses as one three-byte type-2 block whose successor's
    // length reads as 0xFFFFFFFF. A probe that took the first candidate yielding
    // any run at all would stop there, find no frames, and quietly replay at the
    // fallback rate instead of the recording's own.
    let dir = source_dir("firstblock");
    let src = dir.join("rec.oir");
    let bytes = part_bytes(1, 3);
    std::fs::write(&src, &bytes).unwrap();

    let file = File::open(&src).unwrap();
    let len = bytes.len() as u64;
    assert_eq!(
        blocks_that_parse(&file, 0x50, len, START_PROBE_BLOCKS),
        1,
        "0x50 should parse exactly one block in a real-shaped file"
    );
    assert!(blocks_that_parse(&file, 0x60, len, START_PROBE_BLOCKS) >= START_PROBE_BLOCKS);
    assert_eq!(first_block_offset(&file, len), Some(0x60));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_synthetic_fixture_shape_is_found_at_0x50() {
    // FastTIFF's own OIR fixtures put the first block straight after the header,
    // and a reader that only knew the real file's layout would read none of them.
    let dir = source_dir("fixtureshape");
    let src = dir.join("rec.oir");
    let mut b = Builder::fixture();
    for i in 0..4 {
        b.plane_chunk(&format!("t00{}_0_1_aaaa_0", i + 1), 0, &[7u8; 4096]);
    }
    let bytes = b.finish();
    std::fs::write(&src, &bytes).unwrap();

    let file = File::open(&src).unwrap();
    assert_eq!(first_block_offset(&file, bytes.len() as u64), Some(0x50));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_rate_ignores_the_preamble_between_the_first_two_frames() {
    // These are the real offsets of the first eight frameProperties blocks of
    // Field_4_Dynorphin_application.oir. The first gap is 2989381 bytes because
    // the reference snapshot and the lookup tables sit inside it; every later gap
    // is 1052504, which is what one frame of 512x512x2 channels actually costs.
    // Taking the first gap for a frame would replay the file at 22 MB/s instead
    // of 7.9, and every threshold read off that rehearsal would be wrong.
    let offsets = [
        525_742u64, 3_515_123, 4_567_627, 5_620_131, 6_672_635, 7_725_139, 8_777_643, 9_830_147,
    ];
    assert_eq!(bytes_per_frame(&offsets), Some(1_052_504.0));

    let pace = rate_from(bytes_per_frame(&offsets), Some(0.133), 500);
    let mb = pace.bytes_per_second / 1e6;
    assert!(
        (mb - 7.91).abs() < 0.1,
        "the real recording replays at 7.9 MB/s, not {mb:.2}"
    );
    assert_eq!(pace.interval_s, Some(0.133));

    // Two frames is all there is to go on in a very short recording; the one gap
    // has to be used even though it is the odd one.
    assert_eq!(bytes_per_frame(&offsets[..2]), Some(2_989_381.0));
    assert_eq!(bytes_per_frame(&offsets[..1]), None);
    assert_eq!(bytes_per_frame(&[]), None);
}

#[test]
fn one_odd_gap_cannot_set_the_rate() {
    // A median, not a mean: a single missing frame, or a pause the operator took,
    // doubles one gap, and a mean would carry that into the whole rehearsal.
    let offsets = [0u64, 1_000, 2_000, 3_000, 90_000, 91_000, 92_000];
    assert_eq!(bytes_per_frame(&offsets), Some(1_000.0));

    assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
    assert_eq!(median(vec![4.0, 1.0, 3.0, 2.0]), Some(2.5));
    assert_eq!(median(Vec::new()), None);
}

#[test]
fn the_frame_interval_comes_from_the_timestamps_and_survives_a_gap_in_them() {
    let stamp = |s: Option<&str>| FrameMeta {
        created: s.map(|s| s.to_string()),
        ..FrameMeta::default()
    };
    // The real recording's own timestamps: 7.5 Hz, 133 ms apart.
    let metas = vec![
        stamp(Some("2025-10-07T21:58:59.990-04:00")),
        stamp(Some("2025-10-07T21:59:00.123-04:00")),
        // A frame whose metadata could not be parsed. It must cost the two
        // differences it touches, not the whole measurement.
        stamp(None),
        stamp(Some("2025-10-07T21:59:00.390-04:00")),
        stamp(Some("2025-10-07T21:59:00.523-04:00")),
        stamp(Some("2025-10-07T21:59:00.656-04:00")),
    ];
    let dt = frame_interval(&metas).expect("an interval");
    assert!((dt - 0.133).abs() < 1e-6, "{dt} should be 0.133 s");

    assert_eq!(frame_interval(&[]), None);
    assert_eq!(frame_interval(&[stamp(None), stamp(None)]), None);
    // Two frames the scope wrote in the same millisecond say nothing about the
    // rate, and a zero interval would be a division by zero.
    assert_eq!(
        frame_interval(&[
            stamp(Some("2025-10-07T21:58:59.990-04:00")),
            stamp(Some("2025-10-07T21:58:59.990-04:00")),
        ]),
        None
    );
}

#[test]
fn without_timestamps_the_replay_is_still_incremental() {
    // The fallback. It is not the recording's rate and does not pretend to be,
    // but it must still be a rate: dumping the file would make the rehearsal a
    // test of nothing.
    let a = rate_from(None, None, 500);
    assert_eq!(a.interval_s, None);
    assert!(
        (a.bytes_per_second - CHUNK_BYTES as f64 * 2.0).abs() < 1.0,
        "a chunk per 500 ms tick, not {}",
        a.bytes_per_second
    );

    // Knowing a frame's size without knowing its duration is better than
    // neither: a frame per poll is a rate the follower can keep up with.
    let b = rate_from(Some(1_052_504.0), None, 500);
    assert!((b.bytes_per_second - 2_105_008.0).abs() < 1.0);
    assert_eq!(b.interval_s, None);

    // Nonsense in, fallback out, rather than a division by zero or a negative
    // rate that would make `pace` sleep forever.
    for bad in [Some(0.0), Some(-1.0)] {
        let p = rate_from(bad, Some(0.133), 500);
        assert!(p.bytes_per_second > 0.0 && p.bytes_per_second.is_finite());
    }
    let p = rate_from(Some(1_052_504.0), Some(0.0), 500);
    assert!(p.bytes_per_second > 0.0 && p.interval_s.is_none());
    // A zero poll interval is refused by `validate`, but arithmetic that divides
    // by it would be a poor way to find that out.
    assert!(rate_from(None, None, 0).bytes_per_second.is_finite());
}

#[test]
fn a_fixture_with_no_metadata_blocks_falls_back_rather_than_guessing() {
    let dir = source_dir("norate");
    let src = dir.join("rec.oir");
    std::fs::write(&src, part_bytes(1, 4)).unwrap();

    assert!(frame_marks(&src, PROBE_FRAMES).is_empty());
    let pace = replay_rate(&src, &fast_cfg());
    assert_eq!(pace.interval_s, None);
    assert!(pace.bytes_per_second > 0.0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_rate_is_honoured_rather_than_the_disk_being_filled() {
    // The rehearsal is worthless if the copy lands faster than the pipeline can
    // read it: the reader would see one enormous poll and no drift over time. So
    // check that a rate actually costs time. The arithmetic is 32 KiB per 20 ms,
    // about 1.6 MB/s, over rather more than half a megabyte.
    let dir = source_dir("paced");
    let src = dir.join("rec.oir");
    let bytes = part_bytes(1, 7);
    assert!(bytes.len() as u64 > PRIME_BYTES + 400_000);
    std::fs::write(&src, &bytes).unwrap();

    let mut cfg = Config::default();
    cfg.input.poll_interval_ms = 20;
    let started = Instant::now();
    let replay = Replay::start(&src, &cfg).expect("a replay");
    let out = replay.output().to_path_buf();
    assert!(
        wait_for(
            || len_of(&out) == bytes.len() as u64,
            Duration::from_secs(60)
        ),
        "the copy stopped at {} of {} bytes",
        len_of(&out),
        bytes.len()
    );
    let took = started.elapsed();
    assert!(
        took > Duration::from_millis(150),
        "the whole file arrived in {took:?}, so nothing is pacing it"
    );

    drop(replay);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn timestamps_in_the_source_set_the_replay_rate() {
    // The one path these tests cannot take on their own: reading the timestamps
    // out of the metadata blocks goes through `oir::meta`. The arithmetic either
    // side of it is covered above, on the real recording's numbers; this is the
    // glue.
    let dir = source_dir("timestamps");
    let src = dir.join("rec.oir");
    let mut b = Builder::real();
    b.plane_chunk("REF_LSM0_aaaa-bbbb_0", 0, &[0xEEu8; 30_720]);
    for t in 1..=8usize {
        // A millisecond apart, so that a rate read from these replays the fixture
        // quickly rather than in real time.
        b.frame_properties(
            &format!("t{t:03}_0_1"),
            &format!("2025-10-07T21:58:59.{:03}-04:00", 100 + t),
        );
        for uid in ["1111-aaaa", "2222-bbbb"] {
            b.plane_chunk(&format!("t{t:03}_0_1_{uid}_0"), 0, &[3u8; 50_000]);
        }
        b.block(5, &[]);
    }
    let bytes = b.finish();
    std::fs::write(&src, &bytes).unwrap();

    let marks = frame_marks(&src, PROBE_FRAMES);
    assert_eq!(
        marks.len(),
        PROBE_FRAMES,
        "eight metadata blocks, eight marks"
    );
    let pace = replay_rate(&src, &fast_cfg());
    let dt = pace
        .interval_s
        .expect("the timestamps should have been read");
    assert!((dt - 0.001).abs() < 1e-9, "{dt} should be one millisecond");

    let replay = Replay::start(&src, &fast_cfg()).expect("a replay");
    let out = replay.output().to_path_buf();
    assert!(wait_for(
        || len_of(&out) == bytes.len() as u64,
        Duration::from_secs(30)
    ));
    assert_eq!(std::fs::read(&out).unwrap(), bytes);

    drop(replay);
    let _ = std::fs::remove_dir_all(&dir);
}
