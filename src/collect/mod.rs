//! Coletores. A thread rápida usa `FastCollectors`; a lenta, `SlowCollectors`. Cada coletor
//! roda isolado por `catch_unwind`: um pânico vira falha daquela fonte, sem derrubar as outras.

mod cpu;
mod disk;
mod disk_temp;
mod gpu;
mod memory;
mod net;
mod pawnio;
pub mod rtss;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};

use crate::model::{Fast, Metric, Slow, fail};

pub use rtss::RtssCollector;

/// Executa um coletor; pânico vira falha com a mensagem.
fn guarded<T>(name: &str, f: impl FnOnce() -> Metric<T>) -> Metric<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(ToString::to_string)
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        fail(format!("{name} collector panicked: {msg}"))
    })
}

/// Cria um coletor; pânico na criação vira `Err` (a fonte fica marcada com falha).
fn create<T>(name: &str, f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|_| format!("{name} collector panicked on startup"))
}

fn perf_info() -> Result<PERFORMANCE_INFORMATION, String> {
    let mut pi = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: estrutura local com o tamanho informado.
    unsafe { GetPerformanceInfo(&raw mut pi, pi.cb) }
        .map(|()| pi)
        .map_err(|e| format!("GetPerformanceInfo: {e}"))
}

/// Coletores da thread rápida (intervalo principal, padrão 1 s).
pub struct FastCollectors {
    cpu: Result<cpu::CpuCollector, String>,
    pawnio: Result<pawnio::PawnIoCollector, String>,
    memory: Result<memory::MemoryCollector, String>,
    disk: Result<disk::DiskCollector, String>,
    net: Result<net::NetCollector, String>,
    gpu: Result<gpu::GpuCollector, String>,
}

fn run<C, T>(
    name: &str,
    c: &mut Result<C, String>,
    f: impl FnOnce(&mut C) -> Metric<T>,
) -> Metric<T> {
    match c {
        Ok(c) => guarded(name, || f(c)),
        Err(e) => fail(e.clone()),
    }
}

impl FastCollectors {
    pub fn new() -> Self {
        Self {
            cpu: create("CPU", cpu::CpuCollector::new),
            pawnio: create("CPU sensor", pawnio::PawnIoCollector::new),
            memory: create("memory", memory::MemoryCollector::new),
            disk: create("disk", disk::DiskCollector::new),
            net: create("network", net::NetCollector::new),
            gpu: create("GPU", gpu::GpuCollector::new),
        }
    }

    pub fn collect(&mut self) -> Fast {
        let start = Instant::now();
        let pi = perf_info();
        let cpu = run("CPU", &mut self.cpu, |c| Ok(c.read(pi.as_ref())));
        let cpu_sensors = run(
            "CPU sensor",
            &mut self.pawnio,
            pawnio::PawnIoCollector::read,
        );
        let memory = run("memory", &mut self.memory, |c| Ok(c.read(pi.as_ref())));
        let disks = run("disk", &mut self.disk, disk::DiskCollector::read);
        let nics = run("network", &mut self.net, net::NetCollector::read);
        let gpus = run("GPU", &mut self.gpu, gpu::GpuCollector::read);
        Fast {
            cpu,
            cpu_sensors,
            memory,
            disks,
            nics,
            gpus,
            took: start.elapsed(),
        }
    }
}

/// Coletores da thread lenta (leituras caras).
pub struct SlowCollectors {
    gpu: Result<gpu::GpuSlowCollector, String>,
    disk_temp: Result<disk_temp::DiskTempCollector, String>,
}

impl SlowCollectors {
    pub fn new() -> Self {
        Self {
            gpu: create("GPU PCIe", gpu::GpuSlowCollector::new),
            disk_temp: create("disk temperature", disk_temp::DiskTempCollector::new),
        }
    }

    pub fn collect(&mut self) -> Slow {
        let start = Instant::now();
        let gpus = run("GPU PCIe", &mut self.gpu, gpu::GpuSlowCollector::read);
        let disk_temps = run(
            "disk temperature",
            &mut self.disk_temp,
            disk_temp::DiskTempCollector::read,
        );
        Slow {
            gpus,
            disk_temps,
            took: start.elapsed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panico_vira_falha() {
        let r: Metric<u32> = guarded("test", || panic!("boom"));
        assert_eq!(r, fail("test collector panicked: boom"));
        assert_eq!(guarded("test", || Ok(3)), Ok(3));
    }
}
