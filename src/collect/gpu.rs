//! GPU: NVML (uso, clocks, potência, fans, PCIe, encoder/decoder, P-state, throttling) e a
//! VRAM do WDDM pelo PDH `\GPU Adapter Memory(*)` (igual ao Gerenciador de Tarefas).
//!
//! O adaptador do PDH é identificado pelo LUID (`luid_0x<alto>_0x<baixo>_phys_N`), obtido do
//! DXGI casando fabricante, dispositivo e subsistema PCI com o NVML. PCIe TX/RX custam ~23 ms
//! cada: ficam na thread lenta (`GpuSlowCollector`).

use nvml_wrapper::Device;
use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{
    Clock, PcieUtilCounter, TemperatureSensor, TemperatureThreshold,
};
use nvml_wrapper::error::NvmlError;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

use std::collections::HashMap;
use std::time::Instant;

use super::gpu_engine::EngineCollector;
use super::vram::{ProcMem, VramCollector};
use crate::model::{Fan, Gpu, GpuProc, GpuSlow, Metric, Missing, fail, neutral};
use crate::pdh::{self, Query, Slot};
use crate::win;

/// Adaptador DXGI: (fabricante, dispositivo, subsistema, LUID alto, LUID baixo, compartilhada).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Adapter {
    vendor: u32,
    device: u32,
    subsys: u32,
    luid_high: i32,
    luid_low: u32,
    shared_total: u64,
}

fn dxgi_adapters() -> Result<Vec<Adapter>, String> {
    // SAFETY: cria só a fábrica (nenhum dispositivo D3D, nenhum driver de modo usuário).
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
    let mut out = Vec::new();
    for i in 0.. {
        // SAFETY: fábrica válida; erro (DXGI_ERROR_NOT_FOUND) encerra a lista.
        let Ok(a) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: adaptador válido.
        if let Ok(d) = unsafe { a.GetDesc1() } {
            out.push(Adapter {
                vendor: d.VendorId,
                device: d.DeviceId,
                subsys: d.SubSysId,
                luid_high: d.AdapterLuid.HighPart,
                luid_low: d.AdapterLuid.LowPart,
                shared_total: d.SharedSystemMemory as u64,
            });
        }
    }
    Ok(out)
}

/// Adaptador DXGI da GPU do NVML (`pci_device_id` = dispositivo << 16 | fabricante). Vários
/// adaptadores iguais (mesmo modelo e placa) não têm como ser distinguidos: estado neutro.
fn match_adapter(adapters: &[Adapter], pci_device_id: u32, subsys: Option<u32>) -> Metric<Adapter> {
    let (vendor, device) = (pci_device_id & 0xFFFF, pci_device_id >> 16);
    let found: Vec<&Adapter> = adapters
        .iter()
        .filter(|a| {
            a.vendor == vendor && a.device == device && subsys.is_none_or(|s| s == a.subsys)
        })
        .collect();
    let mut luids: Vec<(i32, u32)> = found.iter().map(|a| (a.luid_high, a.luid_low)).collect();
    luids.sort_unstable();
    luids.dedup();
    match (found.first(), luids.len()) {
        (Some(a), 1) => Ok(**a),
        (None, _) => fail("NVML GPU not found in DXGI"),
        // Estado neutro do inventário: sem como distinguir placas idênticas.
        _ => neutral("several identical GPUs: NVML-DXGI matching pending"),
    }
}

/// Adaptador DXGI da GPU `index` do NVML (pelo PCI).
fn adapter_of(n: &Nvml, index: u32, adapters: &Result<Vec<Adapter>, String>) -> Metric<Adapter> {
    let d = n
        .device_by_index(index)
        .map_err(|e| Missing::Fail(format!("NVML: {e}")))?;
    let p = d
        .pci_info()
        .map_err(|e| Missing::Fail(format!("NVML pci_info: {e}")))?;
    let a = adapters.as_ref().map_err(|e| Missing::Fail(e.clone()))?;
    match_adapter(a, p.pci_device_id, p.pci_sub_system_id)
}

/// Um reset do driver (TDR), uma atualização dele ou uma eGPU reconectada trocam o LUID do
/// adaptador: as instâncias do PDH somem do prefixo guardado. A associação é refeita, no máximo
/// a cada `REMATCH_EVERY`.
const REMATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
pub(super) const NO_PDH_INSTANCE: &str = "adapter has no PDH instance";

fn lost_adapter<T>(m: &Metric<T>) -> bool {
    matches!(m, Err(Missing::Fail(c)) if c == NO_PDH_INSTANCE)
}

/// Prefixo da instância PDH de um LUID (comparado sem diferenciar maiúsculas).
fn luid_prefix(high: i32, low: u32) -> String {
    format!("luid_0x{:08x}_0x{low:08x}_phys_", high.cast_unsigned())
}

/// Soma das instâncias PDH de um adaptador.
fn sum_for(items: &[(String, f64)], prefix: &str) -> Option<f64> {
    let mut hit = false;
    let total = items
        .iter()
        .filter(|(n, _)| n.to_ascii_lowercase().starts_with(prefix))
        .inspect(|_| hit = true)
        .map(|(_, v)| *v)
        .sum();
    hit.then_some(total)
}

fn nv<T>(r: Result<T, NvmlError>) -> Metric<T> {
    r.or_else(|e| match e {
        NvmlError::NotSupported => fail("not supported by NVML"),
        e => fail(format!("NVML: {e}")),
    })
}

const THROTTLE_NAMES: [(ThrottleReasons, &str); 9] = [
    (ThrottleReasons::GPU_IDLE, "idle"),
    (
        ThrottleReasons::APPLICATIONS_CLOCKS_SETTING,
        "applications clocks",
    ),
    (ThrottleReasons::SW_POWER_CAP, "power cap"),
    (ThrottleReasons::HW_SLOWDOWN, "HW slowdown"),
    (ThrottleReasons::SYNC_BOOST, "sync boost"),
    (ThrottleReasons::SW_THERMAL_SLOWDOWN, "SW thermal"),
    (ThrottleReasons::HW_THERMAL_SLOWDOWN, "HW thermal"),
    (ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN, "HW power brake"),
    (ThrottleReasons::DISPLAY_CLOCK_SETTING, "display clock"),
];

fn throttle_names(r: ThrottleReasons) -> Vec<&'static str> {
    THROTTLE_NAMES
        .iter()
        .filter(|(f, _)| r.contains(*f))
        .map(|(_, n)| *n)
        .collect()
}

/// NVML único do processo, dividido entre as threads rápida e lenta (`Nvml` é Send + Sync): um
/// só `nvmlInit`/`nvmlShutdown`, sem corrida entre as threads no início e no fim.
pub type SharedNvml = std::sync::Arc<Result<Nvml, String>>;

/// NVML carregado de System32 por caminho absoluto.
pub fn init_nvml() -> Result<Nvml, String> {
    let lib = win::system_dir()
        .ok_or("GetSystemDirectoryW failed")?
        .join("nvml.dll");
    Nvml::builder()
        .lib_path(lib.as_os_str())
        .init()
        .map_err(|e| format!("NVML ({}): {e}", lib.display()))
}

/// Dados fixos de cada GPU.
struct Static {
    index: u32,
    name: Metric<String>,
    vbios: Metric<String>,
    total: Metric<u64>,
    /// Prefixo PDH e limite da memória compartilhada, ou o motivo da falha da associação.
    adapter: Metric<(String, u64)>,
}

pub struct GpuCollector {
    nvml: SharedNvml,
    driver: Metric<String>,
    gpus: Vec<Static>,
    query: Result<Query, String>,
    dedicated: Slot,
    shared: Slot,
    /// Última nova associação NVML-DXGI (ver `REMATCH_EVERY`).
    rematched: Instant,
}

impl GpuCollector {
    pub fn new(nvml: SharedNvml) -> Self {
        let query = Query::open();
        let add = |p: &str| match &query {
            Ok(q) => q.add(p),
            Err(e) => Err(e.clone()),
        };
        let dedicated = add(r"\GPU Adapter Memory(*)\Dedicated Usage");
        let shared = add(r"\GPU Adapter Memory(*)\Shared Usage");
        let adapters = dxgi_adapters();
        let (driver, gpus) = match nvml.as_ref() {
            Err(e) => (fail(e.clone()), Vec::new()),
            Ok(n) => {
                let count = n.device_count().unwrap_or(0);
                let gpus = (0..count)
                    .filter_map(|i| n.device_by_index(i).ok().map(|d| (i, d)))
                    .map(|(index, d)| {
                        let adapter = adapter_of(n, index, &adapters)
                            .map(|a| (luid_prefix(a.luid_high, a.luid_low), a.shared_total));
                        Static {
                            index,
                            name: nv(d.name()),
                            vbios: nv(d.vbios_version()),
                            total: nv(d.memory_info().map(|m| m.total)),
                            adapter,
                        }
                    })
                    .collect();
                (nv(n.sys_driver_version()), gpus)
            }
        };
        Self {
            nvml,
            driver,
            gpus,
            query,
            dedicated,
            shared,
            rematched: Instant::now(),
        }
    }

    /// Refaz a associação das GPUs que perderam a instância do PDH (LUID novo).
    fn rematch(&mut self, n: &Nvml) {
        self.rematched = Instant::now();
        let adapters = dxgi_adapters();
        for s in &mut self.gpus {
            s.adapter = adapter_of(n, s.index, &adapters)
                .map(|a| (luid_prefix(a.luid_high, a.luid_low), a.shared_total));
        }
    }

    pub fn read(&mut self) -> Metric<Vec<Gpu>> {
        let nvml = self
            .nvml
            .as_ref()
            .as_ref()
            .map_err(|e| Missing::Fail(e.clone()))?;
        if self.gpus.is_empty() {
            return fail("no NVIDIA GPU in NVML");
        }
        let sampled = match &self.query {
            Ok(q) => q.collect(),
            Err(e) => Err(e.clone()),
        };
        let arr = |slot: &Slot| {
            sampled
                .clone()
                .and_then(|()| pdh::read_array(&self.query, slot))
        };
        let dedicated = arr(&self.dedicated);
        let shared = arr(&self.shared);
        let out = self
            .gpus
            .iter()
            .map(|s| {
                let vram = |items: &Result<Vec<(String, f64)>, String>| -> Metric<u64> {
                    let (prefix, _) = s.adapter.as_ref().map_err(Clone::clone)?;
                    let items = items.as_ref().map_err(|e| Missing::Fail(e.clone()))?;
                    sum_for(items, prefix)
                        .map(|v| v as u64)
                        .map_or_else(|| fail(NO_PDH_INSTANCE), Ok)
                };
                let shared_total = s.adapter.as_ref().map_or(0, |a| a.1);
                let mut g = read_device(nvml, s.index);
                g.name.clone_from(&s.name);
                g.vbios.clone_from(&s.vbios);
                g.driver.clone_from(&self.driver);
                g.dedicated = vram(&dedicated).and_then(|u| Ok((u, s.total.clone()?)));
                g.shared = vram(&shared).map(|u| (u, shared_total));
                g
            })
            .collect::<Vec<Gpu>>();
        // LUID trocado (reset do driver), ou a associação anterior falhou (a GPU sumiu do DXGI
        // no meio do reset): associa de novo para a próxima coleta. GPUs idênticas (neutro) não.
        if (out.iter().any(|g| lost_adapter(&g.dedicated))
            || self
                .gpus
                .iter()
                .any(|s| matches!(s.adapter, Err(Missing::Fail(_)))))
            && self.rematched.elapsed() >= REMATCH_EVERY
        {
            let shared_nvml = self.nvml.clone();
            if let Ok(n) = shared_nvml.as_ref() {
                self.rematch(n);
            }
        }
        Ok(out)
    }
}

/// Leituras do NVML de uma GPU (os campos fixos e a VRAM do PDH são preenchidos por quem chama).
fn read_device(nvml: &Nvml, index: u32) -> Gpu {
    let d = match nvml.device_by_index(index) {
        Ok(d) => d,
        Err(e) => return failed_gpu(index, &format!("NVML device_by_index: {e}")),
    };
    // O máximo (clock) e o limite (potência) podem não ter suporte (GPUs móveis): o valor atual
    // continua valendo, com 0 = "não informado" (a interface mostra só o atual).
    let clock = |c: Clock| -> Metric<(u32, u32)> {
        Ok((nv(d.clock_info(c))?, d.max_clock_info(c).unwrap_or(0)))
    };
    let util = nv(d.utilization_rates());
    Gpu {
        index,
        name: fail(""),
        driver: fail(""),
        vbios: fail(""),
        usage: util.clone().map(|u| u.gpu),
        mem_activity: util.map(|u| u.memory),
        dedicated: fail(""),
        shared: fail(""),
        physical: nv(d.memory_info()).map(|m| [m.used, m.free, m.reserved, m.total]),
        temp: nv(d.temperature(TemperatureSensor::Gpu)),
        temp_slowdown: nv(d.temperature_threshold(TemperatureThreshold::Slowdown)),
        clock_graphics: clock(Clock::Graphics),
        clock_sm: clock(Clock::SM),
        clock_mem: clock(Clock::Memory),
        clock_video: clock(Clock::Video),
        power: nv(d.power_usage()).map(|p| {
            let l = d.enforced_power_limit().unwrap_or(0);
            (f64::from(p) / 1000.0, f64::from(l) / 1000.0)
        }),
        fans: read_fans(&d),
        pcie_link: nv(d.current_pcie_link_gen())
            .and_then(|g| Ok((g, nv(d.current_pcie_link_width())?))),
        encoder: nv(d.encoder_utilization()).map(|u| u.utilization),
        decoder: nv(d.decoder_utilization()).map(|u| u.utilization),
        pstate: nv(d.performance_state()).map(|p| p.as_c()),
        throttle: nv(d.current_throttle_reasons()).map(throttle_names),
    }
}

fn read_fans(d: &Device<'_>) -> Metric<Vec<Fan>> {
    let n = nv(d.num_fans())?;
    Ok((0..n)
        .map(|i| Fan {
            percent: nv(d.fan_speed(i)),
            rpm: match nv(d.fan_speed_rpm(i)) {
                Ok(0) => neutral("stopped"),
                r => r,
            },
        })
        .collect())
}

fn failed_gpu(index: u32, cause: &str) -> Gpu {
    let m = Missing::Fail(cause.to_string());
    Gpu {
        index,
        name: Err(m.clone()),
        driver: Err(m.clone()),
        vbios: Err(m.clone()),
        usage: Err(m.clone()),
        mem_activity: Err(m.clone()),
        dedicated: Err(m.clone()),
        shared: Err(m.clone()),
        physical: Err(m.clone()),
        temp: Err(m.clone()),
        temp_slowdown: Err(m.clone()),
        clock_graphics: Err(m.clone()),
        clock_sm: Err(m.clone()),
        clock_mem: Err(m.clone()),
        clock_video: Err(m.clone()),
        power: Err(m.clone()),
        fans: Err(m.clone()),
        pcie_link: Err(m.clone()),
        encoder: Err(m.clone()),
        decoder: Err(m.clone()),
        pstate: Err(m.clone()),
        throttle: Err(m),
    }
}

/// PCIe TX/RX (thread lenta; cada leitura leva ~23 ms). NVML próprio desta thread.
pub struct GpuSlowCollector {
    nvml: SharedNvml,
    /// Prefixo PDH (LUID) de cada GPU do NVML, pela mesma associação do coletor rápido.
    luids: Vec<Metric<String>>,
    vram: VramCollector,
    /// Engines por processo: consulta aberta só com a aba GPU visível.
    engines: EngineCollector,
    /// Última leitura de PCIe por GPU: (instante, TX, RX).
    pcie: HashMap<u32, (Instant, Metric<u64>, Metric<u64>)>,
    /// Última nova associação NVML-DXGI (ver `REMATCH_EVERY`).
    rematched: Instant,
}

/// Prefixo PDH (LUID) de cada GPU do NVML.
fn slow_luids(n: &Nvml) -> Vec<Metric<String>> {
    let adapters = dxgi_adapters();
    (0..n.device_count().unwrap_or(0))
        .map(|i| adapter_of(n, i, &adapters).map(|a| luid_prefix(a.luid_high, a.luid_low)))
        .collect()
}

/// Intervalo mínimo entre leituras de PCIe (o piso da thread lenta).
const PCIE_EVERY: std::time::Duration = std::time::Duration::from_millis(1900);

impl GpuSlowCollector {
    pub fn new(nvml: SharedNvml) -> Self {
        let luids = match nvml.as_ref() {
            Err(e) => vec![fail(e.clone())],
            Ok(n) => slow_luids(n),
        };
        Self {
            nvml,
            luids,
            rematched: Instant::now(),
            vram: VramCollector::new(),
            engines: EngineCollector::new(),
            pcie: HashMap::new(),
        }
    }

    /// Coleta as engines por processo (aba GPU). Devolve se já há taxa.
    pub fn collect_engines(&mut self) -> Result<bool, String> {
        self.engines.collect()
    }

    /// Fecha a consulta das engines (aba GPU fora de vista).
    pub fn close_engines(&mut self) {
        self.engines.close();
    }

    /// `detail`: aba GPU visível (lista de processos por GPU, com as engines coletadas aqui).
    /// `names`: PID → nome (da lista de processos, quando já coletada; senão o Toolhelp).
    pub fn read(
        &mut self,
        detail: bool,
        names: Option<&HashMap<u32, String>>,
    ) -> Metric<Vec<GpuSlow>> {
        let nvml = self
            .nvml
            .as_ref()
            .as_ref()
            .map_err(|e| Missing::Fail(e.clone()))?;
        let count = nv(nvml.device_count())?;
        if count == 0 {
            return fail("no NVIDIA GPU in NVML");
        }
        // Uma coleta PDH por ciclo, para todas as GPUs (a VRAM e o detalhe leem dela).
        let collected = self.vram.collect();
        let engine_rows: Option<Metric<Vec<(String, f64)>>> = detail.then(|| {
            match self.engines.collect() {
                Ok(true) => self.engines.rows().or_else(fail),
                // 1ª coleta depois de abrir: a taxa ainda não existe (não é "0%").
                Ok(false) => neutral("waiting for a second sample"),
                Err(e) => fail(e),
            }
        });
        let mut out = Vec::new();
        for index in 0..count {
            out.push(match nvml.device_by_index(index) {
                Err(e) => GpuSlow {
                    index,
                    pcie_tx: fail(format!("NVML: {e}")),
                    pcie_rx: fail(format!("NVML: {e}")),
                    vram: fail(format!("NVML: {e}")),
                    procs: detail.then(|| fail(format!("NVML: {e}"))),
                },
                // KB/s → bytes/s.
                Ok(d) => {
                    let luid = self
                        .luids
                        .get(index as usize)
                        .cloned()
                        .unwrap_or_else(|| fail("GPU without a DXGI adapter"));
                    let vram = match (&luid, nv(d.memory_info()), &collected) {
                        (Ok(l), Ok(m), Ok(())) => self.vram.read(l, m.total, names),
                        (Err(e), _, _) => Err(e.clone()),
                        (_, Err(e), _) => Err(e),
                        (_, _, Err(e)) => fail(e.clone()),
                    };
                    let procs = engine_rows.as_ref().map(|rows| {
                        let l = luid.clone()?;
                        collected.clone().or_else(fail)?;
                        let mem = self.vram.per_process_detail(&l).or_else(fail)?;
                        if names.is_none() {
                            // Sem a lista de processos: o cache cobre os PIDs desta lista.
                            let engine_pids: Vec<u32> = rows.as_ref().map_or_else(
                                |_| Vec::new(),
                                |r| super::gpu_engine::group(r, &l).into_keys().collect(),
                            );
                            self.vram
                                .refresh_names(mem.keys().copied().chain(engine_pids));
                        }
                        let list = gpu_procs(&mem, rows, &l, names.unwrap_or(&self.vram.names));
                        // Engines em falha e nenhum processo com VRAM: a lista vazia esconderia
                        // a causa.
                        match rows {
                            Err(e) if list.is_empty() => Err(e.clone()),
                            _ => Ok(list),
                        }
                    });
                    // PCIe custa ~23 ms por sentido: no máximo a cada `PCIE_EVERY`, mesmo quando
                    // abrir uma aba acorda a thread lenta antes da hora.
                    let fresh = self
                        .pcie
                        .get(&index)
                        .filter(|(t, ..)| t.elapsed() < PCIE_EVERY)
                        .map(|(_, tx, rx)| (tx.clone(), rx.clone()));
                    let (pcie_tx, pcie_rx) = fresh.unwrap_or_else(|| {
                        let tx = nv(d.pcie_throughput(PcieUtilCounter::Send))
                            .map(|k| u64::from(k) * 1024);
                        let rx = nv(d.pcie_throughput(PcieUtilCounter::Receive))
                            .map(|k| u64::from(k) * 1024);
                        self.pcie
                            .insert(index, (Instant::now(), tx.clone(), rx.clone()));
                        (tx, rx)
                    });
                    GpuSlow {
                        index,
                        pcie_tx,
                        pcie_rx,
                        vram,
                        procs,
                    }
                }
            });
        }
        // Como no coletor rápido: LUID trocado ou associação anterior em falha.
        if (out.iter().any(|g| lost_adapter(&g.vram))
            || self
                .luids
                .iter()
                .any(|l| matches!(l, Err(Missing::Fail(_)))))
            && self.rematched.elapsed() >= REMATCH_EVERY
        {
            self.rematched = Instant::now();
            self.luids = slow_luids(nvml);
        }
        Ok(out)
    }
}

/// Junta a VRAM por processo e as engines: um processo entra se tem VRAM ou engine em uso.
fn gpu_procs(
    mem: &HashMap<u32, ProcMem>,
    engine_rows: &Metric<Vec<(String, f64)>>,
    luid: &str,
    names: &HashMap<u32, String>,
) -> Vec<GpuProc> {
    let engines = engine_rows
        .as_ref()
        .map(|rows| super::gpu_engine::group(rows, luid));
    let mut pids: Vec<u32> = mem.keys().copied().collect();
    if let Ok(e) = &engines {
        pids.extend(e.keys().copied());
    }
    pids.sort_unstable();
    pids.dedup();
    pids.into_iter()
        .map(|pid| {
            let (local, non_local, committed) = mem.get(&pid).copied().unwrap_or_default();
            GpuProc {
                pid,
                name: names
                    .get(&pid)
                    .cloned()
                    .unwrap_or_else(|| format!("pid {pid}")),
                local,
                non_local,
                committed,
                engines: match &engines {
                    Ok(e) => Ok(e.get(&pid).cloned().unwrap_or_default()),
                    Err(err) => Err((*err).clone()),
                },
            }
        })
        .filter(|p| {
            p.local + p.non_local + p.committed > 0
                || p.engines
                    .as_ref()
                    .is_ok_and(|e| e.iter().any(|(_, v)| *v > 0.0))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(device: u32, subsys: u32, low: u32) -> Adapter {
        Adapter {
            vendor: 0x10DE,
            device,
            subsys,
            luid_high: 0,
            luid_low: low,
            shared_total: 0,
        }
    }

    #[test]
    fn associa_pelo_pci() {
        let list = [adapter(0x2C05, 1, 0xDBF2), adapter(0x1234, 1, 0x110D2)];
        let a = match_adapter(&list, (0x2C05 << 16) | 0x10DE, Some(1)).expect("achou");
        assert_eq!(a.luid_low, 0xDBF2);
        assert!(match_adapter(&list, (0x9999 << 16) | 0x10DE, None).is_err());
        let twins = [adapter(0x2C05, 1, 1), adapter(0x2C05, 1, 2)];
        assert!(matches!(
            match_adapter(&twins, (0x2C05 << 16) | 0x10DE, Some(1)),
            Err(Missing::Neutral(_))
        ));
        // Mesmo LUID repetido (o DXGI lista o adaptador mais de uma vez) é um adaptador só.
        let dup = [adapter(0x2C05, 1, 7), adapter(0x2C05, 1, 7)];
        let a = match_adapter(&dup, (0x2C05 << 16) | 0x10DE, None).expect("um adaptador");
        assert_eq!(a.luid_low, 7);
        // Dois LUIDs diferentes (com repetição fora de ordem) continuam ambíguos: neutro.
        let two = [
            adapter(0x2C05, 1, 7),
            adapter(0x2C05, 1, 8),
            adapter(0x2C05, 1, 7),
        ];
        assert!(matches!(
            match_adapter(&two, (0x2C05 << 16) | 0x10DE, None),
            Err(Missing::Neutral(_))
        ));
    }

    /// Só a falha "sem instância do PDH" (LUID trocado) refaz a associação.
    #[test]
    fn luid_perdido_so_pela_falha_do_pdh() {
        assert!(lost_adapter::<u64>(&fail(NO_PDH_INSTANCE)));
        assert!(!lost_adapter::<u64>(&fail("NVML: unknown error")));
        assert!(!lost_adapter::<u64>(&neutral("several identical GPUs")));
        assert!(!lost_adapter(&Ok(1u64)));
    }

    #[test]
    fn instancia_pdh_pelo_luid() {
        let p = luid_prefix(0, 0xDBF2);
        assert_eq!(p, "luid_0x00000000_0x0000dbf2_phys_");
        let items = vec![
            ("luid_0x00000000_0x0000DBF2_phys_0".to_string(), 100.0),
            ("luid_0x00000000_0x000110D2_phys_0".to_string(), 7.0),
        ];
        assert_eq!(sum_for(&items, &p), Some(100.0));
        assert_eq!(sum_for(&items, &luid_prefix(0, 1)), None);
    }

    #[test]
    fn nomes_de_throttling() {
        let r = ThrottleReasons::GPU_IDLE | ThrottleReasons::SW_POWER_CAP;
        assert_eq!(throttle_names(r), vec!["idle", "power cap"]);
        assert!(throttle_names(ThrottleReasons::NONE).is_empty());
    }
}
