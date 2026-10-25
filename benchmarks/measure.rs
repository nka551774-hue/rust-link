//! Stable, dependency-free microbenchmark helper. Run without other builds.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

pub struct CountingAllocator;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

pub fn measure(name: &str, iterations: u32, mut operation: impl FnMut()) {
    for _ in 0..1000 {
        operation();
    }
    let mut samples = [0.; 7];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        *sample = start.elapsed().as_nanos() as f64 / f64::from(iterations);
    }
    samples.sort_by(f64::total_cmp);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    for _ in 0..iterations {
        operation();
    }
    COUNTING.store(false, Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) as f64 / f64::from(iterations);
    println!("{name}: median {:.1} ns/op, range {:.1}..{:.1}, {allocations:.3} allocations/op ({iterations} ops x 7)", samples[3], samples[0], samples[6]);
}
