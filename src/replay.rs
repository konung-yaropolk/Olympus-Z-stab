//! Rehearsal: turn a finished recording back into a growing one.
//!
//! This is `--replay`, and it exists because the thresholds in `config.yaml` —
//! the dead band above all — cannot be chosen from first principles. They depend
//! on the sample, the objective, the laser power and how noisy the recording is.
//! The only way to choose them is to watch what the metric actually does on a
//! real recording from the same rig, and the only way to do that without a
//! microscope is to replay one.
//!
//! A background thread copies the source into a scratch file at the rate the
//! source itself was recorded — read from its own frame timestamps, so a 7.5 Hz
//! recording replays at 7.5 Hz — while the reader in the main thread follows the
//! copy exactly as it would follow a live acquisition. Parts are replayed in
//! order and appear one at a time, so the rollover path is exercised too.
//!
//! It is a rehearsal, not a simulation: nothing is synthesised. Every byte the
//! reader sees is a byte the scope wrote.
//!
//! # How the rate is worked out
//!
//! Two numbers are needed: how many bytes one frame costs, and how long one
//! frame took. Both are read out of the head of the source, from the per-frame
//! metadata blocks — the distance between two of them is a frame's worth of
//! bytes, and the difference of their timestamps is a frame's worth of time.
//! On the recording this was built against those come out at 1052504 bytes and
//! 0.133 s, so 7.9 MB/s, and a 1.076 GB part takes the 136 s it originally took.
//!
//! Both are medians rather than means, and the **first** gap is thrown away: the
//! reference snapshot and the lookup tables sit between the first frame's
//! metadata and the second's, which makes that one gap 2989381 bytes against
//! 1052504 for every later one. Believing it replays the file three times too
//! fast, which is the kind of error that makes a rehearsal quietly useless
//! rather than obviously broken.
//!
//! # Why the block walk here is its own
//!
//! [`crate::oir::LiveReader`] walks the same blocks, but it is a stateful
//! consumer: it reassembles planes, it allocates a frame buffer per channel, and
//! its cursor is the session's. All that is needed here is the offset and the
//! timestamp of a handful of metadata blocks in the first few megabytes, so this
//! reads the block headers itself and skips everything else. The parsing of the
//! metadata, which is the part that is actually intricate, is
//! [`crate::oir::meta`]'s.

use crate::config::Config;
use crate::frame::FrameMeta;
use crate::oir::{self, meta};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How much is written at a time.
///
/// This has to be smaller than a block and unrelated to any block boundary, or
/// the reader never meets the case it exists to handle. A real data block is
/// 485376 bytes, so 32 KiB lands inside one about fifteen times out of sixteen.
const CHUNK_BYTES: usize = 32 * 1024;

/// Copied before `start` returns.
///
/// The caller opens the copy the instant this function comes back, and a reader
/// handed an empty file cannot even check the signature — let alone find the
/// first block, which it does by looking for a run of blocks that parse. So the
/// head of the recording is written synchronously, and it is made comfortably
/// longer than a header plus a few blocks. It is still a fraction of one frame.
const PRIME_BYTES: u64 = 128 * 1024;

/// Per-frame metadata blocks to look at when learning the source's own rate.
///
/// Seven usable gaps, of which the first is discarded, is plenty for a median
/// and costs one seek per block over the first ten megabytes.
const PROBE_FRAMES: usize = 8;

/// How far into the source to look for them before giving up.
const PROBE_BYTES: u64 = 32 << 20;

/// And a bound on blocks, so that a file of pathologically small blocks cannot
/// turn the probe into a million seeks.
const PROBE_BLOCKS: usize = 20_000;

/// Where the block stream might start. The real acquisition begins its blocks at
/// `0x60`; FastTIFF's synthetic fixtures begin at `0x50`.
const BLOCK_START_CANDIDATES: [u64; 2] = [0x50, 0x60];

/// Blocks that must parse from a candidate offset for it to be believed.
const START_PROBE_BLOCKS: usize = 8;

/// Block types seen in a real file are 0 to 5; anything far above that is being
/// read from the wrong offset.
const MAX_PLAUSIBLE_TYPE: u32 = 8;

/// The shortest sleep worth asking the OS for. Windows' timer granularity is
/// about 15 ms.
const SLEEP_FLOOR: Duration = Duration::from_millis(15);

/// How long the copying thread may sleep without noticing it has been stopped.
const SLEEP_SLICE: Duration = Duration::from_millis(50);

/// A replay in progress. Dropping it stops the copying thread and removes the
/// scratch files — a rehearsal leaves nothing behind, least of all a half-copy of
/// a gigabyte recording in a temp directory.
#[derive(Debug)]
pub struct Replay {
    output: PathBuf,
    /// Set to stop the thread; the thread checks it between chunks.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Directory holding the copy, removed on drop.
    scratch: PathBuf,
}

impl Replay {
    /// Begin replaying `source` into a scratch directory.
    ///
    /// Must write in pieces small enough that the reader sees partial blocks —
    /// writing a whole frame at a time would never exercise the case the live
    /// reader exists to handle. A chunk of a few tens of kilobytes lands
    /// mid-block often enough to be a real test.
    ///
    /// The replay rate comes from the source's own timestamps. When they cannot be
    /// read, it falls back to `input.poll_interval_ms` worth of bytes per tick,
    /// which is fast but still incremental.
    ///
    /// Nothing here loads a part into memory: the source is normally 1.076 GB and
    /// is copied through one [`CHUNK_BYTES`] buffer.
    pub fn start(source: &Path, cfg: &Config) -> Result<Replay, String> {
        let name = source
            .file_name()
            .ok_or_else(|| format!("{} has no file name to copy", source.display()))?
            .to_owned();
        check_signature(source)?;

        // Every part of the source exists already — it is a finished recording —
        // but they are *written* one at a time, which is the point.
        let parts = source_parts(source);
        let pace = replay_rate(source, cfg);
        match pace.interval_s {
            Some(dt) => println!(
                "  replaying {} part(s) at {:.2} Hz, {:.1} MB/s — the rate it was recorded at",
                parts.len(),
                1.0 / dt,
                pace.bytes_per_second / 1e6,
            ),
            None => println!(
                "  replaying {} part(s) at a fixed {:.1} MB/s — the recording's own frame \
                 timestamps could not be read",
                parts.len(),
                pace.bytes_per_second / 1e6,
            ),
        }

        let scratch = scratch_dir(source);
        // Left over from a crashed run that happened to have this process id: it
        // is ours by name, so it is ours to clear.
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).map_err(|e| {
            format!(
                "could not make the rehearsal directory {}: {e}",
                scratch.display()
            )
        })?;
        let output = scratch.join(name);

        let primed = prime(&parts[0], &output, PRIME_BYTES)
            .map_err(|e| format!("could not start the copy at {}: {e}", output.display()))?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_scratch = scratch.clone();
        let bytes_per_second = pace.bytes_per_second;
        let handle = std::thread::spawn(move || {
            if let Err(e) = copy_parts(
                &parts,
                &thread_scratch,
                primed,
                bytes_per_second,
                &thread_stop,
            ) {
                // The main thread is following the copy and will decide the
                // acquisition has finished once it stops growing, which is the
                // right thing to do anyway — so this only has to be said, not
                // acted on.
                eprintln!("the rehearsal stopped copying: {e}");
            }
        });

        Ok(Replay {
            output,
            stop,
            handle: Some(handle),
            scratch,
        })
    }

    /// The file to follow — the growing copy, not the source.
    pub fn output(&self) -> &Path {
        &self.output
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// Where this replay's copy lives.
///
/// The file stem and the process id are what name it, so that two rehearsals of
/// different recordings, or on different machines sharing a temp directory, do
/// not tread on each other — and so that the name is predictable, since nothing
/// here may use randomness or the clock. The counter is there because one
/// process can hold two rehearsals at once (the test suite does), and a shared
/// scratch directory would have one replay deleting the other's copy.
fn scratch_dir(source: &Path) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "recording".to_string());
    // A recording's name reaches here from a path the user typed; keep it to
    // characters that are a directory name everywhere.
    let stem: String = stem
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir().join(format!(
        "zstab-replay-{stem}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Refuse a source that is not an OIR.
///
/// Without this a typo produces a rehearsal that copies something meaningless,
/// follows it for the whole idle timeout and then reports that the acquisition
/// has finished — a confusing way to learn of a mistyped name.
fn check_signature(path: &Path) -> Result<(), String> {
    let mut file =
        File::open(path).map_err(|e| format!("could not open {}: {e}", path.display()))?;
    let mut magic = vec![0u8; oir::MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    if magic.as_slice() != oir::MAGIC {
        return Err(format!(
            "{} does not begin with {} — only an OIR can be rehearsed",
            path.display(),
            String::from_utf8_lossy(oir::MAGIC)
        ));
    }
    Ok(())
}

/// The parts of the source recording, in order, the named one first.
///
/// The same convention [`crate::oir::acquisition_parts`] reads: `<stem>_00001`,
/// with no extension as the real acquisition writes them, and with `.oir` as
/// some exports do. It is duplicated here rather than borrowed because the
/// replay needs the convention in the other direction too — it *writes* the
/// continuation parts, and their names have to be names the reader will then go
/// looking for.
fn source_parts(first: &Path) -> Vec<PathBuf> {
    let mut parts = vec![first.to_path_buf()];
    let (Some(dir), Some(stem)) = (first.parent(), first.file_stem()) else {
        return parts;
    };
    let stem = stem.to_string_lossy().to_string();
    for n in 1..=oir::MAX_PARTS {
        let plain = dir.join(format!("{stem}_{n:05}"));
        let dotted = dir.join(format!("{stem}_{n:05}.oir"));
        if plain.is_file() {
            parts.push(plain);
        } else if dotted.is_file() {
            parts.push(dotted);
        } else {
            // Numbering stops at the first gap: a later part with a number
            // missing before it is not a continuation of anything.
            break;
        }
    }
    parts
}

/// Copy the head of `source` to `dest`, returning how much was written.
fn prime(source: &Path, dest: &Path, bytes: u64) -> std::io::Result<u64> {
    let mut src = File::open(source)?;
    let mut out = File::create(dest)?;
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut done = 0u64;
    while done < bytes {
        let want = ((bytes - done) as usize).min(buf.len());
        let n = src.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
    }
    Ok(done)
}

/// The copying thread's whole job.
///
/// `primed` bytes of `parts[0]` are already in place. The pacing clock spans all
/// the parts, so the gap the acquisition software leaves when it rolls over is
/// not invented here: one part ends and the next begins at the rate the frames
/// were arriving.
fn copy_parts(
    parts: &[PathBuf],
    scratch: &Path,
    primed: u64,
    bytes_per_second: f64,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    let started = Instant::now();
    let mut written = 0u64;
    let mut buf = vec![0u8; CHUNK_BYTES];

    for (i, part) in parts.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(name) = part.file_name() else {
            continue;
        };
        let dest = scratch.join(name);

        let mut src = File::open(part)?;
        let mut out = if i == 0 {
            // Append rather than create: the head is already there.
            src.seek(SeekFrom::Start(primed))?;
            OpenOptions::new().append(true).open(&dest)?
        } else {
            // A part the reader has not seen before appears here, empty, and
            // grows — which is exactly what it does during a recording.
            File::create(&dest)?
        };

        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            // Unbuffered on purpose: a buffered writer would hold bytes back and
            // hand the reader whole buffers, which is the opposite of the point.
            out.write_all(&buf[..n])?;
            written += n as u64;
            pace(started, written, bytes_per_second, stop);
        }
    }
    Ok(())
}

/// Wait until `written` bytes is the right amount to have written by now.
fn pace(started: Instant, written: u64, bytes_per_second: f64, stop: &AtomicBool) {
    // Negated deliberately, and clippy is wrong to want `<= 0.0` here: that is
    // *false* for a NaN, which would then reach `Duration::from_secs_f64` below
    // and panic. Written this way, zero, negative and NaN all mean "do not pace".
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(bytes_per_second > 0.0) {
        return;
    }
    let due = Duration::from_secs_f64(written as f64 / bytes_per_second);
    let now = started.elapsed();
    if due <= now {
        return;
    }
    // At 7.9 MB/s a chunk is worth 4 ms, and Windows cannot sleep for 4 ms: it
    // sleeps for about 15. Asking anyway would run the whole rehearsal at a
    // quarter speed, so the debt is allowed to build past the granularity and
    // paid off in one sleep. The cost is that bytes arrive in small bursts,
    // which is harmless — four chunks is still a fraction of a block.
    let debt = due - now;
    if debt < SLEEP_FLOOR {
        return;
    }
    sleep_until_stopped(debt, stop);
}

/// Sleep, but in slices, so that dropping the [`Replay`] does not have to wait
/// out a long one.
fn sleep_until_stopped(mut left: Duration, stop: &AtomicBool) {
    while left > Duration::from_millis(0) {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let slice = left.min(SLEEP_SLICE);
        std::thread::sleep(slice);
        left -= slice;
    }
}

/// The rate a rehearsal runs at, and whether the recording set it.
struct Pace {
    bytes_per_second: f64,
    /// Seconds per frame as the source's own timestamps state it, when they can
    /// be read at all. `None` means the fallback rate is in use.
    interval_s: Option<f64>,
}

/// Work out how fast to write `source`.
fn replay_rate(source: &Path, cfg: &Config) -> Pace {
    let marks = frame_marks(source, PROBE_FRAMES);
    let offsets: Vec<u64> = marks.iter().map(|(at, _)| *at).collect();
    let metas: Vec<FrameMeta> = marks.into_iter().map(|(_, m)| m).collect();
    rate_from(
        bytes_per_frame(&offsets),
        frame_interval(&metas),
        cfg.input.poll_interval_ms,
    )
}

/// Bytes per second from a frame's size and a frame's duration.
///
/// The fallback is a poll interval's worth of bytes per tick: a frame per poll
/// when the frame size is known, a chunk per poll when not even that is. Neither
/// is the recording's own rate, but both are incremental, which is what the
/// rehearsal needs to stay a rehearsal.
fn rate_from(bytes_per_frame: Option<f64>, interval_s: Option<f64>, poll_interval_ms: u64) -> Pace {
    let tick = poll_interval_ms.max(1) as f64 / 1000.0;
    match (bytes_per_frame, interval_s) {
        (Some(b), Some(dt)) if b > 0.0 && dt > 0.0 => Pace {
            bytes_per_second: b / dt,
            interval_s: Some(dt),
        },
        (Some(b), _) if b > 0.0 => Pace {
            bytes_per_second: b / tick,
            interval_s: None,
        },
        _ => Pace {
            bytes_per_second: CHUNK_BYTES as f64 / tick,
            interval_s: None,
        },
    }
}

/// A frame's worth of bytes, from the distance between metadata blocks.
fn bytes_per_frame(offsets: &[u64]) -> Option<f64> {
    let mut gaps: Vec<f64> = offsets
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]) as f64)
        .collect();
    // Between the first frame's metadata and the second's sit the reference
    // snapshot and the lookup tables — 2989381 bytes against 1052504 for every
    // later gap in the real recording. It is not a frame, and taking it for one
    // replays the file three times too fast.
    if gaps.len() > 1 {
        gaps.remove(0);
    }
    median(gaps).filter(|b| *b > 0.0)
}

/// A frame's worth of seconds, from the timestamps the file itself carries.
///
/// A median of the differences, not the span over the count: one unparsable or
/// missing timestamp in the middle would otherwise set the rate for the whole
/// rehearsal.
fn frame_interval(metas: &[FrameMeta]) -> Option<f64> {
    let deltas: Vec<f64> = metas
        .windows(2)
        .filter_map(|w| w[1].interval_s(&w[0]))
        .filter(|d| *d > 0.0 && d.is_finite())
        .collect();
    median(deltas)
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 0 {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    })
}

/// Offset and parsed metadata of the first few per-frame metadata blocks.
///
/// Only blocks that [`crate::oir::meta`] can make sense of are returned, so a
/// type-1 block that is not a `frameProperties` document contributes neither an
/// offset nor a timestamp — which matters, because a stray offset would land in
/// the middle of the gap measurement.
fn frame_marks(source: &Path, wanted: usize) -> Vec<(u64, FrameMeta)> {
    let mut found: Vec<(u64, FrameMeta)> = Vec::new();
    let Ok(file) = File::open(source) else {
        return found;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let Some(mut at) = first_block_offset(&file, len) else {
        return found;
    };

    let mut payload: Vec<u8> = Vec::new();
    let mut blocks = 0usize;
    while found.len() < wanted && at < PROBE_BYTES && blocks < PROBE_BLOCKS {
        let Some((body, block_len, ty)) = block_header(&file, at, len) else {
            break;
        };
        if ty == oir::TYPE_META {
            payload.resize(block_len as usize, 0);
            if read_at(&file, body, &mut payload).is_ok() {
                if let Some(xml) = meta::xml_of_block(&payload) {
                    found.push((at, meta::parse_frame_properties(xml)));
                }
            }
        }
        at = body + block_len as u64;
        blocks += 1;
    }
    found
}

/// Where the block stream starts.
///
/// The candidate with the longest agreeing run wins, and not the first candidate
/// that yields any run at all — which is the trap. In the real file `0x50` holds
/// a three-byte type-2 block followed by five bytes of `0xFF` filler, so one
/// block parses there and the next length reads as `0xFFFFFFFF`. A
/// first-match-wins probe takes that single block for the stream, finds no
/// frames after it, and silently falls back to the fixed rate.
fn first_block_offset(file: &File, len: u64) -> Option<u64> {
    let mut best: Option<(usize, u64)> = None;
    for &candidate in BLOCK_START_CANDIDATES.iter() {
        let run = blocks_that_parse(file, candidate, len, START_PROBE_BLOCKS);
        if run >= START_PROBE_BLOCKS {
            return Some(candidate);
        }
        if run > best.map(|(r, _)| r).unwrap_or(0) {
            best = Some((run, candidate));
        }
    }
    best.map(|(_, at)| at)
}

/// How many consecutive blocks parse from `from`, up to `cap`.
fn blocks_that_parse(file: &File, from: u64, len: u64, cap: usize) -> usize {
    let mut at = from;
    let mut n = 0;
    while n < cap {
        match block_header(file, at, len) {
            Some((body, block_len, _)) => at = body + block_len as u64,
            None => break,
        }
        n += 1;
    }
    n
}

/// `(payload offset, payload length, type)` of the block at `at`, or `None` if
/// there is not a plausible whole block there.
///
/// A length beyond [`crate::oir::MAX_BLOCK_BYTES`] is how the walk stops at the
/// index without having to know the index is there: its marker is `0xFFFFFFFF`.
fn block_header(file: &File, at: u64, len: u64) -> Option<(u64, u32, u32)> {
    if at.checked_add(8)? > len {
        return None;
    }
    let mut head = [0u8; 8];
    read_at(file, at, &mut head).ok()?;
    let block_len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let ty = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    if block_len > oir::MAX_BLOCK_BYTES || ty > MAX_PLAUSIBLE_TYPE {
        return None;
    }
    let body = at + 8;
    if body.checked_add(block_len as u64)? > len {
        return None;
    }
    Some((body, block_len, ty))
}

/// Read exactly `buf.len()` bytes from `at`. Never a mapping: the source is read
/// while nothing else holds it, but the copy is written while the reader has it
/// open, and the two should be read the same way.
fn read_at(file: &File, at: u64, buf: &mut [u8]) -> std::io::Result<()> {
    let mut file = file;
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(buf)
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod replay_tests;
