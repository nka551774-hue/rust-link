//! Lock-free system and process CPU/memory sampling.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Machine and process CPU loads (in `0.0..=1.0`) plus resident/total memory.
///
/// Readers (`values`, `memory`, `cores`) are lock-free so HTTP and WebSocket
/// ticks never contend with the background OS sampler.
pub struct Cpu {
    sampler: Mutex<(System, bool)>,
    system_load: AtomicU64,
    process_load: AtomicU64,
    available: AtomicBool,
    memory_used: AtomicU64,
    memory_total: AtomicU64,
    pid: Pid,
    cores: usize,
}

impl Cpu {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_memory();
        let total = system.total_memory();
        Cpu {
            sampler: Mutex::new((system, false)),
            system_load: AtomicU64::new(0.0f64.to_bits()),
            process_load: AtomicU64::new(0.0f64.to_bits()),
            available: AtomicBool::new(false),
            memory_used: AtomicU64::new(0),
            memory_total: AtomicU64::new(total),
            pid: Pid::from_u32(std::process::id()),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
        }
    }

    /// Takes one OS sample; CPU loads become available starting on the second sample.
    pub fn sample(&self) {
        let mut sampler = self.sampler.lock().unwrap_or_else(|e| e.into_inner());
        let (system, sampled) = &mut *sampler;
        system.refresh_cpu_usage();
        let pid = self.pid;
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let cores = self.cores as f64;
        let system_load = f64::from(system.global_cpu_usage()) / 100.0;
        let (process_load, used) = system
            .process(pid)
            .map_or((0.0, 0), |proc| (f64::from(proc.cpu_usage()) / 100.0 / cores, proc.memory()));
        let was_sampled = *sampled;
        *sampled = true;
        drop(sampler);

        self.memory_used.store(used, Ordering::Relaxed);
        if was_sampled {
            self.system_load.store(clamp(system_load).to_bits(), Ordering::Relaxed);
            self.process_load.store(clamp(process_load).to_bits(), Ordering::Relaxed);
            self.available.store(true, Ordering::Release);
        }
    }

    pub fn values(&self) -> (f64, f64, bool) {
        let available = self.available.load(Ordering::Acquire);
        let sys = f64::from_bits(self.system_load.load(Ordering::Relaxed));
        let proc = f64::from_bits(self.process_load.load(Ordering::Relaxed));
        (sys, proc, available)
    }

    pub fn cores(&self) -> usize {
        self.cores
    }

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
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}
