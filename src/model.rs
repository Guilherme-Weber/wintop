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
    /// Tempo de kernel por lógico, na mesma ordem, 0-100 (parte de `per_logical`).
    pub kernel_per_logical: Metric<Vec<f64>>,
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
    /// As duas partes do "em cache": standby (livre para reuso) e modificada (a gravar).
    pub standby: Metric<u64>,
    pub modified: Metric<u64>,
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
#[derive(Clone, Debug, PartialEq)]
pub struct GpuSlow {
    pub index: u32,
    pub pcie_tx: Metric<u64>,
    pub pcie_rx: Metric<u64>,
    /// VRAM dedicada por categoria (como o gputaskman).
    pub vram: Metric<VramBreakdown>,
    /// Processos com VRAM ou uso de engine (só com a aba GPU visível; `None` fora dela).
    pub procs: Option<Metric<Vec<GpuProc>>>,
}

/// Um processo na GPU (aba GPU).
#[derive(Clone, Debug, PartialEq)]
pub struct GpuProc {
    pub pid: u32,
    pub name: String,
    /// VRAM local (dedicada), não local (na RAM) e confirmada, bytes.
    pub local: u64,
    pub non_local: u64,
    pub committed: u64,
    /// Uso por tipo de engine ("3D", "Copy", "VideoEncode", "Compute_0"...), 0-100; `Err` =
    /// ainda sem taxa (aba recém-aberta) ou falha do contador.
    pub engines: Metric<Vec<(String, f64)>>,
}

impl GpuProc {
    /// Memória paginada: o confirmado que não está nem na VRAM nem na RAM compartilhada.
    pub fn paged(&self) -> u64 {
        self.committed
            .saturating_sub(self.local)
            .saturating_sub(self.non_local)
    }
}

/// Um processo (aba Processos; RAM e CPU da aba GPU).
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    /// Instante de criação (FILETIME, 100 ns): identifica o processo mesmo com PID reaproveitado.
    pub create: u64,
    /// % do sistema inteiro (todos os lógicos), como o Task Manager.
    pub cpu: f64,
    /// Working set privado (coluna "Memory" do Task Manager), bytes.
    pub mem: u64,
    /// Bytes privados confirmados.
    pub commit: u64,
    pub threads: u32,
    pub handles: u32,
    /// Bytes por segundo (todo I/O do processo).
    pub io_read: f64,
    pub io_write: f64,
}

/// Quais abas com coleta própria estão visíveis (o motor só coleta o que aparece).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub procs: bool,
    pub gpu: bool,
}

/// Categoria de uso da VRAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VramKind {
    /// O jogo que o RTSS acompanha (a interface separa pelo PID; o coletor não sabe qual é).
    Game,
    App,
    Compositor,
    Browser,
    System,
    Kernel,
    Free,
}

impl VramKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::App => "app",
            Self::Compositor => "compositor",
            Self::Browser => "browser",
            Self::System => "system",
            Self::Kernel => "kernel",
            Self::Free => "free",
        }
    }
}

/// VRAM local de um processo.
#[derive(Clone, Debug, PartialEq)]
pub struct VramProc {
    pub pid: u32,
    pub name: String,
    pub local: u64,
    pub kind: VramKind,
}

/// VRAM dedicada dividida por categoria; processos do maior para o menor.
#[derive(Clone, Debug, PartialEq)]
pub struct VramBreakdown {
    pub total: u64,
    pub used: u64,
    /// Na ordem de exibição: app, compositor, browser, system, kernel, free (o jogo é separado
    /// do "app" pela interface, que sabe o PID dele).
    pub segments: Vec<(VramKind, u64)>,
    pub processes: Vec<VramProc>,
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
#[derive(Clone, Debug, PartialEq)]
pub struct DiskTemp {
    pub number: u32,
    /// Modelo do disco lido (confere com `Disk::model` antes de exibir).
    pub model: Option<String>,
    /// °C.
    pub celsius: Metric<i16>,
    /// De onde veio: "native", "SMART 194", "SMART 190", "pool members".
    pub source: &'static str,
}

/// Jogo acompanhado pelo RTSS.
#[derive(Clone, Debug, PartialEq)]
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
    /// Pior frametime (ms) desde a publicação anterior: o gráfico de frametime usa este (a
    /// janela de 1 s repetiria o mesmo engasgo em várias publicações). `None`: sem frames novos.
    pub peak_ms: Option<f64>,
}

/// Estado do RTSS e do jogo (thread do RTSS, a cada 250 ms).
#[derive(Clone, Debug, PartialEq)]
pub struct Fps {
    /// RTSS rodando (versão da Shared Memory).
    pub rtss: Metric<String>,
    /// Pipe de frametime: quantas instâncias e clientes conectados.
    pub pipe: Metric<(usize, usize)>,
    pub game: Metric<Game>,
    /// PIDs com o hook do RTSS (apps 3D, possivelmente jogos com anticheat): a leitura de
    /// módulos sob demanda nunca abre estes processos.
    pub hooked: Vec<u32>,
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
    /// Processos (só com a aba Processos ou GPU visível; `None` fora delas).
    pub procs: Option<Metric<Vec<Proc>>>,
    pub took: Duration,
}

impl Fast {
    /// Todas as fontes com a mesma falha (a thread parou).
    pub fn failed(cause: &str) -> Self {
        let m = Missing::Fail(cause.to_string());
        Self {
            cpu: Err(m.clone()),
            cpu_sensors: Err(m.clone()),
            memory: Err(m.clone()),
            disks: Err(m.clone()),
            nics: Err(m.clone()),
            gpus: Err(m.clone()),
            own: Err(m),
            took: Duration::ZERO,
        }
    }
}

impl Slow {
    /// Todas as fontes com a mesma falha (a thread parou).
    pub fn failed(cause: &str) -> Self {
        let m = Missing::Fail(cause.to_string());
        Self {
            gpus: Err(m.clone()),
            disk_temps: Err(m.clone()),
            procs: Some(Err(m)),
            took: Duration::ZERO,
        }
    }
}
