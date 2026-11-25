//! Sampling how busy the process and the machine are.
//!
//! The stats route reports both loads, and a node that cannot read them says so
//! rather than reporting zero: the flag travels with the values, so a client
//! can tell "idle" from "not measured".

use std::sync::Mutex;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// The machine load and the process load, each as a fraction of one, with a
/// flag that says whether a sample has been taken yet.
pub struct Cpu {
    /// Held only by the background `spawn_blocking` sampler so slow OS table
    /// refreshes never block HTTP or WebSocket readers.
    sampler: Mutex<(System, bool)>,
    snapshot: Mutex<Snapshot>,
    /// Read once: on Linux each query walks the cgroup files.
    cores: usize,
}

#[derive(Clone, Copy, Default)]
struct Snapshot {
    system_load: f64,
    process_load: f64,
    available: bool,
    /// The memory this process holds and the machine's total, taken with the
    /// same sample as the loads so a stats request reads two numbers instead of
    /// walking the process table again.
    memory_used: u64,
    memory_total: u64,
}

impl Cpu {
    pub fn new() -> Self {
        Cpu {
            sampler: Mutex::new((System::new(), false)),
            snapshot: Mutex::new(Snapshot::default()),
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
        let pid = Pid::from_u32(std::process::id());
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let cores = self.cores as f64;
        let system_load = system.global_cpu_usage() as f64 / 100.0;
        let process_load = system
            .process(pid)
            .map(|process| process.cpu_usage() as f64 / 100.0 / cores)
            .unwrap_or(0.0);
        let used = system.process(pid).map(|process| process.memory()).unwrap_or(0);
        let total = system.total_memory();
        let was_sampled = *sampled;
        *sampled = true;
        drop(sampler);

        let mut snapshot = self.snapshot.lock().unwrap();
        if was_sampled {
            snapshot.system_load = clamp(system_load);
            snapshot.process_load = clamp(process_load);
            snapshot.available = true;
        }
        snapshot.memory_used = used;
        snapshot.memory_total = total;
    }

    /// The two loads and whether they mean anything yet.
    pub fn values(&self) -> (f64, f64, bool) {
        let snapshot = *self.snapshot.lock().unwrap();
        (snapshot.system_load, snapshot.process_load, snapshot.available)
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
        let snapshot = *self.snapshot.lock().unwrap();
        (snapshot.memory_used, snapshot.memory_total)
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
