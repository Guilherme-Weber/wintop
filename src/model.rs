//! Dados coletados (snapshots) e o estado de cada métrica.
//!
//! Regra do projeto: toda métrica aparece. Sem dado, ela carrega o motivo: um estado neutro
//! (listado em `docs/metricas.md`, ex.: "desconectada") ou uma falha (exibida em vermelho).

use std::fmt;
use std::time::Duration;

/// Por que uma métrica está sem valor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
    /// Estado previsto no inventário (não é erro).
    Neutral(String),
    /// Falha da fonte, com a causa.
    Fail(String),
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Neutral(s) | Self::Fail(s) => f.write_str(s),
        }
    }
}

pub type Metric<T> = Result<T, Missing>;

pub fn fail<T>(cause: impl Into<String>) -> Metric<T> {
    Err(Missing::Fail(cause.into()))
}

pub fn neutral<T>(state: impl Into<String>) -> Metric<T> {
    Err(Missing::Neutral(state.into()))
}

/// Contagens do sistema (`GetPerformanceInfo`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub processes: u32,
    pub threads: u32,
    pub handles: u32,
}

/// Topologia da CPU (`GetLogicalProcessorInformationEx`). Caches em bytes, somados por nível.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Topology {
    pub sockets: u32,
    pub cores: u32,
    pub logical: u32,
    pub l1: u64,
    pub l2: u64,
    pub l3: u64,
}

#[derive(Clone, Debug)]
pub struct Cpu {
    pub name: Metric<String>,
    pub topology: Metric<Topology>,
    pub base_mhz: Metric<u32>,
    pub virtualization: Metric<bool>,
    /// Uso total, 0-100.
    pub usage: Metric<f64>,
    /// Uso por processador lógico, na ordem (grupo, número), 0-100.
    pub per_logical: Metric<Vec<f64>>,
    /// Parcela do tempo total gasta em modo kernel, 0-100.
    pub kernel: Metric<f64>,
    pub freq_mhz: Metric<f64>,
    pub counts: Metric<Counts>,
    pub uptime: Duration,
}

#[derive(Clone, Debug)]
pub struct Memory {
    pub total: Metric<u64>,
    pub used: Metric<u64>,
    pub commit: Metric<(u64, u64)>,
    /// Standby + Modified (igual ao "Em cache" do Gerenciador de Tarefas).
    pub cached: Metric<u64>,
    pub paged_pool: Metric<u64>,
    pub nonpaged_pool: Metric<u64>,
    /// Uso do arquivo de paginação (`_Total`), 0-100.
    pub pagefile_usage: Metric<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    Sata,
    Nvme,
    Spaces,
    Usb,
    Sas,
    Raid,
    Virtual,
    Other(u32),
}

impl Bus {
    pub fn from_raw(v: u32) -> Self {
        match v {
            7 => Self::Usb,
            8 => Self::Raid,
            10 => Self::Sas,
            11 => Self::Sata,
            14 => Self::Virtual,
            16 => Self::Spaces,
            17 => Self::Nvme,
            n => Self::Other(n),
        }
    }
}

impl fmt::Display for Bus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sata => f.write_str("SATA"),
            Self::Nvme => f.write_str("NVMe"),
            Self::Spaces => f.write_str("Spaces"),
            Self::Usb => f.write_str("USB"),
            Self::Sas => f.write_str("SAS"),
            Self::Raid => f.write_str("RAID"),
            Self::Virtual => f.write_str("Virtual"),
            Self::Other(n) => write!(f, "bus {n}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Disk {
    /// Número do disco físico (`\\.\PhysicalDriveN`).
    pub number: u32,
    pub model: Metric<String>,
    pub bus: Metric<Bus>,
    /// `true` = SSD (sem penalidade de busca).
    pub ssd: Metric<bool>,
    /// Bytes.
    pub capacity: Metric<u64>,
    pub pool_member: Metric<bool>,
    /// Letras das unidades (ex.: `["C:"]`); membros do pool não têm letra.
    pub letters: Vec<String>,
    pub system: bool,
    pub pagefile: bool,
    /// Tempo ativo, 0-100.
    pub active: Metric<f64>,
    /// Bytes por segundo.
    pub read: Metric<f64>,
    pub write: Metric<f64>,
    /// Tempo médio de resposta, em segundos.
    pub response: Metric<f64>,
}

#[derive(Clone, Debug)]
pub struct Nic {
    pub alias: String,
    pub description: String,
    /// bit/s.
    pub link: Metric<u64>,
    /// bit/s.
    pub send: Metric<f64>,
    pub recv: Metric<f64>,
}

#[derive(Clone, Debug)]
pub struct Fan {
    pub percent: Metric<u32>,
    pub rpm: Metric<u32>,
}

#[derive(Clone, Debug)]
pub struct Gpu {
    pub index: u32,
    pub name: Metric<String>,
    pub driver: Metric<String>,
    pub vbios: Metric<String>,
    pub usage: Metric<u32>,
    pub mem_activity: Metric<u32>,
    /// VRAM dedicada do WDDM (PDH, igual ao Gerenciador de Tarefas) e total do NVML, bytes.
    pub dedicated: Metric<(u64, u64)>,
    /// Memória compartilhada usada (PDH) e limite (DXGI), bytes.
    pub shared: Metric<(u64, u64)>,
    /// Frame buffer físico pelo NVML: usada, livre, reservada, total (bytes).
    pub physical: Metric<[u64; 4]>,
    pub temp: Metric<u32>,
    pub temp_slowdown: Metric<u32>,
    /// Clocks em MHz: atual e máximo.
    pub clock_graphics: Metric<(u32, u32)>,
    pub clock_sm: Metric<(u32, u32)>,
    pub clock_mem: Metric<(u32, u32)>,
    pub clock_video: Metric<(u32, u32)>,
    /// Watts: atual e limite.
    pub power: Metric<(f64, f64)>,
    pub fans: Metric<Vec<Fan>>,
    /// Geração e largura atuais do link.
    pub pcie_link: Metric<(u32, u32)>,
    pub encoder: Metric<u32>,
    pub decoder: Metric<u32>,
    pub pstate: Metric<u32>,
    pub throttle: Metric<Vec<&'static str>>,
}

/// Leituras da thread lenta por GPU: PCIe TX/RX em bytes/s.
#[derive(Clone, Debug)]
pub struct GpuSlow {
    pub index: u32,
    pub pcie_tx: Metric<u64>,
    pub pcie_rx: Metric<u64>,
}

/// Sensores da CPU pelo PawnIO.
#[derive(Clone, Debug)]
pub struct CpuSensors {
    /// °C.
    pub tctl: Metric<f64>,
    /// (número do CCD a partir de 1, °C).
    pub ccds: Metric<Vec<(u32, f64)>>,
    /// Watts.
    pub package_power: Metric<f64>,
}

/// Temperatura de um disco (thread lenta).
#[derive(Clone, Debug)]
pub struct DiskTemp {
    pub number: u32,
    /// °C.
    pub celsius: Metric<i16>,
    /// De onde veio: "native", "SMART 194", "SMART 190", "pool members".
    pub source: &'static str,
}

/// Jogo acompanhado pelo RTSS.
#[derive(Clone, Debug)]
pub struct Game {
    pub pid: u32,
    pub name: String,
    pub api: &'static str,
    pub resolution: Metric<(u32, u32)>,
    /// `true` = valores da Shared Memory (jogo aberto antes do wintop): aproximados.
    pub approx: bool,
    pub fps: Metric<f64>,
    /// Frametime no último segundo, em ms: médio, mínimo e máximo.
    pub frametime: Metric<(f64, f64, f64)>,
    /// 1% e 0,1% low em FPS (janela de 60 s).
    pub lows: Metric<(f64, f64)>,
    /// Tempo de GPU ativo e frame de GPU, em ms.
    pub gpu_time: Metric<(f64, f64)>,
    /// "GPU" ou "CPU": quem limita o frame.
    pub bound: Metric<&'static str>,
}

/// Estado do RTSS e do jogo (thread do RTSS, a cada 250 ms).
#[derive(Clone, Debug)]
pub struct Fps {
    /// RTSS rodando (versão da Shared Memory).
    pub rtss: Metric<String>,
    /// Pipe de frametime: quantas instâncias e clientes conectados.
    pub pipe: Metric<(usize, usize)>,
    pub game: Metric<Game>,
}

/// Consumo do próprio wintop.
#[derive(Clone, Debug)]
pub struct OwnUsage {
    /// % do sistema inteiro (todos os processadores lógicos).
    pub cpu: Metric<f64>,
    /// Working set privado (a coluna "Memory" do Task Manager), bytes.
    pub private_ws: Metric<u64>,
    /// Memória confirmada (privada), bytes.
    pub commit: Metric<u64>,
}

/// Uma coleta da thread rápida.
#[derive(Clone, Debug)]
pub struct Fast {
    pub cpu: Metric<Cpu>,
    pub cpu_sensors: Metric<CpuSensors>,
    pub memory: Metric<Memory>,
    pub disks: Metric<Vec<Disk>>,
    pub nics: Metric<Vec<Nic>>,
    pub gpus: Metric<Vec<Gpu>>,
    pub own: Metric<OwnUsage>,
    /// Quanto a coleta levou.
    pub took: Duration,
}

/// Uma coleta da thread lenta.
#[derive(Clone, Debug)]
pub struct Slow {
    pub gpus: Metric<Vec<GpuSlow>>,
    pub disk_temps: Metric<Vec<DiskTemp>>,
    pub took: Duration,
}
