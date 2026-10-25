//! RFC 7845 page granules, including cropping and trimming across packets.

use std::io::Cursor;

use rustlink_audio::{decode::Decoder, AudioError, Body};

const SILENCE: &[u8] = &[0xf8, 0xff, 0xfe]; // 960 decoded samples.

fn page(flags: u8, sequence: u32, granule: u64, packets: &[&[u8]]) -> Vec<u8> {
    let mut lacing = Vec::new();
    let mut payload = Vec::new();
    for packet in packets {
        lacing.extend(std::iter::repeat_n(255, packet.len() / 255));
        lacing.push((packet.len() % 255) as u8);
        payload.extend_from_slice(packet);
    }
    raw_page(flags, sequence, granule, &lacing, &payload)
}

fn raw_page(flags: u8, sequence: u32, granule: u64, lacing: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = b"OggS\0".to_vec();
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    assert!(lacing.len() <= 255);
    out.push(lacing.len() as u8);
    out.extend_from_slice(lacing);
    out.extend_from_slice(payload);
    let mut crc = 0u32;
    for byte in &out {
        crc ^= (*byte as u32) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000_0000 != 0 { 0x04c1_1db7 } else { 0 };
        }
    }
    out[22..26].copy_from_slice(&crc.to_le_bytes());
    out
}

fn stream(skip: u16, pages: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    let mut out = page(2, 0, 0, &[&head]);
    out.extend(page(0, 1, 0, &[b"OpusTags\0\0\0\0\0\0\0\0"]));
    for item in pages {
        out.extend(item);
    }
    out
}

fn decode(bytes: Vec<u8>) -> Result<Vec<f32>, AudioError> {
    let mut decoder = Decoder::new(Body::new(Box::new(Cursor::new(bytes))))?;
    let mut samples = Vec::new();
    while decoder.decode(&mut samples)? {}
    Ok(samples)
}

#[test]
fn eos_can_trim_into_the_first_packet_on_its_page() {
    let samples = decode(stream(0, [page(4, 2, 100, &[SILENCE, SILENCE])])).unwrap();
    assert_eq!(samples.len(), 100 * 2);
}

#[test]
fn cropped_stream_uses_its_nonzero_initial_granule() {
    let samples =
        decode(stream(312, [page(0, 2, 10_960, &[SILENCE]), page(4, 3, 11_500, &[SILENCE])]))
            .unwrap();
    assert_eq!(samples.len(), (1500 - 312) * 2);
}

#[test]
fn ordinary_pages_preserve_preskip_and_trim_the_whole_final_page() {
    let samples = decode(stream(
        312,
        [
            page(0, 2, 1920, &[SILENCE, SILENCE]),
            page(0, 3, 2880, &[SILENCE]),
            page(4, 4, 3000, &[SILENCE, SILENCE]),
        ],
    ))
    .unwrap();
    assert_eq!(samples.len(), (3000 - 312) * 2);
}

#[test]
fn a_single_eos_page_can_also_have_a_nonzero_initial_granule() {
    let samples = decode(stream(312, [page(4, 2, 11_920, &[SILENCE, SILENCE])])).unwrap();
    assert_eq!(samples.len(), (1920 - 312) * 2);
}

#[test]
fn invalid_initial_granules_are_rejected_while_opening() {
    for bytes in [
        stream(0, [page(0, 2, 100, &[SILENCE])]),
        stream(312, [page(4, 2, 100, &[SILENCE])]),
        stream(0, [page(0, 2, u64::MAX, &[SILENCE])]),
    ] {
        assert!(Decoder::new(Body::new(Box::new(Cursor::new(bytes)))).is_err());
    }
}

#[test]
fn backwards_and_overlong_final_granules_are_rejected() {
    for end in [959, 1921, u64::MAX] {
        assert!(
            decode(stream(0, [page(0, 2, 960, &[SILENCE]), page(4, 3, end, &[SILENCE]),])).is_err()
        );
    }
}

#[test]
fn a_truncated_final_page_is_not_published() {
    let mut last = page(4, 3, 1500, &[SILENCE]);
    last.pop();
    let mut decoder = Decoder::new(Body::new(Box::new(Cursor::new(stream(
        0,
        [page(0, 2, 960, &[SILENCE]), last],
    )))))
    .unwrap();
    let mut samples = Vec::new();
    assert!(decoder.decode(&mut samples).unwrap());
    assert_eq!(samples.len(), 960 * 2);
    assert!(decoder.decode(&mut samples).is_err());
    assert_eq!(samples.len(), 960 * 2);
}

#[test]
fn page_granule_precedes_a_trailing_packet_continuation() {
    // One 20 ms silence packet with 294 bytes of valid Opus padding.
    let mut padded = vec![0xfb, 0x41, 255, 40, 0xff, 0xfe];
    padded.resize(300, 0);
    let mut first = SILENCE.to_vec();
    first.extend_from_slice(&padded[..255]);
    let samples = decode(stream(
        0,
        [raw_page(0, 2, 960, &[3, 255], &first), raw_page(5, 3, 1500, &[45], &padded[255..])],
    ))
    .unwrap();
    assert_eq!(samples.len(), 1500 * 2);
}

#[test]
fn first_audio_packet_can_span_pages_without_completed_packets() {
    let mut padded = vec![0xfb, 0x41, 255, 40, 0xff, 0xfe];
    padded.resize(300, 0);
    let samples = decode(stream(
        312,
        [
            raw_page(0, 2, u64::MAX, &[255], &padded[..255]),
            raw_page(5, 3, 800, &[45], &padded[255..]),
        ],
    ))
    .unwrap();
    assert_eq!(samples.len(), (800 - 312) * 2);
}

#[test]
fn a_final_page_with_an_unfinished_packet_is_rejected() {
    let mut payload = SILENCE.to_vec();
    payload.extend_from_slice(&[0; 255]);
    assert!(Decoder::new(Body::new(Box::new(Cursor::new(stream(
        0,
        [raw_page(4, 2, 100, &[3, 255], &payload),]
    )))))
    .is_err());
}

#[test]
fn opening_only_buffers_one_audio_page() {
    use std::io::{self, Read};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct CountingReader {
        bytes: Cursor<Vec<u8>>,
        read: Arc<AtomicUsize>,
    }
    impl Read for CountingReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let count = self.bytes.read(out)?;
            self.read.fetch_add(count, Ordering::Relaxed);
            Ok(count)
        }
    }
    let bytes = stream(
        0,
        (2..10_002).map(|sequence| page(0, sequence, u64::from(sequence - 1) * 960, &[SILENCE])),
    );
    let read = Arc::new(AtomicUsize::new(0));
    let mut decoder = Decoder::new(Body::new(Box::new(CountingReader {
        bytes: Cursor::new(bytes),
        read: read.clone(),
    })))
    .unwrap();
    assert!(decoder.decode(&mut Vec::new()).unwrap());
    assert!(read.load(Ordering::Relaxed) <= 32 * 1024);
}
