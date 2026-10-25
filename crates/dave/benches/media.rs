//! Compare the 0.4.0 sender with the current gateway's reusable-buffer path.
#![allow(dead_code, unused_imports)]
#[path = "support/legacy_media.rs"]
mod legacy;
#[path = "../../../benchmarks/measure.rs"]
mod measure;
#[path = "../src/media.rs"]
mod media;
pub use rustlink_dave::{Error, Result};
use std::hint::black_box;

#[global_allocator]
static ALLOCATOR: measure::CountingAllocator = measure::CountingAllocator;

fn main() {
    let opus = [0x5a; 320]; // 20 ms at 128 kbit/s
    let mut legacy = legacy::MediaSender::new(vec![42; 16]);
    let mut current = media::MediaSender::new(vec![42; 16]);
    assert_eq!(legacy.encrypt(&opus).unwrap(), current.encrypt(&opus).unwrap());
    let mut frame = Vec::with_capacity(1500);
    measure::measure("DAVE 0.4.0", 200_000, || {
        black_box(legacy.encrypt(black_box(&opus)).unwrap());
    });
    measure::measure("DAVE reusable", 200_000, || {
        frame.clear();
        frame.extend_from_slice(black_box(&opus));
        current.encrypt_in_place(&mut frame).unwrap();
        black_box(&frame);
    });
}
