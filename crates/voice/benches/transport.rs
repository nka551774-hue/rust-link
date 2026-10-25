//! Compare per-packet key setup with a cached AES schedule. XChaCha retains
//! per-packet construction because it has no reusable expanded key schedule.
#![allow(dead_code, unused_imports)]
#[path = "support/legacy_transport.rs"]
mod legacy;
#[path = "../../../benchmarks/measure.rs"]
mod measure;
#[path = "../src/transport.rs"]
mod transport;
pub use rustlink_voice::VoiceError;
use std::hint::black_box;

#[global_allocator]
static ALLOCATOR: measure::CountingAllocator = measure::CountingAllocator;

fn main() {
    let payload = [0x5a; 336];
    let key = [42; 32];
    let header = transport::header(7, 960, 12345);
    for mode in
        [transport::Mode::AeadAes256GcmRtpSize, transport::Mode::AeadXChaCha20Poly1305RtpSize]
    {
        let old_mode = legacy::Mode::parse(mode.name()).unwrap();
        let cipher = transport::PacketCipher::new(mode, &key);
        let mut old = Vec::with_capacity(1500);
        let mut new = Vec::with_capacity(1500);
        legacy::seal_into(&mut old, old_mode, &key, &header, 0, &payload).unwrap();
        cipher.seal_into(&mut new, &header, 0, &payload).unwrap();
        assert_eq!(old, new);
        let mut counter = 1;
        measure::measure(&format!("{} 0.4.0", mode.name()), 200_000, || {
            legacy::seal_into(
                &mut old,
                old_mode,
                black_box(&key),
                &header,
                counter,
                black_box(&payload),
            )
            .unwrap();
            counter += 1;
            black_box(&old);
        });
        let mut counter = 1;
        measure::measure(&format!("{} current", mode.name()), 200_000, || {
            cipher.seal_into(&mut new, &header, counter, black_box(&payload)).unwrap();
            counter += 1;
            black_box(&new);
        });
    }
}
