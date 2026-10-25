//! Deterministic local workloads; run with `cargo bench -p rustlink-audio --bench hot_paths`.
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rustlink_audio::{ogg::OggOpusReader, Body, Engine, Preference};
use rustlink_protocol::{Track, TrackInfo};

#[allow(dead_code)]
#[path = "support/legacy_ogg.rs"]
mod legacy_ogg;

struct CountAlloc;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;

fn page(sequence: u32, packets: &[&[u8]], out: &mut Vec<u8>) {
    out.extend_from_slice(b"OggS\0\0");
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(packets.len() as u8);
    out.extend(packets.iter().map(|packet| packet.len() as u8));
    for packet in packets {
        out.extend_from_slice(packet);
    }
}

fn ogg_fixture() -> Vec<u8> {
    let mut out = Vec::new();
    page(0, &[b"OpusHead\x01\x02\0\0\x80\xbb\0\0\0\0\0"], &mut out);
    page(1, &[b"OpusTags\0\0\0\0\0\0\0\0"], &mut out);
    for sequence in 2..4002 {
        page(sequence, &[&[42; 160][..]; 50], &mut out);
    }
    out
}

fn wav_fixture() -> Vec<u8> {
    let frames = 480_000u32;
    let data_len = frames * 4;
    let mut out = Vec::with_capacity(data_len as usize + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x02\0");
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&192_000u32.to_le_bytes());
    out.extend_from_slice(b"\x04\0\x10\0data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        let sample =
            ((i as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 12_000.0) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

fn measure(name: &str, count: u64, mut run: impl FnMut()) {
    run();
    let mut samples = Vec::new();
    for _ in 0..7 {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        run();
        let elapsed = start.elapsed();
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        samples.push((elapsed, allocations));
    }
    samples.sort_unstable();
    let (elapsed, allocations) = samples[3];
    println!("{name}: {count} units, median {:.3} ms, {:.1} ns/unit, {allocations} Rust allocations/reallocations", elapsed.as_secs_f64() * 1000.0, elapsed.as_nanos() as f64 / count as f64);
}

/// Check every payload byte and packet boundary outside the timed region.
/// This keeps checksum cost out of the parser measurements while ensuring all
/// three implementations consume equivalent data on the benchmark fixture.
fn inspect_packets(mut next: impl FnMut(&mut Vec<u8>) -> std::io::Result<bool>) -> (usize, u64) {
    let mut packet = Vec::new();
    let mut count = 0;
    let mut checksum = 0xcbf29ce484222325u64;
    while next(&mut packet).unwrap() {
        for byte in (packet.len() as u64).to_le_bytes().iter().chain(packet.iter()) {
            checksum = (checksum ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
        count += 1;
    }
    (count, checksum)
}

fn main() {
    let ogg = ogg_fixture();
    let mut legacy = legacy_ogg::OggOpusReader::new(ogg.as_slice());
    let expected = inspect_packets(|packet| match legacy.next_packet()? {
        Some(next) => {
            *packet = next;
            Ok(true)
        }
        None => Ok(false),
    });
    let mut owned = OggOpusReader::new(ogg.as_slice());
    let owned_result = inspect_packets(|packet| match owned.next_packet()? {
        Some(next) => {
            *packet = next;
            Ok(true)
        }
        None => Ok(false),
    });
    let mut reused = OggOpusReader::new(ogg.as_slice());
    let reused_result = inspect_packets(|packet| reused.next_packet_into(packet));
    assert_eq!(expected.0, 200_000);
    assert_eq!(owned_result, expected, "owned reader differs from the frozen baseline");
    assert_eq!(reused_result, expected, "reused reader differs from the frozen baseline");
    println!(
        "validated Ogg baseline/owned/reused: {} packets, checksum {:016x}",
        expected.0, expected.1
    );
    measure("ogg_legacy_0_4_packets", 200_000, || {
        let mut reader = legacy_ogg::OggOpusReader::new(ogg.as_slice());
        let mut count = 0;
        while let Some(packet) = reader.next_packet().unwrap() {
            black_box(&packet);
            count += 1;
        }
        assert_eq!(count, 200_000);
    });
    measure("ogg_owned_packets", 200_000, || {
        let mut reader = OggOpusReader::new(ogg.as_slice());
        let mut count = 0;
        while let Some(packet) = reader.next_packet().unwrap() {
            black_box(&packet);
            count += 1;
        }
        assert_eq!(count, 200_000);
    });
    measure("ogg_reused_packets", 200_000, || {
        let mut reader = OggOpusReader::new(ogg.as_slice());
        let mut packet = Vec::new();
        let mut count = 0;
        while reader.next_packet_into(&mut packet).unwrap() {
            black_box(&packet);
            count += 1;
        }
        assert_eq!(count, 200_000);
    });
    let wav = wav_fixture();
    measure("wav_48khz_stereo_opus_128k_complexity10", 500, || {
        let engine = Engine { ffmpeg: None, in_process: true, bitrate: 128_000, complexity: 10 };
        let track = Track::new("bench".into(), TrackInfo::default());
        let mut frames = engine
            .start(
                &track,
                Body::new(Box::new(std::io::Cursor::new(wav.clone()))),
                0,
                100,
                &Default::default(),
                Preference::Auto,
            )
            .unwrap();
        let mut packet = Vec::new();
        let mut count = 0;
        while frames.next_frame(&mut packet).unwrap() {
            black_box(&packet);
            count += 1;
        }
        assert_eq!(count, 500);
    });
}
