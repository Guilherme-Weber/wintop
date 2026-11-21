//! Coletores. A thread rápida usa `FastCollectors`; a lenta, `SlowCollectors`. Cada coletor
//! roda isolado por `catch_unwind`: um pânico vira falha daquela fonte, sem derrubar as outras.

mod cpu;
mod disk;
mod disk_temp;
mod gpu;
mod gpu_engine;
mod memory;
pub mod modules;
mod net;
mod own;
mod pawnio;
#[cfg(test)]
mod probe_tests;
pub mod procs;
pub mod rtss;
mod vram;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};

use std::sync::atomic::AtomicBool;

use crate::model::{Fast, Metric, Slow, View, fail};

pub use gpu::SharedNvml;

/// NVML do processo (um só para as duas threads).
pub fn shared_nvml() -> SharedNvml {
    std::sync::Arc::new(gpu::init_nvml())
}

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
    own: Result<own::OwnCollector, String>,
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
    pub fn new(nvml: SharedNvml) -> Self {
        Self {
            cpu: create("CPU", cpu::CpuCollector::new),
            pawnio: create("CPU sensor", pawnio::PawnIoCollector::new),
            memory: create("memory", memory::MemoryCollector::new),
            disk: create("disk", disk::DiskCollector::new),
            net: create("network", net::NetCollector::new),
            gpu: create("GPU", || gpu::GpuCollector::new(nvml)),
            own: create("wintop usage", own::OwnCollector::new),
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
        let own = run("wintop usage", &mut self.own, own::OwnCollector::read);
        Fast {
            cpu,
            cpu_sensors,
            memory,
            disks,
            nics,
            gpus,
            own,
            took: start.elapsed(),
        }
    }
}

/// Coletores da thread lenta (leituras caras e as das abas Processos e GPU).
pub struct SlowCollectors {
    gpu: Result<gpu::GpuSlowCollector, String>,
    disk_temp: Result<disk_temp::DiskTempCollector, String>,
    procs: procs::ProcCollector,
    /// Engines por processo já em coleta (aba GPU visível no ciclo anterior).
    engines_open: bool,
    /// Ciclos seguidos com a aba GPU fora de vista.
    gpu_hidden: u32,
}

/// Espera entre a 1ª leitura das taxas (aba recém-aberta) e a publicada.
const TAB_WARMUP: std::time::Duration = std::time::Duration::from_millis(300);

/// Dorme `d` em fatias, desistindo se a parada for pedida. `true` = parar.
fn nap(d: std::time::Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20).min(end - Instant::now()));
    }
    stop.load(std::sync::atomic::Ordering::Relaxed)
}

impl SlowCollectors {
    pub fn new(nvml: SharedNvml) -> Self {
        Self {
            gpu: create("GPU PCIe", || gpu::GpuSlowCollector::new(nvml)),
            disk_temp: create("disk temperature", disk_temp::DiskTempCollector::new),
            procs: procs::ProcCollector::new(),
            engines_open: false,
            gpu_hidden: 0,
        }
    }

    /// `stop`: parada pedida (a leitura dos discos, lenta, desiste entre um disco e outro).
    /// `view`: abas visíveis; processos e engines da GPU só são coletados com elas à vista
    /// (ao abrir, uma leitura de aquecimento e ~300 ms de espera: as taxas precisam de duas).
    pub fn collect(&mut self, stop: &AtomicBool, view: View) -> Slow {
        let start = Instant::now();
        let want_procs = view.procs || view.gpu;
        if !want_procs {
            self.procs.reset();
        }
        // Fora da aba GPU por um ciclo inteiro: a consulta fecha. Voltar antes disso não reabre
        // a consulta nem repete o aquecimento.
        if view.gpu {
            self.gpu_hidden = 0;
        } else if self.engines_open {
            self.gpu_hidden += 1;
            if self.gpu_hidden >= 2 {
                if let Ok(g) = &mut self.gpu {
                    g.close_engines();
                }
                self.engines_open = false;
            }
        }
        // Aquecimento das taxas que acabaram de entrar em vista.
        let mut warm = false;
        if want_procs && !self.procs.primed() {
            let _ = guarded("process", || self.procs.read());
            warm = true;
        }
        if view.gpu
            && !self.engines_open
            && let Ok(g) = &mut self.gpu
        {
            let _ = catch_unwind(AssertUnwindSafe(|| g.collect_engines()));
            self.engines_open = true;
            warm = true;
        }
        if warm && nap(TAB_WARMUP, stop) {
            return Slow {
                gpus: fail("stopping"),
                disk_temps: fail("stopping"),
                procs: None,
                took: start.elapsed(),
            };
        }
        // Aba trocada entre Processos e GPU logo depois de uma leitura: a taxa precisa de uma
        // janela mínima (senão o tempo do sistema nem avançou um tick e a CPU sai 0%).
        if want_procs
            && let Some(left) = TAB_WARMUP.checked_sub(self.procs.since_last())
            && nap(left, stop)
        {
            return Slow {
                gpus: fail("stopping"),
                disk_temps: fail("stopping"),
                procs: None,
                took: start.elapsed(),
            };
        }
        let procs = want_procs.then(|| guarded("process", || self.procs.read()));
        // Nomes dos processos: da lista recém-lida (aba aberta); senão o cache do coletor.
        let names: Option<std::collections::HashMap<u32, String>> = match &procs {
            Some(Ok(list)) => Some(list.iter().map(|p| (p.pid, p.name.clone())).collect()),
            _ => None,
        };
        let gpus = run("GPU PCIe", &mut self.gpu, |g| {
            g.read(view.gpu, names.as_ref())
        });
        let disk_temps = run("disk temperature", &mut self.disk_temp, |c| c.read(stop));
        Slow {
            gpus,
            disk_temps,
            procs,
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
