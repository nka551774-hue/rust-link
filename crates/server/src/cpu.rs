//! Sampling how busy the process and the machine are.
//!
//! The stats route reports both loads, and a node that cannot read them says so
//! rather than reporting zero: the flag travels with the values, so a client
//! can tell "idle" from "not measured".

use std::sync::Mutex;

use sysinfo::{Pid, ProcessesToUpdate, System};

/// The machine load and the process load, each as a fraction of one, with a
/// flag that says whether a sample has been taken yet.
pub struct Cpu {
    inner: Mutex<Inner>,
}

struct Inner {
    system: System,
    sampled: bool,
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
            inner: Mutex::new(Inner {
                system: System::new(),
                sampled: false,
                system_load: 0.0,
                process_load: 0.0,
                available: false,
                memory_used: 0,
                memory_total: 0,
            }),
        }
    }

    /// Takes one sample.
    ///
    /// A load is the difference between two moments, so the first sample only
    /// establishes the baseline and reports nothing: the values stay
    /// unavailable until there is a second one to compare against.
    pub fn sample(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.system.refresh_cpu_usage();
        inner.system.refresh_memory();
        let pid = Pid::from_u32(std::process::id());
        inner.system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
        let system_load = inner.system.global_cpu_usage() as f64 / 100.0;
        let process_load = inner
            .system
            .process(pid)
            .map(|process| process.cpu_usage() as f64 / 100.0 / cores)
            .unwrap_or(0.0);
        if inner.sampled {
            inner.system_load = clamp(system_load);
            inner.process_load = clamp(process_load);
            inner.available = true;
        }
        inner.sampled = true;
        let used = inner.system.process(pid).map(|process| process.memory()).unwrap_or(0);
        let total = inner.system.total_memory();
        inner.memory_used = used;
        inner.memory_total = total;
    }

    /// The two loads and whether they mean anything yet.
    pub fn values(&self) -> (f64, f64, bool) {
        let inner = self.inner.lock().unwrap();
        (inner.system_load, inner.process_load, inner.available)
    }

    /// The memory this process holds and the machine has, as of the last sample.
    ///
    /// A Rust node has no equivalent of the heap statistics the reference
    /// implementation reports, so what a client reads is the resident size of
    /// the process and the total it is competing for.
    pub fn memory(&self) -> (u64, u64) {
        let inner = self.inner.lock().unwrap();
        (inner.memory_used, inner.memory_total)
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
