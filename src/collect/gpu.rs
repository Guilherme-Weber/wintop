//! GPU: NVML (uso, clocks, potência, fans, PCIe, encoder/decoder, P-state, throttling) e a
//! VRAM do WDDM pelo PDH `\GPU Adapter Memory(*)` (igual ao Gerenciador de Tarefas).
//!
//! O adaptador do PDH é identificado pelo LUID (`luid_0x<alto>_0x<baixo>_phys_N`), obtido do
//! DXGI casando fabricante, dispositivo e subsistema PCI com o NVML. PCIe TX/RX custam ~23 ms
//! cada: ficam na thread lenta (`GpuSlowCollector`).
//!
//! GPUs sem NVML (AMD, Intel, ou NVIDIA com o NVML em falha) entram depois das do NVML, só pelo
//! DXGI e pelo PDH: nome, driver, VRAM e o uso das engines (`\GPU Engine(*)`, a mesma conta do
//! Gerenciador de Tarefas). Essa consulta tem centenas de instâncias: só existe na thread rápida
//! quando há uma GPU assim.

use nvml_wrapper::Device;
use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{
    Clock, PcieUtilCounter, TemperatureSensor, TemperatureThreshold,
};
use nvml_wrapper::error::NvmlError;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIDevice, IDXGIFactory1,
};
use windows::core::Interface;

use std::collections::HashMap;
use std::time::Instant;

use super::gpu_engine::{AdapterUse, EngineCollector, adapter_use};
use super::vram::{ProcMem, VramCollector};
use crate::model::{Fan, Gpu, GpuProc, GpuSlow, Metric, Missing, VramBreakdown, fail, neutral};
use crate::pdh::{self, Query, Slot};
use crate::win;

const NVIDIA: u32 = 0x10DE;
/// Adaptadores da Microsoft (Basic Render, Remote Display): sem hardware próprio.
const MICROSOFT: u32 = 0x1414;
/// Campos só do NVML numa GPU de outro fabricante (estado neutro do inventário).
pub const NO_NVML: &str = "needs NVML (NVIDIA only)";
const NO_GPU: &str = "no GPU found (NVML and DXGI)";

/// Adaptador DXGI.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Adapter {
    vendor: u32,
    device: u32,
    subsys: u32,
    luid_high: i32,
    luid_low: u32,
    shared_total: u64,
    dedicated_total: u64,
    name: String,
    /// Versão do driver de modo usuário ("32.0.15.8180"), quando o DXGI informa.
    driver: Option<String>,
    /// Adaptador de software (WARP).
    software: bool,
}

/// Versão do driver pelo `CheckInterfaceSupport` (quatro partes de 16 bits).
fn driver_version(v: i64) -> String {
    let v = v.cast_unsigned();
    format!(
        "{}.{}.{}.{}",
        v >> 48,
        (v >> 32) & 0xFFFF,
        (v >> 16) & 0xFFFF,
        v & 0xFFFF
    )
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
            let len = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
            // SAFETY: adaptador válido; só consulta a versão do driver registrada.
            let driver = unsafe { a.CheckInterfaceSupport(&IDXGIDevice::IID) }
                .ok()
                .map(driver_version);
            out.push(Adapter {
                vendor: d.VendorId,
                device: d.DeviceId,
                subsys: d.SubSysId,
                luid_high: d.AdapterLuid.HighPart,
                luid_low: d.AdapterLuid.LowPart,
                shared_total: d.SharedSystemMemory as u64,
                dedicated_total: d.DedicatedVideoMemory as u64,
                name: String::from_utf16_lossy(&d.Description[..len])
                    .trim()
                    .to_string(),
                driver,
                software: d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0.cast_unsigned() != 0,
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
        (Some(a), 1) => Ok((*a).clone()),
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

/// Adaptadores de hardware que o NVML não cobre, com o motivo da falta do NVML. `nvml`: o
/// `pci_device_id` de cada GPU do NVML (`None` = PCI desconhecido: toda NVIDIA fica com o NVML),
/// ou a falha do NVML. Um LUID repetido no DXGI é um adaptador só.
fn dxgi_only(adapters: &[Adapter], nvml: Result<&[Option<u32>], &str>) -> Vec<(Adapter, Missing)> {
    let mut seen: Vec<(i32, u32)> = Vec::new();
    let mut out = Vec::new();
    for a in adapters {
        let luid = (a.luid_high, a.luid_low);
        if a.software || a.vendor == MICROSOFT || seen.contains(&luid) {
            continue;
        }
        seen.push(luid);
        let why = match (a.vendor, nvml) {
            (NVIDIA, Ok(ids)) => {
                let covered = ids
                    .iter()
                    .any(|id| id.is_none_or(|id| id & 0xFFFF == a.vendor && id >> 16 == a.device));
                if covered {
                    continue;
                }
                Missing::Fail("NVIDIA GPU not listed by NVML".into())
            }
            (NVIDIA, Err(e)) => Missing::Fail(e.to_string()),
            _ => Missing::Neutral(NO_NVML.into()),
        };
        out.push((a.clone(), why));
    }
    out
}

/// De onde vêm as leituras de uma GPU.
#[derive(Clone, Debug)]
enum Source {
    /// GPU do NVML (índice dele).
    Nvml(u32),
    /// Só DXGI e PDH, com o motivo da falta do NVML.
    Dxgi(Adapter, Missing),
}

/// GPUs do wintop, na mesma ordem nas threads rápida e lenta: as do NVML e depois as só do DXGI.
fn gpu_list(nvml: &Result<Nvml, String>) -> Vec<(Source, Metric<Adapter>)> {
    let adapters = dxgi_adapters();
    let mut out = Vec::new();
    let mut ids = Vec::new();
    if let Ok(n) = nvml {
        for i in 0..n.device_count().unwrap_or(0) {
            let Ok(d) = n.device_by_index(i) else {
                continue;
            };
            ids.push(d.pci_info().ok().map(|p| p.pci_device_id));
            out.push((Source::Nvml(i), adapter_of(n, i, &adapters)));
        }
    }
    if let Ok(list) = &adapters {
        let nv = nvml
            .as_ref()
            .map(|_| ids.as_slice())
            .map_err(String::as_str);
        for (a, why) in dxgi_only(list, nv) {
            out.push((Source::Dxgi(a.clone(), why), Ok(a)));
        }
    }
    out
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

fn prefix_of(a: &Adapter) -> String {
    luid_prefix(a.luid_high, a.luid_low)
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
    source: Source,
    name: Metric<String>,
    driver: Metric<String>,
    vbios: Metric<String>,
    total: Metric<u64>,
    /// Prefixo PDH e limite da memória compartilhada, ou o motivo da falha da associação.
    adapter: Metric<(String, u64)>,
}

fn statics(nvml: &Result<Nvml, String>) -> Vec<Static> {
    gpu_list(nvml)
        .into_iter()
        .map(|(source, adapter)| {
            let adapter = adapter.map(|a| (prefix_of(&a), a.shared_total));
            match (&source, nvml) {
                (Source::Nvml(i), Ok(n)) => {
                    let d = nv(n.device_by_index(*i));
                    let get = |f: &dyn Fn(&Device<'_>) -> Metric<String>| {
                        d.as_ref().map_err(Clone::clone).and_then(f)
                    };
                    Static {
                        name: get(&|d| nv(d.name())),
                        vbios: get(&|d| nv(d.vbios_version())),
                        total: d
                            .as_ref()
                            .map_err(Clone::clone)
                            .and_then(|d| nv(d.memory_info().map(|m| m.total))),
                        driver: nv(n.sys_driver_version()),
                        source,
                        adapter,
                    }
                }
                (Source::Dxgi(a, why), _) => Static {
                    name: Ok(a.name.clone()),
                    driver: a
                        .driver
                        .clone()
                        .map_or_else(|| fail("driver version not reported by DXGI"), Ok),
                    vbios: Err(why.clone()),
                    total: Ok(a.dedicated_total),
                    source,
                    adapter,
                },
                // Lista do NVML com o NVML em falha: não acontece (`gpu_list`).
                (Source::Nvml(_), Err(e)) => Static {
                    name: fail(e.clone()),
                    driver: fail(e.clone()),
                    vbios: fail(e.clone()),
                    total: fail(e.clone()),
                    source,
                    adapter,
                },
            }
        })
        .collect()
}

pub struct GpuCollector {
    nvml: SharedNvml,
    gpus: Vec<Static>,
    query: Result<Query, String>,
    dedicated: Slot,
    shared: Slot,
    /// `\GPU Engine(*)` do adaptador (uso sem NVML e a engine fora o 3D e o vídeo), e as
    /// coletas desde que entrou (taxa). ~540 instâncias, ~1 ms por coleta (medido na RTX 5070 Ti).
    engines: Option<Slot>,
    engine_samples: u32,
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
        let gpus = statics(nvml.as_ref());
        let mut c = Self {
            nvml,
            gpus,
            query,
            dedicated,
            shared,
            engines: None,
            engine_samples: 0,
            rematched: Instant::now(),
        };
        c.open_engines();
        c
    }

    /// Abre o contador das engines (uso sem NVML; a engine fora o 3D e o vídeo em todas).
    fn open_engines(&mut self) {
        if !self.gpus.is_empty() && self.engines.is_none() {
            self.engines = Some(match &self.query {
                Ok(q) => q.add(r"\GPU Engine(*)\Utilization Percentage"),
                Err(e) => Err(e.clone()),
            });
            self.engine_samples = 0;
        }
    }

    /// Refaz a lista e a associação (LUID novo, GPU que sumiu ou apareceu).
    fn rematch(&mut self) {
        self.rematched = Instant::now();
        self.gpus = statics(self.nvml.as_ref());
        self.open_engines();
    }

    pub fn read(&mut self) -> Metric<Vec<Gpu>> {
        // Lista vazia (GPU fora do DXGI no meio de um reset, eGPU ainda não conectada): procura
        // de novo, no mesmo ritmo da nova associação.
        if self.gpus.is_empty() && self.rematched.elapsed() >= REMATCH_EVERY {
            self.rematch();
        }
        if self.gpus.is_empty() {
            return match self.nvml.as_ref() {
                Err(e) => fail(e.clone()),
                Ok(_) => fail(NO_GPU),
            };
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
        if sampled.is_ok() && self.engines.is_some() {
            self.engine_samples = self.engine_samples.saturating_add(1);
        }
        let engines: Metric<Vec<(String, f64)>> = match &self.engines {
            None => fail("GPU engine query is closed"),
            Some(slot) => match (&sampled, slot) {
                (_, Err(e)) | (Err(e), _) => fail(e.clone()),
                // 1ª coleta: a taxa ainda não existe (não é "0%").
                _ if self.engine_samples < 2 => neutral("waiting for a second sample"),
                _ => pdh::read_array(&self.query, slot).or_else(fail),
            },
        };
        let out = self
            .gpus
            .iter()
            .enumerate()
            .map(|(pos, s)| {
                let vram = |items: &Result<Vec<(String, f64)>, String>| -> Metric<u64> {
                    let (prefix, _) = s.adapter.as_ref().map_err(Clone::clone)?;
                    let items = items.as_ref().map_err(|e| Missing::Fail(e.clone()))?;
                    sum_for(items, prefix)
                        .map(|v| v as u64)
                        .map_or_else(|| fail(NO_PDH_INSTANCE), Ok)
                };
                let shared_total = s.adapter.as_ref().map_or(0, |a| a.1);
                let used_vram = vram(&dedicated);
                let mut g = match (&s.source, self.nvml.as_ref()) {
                    (Source::Nvml(i), Ok(n)) => read_device(n, *i),
                    (Source::Nvml(i), Err(e)) => failed_gpu(*i, e),
                    // LUID trocado (reset): sem instância do prefixo, as engines dariam 0% falso.
                    (Source::Dxgi(_, why), _) if lost_adapter(&used_vram) => {
                        dxgi_gpu(why, &fail(NO_PDH_INSTANCE))
                    }
                    (Source::Dxgi(_, why), _) => {
                        let used = s.adapter.as_ref().map_err(Clone::clone).and_then(|(p, _)| {
                            Ok(adapter_use(engines.as_ref().map_err(Clone::clone)?, p))
                        });
                        dxgi_gpu(why, &used)
                    }
                };
                // Engine fora o 3D e o vídeo, pelo LUID (sem instância do prefixo: falha, nunca
                // um 0% falso).
                if matches!(s.source, Source::Nvml(_)) {
                    g.other_engine = match (&s.adapter, &engines) {
                        (Err(e), _) | (_, Err(e)) => Err(e.clone()),
                        _ if lost_adapter(&used_vram) => fail(NO_PDH_INSTANCE),
                        (Ok((p, _)), Ok(rows)) => other_engine(Ok(adapter_use(rows, p).other)),
                    };
                }
                g.index = pos as u32;
                g.name.clone_from(&s.name);
                g.vbios.clone_from(&s.vbios);
                g.driver.clone_from(&s.driver);
                g.dedicated = used_vram.and_then(|u| Ok((u, s.total.clone()?)));
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
            self.rematch();
        }
        Ok(out)
    }
}

/// Engine fora o 3D e o vídeo, arredondada (`None` = o adaptador não tem outra).
fn other_engine(m: Metric<Option<(String, f64)>>) -> Metric<(String, u32)> {
    m?.map_or_else(
        || neutral("no other engine reported"),
        |(t, v)| Ok((t, v.round() as u32)),
    )
}

/// GPU sem NVML: uso, encode e decode pelas engines; o resto com o motivo (`no_nvml`).
fn dxgi_gpu(why: &Missing, used: &Metric<AdapterUse>) -> Gpu {
    let pct = |v: f64| v.round() as u32;
    let video = |f: fn(&AdapterUse) -> Option<f64>, kind: &str| -> Metric<u32> {
        let u = used.as_ref().map_err(Clone::clone)?;
        f(u).map(pct)
            .map_or_else(|| neutral(format!("no video {kind} engine reported")), Ok)
    };
    let mut g = missing_gpu(0, why);
    g.no_nvml = Some(why.clone());
    g.usage = used.as_ref().map(|u| pct(u.usage)).map_err(Clone::clone);
    g.encoder = video(|u| u.encode, "encode");
    g.decoder = video(|u| u.decode, "decode");
    g.other_engine = other_engine(used.as_ref().map(|u| u.other.clone()).map_err(Clone::clone));
    g
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
        no_nvml: None,
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
        other_engine: fail(""),
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
    missing_gpu(index, &Missing::Fail(cause.to_string()))
}

/// GPU com todos os campos ausentes pelo mesmo motivo.
fn missing_gpu(index: u32, m: &Missing) -> Gpu {
    Gpu {
        index,
        no_nvml: None,
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
        other_engine: Err(m.clone()),
        pstate: Err(m.clone()),
        throttle: Err(m.clone()),
    }
}

/// PCIe TX/RX (thread lenta; cada leitura leva ~23 ms), VRAM por categoria e processos.
pub struct GpuSlowCollector {
    nvml: SharedNvml,
    /// Cada GPU (mesma lista e ordem do coletor rápido) e o prefixo PDH (LUID) dela.
    gpus: Vec<(Source, Metric<String>)>,
    vram: VramCollector,
    /// Engines por processo: consulta aberta só com a aba GPU visível.
    engines: EngineCollector,
    /// Última leitura de PCIe por GPU: (instante, TX, RX).
    pcie: HashMap<u32, (Instant, Metric<u64>, Metric<u64>)>,
    /// Última nova associação NVML-DXGI (ver `REMATCH_EVERY`).
    rematched: Instant,
}

fn slow_list(nvml: &Result<Nvml, String>) -> Vec<(Source, Metric<String>)> {
    gpu_list(nvml)
        .into_iter()
        .map(|(s, a)| (s, a.map(|a| prefix_of(&a))))
        .collect()
}

/// Intervalo mínimo entre leituras de PCIe (o piso da thread lenta).
const PCIE_EVERY: std::time::Duration = std::time::Duration::from_millis(1900);

/// VRAM por categoria e a lista de processos (aba GPU) de um adaptador.
fn vram_and_procs(
    vram: &mut VramCollector,
    luid: &Metric<String>,
    total: Metric<u64>,
    collected: &Result<(), String>,
    engine_rows: Option<&Metric<Vec<(String, f64)>>>,
    names: Option<&HashMap<u32, String>>,
) -> (Metric<VramBreakdown>, Option<Metric<Vec<GpuProc>>>) {
    let breakdown = match (luid, total, collected) {
        (Ok(l), Ok(t), Ok(())) => vram.read(l, t, names),
        (Err(e), _, _) => Err(e.clone()),
        (_, Err(e), _) => Err(e),
        (_, _, Err(e)) => fail(e.clone()),
    };
    let procs = engine_rows.map(|rows| {
        let l = luid.clone()?;
        // LUID trocado (reset): a lista vazia esconderia a causa.
        if lost_adapter(&breakdown) {
            return fail(NO_PDH_INSTANCE);
        }
        collected.clone().or_else(fail)?;
        let mem = vram.per_process_detail(&l).or_else(fail)?;
        if names.is_none() {
            // Sem a lista de processos: o cache cobre os PIDs desta lista.
            let engine_pids: Vec<u32> = rows.as_ref().map_or_else(
                |_| Vec::new(),
                |r| super::gpu_engine::group(r, &l).into_keys().collect(),
            );
            vram.refresh_names(mem.keys().copied().chain(engine_pids));
        }
        let list = gpu_procs(&mem, rows, &l, names.unwrap_or(&vram.names));
        // Engines em falha e nenhum processo com VRAM: a lista vazia esconderia a causa.
        match rows {
            Err(e) if list.is_empty() => Err(e.clone()),
            _ => Ok(list),
        }
    });
    (breakdown, procs)
}

impl GpuSlowCollector {
    pub fn new(nvml: SharedNvml) -> Self {
        let gpus = slow_list(nvml.as_ref());
        Self {
            nvml,
            gpus,
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
        // Como no coletor rápido: lista vazia procura de novo.
        if self.gpus.is_empty() && self.rematched.elapsed() >= REMATCH_EVERY {
            self.rematched = Instant::now();
            self.gpus = slow_list(self.nvml.as_ref());
            self.pcie.clear();
        }
        if self.gpus.is_empty() {
            return match self.nvml.as_ref() {
                Err(e) => fail(e.clone()),
                Ok(_) => fail(NO_GPU),
            };
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
        let Self {
            nvml,
            gpus,
            vram,
            pcie,
            ..
        } = self;
        let mut out = Vec::new();
        for (pos, (source, luid)) in gpus.iter().enumerate() {
            let index = pos as u32;
            let device = match (source, nvml.as_ref()) {
                (Source::Nvml(i), Ok(n)) => Some(nv(n.device_by_index(*i))),
                (Source::Nvml(_), Err(e)) => Some(fail(e.clone())),
                (Source::Dxgi(..), _) => None,
            };
            let total = match (&device, source) {
                (Some(d), _) => d
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|d| nv(d.memory_info()).map(|m| m.total)),
                (None, Source::Dxgi(a, _)) => Ok(a.dedicated_total),
                (None, Source::Nvml(_)) => fail("NVML device missing"),
            };
            let (vram_b, procs) =
                vram_and_procs(vram, luid, total, &collected, engine_rows.as_ref(), names);
            // PCIe custa ~23 ms por sentido: no máximo a cada `PCIE_EVERY`, mesmo quando abrir
            // uma aba acorda a thread lenta antes da hora. KB/s → bytes/s.
            let (pcie_tx, pcie_rx) = match (&device, source) {
                (Some(Ok(d)), _) => {
                    let fresh = pcie
                        .get(&index)
                        .filter(|(t, ..)| t.elapsed() < PCIE_EVERY)
                        .map(|(_, tx, rx)| (tx.clone(), rx.clone()));
                    fresh.unwrap_or_else(|| {
                        let tx = nv(d.pcie_throughput(PcieUtilCounter::Send))
                            .map(|k| u64::from(k) * 1024);
                        let rx = nv(d.pcie_throughput(PcieUtilCounter::Receive))
                            .map(|k| u64::from(k) * 1024);
                        pcie.insert(index, (Instant::now(), tx.clone(), rx.clone()));
                        (tx, rx)
                    })
                }
                (Some(Err(e)), _) => (Err(e.clone()), Err(e.clone())),
                (None, Source::Dxgi(_, why)) => (Err(why.clone()), Err(why.clone())),
                (None, Source::Nvml(_)) => {
                    (fail("NVML device missing"), fail("NVML device missing"))
                }
            };
            out.push(GpuSlow {
                index,
                pcie_tx,
                pcie_rx,
                vram: vram_b,
                procs,
            });
        }
        // Como no coletor rápido: LUID trocado ou associação anterior em falha.
        if (out.iter().any(|g| lost_adapter(&g.vram))
            || self
                .gpus
                .iter()
                .any(|(_, l)| matches!(l, Err(Missing::Fail(_)))))
            && self.rematched.elapsed() >= REMATCH_EVERY
        {
            self.rematched = Instant::now();
            self.gpus = slow_list(self.nvml.as_ref());
            self.pcie.clear();
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
            dedicated_total: 0,
            name: String::new(),
            driver: None,
            software: false,
        }
    }

    /// Adaptadores sem NVML: os de outros fabricantes (neutro), NVIDIA fora do NVML ou com o
    /// NVML em falha (falha); software, Microsoft e LUID repetido ficam de fora.
    #[test]
    fn adaptadores_sem_nvml() {
        let amd = Adapter {
            vendor: 0x1002,
            ..adapter(0x744C, 0, 3)
        };
        let warp = Adapter {
            vendor: MICROSOFT,
            software: true,
            ..adapter(0x8C, 0, 4)
        };
        let rtx = adapter(0x2C05, 1, 1);
        let list = [rtx, amd.clone(), amd.clone(), warp];
        let rtx_id = Some((0x2C05 << 16) | NVIDIA);
        let only = dxgi_only(&list, Ok(&[rtx_id]));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].0, amd);
        assert_eq!(only[0].1, Missing::Neutral(NO_NVML.into()));
        // PCI desconhecido no NVML: a NVIDIA continua com ele.
        assert_eq!(dxgi_only(&list, Ok(&[None])).len(), 1);
        // NVIDIA que o NVML não lista.
        let other = dxgi_only(&list, Ok(&[]));
        assert_eq!(
            other[0].1,
            Missing::Fail("NVIDIA GPU not listed by NVML".into())
        );
        // NVML em falha: a NVIDIA entra pelo DXGI com a causa.
        let down = dxgi_only(&list, Err("NVML: driver not loaded"));
        assert_eq!(down.len(), 2);
        assert_eq!(down[0].1, Missing::Fail("NVML: driver not loaded".into()));
    }

    /// Caminho sem NVML validado na própria NVIDIA: com o NVML "em falha", a GPU entra pelo DXGI
    /// e as leituras batem com as do NVML lado a lado (VRAM = mesmo contador; uso pelas engines
    /// perto do uso do NVML). Rodar parado e com carga (`fps_probe` ou um jogo) para ver o uso.
    #[test]
    #[ignore = "hardware: GPU NVIDIA com NVML; ~12 s"]
    fn sem_nvml_bate_com_o_nvml() {
        let real: SharedNvml = std::sync::Arc::new(init_nvml());
        assert!(real.is_ok(), "NVML: {:?}", real.as_ref().as_ref().err());
        let mut with = GpuCollector::new(real);
        let mut without = GpuCollector::new(std::sync::Arc::new(Err("NVML: forced".into())));
        let mut slow = GpuSlowCollector::new(std::sync::Arc::new(Err("NVML: forced".into())));
        let mut diffs = Vec::new();
        for i in 0..12 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let a = with.read().expect("com NVML");
            let b = without.read().expect("sem NVML");
            let s = slow.read(true, None).expect("lenta sem NVML");
            let (a, b) = (&a[0], &b[0]);
            assert!(a.no_nvml.is_none());
            assert_eq!(b.no_nvml, Some(Missing::Fail("NVML: forced".into())));
            assert!(b.temp.is_err() && s[0].pcie_tx.is_err());
            let (va, vb) = (
                a.dedicated.clone().expect("VRAM"),
                b.dedicated.clone().expect("VRAM"),
            );
            println!(
                "{i:2}: uso NVML {:?} / engines {:?} · enc {:?}/{:?} · dec {:?}/{:?} · VRAM {} / {} MB · total {} / {} MB",
                a.usage,
                b.usage,
                a.encoder,
                b.encoder,
                a.decoder,
                b.decoder,
                va.0 >> 20,
                vb.0 >> 20,
                va.1 >> 20,
                vb.1 >> 20
            );
            // Mesma VRAM (mesmo contador do PDH), com folga para a coleta em instantes diferentes.
            assert!(va.0.abs_diff(vb.0) < 64 << 20, "VRAM {va:?} x {vb:?}");
            // Total: o do DXGI (DedicatedVideoMemory) e o do NVML diferem pela reserva do driver.
            assert!(
                vb.1 > va.1 * 9 / 10 && vb.1 <= va.1,
                "total {va:?} x {vb:?}"
            );
            if i >= 1 {
                let u = b.usage.clone().expect("uso pelas engines");
                diffs.push(f64::from(a.usage.clone().expect("uso NVML")) - f64::from(u));
            }
            if i >= 2 {
                let v = s[0].vram.as_ref().expect("VRAM por categoria");
                assert!(v.used > 0);
                assert!(s[0].procs.as_ref().expect("aba GPU").is_ok());
            }
        }
        assert!(
            without.gpus[0]
                .name
                .as_ref()
                .is_ok_and(|n| n.contains("NVIDIA"))
        );
        assert!(without.gpus[0].driver.is_ok());
        let mean = diffs.iter().map(|d| d.abs()).sum::<f64>() / diffs.len() as f64;
        println!("diferença média do uso: {mean:.1} pontos");
        assert!(mean < 15.0, "uso pelas engines longe do NVML: {diffs:?}");
    }

    #[test]
    fn versao_do_driver() {
        let v: i64 = (32 << 48) | (15 << 16) | 8180;
        assert_eq!(driver_version(v), "32.0.15.8180");
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
