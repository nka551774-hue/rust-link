//! Sampling how busy the process and the machine are.
//!
//! The stats route reports both loads, and a node that cannot read them says so
//! rather than reporting zero: the flag travels with the values, so a client
//! can tell "idle" from "not measured".

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// The machine load and the process load, each as a fraction of one, with a
/// flag that says whether a sample has been taken yet.
///
/// Readers (`values`, `memory`, `cores`) are completely lock-free so HTTP
/// `/stats`, `/info`, `/metrics` and WebSocket ticks never contend with the
/// background OS sampler.
pub struct Cpu {
    /// Held only by the background `spawn_blocking` sampler so slow OS table
    /// refreshes never block HTTP or WebSocket readers.
    sampler: Mutex<(System, bool)>,
    system_load: AtomicU64,
    process_load: AtomicU64,
    available: AtomicBool,
    memory_used: AtomicU64,
    memory_total: AtomicU64,
    pid: Pid,
    /// Read once: on Linux each query walks the cgroup files.
    cores: usize,
}

impl Cpu {
    pub fn new() -> Self {
        Cpu {
            sampler: Mutex::new((System::new(), false)),
            system_load: AtomicU64::new(0.0f64.to_bits()),
            process_load: AtomicU64::new(0.0f64.to_bits()),
            available: AtomicBool::new(false),
            memory_used: AtomicU64::new(0),
            memory_total: AtomicU64::new(0),
            pid: Pid::from_u32(std::process::id()),
            cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        }
    }

    /// Takes one sample.
    ///
    /// A load is the difference between two moments, so the first sample only
    /// establishes the baseline and reports nothing: the values stay
    /// unavailable until there is a second one to compare against.
    pub fn sample(&self) {
        let mut sampler = self.sampler.lock().unwrap();
        let (system, sampled) = &mut *sampler;
        system.refresh_cpu_usage();
        system.refresh_memory();
        let pid = self.pid;
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let cores = self.cores as f64;
        let system_load = f64::from(system.global_cpu_usage()) / 100.0;
        let (process_load, used) = system.process(pid).map_or((0.0, 0), |process| {
            (f64::from(process.cpu_usage()) / 100.0 / cores, process.memory())
        });
        let total = system.total_memory();
        let was_sampled = *sampled;
        *sampled = true;
        drop(sampler);

        self.memory_used.store(used, Ordering::Relaxed);
        self.memory_total.store(total, Ordering::Relaxed);
        if was_sampled {
            self.system_load.store(clamp(system_load).to_bits(), Ordering::Relaxed);
            self.process_load.store(clamp(process_load).to_bits(), Ordering::Relaxed);
            self.available.store(true, Ordering::Release);
        }
    }

    /// The two loads and whether they mean anything yet.
    pub fn values(&self) -> (f64, f64, bool) {
        let available = self.available.load(Ordering::Acquire);
        let sys = f64::from_bits(self.system_load.load(Ordering::Relaxed));
        let proc = f64::from_bits(self.process_load.load(Ordering::Relaxed));
        (sys, proc, available)
    }

    /// The logical cores the process may run on.
    pub fn cores(&self) -> usize {
        self.cores
    }

    /// The memory this process holds and the machine has, as of the last sample.
    ///
    /// A Rust node has no equivalent of the heap statistics the reference
    /// implementation reports, so what a client reads is the resident size of
    /// the process and the total it is competing for.
    pub fn memory(&self) -> (u64, u64) {
        (self.memory_used.load(Ordering::Relaxed), self.memory_total.load(Ordering::Relaxed))
    }
}

impl Default for Cpu {
    fn default() -> Self {
        Cpu::new()
    }
}

fn clamp(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}
